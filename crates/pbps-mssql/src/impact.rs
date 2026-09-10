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

use pbps_db::{Conn, DbError};
use pbps_dialect::DialectError;
use pbps_model::{Change, ColumnRef, ObjectName, TableName};

use crate::catalog::{get, opt};
use crate::emit::qualified;

/// What is being renamed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameTarget {
    Table(TableName),
    Column(ColumnRef),
    /// A module about to be dropped. Not a rename — but a module rename *is* a
    /// drop plus a create (ADR-0002), and the question the catalog is asked is
    /// the same one: what still refers to this name. Without it the referrers
    /// of a dropped view are never reported and are simply left broken.
    Module(ObjectName),
}

impl RenameTarget {
    /// The table whose dependencies have to be queried. For a column rename it
    /// is the column's table: SQL Server records the dependency against the
    /// object, and a module that names the table is a module that may name this
    /// column.
    pub fn table(&self) -> &TableName {
        match self {
            RenameTarget::Table(t) => t,
            RenameTarget::Column(c) => &c.table,
            // A module has no table; it *is* the object the dependency rows
            // point at, and tables and modules share one namespace.
            RenameTarget::Module(m) => m,
        }
    }

    /// The verb to use when reporting the impact.
    pub fn verb(&self) -> &'static str {
        match self {
            RenameTarget::Table(_) | RenameTarget::Column(_) => "Renaming",
            RenameTarget::Module(_) => "Dropping",
        }
    }

    /// Every rename in a plan, as the objects they are renamed *from* — which is
    /// the name the catalog still knows them by.
    pub fn from_changes(changes: &pbps_model::ChangeSet) -> Vec<RenameTarget> {
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
                | Change::Revoke { .. } => None,
            })
            .collect()
    }
}

impl std::fmt::Display for RenameTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenameTarget::Table(t) => write!(f, "table {t}"),
            RenameTarget::Column(c) => write!(f, "column {c}"),
            RenameTarget::Module(m) => write!(f, "module {m}"),
        }
    }
}

/// One object that refers to the thing being renamed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Referrer {
    /// `view`, `procedure`, `function`, `trigger`, `computed column`, ...
    pub kind: String,
    pub name: String,
    /// Why this one matters, when the kind alone does not say it.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImpactReport {
    pub target: String,

    /// SCHEMABINDING referrers. The engine refuses the rename outright, so this
    /// is not a warning: it is the reason the apply cannot start.
    pub blocking: Vec<Referrer>,

    /// Objects that will break but that the engine will not stop.
    pub advisory: Vec<Referrer>,
}

impl ImpactReport {
    pub fn is_empty(&self) -> bool {
        self.blocking.is_empty() && self.advisory.is_empty()
    }
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

/// Why a rename's impact could not be reported.
///
/// Two failures, kept apart, because the whole point of this module is that an
/// empty report means "nothing depends on this". A name that cannot be quoted
/// and a query that could not run are both *unknown*, and neither may arrive
/// at the caller wearing the shape of "no dependants".
#[derive(Debug, thiserror::Error)]
pub enum ImpactError {
    #[error(transparent)]
    Query(#[from] DbError),

    /// The target's own name cannot be written as an identifier.
    #[error(transparent)]
    Name(#[from] DialectError),
}

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
    use pbps_model::{ChangeSet, PlannedChange};

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
        let targets = RenameTarget::from_changes(&changes);
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
        let targets = RenameTarget::from_changes(&cs);
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
            RenameTarget::from_changes(&cs),
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
