//! DROP RESTRICT over catalog object addresses, not an enumeration of classes.
//! Internal owners preserve the view-rule and row-type paths measured by the
//! module reader; unknown classes and domain constraints cannot fall out of a
//! class-specific join. Automatic removal is distinct from a blocking edge.

use pbps_model::escape::Escaped;
use std::collections::{BTreeMap, BTreeSet};

use pbps_db::impact::{DropReport, ImpactError};
use pbps_db::{Conn, DbError, Param, Row};
use pbps_model::part_effects::{DefaultChange, Effect, Part as DependencyPart};
use pbps_model::physical_sources::{PhysicalSources, Source};
use pbps_model::plan_names::At;
use pbps_model::{Change, ModuleId, ModuleKind, TableName};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Address {
    class: i64,
    object: i64,
    part: i64,
}

impl Address {
    fn contains(self, other: Self) -> bool {
        self.class == other.class
            && self.object == other.object
            && (self.part == 0 || self.part == other.part)
    }
}

struct Edge {
    dependent: Address,
    referenced: Address,
    kind: String,
    dependent_name: String,
    referenced_name: String,
}

impl Edge {
    fn automatic(&self) -> bool {
        matches!(self.kind.as_str(), "a" | "i" | "x" | "P" | "S")
    }

    fn owner(&self) -> bool {
        matches!(self.kind.as_str(), "i" | "e")
    }
}

struct Graph(Vec<Edge>);

impl Graph {
    fn closure(
        &self,
        root: Address,
        automatic: bool,
        replaced: &BTreeSet<Address>,
    ) -> BTreeSet<Address> {
        let mut found = BTreeSet::from([root]);
        loop {
            let mut next = found.clone();
            for edge in &self.0 {
                if (!automatic || edge.automatic())
                    && found.iter().any(|a| a.contains(edge.referenced))
                {
                    next.insert(edge.dependent);
                }
                // An indirectly removed internal object promotes removal to
                // its owner. The explicit root cannot authorize deleting its
                // owner: DROP of an extension member is still refused.
                //
                // A replaced object promotes nothing: `SET EXPRESSION` swaps a
                // generated column's `pg_attrdef` row and leaves the column,
                // and what depends on the column, in place (DEC-1316.1).
                if edge.owner()
                    && found.iter().any(|a| a.contains(edge.dependent))
                    && (!automatic || !root.contains(edge.dependent))
                    && !replaced.iter().any(|r| r.contains(edge.dependent))
                {
                    next.insert(edge.referenced);
                }
            }
            if next == found {
                return found;
            }
            found = next;
        }
    }

    fn blockers(
        &self,
        root: Address,
        at_root: usize,
        removals: &[(usize, Address)],
        replaced: &BTreeSet<Address>,
    ) -> Vec<String> {
        let reachable = self.closure(root, false, replaced);
        let mut removed = BTreeMap::<Address, usize>::new();
        for &(index, address) in removals {
            for auto in self.closure(address, true, replaced) {
                removed
                    .entry(auto)
                    .and_modify(|old| *old = (*old).min(index))
                    .or_insert(index);
            }
        }
        let position = |address| {
            removed
                .iter()
                .filter(|(a, _)| a.contains(address))
                .map(|(_, i)| *i)
                .min()
        };
        let mut blocking = BTreeSet::new();
        for edge in &self.0 {
            if reachable.iter().any(|a| a.contains(edge.referenced))
                && !edge.automatic()
                && position(edge.dependent)
                    .is_none_or(|before| before > position(edge.referenced).unwrap_or(at_root))
            {
                blocking.insert(Escaped(&edge.dependent_name).to_string());
            }
            // A replaced object keeps its owner: `SET EXPRESSION` swaps a
            // generated column's `pg_attrdef` row and leaves the column, which
            // owns the row internally (DEC-1316.1).
            if edge.owner()
                && !replaced.iter().any(|r| r.contains(edge.dependent))
                && reachable.iter().any(|a| a.contains(edge.dependent))
                && let Some(at) = position(edge.dependent)
                && position(edge.referenced).is_none_or(|before| before > at)
            {
                blocking.insert(format!(
                    "{} (internal owner of {})",
                    Escaped(&edge.referenced_name),
                    Escaped(&edge.dependent_name)
                ));
            }
        }
        blocking.into_iter().collect()
    }
}

fn required(row: &Row, name: &str) -> Result<i64, DbError> {
    row.try_get::<i64>(name)?
        .ok_or_else(|| error(format!("drop_blockers: catalog omitted {name}")))
}

fn error(message: String) -> DbError {
    DbError::Refused(message)
}

struct Classes {
    relation: i64,
    routine: i64,
    trigger: i64,
    constraint: i64,
    default: i64,
    policy: i64,
}

impl Classes {
    async fn read(conn: &mut Conn) -> Result<Self, DbError> {
        let rows = conn
            .query(
                "SELECT 'pg_catalog.pg_class'::regclass::int8 AS relation,
            'pg_catalog.pg_proc'::regclass::int8 AS routine,
            'pg_catalog.pg_trigger'::regclass::int8 AS trigger,
            'pg_catalog.pg_constraint'::regclass::int8 AS constraint,
            'pg_catalog.pg_attrdef'::regclass::int8 AS default,
            'pg_catalog.pg_policy'::regclass::int8 AS policy",
            )
            .await?;
        let row = rows
            .first()
            .ok_or_else(|| error("drop_blockers: no catalog classes".into()))?;
        Ok(Self {
            relation: required(row, "relation")?,
            routine: required(row, "routine")?,
            trigger: required(row, "trigger")?,
            constraint: required(row, "constraint")?,
            default: required(row, "default")?,
            policy: required(row, "policy")?,
        })
    }
}

/// Read existing blockers for table, column, key and index drops. Must run in
/// the caller's transaction; no DDL or catalog mutation is used to test a drop.
pub async fn drop_blockers(
    conn: &mut Conn,
    sources: &PhysicalSources<'_>,
) -> Result<Vec<DropReport>, ImpactError> {
    let cs = sources.names().changes();
    let targets: Vec<_> = cs
        .iter()
        .enumerate()
        .filter_map(|(index, p)| match &p.change {
            Change::RebuildPartitionedTable(_)
            | Change::BuildPartitionedIndex(_)
            | Change::CopyTableRows(_)
            | Change::AnalyzeTableTree(_) => None,
            Change::DropTable { name, .. } => {
                Some((index, format!("table {name}", name = name.shown())))
            }
            Change::DropColumn { column, .. } => {
                Some((index, format!("column {column}", column = column.shown())))
            }
            Change::SetPrimaryKey {
                table,
                from: Some(_),
                ..
            } => Some((
                index,
                format!("primary key on {table}", table = table.shown()),
            )),
            Change::DropUnique { table, name } => Some((
                index,
                format!(
                    "unique constraint `{name}` on {table}",
                    table = table.shown(),
                    name = name.shown(),
                ),
            )),
            Change::DropIndex { table, name } => Some((
                index,
                format!(
                    "index `{name}` on {table}",
                    table = table.shown(),
                    name = name.shown(),
                ),
            )),
            Change::AlterColumnNumbering {
                column, from, to, ..
            } if gives_up(from, to) => Some((
                index,
                format!("numbering source of {column}", column = column.shown()),
            )),
            Change::CreateTable { .. }
            | Change::DetachPartition { .. }
            | Change::AttachPartition { .. }
            | Change::RenameTable { .. }
            | Change::AddColumn { .. }
            | Change::RenameColumn { .. }
            | Change::AlterColumnType { .. }
            | Change::AlterColumnNullability { .. }
            | Change::AlterColumnDefault { .. }
            | Change::AlterColumnExpression { .. }
            | Change::AlterColumnNumbering { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::SetPrimaryKey { .. }
            | Change::SetIndexStorageParameters { .. }
            | Change::SetTablePersistence { .. }
            | Change::SetPartitionDefault { .. }
            | Change::SetPartitionNotNull { .. }
            | Change::SetStorageParameters { .. }
            | Change::DropPolicy { .. }
            | Change::CreatePolicy { .. }
            | Change::SetRowSecurity { .. }
            | Change::SetReplicaIdentity { .. }
            | Change::AddUnique { .. }
            | Change::AddForeignKey { .. }
            | Change::DropForeignKey { .. }
            | Change::AddCheck { .. }
            | Change::DropCheck { .. }
            | Change::AddIndex { .. }
            | Change::AddComputedColumn { .. }
            | Change::DropComputedColumn { .. }
            | Change::InsertRow { .. }
            | Change::UpdateRow { .. }
            | Change::DeleteRow { .. }
            | Change::SetDataMode { .. }
            | Change::SetEnsureKeys { .. }
            | Change::CreateModule { .. }
            | Change::AlterModule { .. }
            | Change::DropModule { .. }
            | Change::AddEnumValue { .. }
            | Change::AlterDomain { .. }
            | Change::RenameEnumValue { .. }
            | Change::EnumRebuild { .. }
            | pbps_model::catalog_change!()
            | Change::Grant { .. }
            | Change::Revoke { .. }
            | Change::PublicExecution { .. } => None,
        })
        .collect();
    let Some(&(last, _)) = targets.last() else {
        return Ok(Vec::new());
    };
    let token = crate::catalog::probe_token();
    conn.query(&crate::catalog::probe_set(&token)).await?;
    let rows = conn.query(crate::catalog::PROBE_READ).await?;
    if rows
        .first()
        .map(|r| r.try_get::<&str>("probe"))
        .transpose()?
        .flatten()
        != Some(token.as_str())
    {
        return Err(error("drop_blockers requires the caller's transaction".into()).into());
    }
    conn.query("SELECT pg_catalog.set_config('search_path', 'pg_catalog', true)")
        .await?;
    let classes = Classes::read(conn).await?;
    let mut removals = Vec::new();
    let mut detached = BTreeSet::new();
    for (index, step) in cs.iter().enumerate().take(last + 1) {
        if let Some(address) = removal(conn, &classes, sources, index).await? {
            removals.push((index, address));
            // A partition's drop detaches it first, and the detach removes
            // the clones a foreign key to its parent keeps on each
            // referencing table, which name this partition (#1171). Measured
            // on 16 and 18: read as standing, the clone blocked every drop of
            // a partition such a key reaches, with no row referencing it.
            if let Change::DropTable {
                detach_from: Some(_),
                ..
            } = &step.change
            {
                for clone in detached_clones(conn, &classes, address).await? {
                    removals.push((index, clone));
                    detached.insert(clone);
                }
            }
            // Giving up a serial drops its default first, and dropping its
            // column drops the default with it. Both recurse into every
            // partition (DEC-2219.1), including external partitions.
            if let Change::DropColumn { column, .. }
            | Change::AlterColumnNumbering {
                column,
                from: pbps_model::ValueSource::Sequence(_),
                ..
            } = &cs[index].change
            {
                for default in recursed_defaults(conn, &classes, sources, index, column).await? {
                    removals.push((index, default));
                }
            }
        } else if let Change::SetPrimaryKey {
            table,
            from: Some(_),
            ..
        }
        | Change::DropUnique { table, .. }
        | Change::DropIndex { table, .. } = &step.change
            && stored(sources, index, table, None)?.is_some()
        {
            return Err(error(format!(
                "drop_blockers: key on {table} is absent from the catalog",
                table = table.shown()
            ))
            .into());
        }
    }
    let roots: BTreeSet<_> = removals.iter().map(|(_, a)| *a).collect();
    // A clone the detach removes promotes nothing either: its internal owner
    // is the foreign key on its own table, which stays (#1171).
    // Nor does an identity's sequence given up: `DROP IDENTITY` drops it and
    // leaves the column, which owns it internally (#1930 review).
    let replaced: BTreeSet<Address> = removals
        .iter()
        .filter(|(i, _)| {
            matches!(
                cs[*i].change,
                Change::AlterColumnExpression { .. }
                    | Change::AlterColumnNumbering {
                        from: pbps_model::ValueSource::Identity(_),
                        ..
                    }
            )
        })
        .map(|(_, a)| *a)
        .chain(detached)
        .collect();
    let graph = read_graph(conn, &roots).await?;
    let mut reports = Vec::new();
    for (index, target) in targets {
        let blocking = match removals.iter().find(|(i, _)| *i == index) {
            Some((_, root)) => {
                let prior: Vec<_> = removals
                    .iter()
                    .copied()
                    .filter(|(i, _)| *i <= index)
                    .collect();
                graph.blockers(*root, index, &prior, &replaced)
            }
            // An explicit earlier creation has no current catalog dependency
            // graph. Missing existing targets are errors in removal(), below.
            None => Vec::new(),
        };
        reports.push(DropReport {
            change_index: index,
            target,
            blocking,
        });
    }
    Ok(reports)
}

/// Ask for the physical source at this removal's execution boundary. A
/// managed-scope change is not a creation, and unavailable proof is not absence.
fn stored(
    sources: &PhysicalSources<'_>,
    index: usize,
    table: &TableName,
    column: Option<&str>,
) -> Result<Option<(TableName, Option<String>)>, ImpactError> {
    fn existing<A>(source: Source<A>, about: &str) -> Result<Option<A>, ImpactError> {
        match source {
            Source::Stored(address) => Ok(Some(address)),
            Source::Created { .. } => Ok(None),
            Source::Retired | Source::Unavailable => Err(error(format!(
                "drop_blockers: physical source for {about} is unavailable"
            ))
            .into()),
        }
    }
    let at = At::Before(index);
    match column {
        Some(column) => {
            let address = table.column(column);
            Ok(
                existing(sources.source(&address, at), &address.shown().to_string())?
                    .map(|stored| (stored.table, Some(stored.name.into_raw()))),
            )
        }
        None => Ok(
            existing(sources.table_source(table, at), &table.shown().to_string())?
                .map(|stored| (stored, None)),
        ),
    }
}

async fn relation(
    conn: &mut Conn,
    classes: &Classes,
    table: &TableName,
    column: Option<&str>,
) -> Result<Option<Address>, ImpactError> {
    let name = crate::emit::qualified(table)?;
    let rows = conn
        .query_with(
            "SELECT c.oid::int8 AS oid, COALESCE(a.attnum,0)::int8 AS part
           FROM pg_catalog.pg_class c
           LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid=c.oid AND a.attname=$2
             AND a.attnum > 0 AND NOT a.attisdropped
          WHERE c.oid=pg_catalog.to_regclass($1) AND c.relkind IN ('r','p')
            AND ($2::text = '' OR a.attnum IS NOT NULL)",
            &[Param::Str(&name), Param::Str(column.unwrap_or_default())],
        )
        .await?;
    rows.first()
        .map(|row| {
            Ok(Address {
                class: classes.relation,
                object: required(row, "oid")?,
                part: required(row, "part")?,
            })
        })
        .transpose()
}

/// The foreign-key clones on other tables that reference `partition`, which
/// detaching it removes with it (#1171).
async fn detached_clones(
    conn: &mut Conn,
    classes: &Classes,
    partition: Address,
) -> Result<Vec<Address>, ImpactError> {
    let rows = conn
        .query_with(
            "SELECT con.oid::int8 AS oid FROM pg_catalog.pg_constraint con
              WHERE con.contype = 'f' AND con.conparentid <> 0
                AND con.confrelid = $1::int8::oid",
            &[Param::I64(partition.object)],
        )
        .await?;
    rows.iter()
        .map(|row| {
            Ok(Address {
                class: classes.constraint,
                object: required(row, "oid")?,
                part: 0,
            })
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
enum Removal<'a> {
    Table(&'a TableName),
    Column(pbps_model::ColumnRef),
    Module(&'a ModuleId, ModuleKind),
    Part(&'a TableName, Part<'a>),
    Default(pbps_model::ColumnRef),
}

// Catalog classes and absent/unreadable policy stay here. The model supplies
// declaration effects, not a claim that a catalog address exists (DEC-1316.1).
#[allow(clippy::wildcard_enum_match_arm)]
fn removal_part(change: &Change) -> Option<Removal<'_>> {
    change.part_effects().into_iter().find_map(|fact| match (fact.part, fact.effect) {
        (DependencyPart::Table(table), Effect::Removed) => Some(Removal::Table(table)),
        (DependencyPart::Column(column), Effect::Removed) => Some(Removal::Column(column)),
        (DependencyPart::Module { id, kind }, Effect::Removed | Effect::Rewritten) => Some(Removal::Module(id, kind)),
        (DependencyPart::Named { table, kind: pbps_model::Part::Check | pbps_model::Part::Unique | pbps_model::Part::ForeignKey, name }, Effect::Removed) => Some(Removal::Part(table, Part::Constraint(name))),
        (DependencyPart::Named { table, kind: pbps_model::Part::Index, name }, Effect::Removed) => Some(Removal::Part(table, Part::Index(name))),
        (DependencyPart::Named { table, kind: pbps_model::Part::Policy, name }, Effect::Removed) => Some(Removal::Part(table, Part::Policy(name))),
        (DependencyPart::PrimaryKey(table), Effect::Removed | Effect::Rewritten) => Some(Removal::Part(table, Part::PrimaryKey)),
        (DependencyPart::Default { column, change: DefaultChange::Column { from: Some(_), .. } | DefaultChange::Partition { .. } }, _)
        | (DependencyPart::Generated(column), Effect::Rewritten) => Some(Removal::Default(column)),
        _ => None,
    })
}

async fn removal(
    conn: &mut Conn,
    classes: &Classes,
    sources: &PhysicalSources<'_>,
    index: usize,
) -> Result<Option<Address>, ImpactError> {
    let change = &sources.names().changes()[index].change;
    // What a numbering change gives up (#1889): the sequence an owned
    // sequence or an identity numbered from, which no part the model
    // projects names. An owned sequence's default goes with it, added beside
    // it in `drop_blockers`.
    if let Change::AlterColumnNumbering {
        column, from, to, ..
    } = change
        && gives_up(from, to)
    {
        return numbering_sequence(
            conn,
            classes,
            sources,
            index,
            &column.table,
            column.name.raw(),
        )
        .await;
    }
    match removal_part(change) {
        Some(Removal::Table(name)) => {
            let Some((table, _)) = stored(sources, index, name, None)? else {
                return Ok(None);
            };
            relation(conn, classes, &table, None)
                .await?
                .map(Some)
                .ok_or_else(|| {
                    error(format!(
                        "drop_blockers: table {table} is absent from the catalog",
                        table = table.shown()
                    ))
                    .into()
                })
        }
        Some(Removal::Column(column)) => {
            let Some((table, name)) =
                stored(sources, index, &column.table, Some(column.name.raw()))?
            else {
                return Ok(None);
            };
            relation(conn, classes, &table, name.as_deref())
                .await?
                .map(Some)
                .ok_or_else(|| {
                    error(format!(
                        "drop_blockers: column {column} is absent from the catalog",
                        column = column.shown()
                    ))
                    .into()
                })
        }
        Some(Removal::Module(id, kind)) => module_address(conn, classes, id, kind).await,
        Some(Removal::Part(table, part)) => {
            table_part(conn, classes, sources, index, table, part).await
        }
        Some(Removal::Default(column)) => {
            default_row(
                conn,
                classes,
                sources,
                index,
                &column.table,
                column.name.raw(),
            )
            .await
        }
        None => Ok(None),
    }
}

/// The `pg_attrdef` row a default change on `table`'s `column` replaces, as
/// the plan stored them at `index`.
async fn default_row(
    conn: &mut Conn,
    classes: &Classes,
    sources: &PhysicalSources<'_>,
    index: usize,
    table: &TableName,
    column: &str,
) -> Result<Option<Address>, ImpactError> {
    let Some((table, name)) = stored(sources, index, table, Some(column))? else {
        return Ok(None);
    };
    let Some(on) = relation(conn, classes, &table, name.as_deref()).await? else {
        return Ok(None);
    };
    let rows = conn
        .query_with(
            "SELECT oid::int8 AS oid FROM pg_catalog.pg_attrdef
            WHERE adrelid=($1::int8)::oid AND adnum=$2::int8",
            &[Param::I64(on.object), Param::I64(on.part)],
        )
        .await?;
    rows.first()
        .map(|row| {
            Ok::<Address, DbError>(Address {
                class: classes.default,
                object: required(row, "oid")?,
                part: 0,
            })
        })
        .transpose()
        .map_err(Into::into)
}

/// The defaults removed by recursive DROP DEFAULT or DROP COLUMN. Match each
/// partition column by name, since an attached table can have a different
/// attribute number. Defaults outside this tree remain blockers.
async fn recursed_defaults(
    conn: &mut Conn,
    classes: &Classes,
    sources: &PhysicalSources<'_>,
    index: usize,
    column: &pbps_model::ColumnRef,
) -> Result<Vec<Address>, ImpactError> {
    let Some((table, name)) = stored(sources, index, &column.table, Some(column.name.raw()))?
    else {
        return Ok(Vec::new());
    };
    let Some(on) = relation(conn, classes, &table, name.as_deref()).await? else {
        return Ok(Vec::new());
    };
    let rows = conn
        .query_with(
            "SELECT d.oid::int8 AS oid FROM pg_catalog.pg_attrdef d
         JOIN pg_catalog.pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum
         JOIN pg_catalog.pg_attribute parent ON parent.attrelid = $1::int8::oid
             AND parent.attnum = $2::int8 AND parent.attname = a.attname
         WHERE d.adrelid = parent.attrelid OR d.adrelid IN
             (SELECT relid FROM pg_catalog.pg_partition_tree(parent.attrelid))",
            &[Param::I64(on.object), Param::I64(on.part)],
        )
        .await?;
    rows.iter()
        .map(|row| {
            Ok(Address {
                class: classes.default,
                object: required(row, "oid")?,
                part: 0,
            })
        })
        .collect()
}

/// Whether a numbering change drops the sequence its column was numbered
/// by: an owned sequence's or an identity's the column stops having. A
/// start, increment or name changed keeps it. A plain default given up
/// beside it is an `AlterColumnDefault` of its own.
fn gives_up(from: &pbps_model::ValueSource, to: &pbps_model::ValueSource) -> bool {
    use pbps_model::ValueSource;
    match from {
        ValueSource::Sequence(_) => !matches!(to, ValueSource::Sequence(_)),
        ValueSource::Identity(_) => !matches!(to, ValueSource::Identity(_)),
        ValueSource::None | ValueSource::Default(_) | ValueSource::Generated(_) => false,
    }
}

/// The sequence a column is numbered from: an owned sequence's (`a`) or an
/// identity's (`i`), found by its dependency on the column, as
/// `pg_get_serial_sequence` finds it. By name it would be the base's, which
/// a read that never saw the sequence renamed does not hold (#1631).
async fn numbering_sequence(
    conn: &mut Conn,
    classes: &Classes,
    sources: &PhysicalSources<'_>,
    index: usize,
    table: &TableName,
    column: &str,
) -> Result<Option<Address>, ImpactError> {
    let Some((table, name)) = stored(sources, index, table, Some(column))? else {
        return Ok(None);
    };
    let Some(on) = relation(conn, classes, &table, name.as_deref()).await? else {
        return Ok(None);
    };
    let rows = conn
        .query_with(
            "SELECT d.objid::int8 AS oid FROM pg_catalog.pg_depend d
              JOIN pg_catalog.pg_class s ON s.oid=d.objid AND s.relkind='S'
             WHERE d.classid='pg_catalog.pg_class'::pg_catalog.regclass
               AND d.refclassid='pg_catalog.pg_class'::pg_catalog.regclass
               AND d.refobjid=($1::int8)::oid AND d.refobjsubid=$2::int8
               AND d.deptype IN ('a','i')",
            &[Param::I64(on.object), Param::I64(on.part)],
        )
        .await?;
    rows.first()
        .map(|row| {
            Ok::<Address, DbError>(Address {
                class: classes.relation,
                object: required(row, "oid")?,
                part: 0,
            })
        })
        .transpose()
        .map_err(Into::into)
}

async fn module_address(
    conn: &mut Conn,
    classes: &Classes,
    id: &ModuleId,
    kind: ModuleKind,
) -> Result<Option<Address>, ImpactError> {
    Ok(crate::modules::module_oid(conn, id, kind)
        .await?
        .map(|object| Address {
            class: match kind {
                ModuleKind::View => classes.relation,
                ModuleKind::Function | ModuleKind::Procedure => classes.routine,
                ModuleKind::Trigger => classes.trigger,
            },
            object,
            part: 0,
        }))
}

#[derive(Debug, PartialEq, Eq)]
enum Part<'a> {
    Policy(&'a str),
    Constraint(&'a str),
    Index(&'a str),
    PrimaryKey,
}

async fn table_part(
    conn: &mut Conn,
    classes: &Classes,
    sources: &PhysicalSources<'_>,
    index: usize,
    table: &TableName,
    part: Part<'_>,
) -> Result<Option<Address>, ImpactError> {
    let Some((table, _)) = stored(sources, index, table, None)? else {
        return Ok(None);
    };
    let Some(on) = relation(conn, classes, &table, None).await? else {
        return Ok(None);
    };
    let (query, class, name) = match part {
        Part::Index(name) => (
            "SELECT c.oid::int8 AS oid FROM pg_catalog.pg_index i
         JOIN pg_catalog.pg_class c ON c.oid=i.indexrelid
         WHERE i.indrelid=($1::int8)::oid AND c.relname=$2",
            classes.relation,
            name,
        ),
        Part::Constraint(name) => (
            "SELECT oid::int8 AS oid FROM pg_catalog.pg_constraint
         WHERE conrelid=($1::int8)::oid AND conname=$2",
            classes.constraint,
            name,
        ),
        Part::Policy(name) => (
            "SELECT oid::int8 AS oid FROM pg_catalog.pg_policy
         WHERE polrelid=($1::int8)::oid AND polname=$2",
            classes.policy,
            name,
        ),
        Part::PrimaryKey => (
            "SELECT oid::int8 AS oid FROM pg_catalog.pg_constraint
         WHERE conrelid=($1::int8)::oid AND contype='p' AND $2::text = ''",
            classes.constraint,
            "",
        ),
    };
    let rows = conn
        .query_with(query, &[Param::I64(on.object), Param::Str(name)])
        .await?;
    rows.first()
        .map(|row| {
            Ok::<Address, DbError>(Address {
                class,
                object: required(row, "oid")?,
                part: 0,
            })
        })
        .transpose()
        .map_err(Into::into)
}

async fn read_graph(conn: &mut Conn, roots: &BTreeSet<Address>) -> Result<Graph, DbError> {
    if roots.is_empty() {
        return Ok(Graph(Vec::new()));
    }
    // Numeric catalog addresses only; no declaration text enters this SQL.
    let seeds = roots
        .iter()
        .map(|a| format!("({}::oid,{}::oid,{}::int4)", a.class, a.object, a.part))
        .collect::<Vec<_>>()
        .join(",");
    let query = format!(
        "WITH RECURSIVE reached(classid,objid,objsubid) AS (
        VALUES {seeds}
        UNION
        SELECT next.* FROM reached o CROSS JOIN LATERAL (
            SELECT d.classid,d.objid,d.objsubid FROM pg_catalog.pg_depend d
             WHERE d.refclassid=o.classid AND d.refobjid=o.objid
               AND (o.objsubid=0 OR d.refobjsubid=o.objsubid)
            UNION
            SELECT d.refclassid,d.refobjid,d.refobjsubid FROM pg_catalog.pg_depend d
             WHERE d.classid=o.classid AND d.objid=o.objid
               AND (o.objsubid=0 OR d.objsubid=o.objsubid) AND d.deptype IN ('i','e')
        ) next
    )
    SELECT DISTINCT d.classid::int8 AS dc,d.objid::int8 AS dob,d.objsubid::int8 AS dp,
           d.refclassid::int8 AS rc,d.refobjid::int8 AS rob,d.refobjsubid::int8 AS rp,
           d.deptype::text AS kind,
           pg_catalog.pg_describe_object(d.classid,d.objid,d.objsubid) AS dn,
           pg_catalog.pg_describe_object(d.refclassid,d.refobjid,d.refobjsubid) AS rn
      FROM pg_catalog.pg_depend d
     WHERE EXISTS (SELECT FROM reached o
        WHERE (d.refclassid=o.classid AND d.refobjid=o.objid
               AND (o.objsubid=0 OR d.refobjsubid=o.objsubid))
           OR (d.classid=o.classid AND d.objid=o.objid
               AND (o.objsubid=0 OR d.objsubid=o.objsubid) AND d.deptype IN ('i','e')))"
    );
    let mut edges = Vec::new();
    for row in conn.query(&query).await? {
        let text = |name| {
            row.try_get::<&str>(name)?
                .map(str::to_owned)
                .ok_or_else(|| error(format!("drop_blockers: catalog omitted {name}")))
        };
        edges.push(Edge {
            dependent: Address {
                class: required(&row, "dc")?,
                object: required(&row, "dob")?,
                part: required(&row, "dp")?,
            },
            referenced: Address {
                class: required(&row, "rc")?,
                object: required(&row, "rob")?,
                part: required(&row, "rp")?,
            },
            kind: text("kind")?,
            dependent_name: text("dn")?,
            referenced_name: text("rn")?,
        });
    }
    Ok(Graph(edges))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{ChangeSet, IdsFile, PlannedChange, Schema};

    fn address(object: i64) -> Address {
        Address {
            class: 1,
            object,
            part: 0,
        }
    }

    fn edge(dependent: i64, referenced: i64, kind: &str) -> Edge {
        Edge {
            dependent: address(dependent),
            referenced: address(referenced),
            kind: kind.into(),
            dependent_name: format!("object {dependent}"),
            referenced_name: format!("object {referenced}"),
        }
    }

    #[test]
    fn policy_removals_enter_the_catalog_release_set_but_creations_do_not() {
        let table = TableName::new("app", "t");
        let drop = Change::DropPolicy {
            table: table.clone(),
            name: "gate".into(),
            policy: Default::default(),
        };
        assert_eq!(
            removal_part(&drop),
            Some(Removal::Part(&table, Part::Policy("gate")))
        );
        let create = Change::CreatePolicy {
            table,
            name: "gate".into(),
            policy: Default::default(),
            origin: pbps_model::RowSecurityOrigin::Standing,
        };
        assert_eq!(removal_part(&create), None);
    }

    #[test]
    fn automatic_edges_win_over_normal_edges_but_unknown_classes_still_block() {
        let mut unknown = edge(3, 1, "n");
        unknown.dependent.class = 999;
        let graph = Graph(vec![edge(2, 1, "n"), edge(2, 1, "a"), unknown]);
        assert_eq!(
            graph.blockers(address(1), 0, &[(0, address(1))], &BTreeSet::new()),
            ["object 3"]
        );
        assert!(
            Graph(vec![edge(2, 1, "n"), edge(2, 1, "a")])
                .blockers(address(1), 0, &[(0, address(1))], &BTreeSet::new())
                .is_empty()
        );
    }

    #[test]
    fn internal_view_owners_preserve_transitive_blockers_and_removal_order() {
        // Rule 2 belongs to view 3; rule 4 belongs to view 5.
        let graph = Graph(vec![
            edge(2, 1, "n"),
            edge(2, 3, "i"),
            edge(4, 3, "n"),
            edge(4, 5, "i"),
        ]);
        assert_eq!(
            graph.blockers(address(1), 0, &[(0, address(1))], &BTreeSet::new()),
            ["object 2", "object 4"]
        );
        assert!(
            graph
                .blockers(
                    address(1),
                    2,
                    &[(0, address(5)), (1, address(3)), (2, address(1))],
                    &BTreeSet::new()
                )
                .is_empty()
        );
        assert_eq!(
            graph.blockers(
                address(1),
                2,
                &[(0, address(3)), (1, address(5)), (2, address(1))],
                &BTreeSet::new()
            ),
            ["object 4"]
        );
    }

    #[test]
    fn deleting_an_internal_member_does_not_authorize_deleting_its_owner() {
        let graph = Graph(vec![edge(1, 2, "e")]);
        assert_eq!(
            graph.blockers(address(1), 0, &[(0, address(1))], &BTreeSet::new()),
            ["object 2 (internal owner of object 1)"]
        );
    }

    /// A generated column's expression replaced by `SET EXPRESSION` keeps its
    /// owner: the replaced `pg_attrdef` row (2), owned internally by its
    /// column (3), no longer blocks the drop of the input it read (1). The same
    /// row removed outright still names its owner (DEC-1316.1).
    #[test]
    fn a_replaced_internal_member_keeps_its_owner() {
        let graph = Graph(vec![edge(2, 1, "n"), edge(2, 3, "i")]);
        let removals = [(0, address(2)), (1, address(1))];
        assert!(
            graph
                .blockers(address(1), 1, &removals, &BTreeSet::from([address(2)]))
                .is_empty()
        );
        assert_eq!(
            graph.blockers(address(1), 1, &removals, &BTreeSet::new()),
            ["object 3 (internal owner of object 2)"]
        );
    }

    /// Nor does it reach, through its owner, what depends on the owner: a view
    /// over the generated column (4) still stands after `SET EXPRESSION`, and
    /// is no blocker of the input's drop (DEC-1316.1).
    #[test]
    fn a_replaced_internal_member_does_not_reach_its_owners_dependents() {
        let graph = Graph(vec![edge(2, 1, "n"), edge(2, 3, "i"), edge(4, 3, "n")]);
        let removals = [(0, address(2)), (1, address(1))];
        assert!(
            graph
                .blockers(address(1), 1, &removals, &BTreeSet::from([address(2)]))
                .is_empty()
        );
        assert!(
            graph
                .blockers(address(1), 1, &removals, &BTreeSet::new())
                .contains(&"object 4".to_owned())
        );
    }

    #[test]
    fn stored_names_reverse_only_the_prefix_and_stop_at_a_new_object() {
        let t = TableName::new("app", "t");
        let u = TableName::new("app", "u");
        let cs = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::RenameTable {
                    uid: "t_aaaaaa".parse().unwrap(),
                    from: t.clone(),
                    to: u.clone(),
                    defaults: Vec::new(),
                }),
                PlannedChange::new(Change::RenameColumn {
                    uid: "c_aaaaaa".parse().unwrap(),
                    table: u.clone(),
                    from: "old".into(),
                    to: "new".into(),
                    table_was: None,
                }),
                PlannedChange::new(Change::CreateTable {
                    uid: "t_bbbbbb".parse().unwrap(),
                    name: t.clone(),
                    table: Box::default(),
                    beside_default: None,
                    moves_at_most: None,
                }),
            ],
        };
        let ids = IdsFile {
            tables: [
                ("t_aaaaaa".parse().unwrap(), u.clone()),
                ("t_bbbbbb".parse().unwrap(), t.clone()),
            ]
            .into(),
            columns: [
                ("c_aaaaaa".parse().unwrap(), u.column("new")),
                ("c_bbbbbb".parse().unwrap(), u.column("other")),
            ]
            .into(),
            ..Default::default()
        };
        let names = pbps_model::plan_names::PlanNames::of(&cs.changes, &ids).unwrap();
        let sources = PhysicalSources::of(names, &Schema::default(), &IdsFile::default()).unwrap();
        assert_eq!(
            stored(&sources, 2, &u, Some("new")).unwrap(),
            Some((t.clone(), Some("old".into())))
        );
        assert_eq!(
            stored(&sources, 1, &u, Some("old")).unwrap(),
            Some((t.clone(), Some("old".into())))
        );
        // There is no catalog source for this future spelling. The reader
        // must not invent one from the name of an unrelated occupant.
        assert!(stored(&sources, 1, &u, Some("new")).is_err());
        assert_eq!(
            sources.table_source(&t, At::End),
            Source::Created { step: 2 }
        );
        assert_eq!(
            sources.source(&u.column("other"), At::End),
            Source::Stored(t.column("other"))
        );
    }

    #[test]
    fn catalog_prose_drop_blockers_show_both_owner_operands_and_keep_addresses() {
        let raw = "a\n\u{1b}\u{202e}\\n\\u{1b}é*/";
        let mut ordinary = edge(2, 1, "n");
        ordinary.dependent_name = format!("view {raw}");
        let graph = Graph(vec![ordinary]);
        assert_eq!(
            graph.blockers(address(1), 0, &[(0, address(1))], &BTreeSet::new()),
            [r"view a\n\u{1b}\u{202e}\\n\\u{1b}é*/"]
        );
        assert_eq!(graph.0[0].dependent_name, format!("view {raw}"));
        assert!(
            graph
                .blockers(
                    address(1),
                    1,
                    &[(0, address(2)), (1, address(1))],
                    &BTreeSet::new()
                )
                .is_empty()
        );
        let mut internal = edge(2, 3, "i");
        internal.dependent_name = format!("rule {raw}");
        internal.referenced_name = format!("view {raw}");
        let graph = Graph(vec![internal]);
        assert_eq!(
            graph.blockers(address(2), 0, &[(0, address(2))], &BTreeSet::new()),
            [
                r"view a\n\u{1b}\u{202e}\\n\\u{1b}é*/ (internal owner of rule a\n\u{1b}\u{202e}\\n\\u{1b}é*/)"
            ]
        );
        assert_eq!(graph.0[0].referenced_name, format!("view {raw}"));
        assert!(
            graph
                .blockers(
                    address(2),
                    0,
                    &[(0, address(2))],
                    &BTreeSet::from([address(2)])
                )
                .is_empty()
        );
    }
}
