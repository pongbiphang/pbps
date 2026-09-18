//! What a rename breaks (SPEC §7.4).
//!
//! # Why this is dialect knowledge and not a shared helper
//!
//! PostgreSQL resolves dependencies at creation time, so `RENAME COLUMN`
//! rewrites the views that referred to the old name and there is almost nothing
//! to report. SQL Server stores module *text*, and `sp_rename` does not touch
//! it: a view selecting `customer_name` keeps selecting `customer_name` after
//! the column is gone, and fails the next time anybody runs it. The rename is
//! the same operation with an entirely different blast radius, which is exactly
//! why the abstraction puts this behind the dialect.
//!
//! # Blocking versus advisory
//!
//! A SCHEMABINDING view **blocks** the rename: the engine refuses, so reporting
//! it as a warning would be reporting a certainty as a possibility. Everything
//! else is advisory — it will break, but the engine will let the rename through,
//! and whether that is acceptable is a human's call.
//!
//! # What this cannot see
//!
//! Applications, reports and downstream ELT are invisible to any query. The
//! report says so rather than implying the list is complete; §7.4 puts a
//! checklist in front of a human for exactly that reason.

use std::collections::BTreeMap;

use pbps_db::Conn;
use pbps_dialect::DialectError;
use pbps_model::{Change, TableName};

use crate::catalog::{get, opt};
use crate::emit::qualified;

// The target, the referrer, the report and its error are `pbps-db`'s: one
// shape, filled by each engine's own catalog queries (DECISIONS 417). Which
// changes in a plan are rename targets stays this engine's question —
// `rename_targets` below — because only this engine asks the drop side of a
// module rename here.
pub use pbps_db::impact::{ImpactError, ImpactReport, Referrer, RenameTarget};

/// The original table at one statement's position. Reversing only the prefix
/// keeps a name reused by CREATE from resolving to the previous occupant.
fn stored_key_table(
    cs: &pbps_model::ChangeSet,
    index: usize,
    table: &TableName,
) -> Option<TableName> {
    let mut name = table.clone();
    for p in cs.changes[..index].iter().rev() {
        if let Change::RenameTable { from, to, .. } = &p.change
            && to == &name
        {
            name = from.clone();
        } else if let Change::CreateTable { name: created, .. } = &p.change
            && created == &name
        {
            return None;
        }
    }
    Some(name)
}

/// The original column at one statement's position, by identity rather than by
/// name. A `RenameColumn` runs at rank 3 and an `AlterColumnType` at 9, so by
/// the time the `ALTER` runs the column has its declared name — but this check
/// runs before any statement, against a catalog that still has the old one.
///
/// Matched on the column's uid, which both changes carry: a plan may rename
/// two columns into each other's names, and walking the chain backwards by uid
/// cannot follow the wrong one.
fn stored_key_column(
    cs: &pbps_model::ChangeSet,
    index: usize,
    uid: &pbps_model::Uid,
    declared: &str,
) -> String {
    let mut name = declared.to_owned();
    for p in cs.changes[..index].iter().rev() {
        if let Change::RenameColumn {
            uid: renamed, from, ..
        } = &p.change
            && renamed == uid
        {
            name = from.clone();
        }
    }
    name
}

/// Existing inbound FK dependencies of constraint or standalone unique keys.
/// Called in the planning/preflight transaction; it never tries DDL to learn
/// whether a drop is legal. The engine's key_index_id, not a column-set match,
/// decides which standing key an external foreign key actually depends on.
pub async fn key_drop_blockers(
    conn: &mut Conn,
    cs: &pbps_model::ChangeSet,
) -> Result<Vec<pbps_db::impact::DropReport>, ImpactError> {
    use pbps_db::{DbError, Param};
    use std::collections::BTreeSet;

    let targets: Vec<_> = cs
        .changes
        .iter()
        .enumerate()
        .filter_map(|(index, p)| {
            let (table, kind, name) = match &p.change {
                Change::SetPrimaryKey {
                    table,
                    from: Some(_),
                    ..
                } => (table, "PK", ""),
                Change::DropUnique { table, name } => (table, "UQ", name.as_str()),
                Change::DropIndex { table, name } => (table, "IX", name.as_str()),
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
                | Change::AddForeignKey { .. }
                | Change::DropForeignKey { .. }
                | Change::AddCheck { .. }
                | Change::DropCheck { .. }
                | Change::AddIndex { .. }
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
                | Change::Revoke { .. }
                | Change::PublicExecution { .. } => return None,
            };
            let stored = stored_key_table(cs, index, table)?;
            Some((index, stored, kind, name))
        })
        .collect();
    struct Key {
        change_index: usize,
        table: TableName,
        name: String,
        parent: i32,
        index: i32,
    }
    let mut keys = Vec::new();
    for (change_index, table, kind, name) in targets {
        let qualified = qualified(&table)?;
        let rows = conn
            .query_with(
                "SELECT i.object_id AS parent, i.index_id AS idx, i.name, i.is_unique, i.has_filter
             FROM sys.indexes i LEFT JOIN sys.key_constraints kc
               ON kc.parent_object_id=i.object_id AND kc.unique_index_id=i.index_id
             WHERE i.object_id=OBJECT_ID(@P1, N'U')
               AND ((@P2=N'PK' AND kc.type=N'PK') OR (@P2=N'UQ' AND kc.type=N'UQ' AND kc.name=@P3)
                 OR (@P2=N'IX' AND i.name=@P3))",
                &[Param::Str(&qualified), Param::Str(kind), Param::Str(name)],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(DbError::Refused(format!(
                "key_drop_blockers: {kind} `{name}` on {table} is absent from the readable catalog"
            ))
            .into());
        };
        // Neither a nonunique nor a filtered index can back a foreign key.
        // Resolve that with the parent's metadata rights before asking for
        // broader visibility (decision 460).
        if !get::<bool>(row, "is_unique")? || get::<bool>(row, "has_filter")? {
            continue;
        }
        keys.push(Key {
            change_index,
            table,
            name: get::<&str>(row, "name")?.into(),
            parent: get::<i32>(row, "parent")?,
            index: get::<i32>(row, "idx")?,
        });
    }
    // The columns whose type change the engine will not perform while a
    // foreign key stands on them, whether or not the same change also drops a
    // key. A widening that keeps its key and index (`keys_and_indexes` false)
    // has no key drop for the loop above to inspect, and the differ's own
    // foreign-key maintenance reaches only the *managed* projection — so an
    // inbound key from a table this project does not declare is nobody's
    // question until the `ALTER` runs and the engine refuses it. Measured on
    // 17.0.4075.5: widening `varchar(10)` to `varchar(20)` under an external
    // inbound key fails with 5074 naming the constraint, then 4922; the child
    // side of the same key fails the same way, and the same column with no key
    // on it widens (DECISIONS 515).
    struct Retyped {
        change_index: usize,
        table: TableName,
        column: String,
    }
    let mut retypes = Vec::new();
    for (index, p) in cs.changes.iter().enumerate() {
        let Change::AlterColumnType {
            uid,
            column,
            from,
            to,
            ..
        } = &p.change
        else {
            continue;
        };
        if !crate::types::retype_dependents(from, to).foreign_keys {
            continue;
        }
        let Some(table) = stored_key_table(cs, index, &column.table) else {
            continue;
        };
        retypes.push(Retyped {
            change_index: index,
            table,
            column: stored_key_column(cs, index, uid, &column.name),
        });
    }
    if keys.is_empty() && retypes.is_empty() {
        return Ok(Vec::new());
    }

    // ALTER plus VIEW DEFINITION on the parent still hides an FK on a child
    // with no grants. Measured: zero sys.foreign_keys rows until database-wide
    // VIEW DEFINITION is granted. An invisible external child is not absent.
    let visibility = conn
        .query("SELECT HAS_PERMS_BY_NAME(DB_NAME(), 'DATABASE', 'VIEW DEFINITION') AS visible")
        .await?;
    if visibility
        .first()
        .map(|row| get::<i32>(row, "visible"))
        .transpose()?
        != Some(1)
    {
        return Err(DbError::Refused("key_drop_blockers requires database VIEW DEFINITION to see foreign keys outside the managed tables".into()).into());
    }
    // A database grant does not override an object/schema DENY. Its own
    // permission row remains visible even when the child FK disappears, also
    // through role membership. Effective permission keeps owner/sysadmin
    // overrides from being mistaken for denials (decision 460).
    let denials = conn.query(
        "SELECT dp.class, dp.major_id FROM sys.database_permissions dp
         WHERE dp.state=N'D' AND dp.permission_name IN (N'VIEW DEFINITION',N'CONTROL')
           AND (dp.grantee_principal_id=USER_ID() OR IS_MEMBER(USER_NAME(dp.grantee_principal_id))=1)
           AND ((dp.class=1 AND COALESCE(HAS_PERMS_BY_NAME(
                    QUOTENAME(OBJECT_SCHEMA_NAME(dp.major_id))+N'.'+QUOTENAME(OBJECT_NAME(dp.major_id)),
                    'OBJECT','VIEW DEFINITION'),0)<>1)
             OR (dp.class=3 AND COALESCE(HAS_PERMS_BY_NAME(SCHEMA_NAME(dp.major_id),
                    'SCHEMA','VIEW DEFINITION'),0)<>1))"
    ).await?;
    if !denials.is_empty() {
        return Err(DbError::Refused("key_drop_blockers cannot prove external dependencies are visible: an effective object/schema metadata DENY overrides database VIEW DEFINITION".into()).into());
    }
    let mut removals = Vec::new();
    for (index, p) in cs.changes.iter().enumerate() {
        let (table, fk) = match &p.change {
            Change::DropForeignKey { table, name } => (table, Some(name)),
            Change::DropTable { name, .. } => (name, None),
            Change::CreateTable { .. }
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
            | Change::Revoke { .. }
            | Change::PublicExecution { .. } => continue,
        };
        let Some(table) = stored_key_table(cs, index, table) else {
            continue;
        };
        let qualified = qualified(&table)?;
        let rows = if let Some(name) = fk {
            conn.query_with("SELECT fk.object_id AS oid, fk.parent_object_id AS parent FROM sys.foreign_keys fk WHERE fk.parent_object_id=OBJECT_ID(@P1, N'U') AND fk.name=@P2", &[Param::Str(&qualified), Param::Str(name)]).await?
        } else {
            conn.query_with("SELECT t.object_id AS oid, t.object_id AS parent FROM sys.tables t WHERE t.object_id=OBJECT_ID(@P1, N'U')", &[Param::Str(&qualified)]).await?
        };
        for row in rows {
            removals.push((
                index,
                get::<i32>(&row, "oid")?,
                get::<i32>(&row, "parent")?,
                fk.is_none(),
            ));
        }
    }
    let mut reports = Vec::new();
    for Key {
        change_index: index,
        table,
        name: key_name,
        parent,
        index: idx,
    } in keys
    {
        let mut blocking = BTreeSet::new();
        for row in conn.query_with(
            "SELECT fk.object_id AS oid, fk.parent_object_id AS child, s.name AS schema_name, t.name AS table_name, fk.name AS name
             FROM sys.foreign_keys fk JOIN sys.tables t ON t.object_id=fk.parent_object_id JOIN sys.schemas s ON s.schema_id=t.schema_id
             WHERE fk.referenced_object_id=@P1 AND fk.key_index_id=@P2",
            &[Param::I32(parent), Param::I32(idx)],
        ).await? {
            let oid = get::<i32>(&row, "oid")?;
            let child = get::<i32>(&row, "child")?;
            if !removals.iter().any(|&(at, removed, on, whole)| at < index && if whole { on == child } else { removed == oid }) {
                let child = TableName::new(get::<&str>(&row, "schema_name")?, get::<&str>(&row, "table_name")?);
                blocking.insert(format!("foreign key `{}` on {child}", get::<&str>(&row, "name")?));
            }
        }
        reports.push(pbps_db::impact::DropReport {
            change_index: index,
            target: format!("key `{key_name}` on {table}"),
            blocking: blocking.into_iter().collect(),
        });
    }
    for Retyped {
        change_index: index,
        table,
        column,
    } in retypes
    {
        let qualified = qualified(&table)?;
        let mut blocking = BTreeSet::new();
        // A column the catalog does not have is a failed read, not a report of
        // no dependencies (decision 460). A table this plan creates has
        // already left `retypes` at `stored_key_table`, so an absent column
        // here means the name this check reversed is not the one the
        // environment has, and answering "nothing blocks it" would be a guess.
        let located = conn
            .query_with(
                "SELECT OBJECT_ID(@P1, N'U') AS obj,
                    COLUMNPROPERTY(OBJECT_ID(@P1, N'U'), @P2, 'ColumnId') AS col",
                &[Param::Str(&qualified), Param::Str(&column)],
            )
            .await?;
        let located = located.first().ok_or_else(|| {
            DbError::Refused(format!(
                "key_drop_blockers: column `{column}` on {table} could not be located"
            ))
        })?;
        let (Some(object), Some(column_id)) =
            (opt::<i32>(located, "obj")?, opt::<i32>(located, "col")?)
        else {
            return Err(DbError::Refused(format!(
                "key_drop_blockers: column `{column}` on {table} is absent from the readable catalog"
            ))
            .into());
        };
        // Both directions, because both refuse: the key that references this
        // column and the key this column is part of. A managed key of either
        // kind is dropped by the plan's own retype maintenance and filtered
        // out below with everything else it removes; what is left is a key
        // this project does not declare.
        let rows = conn
            .query_with(
                "SELECT fk.object_id AS oid, fk.parent_object_id AS child,
                    s.name AS schema_name, t.name AS table_name, fk.name AS name
             FROM sys.foreign_key_columns fkc
             JOIN sys.foreign_keys fk ON fk.object_id = fkc.constraint_object_id
             JOIN sys.tables t ON t.object_id = fk.parent_object_id
             JOIN sys.schemas s ON s.schema_id = t.schema_id
             WHERE (fkc.referenced_object_id = @P1 AND fkc.referenced_column_id = @P2)
                OR (fkc.parent_object_id = @P1 AND fkc.parent_column_id = @P2)",
                &[Param::I32(object), Param::I32(column_id)],
            )
            .await?;
        for row in rows {
            let oid = get::<i32>(&row, "oid")?;
            let child = get::<i32>(&row, "child")?;
            if !removals.iter().any(|&(at, removed, on, whole)| {
                at < index && if whole { on == child } else { removed == oid }
            }) {
                let child = TableName::new(
                    get::<&str>(&row, "schema_name")?,
                    get::<&str>(&row, "table_name")?,
                );
                blocking.insert(format!(
                    "foreign key `{}` on {child}",
                    get::<&str>(&row, "name")?
                ));
            }
        }
        reports.push(pbps_db::impact::DropReport {
            change_index: index,
            target: format!("column `{column}` on {table}"),
            blocking: blocking.into_iter().collect(),
        });
    }
    Ok(reports)
}

/// Every rename in a plan, as the objects they are renamed *from* — which is
/// the name the catalog still knows them by.
pub fn rename_targets(changes: &pbps_model::ChangeSet) -> Vec<RenameTarget> {
    // A RenameColumn carries the declared, post-rename table because its
    // statement runs after RenameTable. Impact runs before either one, so
    // translate through a map built over the whole plan rather than rely on
    // the changes' current ordering (DECISIONS 416).
    let mut stored: BTreeMap<&TableName, &TableName> = BTreeMap::new();
    for p in &changes.changes {
        if let Change::RenameTable { from, to, .. } = &p.change {
            stored.insert(to, from);
        }
    }
    changes
        .changes
        .iter()
        .filter_map(|p| match &p.change {
            Change::RenameTable { from, .. } => Some(RenameTarget::Table(from.clone())),
            Change::RenameColumn { table, from, .. } => {
                let table = stored.get(table).map_or(table, |t| *t);
                Some(RenameTarget::Column(table.column(from)))
            }
            // A module rename reaches the plan as this drop plus a create,
            // so the drop side is where the catalog has to be asked what
            // still points at the old name.
            Change::DropModule { id, .. } => Some(RenameTarget::Module(id.object_name())),
            // Exhaustive rather than `_`: a change added later that also
            // moves a name must be considered here, and a catch-all would
            // let it through silently.
            Change::CreateTable { .. }
            | Change::DropTable { .. }
            | Change::AddColumn { .. }
            | Change::DropColumn { .. }
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
            | Change::CreateModule { .. }
            | Change::AlterModule { .. }
            // Row changes move no name: a row's identity is its key, and a
            // changed key is a delete plus an insert, not a rename.
            | Change::InsertRow { .. }
            | Change::UpdateRow { .. }
            | Change::DeleteRow { .. }
            | Change::SetDataMode { .. }
            | Change::CreateRole { .. }
            | Change::DropRole { .. }
            | Change::RenameRole { .. }
            | Change::Grant { .. }
            | Change::Revoke { .. }
            | Change::PublicExecution { .. } => None,
        })
        .collect()
}

/// `DISTINCT` is not tidiness: the catalog holds one row per referenced
/// *column*, so a view naming three columns of the table appears three times,
/// and a report that listed it three times would read like three problems.
///
/// The type filter keeps constraints out. A check constraint genuinely depends
/// on its table, so it turns up here — but whether it is affected depends on
/// which column its text names, which is what [`EXPRESSIONS`] decides. Letting
/// both report it would flag every constraint on the table for every rename.
const DEPENDENCIES_TABLE: &str = "\
SELECT DISTINCT o.type AS type_code, s.name AS schema_name, o.name AS object_name,
       CONVERT(bit, ISNULL(m.is_schema_bound, 0)) AS schema_bound
  FROM sys.sql_expression_dependencies d
  JOIN sys.objects o ON o.object_id = d.referencing_id
  JOIN sys.schemas s ON s.schema_id = o.schema_id
  LEFT JOIN sys.sql_modules m ON m.object_id = o.object_id
 WHERE d.referenced_id = OBJECT_ID(@P1)
   AND o.type IN ('V', 'P', 'PC', 'FN', 'IF', 'TF', 'FS', 'FT', 'TR')
 ORDER BY s.name, o.name;";

/// The same, narrowed to modules that name *this column*.
///
/// `referenced_minor_id = 0` is a reference to the object as a whole (`SELECT
/// *`), which a column rename does affect. Anything else is a specific column,
/// and a module naming a different one keeps working — reporting it would flag
/// a rename as risky when it is not, and a report that blocks valid deploys is
/// one people learn to override.
const DEPENDENCIES_COLUMN: &str = "\
SELECT DISTINCT o.type AS type_code, s.name AS schema_name, o.name AS object_name,
       CONVERT(bit, ISNULL(m.is_schema_bound, 0)) AS schema_bound
  FROM sys.sql_expression_dependencies d
  JOIN sys.objects o ON o.object_id = d.referencing_id
  JOIN sys.schemas s ON s.schema_id = o.schema_id
  LEFT JOIN sys.sql_modules m ON m.object_id = o.object_id
 WHERE d.referenced_id = OBJECT_ID(@P1)
   AND o.type IN ('V', 'P', 'PC', 'FN', 'IF', 'TF', 'FS', 'FT', 'TR')
   AND (d.referenced_minor_id = 0
        OR d.referenced_minor_id = COLUMNPROPERTY(OBJECT_ID(@P1), @P2, 'ColumnId'))
 ORDER BY s.name, o.name;";

const COMPUTED_COLUMNS: &str = "\
SELECT c.name AS column_name, c.definition
  FROM sys.computed_columns c
 WHERE c.object_id = OBJECT_ID(@P1)
 ORDER BY c.name;";

/// Checks and defaults together: both store definition text that embeds column
/// names, and both go stale the same way.
const EXPRESSIONS: &str = "\
SELECT 'check constraint' AS kind, cc.name, cc.definition
  FROM sys.check_constraints cc WHERE cc.parent_object_id = OBJECT_ID(@P1)
UNION ALL
SELECT 'default constraint', dc.name, dc.definition
  FROM sys.default_constraints dc WHERE dc.parent_object_id = OBJECT_ID(@P1)
ORDER BY kind, name;";

/// Indexes and constraints whose *name* embeds the old column name. The objects
/// keep working; only the naming goes stale.
const NAMED_OBJECTS: &str = "\
SELECT 'index' AS kind, i.name
  FROM sys.indexes i WHERE i.object_id = OBJECT_ID(@P1) AND i.name IS NOT NULL
UNION ALL
SELECT 'key constraint', kc.name
  FROM sys.key_constraints kc WHERE kc.parent_object_id = OBJECT_ID(@P1)
UNION ALL
SELECT 'foreign key', fk.name
  FROM sys.foreign_keys fk WHERE fk.parent_object_id = OBJECT_ID(@P1)
ORDER BY kind, name;";

/// The name every one of these queries hands to `OBJECT_ID`.
///
/// Its own function so the choice is visible and testable without a server:
/// the argument is one string built once and bound to `@P1` five times, so
/// getting it wrong is not five bugs but one, and it cannot be checked by
/// reading the query text.
fn object_id_argument(target: &RenameTarget) -> Result<String, DialectError> {
    qualified(target.table())
}

/// Queries every impact of one rename.
pub async fn rename_impact(
    conn: &mut Conn,
    target: &RenameTarget,
) -> Result<ImpactReport, ImpactError> {
    // `OBJECT_ID` *parses* its argument as a name, so it is given the quoted
    // form and not `TableName`'s `Display`. It splits on periods, so a name
    // holding one makes the bare `schema.name` a three-part name — `dbo`.
    // `cust`.`omer` — which names a different object and resolves to NULL.
    // Every one of these queries then joins against NULL and matches nothing,
    // so the report came back empty, which reads as "nothing depends on this".
    // The `blocking` list goes with it, and a rename `sp_rename` will refuse
    // because of a SCHEMABINDING view is waved through to fail at apply.
    //
    // A *space* is not one of these: `OBJECT_ID` is tolerant of it and finds
    // the table from the bare form. That is worth writing down, because it is
    // the character a test reaches for first, and a test built on it passes
    // with this fix reverted.
    //
    // `preflight` and `doctor` already pass the quoted form (the latter builds
    // it server-side with `QUOTENAME`); this was the one place that did not.
    //
    // `@P2` is deliberately *not* quoted: `COLUMNPROPERTY` takes a column name
    // as a plain string, not as a name to parse, and bracketing it there would
    // break the one query that works today.
    let table = object_id_argument(target)?;
    let mut report = ImpactReport {
        target: target.to_string(),
        ..Default::default()
    };

    let dependencies = match target {
        RenameTarget::Table(_) | RenameTarget::Module(_) => {
            conn.query_with(DEPENDENCIES_TABLE, &[table.as_str().into()])
                .await?
        }
        RenameTarget::Column(c) => {
            conn.query_with(
                DEPENDENCIES_COLUMN,
                &[table.as_str().into(), c.name.as_str().into()],
            )
            .await?
        }
    };
    for row in dependencies {
        let kind = module_kind(get::<&str>(&row, "type_code")?);
        let name = format!(
            "{}.{}",
            get::<&str>(&row, "schema_name")?,
            get::<&str>(&row, "object_name")?
        );
        let schema_bound: bool = get(&row, "schema_bound")?;
        let referrer = Referrer {
            kind: kind.to_owned(),
            name,
            detail: schema_bound.then(|| "SCHEMABINDING".to_owned()),
        };
        if schema_bound {
            report.blocking.push(referrer);
        } else {
            report.advisory.push(referrer);
        }
    }

    // The remaining sources are all about a column's *name* appearing in stored
    // text, so they have nothing to say about a table rename.
    let RenameTarget::Column(column) = target else {
        return Ok(report);
    };

    for row in conn
        .query_with(COMPUTED_COLUMNS, &[table.as_str().into()])
        .await?
    {
        let definition: Option<&str> = opt(&row, "definition")?;
        if mentions(definition.unwrap_or_default(), &column.name) {
            report.advisory.push(Referrer {
                kind: "computed column".to_owned(),
                name: format!("{}.{}", column.table, get::<&str>(&row, "column_name")?),
                detail: definition.map(str::to_owned),
            });
        }
    }

    for row in conn
        .query_with(EXPRESSIONS, &[table.as_str().into()])
        .await?
    {
        let definition: Option<&str> = opt(&row, "definition")?;
        if mentions(definition.unwrap_or_default(), &column.name) {
            report.advisory.push(Referrer {
                kind: get::<&str>(&row, "kind")?.to_owned(),
                name: get::<&str>(&row, "name")?.to_owned(),
                detail: definition.map(str::to_owned),
            });
        }
    }

    for row in conn
        .query_with(NAMED_OBJECTS, &[table.as_str().into()])
        .await?
    {
        let name = get::<&str>(&row, "name")?;
        if name.contains(column.name.as_str()) {
            report.advisory.push(Referrer {
                kind: get::<&str>(&row, "kind")?.to_owned(),
                name: name.to_owned(),
                detail: Some("the name embeds the old column name".to_owned()),
            });
        }
    }

    Ok(report)
}

/// `sys.objects.type` as a word. Anything unrecognized keeps its code rather
/// than being dropped: an unnamed referrer is still a referrer.
fn module_kind(code: &str) -> &str {
    match code.trim() {
        "V" => "view",
        "P" | "PC" => "procedure",
        "FN" | "IF" | "TF" | "FS" | "FT" => "function",
        "TR" => "trigger",
        other => other,
    }
}

/// Whether a stored definition names this column.
///
/// The engine stores `([amount]>=(0))`, so the bracketed form is the one that
/// actually appears — but a definition written without brackets keeps the bare
/// name, and both have to match. Bare matching is bounded by the characters
/// around it so that `amount` does not match `amount_paid`.
fn mentions(definition: &str, column: &str) -> bool {
    let Some(first) = column.chars().next() else {
        return false;
    };
    if definition.contains(&format!("[{column}]")) {
        return true;
    }
    let mut from = 0;
    while let Some(offset) = definition[from..].find(column) {
        let start = from + offset;
        let end = start + column.len();
        // A UTF-8 continuation byte is not a delimiter. Share the SQL Server
        // lexer's character rule so every scan agrees on where a name ends.
        let before_ok = !definition[..start]
            .chars()
            .next_back()
            .is_some_and(pbps_model::module::is_regular_identifier_continue);
        let after_ok = !definition[end..]
            .chars()
            .next()
            .is_some_and(pbps_model::module::is_regular_identifier_continue);
        if before_ok && after_ok {
            return true;
        }
        from = start + first.len_utf8();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{ChangeSet, PlannedChange, TableName};

    fn tname(s: &str) -> TableName {
        s.parse().unwrap()
    }

    /// `OBJECT_ID` parses its argument as a name, so a name that needs quoting
    /// has to arrive quoted. The bare form returns NULL, every query joins
    /// against NULL, and the report comes back empty — which reads as "nothing
    /// depends on this" and takes the `blocking` list with it.
    #[test]
    fn the_object_id_argument_is_quoted_so_a_name_needing_it_still_resolves() {
        let plain = object_id_argument(&RenameTarget::Table(tname("dbo.customer"))).unwrap();
        assert_eq!(plain, "[dbo].[customer]");

        // Quoted whatever the character is, rather than only for the ones
        // that are known to break the bare form: `OBJECT_ID` resolves
        // `dbo.a b` but not `dbo.a.b`, and which side of that line a
        // character falls on is the server's business, not this function's.
        // Built with `TableName::new` rather than parsed,
        // because that is how such a name actually arrives — `assemble` builds
        // them straight from the catalog, and `from_str` would refuse the one
        // with a period as three segments.
        for name in ["a b", "a.b", "a]b"] {
            let table = TableName::new("dbo", name);
            let arg = object_id_argument(&RenameTarget::Table(table)).unwrap();
            assert!(arg.starts_with("[dbo].["), "{arg}");
            assert!(arg.ends_with(']'), "{arg}");
            // The bare form is what returned NULL. It must not be what is sent.
            assert_ne!(arg, format!("dbo.{name}"));
        }
        // A bracket is doubled rather than passed through, or the argument
        // ends early inside `OBJECT_ID` just as it would in code position.
        assert_eq!(
            object_id_argument(&RenameTarget::Table(TableName::new("dbo", "a]b"))).unwrap(),
            "[dbo].[a]]b]"
        );

        // A column target names its table, not the column: `@P1` locates the
        // object and `@P2` carries the column separately.
        let column = object_id_argument(&RenameTarget::Column(
            "dbo.cust omer.email".parse().unwrap(),
        ))
        .unwrap();
        assert_eq!(column, "[dbo].[cust omer]");
    }

    /// A name that cannot be an identifier is not a report with no rows in it.
    /// The two arms of `ImpactError` exist so that neither reaches a caller
    /// looking like an empty report.
    #[test]
    fn a_name_that_cannot_be_quoted_is_an_error_and_not_an_empty_report() {
        let empty = TableName::new("dbo", "");
        assert!(object_id_argument(&RenameTarget::Table(empty)).is_err());
    }

    /// A module rename reaches the plan as a drop plus a create, so the drop is
    /// where the catalog can still be asked what points at the old name. Left
    /// out, the views and procedures that referred to it are simply broken by
    /// an approved apply, with nothing said beforehand.
    #[test]
    fn a_module_drop_is_a_target_and_a_create_is_not() {
        let changes = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::DropModule {
                    id: "dbo.active_customer".parse().unwrap(),
                    kind: pbps_model::ModuleKind::View,
                }),
                PlannedChange::new(Change::CreateModule {
                    id: "dbo.live_customer".parse().unwrap(),
                    module: Box::new(pbps_model::Module {
                        kind: pbps_model::ModuleKind::View,
                        description: None,
                        definition: "SELECT 1".into(),
                    }),
                }),
            ],
        };
        let targets = rename_targets(&changes);
        assert_eq!(
            targets,
            vec![RenameTarget::Module("dbo.active_customer".parse().unwrap())]
        );
        assert_eq!(targets[0].verb(), "Dropping");
        assert_eq!(targets[0].to_string(), "module dbo.active_customer");
    }

    #[test]
    fn a_column_target_reports_its_own_table() {
        let target = RenameTarget::Column("dbo.customer.email".parse().unwrap());
        assert_eq!(target.table(), &tname("dbo.customer"));
        assert_eq!(target.to_string(), "column dbo.customer.email");
    }

    /// The catalog knows the object by its **old** name, so that is the one to
    /// query with. Taking `to` would query a name that does not exist yet and
    /// report no impact at all — the most dangerous possible answer.
    #[test]
    fn targets_are_taken_from_the_old_names() {
        let cs = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::RenameTable {
                    uid: "t_aaaaaa".parse().unwrap(),
                    from: tname("dbo.client"),
                    to: tname("dbo.customer"),
                }),
                PlannedChange::new(Change::RenameColumn {
                    uid: "c_aaaaaa".parse().unwrap(),
                    table: tname("dbo.customer"),
                    from: "email".into(),
                    to: "contact_email".into(),
                }),
                PlannedChange::new(Change::DropTable {
                    uid: "t_bbbbbb".parse().unwrap(),
                    name: tname("dbo.old"),
                }),
            ],
        };
        let targets = rename_targets(&cs);
        assert_eq!(
            targets,
            [
                RenameTarget::Table(tname("dbo.client")),
                RenameTarget::Column("dbo.client.email".parse().unwrap()),
            ],
            "only renames, and by their old names"
        );
    }

    /// A column-only rename already names the table the catalog has. A reverse
    /// lookup must not rewrite it unless this same plan renames that table.
    #[test]
    fn a_column_rename_without_a_table_rename_keeps_its_table() {
        let cs = ChangeSet {
            changes: vec![PlannedChange::new(Change::RenameColumn {
                uid: "c_aaaaaa".parse().unwrap(),
                table: tname("dbo.customer"),
                from: "email".into(),
                to: "contact_email".into(),
            })],
        };

        assert_eq!(
            rename_targets(&cs),
            [RenameTarget::Column("dbo.customer.email".parse().unwrap())]
        );
    }

    #[test]
    fn the_stored_bracketed_form_is_recognized() {
        assert!(mentions("([amount]>=(0))", "amount"));
        assert!(mentions("amount >= 0", "amount"));
    }

    /// The trap this function exists for: a prefix match would report every
    /// constraint on a similarly named column and train the operator to ignore
    /// the report.
    #[test]
    fn a_longer_name_is_not_a_match() {
        assert!(!mentions("([amount_paid]>=(0))", "amount"));
        assert!(!mentions("amount_paid >= 0", "amount"));
        assert!(!mentions("net_amount >= 0", "amount"));
        assert!(!mentions("@amount >= 0", "amount"));
    }

    #[test]
    fn multibyte_names_advance_at_character_boundaries() {
        assert!(!mentions("SELECT xä FROM t", "ä"));
        assert!(!mentions("SELECT äx FROM t", "ä"));
        assert!(mentions("SELECT ä FROM t", "ä"));
        assert!(mentions("SELECT xä, ä FROM t", "ä"));
        assert!(!mentions("SELECT x FROM t", "ä"));
        assert!(!mentions("", "ä"));
    }

    #[test]
    fn unicode_identifier_neighbors_do_not_become_delimiters() {
        for definition in ["SELECT xää FROM t", "SELECT ääx FROM t", "SELECT ää FROM t"] {
            assert!(!mentions(definition, "ä"), "{definition}");
        }
        for definition in [
            "SELECT ä FROM t",
            "SELECT [ä] FROM t",
            "SELECT xää, ä FROM t",
            "SELECT ääx, ä FROM t",
        ] {
            assert!(mentions(definition, "ä"), "{definition}");
        }
    }

    #[test]
    fn sql_server_identifier_symbols_do_not_split_a_reference() {
        for symbol in ['@', '#', '_', '$'] {
            for definition in [
                format!("SELECT x{symbol}ä FROM t"),
                format!("SELECT ä{symbol}x FROM t"),
            ] {
                assert!(!mentions(&definition, "ä"), "{definition}");
                assert!(mentions(&format!("{definition}; SELECT ä FROM t"), "ä"));
            }
        }
    }

    #[test]
    fn an_empty_column_is_not_a_reference() {
        assert!(!mentions("", ""));
        assert!(!mentions("SELECT [] FROM t", ""));
        assert!(!mentions("SELECT ä FROM t", ""));
    }

    /// The two traps a live run found: one row per referenced column made a
    /// single view look like three problems, and check constraints arrived as
    /// dependencies of their own table, flagging every one of them for every
    /// rename.
    #[test]
    fn the_dependency_queries_deduplicate_and_exclude_constraints() {
        for sql in [DEPENDENCIES_TABLE, DEPENDENCIES_COLUMN] {
            assert!(sql.contains("SELECT DISTINCT"), "{sql}");
            assert!(sql.contains("o.type IN ("), "{sql}");
            assert!(
                !sql.contains("'C'"),
                "constraints are EXPRESSIONS' job: {sql}"
            );
        }
        // Only the column query narrows to a column; a table rename affects
        // every referrer.
        assert!(DEPENDENCIES_COLUMN.contains("referenced_minor_id"));
        assert!(!DEPENDENCIES_TABLE.contains("referenced_minor_id"));
    }

    #[test]
    fn unknown_object_types_keep_their_code() {
        assert_eq!(module_kind("V"), "view");
        assert_eq!(module_kind("TR"), "trigger");
        assert_eq!(module_kind("XX"), "XX");
    }

    #[test]
    fn an_empty_report_knows_it_is_empty() {
        let r = ImpactReport {
            target: "column dbo.customer.email".into(),
            ..Default::default()
        };
        assert!(r.is_empty());
    }
}
