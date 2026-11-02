//! What connected rename and drop impact queries return (SPEC §7.4–7.5).
//!
//! The question is the same on both engines — what still refers to the name a
//! plan is about to change — and so is the shape of the answer. How the
//! catalog is asked is each engine's: `pbps_mssql::impact` reads
//! `sys.sql_expression_dependencies`, `pbps_pg::impact` reads `pg_depend` and
//! the bodies `pg_proc` keeps as text. The target builder shares the plan
//! traversal and stored-name translation. Each engine chooses whether it also
//! asks the drop side of a module rename here: SQL Server does, while
//! PostgreSQL answers it in `pbps_pg::modules` (ADR-0002, DECISIONS 417).
//! Table/column DROP RESTRICT has its own report: an engine carrying a rename
//! into a dependent does not imply that it permits deleting the target.

use pbps_dialect::DialectError;
use pbps_model::{Change, ColumnRef, ModuleId, ModuleKind, ObjectName, TableName};

use crate::DbError;

/// Existing catalog dependencies of a table, column or key the typed plan drops.
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
            RenameTarget::Column(c) => Some(c.name.raw()),
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
            RenameTarget::Table(t) => write!(f, "table {t}", t = t.shown()),
            RenameTarget::Column(c) => write!(f, "column {c}", c = c.shown()),
            RenameTarget::Module(m) => write!(f, "module {m}", m = m.shown()),
        }
    }
}

/// Impact targets in first-occurrence order, with each renamed identity queried
/// once, named as the catalog knows it before execution.
///
/// Both halves of a renamed column's address must use the stored spelling:
/// its change carries the declared table name, which may itself be renamed.
/// Querying that future address either refuses a valid plan or silently misses
/// dependencies (DECISIONS 407, 416).
///
/// `include_module_drops` is SQL Server's policy: a module rename is a drop
/// plus a create, so its drop is another impact target. PostgreSQL leaves the
/// drop side to its module reader, which already answers that question in full.
#[must_use]
pub fn rename_targets(
    names: &pbps_model::plan_names::PlanNames<'_>,
    include_module_drops: bool,
) -> Vec<RenameTarget> {
    use pbps_model::plan_names::At;
    let mut seen = std::collections::BTreeSet::new();
    names
        .changes()
        .iter()
        .filter_map(|p| match &p.change {
Change::RebuildPartitionedTable(_) | Change::BuildPartitionedIndex(_) | Change::CopyTableRows(_) | Change::AnalyzeTableTree(_) => None,
            Change::RenameTable { uid, .. } => names.table_at(uid, At::Start)
                .filter(|_| seen.insert(uid)).cloned().map(RenameTarget::Table),
            Change::RenameColumn { uid, .. } => {
                let (owner, column) = names.column_at(uid, At::Start)?;
                let table = names.table_at(owner, At::Start)?;
                seen.insert(uid).then(|| RenameTarget::Column(table.column(column)))
            }
            // A module rename reaches the plan as this drop plus a create,
            // so the drop side is where the catalog has to be asked what
            // still points at the old name.
            Change::DropModule { id, .. } => {
                include_module_drops.then(|| RenameTarget::Module(id.object_name()))
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
            | Change::AlterColumnExpression { .. }
            // A sequence it renames is reached from a default or a view by
            // its oid, which the rename keeps; only text names it, and no
            // catalog lists that (#1889).
            | Change::AlterColumnNumbering { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::SetPrimaryKey { .. }
            | Change::SetIndexStorageParameters { .. }
            | Change::SetTablePersistence { .. }
            | Change::SetStorageParameters { .. }
            | Change::DropPolicy { .. }
            | Change::CreatePolicy { .. }
            | Change::SetRowSecurity { .. }
            | Change::SetReplicaIdentity { .. }
            | Change::AddUnique { .. }
            | Change::DropUnique { .. }
            | Change::AddForeignKey { .. }
            | Change::DropForeignKey { .. }
            | Change::AddCheck { .. }
            | Change::DropCheck { .. }
            | Change::AddIndex { .. }
            | Change::AddComputedColumn { .. }
            | Change::DropComputedColumn { .. }
            | Change::DropIndex { .. }
            | Change::CreateModule { .. }
            | Change::AlterModule { .. }
            // Row changes move no name: a row's identity is its key, and a
            // changed key is a delete plus an insert, not a rename.
            | Change::InsertRow { .. }
            | Change::UpdateRow { .. }
            | Change::DeleteRow { .. }
            | Change::SetDataMode { .. }
            | Change::SetEnsureKeys { .. }
            | Change::AddEnumValue { .. }
            | Change::AlterDomain { .. }
            | Change::RenameEnumValue { .. }
            | Change::EnumRebuild { .. }
             | pbps_model::catalog_change!() | Change::Grant { .. } | Change::Revoke { .. } | Change::DetachPartition { .. } | Change::AttachPartition { .. } | Change::SetPartitionDefault { .. } | Change::SetPartitionNotNull { .. } | Change::PublicExecution { .. } => None,
        })
        .collect()
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
    /// The module whose explicit drop removes this referrer, when the engine
    /// proved one — so a plan that drops it is not told it keeps working
    /// (#823). Typed rather than read back out of `name`: the display is the
    /// engine's wording, and a name match would excuse a same-named survivor.
    /// `None` means nothing proves removal, and the referrer stays reported.
    pub removed_with: Option<(ModuleKind, ModuleId)>,
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
    use pbps_model::{IdsFile, PlannedChange};

    fn table(s: &str) -> TableName {
        s.parse().unwrap()
    }

    fn rename_table(uid: &str, from: &str, to: &str) -> PlannedChange {
        PlannedChange::new(Change::RenameTable {
            uid: uid.parse().unwrap(),
            from: table(from),
            to: table(to),
            defaults: vec![],
        })
    }

    fn rename_column(uid: &str, owner: &str, from: &str, to: &str) -> PlannedChange {
        PlannedChange::new(Change::RenameColumn {
            uid: uid.parse().unwrap(),
            table: table(owner),
            from: from.into(),
            to: to.into(),
            table_was: None,
        })
    }

    #[test]
    fn label_changes_do_not_acquire_table_rename_targets_or_hide_actual_table_renames() {
        let add = PlannedChange::new(Change::AddEnumValue {
            uid: "y_000001".parse().unwrap(),
            name: "app.e".parse().unwrap(),
            value: "blue".into(),
            placement: None,
            ordinal: 0,
            definition: Box::new(pbps_model::ManagedType::Enum {
                values: vec!["blue".into()],
            }),
        });
        let steps = vec![add, rename_table("t_000001", "app.old", "app.new")];
        let ids = IdsFile {
            types: [("y_000001".parse().unwrap(), "app.e".parse().unwrap())].into(),
            tables: [("t_000001".parse().unwrap(), table("app.new"))].into(),
            ..Default::default()
        };
        let names = pbps_model::plan_names::PlanNames::of(&steps, &ids).unwrap();
        let targets = rename_targets(&names, false);
        assert_eq!(targets, vec![RenameTarget::Table(table("app.old"))]);
        let names = pbps_model::plan_names::PlanNames::of(&steps[..1], &ids).unwrap();
        assert!(rename_targets(&names, true).is_empty());
    }

    #[test]
    fn a_swap_reports_each_starting_identity_once_in_first_occurrence_order() {
        let changes = [
            rename_table("t_bbbbbb", "app.b", "app.tmp"),
            rename_table("t_aaaaaa", "app.a", "app.b"),
            rename_table("t_bbbbbb", "app.tmp", "app.a"),
        ];
        let after = IdsFile {
            tables: [
                ("t_aaaaaa".parse().unwrap(), table("app.b")),
                ("t_bbbbbb".parse().unwrap(), table("app.a")),
            ]
            .into(),
            ..Default::default()
        };
        let names = pbps_model::plan_names::PlanNames::of(&changes, &after).unwrap();
        assert_eq!(
            rename_targets(&names, false),
            [
                RenameTarget::Table(table("app.b")),
                RenameTarget::Table(table("app.a")),
            ]
        );
    }

    #[test]
    fn column_chains_keep_their_owners_and_the_table_target_distinct() {
        let changes = [
            rename_column("c_bbbbbb", "app.b", "email", "tmp"),
            rename_table("t_aaaaaa", "app.a", "app.moved"),
            rename_column("c_aaaaaa", "app.moved", "email", "tmp"),
            rename_column("c_bbbbbb", "app.b", "tmp", "contact"),
            rename_column("c_aaaaaa", "app.moved", "tmp", "contact"),
        ];
        let after = IdsFile {
            tables: [
                ("t_aaaaaa".parse().unwrap(), table("app.moved")),
                ("t_bbbbbb".parse().unwrap(), table("app.b")),
            ]
            .into(),
            columns: [
                (
                    "c_aaaaaa".parse().unwrap(),
                    table("app.moved").column("contact"),
                ),
                (
                    "c_bbbbbb".parse().unwrap(),
                    table("app.b").column("contact"),
                ),
            ]
            .into(),
            ..Default::default()
        };
        let names = pbps_model::plan_names::PlanNames::of(&changes, &after).unwrap();
        assert_eq!(
            rename_targets(&names, false),
            [
                RenameTarget::Column(table("app.b").column("email")),
                RenameTarget::Table(table("app.a")),
                RenameTarget::Column(table("app.a").column("email")),
            ]
        );
    }

    #[test]
    fn module_drop_selection_remains_the_callers_policy_beside_rename_chains() {
        let changes = [
            rename_table("t_aaaaaa", "app.a", "app.tmp"),
            PlannedChange::new(Change::DropModule {
                id: "app.v".parse().unwrap(),
                kind: ModuleKind::View,
            }),
            rename_table("t_aaaaaa", "app.tmp", "app.b"),
        ];
        let after = IdsFile {
            tables: [("t_aaaaaa".parse().unwrap(), table("app.b"))].into(),
            ..Default::default()
        };
        let names = pbps_model::plan_names::PlanNames::of(&changes, &after).unwrap();
        assert_eq!(
            rename_targets(&names, false),
            [RenameTarget::Table(table("app.a"))]
        );
        assert_eq!(
            rename_targets(&names, true),
            [
                RenameTarget::Table(table("app.a")),
                RenameTarget::Module(table("app.v")),
            ]
        );
    }

    #[test]
    fn a_temporary_table_name_is_never_queried_as_a_stored_impact_target() {
        use pbps_model::{IdsFile, PlannedChange, Uid};
        let uid: Uid = "t_aaaaaa".parse().unwrap();
        let changes = [
            PlannedChange::new(Change::RenameTable {
                uid: uid.clone(),
                from: table("app.a"),
                to: table("app.tmp"),
                defaults: vec![],
            }),
            PlannedChange::new(Change::RenameTable {
                uid: uid.clone(),
                from: table("app.tmp"),
                to: table("app.b"),
                defaults: vec![],
            }),
        ];
        let after = IdsFile {
            tables: [(uid, table("app.b"))].into(),
            ..Default::default()
        };
        let names = pbps_model::plan_names::PlanNames::of(&changes, &after).unwrap();
        assert_eq!(
            rename_targets(&names, false),
            vec![RenameTarget::Table(table("app.a"))]
        );
    }

    #[test]
    fn a_column_created_by_this_plan_has_no_stored_rename_impact() {
        use pbps_model::{Column, IdsFile, PlannedChange, Table, Uid};
        let owner: Uid = "t_aaaaaa".parse().unwrap();
        let column: Uid = "c_aaaaaa".parse().unwrap();
        let mut shape = Table::default();
        shape
            .columns
            .insert("before".into(), Column::new("int".parse().unwrap()));
        let changes = [
            PlannedChange::new(Change::CreateTable {
                uid: owner.clone(),
                name: table("app.new"),
                table: Box::new(shape),
                beside_default: None,
                moves_at_most: None,
            }),
            PlannedChange::new(Change::RenameColumn {
                uid: column.clone(),
                table: table("app.new"),
                from: "before".into(),
                to: "after".into(),
                table_was: None,
            }),
        ];
        let after = IdsFile {
            tables: [(owner, table("app.new"))].into(),
            columns: [(column, table("app.new").column("after"))].into(),
            ..Default::default()
        };
        let names = pbps_model::plan_names::PlanNames::of(&changes, &after).unwrap();
        assert!(rename_targets(&names, false).is_empty());
    }

    #[test]
    fn a_new_tables_rename_chain_has_no_catalog_target() {
        let changes = [
            PlannedChange::new(Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: table("app.new"),
                table: Box::default(),
                beside_default: None,
                moves_at_most: None,
            }),
            rename_table("t_aaaaaa", "app.new", "app.tmp"),
            rename_table("t_aaaaaa", "app.tmp", "app.final"),
        ];
        let after = IdsFile {
            tables: [("t_aaaaaa".parse().unwrap(), table("app.final"))].into(),
            ..Default::default()
        };
        let names = pbps_model::plan_names::PlanNames::of(&changes, &after).unwrap();
        assert!(rename_targets(&names, true).is_empty());
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
            removed_with: None,
        });
        assert!(!report.is_empty());
    }
}
