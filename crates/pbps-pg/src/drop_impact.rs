//! DROP RESTRICT over catalog object addresses, not an enumeration of classes.
//! Internal owners preserve the view-rule and row-type paths measured by the
//! module reader; unknown classes and domain constraints cannot fall out of a
//! class-specific join. Automatic removal is distinct from a blocking edge.

use std::collections::{BTreeMap, BTreeSet};

use pbps_db::impact::{DropReport, ImpactError};
use pbps_db::{Conn, DbError, Param, Row};
use pbps_model::{Change, ChangeSet, ModuleId, ModuleKind, TableName};

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
    fn closure(&self, root: Address, automatic: bool) -> BTreeSet<Address> {
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
                if edge.owner()
                    && found.iter().any(|a| a.contains(edge.dependent))
                    && (!automatic || !root.contains(edge.dependent))
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
    ) -> Vec<String> {
        let reachable = self.closure(root, false);
        let mut removed = BTreeMap::<Address, usize>::new();
        for &(index, address) in removals {
            for auto in self.closure(address, true) {
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
                blocking.insert(edge.dependent_name.clone());
            }
            if edge.owner()
                && reachable.iter().any(|a| a.contains(edge.dependent))
                && let Some(at) = position(edge.dependent)
                && position(edge.referenced).is_none_or(|before| before > at)
            {
                blocking.insert(format!(
                    "{} (internal owner of {})",
                    edge.referenced_name, edge.dependent_name
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
    DbError::Driver {
        code: None,
        message,
    }
}

struct Classes {
    relation: i64,
    routine: i64,
    trigger: i64,
    constraint: i64,
    default: i64,
}

impl Classes {
    async fn read(conn: &mut Conn) -> Result<Self, DbError> {
        let rows = conn
            .query(
                "SELECT 'pg_catalog.pg_class'::regclass::int8 AS relation,
            'pg_catalog.pg_proc'::regclass::int8 AS routine,
            'pg_catalog.pg_trigger'::regclass::int8 AS trigger,
            'pg_catalog.pg_constraint'::regclass::int8 AS constraint,
            'pg_catalog.pg_attrdef'::regclass::int8 AS default",
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
        })
    }
}

/// Read the existing blockers for every DropTable/DropColumn. Must run in the
/// caller's transaction; no DDL or catalog mutation is used to test a drop.
pub async fn drop_blockers(
    conn: &mut Conn,
    cs: &ChangeSet,
) -> Result<Vec<DropReport>, ImpactError> {
    let targets: Vec<_> = cs
        .changes
        .iter()
        .enumerate()
        .filter_map(|(index, p)| match &p.change {
            Change::DropTable { name, .. } => Some((index, format!("table {name}"))),
            Change::DropColumn { column, .. } => Some((index, format!("column {column}"))),
            Change::CreateTable { .. }
            | Change::RenameTable { .. }
            | Change::AddColumn { .. }
            | Change::RenameColumn { .. }
            | Change::AlterColumnType { .. }
            | Change::AlterColumnNullability { .. }
            | Change::AlterColumnDefault { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::SetPrimaryKey { .. }
            | Change::AddUnique { .. }
            | Change::DropUnique { .. }
            | Change::AddForeignKey { .. }
            | Change::DropForeignKey { .. }
            | Change::AddCheck { .. }
            | Change::DropCheck { .. }
            | Change::AddIndex { .. }
            | Change::DropIndex { .. }
            | Change::InsertRow { .. }
            | Change::UpdateRow { .. }
            | Change::DeleteRow { .. }
            | Change::SetDataMode { .. }
            | Change::CreateModule { .. }
            | Change::AlterModule { .. }
            | Change::DropModule { .. }
            | Change::CreateRole { .. }
            | Change::DropRole { .. }
            | Change::RenameRole { .. }
            | Change::Grant { .. }
            | Change::Revoke { .. } => None,
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
    for index in 0..=last {
        if let Some(address) = removal(conn, &classes, cs, index).await? {
            removals.push((index, address));
        }
    }
    let roots: BTreeSet<_> = removals.iter().map(|(_, a)| *a).collect();
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
                graph.blockers(*root, index, &prior)
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

/// Reverse only earlier renames: these names describe statement-time objects,
/// while the read still sees the original catalog. Return None for a table
/// created earlier by this plan instead of querying an unrelated old occupant.
fn stored(
    cs: &ChangeSet,
    index: usize,
    table: &TableName,
    column: Option<&str>,
) -> Option<(TableName, Option<String>)> {
    let mut table = table.clone();
    let mut column = column.map(str::to_owned);
    for p in cs.changes[..index].iter().rev() {
        match &p.change {
            Change::RenameColumn {
                table: on,
                from,
                to,
                ..
            } if on == &table && column.as_ref() == Some(to) => column = Some(from.clone()),
            Change::RenameTable { from, to, .. } if to == &table => table = from.clone(),
            Change::CreateTable { name, .. } if name == &table => return None,
            Change::AddColumn {
                table: on, name, ..
            } if on == &table && column.as_ref() == Some(name) => return None,
            Change::CreateTable { .. }
            | Change::DropTable { .. }
            | Change::RenameTable { .. }
            | Change::AddColumn { .. }
            | Change::DropColumn { .. }
            | Change::RenameColumn { .. }
            | Change::AlterColumnType { .. }
            | Change::AlterColumnNullability { .. }
            | Change::AlterColumnDefault { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::SetPrimaryKey { .. }
            | Change::AddUnique { .. }
            | Change::DropUnique { .. }
            | Change::AddForeignKey { .. }
            | Change::DropForeignKey { .. }
            | Change::AddCheck { .. }
            | Change::DropCheck { .. }
            | Change::AddIndex { .. }
            | Change::DropIndex { .. }
            | Change::InsertRow { .. }
            | Change::UpdateRow { .. }
            | Change::DeleteRow { .. }
            | Change::SetDataMode { .. }
            | Change::CreateModule { .. }
            | Change::AlterModule { .. }
            | Change::DropModule { .. }
            | Change::CreateRole { .. }
            | Change::DropRole { .. }
            | Change::RenameRole { .. }
            | Change::Grant { .. }
            | Change::Revoke { .. } => {}
        }
    }
    Some((table, column))
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

async fn removal(
    conn: &mut Conn,
    classes: &Classes,
    cs: &ChangeSet,
    index: usize,
) -> Result<Option<Address>, ImpactError> {
    let p = &cs.changes[index];
    match &p.change {
        Change::DropTable { name, .. } => {
            let Some((table, _)) = stored(cs, index, name, None) else {
                return Ok(None);
            };
            relation(conn, classes, &table, None)
                .await?
                .map(Some)
                .ok_or_else(|| {
                    error(format!(
                        "drop_blockers: table {table} is absent from the catalog"
                    ))
                    .into()
                })
        }
        Change::DropColumn { column, .. } => {
            let Some((table, name)) = stored(cs, index, &column.table, Some(&column.name)) else {
                return Ok(None);
            };
            relation(conn, classes, &table, name.as_deref())
                .await?
                .map(Some)
                .ok_or_else(|| {
                    error(format!(
                        "drop_blockers: column {column} is absent from the catalog"
                    ))
                    .into()
                })
        }
        Change::DropModule { id, kind } => module_address(conn, classes, id, *kind).await,
        Change::AlterModule { id, module } => module_address(conn, classes, id, module.kind).await,
        Change::DropCheck { table, name }
        | Change::DropUnique { table, name }
        | Change::DropForeignKey { table, name } => {
            table_part(conn, classes, cs, index, table, Part::Constraint(name)).await
        }
        Change::DropIndex { table, name } => {
            table_part(conn, classes, cs, index, table, Part::Index(name)).await
        }
        Change::SetPrimaryKey {
            table,
            from: Some(_),
            ..
        } => table_part(conn, classes, cs, index, table, Part::PrimaryKey).await,
        Change::AlterColumnDefault {
            column,
            from: Some(_),
            ..
        } => {
            let Some((table, name)) = stored(cs, index, &column.table, Some(&column.name)) else {
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
        Change::CreateTable { .. }
        | Change::RenameTable { .. }
        | Change::AddColumn { .. }
        | Change::RenameColumn { .. }
        | Change::AlterColumnType { .. }
        | Change::AlterColumnNullability { .. }
        | Change::AlterColumnDefault { .. }
        | Change::SetColumnDeprecated { .. }
        | Change::SetPrimaryKey { .. }
        | Change::AddUnique { .. }
        | Change::AddForeignKey { .. }
        | Change::AddCheck { .. }
        | Change::AddIndex { .. }
        | Change::InsertRow { .. }
        | Change::UpdateRow { .. }
        | Change::DeleteRow { .. }
        | Change::SetDataMode { .. }
        | Change::CreateModule { .. }
        | Change::CreateRole { .. }
        | Change::DropRole { .. }
        | Change::RenameRole { .. }
        | Change::Grant { .. }
        | Change::Revoke { .. } => Ok(None),
    }
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

enum Part<'a> {
    Constraint(&'a str),
    Index(&'a str),
    PrimaryKey,
}

async fn table_part(
    conn: &mut Conn,
    classes: &Classes,
    cs: &ChangeSet,
    index: usize,
    table: &TableName,
    part: Part<'_>,
) -> Result<Option<Address>, ImpactError> {
    let Some((table, _)) = stored(cs, index, table, None) else {
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
    use pbps_model::PlannedChange;

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
    fn automatic_edges_win_over_normal_edges_but_unknown_classes_still_block() {
        let mut unknown = edge(3, 1, "n");
        unknown.dependent.class = 999;
        let graph = Graph(vec![edge(2, 1, "n"), edge(2, 1, "a"), unknown]);
        assert_eq!(
            graph.blockers(address(1), 0, &[(0, address(1))]),
            ["object 3"]
        );
        assert!(
            Graph(vec![edge(2, 1, "n"), edge(2, 1, "a")])
                .blockers(address(1), 0, &[(0, address(1))])
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
            graph.blockers(address(1), 0, &[(0, address(1))]),
            ["object 2", "object 4"]
        );
        assert!(
            graph
                .blockers(
                    address(1),
                    2,
                    &[(0, address(5)), (1, address(3)), (2, address(1))]
                )
                .is_empty()
        );
        assert_eq!(
            graph.blockers(
                address(1),
                2,
                &[(0, address(3)), (1, address(5)), (2, address(1))]
            ),
            ["object 4"]
        );
    }

    #[test]
    fn deleting_an_internal_member_does_not_authorize_deleting_its_owner() {
        let graph = Graph(vec![edge(1, 2, "e")]);
        assert_eq!(
            graph.blockers(address(1), 0, &[(0, address(1))]),
            ["object 2 (internal owner of object 1)"]
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
                }),
                PlannedChange::new(Change::RenameColumn {
                    uid: "c_aaaaaa".parse().unwrap(),
                    table: u.clone(),
                    from: "old".into(),
                    to: "new".into(),
                }),
                PlannedChange::new(Change::CreateTable {
                    uid: "t_bbbbbb".parse().unwrap(),
                    name: t.clone(),
                    table: Box::default(),
                }),
            ],
        };
        assert_eq!(
            stored(&cs, 2, &u, Some("new")),
            Some((t.clone(), Some("old".into())))
        );
        assert_eq!(
            stored(&cs, 1, &u, Some("new")),
            Some((t.clone(), Some("new".into())))
        );
        assert_eq!(stored(&cs, 3, &t, None), None);
        assert_eq!(
            stored(&cs, 3, &u, Some("other")),
            Some((t, Some("other".into())))
        );
    }
}
