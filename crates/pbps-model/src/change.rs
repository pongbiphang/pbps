//! Change set: the output of diff, the input of the planner.
//!
//! # Why structured data instead of SQL strings
//!
//! Risk classification, gate decisions and impact analysis all happen at this
//! layer; SQL appears exactly once, in the dialect emitter (SPEC §11.1). If diff
//! emitted strings directly, every one of those decisions would have to guess —
//! with regexes — what it had just written. That is the precise opposite of the
//! reason this tool exists.
//!
//! # Why risk is a field and not a method
//!
//! "Is `int → bigint` a widening or a narrowing?" needs dialect knowledge, which
//! the model layer does not have. So risks are computed by the differ, which does
//! hold a `Dialect`, and attached to [`PlannedChange`] as data.

use std::collections::BTreeSet;
use std::fmt;

use crate::name::{ColumnRef, TableName};
use crate::schema::{
    CheckConstraint, Column, ForeignKey, Index, PrimaryKey, Table, UniqueConstraint,
};
use crate::strategy::Strategy;
use crate::types::ColumnType;
use crate::uid::Uid;

/// A risk class that must be explicitly allowed at the command level (SPEC §7.2).
///
/// The criterion is **whether this kind of change can fail at all**, not whether
/// today's data happens to be safe — inspecting data is a runtime concern and has
/// no place in the declarative layer.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum RiskClass {
    /// Rename. Views, stored procedures and applications that depend on the
    /// object will break.
    Rename,
    /// Data loss: DROP COLUMN / TABLE / INDEX.
    Destructive,
    /// Type narrowing or an incompatible conversion: may truncate or fail.
    Narrowing,
    /// nullable → NOT NULL with no DEFAULT: existing NULLs will violate it.
    NotNull,
    /// Adding UNIQUE / FK / CHECK: existing rows may not satisfy it.
    Constraint,
}

impl RiskClass {
    /// The name accepted by `--allow`.
    pub const fn as_str(self) -> &'static str {
        match self {
            RiskClass::Rename => "rename",
            RiskClass::Destructive => "destructive",
            RiskClass::Narrowing => "narrowing",
            RiskClass::NotNull => "not-null",
            RiskClass::Constraint => "constraint",
        }
    }

    pub const ALL: [RiskClass; 5] = [
        RiskClass::Rename,
        RiskClass::Destructive,
        RiskClass::Narrowing,
        RiskClass::NotNull,
        RiskClass::Constraint,
    ];
}

impl fmt::Display for RiskClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RiskClass {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        RiskClass::ALL
            .into_iter()
            .find(|r| r.as_str() == s)
            .ok_or_else(|| {
                let all: Vec<_> = RiskClass::ALL.iter().map(|r| r.as_str()).collect();
                format!("unknown risk class `{s}`; available: {}", all.join(", "))
            })
    }
}

/// A single atomic change.
///
/// Every variant carries the UID of the object it affects, so that applying a
/// plan never has to match on names — names are exactly what may be changing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Change {
    CreateTable {
        uid: Uid,
        name: TableName,
        table: Box<Table>,
    },
    DropTable {
        uid: Uid,
        name: TableName,
    },
    RenameTable {
        uid: Uid,
        from: TableName,
        to: TableName,
    },

    AddColumn {
        uid: Uid,
        table: TableName,
        name: String,
        column: Box<Column>,
    },
    DropColumn {
        uid: Uid,
        column: ColumnRef,
    },
    RenameColumn {
        uid: Uid,
        table: TableName,
        from: String,
        to: String,
    },
    AlterColumnType {
        uid: Uid,
        column: ColumnRef,
        from: ColumnType,
        to: ColumnType,
        /// Nullability before and after, carried even when it is not what
        /// changed.
        ///
        /// `ALTER COLUMN` restates the entire column definition, and SQL Server
        /// reads an omitted `NULL` / `NOT NULL` as `NULL` — so a type change
        /// emitted without the nullability silently drops a `NOT NULL`. The
        /// differ therefore never emits a separate
        /// [`Change::AlterColumnNullability`] beside a type change on the same
        /// column, and carrying both ends keeps the `not-null` risk derivable
        /// from the change alone.
        from_nullable: bool,
        to_nullable: bool,
    },
    AlterColumnNullability {
        uid: Uid,
        column: ColumnRef,
        /// The column's type, unchanged, restated for the same reason
        /// [`Change::AlterColumnType`] carries the nullability: `ALTER COLUMN`
        /// takes a whole column definition, and there is no way to say "keep the
        /// type, change only this".
        ty: ColumnType,
        /// Whether the column is nullable after the change.
        to_nullable: bool,
    },
    AlterColumnDefault {
        uid: Uid,
        column: ColumnRef,
        from: Option<String>,
        to: Option<String>,
    },
    /// Deprecation flag changed. Produces no structural change; may optionally be
    /// written to an extended property.
    SetColumnDeprecated {
        uid: Uid,
        column: ColumnRef,
        reason: Option<String>,
    },

    // Constraints and indexes are always drop + add, never modified in place —
    // that is how the database itself implements it, and pretending otherwise
    // would only give the emitter one more path that can fail.
    SetPrimaryKey {
        table: TableName,
        from: Option<PrimaryKey>,
        to: Option<PrimaryKey>,
    },
    AddUnique {
        table: TableName,
        name: String,
        constraint: UniqueConstraint,
    },
    DropUnique {
        table: TableName,
        name: String,
    },
    AddForeignKey {
        table: TableName,
        name: String,
        constraint: Box<ForeignKey>,
    },
    DropForeignKey {
        table: TableName,
        name: String,
    },
    AddCheck {
        table: TableName,
        name: String,
        constraint: CheckConstraint,
    },
    DropCheck {
        table: TableName,
        name: String,
    },
    AddIndex {
        table: TableName,
        name: String,
        index: Box<Index>,
    },
    DropIndex {
        table: TableName,
        name: String,
    },
}

impl Change {
    /// The table this change acts on. Used for grouping in output and for ordering.
    pub fn table(&self) -> &TableName {
        match self {
            Change::CreateTable { name, .. } | Change::DropTable { name, .. } => name,
            Change::RenameTable { from, .. } => from,
            Change::AddColumn { table, .. }
            | Change::RenameColumn { table, .. }
            | Change::SetPrimaryKey { table, .. }
            | Change::AddUnique { table, .. }
            | Change::DropUnique { table, .. }
            | Change::AddForeignKey { table, .. }
            | Change::DropForeignKey { table, .. }
            | Change::AddCheck { table, .. }
            | Change::DropCheck { table, .. }
            | Change::AddIndex { table, .. }
            | Change::DropIndex { table, .. } => table,
            Change::DropColumn { column, .. }
            | Change::AlterColumnType { column, .. }
            | Change::AlterColumnNullability { column, .. }
            | Change::AlterColumnDefault { column, .. }
            | Change::SetColumnDeprecated { column, .. } => &column.table,
        }
    }

    /// Risks that follow from the kind of change alone, with no dialect knowledge.
    ///
    /// Risks that require comparing types (narrowing) are not here; the differ
    /// adds those.
    pub fn intrinsic_risks(&self) -> BTreeSet<RiskClass> {
        let mut r = BTreeSet::new();
        match self {
            Change::DropTable { .. } | Change::DropColumn { .. } | Change::DropIndex { .. } => {
                r.insert(RiskClass::Destructive);
            }
            Change::RenameTable { .. } | Change::RenameColumn { .. } => {
                r.insert(RiskClass::Rename);
            }
            Change::AlterColumnNullability {
                to_nullable: false, ..
            }
            | Change::AlterColumnType {
                from_nullable: true,
                to_nullable: false,
                ..
            } => {
                r.insert(RiskClass::NotNull);
            }
            Change::AddUnique { .. } | Change::AddForeignKey { .. } | Change::AddCheck { .. } => {
                r.insert(RiskClass::Constraint);
            }
            Change::SetPrimaryKey { to: Some(_), .. } => {
                r.insert(RiskClass::Constraint);
            }
            Change::SetPrimaryKey { to: None, .. } => {
                r.insert(RiskClass::Destructive);
            }
            Change::CreateTable { .. }
            | Change::AddColumn { .. }
            | Change::AlterColumnType { .. }
            | Change::AlterColumnNullability {
                to_nullable: true, ..
            }
            | Change::AlterColumnDefault { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::DropUnique { .. }
            | Change::DropForeignKey { .. }
            | Change::DropCheck { .. }
            | Change::AddIndex { .. } => {}
        }
        r
    }
}

/// A change together with the risks that have been determined for it, and the
/// execution strategy it is to be carried out with.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlannedChange {
    #[serde(flatten)]
    pub change: Change,

    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub risks: BTreeSet<RiskClass>,

    /// How to get there, never where to go (ADR-0003).
    ///
    /// It travels *with the change* rather than being looked up at emit time
    /// because the plan file is the reviewed artifact: an approver reading
    /// plan.json has to be able to see that this index will be rebuilt online,
    /// and a hint resolved later against a YAML file the deployment host may
    /// not even have is a hint nobody reviewed.
    #[serde(default, skip_serializing_if = "Strategy::is_default")]
    pub strategy: Strategy,
}

impl PlannedChange {
    /// Builds from the risks the change itself implies. Risks that need dialect
    /// knowledge are added separately by the differ.
    pub fn new(change: Change) -> Self {
        let risks = change.intrinsic_risks();
        Self {
            change,
            risks,
            strategy: Strategy::default(),
        }
    }

    pub fn with_risk(mut self, r: RiskClass) -> Self {
        self.risks.insert(r);
        self
    }

    pub fn with_strategy(mut self, s: Strategy) -> Self {
        self.strategy = s;
        self
    }
}

/// The complete result of one diff.
///
/// The order of `changes` is the order of application. Producing that order
/// (drop indexes before dropping columns, and so on) is the planner's job; the
/// model only guarantees the order is preserved.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChangeSet {
    pub changes: Vec<PlannedChange>,
}

impl ChangeSet {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Every risk class this plan involves — the set `--allow` must cover.
    pub fn risks(&self) -> BTreeSet<RiskClass> {
        self.changes
            .iter()
            .flat_map(|c| c.risks.iter().copied())
            .collect()
    }

    /// Risks not covered by `allowed`. Non-empty means apply must abort.
    pub fn unapproved_risks(&self, allowed: &BTreeSet<RiskClass>) -> BTreeSet<RiskClass> {
        self.risks().difference(allowed).copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Column;

    fn col(s: &str) -> ColumnRef {
        s.parse().unwrap()
    }
    fn uid(s: &str) -> Uid {
        s.parse().unwrap()
    }
    fn ty(s: &str) -> ColumnType {
        s.parse().unwrap()
    }

    fn drop_column() -> Change {
        Change::DropColumn {
            uid: uid("c_k7x2mq"),
            column: col("dbo.customer.legacy_code"),
        }
    }

    fn add_column() -> Change {
        Change::AddColumn {
            uid: uid("c_p3n8vd"),
            table: "dbo.customer".parse().unwrap(),
            name: "mobile".into(),
            column: Box::new(Column::new(ty("nvarchar(20)"))),
        }
    }

    #[test]
    fn destructive_changes_are_flagged() {
        assert!(
            drop_column()
                .intrinsic_risks()
                .contains(&RiskClass::Destructive)
        );
    }

    #[test]
    fn additive_changes_carry_no_risk() {
        assert!(add_column().intrinsic_risks().is_empty());
    }

    /// Adding NOT NULL is risky, relaxing to nullable is not — the direction has
    /// to be distinguished.
    #[test]
    fn nullability_risk_is_directional() {
        let make = |to_nullable| Change::AlterColumnNullability {
            uid: uid("c_k7x2mq"),
            column: col("dbo.customer.email"),
            ty: ty("nvarchar(255)"),
            to_nullable,
        };
        let (tighten, loosen) = (make(false), make(true));
        assert!(tighten.intrinsic_risks().contains(&RiskClass::NotNull));
        assert!(loosen.intrinsic_risks().is_empty());
    }

    /// Type narrowing needs a dialect to judge; the model layer must not decide
    /// on its own.
    #[test]
    fn type_change_risk_is_left_to_the_dialect() {
        let c = Change::AlterColumnType {
            uid: uid("c_k7x2mq"),
            column: col("dbo.customer.balance"),
            from: ty("bigint"),
            to: ty("int"),
            from_nullable: true,
            to_nullable: true,
        };
        assert!(
            c.intrinsic_risks().is_empty(),
            "the model layer must not guess type risks"
        );

        let planned = PlannedChange::new(c).with_risk(RiskClass::Narrowing);
        assert!(planned.risks.contains(&RiskClass::Narrowing));
    }

    /// A type change that also tightens nullability carries the not-null risk:
    /// the differ folds the two into one change, and folding must not lose the
    /// risk that the separate change would have carried.
    #[test]
    fn a_type_change_that_tightens_nullability_is_a_not_null_risk() {
        let make = |from_nullable, to_nullable| Change::AlterColumnType {
            uid: uid("c_k7x2mq"),
            column: col("dbo.customer.email"),
            from: ty("nvarchar(50)"),
            to: ty("nvarchar(100)"),
            from_nullable,
            to_nullable,
        };
        assert!(
            make(true, false)
                .intrinsic_risks()
                .contains(&RiskClass::NotNull)
        );
        // Already NOT NULL, and staying that way: restating it risks nothing.
        assert!(make(false, false).intrinsic_risks().is_empty());
        assert!(make(true, true).intrinsic_risks().is_empty());
        assert!(make(false, true).intrinsic_risks().is_empty());
    }

    #[test]
    fn unapproved_risks_are_reported() {
        let cs = ChangeSet {
            changes: vec![
                PlannedChange::new(drop_column()),
                PlannedChange::new(add_column()),
            ],
        };
        assert_eq!(cs.risks(), BTreeSet::from([RiskClass::Destructive]));

        let none = BTreeSet::new();
        assert_eq!(
            cs.unapproved_risks(&none),
            BTreeSet::from([RiskClass::Destructive])
        );

        let allowed = BTreeSet::from([RiskClass::Destructive]);
        assert!(cs.unapproved_risks(&allowed).is_empty());
    }

    /// A plan with no risks needs no flags at all.
    #[test]
    fn safe_plans_need_no_flags() {
        let cs = ChangeSet {
            changes: vec![PlannedChange::new(add_column())],
        };
        assert!(cs.unapproved_risks(&BTreeSet::new()).is_empty());
    }

    #[test]
    fn risk_names_round_trip() {
        for r in RiskClass::ALL {
            assert_eq!(r.as_str().parse::<RiskClass>().unwrap(), r);
        }
        assert!("nonsense".parse::<RiskClass>().is_err());
    }

    #[test]
    fn changes_round_trip_through_json() {
        let cs = ChangeSet {
            changes: vec![
                PlannedChange::new(drop_column()),
                PlannedChange::new(add_column()),
            ],
        };
        let back: ChangeSet = serde_json::from_str(&serde_json::to_string(&cs).unwrap()).unwrap();
        assert_eq!(cs, back);
    }

    #[test]
    fn every_change_reports_its_table() {
        assert_eq!(drop_column().table().to_string(), "dbo.customer");
        assert_eq!(add_column().table().to_string(), "dbo.customer");
    }
}
