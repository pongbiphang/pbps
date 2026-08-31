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

use pbps_db::{Conn, DbError};
use pbps_model::{Change, ColumnRef, TableName};

use crate::catalog::{get, opt};

/// What is being renamed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameTarget {
    Table(TableName),
    Column(ColumnRef),
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
        }
    }

    /// Every rename in a plan, as the objects they are renamed *from* — which is
    /// the name the catalog still knows them by.
    pub fn from_changes(changes: &pbps_model::ChangeSet) -> Vec<RenameTarget> {
        changes
            .changes
            .iter()
            .filter_map(|p| match &p.change {
                Change::RenameTable { from, .. } => Some(RenameTarget::Table(from.clone())),
                Change::RenameColumn { table, from, .. } => {
                    Some(RenameTarget::Column(table.column(from)))
                }
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
                | Change::DropIndex { .. } => None,
            })
            .collect()
    }
}

impl std::fmt::Display for RenameTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenameTarget::Table(t) => write!(f, "table {t}"),
            RenameTarget::Column(c) => write!(f, "column {c}"),
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

const DEPENDENCIES: &str = "\
SELECT o.type AS type_code, s.name AS schema_name, o.name AS object_name,
       CONVERT(bit, ISNULL(m.is_schema_bound, 0)) AS schema_bound
  FROM sys.sql_expression_dependencies d
  JOIN sys.objects o ON o.object_id = d.referencing_id
  JOIN sys.schemas s ON s.schema_id = o.schema_id
  LEFT JOIN sys.sql_modules m ON m.object_id = o.object_id
 WHERE d.referenced_id = OBJECT_ID(@P1)
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

/// Queries every impact of one rename.
pub async fn rename_impact(
    conn: &mut Conn,
    target: &RenameTarget,
) -> Result<ImpactReport, DbError> {
    let table = target.table().to_string();
    let mut report = ImpactReport {
        target: target.to_string(),
        ..Default::default()
    };

    for row in conn.query_with(DEPENDENCIES, &[&table.as_str()]).await? {
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
        .query_with(COMPUTED_COLUMNS, &[&table.as_str()])
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

    for row in conn.query_with(EXPRESSIONS, &[&table.as_str()]).await? {
        let definition: Option<&str> = opt(&row, "definition")?;
        if mentions(definition.unwrap_or_default(), &column.name) {
            report.advisory.push(Referrer {
                kind: get::<&str>(&row, "kind")?.to_owned(),
                name: get::<&str>(&row, "name")?.to_owned(),
                detail: definition.map(str::to_owned),
            });
        }
    }

    for row in conn.query_with(NAMED_OBJECTS, &[&table.as_str()]).await? {
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
    if definition.contains(&format!("[{column}]")) {
        return true;
    }
    let bytes = definition.as_bytes();
    let mut from = 0;
    while let Some(offset) = definition[from..].find(column) {
        let start = from + offset;
        let end = start + column.len();
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end == bytes.len() || !is_ident_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'@' || b == b'#'
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{ChangeSet, PlannedChange};

    fn tname(s: &str) -> TableName {
        s.parse().unwrap()
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
                RenameTarget::Column("dbo.customer.email".parse().unwrap()),
            ],
            "only renames, and by their old names"
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
