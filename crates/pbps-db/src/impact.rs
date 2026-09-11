//! What connected rename and drop impact queries return (SPEC §7.4–7.5).
//!
//! The question is the same on both engines — what still refers to the name a
//! plan is about to change — and so is the shape of the answer. How the
//! catalog is asked is each engine's: `pbps_mssql::impact` reads
//! `sys.sql_expression_dependencies`, `pbps_pg::impact` reads `pg_depend` and
//! the bodies `pg_proc` keeps as text. Which changes in a plan *are* renames
//! is each engine's too, because a module rename is a drop plus a create
//! (ADR-0002) and only one engine asks the drop side here (the other answers
//! it in `pbps_pg::modules`), so the builder stays with the engine and only
//! the target it builds lives here (DECISIONS 417).
//! Table/column DROP RESTRICT has its own report: an engine carrying a rename
//! into a dependent does not imply that it permits deleting the target.

use pbps_dialect::DialectError;
use pbps_model::{ColumnRef, ObjectName, TableName};

use crate::DbError;

/// Existing catalog dependencies of a table or column the typed plan drops.
/// A replacement may introduce new dependencies; this reports the objects the
/// catalog contains now and whether the plan removes them in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropReport {
    pub change_index: usize,
    pub target: String,
    pub blocking: Vec<String>,
}

/// What is being renamed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameTarget {
    Table(TableName),
    Column(ColumnRef),
    /// A module about to be dropped. Not a rename — but a module rename *is* a
    /// drop plus a create (ADR-0002), and the question the catalog is asked is
    /// the same one: what still refers to this name. Without it the referrers
    /// of a dropped view are never reported and are simply left broken.
    ///
    /// Built by SQL Server's target builder only. PostgreSQL answers the
    /// drop side of a module rename through `pbps_pg::modules`, in full, and
    /// its builder never produces this variant.
    Module(ObjectName),
}

impl RenameTarget {
    /// The table whose dependencies have to be queried. For a column rename it
    /// is the column's table: both engines record the dependency against the
    /// object, and a module that names the table is a module that may name
    /// this column.
    #[must_use]
    pub fn table(&self) -> &TableName {
        match self {
            RenameTarget::Table(t) => t,
            RenameTarget::Column(c) => &c.table,
            // A module has no table; it *is* the object the dependency rows
            // point at, and tables and modules share one namespace.
            RenameTarget::Module(m) => m,
        }
    }

    /// The column, where the target is one.
    #[must_use]
    pub fn column(&self) -> Option<&str> {
        match self {
            RenameTarget::Table(_) | RenameTarget::Module(_) => None,
            RenameTarget::Column(c) => Some(&c.name),
        }
    }

    /// The verb to use when reporting the impact.
    #[must_use]
    pub fn verb(&self) -> &'static str {
        match self {
            RenameTarget::Table(_) | RenameTarget::Column(_) => "Renaming",
            RenameTarget::Module(_) => "Dropping",
        }
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
    /// `view`, `procedure`, `function`, `trigger`, `computed column`, `index`,
    /// `constraint`, ...
    pub kind: String,
    pub name: String,
    /// Why this one matters, when the kind alone does not say it.
    pub detail: Option<String>,
}

/// Everything the catalog says depends on the rename target.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImpactReport {
    /// The target, as the engine names it.
    pub target: String,
    /// Referrers the engine refuses the statement over — a `SCHEMABINDING`
    /// view on SQL Server. Empty on PostgreSQL for a rename, measured; the
    /// field is the abstraction's, and `pbps_pg::impact` says why nothing
    /// fills it there.
    pub blocking: Vec<Referrer>,
    /// Objects that will break, which the engine will not stop.
    pub advisory: Vec<Referrer>,
    /// Objects the rename is carried into, which keep working: a PostgreSQL
    /// view or constraint bound by identity rather than by name. SQL Server
    /// carries nothing — every referrer there is text, and text breaks — so
    /// this is empty from that engine, and empty means "none", not
    /// "not asked".
    pub carried: Vec<Referrer>,
}

impl ImpactReport {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocking.is_empty() && self.advisory.is_empty() && self.carried.is_empty()
    }
}

/// Why a rename's impact could not be reported.
///
/// Two failures kept apart, because the whole point of an impact report is
/// that an empty one means "nothing depends on this". A name that cannot be
/// written and a query that could not run are both *unknown*, and neither may
/// arrive at a caller wearing the shape of "no dependants".
#[derive(Debug, thiserror::Error)]
pub enum ImpactError {
    #[error(transparent)]
    Query(#[from] DbError),

    /// The target cannot be named: its own name cannot be written as an
    /// identifier, or this database has no such column to report on. Both are
    /// questions that could not be asked, and neither is an answer.
    #[error(transparent)]
    Name(#[from] DialectError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(s: &str) -> TableName {
        s.parse().unwrap()
    }

    #[test]
    fn every_target_names_the_object_the_catalog_is_asked_about() {
        let t = RenameTarget::Table(table("app.customer"));
        let c = RenameTarget::Column(ColumnRef::new(table("app.customer"), "email"));
        let m = RenameTarget::Module("app.v_customer".parse().unwrap());
        assert_eq!(t.table(), &table("app.customer"));
        assert_eq!(c.table(), &table("app.customer"));
        assert_eq!(m.table(), &table("app.v_customer"));
        assert_eq!(c.column(), Some("email"));
        assert_eq!(t.column(), None);
        assert_eq!(m.column(), None);
    }

    #[test]
    fn a_dropped_module_is_reported_as_a_drop_and_a_rename_as_a_rename() {
        let m = RenameTarget::Module("app.v".parse().unwrap());
        let t = RenameTarget::Table(table("app.t"));
        assert_eq!(m.verb(), "Dropping");
        assert_eq!(t.verb(), "Renaming");
        assert_eq!(m.to_string(), "module app.v");
        assert_eq!(t.to_string(), "table app.t");
    }

    /// A report with only carried referrers is not empty: the operator has to
    /// see what the engine carried the rename into, or a view that kept
    /// working reads as one nobody checked.
    #[test]
    fn a_report_with_only_carried_referrers_is_not_empty() {
        let mut report = ImpactReport::default();
        assert!(report.is_empty());
        report.carried.push(Referrer {
            kind: "view".into(),
            name: "app.v".into(),
            detail: None,
        });
        assert!(!report.is_empty());
    }
}
