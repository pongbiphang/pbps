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

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::data::{Cell, DataMode, Row, RowKey, Value};
use crate::module::{Module, ModuleKind, ObjectName};
use crate::name::{ColumnRef, TableName};
use crate::role::{GrantTarget, Permission};
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
    /// A declared reference row's values are overwritten (ADR-0004). What is
    /// there now is not recoverable from the declarations, because what is
    /// there now is what is being replaced.
    DataUpdate,
    /// A row leaves the table: an `exact` table's undeclared row, or a changed
    /// primary-key value, which is delete plus insert.
    DataDelete,
    /// Access is taken away (ADR-0005): a `REVOKE`, or a role drop, whose whole
    /// effect is one. An availability risk — a running application loses
    /// access mid-flight.
    Revoke,
    /// Access widens (ADR-0005). A security risk, labelled in every plan so
    /// both review layers see it — but **not gated**: granting is the normal
    /// case, the merge request reviews the YAML diff and the gate approves the
    /// pinned plan, and a flag for it would add friction without safety.
    GrantWiden,
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
            RiskClass::DataUpdate => "data-update",
            RiskClass::DataDelete => "data-delete",
            RiskClass::Revoke => "revoke",
            RiskClass::GrantWiden => "grant-widen",
        }
    }

    /// Whether `apply` refuses the plan until `--allow` names this class.
    ///
    /// Every class is *labelled*; only the gated ones stop an apply. The one
    /// exception exists because the alternative is worse: a `--allow
    /// grant-widen` typed on every deployment that adds a permission would be
    /// typed out of habit, and a gate that is always opened protects nothing.
    pub const fn is_gated(self) -> bool {
        !matches!(self, RiskClass::GrantWiden)
    }

    /// What can go wrong, in the operator's words.
    ///
    /// The doc comments above say the same thing to whoever reads this file;
    /// this says it to the reviewer at the deployment gate, who is deciding
    /// whether to type `--allow destructive` at four in the afternoon and has
    /// not read this file. A risk class the reviewer cannot explain is one they
    /// will approve out of habit.
    pub const fn why(self) -> &'static str {
        match self {
            RiskClass::Rename => {
                "the old name disappears: views, procedures and any application still using it break"
            }
            RiskClass::Destructive => "data is lost, and no plan brings it back",
            RiskClass::Narrowing => {
                "the new type may not hold what is already stored: values can be truncated or the statement rejected"
            }
            RiskClass::NotNull => {
                "existing NULLs, or rows with no value for a newly required column, make the statement fail"
            }
            RiskClass::Constraint => {
                "existing rows may not satisfy the new constraint and the statement fails"
            }
            RiskClass::DataUpdate => {
                "a reference row's current values are overwritten, and the plan does not record what they were"
            }
            RiskClass::DataDelete => {
                "a reference row is removed: rows in other tables that point at it fail, or lose what they pointed at"
            }
            RiskClass::Revoke => {
                "access is taken away: an application still relying on it fails from the moment the plan commits"
            }
            RiskClass::GrantWiden => {
                "access widens: a role can do more than before, which the merge request should have reviewed"
            }
        }
    }

    pub const ALL: [RiskClass; 9] = [
        RiskClass::Rename,
        RiskClass::Destructive,
        RiskClass::Narrowing,
        RiskClass::NotNull,
        RiskClass::Constraint,
        RiskClass::DataUpdate,
        RiskClass::DataDelete,
        RiskClass::Revoke,
        RiskClass::GrantWiden,
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
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
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

    // Reference data (ADR-0004). No uid, and no rename intent: a row's entire
    // content is declared, so recreating one is lossless — the generalized
    // identity criterion of ADR-0005. The identity is the primary-key value,
    // and a change to *that* is a delete plus an insert, gated as `data-delete`.
    //
    // The table's name rather than its uid, unlike the column changes above,
    // because these run in the same plan as the renames and after them: the
    // emitter needs the name the catalog will have when the statement runs, and
    // for a row that is always the post-rename one.
    InsertRow {
        table: TableName,
        /// The single primary-key column the key belongs in.
        ///
        /// Carried rather than looked up: the emitter has the change and
        /// nothing else, and a saved plan is applied on a host that may have no
        /// checkout at all — the same reason the plan carries its own ids.
        key_column: String,
        /// Whether the key column is an `IDENTITY` column, which the engine
        /// assigns unless told otherwise. Carried for the same reason the
        /// column name is: the emitter sees the change and nothing else, and
        /// an explicit value into an identity column is refused unless the
        /// statement says `SET IDENTITY_INSERT ... ON` first (ADR-0004).
        identity_key: bool,
        key: RowKey,
        row: Row,
        /// The default of every column the row omits and the table gives
        /// one, by column. An omitted column is inserted at its default
        /// (ADR-0004), and the pre-delete probe has to know what that is: a
        /// default that names a parent row this plan deletes is a row
        /// arriving on it, and the probe cannot see the arrival without this
        /// (DECISIONS 117). Carried for the reason `key_column` is: apply
        /// has the plan and nothing else. Absent from older plans, which is
        /// an empty map.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        defaults: BTreeMap<String, String>,
        /// The type of every non-key column the table has, an `IDENTITY`
        /// column aside: the ones the row spells, the columns in `defaults`,
        /// and the ones the table gives no default, which the insert leaves
        /// at NULL. Carried for the same reason `UpdateRow` carries one: the
        /// emitter holds the row to what it wrote, by the rendering that
        /// reads each cell back and under a binary collation, so a rewrite
        /// the column's own collation would call equal is still a rewrite
        /// (DECISIONS 137); a column left to a *constant* default is checked
        /// against that default the same way — which needs the type to know
        /// the comparison is one the engine allows at all (133) — and a
        /// column left to nothing is held to NULL (136). Absent from older
        /// plans, which is an empty map: a spelled cell is then compared as
        /// the engine compares, and the rest holds nothing.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        types: BTreeMap<String, ColumnType>,
    },
    /// The columns that differ, never the whole row: an `UPDATE` restating a
    /// column that did not change would overwrite a value the declaration and
    /// the database already agree on, and would make plan.sql claim a change
    /// that is not one.
    UpdateRow {
        table: TableName,
        key_column: String,
        key: RowKey,
        /// Column to (before, after). The before is carried so the plan can say
        /// what is being replaced — the reviewer at the gate has no connection
        /// (SPEC §14.1) and cannot look it up.
        ///
        /// [`Cell`], not [`Value`]: an omitted column means the declared
        /// default, and the emitter has to write `DEFAULT`, not `NULL`.
        columns: BTreeMap<String, (Cell, Cell)>,
        /// The declared cells the row already holds — every non-key column
        /// outside `columns`, resolved by the omission rule. Never restated
        /// in the `UPDATE`'s `SET`, for the reason `columns` gives; but the
        /// statement holds the row to them, before and after it runs: a
        /// cell the plan did not touch is still one the declaration claims,
        /// and a trigger rewriting it, or a hand edit since the plan was
        /// made, would otherwise be read back and recorded as the plan's
        /// own result (DECISIONS 136). Absent from older plans, which is an
        /// empty map and holds the row to its changed cells alone.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        unchanged: BTreeMap<String, Cell>,
        /// The type each column in `columns` or `unchanged` has in the state
        /// the plan was made against, for the columns that state has. The
        /// emitter compares each cell by the rendering that read it — which
        /// is the column's type's — and refuses the update when the row is
        /// no longer as recorded (DECISIONS 122). A column the base does not
        /// have is absent: its `before` is what this plan's `AddColumn`
        /// leaves there, not a recorded cell. Absent from older plans,
        /// which is an empty map.
        ///
        /// A column this plan *retypes* is here too, paired with its entry in
        /// `after_types`: the recorded text is the old type's spelling of a
        /// value the `AlterColumnType` has since converted, so the two
        /// together say "recorded in this type, held in that one" and the
        /// emitter asks the engine for the same conversion. Leaving it out
        /// dropped the precondition for exactly the cell most likely to be
        /// contended (DECISIONS 149).
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        types: BTreeMap<String, ColumnType>,
        /// The type each column has *once this plan has run*, where that is
        /// not the type in `types` — a column this plan adds, and a column
        /// whose type it changes. Both sort before the row changes
        /// (`order_key`), so the `UPDATE` meets the declared type, not the
        /// base one.
        ///
        /// Separate from `types` because the two checks around the write ask
        /// different questions of the same cell. The precondition asks what
        /// the recorded state holds, and a column the base lacks has no
        /// recorded cell to hold the row to. The postcondition asks what the
        /// row holds afterwards, and *every* declared cell is one the write
        /// is answerable for — including the one this plan's `AddColumn` just
        /// made room for. Carrying one map made the added column unheld: an
        /// `AFTER UPDATE` trigger could rewrite it, the apply would record the
        /// rewritten value, and the next plan would propose the update again
        /// (DECISIONS 140).
        ///
        /// Only the entries that differ, so the plan does not carry the
        /// whole column list twice per row. The emitter falls back to
        /// `types`. Absent from older plans, which is an empty map.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        after_types: BTreeMap<String, ColumnType>,
    },
    DeleteRow {
        table: TableName,
        key_column: String,
        key: RowKey,
        /// Why this row is going: `exact` means the table declared itself
        /// complete and this row is not in it. Recorded because the two reasons
        /// read very differently at the gate.
        cause: DeleteCause,
        /// The row as the baseline recorded it, so the `DELETE` removes the
        /// row that was reviewed and not whatever stands under that key when
        /// it runs.
        ///
        /// The checksum pins the state only up to the moment `apply` reads
        /// it: an application session that rewrites this row in between
        /// leaves a key-only `DELETE` deleting the new version and
        /// `@@ROWCOUNT = 1` calling it the reviewed one — an unreviewed loss
        /// the apply then records as its own result. An update holds every
        /// declared cell for exactly this reason (136); a delete has more to
        /// lose, since what it removes cannot be compared afterwards
        /// (DECISIONS 143). Absent from older plans, which is an empty map.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        row: BTreeMap<String, Cell>,
        /// The type each recorded cell was read by, so the predicate compares
        /// it the way the read-back rendered it (122). A cell whose type has
        /// no comparison — `xml`, `text`, the spatial types — is carried but
        /// not held, exactly as in an update.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        types: BTreeMap<String, ColumnType>,
        /// The type each of those columns has *when the `DELETE` runs*, where
        /// that is not the type in `types`: a column this plan retypes, whose
        /// `AlterColumnType` sorts before every row change. The recorded text
        /// is the old type's spelling and the column now holds the converted
        /// value, so the predicate converts the recorded text the same way
        /// rather than comparing two spellings of one value — or, as before
        /// this pair existed, holding the row to nothing at all
        /// (DECISIONS 149). Only the entries that differ; the emitter falls
        /// back to `types`. Absent from older plans, which is an empty map.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        after_types: BTreeMap<String, ColumnType>,
    },
    /// `exact` <-> `ensure`. It emits no SQL by itself — the row changes it
    /// implies are separate entries — but it is a change to the declaration
    /// that `verify` compares against, so a plan that omitted it would leave
    /// the recorded state disagreeing with the file.
    SetDataMode {
        table: TableName,
        from: Option<DataMode>,
        to: Option<DataMode>,
    },

    // Modules (ADR-0002) carry no uid: they carry no data either, so a rename
    // is drop + add and the audit trail is git.
    CreateModule {
        name: ObjectName,
        module: Box<Module>,
    },
    /// Re-stated in full. `CREATE OR ALTER` is idempotent and — unlike drop plus
    /// create — preserves the permissions granted on the object, which is the
    /// DACPAC pain point this avoids.
    AlterModule {
        name: ObjectName,
        module: Box<Module>,
    },
    DropModule {
        name: ObjectName,
        kind: ModuleKind,
    },

    // Roles (ADR-0005). Identity-tracked like tables: a rename is `ALTER ROLE
    // ... WITH NAME`, never drop + add, because membership lives only in the
    // environment. Grants are split out of the create, as foreign keys are
    // out of `CreateTable`: they sort after the objects they name exist.
    CreateRole {
        uid: Uid,
        name: String,
    },
    /// Dropping a role that still has members is refused by the engine, and
    /// membership is each environment's own — so a connected plan lists the
    /// members it found and removes them first, by name, where the reviewer
    /// can see who loses what. An offline plan has no environment to ask and
    /// leaves the list empty (ADR-0005).
    DropRole {
        uid: Uid,
        name: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        members: Vec<String>,
    },
    RenameRole {
        uid: Uid,
        from: String,
        to: String,
    },
    /// Permissions added on one target. Only the ones that are new: restating
    /// what the role already holds would make the plan claim a widening that
    /// is not one.
    Grant {
        role: String,
        target: GrantTarget,
        permissions: BTreeSet<Permission>,
    },
    /// Permissions removed from one target — the ones the declaration no
    /// longer lists.
    Revoke {
        role: String,
        target: GrantTarget,
        permissions: BTreeSet<Permission>,
    },
}

/// What a row change leaves at its key.
///
/// The cells are the ones the plan *spells* — never the ones it leaves to a
/// default. A read-back omits a cell that is at its column's default
/// ([`crate::data::cell`]), so a plan value that happens to equal that default
/// is simply not there to compare, and demanding it would refuse a valid
/// apply. What is spelled and present must match, and that is exact: a
/// connected plan is refused outright if the engine reads a declared value
/// back differently (DECISIONS 101, 165).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowAfter<'a> {
    /// The row is there, and holds at least these cells.
    Holding(BTreeMap<&'a str, &'a Value>),
    /// The row is gone.
    Gone,
}

/// Whether a name is there once this plan has run.
///
/// For the caller that has to check a plan did what it said: a `CREATE` that
/// reports success and a table that is not there afterwards are two different
/// facts, and nothing but this comparison puts them together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Present,
    Absent,
}

/// What a module change leaves standing at its name.
///
/// An enum rather than `Option<Option<_>>`: "dropped" and "no module change
/// here" are different answers, and nesting them is how they get confused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleAfter<'a> {
    /// The definition the plan creates the module with, or replaces it by.
    Standing(&'a Module),
    /// Nothing: the plan drops it.
    Gone,
}

/// Which way a [`Change::Grant`] or [`Change::Revoke`] moves a role's
/// permissions on one target.
///
/// An enum rather than a flag beside the set: the caller adds one and
/// subtracts the other, and a bool in that position is a mistake that
/// compiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionChange<'a> {
    /// The role holds these afterwards and did not before.
    Granted(&'a BTreeSet<Permission>),
    /// The role held these before and does not afterwards.
    Revoked(&'a BTreeSet<Permission>),
}

impl Change {
    /// What this change acts on, for grouping in output and for ordering:
    /// `dbo.customer`, or `role app_reader`.
    // The wildcard stands in for "every change with a table", and `table()`
    // is the exhaustive match that decides which those are; a variant added
    // later is handled there, not here.
    #[allow(clippy::wildcard_enum_match_arm)]
    pub fn subject(&self) -> String {
        match self {
            Change::CreateRole { name, .. }
            | Change::DropRole { name, .. }
            | Change::Grant { role: name, .. }
            | Change::Revoke { role: name, .. } => format!("role {name}"),
            Change::RenameRole { from, .. } => format!("role {from}"),
            // Every other change acts on an object with a name.
            other => other.table().map(ToString::to_string).unwrap_or_default(),
        }
    }

    /// The object this change acts on, or `None` for a change that is not to
    /// an object in the tables-and-modules namespace at all.
    ///
    /// For a module change it is the module's own qualified name: tables and
    /// modules share one namespace, so one type covers both and a plan groups
    /// by "the thing being changed" either way. A role change has no such
    /// name — a role is a principal, not an object — and the callers that
    /// want a label use [`Change::subject`].
    pub fn table(&self) -> Option<&TableName> {
        Some(match self {
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
            | Change::DropIndex { table, .. }
            | Change::InsertRow { table, .. }
            | Change::UpdateRow { table, .. }
            | Change::DeleteRow { table, .. }
            | Change::SetDataMode { table, .. } => table,
            Change::DropColumn { column, .. }
            | Change::AlterColumnType { column, .. }
            | Change::AlterColumnNullability { column, .. }
            | Change::AlterColumnDefault { column, .. }
            | Change::SetColumnDeprecated { column, .. } => &column.table,
            Change::CreateModule { name, .. }
            | Change::AlterModule { name, .. }
            | Change::DropModule { name, .. } => name,
            Change::CreateRole { .. }
            | Change::DropRole { .. }
            | Change::RenameRole { .. }
            | Change::Grant { .. }
            | Change::Revoke { .. } => return None,
        })
    }

    /// Every name in the tables-and-modules namespace this change reaches:
    /// the one [`Change::table`] gives, plus the far end of a rename, which
    /// is a second name the same object answers to across one plan.
    ///
    /// For a caller asking "did this plan touch that object", both ends have
    /// to be in the answer: the recorded state knows a renamed table by its
    /// old name and the plan's result knows it by its new one, and an object
    /// present under one name and absent under the other is exactly what a
    /// rename looks like from the outside.
    // Exhaustive rather than a wildcard, for the reason the body gives: a
    // change added later that moves an object's name has to be named here, or
    // a caller asking what this plan touched would be told about one end of it
    // and left to bless whatever happened at the other.
    pub fn objects(&self) -> impl Iterator<Item = &TableName> {
        let far_end = match self {
            Change::RenameTable { to, .. } => Some(to),
            Change::CreateTable { .. }
            | Change::DropTable { .. }
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
            | Change::Revoke { .. } => None,
        };
        self.table().into_iter().chain(far_end)
    }

    /// The declared row this change writes, where it writes one: the table it
    /// is in, under the name the plan gives that table, and its key.
    ///
    /// A caller comparing a state before an apply with the state after it
    /// needs this to tell the rows the plan is answerable for from the ones it
    /// is not. The plan's own statements hold the first kind to what they
    /// wrote (132, 136, 143); nothing else in a run speaks for the second, and
    /// an `AFTER` trigger reaches them from inside the very statement that
    /// writes a row the plan *did* name.
    ///
    /// The direction comes with it because those statements stop speaking at
    /// their own commit. In one transaction that is enough — the row stays
    /// locked until the commit, so nothing can reach it — but a staged run
    /// commits each statement, and between that and the checkpoint read a row
    /// it inserted can be deleted, or one it deleted put back (DECISIONS 162).
    // Exhaustive rather than a wildcard: a change added later that writes a
    // declared row has to be named here, or the row it writes would be
    // compared against a state it was never part of.
    pub fn row(&self) -> Option<(&TableName, &RowKey, RowAfter<'_>)> {
        // Only what the plan writes down. A NULL is skipped for the same
        // reason a defaulted cell is: the read-back omits a NULL in a column
        // with no default, so its absence proves nothing either way.
        fn spelled(cell: &Cell) -> Option<&Value> {
            match cell {
                Cell::Value(Value::Null) | Cell::Default(_) => None,
                Cell::Value(v) => Some(v),
            }
        }
        match self {
            Change::InsertRow {
                table, key, row, ..
            } => Some((
                table,
                key,
                RowAfter::Holding(
                    row.0
                        .iter()
                        .filter(|(_, v)| !matches!(v, Value::Null))
                        .map(|(column, v)| (column.as_str(), v))
                        .collect(),
                ),
            )),
            Change::UpdateRow {
                table,
                key,
                columns,
                unchanged,
                ..
            } => Some((
                table,
                key,
                RowAfter::Holding(
                    columns
                        .iter()
                        .map(|(column, (_, to))| (column, to))
                        .chain(unchanged)
                        .filter_map(|(column, cell)| spelled(cell).map(|v| (column.as_str(), v)))
                        .collect(),
                ),
            )),
            Change::DeleteRow { table, key, .. } => Some((table, key, RowAfter::Gone)),
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
            | Change::SetDataMode { .. }
            | Change::CreateModule { .. }
            | Change::AlterModule { .. }
            | Change::DropModule { .. }
            | Change::CreateRole { .. }
            | Change::DropRole { .. }
            | Change::RenameRole { .. }
            | Change::Grant { .. }
            | Change::Revoke { .. } => None,
        }
    }

    /// Every column this change moves the *reading* of: its name, or the way
    /// its cells come back.
    ///
    /// For a caller comparing a table's rows before and after an apply. A row
    /// is keyed by column name and each cell reads back in its column's own
    /// rendering, so a column the plan renames, adds, drops or retypes changes
    /// the shape of every row in the table without any row change saying so.
    /// Compared whole, those rows all read as somebody else's work
    /// (DECISIONS 158).
    ///
    /// 158 named every column-level change on the argument that naming one too
    /// many only narrows a comparison. It does — and that narrowing has a
    /// price: a column skipped here is a cell nothing compares. The two that
    /// move no reading are excluded now (DECISIONS 162). **Nullability**
    /// rewrites no stored value, and the read-back's omission rule turns on
    /// whether a column *has a default*, not on whether it accepts NULL. A
    /// column's **deprecation** is a description; it touches no cell at all.
    /// A change to the **default** stays, because a cell at its default is
    /// spelled from that default and omitted where it matches.
    // Exhaustive rather than a wildcard: a change added later that touches a
    // column has to be named here, or the rows of its table would be compared
    // against a shape the plan itself moved.
    pub fn columns(&self) -> Vec<ColumnRef> {
        match self {
            Change::AddColumn { table, name, .. } => vec![table.column(name)],
            Change::RenameColumn {
                table, from, to, ..
            } => vec![table.column(from), table.column(to)],
            Change::DropColumn { column, .. }
            | Change::AlterColumnType { column, .. }
            | Change::AlterColumnDefault { column, .. } => vec![column.clone()],
            Change::AlterColumnNullability { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::CreateTable { .. }
            | Change::DropTable { .. }
            | Change::RenameTable { .. }
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
            | Change::Revoke { .. } => Vec::new(),
        }
    }

    /// The object this change removes from the database outright, if it
    /// removes one.
    ///
    /// A securable takes its permissions with it — measured: dropping a table
    /// leaves the role holding none of what it was granted on it — so a plan
    /// that drops a granted object emits no `REVOKE` and the grant is simply
    /// not there afterwards. A caller comparing a role's grants across an
    /// apply has to know that (DECISIONS 158).
    // Exhaustive rather than a wildcard, for the reason above: a change added
    // later that removes an object takes grants with it too.
    pub fn drops(&self) -> Option<&TableName> {
        match self {
            Change::DropTable { name, .. } | Change::DropModule { name, .. } => Some(name),
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
            | Change::CreateRole { .. }
            | Change::DropRole { .. }
            | Change::RenameRole { .. }
            | Change::Grant { .. }
            | Change::Revoke { .. } => None,
        }
    }

    /// The permissions this change writes, where it writes any: the role, the
    /// target they are on, and which way the set moves.
    ///
    /// The direction is carried rather than left to the caller because the
    /// caller has to reconstruct what the role will hold, and "the plan moves
    /// these" is not enough to do that — subtracting them from both sides
    /// instead left the plan's own grant checked by nothing at all
    /// (DECISIONS 160).
    ///
    /// The counterpart of [`Change::row`] on the other half of the model, and
    /// for the same caller. A role a plan touches is not a role the plan is
    /// answerable for *whole*: it grants or revokes some permissions on some
    /// targets, and every other permission that role holds is one nothing in
    /// the run speaks for (DECISIONS 156).
    // Exhaustive rather than a wildcard: a change added later that moves a
    // permission has to be named here, or the permission it moves would be
    // compared against a state it was never part of.
    pub fn grant(&self) -> Option<(&str, &GrantTarget, PermissionChange<'_>)> {
        match self {
            Change::Grant {
                role,
                target,
                permissions,
            } => Some((role, target, PermissionChange::Granted(permissions))),
            Change::Revoke {
                role,
                target,
                permissions,
            } => Some((role, target, PermissionChange::Revoked(permissions))),
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
            | Change::RenameRole { .. } => None,
        }
    }

    /// Every role name this change reaches, both ends of a rename included,
    /// for the reason [`Change::objects`] gives. Empty for every change that
    /// is not about a principal.
    // Exhaustive rather than a wildcard: a change added later that names a
    // role has to be classified here, or a caller asking what this plan
    // touched would be told "nothing" about it.
    pub fn roles(&self) -> impl Iterator<Item = &str> {
        let (one, two) = match self {
            Change::CreateRole { name, .. }
            | Change::DropRole { name, .. }
            | Change::Grant { role: name, .. }
            | Change::Revoke { role: name, .. } => (Some(name.as_str()), None),
            Change::RenameRole { from, to, .. } => (Some(from.as_str()), Some(to.as_str())),
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
            | Change::DropModule { .. } => (None, None),
        };
        one.into_iter().chain(two)
    }

    /// Which table names this change leaves standing, and which it leaves
    /// empty.
    ///
    /// Existence only, deliberately: the *shape* of a table the plan creates
    /// or alters is read back from the catalog precisely because the engine's
    /// stored form is the only one that compares equal on the next drift check
    /// (SPEC §8.2), so holding it to the declared shape would refuse valid
    /// applies. Whether the object is there at all has no such ambiguity
    /// (DECISIONS 161).
    // Exhaustive rather than a wildcard, as every accessor here is.
    pub fn tables_after(&self) -> Vec<(&TableName, Presence)> {
        match self {
            Change::CreateTable { name, .. } => vec![(name, Presence::Present)],
            Change::DropTable { name, .. } => vec![(name, Presence::Absent)],
            Change::RenameTable { from, to, .. } => {
                vec![(from, Presence::Absent), (to, Presence::Present)]
            }
            Change::AddColumn { .. }
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
            | Change::Revoke { .. } => Vec::new(),
        }
    }

    /// The same for roles. A role with no grants is invisible to every other
    /// comparison here — its whole state is its name — so without this a
    /// `CREATE ROLE` that another session undid was recorded as success.
    // Exhaustive rather than a wildcard, as every accessor here is.
    pub fn roles_after(&self) -> Vec<(&str, Presence)> {
        match self {
            Change::CreateRole { name, .. } => vec![(name, Presence::Present)],
            Change::DropRole { name, .. } => vec![(name, Presence::Absent)],
            Change::RenameRole { from, to, .. } => {
                vec![(from, Presence::Absent), (to, Presence::Present)]
            }
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
            | Change::Grant { .. }
            | Change::Revoke { .. } => Vec::new(),
        }
    }

    /// What this plan leaves standing where a module change names one.
    ///
    /// A module statement has no postcondition of its own — `CREATE OR ALTER`
    /// reports success and says nothing about what is now stored — so the only
    /// thing that can hold one to what the plan wrote is a caller comparing
    /// the read-back with the definition. Safe to compare exactly: a module
    /// read back equals the declaration that produced it, which the whole
    /// drift check already rests on, and an apply followed by drift for ever
    /// is what it would mean if it did not (DECISIONS 160).
    // Exhaustive rather than a wildcard, as every accessor here is: a change
    // added later that leaves a definition standing has to say so, or the
    // read-back would record whatever is there as this plan's own result.
    pub fn module(&self) -> Option<(&ObjectName, ModuleAfter<'_>)> {
        match self {
            Change::CreateModule { name, module } | Change::AlterModule { name, module } => {
                Some((name, ModuleAfter::Standing(module)))
            }
            Change::DropModule { name, .. } => Some((name, ModuleAfter::Gone)),
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
            | Change::CreateRole { .. }
            | Change::DropRole { .. }
            | Change::RenameRole { .. }
            | Change::Grant { .. }
            | Change::Revoke { .. } => None,
        }
    }

    /// The module this change acts on, if it is a module change at all.
    pub fn module_name(&self) -> Option<&ObjectName> {
        match self {
            Change::CreateModule { name, .. }
            | Change::AlterModule { name, .. }
            | Change::DropModule { name, .. } => Some(name),
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
            | Change::CreateRole { .. }
            | Change::DropRole { .. }
            | Change::RenameRole { .. }
            | Change::Grant { .. }
            | Change::Revoke { .. } => None,
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
            // What a dropped module destroys is the validity of whatever
            // depends on it, not data — so it faces the gate, but needs no
            // tombstone and no reason: the definition is in git history, which
            // is this object family's audit trail (ADR-0002).
            Change::DropModule { .. } => {
                r.insert(RiskClass::Destructive);
            }
            // A role rename keeps its membership — which is why it is a
            // rename and not drop + add — but the old name is gone the same
            // way a table's is, and `IS_ROLEMEMBER('old')` in a module or an
            // application breaks on the spot. Same gate as the other renames.
            Change::RenameTable { .. } | Change::RenameColumn { .. } | Change::RenameRole { .. } => {
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
            Change::UpdateRow { .. } => {
                r.insert(RiskClass::DataUpdate);
            }
            Change::DeleteRow { .. } => {
                r.insert(RiskClass::DataDelete);
            }
            Change::AddColumn { column, .. } => {
                // Existing rows have no value for a newly added column. SQL
                // Server can populate one from a DEFAULT or IDENTITY; without
                // either, a NOT NULL addition is the same data hazard as
                // tightening an existing nullable column (SPEC §7.1).
                if !column.nullable && !column.has_required_add_value_source() {
                    r.insert(RiskClass::NotNull);
                }
            }
            // A role drop's whole effect is revocation, on top of the reason
            // its tombstone already demanded (ADR-0005).
            Change::DropRole { .. } | Change::Revoke { .. } => {
                r.insert(RiskClass::Revoke);
            }
            Change::Grant { .. } => {
                r.insert(RiskClass::GrantWiden);
            }
            Change::CreateTable { .. }
            | Change::AlterColumnType { .. }
            | Change::AlterColumnNullability {
                to_nullable: true, ..
            }
            | Change::AlterColumnDefault { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::DropUnique { .. }
            | Change::DropForeignKey { .. }
            | Change::DropCheck { .. }
            | Change::AddIndex { .. }
            // Neither creating nor re-stating a module risks anything: a failed
            // CREATE OR ALTER rolls back with the plan's transaction and the
            // environment is unchanged.
            | Change::CreateModule { .. }
            | Change::AlterModule { .. }
            // Inserting a declared row adds what the declaration says should be
            // there; a failure (a duplicate key, a violated FK) rolls back with
            // the plan. Changing the mode emits nothing at all.
            | Change::InsertRow { .. }
            | Change::SetDataMode { .. }
            // Creating a role grants nothing by itself.
            | Change::CreateRole { .. } => {}
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

    /// What the analyzers said about this change (ADR-0008). Beside the risks,
    /// never inside them: a finding carries a severity the project chose and
    /// can be suppressed, a risk class is what the gate reads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<crate::finding::Finding>,
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
            findings: Vec::new(),
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
#[serde(deny_unknown_fields)]
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

    /// The risk classes `--allow` has to name: every gated one this plan
    /// involves. The advice a plan prints is built from this, never from
    /// [`ChangeSet::risks`] — advising `--allow grant-widen` for a flag the
    /// gate never asks for would teach reviewers to type flags by rote.
    pub fn gated_risks(&self) -> BTreeSet<RiskClass> {
        self.risks().into_iter().filter(|r| r.is_gated()).collect()
    }

    /// Risks not covered by `allowed`. Non-empty means apply must abort.
    ///
    /// Only the gated classes count: a labelled-but-ungated class
    /// (`grant-widen`) is in [`ChangeSet::risks`] for the reviewer and absent
    /// here for the gate, by design (ADR-0005).
    pub fn unapproved_risks(&self, allowed: &BTreeSet<RiskClass>) -> BTreeSet<RiskClass> {
        self.risks()
            .difference(allowed)
            .copied()
            .filter(|r| r.is_gated())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--allow` names a class and the gate explains it; a class that gained a
    /// name without an explanation would reach a reviewer as a bare word.
    #[test]
    fn every_risk_class_has_a_name_and_a_reason() {
        for r in RiskClass::ALL {
            assert!(!r.as_str().is_empty());
            assert!(r.why().len() > 20, "{r} needs a real explanation");
            assert_eq!(r.as_str().parse::<RiskClass>().unwrap(), r);
        }
    }

    /// ADR-0005: a widening is labelled for the reviewer and never stops an
    /// apply; a revoke is both labelled and gated. The plan's risk list has to
    /// show both, and the gate has to see exactly one.
    #[test]
    fn a_grant_is_labelled_but_not_gated_and_a_revoke_is_both() {
        let target: GrantTarget = "dbo.customer".parse().unwrap();
        let perms: BTreeSet<Permission> = [Permission::Select].into_iter().collect();
        let cs = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::Grant {
                    role: "r".into(),
                    target: target.clone(),
                    permissions: perms.clone(),
                }),
                PlannedChange::new(Change::Revoke {
                    role: "r".into(),
                    target,
                    permissions: perms,
                }),
            ],
        };
        assert!(cs.risks().contains(&RiskClass::GrantWiden));
        assert!(cs.risks().contains(&RiskClass::Revoke));
        let unapproved = cs.unapproved_risks(&BTreeSet::new());
        assert_eq!(
            unapproved,
            [RiskClass::Revoke].into_iter().collect(),
            "only the gated class stops the apply"
        );
        assert!(
            cs.unapproved_risks(&[RiskClass::Revoke].into_iter().collect())
                .is_empty()
        );
        // And a role drop is a revocation on top of its tombstone.
        assert!(
            Change::DropRole {
                uid: uid("r_aaaaaa"),
                name: "r".into(),
                members: Vec::new(),
            }
            .intrinsic_risks()
            .contains(&RiskClass::Revoke)
        );
        assert!(
            Change::CreateRole {
                uid: uid("r_aaaaaa"),
                name: "r".into()
            }
            .intrinsic_risks()
            .is_empty()
        );
    }
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
    fn adding_a_required_column_without_a_value_source_is_a_not_null_risk() {
        let make = |nullable, default: Option<&str>, identity: bool| {
            let mut column = Column::new(ty("int"));
            column.nullable = nullable;
            column.default = default.map(str::to_owned);
            column.identity = identity.then_some(crate::schema::Identity {
                seed: 1,
                increment: 1,
            });
            Change::AddColumn {
                uid: uid("c_k7x2mq"),
                table: "dbo.customer".parse().unwrap(),
                name: "score".into(),
                column: Box::new(column),
            }
        };

        assert_eq!(
            make(false, None, false).intrinsic_risks(),
            BTreeSet::from([RiskClass::NotNull])
        );
        assert!(make(true, None, false).intrinsic_risks().is_empty());
        assert!(make(false, Some("0"), false).intrinsic_risks().is_empty());
        assert_eq!(
            make(false, Some("CAST(NULL AS int)"), false).intrinsic_risks(),
            BTreeSet::from([RiskClass::NotNull])
        );
        assert!(make(false, None, true).intrinsic_risks().is_empty());
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
        assert_eq!(drop_column().table().unwrap().to_string(), "dbo.customer");
        assert_eq!(add_column().table().unwrap().to_string(), "dbo.customer");
        assert_eq!(drop_column().subject(), "dbo.customer");
        // A role is a principal, not an object: no table, but a subject.
        let grant = Change::Grant {
            role: "app_reader".into(),
            target: "dbo.customer".parse().unwrap(),
            permissions: BTreeSet::new(),
        };
        assert_eq!(grant.table(), None);
        assert_eq!(grant.subject(), "role app_reader");
    }

    // ---- modules (ADR-0002) ----

    fn a_view() -> crate::module::Module {
        crate::module::Module {
            kind: crate::module::ModuleKind::View,
            description: None,
            on: None,
            definition: "SELECT customer_id FROM dbo.customer".into(),
        }
    }

    /// Dropping a module destroys the validity of its dependents, so it faces
    /// the gate — but creating or re-stating one risks nothing, because a
    /// failed CREATE OR ALTER rolls back with the plan.
    #[test]
    fn only_dropping_a_module_is_risky() {
        let create = Change::CreateModule {
            name: "dbo.active_customer".parse().unwrap(),
            module: Box::new(a_view()),
        };
        let alter = Change::AlterModule {
            name: "dbo.active_customer".parse().unwrap(),
            module: Box::new(a_view()),
        };
        let drop = Change::DropModule {
            name: "dbo.active_customer".parse().unwrap(),
            kind: crate::module::ModuleKind::View,
        };
        assert!(create.intrinsic_risks().is_empty());
        assert!(alter.intrinsic_risks().is_empty());
        assert_eq!(
            drop.intrinsic_risks(),
            BTreeSet::from([RiskClass::Destructive])
        );
    }

    #[test]
    fn a_module_change_reports_its_own_name() {
        let c = Change::CreateModule {
            name: "dbo.active_customer".parse().unwrap(),
            module: Box::new(a_view()),
        };
        assert_eq!(c.table().unwrap().to_string(), "dbo.active_customer");
        assert_eq!(
            c.module_name().map(ToString::to_string).as_deref(),
            Some("dbo.active_customer")
        );
        assert!(add_column().module_name().is_none());
    }

    #[test]
    fn module_changes_round_trip_through_json() {
        let cs = ChangeSet {
            changes: vec![PlannedChange::new(Change::CreateModule {
                name: "dbo.active_customer".parse().unwrap(),
                module: Box::new(a_view()),
            })],
        };
        let back: ChangeSet = serde_json::from_str(&serde_json::to_string(&cs).unwrap()).unwrap();
        assert_eq!(cs, back);
    }
}

/// Why a [`Change::DeleteRow`] is in the plan (ADR-0004).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeleteCause {
    /// The table declares `mode: exact` and this row is not among the declared
    /// ones. Only `exact` ever produces this; `ensure` never emits a DELETE.
    Undeclared,
    /// The row's primary-key value changed, which is delete plus insert because
    /// rows carry no identity beyond their key.
    KeyChanged,
}
