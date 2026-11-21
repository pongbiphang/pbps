//! Change planning: comparing two "state + identity" pairs to produce a change
//! set.
//!
//! # Why both sides carry an identity file
//!
//! Matching up "which column is which" cannot rely on names — a rename makes the
//! names disagree across the two sides. Nor can it rely on this revision's intent
//! annotations, because a deployment may be many versions behind: when prod sits
//! at v1 and the declarations have reached v5, that rename intent left the
//! working tree long ago.
//!
//! So each side brings its own identity file and matching happens by **uid**: the
//! base's ids say `c_x → customer_name`, the declared ids say `c_x → full_name`,
//! and one comparison gives the rename directly, with no need to walk a chain of
//! names version by version. (This is exactly the problem SPEC §4.2 sets out to
//! solve.)
//!
//! # Where the base comes from
//!
//! `base` may come from the previous version in source control (an offline
//! preview) or from querying the database itself (Phase 3's authoritative plan).
//! This layer does not care which; it does the comparison and nothing else.

use crate::identity::type_refs::{TypeIdentity, TypeResolver, TypeSide};
use pbps_model::ColumnName;
use std::collections::{BTreeMap, BTreeSet};

use pbps_dialect::Dialect;
use pbps_model::ColumnChange;
use pbps_model::ConstraintChange;
use pbps_model::ExtensionChange;
use pbps_model::GrantChange;
use pbps_model::IndexChange;
use pbps_model::ModuleChange;
use pbps_model::PartitionChange;
use pbps_model::PolicyChange;
use pbps_model::RoleChange;
use pbps_model::RowChange;
use pbps_model::SearchChange;
use pbps_model::SearchObject;
use pbps_model::TableChange;
use pbps_model::TablePropertyChange;
use pbps_model::TypeChange;
use pbps_model::TypeValueChange;
use pbps_model::change::DeleteCause;
use pbps_model::data::cell;
use pbps_model::{
    Cell, Change, ChangeSet, ColumnRef, ColumnType, DataMode, DetachedKind, DetachedName,
    GrantTarget, Hints, IdsFile, Index, IndexMethod, IndexPart, Module, ModuleId, Permission,
    PlannedChange, PrimaryKey, PublicAccess, Renames, ReplicaIdentity, RoutineOrigin, Schema,
    Table, TableName, Uid, UniqueConstraint, Value, ValueSource,
};

/// What makes a table's replacement unorderable, for the one message that
/// serves every reason (`LastColumnReplacementUnorderable`).
fn last_column_reason(column: &ColumnName, block: pbps_dialect::last_column::Block) -> String {
    use pbps_dialect::last_column::Block;
    match block {
        Block::SameName => format!(
            "its only column `{}` is replaced by a new column of the same name, and the engine \
             will not drop a table's last column, nor add a name that is still there",
            column.shown()
        ),
        Block::Identity => format!(
            "its IDENTITY column `{}` is replaced by a new IDENTITY column, and the engine will \
             not drop a table's last column, nor hold two IDENTITY columns",
            column.shown()
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiffError {
    #[error("{reason}")]
    DropRequirement { reason: String },
    #[error("{target} on `{}`: storage parameter `{}` of a {method} index cannot be changed in place, and this model does not rebuild the index for it (DEC-2448.1)", .table.shown(), pbps_model::escape::Escaped(.parameter))]
    IndexParameterRefused {
        table: TableName,
        target: String,
        method: &'static str,
        parameter: String,
    },
    #[error("{target} on `{}`: its filegroup would change from {was} to {now}; moving data between filegroups is a rebuild, and no plan does that yet (DEC-2813.1)", .table.shown())]
    FilegroupChangeRefused {
        table: TableName,
        target: String,
        was: String,
        now: String,
    },
    #[error("exclusion constraint `{}` on `{}`: {reason} (DEC-2448.3)", .name.shown(), .table.shown())]
    ExclusionRefused {
        table: TableName,
        name: pbps_model::ObjectLocalName,
        reason: String,
    },
    #[error("{}: {reason} (DEC-2728.1)", .name.shown())]
    SearchRefused {
        name: pbps_model::SearchRef,
        reason: String,
    },
    #[error("sequence `{}`: {reason} (DEC-2721.1)", .name.shown())]
    SequenceRefused {
        name: pbps_model::ObjectName,
        reason: String,
    },
    #[error("extension `{}`: {action} is unsupported in the read-only extension phase (DEC-2447.1)", .name.shown())]
    ExtensionLifecycleUnsupported {
        name: pbps_model::ObjectLocalName,
        action: String,
    },
    #[error("extension `{}`: DROP EXTENSION deletes the package's members and what depends on it, so it is planned only against a database (`plan --db`), where the plan reads and pins what the drop reaches (DEC-2447.8)", .name.shown())]
    ExtensionDropNeedsDatabase { name: pbps_model::ObjectLocalName },
    #[error("extension `{}`: DROP EXTENSION would reach what this plan does not remove: {}; drop or release each in an earlier plan (`ALTER ... NO DEPENDS ON EXTENSION` for an object that is not declared), then drop the package (DEC-2447.8)", .name.shown(), .found.join("; "))]
    ExtensionDropRefused {
        name: pbps_model::ObjectLocalName,
        found: Vec<String>,
    },
    #[error("extension `{}`: ALTER EXTENSION UPDATE from `{}` to `{}` runs the package's update scripts, so it is planned only against a database (`plan --db`), where a rehearsal reads and pins what they leave (DEC-2447.10)", .name.shown(), pbps_model::escape::Escaped(.from), pbps_model::escape::Escaped(.to))]
    ExtensionUpdateNeedsDatabase {
        name: pbps_model::ObjectLocalName,
        from: String,
        to: String,
    },
    #[error("extension `{}`: CREATE EXTENSION runs the package's install script, so it is planned only against a database (`plan --db`), where a rehearsal reads and pins what the script installs (DEC-2447.6)", .name.shown())]
    ExtensionCreateNeedsDatabase { name: pbps_model::ObjectLocalName },
    #[error("extension `{}`: ALTER EXTENSION SET SCHEMA from `{}` to `{}` is judged against the members, owners and relocatability the target holds, so it is planned only against a database (`plan --db`), where a rehearsal reads and pins what it leaves (DEC-2447.11)", .name.shown(), pbps_model::escape::Escaped(.from), pbps_model::escape::Escaped(.to))]
    ExtensionMoveNeedsDatabase {
        name: pbps_model::ObjectLocalName,
        from: String,
        to: String,
    },
    #[error("type `{}`: new enum label `{}` is used by {}; apply the label first, then the rest", .name.shown(), pbps_model::escape::Escaped(.label), pbps_model::escape::Escaped(.subject))]
    EnumValueUse {
        name: pbps_model::TypeRef,
        label: String,
        subject: String,
    },
    #[error("type `{name}`: {reason}", name = .name.shown())]
    TypeLifecycleUnsupported {
        name: pbps_model::TypeRef,
        reason: String,
    },
    #[error(
        "column {column}: its partition default cannot be planned: {reason}",
        column = .column.shown(),
    )]
    PartitionDefault { column: ColumnRef, reason: String },
    /// In most databases IDENTITY cannot be changed with ALTER; the whole table
    /// has to be rebuilt. That is beyond what declarative automation should do on
    /// its own — a human has to choose the migration strategy.
    #[error(
        "the IDENTITY property of {column} changed, but IDENTITY cannot be modified with ALTER",
        column = .column.shown(),
    )]
    IdentityChangeUnsupported { column: ColumnRef },

    /// A recorded conversion (#2331) written for another starting type than
    /// this environment's column holds. An expression is written for one
    /// source type, and applying it to another would transform data nobody
    /// reviewed the expression for.
    #[error(
        "column {column}: a conversion is recorded from `{recorded}` to `{to}`, but this column \
         is `{actual}`. An expression written for one starting type is never applied to another. \
         Record one for this start: `pbps convert {column} --using - --from '{actual}'`",
        column = .column.shown(),
        recorded = pbps_model::escape::Escaped(.recorded),
        actual = pbps_model::escape::Escaped(.actual),
        to = pbps_model::escape::Escaped(.to),
    )]
    ConversionFromMismatch {
        column: ColumnRef,
        recorded: String,
        actual: String,
        to: String,
    },

    /// A recorded conversion (#2331) for a column this plan also renames: the
    /// expression names the column, and which name it means would depend on
    /// the order the two run in.
    #[error(
        "column {column}: a conversion is recorded for it, and this plan also renames it. The \
         expression names the column once, so apply the rename in a plan of its own first and \
         then plan the type change",
        column = .column.shown(),
    )]
    ConversionWithRename { column: ColumnRef },

    /// A type change that leaves a foreign key between two types the engine
    /// cannot join (#2359, DEC-2359.1): one end retyped and the other left
    /// as it was, or both retyped to types that do not meet. The apply would
    /// take the key down for the change and be refused when it put it back.
    #[error(
        "foreign key {constraint} of {table} would join {referencing} (`{referencing_type}`) to \
         {referenced} (`{referenced_type}`) once this plan has run, and the engine cannot join \
         those two types, so it would refuse the key at the apply. {next}",
        constraint = .constraint.shown(),
        table = .table.shown(),
        referencing = .referencing.shown(),
        referenced = .referenced.shown(),
        referencing_type = pbps_model::escape::Escaped(.referencing_type),
        referenced_type = pbps_model::escape::Escaped(.referenced_type),
    )]
    ForeignKeyTypesDiffer {
        table: TableName,
        constraint: pbps_model::ObjectLocalName,
        referencing: ColumnRef,
        referenced: ColumnRef,
        referencing_type: String,
        referenced_type: String,
        /// What to do about it, which depends on which end this plan retypes.
        next: String,
    },

    /// The identity file records conversions for an engine that converts
    /// implicitly and has no `USING` (#2331, ADR-0012 section 5).
    #[error(
        "the identity file records {count} conversion(s), but this engine has no `USING` clause \
         and converts a column's values itself, so nothing would run the expression. Remove \
         the `conversions` entries from the identity file"
    )]
    ConversionsUnsupported { count: usize },

    /// A `serial` put on a column of an `UNLOGGED` table, added with it or
    /// given to it (#1631, DEC-1631.1): its sequence is created logged, and
    /// the reader holds a `serial` only where the two agree. Or an owned
    /// sequence changed on an engine without one (#1889).
    #[error(
        "column {column}: a `serial` put on an UNLOGGED table, or an owned sequence changed \
         where this engine has none, is not planned yet. Make the change by hand, then pull.",
        column = .column.shown(),
    )]
    SequenceChangeUnsupported { column: ColumnRef },

    /// An identity changed on a partitioned table, or any numbering source
    /// changed on a partition itself: identity inheritance differs between
    /// supported PostgreSQL versions (#1681, DEC-2219.1).
    #[error(
        "column {column}: its identity changes on a partitioned table, or its numbering source \
         changes on a partition, which is not planned yet (issue #1681). Make the change by \
         hand, then pull.",
        column = .column.shown(),
    )]
    NumberingChangeOnPartitionedTable { column: ColumnRef },

    /// An identity or owned sequence kept, whose increment changes sign
    /// (#1889): the sequence keeps the bounds of the direction it was made
    /// for, so a start in the other is refused, and continuing "past" the
    /// numbers issued has no meaning once they run the other way.
    #[error(
        "column {column}: its identity or `serial` sequence changes direction, from counting up \
         to counting down or back, which is not planned. Give the column a new source in two \
         steps, removing the old one first, or make the change by hand, then pull.",
        column = .column.shown(),
    )]
    NumberingDirectionChange { column: ColumnRef },

    /// Rows of a table referencing one whose declared inserts follow an
    /// identity put on it, after the deletes that may clear its NULLs
    /// (#1930 review): written first, they would reference rows not yet
    /// there.
    #[error(
        "rows of {table} reference {referenced}, whose rows this plan inserts after putting a \
         numbering source on it, once its deleted rows are gone; {table}'s rows would be \
         written first. Apply the row changes of {referenced} in a plan of their own first.",
        table = .table.shown(), referenced = .referenced.shown(),
    )]
    RowsReferenceLateIdentityRows {
        table: TableName,
        referenced: TableName,
    },

    /// A column that becomes generated, stops being generated, or changes
    /// its generation kind (DEC-1168.1). The engine has no in-place form for
    /// any of those: an ordinary column cannot be given an expression, and
    /// dropping one would keep today's computed values as ordinary data. Only
    /// the expression of a column that stays generated is changed in place.
    #[error(
        "column {column}: a change between generated and not generated, or between stored and \
         virtual, has no in-place form. To change it, add a new column and drop this one.",
        column = .column.shown(),
    )]
    GenerationChangeUnsupported { column: ColumnRef },

    /// A plan that renames, drops, retypes or changes the nullability of a
    /// column a SQL Server computed column may read, while the computed
    /// column stands: the engine refuses each (15336, 4922; measured on 17.0,
    /// #1174). A computed column the plan drops, or changes and so drops and
    /// re-adds, is out of the way first and is not this.
    #[error(
        "computed column {computed} may read `{column}`, which this plan {change}, and SQL \
         Server refuses that while the computed column stands. Drop the computed column, or \
         change its expression, in a plan of its own first, then this one.",
        computed = .computed.shown(), column = .column.shown(),
    )]
    ComputedInputChanged {
        computed: ColumnRef,
        column: ColumnName,
        change: &'static str,
    },

    /// A plan that alters or drops a function a SQL Server computed column
    /// may call while the column stands (3729, measured on 17.0), or adds a
    /// computed column calling one the plan creates, which comes after it
    /// (#1174).
    #[error(
        "computed column {computed} may call `{function}`, which this plan {change}. Apply \
         the function change and the computed column change in separate plans.",
        computed = .computed.shown(),
    )]
    ComputedFunctionChanged {
        computed: ColumnRef,
        function: String,
        change: &'static str,
    },

    /// The typed `depends_on:` annotations of a plan cannot be bound to the
    /// objects of its two states (#2469). The words are the registry's.
    #[error("{0}")]
    Annotations(String),

    /// A `data:` block on a table whose primary key cannot key its rows
    /// (ADR-0004). `validate` says the same thing against the file and the
    /// line; this is here so that a differ reached another way never quietly
    /// produces a plan with the rows left out of it.
    #[error(
        "{table} declares `data:` but has no single-column primary key, so its rows have no identity",
        table = .table.shown(),
    )]
    DataWithoutKey { table: TableName },

    /// A permanent table whose foreign key references an unlogged one, which
    /// PostgreSQL refuses (`constraints on permanent tables may reference only
    /// permanent tables`, measured on 16 and 18, #1443).
    #[error(
        "{table}: its foreign key `{key}` references {target}, which is unlogged, and \
         PostgreSQL lets only an unlogged table reference an unlogged one. Make {table} \
         unlogged too, or {target} permanent.",
        key = .key.shown(), table = .table.shown(), target = .target.shown(),
    )]
    PermanentReferencesUnlogged {
        table: TableName,
        key: pbps_model::ObjectLocalName,
        target: TableName,
    },

    /// A permanent table whose foreign key references a partitioned table
    /// with an unlogged partition (#1580, DEC-1580.1). PostgreSQL accepts
    /// it, measured on 16 and 18, whichever is made first: it refuses only a
    /// key naming the unlogged table itself. A crash empties the partition
    /// and leaves the referencing rows pointing at nothing, which is what
    /// the direct refusal exists to prevent.
    #[error(
        "{table}: its foreign key `{key}` references {target}, whose partition {partition} \
         is unlogged; a crash would empty {partition} and leave rows of {table} referencing \
         nothing, which PostgreSQL does not refuse through a partitioned table. Make \
         {partition} permanent, or {table} unlogged.",
        key = .key.shown(), table = .table.shown(), target = .target.shown(), partition = .partition.shown(),
    )]
    PermanentReferencesUnloggedPartition {
        table: TableName,
        key: pbps_model::ObjectLocalName,
        target: TableName,
        partition: TableName,
    },

    /// A change to a table with `system_time`, on either side, other than
    /// creating it or adding a nullable column (#1176, #1177, DEC-1177.1).
    /// The engine refuses some (dropping the table, 13552; altering the
    /// period, 13599) and does others with history side effects nobody
    /// declared: DROP COLUMN deletes that column's history, and ADD NOT NULL
    /// with a default writes the default into every history row (measured on
    /// 17.0). `what` names each refused change, in plan order.
    #[error(
        "{table} has `system_time`, and the only change pbps makes to such a table is adding \
         a nullable column; this plan would also {}. SQL Server refuses some of these and \
         does others by rewriting or deleting the table's history. Declare {table} as it is \
         recorded apart from nullable columns it gains, and make the rest by hand.",
        what.join(", "),
        table = .table.shown(),
    )]
    TemporalTableChange { table: TableName, what: Vec<String> },

    /// A change to a partitioned table or a partition, on either side, other
    /// than creating it (#1170, DEC-1170.1): the changes a hierarchy takes
    /// are #1171's to qualify, and some reach every partition under a lock.
    #[error(
        "{table} is a partitioned table or a partition, and pbps can create one but not change \
         it yet; this plan would {}. Declare {table} as it is recorded, and make the change by \
         hand.",
        what.join(", "),
        table = .table.shown(),
    )]
    PartitionedTableChange { table: TableName, what: Vec<String> },

    #[error(
        "cannot plan the blocking rewrite of {table}: {why}. Table rebuild support is tracked \
         in issue #2298",
        table = .table.shown(),
    )]
    RebuildTable { table: TableName, why: String },

    #[error("the rebuild intent for {uid} has no source parent identity")]
    RebuildIdentity { uid: Uid },

    /// A partition declared as an ordinary table whose shape is not its
    /// parent's, names aside (#1544): the detach gives it the parent's shape,
    /// and anything more is a second change to make in a later revision.
    #[error(
        "{table} is detached from {parent}, and declared as other than its parent's shape: {}. \
         Declare it with its parent's columns, keys, constraints and indexes (under any names), \
         and change it in a later revision.",
        what.join("; "),
        table = .table.shown(), parent = .parent.shown(),
    )]
    DetachedShape {
        table: TableName,
        parent: TableName,
        what: Vec<String>,
    },

    /// An ordinary table declared as a partition that the engine would not
    /// attach as declared, or would attach into something the model does
    /// not hold (#1545, DEC-1545.1): each reason is a change to make to the
    /// table in an earlier plan.
    #[error(
        "{table} is attached to {parent} as a partition, but {}. Bring the table to its \
         parent's shape in an earlier plan, then attach it.",
        what.join("; "),
        table = .table.shown(), parent = .parent.shown(),
    )]
    AttachedShape {
        table: TableName,
        parent: TableName,
        what: Vec<String>,
    },

    /// A `data:` table whose primary key moved to a different column. The row
    /// keys on each side are values of that side's key column, so the two sets
    /// have nothing in common and matching them by text would update and
    /// delete the wrong rows. Renaming the key column is fine — same column,
    /// same values.
    #[error(
        "{table} declares `data:` and its primary key moved to another column, so the declared \
         rows cannot be matched to the recorded ones. Remove the block, apply the key change, \
         then declare the rows again — or rename the column instead of replacing it",
        table = .table.shown(),
    )]
    DataKeyColumnChanged { table: TableName },

    #[error(
        "{table} has no baseline primary key and this plan does not restore the declared key, so its data rows cannot be matched",
        table = .table.shown(),
    )]
    DataBaselineKeyAbsent { table: TableName },

    /// Column renames on one table that trade names in a cycle, which skipped
    /// revisions can produce (`a -> c`, then `b -> a`, then `c -> b`). No
    /// order of the renames is one an engine performs: each target is still
    /// held when its rename runs. It takes a temporary name, which is the
    /// deployer's to choose (DEC-541.1).
    #[error(
        "{table} renames columns {} into each other's names in a cycle, and no order of those \
         renames is one the engine performs. Apply the revisions that form it in separate \
         deployments, or rename one of the columns through a temporary name first",
        columns.iter().map(|c| format!("`{c}`",
            c = c.shown(),
)).collect::<Vec<_>>().join(", "),
        table = .table.shown(),
    )]
    ColumnRenameCycle {
        table: TableName,
        columns: BTreeSet<ColumnName>,
    },

    /// Table renames that each wait for a name another of them still holds,
    /// in a cycle (#1966). Skipped revisions can produce one (`a -> c`, then
    /// `b -> a`, then `c -> b`), and so can two moves across schemas, each
    /// passing through its old name in the other's destination. No order of
    /// the renames is one an engine performs; the temporary name is the
    /// deployer's to choose, as for columns (DEC-541.1, DEC-1966.1).
    #[error(
        "the table renames {} each need a name another of them still holds, in a cycle, and no \
         order of those renames is one the engine performs. Apply the revisions that form it in \
         separate deployments, or rename one of the tables through a temporary name first",
        renames.iter().map(|(from, to)| format!("`{from} -> {to}`", from = from.shown(), to = to.shown())).collect::<Vec<_>>().join(", ")
    )]
    TableRenameCycle {
        renames: Vec<(TableName, TableName)>,
    },

    /// A table's columns all replaced, and no order of the replacement is one
    /// the engine performs (#2963, DEC-2963.1): the engine will not drop a
    /// table's last column, and the add that would come first is one it
    /// refuses (a name that stands, a second IDENTITY column). A temporary
    /// name is the deployer's to choose, as for renames.
    #[error(
        "{table} replaces every one of its columns, and no order of those changes is one the \
         engine performs: {reason}. Keep the existing column instead of replacing it, or \
         make the change in two revisions: add a column of another kind or name first, then \
         replace it",
        table = .table.shown(),
        reason = last_column_reason(.column, *.block),
    )]
    LastColumnReplacementUnorderable {
        table: TableName,
        column: ColumnName,
        block: pbps_dialect::last_column::Block,
    },

    /// `depends_on:` declarations that close a cycle (#2083, DEC-2083.1):
    /// no order honours them all, and planning the members in name order
    /// would break one without saying which. Refused here as well as by
    /// `validate`, because an offline plan does not ask `validate`'s
    /// questions first.
    #[error("{}", pbps_model::module::dependency_cycle_problem(modules))]
    DependsOnCycle { modules: Vec<ModuleId> },

    /// Owned sequences renamed into each other's names, in a cycle (#1930
    /// review). As for tables and columns, no order of the renames is one
    /// the engine performs, and the temporary name is the deployer's to
    /// choose (DEC-541.1, DEC-1966.1): refused here rather than by the
    /// engine mid-apply.
    #[error(
        "the sequence renames {} each need a name another of them still holds, in a cycle, and \
         no order of those renames is one the engine performs. Apply the revisions that form it \
         in separate deployments, or rename one of the sequences through a temporary name first",
        renames.iter().map(|(from, to)| format!("`{from} -> {to}`", from = from.shown(), to = to.shown())).collect::<Vec<_>>().join(", ")
    )]
    SequenceRenameCycle {
        renames: Vec<(TableName, TableName)>,
    },

    /// A baseline without a primary key, restored by this plan on a column
    /// the baseline does not have. Not [`Self::DataKeyColumnChanged`]: there
    /// was no key to move, and "rename the column instead" does not apply.
    /// The recorded rows were keyed before that column existed, so none of
    /// their keys are its values (DECISIONS 539).
    #[error(
        "{table} has no baseline primary key and this plan restores it on `{column}`, a column \
         the baseline does not have, so the recorded data rows cannot be matched to the declared \
         ones. Restore the key on a column the table already has, or remove the block, apply \
         the key change, then declare the rows again",
        table = .table.shown(), column = .column.shown(),
    )]
    DataBaselineKeyOnNewColumn {
        table: TableName,
        column: ColumnName,
    },

    #[error(
        "{table} has a baseline primary key with {} columns ({:?}); pbps matches data rows only on a single-column key",
        columns.len(),
        // Debug already escaped the former Vec<String>; preserve that spelling.
        columns.iter().map(ColumnName::raw).collect::<Vec<_>>(),
        table = .table.shown(),
    )]
    DataBaselineKeyNotSingle {
        table: TableName,
        columns: Vec<ColumnName>,
    },
}

enum BaselineDataKey<'a> {
    Absent,
    Single(&'a ColumnName),
    Multiple(&'a [ColumnName]),
}

impl<'a> BaselineDataKey<'a> {
    fn of(table: &'a Table) -> Self {
        match table.primary_key.as_ref().map(|pk| pk.columns.as_slice()) {
            None => Self::Absent,
            Some([column]) => Self::Single(column),
            Some(columns) => Self::Multiple(columns),
        }
    }
}

/// One side's complete input: a state plus its own identity mapping.
#[derive(Debug, Clone, Copy)]
pub struct Side<'a> {
    pub schema: &'a Schema,
    pub ids: &'a IdsFile,
}

/// Both sides' identity files, indexed once per diff for the passes that look
/// up per table or per column (#1870).
struct Indexes<'a> {
    base: pbps_model::IdsIndex<'a>,
    declared: pbps_model::IdsIndex<'a>,
}

/// Compares the base against the declarations and produces a change set.
///
/// `description` is **not compared** yet: it only affects data-catalogue prose,
/// not structure, and writing it to an extended property belongs to Phase 5.
///
/// The hints are the **declared** side's: a strategy describes how to operate on
/// the table as it will be, and a table that no longer exists has nothing left
/// to operate on (ADR-0003).
pub fn diff(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
) -> Result<crate::order::DiffPlan, Vec<DiffError>> {
    diff_rebuilding(base, declared, dialect, hints, &BTreeSet::new())
}

/// [`diff`] for the plan a resolver's scratch compiles: from the packages the
/// declarations adopt to the declarations. Nothing runs this order. Scratch
/// takes its changes and re-settles its own sequence over them, one part at a
/// time, with the text edges as guesses the engine corrects (DEC-1959.6,
/// DEC-2143.9), so the debug observer does not judge it as an order that runs,
/// as it does not judge the resolver's other intermediate plans. A pair of
/// modules whose texts name each other, one name being only an alias, has no
/// lexical order at all and would otherwise fail the observer here.
pub fn bootstrap(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
) -> Result<crate::order::DiffPlan, Vec<DiffError>> {
    // The buildable form of both sides: a text search object no identity names is not built,
    // so the differ does not refuse a held one as having none and stop a rehearsal or a
    // resolver run before it starts (#2855, DEC-2855.1).
    let (base_schema, declared_schema) = (
        base.schema.buildable(base.ids),
        declared.schema.buildable(declared.ids),
    );
    rebuilding_by(
        Side {
            schema: &base_schema,
            ids: base.ids,
        },
        Side {
            schema: &declared_schema,
            ids: declared.ids,
        },
        dialect,
        hints,
        &BTreeSet::new(),
        // From nothing, no module stands to be rebuilt, so the only difference
        // from `diff` is that the plan is not observed.
        Rebinding::Evidence,
        Screen::Text,
        None,
    )
}

/// [`diff`], with these unchanged modules rebuilt as if the declarations had
/// edited them.
///
/// For the engine fact the declarations cannot see: on PostgreSQL a module a
/// rebuilt module's dependents include has to be dropped and created around
/// it (#314, DEC-314.1), and only a connected read of `pg_depend` knows which.
/// Rebuilt here, not synthesized by the caller, because a rebuild is more
/// than its two statements: the grants it takes with it and the `PUBLIC`
/// execute it gets back are written by the passes below, and a caller adding
/// the pair after those passes had run would restore neither. It is the same
/// place `rebound_modules` adds the rebuilds a dialect asks for (DECISIONS 422).
///
/// An id that the plan already changes, or that is not declared on both
/// sides, is ignored: there is nothing unchanged to rebuild.
pub fn diff_rebuilding(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
    also: &BTreeSet<ModuleId>,
) -> Result<crate::order::DiffPlan, Vec<DiffError>> {
    rebuilding_by(
        base,
        declared,
        dialect,
        hints,
        also,
        Rebinding::Candidates,
        Screen::Text,
        None,
    )
}

/// Who decides which unchanged modules a plan's arrivals rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rebinding {
    /// ADR-0013's candidate test, through `Dialect::rebound_modules`: the
    /// conservative rule ordinary planning keeps (DECISIONS 422).
    Candidates,
    /// The resolver's binding evidence, which the caller passes as `also`.
    /// A candidate is a reason to resolve, not proof that a rebuild is
    /// needed (ADR-0016 §1, DEC-1515.1), so the differ adds none of its own.
    Evidence,
}

/// Who judges a standing SQL Server computed column against the plan's
/// changes to a column it reads or a function it calls (#1460).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// The differ, by text, over-approximating (DEC-1174.1): an offline
    /// plan's only judge, and never applied (SPEC §7.3).
    Text,
    /// The catalog's expression edges, which a connected SQL Server plan
    /// reads and matches under the database's collation (DEC-1431.1). The
    /// text screen folds case, and refused a retype of `A` beside a computed
    /// column reading `a` in a case-sensitive database. A computed column the
    /// plan adds, new or again, has no edge for its new text and is still
    /// screened here (#1459).
    Catalog,
}

/// [`diff_rebuilding`], or [`diff_connected`] for `Screen::Catalog`, for a
/// connected plan, which also hands over `read_back`: what it read, before the
/// recorded declared texts are put in place of the engine's spelling. Only
/// the comparisons the engine makes by parse tree read it; every other
/// comparison stays on the recorded texts (#1642 review).
pub fn diff_read_back(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
    also: &BTreeSet<ModuleId>,
    screen: Screen,
    read_back: &Schema,
) -> Result<crate::order::DiffPlan, Vec<DiffError>> {
    rebuilding_by(
        base,
        declared,
        dialect,
        hints,
        also,
        Rebinding::Candidates,
        screen,
        Some(read_back),
    )
}

/// [`diff_rebuilding`] for a connected SQL Server plan, which judges its
/// standing computed columns by the catalog's edges (`Screen::Catalog`).
pub fn diff_connected(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
    also: &BTreeSet<ModuleId>,
) -> Result<crate::order::DiffPlan, Vec<DiffError>> {
    rebuilding_by(
        base,
        declared,
        dialect,
        hints,
        also,
        Rebinding::Candidates,
        Screen::Catalog,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn rebuilding_by(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
    also: &BTreeSet<ModuleId>,
    rebinding: Rebinding,
    screen: Screen,
    read_back: Option<&Schema>,
) -> Result<crate::order::DiffPlan, Vec<DiffError>> {
    let mut d = diff_partial_rebuilding(
        base, declared, dialect, hints, also, rebinding, screen, read_back,
    );
    // Only an extended input takes the general route, and only then can an
    // annotation edge discharge a refusal that a module-only plan keeps: a
    // module-only plan never builds the contract, which needs the identity of
    // every table of both states that an offline plan may not hold.
    let extended = hints.annotations.is_extended();
    let mut deferred = Vec::new();
    if extended {
        d.errors.retain(|error| {
            if matches!(error, DiffError::ComputedFunctionChanged { .. }) {
                deferred.push(error.clone());
                false
            } else {
                true
            }
        });
    }
    if d.errors.is_empty() {
        // The resolver's intermediate plan is not an order anything runs: its
        // final one is observed where it is made.
        #[cfg(any(test, debug_assertions))]
        if rebinding == Rebinding::Candidates {
            crate::order::shadow::observe_plan(base, declared, dialect, hints, &d.changes);
        }
        let plan = finish_diff(base, declared, dialect, d.changes);
        if !extended {
            return Ok(plan);
        }
        let mut plan = plan
            .with_annotations(
                pbps_model::operation::Snapshot {
                    schema: base.schema,
                    ids: base.ids,
                },
                pbps_model::operation::Snapshot {
                    schema: declared.schema,
                    ids: declared.ids,
                },
                &hints.annotations.recorded,
                &hints.annotations.declared,
                if dialect.rebuilds_modules() {
                    pbps_model::operation::ModuleEdits::Rebuilt
                } else {
                    pbps_model::operation::ModuleEdits::InPlace
                },
            )
            .map_err(|why| vec![DiffError::Annotations(why)])?;
        plan.defer(deferred);
        Ok(plan)
    } else {
        Err(d.errors)
    }
}

// The annotation entry point admits the same complete differ result, with
// class-order refusals checked against its final hard order before returning.
pub(crate) fn finish_diff(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    changes: ChangeSet,
) -> crate::order::DiffPlan {
    let pending = crate::order::boundaries::meetings(base, declared, &changes);
    let mut plan = crate::order::propose(changes);
    plan.pending_boundaries(
        pending,
        if dialect.rebuilds_modules() {
            pbps_model::operation::ModuleEdits::Rebuilt
        } else {
            pbps_model::operation::ModuleEdits::InPlace
        },
    );
    crate::order::differ(plan)
}

/// Everything [`diff`] found, with the differences it could not express kept
/// *beside* the ones it could rather than replacing them.
///
/// # Why both, and why only `verify` wants them
///
/// The two callers ask different questions. `plan` asks "may this be applied",
/// and one difference the emitter has no statement for makes the answer no —
/// a partial plan is worse than none, so it takes the `Result`.
///
/// `verify` asks "what is different", and that is a report. Dropping the
/// expressible changes on the floor because *another* table had an altered
/// `IDENTITY` undercounted the drift, and sent the `on_drift` hook a payload
/// missing differences the differ had already phrased perfectly well.
pub fn diff_partial(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
) -> Diffed {
    diff_partial_rebuilding(
        base,
        declared,
        dialect,
        hints,
        &BTreeSet::new(),
        Rebinding::Candidates,
        Screen::Text,
        None,
    )
}

/// Lifecycle refusals for the attributes no executable change covers, shared
/// with connected prerequisite admission so a missing declared type is named
/// before its column's reference gate. `alterable` leaves out what a retained
/// domain changes in place: its NOT NULL, default and checks (DEC-1635.8).
pub fn type_differences(
    base: &Schema,
    declared: &Schema,
    dialect: &(impl Dialect + ?Sized),
    alterable: bool,
) -> Vec<DiffError> {
    let mut errs = Vec::new();
    // Whole-object changes are handled by uid; retained definitions still
    // refuse the attributes no change covers.
    let names: BTreeSet<_> = base.types.keys().chain(declared.types.keys()).collect();
    for name in names {
        let reason = match (base.types.get(name), declared.types.get(name)) {
            (None, Some(_)) | (Some(_), None) => None,
            (Some(old), Some(new)) => {
                let normalize = |definition: &pbps_model::ManagedType| {
                    let mut definition = definition.clone();
                    if let pbps_model::ManagedType::Domain { base, .. } = &mut definition {
                        *base = dialect
                            .normalize_type(base)
                            .unwrap_or_else(|_| base.clone());
                    }
                    definition
                };
                let mut fields = normalize(old).differences(&normalize(new));
                if alterable {
                    fields.retain(|field| {
                        !matches!(
                            *field,
                            "domain NOT NULL" | "domain default" | "domain checks"
                        )
                    });
                }
                (!fields.is_empty()).then(|| {
                    let why = match new {
                        pbps_model::ManagedType::Enum { .. } => {
                            "an enum's labels change only through its recorded uid (#1635 slice 3, DEC-1635.6, DEC-1635.7)"
                        }
                        pbps_model::ManagedType::Domain { .. } => {
                            "a domain's base type and collation change by rebuilding it (#1635 slice 4b), and its NOT NULL, default and checks only through its recorded uid (DEC-1635.8)"
                        }
                    };
                    format!("{} changed; {why}", fields.join(", "))
                })
            }
            (None, None) => None,
        };
        if let Some(reason) = reason {
            errs.push(DiffError::TypeLifecycleUnsupported {
                name: name.clone(),
                reason,
            });
        }
    }

    errs
}

/// The DEFAULT partition a new range or list partition is made beside, under
/// the name the plan leaves it (#1547, DEC-1547.1, #1632): one the base holds
/// as the parent's DEFAULT and the declarations keep as that parent's. Its
/// rows in the new range or list are moved into the partition before it is attached, so the
/// plan is the same whether it holds any or not. A parent the plan creates
/// has none, and neither has a DEFAULT the plan drops, detaches or makes:
/// one leaving goes in class 6, before the partition is created, and a new
/// one starts empty. The name is the declared one because renames run in
/// class 1, before every create.
fn standing_default(
    base: Side<'_>,
    declared: Side<'_>,
    of: &pbps_model::PartitionOf,
) -> Option<TableName> {
    // A HASH parent has no DEFAULT (#1632).
    if !matches!(
        of.bound,
        pbps_model::PartitionBound::Range { .. } | pbps_model::PartitionBound::List { .. }
    ) {
        return None;
    }
    let parent = base.ids.tables.get(declared.ids.table_uid(&of.parent)?)?;
    let is_default_of = |side: Side<'_>, name: &TableName, parent: &TableName| {
        side.schema
            .tables
            .get(name)
            .and_then(|t| t.partition_of.as_ref())
            .is_some_and(|o| {
                &o.parent == parent && matches!(o.bound, pbps_model::PartitionBound::Default)
            })
    };
    declared.ids.tables.iter().find_map(|(uid, name)| {
        let was = base.ids.tables.get(uid)?;
        (is_default_of(declared, name, &of.parent) && is_default_of(base, was, parent))
            .then(|| name.clone())
    })
}

// Visible in the crate for the ordering benchmark, which times planning
// without the test-only observer that `diff` runs (#1965).
#[allow(clippy::too_many_arguments)]
pub(crate) fn diff_partial_rebuilding(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
    also: &BTreeSet<ModuleId>,
    rebinding: Rebinding,
    screen: Screen,
    read_back: Option<&Schema>,
) -> Diffed {
    let mut changes = Vec::new();
    // An approved rewrite must never degrade to an ordinary DROP/CREATE: what
    // each lineage record means depends on what this environment holds. PR4a
    // qualifies the rewrite but deliberately grants no execution (DEC-2298.12).
    let lineage = crate::rebuild::lineage_errors(base, declared, dialect);
    if !lineage.is_empty() {
        return Diffed {
            changes: ChangeSet::default(),
            errors: lineage,
        };
    }
    let rebuilds: Vec<_> = declared
        .ids
        .rebuilds
        .values()
        .filter_map(|r| declared.ids.tables.get(&r.replacement_root))
        .filter_map(|table| crate::rebuild::qualify(base, declared, table, dialect).ok())
        .map(|root| Change::Partition(PartitionChange::RebuildPartitionedTable(Box::new(root))))
        .map(|change| PlannedChange {
            risks: dialect.change_risks(&change),
            ..PlannedChange::new(change)
        })
        .collect();
    if !rebuilds.is_empty() {
        return Diffed {
            changes: ChangeSet { changes: rebuilds },
            errors: crate::extensions::differences(base, declared, dialect.name()),
        };
    }
    // The passes below read module-to-module edges; the other references are
    // the transient contract's (`rebuilding_by`).
    let module_deps = hints.annotations.modules(declared.schema);
    let mut errs: Vec<DiffError> =
        pbps_model::module::dependency_cycles(declared.schema, &module_deps)
            .into_iter()
            .map(|modules| DiffError::DependsOnCycle { modules })
            .collect();
    errs.extend(crate::extensions::differences(
        base,
        declared,
        dialect.name(),
    ));
    diff_sequences(base.schema, declared.schema, dialect, &mut errs);
    if !dialect.records_conversions() && !declared.ids.conversions.is_empty() {
        errs.push(DiffError::ConversionsUnsupported {
            count: declared.ids.conversions.len(),
        });
    }

    let (mut label_changes, label_errors) = crate::enum_labels::changes(base, declared, hints);
    errs.extend(label_errors);
    let comparison = crate::retained::comparison(base, declared, hints, &mut label_changes);
    let comparison =
        crate::extensions::forward_moved_types(comparison, base, declared, &hints.extension_moves);
    let comparison = crate::extensions::forward_moved_classes(
        comparison,
        base,
        declared,
        &hints.extension_moves,
    );
    let comparison = crate::extensions::forward_moved_operators(
        comparison,
        base,
        declared,
        &hints.extension_moves,
    );
    let base = Side {
        schema: &comparison,
        ids: base.ids,
    };
    // The domains rebuilt under their uid: for a changed base or collation, as
    // the replacement of the type they are built on follows, or for an array
    // that blocks an in-place check (DEC-1635.9). They get no alteration.
    let rebuilt_domains =
        crate::domain_rebuild::plan(base, declared, hints, &label_changes, dialect);
    errs.extend(rebuilt_domains.errors);
    changes.extend(label_changes);
    changes.extend(rebuilt_domains.changes);
    changes.extend(crate::extensions::creates(
        base,
        declared,
        dialect.name(),
        &hints.extension_requires,
    ));
    let (extension_drops, extension_drop_errors) =
        crate::extensions::drops(base, declared, dialect.name(), &hints.extension_drops);
    changes.extend(extension_drops);
    errs.extend(extension_drop_errors);
    let (extension_updates, extension_update_errors) =
        crate::extensions::updates(base, declared, dialect.name(), &hints.extension_updates);
    changes.extend(extension_updates);
    errs.extend(extension_update_errors);
    // After the updates: a move of a package that is also updated starts from
    // the version the update leaves, and the plan's own order keeps them so
    // (DEC-2447.11).
    let (extension_moves, extension_move_errors) =
        crate::extensions::moves(base, declared, dialect.name(), &hints.extension_moves);
    changes.extend(extension_moves);
    errs.extend(extension_move_errors);
    changes.extend(diff_search(base, declared, hints, &mut errs));

    // Definitions match by recorded identity. A rename is never inferred
    // from equal definitions or a pair of disappearing/appearing names.
    let types = TypeResolver::new(base.ids, declared.ids);
    let unrecorded_base = Schema {
        types: base
            .schema
            .types
            .iter()
            .filter(|(name, _)| {
                matches!(
                    types.resolve(TypeSide::Base, name),
                    TypeIdentity::ExternalOrUnrecorded(_)
                ) && matches!(
                    types.resolve(TypeSide::Desired, name),
                    TypeIdentity::ExternalOrUnrecorded(_)
                )
            })
            .map(|(n, d)| (n.clone(), d.clone()))
            .collect(),
        ..Default::default()
    };
    errs.extend(type_differences(
        &unrecorded_base,
        declared.schema,
        dialect,
        false,
    ));
    let mut type_renames = BTreeMap::new();
    for (uid, old_name) in &base.ids.types {
        let Some(old) = base.schema.types.get(old_name) else {
            continue;
        };
        match declared
            .ids
            .types
            .get(uid)
            .and_then(|name| declared.schema.types.get(name).map(|ty| (name, ty)))
        {
            None => {
                let used = types.has_retained_dependent(base.schema, declared.schema, uid);
                if used {
                    errs.push(DiffError::TypeLifecycleUnsupported { name: old_name.clone(), reason: "a retained declaration still uses this type; drop or change its dependents first".into() });
                }
                changes.push(Change::Type(TypeChange::DropType {
                    uid: uid.clone(),
                    name: old_name.clone(),
                    definition: Box::new(old.clone()),
                }));
            }
            Some((name, new)) => {
                let mut comparable = old.clone();
                if let pbps_model::ManagedType::Domain { base: ty, .. } = &mut comparable {
                    types.remap(ty, TypeSide::Base, TypeSide::Desired);
                }
                let old_schema = Schema {
                    types: [(name.clone(), comparable)].into(),
                    ..Default::default()
                };
                let new_schema = Schema {
                    types: [(name.clone(), new.clone())].into(),
                    ..Default::default()
                };
                let mut refused = Vec::new();
                // An enum's labels and a domain's every attribute have their
                // own changes; only a change of kind is refused here
                // (DEC-1635.6, DEC-1635.8, DEC-1635.9).
                if !matches!(
                    (&old_schema.types[name], new),
                    (
                        pbps_model::ManagedType::Enum { .. },
                        pbps_model::ManagedType::Enum { .. }
                    ) | (
                        pbps_model::ManagedType::Domain { .. },
                        pbps_model::ManagedType::Domain { .. }
                    )
                ) {
                    refused = type_differences(&old_schema, &new_schema, dialect, true);
                }
                // A retained domain changes its checks, default and NOT NULL
                // in place: what it drops before the plan's rows, and what it
                // adds after them. A base or collation change was refused
                // above and plans nothing (DEC-1635.8).
                let comparable = &old_schema.types[name];
                let alters = refused.is_empty()
                    && !rebuilt_domains.uids.contains_key(uid)
                    && matches!(
                        (comparable, new),
                        (
                            pbps_model::ManagedType::Domain { .. },
                            pbps_model::ManagedType::Domain { .. }
                        )
                    );
                if alters && let Some(delta) = pbps_model::DomainDelta::of(comparable, new) {
                    for (half, wanted) in [
                        (pbps_model::DomainHalf::Loosen, delta.loosens()),
                        (pbps_model::DomainHalf::Tighten, delta.tightens()),
                    ] {
                        if wanted {
                            changes.push(Change::TypeValue(TypeValueChange::AlterDomain {
                                uid: uid.clone(),
                                name: name.clone(),
                                from: Box::new(comparable.clone()),
                                to: Box::new(new.clone()),
                                half,
                            }));
                        }
                    }
                }
                errs.extend(refused);
                if old_name != name {
                    type_renames.insert(old_name.clone(), name.clone());
                    changes.push(Change::Type(TypeChange::RenameType {
                        uid: uid.clone(),
                        from: old_name.clone(),
                        to: name.clone(),
                        // What stands when the rename runs: an enum's labels
                        // and a domain the plan alters are the old ones, the
                        // domain under the spellings of the types it is over
                        // that the plan leaves.
                        definition: Box::new(match (old, alters) {
                            (pbps_model::ManagedType::Enum { .. }, _) => old.clone(),
                            (pbps_model::ManagedType::Domain { .. }, true) => comparable.clone(),
                            (pbps_model::ManagedType::Domain { .. }, false) => new.clone(),
                        }),
                    }));
                }
            }
        }
    }
    for (uid, name) in &declared.ids.types {
        if base
            .ids
            .types
            .get(uid)
            .and_then(|old| base.schema.types.get(old))
            .is_none()
            && let Some(definition) = declared.schema.types.get(name)
        {
            changes.push(Change::Type(TypeChange::CreateType {
                uid: uid.clone(),
                name: name.clone(),
                definition: Box::new(definition.clone()),
            }));
        }
    }
    for start in type_renames.keys() {
        let mut seen = BTreeSet::new();
        let mut name = start;
        while let Some(next) = type_renames.get(name) {
            if !seen.insert(name) {
                errs.push(DiffError::TypeLifecycleUnsupported {
                    name: start.clone(),
                    reason: "type rename cycle (swap) is refused; use separate plans".into(),
                });
                break;
            }
            name = next;
        }
    }

    // A retained uid is still a managed claim after a manual drop and a
    // baseline. It cannot become an unheld identity in an empty apply.
    for (uid, name) in &declared.ids.types {
        if !declared.schema.types.contains_key(name)
            && types
                .name(TypeSide::Base, uid)
                .and_then(|old| base.schema.types.get(old))
                .is_none()
        {
            errs.push(DiffError::TypeLifecycleUnsupported {
                name: name.clone(),
                reason: "its identity remains recorded but its declaration is absent".into(),
            });
        }
    }

    let base_tables = &base.ids.tables;
    let declared_tables = &declared.ids.tables;
    let index = Indexes {
        base: base.ids.index(),
        declared: declared.ids.index(),
    };

    // One UID-derived address/template map, shared by every moving boundary.
    // Pending is a pure comparison; diff_columns still runs once in its usual place.
    let parent_shapes: BTreeMap<_, _> = base_tables
        .iter()
        .filter_map(|(uid, old_name)| {
            if !dialect.partition_meetings() {
                return None;
            }
            let new_name = declared_tables.get(uid)?;
            let old = base.schema.tables.get(old_name)?;
            let new = declared.schema.tables.get(new_name)?;
            let pending = old.partition_by.is_some()
                && old.partition_of.is_none()
                && new.partition_by.is_some()
                && new.partition_of.is_none()
                && (old_name != new_name || parent_shape_changes(old, new));
            Some((uid, (old_name, new_name, new, pending)))
        })
        .collect();
    let final_parent = |name: &TableName| {
        base.ids
            .table_uid(name)
            .and_then(|uid| parent_shapes.get(uid))
            .map_or_else(|| name.clone(), |(_, name, _, _)| (*name).clone())
    };

    // Table present only in the base: dropped. Its foreign keys are dropped
    // first as separate changes — two dropped tables that reference each other
    // would otherwise fail or succeed depending on which DROP TABLE runs first.
    for (uid, name) in base_tables {
        if !declared_tables.contains_key(uid) {
            if let Some(t) = base.schema.tables.get(name) {
                for fk_name in t.foreign_keys.keys() {
                    changes.push(Change::Constraint(ConstraintChange::DropForeignKey {
                        table: name.clone(),
                        name: fk_name.clone(),
                    }));
                }
            }
            changes.push(Change::Table(TableChange::DropTable {
                uid: uid.clone(),
                name: name.clone(),
                // A partition is detached from its parent before it goes
                // (#1171, DEC-1171.1).
                detach_from: base
                    .schema
                    .tables
                    .get(name)
                    .and_then(|t| t.partition_of.as_ref())
                    .map(|of| final_parent(&of.parent)),
            }));
        }
    }

    // Table present only in the declarations: created. Foreign keys are split
    // out of the CREATE into their own changes, because they sort after every
    // CreateTable — a new table's FK may reference another new table, and
    // within one ordering class the order of creation is not meaningful.
    for (uid, name) in declared_tables {
        if !base_tables.contains_key(uid)
            && let Some(t) = declared.schema.tables.get(name)
        {
            let mut table = t.clone();
            for (fk_name, fk) in std::mem::take(&mut table.foreign_keys) {
                changes.push(Change::Constraint(ConstraintChange::AddForeignKey {
                    table: name.clone(),
                    name: fk_name,
                    constraint: Box::new(fk),
                }));
            }
            // Rows are split out of the CREATE for the same reason the
            // foreign keys above are: they are separate statements in a
            // separate ordering class, and a `CreateTable` carrying rows would
            // give the emitter a second place to produce DML.
            match (&table.data, table.primary_key.as_ref()) {
                (Some(data), Some(pk)) if pk.columns.len() == 1 => {
                    let identity_key = table.columns.get(&pk.columns[0]).is_some_and(|c| match &c
                        .value_source
                    {
                        ValueSource::Identity(_) => true,
                        // A `serial` key is refused by its dialect (#1631).
                        ValueSource::None
                        | ValueSource::Default(_)
                        | ValueSource::Sequence(_)
                        | ValueSource::Generated(_) => false,
                    });
                    for (key, row) in &data.rows {
                        let (defaults, types) = omitted_defaults(&table, pk.columns[0].raw(), row);
                        changes.push(Change::Row(RowChange::InsertRow {
                            table: name.clone(),
                            key_column: pk.columns[0].clone(),
                            identity_key,
                            key: key.clone(),
                            row: row.clone(),
                            defaults,
                            types,
                        }));
                    }
                }
                (Some(_), _) => errs.push(DiffError::DataWithoutKey {
                    table: name.clone(),
                }),
                (None, _) => {}
            }
            let beside_default = table
                .partition_of
                .as_ref()
                .and_then(|of| standing_default(base, declared, of));
            // What a human allowed it to take (#1788): read from the
            // declared side, where `pbps move-rows` writes it.
            let moves_at_most = beside_default
                .as_ref()
                .and_then(|_| declared.ids.moves.get(uid))
                .map(|m| m.max_rows);
            for (policy_name, policy) in &table.row_policies {
                changes.push(Change::Policy(PolicyChange::CreatePolicy {
                    table: name.clone(),
                    name: policy_name.clone(),
                    policy: policy.clone(),
                    origin: pbps_model::RowSecurityOrigin::Created,
                }));
            }
            if !table.row_security.is_default() {
                changes.push(Change::Policy(PolicyChange::SetRowSecurity {
                    uid: uid.clone(),
                    table: name.clone(),
                    from: Default::default(),
                    to: table.row_security,
                    origin: pbps_model::RowSecurityOrigin::Created,
                }));
            }
            // An exclusion is added by its own change, after the table's rows
            // (DEC-2448.3): the creation payload never carries one, so no
            // emitter can create the table and lose it.
            let exclusions = std::mem::take(&mut table.exclude);
            changes.push(Change::Table(TableChange::CreateTable {
                uid: uid.clone(),
                name: name.clone(),
                table: Box::new(table),
                beside_default,
                moves_at_most,
            }));
            for (exclusion, constraint) in exclusions {
                changes.push(Change::Constraint(ConstraintChange::SetExclusion {
                    table: name.clone(),
                    name: exclusion,
                    from: None,
                    to: Some(Box::new(constraint)),
                }));
            }
        }
    }

    // Table present on both sides: possibly renamed, and its contents must be
    // compared.
    let renames = renames_of(base, declared);
    let mut standing: BTreeMap<TableName, Table> = BTreeMap::new();
    for (name, table) in &declared.schema.tables {
        if table.unlogged {
            continue;
        }
        for (key, fk) in &table.foreign_keys {
            if declared
                .schema
                .tables
                .get(&fk.references_table)
                .is_some_and(|t| t.unlogged)
            {
                errs.push(DiffError::PermanentReferencesUnlogged {
                    table: name.clone(),
                    key: key.clone(),
                    target: fk.references_table.clone(),
                });
            }
            // Through a partitioned table, which the engine lets pass: the
            // key reaches each of its partitions (#1580).
            for (partition, _) in declared.schema.tables.iter().filter(|(_, t)| {
                t.unlogged
                    && t.partition_of
                        .as_ref()
                        .is_some_and(|of| of.parent == fk.references_table)
            }) {
                errs.push(DiffError::PermanentReferencesUnloggedPartition {
                    table: name.clone(),
                    key: key.clone(),
                    target: fk.references_table.clone(),
                    partition: partition.clone(),
                });
            }
        }
    }
    for (uid, declared_name) in declared_tables {
        let Some(base_name) = base_tables.get(uid) else {
            continue;
        };
        if base_name != declared_name {
            let defaults = base
                .schema
                .tables
                .get(base_name)
                .map(|t| {
                    t.columns
                        .iter()
                        .filter(|(_, c)| match &c.value_source {
                            ValueSource::Default(_) => true,
                            ValueSource::None
                            | ValueSource::Identity(_)
                            | ValueSource::Sequence(_)
                            | ValueSource::Generated(_) => false,
                        })
                        .map(|(name, _)| name.clone())
                        .collect()
                })
                .unwrap_or_default();
            changes.push(Change::Table(TableChange::RenameTable {
                uid: uid.clone(),
                from: base_name.clone(),
                to: declared_name.clone(),
                defaults,
            }));
        }
        let (Some(base_table), Some(declared_table)) = (
            base.schema.tables.get(base_name),
            declared.schema.tables.get(declared_name),
        ) else {
            continue;
        };
        // Only a retained identity needs explicit policy teardown. Dropping
        // the owner removes its policies, while a replacement UID is created
        // through the ordinary new-table path (DEC-1173.2).
        for name in base_table
            .row_policies
            .keys()
            .chain(declared_table.row_policies.keys())
            .collect::<BTreeSet<_>>()
        {
            let before = base_table.row_policies.get(name);
            let after = declared_table.row_policies.get(name);
            if before == after {
                continue;
            }
            if let Some(policy) = before {
                changes.push(Change::Policy(PolicyChange::DropPolicy {
                    table: base_name.clone(),
                    name: name.clone(),
                    policy: policy.clone(),
                }));
            }
            if let Some(policy) = after {
                changes.push(Change::Policy(PolicyChange::CreatePolicy {
                    table: declared_name.clone(),
                    name: name.clone(),
                    policy: policy.clone(),
                    origin: pbps_model::RowSecurityOrigin::Standing,
                }));
            }
        }
        if base_table.row_security != declared_table.row_security {
            changes.push(Change::Policy(PolicyChange::SetRowSecurity {
                uid: uid.clone(),
                table: declared_name.clone(),
                from: base_table.row_security,
                to: declared_table.row_security,
                origin: pbps_model::RowSecurityOrigin::Standing,
            }));
        }
        // A partition declared as an ordinary table is detached, and that is
        // all this plan does to it (#1544, DEC-1544.1): its base holds no
        // columns of its own, so a column-by-column diff would read every one
        // as new.
        if let Some(of) = &base_table.partition_of
            && declared_table.partition_of.is_none()
            && declared_table.partition_by.is_none()
        {
            match base
                .schema
                .tables
                .get(&of.parent)
                .ok_or_else(|| {
                    vec![format!(
                        "its parent {} is not in the base",
                        of.parent.shown()
                    )]
                })
                // The parent as this plan leaves it: a foreign key to a
                // table renamed in the same plan is declared under the new
                // name, as `diff_constraints` reads it below. And the
                // partition as well, whose own foreign keys are matched by
                // name and definition (#1632).
                .and_then(|parent| {
                    detached_names(
                        &renames.apply(parent, &of.parent),
                        &renames.apply(base_table, base_name),
                        declared_table,
                        dialect,
                        base.ids
                            .table_uid(&of.parent)
                            .and_then(|uid| parent_shapes.get(uid))
                            .is_some_and(|(_, _, _, pending)| *pending),
                    )
                }) {
                Ok(names) => changes.push(Change::Partition(PartitionChange::DetachPartition {
                    uid: uid.clone(),
                    table: declared_name.clone(),
                    parent: final_parent(&of.parent),
                    names,
                    shape: Box::new(declared_table.clone()),
                })),
                Err(what) => errs.push(DiffError::DetachedShape {
                    table: declared_name.clone(),
                    parent: of.parent.clone(),
                    what,
                }),
            }
            continue;
        }

        // An ordinary table declared as a partition is attached, and keeps
        // its rows (#1545, DEC-1545.1). Its columns become its parent's, so
        // they are not compared; what it holds of its own once attached is
        // compared with the declaration as a standing partition's is, and
        // brought to it by the same changes after the attach.
        if base_table.partition_of.is_none()
            && let Some(of) = &declared_table.partition_of
        {
            match attached(
                base,
                base_name,
                declared_name,
                base_table,
                declared_table,
                of,
                dialect,
                read_back,
                declared
                    .ids
                    .table_uid(&of.parent)
                    .and_then(|uid| parent_shapes.get(uid))
                    .map(|(old, _, goal, pending)| (*old, *pending, *goal)),
            ) {
                Ok(Attached {
                    table,
                    defaultless,
                    displaced,
                    released,
                }) => {
                    changes.extend(released);
                    // Gone before the attach, so the engine adopts the one
                    // index left for each of its parent's or builds it; one
                    // the declaration keeps as its own is added after.
                    for name in displaced {
                        changes.push(Change::Index(IndexChange::DropIndex {
                            table: declared_name.clone(),
                            name,
                        }));
                    }
                    changes.push(Change::Partition(PartitionChange::AttachPartition {
                        uid: uid.clone(),
                        table: declared_name.clone(),
                        parent: of.parent.clone(),
                        bound: of.bound.clone(),
                        key: dialect.attach_scan_key(
                            uid,
                            &declared
                                .schema
                                .tables
                                .get(&of.parent)
                                .and_then(|parent| {
                                    let by = parent.partition_by.as_ref()?;
                                    by.columns
                                        .iter()
                                        .map(|c| {
                                            Some((c.clone(), parent.columns.get(c)?.ty.clone()))
                                        })
                                        .collect::<Option<Vec<_>>>()
                                })
                                .unwrap_or_default(),
                            &declared.schema.types,
                        ),
                        shape: Box::new(declared_table.clone()),
                    }));
                    // Compared as this plan leaves them: an own foreign key
                    // to a table renamed in the same plan is declared under
                    // the new name (#1632). Matched to the parent's above on
                    // the base side, where the engine matches by relation.
                    diff_constraints(
                        declared_name,
                        &renames.apply(&table, base_name),
                        declared_table,
                        &mut changes,
                        &mut errs,
                    );
                    diff_storage_parameters(
                        uid,
                        declared_name,
                        &table,
                        declared_table,
                        &mut changes,
                    );
                    if table.unlogged != declared_table.unlogged {
                        changes.push(Change::TableProperty(
                            TablePropertyChange::SetTablePersistence {
                                uid: uid.clone(),
                                table: declared_name.clone(),
                                unlogged: declared_table.unlogged,
                            },
                        ));
                    }
                    diff_partition_columns(
                        uid,
                        declared_name,
                        &table,
                        declared_table,
                        base,
                        declared,
                        &index,
                        dialect,
                        &mut changes,
                        &mut errs,
                    );
                    // A column without a default where its parent's has one:
                    // the attach gives it none, which no declaration can say
                    // and the reader does not hold, so it takes its parent's
                    // unless it declares its own, which the comparison above
                    // sets (measured on 16 and 18).
                    let parent = declared.schema.tables.get(&of.parent);
                    for column in defaultless {
                        let own = of.columns.get(&column).and_then(|c| c.default.as_ref());
                        let fallback = partition_default(
                            dialect,
                            &of.parent,
                            &column,
                            parent.and_then(|p| p.columns.get(&column)),
                            base.schema
                                .tables
                                .get(&of.parent)
                                .and_then(|p| p.columns.get(&column)),
                            &mut errs,
                        );
                        if let (None, Some(fallback)) = (own, fallback) {
                            changes.push(Change::Partition(PartitionChange::SetPartitionDefault {
                                uid: uid.clone(),
                                table: declared_name.clone(),
                                parent: of.parent.clone(),
                                column,
                                from: None,
                                to: None,
                                fallback: Some(fallback),
                            }));
                        }
                    }
                }
                Err(what) => errs.push(DiffError::AttachedShape {
                    table: declared_name.clone(),
                    parent: of.parent.clone(),
                    what,
                }),
            }
            continue;
        }

        diff_columns(
            &index,
            &types,
            base_name,
            declared_name,
            base_table,
            declared_table,
            dialect,
            &declared.ids.conversions,
            &mut changes,
            &mut errs,
        );
        // Asked of the whole change list below, not of this loop's own: an enum
        // or domain rebuild retypes a column before this loop runs.
        if !base_table.exclude.is_empty() {
            standing.insert(
                declared_name.clone(),
                renames.apply(base_table, base_name).into_owned(),
            );
        }
        diff_filegroups(declared_name, base_table, declared_table, &mut errs);
        diff_constraints(
            declared_name,
            &renames.apply(base_table, base_name),
            declared_table,
            &mut changes,
            &mut errs,
        );
        // A retirement holds the definition as the baseline wrote it, and the
        // table compared below has had this plan's column renames applied, so
        // authority is asked of the original: a plan that renames a key column
        // and drops the constraint is a valid one, and no rerun of `pbps
        // drop-exclusion` could record the renamed text.
        let retired = |key: &pbps_model::ObjectLocalName| {
            base_table
                .exclude
                .get(key)
                .is_some_and(|held| declared.ids.exclusion_retired(uid, key, held))
        };
        diff_exclusions(
            declared_name,
            &renames.apply(base_table, base_name),
            declared_table,
            &retired,
            &mut changes,
            &mut errs,
        );
        diff_computed(declared_name, base_table, declared_table, &mut changes);
        diff_storage_parameters(uid, declared_name, base_table, declared_table, &mut changes);
        if base_table.unlogged != declared_table.unlogged {
            changes.push(Change::TableProperty(
                TablePropertyChange::SetTablePersistence {
                    uid: uid.clone(),
                    table: declared_name.clone(),
                    unlogged: declared_table.unlogged,
                },
            ));
        }
        diff_partition_columns(
            uid,
            declared_name,
            base_table,
            declared_table,
            base,
            declared,
            &index,
            dialect,
            &mut changes,
            &mut errs,
        );
        // Rows are compared by column *name* on each side, and a rename in
        // this same plan means the two sides know one column by two names.
        // The uid is what says they are the same column.
        let base_cols = columns_of(&index.base, base_name);
        let base_name_of: BTreeMap<ColumnName, ColumnName> =
            columns_of(&index.declared, declared_name)
                .iter()
                .filter_map(|(uid, d)| base_cols.get(uid).map(|b| (d.name.clone(), b.name.clone())))
                .collect();
        // The observed rows as this plan's label renames leave them: the
        // engine renames a label in the rows that hold it (DEC-2346.2).
        let forwarded = crate::enum_labels::forwarded_rows(
            base_table,
            (&TypeResolver::new(base.ids, declared.ids), TypeSide::Base),
            &crate::enum_labels::label_renames(&changes),
        );
        diff_data(
            declared_name,
            forwarded.as_ref().unwrap_or(base_table),
            declared_table,
            &base_name_of,
            &mut changes,
            &mut errs,
        );
    }

    split_tightenings_from_retypes(&mut changes);
    clear_external_partitions_after_loosening(declared.schema, &mut changes);
    crate::enum_labels::rebuild_defaults(base, declared, &mut changes);
    recreate_retyped_dependents(base, declared, &renames, dialect, &mut changes);
    recreate_referenced_foreign_keys(base, declared, &renames, &mut changes);
    rebind_foreign_keys_to_a_new_occupant(base, declared, &mut changes);
    unlink_cycles_around_persistence_switches(declared.schema, &mut changes);
    // After every pass that drops and re-adds an index: each re-add of the
    // identity's index loses the identity, and is followed by it again.
    let early_identity = diff_replica_identity(
        base.schema,
        declared.schema,
        declared_tables,
        base_tables,
        &mut changes,
    );

    diff_modules(
        base.schema,
        declared.schema,
        &names_changing_hands(base, declared),
        dialect,
        &mut changes,
    );
    refuse_computed_dependencies(
        base.schema,
        declared.schema,
        dialect,
        screen,
        &changes,
        &mut errs,
    );
    refuse_temporal_changes(base, declared, &changes, &mut errs);
    refuse_unjoinable_foreign_keys(declared.schema, dialect, &changes, &mut errs);
    refuse_rows_referencing_late_inserts(declared, &changes, &mut errs);
    refuse_partition_changes(base, declared, &index, hints, dialect, &changes, &mut errs);
    // A module declaration can stay byte-for-byte identical while a new
    // overload or shadow changes what it should bind to. Ask the dialect
    // before sorting, rather than appending unreviewed SQL at apply time.
    let arriving: Vec<ModuleId> = changes
        .iter()
        .filter_map(|change| match change {
            Change::Partition(PartitionChange::RebuildPartitionedTable(_))
            | Change::Index(IndexChange::BuildPartitionedIndex(_))
            | Change::Partition(PartitionChange::CopyTableRows(_))
            | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => None,
            Change::Module(ModuleChange::CreateModule { id, .. }) => Some(id.clone()),
            Change::Table(TableChange::CreateTable { name, .. }) => {
                Some(ModuleId::Named(name.clone()))
            }
            Change::Table(TableChange::RenameTable { to, .. }) => Some(ModuleId::Named(to.clone())),
            Change::Table(TableChange::DropTable { .. })
            | Change::Partition(PartitionChange::DetachPartition { .. })
            | Change::Partition(PartitionChange::AttachPartition { .. })
            | Change::Column(_)
            | Change::TableProperty(_)
            | Change::Constraint(_)
            | Change::Index(IndexChange::SetIndexStorageParameters { .. })
            | Change::Partition(PartitionChange::SetPartitionDefault { .. })
            | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
            | Change::Policy(_)
            | Change::Index(IndexChange::AddIndex { .. })
            | Change::Index(IndexChange::DropIndex { .. })
            | Change::Row(_)
            | Change::Module(ModuleChange::AlterModule { .. })
            | Change::Module(ModuleChange::DropModule { .. })
            | Change::TypeValue(_)
            | Change::Role(_)
            | Change::Type(_)
            | Change::Search(_)
            | Change::Extension(_)
            | Change::Permission(_) => None,
        })
        .collect();
    let changed: BTreeSet<ModuleId> = changes
        .iter()
        .filter_map(|change| match change {
            Change::Partition(PartitionChange::RebuildPartitionedTable(_))
            | Change::Index(IndexChange::BuildPartitionedIndex(_))
            | Change::Partition(PartitionChange::CopyTableRows(_))
            | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => None,
            Change::Module(ModuleChange::CreateModule { id, .. })
            | Change::Module(ModuleChange::AlterModule { id, .. })
            | Change::Module(ModuleChange::DropModule { id, .. }) => Some(id.clone()),
            Change::Table(_)
            | Change::Partition(PartitionChange::DetachPartition { .. })
            | Change::Partition(PartitionChange::AttachPartition { .. })
            | Change::Column(_)
            | Change::TableProperty(_)
            | Change::Constraint(_)
            | Change::Index(IndexChange::SetIndexStorageParameters { .. })
            | Change::Partition(PartitionChange::SetPartitionDefault { .. })
            | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
            | Change::Policy(_)
            | Change::Index(IndexChange::AddIndex { .. })
            | Change::Index(IndexChange::DropIndex { .. })
            | Change::Row(_)
            | Change::TypeValue(_)
            | Change::Role(_)
            | Change::Type(_)
            | Change::Search(_)
            | Change::Extension(_)
            | Change::Permission(_) => None,
        })
        .collect();
    // One set, so a module both the dialect and the caller ask for is rebuilt
    // once.
    let candidates = match rebinding {
        Rebinding::Candidates => dialect.rebound_modules(declared.schema, &arriving, &changed),
        Rebinding::Evidence => BTreeSet::new(),
    };
    let rebound: BTreeSet<ModuleId> = candidates
        .into_iter()
        .chain(also.iter().cloned())
        .chain(hints.enum_dependents.iter().cloned())
        .collect();
    for id in rebound {
        if !changed.contains(&id)
            && base.schema.modules.contains_key(&id)
            && let Some(module) = declared.schema.modules.get(&id)
        {
            changes.push(Change::Module(ModuleChange::AlterModule {
                id,
                module: Box::new(module.clone()),
            }));
        }
    }
    // A module over a rebuilt enum's column cannot be restated in place after
    // the retype: it comes down in class 0 and goes back in class 14, around
    // the retypes and the swap (DEC-1635.7).
    if !hints.enum_dependents.is_empty() {
        let mut split = Vec::new();
        for change in std::mem::take(&mut changes) {
            if let Change::Module(ModuleChange::AlterModule { id, module }) = &change
                && hints.enum_dependents.contains(id)
            {
                split.push(Change::Module(ModuleChange::DropModule {
                    id: id.clone(),
                    kind: module.kind,
                }));
                split.push(Change::Module(ModuleChange::CreateModule {
                    id: id.clone(),
                    module: module.clone(),
                }));
            } else {
                split.push(change);
            }
        }
        changes = split;
    }
    // A module the engine binds to what this plan changes cannot stand beside
    // the change, whether the declarations edited it or not (SQL Server's
    // SCHEMABINDING, DEC-2329.1). It goes as a drop and comes back as a
    // create: the classes put the first before the table changes and the
    // second after them, as they do for any module. Before `diff_roles`, so
    // the grants a rebuilt module loses are written again.
    for id in dialect.schema_bound_teardown(base.schema, declared.schema, &changes) {
        let (Some(before), Some(module)) = (
            base.schema.modules.get(&id),
            declared.schema.modules.get(&id),
        ) else {
            continue;
        };
        if changes.iter().any(|c| {
            matches!(c, Change::Module(ModuleChange::DropModule { id: other, .. }) | Change::Module(ModuleChange::CreateModule { id: other, .. }) if *other == id)
        }) {
            continue;
        }
        changes.retain(|c| !matches!(c, Change::Module(ModuleChange::AlterModule { id: other, .. }) if *other == id));
        changes.push(Change::Module(ModuleChange::DropModule {
            id: id.clone(),
            kind: before.kind,
        }));
        changes.push(Change::Module(ModuleChange::CreateModule {
            id,
            module: Box::new(module.clone()),
        }));
    }
    diff_roles(base, declared, dialect, &mut changes);
    revoke_public_execution(base.schema, dialect, hints, &mut changes);

    // The ordering and risk pass below runs whether or not there are errors:
    // it is pure computation over the changes already built, and a caller that
    // is going to *report* the partial set needs it sorted and classified
    // exactly as a plan would be.
    // After every pass that rebuilds an index: its `CREATE` carries the
    // declared parameters (#1483 review).
    // Every change is in: whichever pass produced a retype or a drop of a held
    // column is seen, not only the table loop's.
    for (table, held) in &standing {
        let stated = exclusion_hazards(
            table,
            held,
            declared.schema.tables.get(table),
            changes.iter().filter(|c| c.table() == Some(table)),
            dialect,
            &mut errs,
        );
        changes.extend(stated);
    }
    // Descriptions are comments only where the engine holds them; elsewhere they stay the
    // free text they were (#2729).
    if dialect.holds_descriptions() {
        let comments = crate::comments::changes(base, declared, &changes);
        changes.extend(comments);
    }
    crate::order::drop_parameter_changes_of_rebuilt_indexes(&mut changes, |c| c);
    let mut planned: Vec<PlannedChange> = changes.into_iter().map(PlannedChange::new).collect();
    for p in &mut planned {
        // Attached here, not looked up at emit time: the plan file is the
        // artifact the deployment gate reviews, and a hint resolved later
        // against a YAML file the deployment host may not have is a hint
        // nobody read (ADR-0003).
        if let Some(strategy) = p.change.table().and_then(|t| hints.strategies.get(t)) {
            p.strategy = *strategy;
        }
    }

    crate::row_defaults::catalog(&mut planned, base, declared, &hints.row_guard_defaults);

    // Modules are ordered among themselves by dependency: a view over a view
    // has to be created second, and dropped first (ADR-0002).
    // Lexed by the dialect: where a literal ends is the engine's rule, and an
    // edge read out of a literal the engine had not closed put a view before
    // the one it selects from (DECISIONS 315).
    let lex = |definition: &str| dialect.code_only(definition);
    let rank = |from: &str, to: &str| dialect.bare_name_rank(from, to);
    let lexis = pbps_model::module::Lexis {
        code_only: &lex,
        quoted_identifiers: dialect.lexicon().quoted_identifiers,
        continues_ident: dialect.lexicon().identifier_continues,
        reserved: dialect.lexicon().reserved,
        bare_rank: &rank,
    };
    let create_rank = rank_of(&pbps_model::module::creation_order_with(
        &declared.schema.modules,
        &module_deps,
        &lexis,
    ));
    let drop_rank = rank_of(&pbps_model::module::creation_order_with(
        &base.schema.modules,
        &module_deps,
        &lexis,
    ));
    // The tiebreaker within an ordering class is the table name, then the
    // change's rendering. Debug output alone would sort by uid, which is random
    // at mint time — the plan would be correct but differently ordered per
    // project, and a reviewer diffing two plan.sql files would see noise.
    //
    // `subject()` does not speak for one side consistently: `RenameTable` and
    // `RenameRole` answer with the name the object is losing, everything else
    // with the name it will have. Two changes to one object are therefore
    // sorted against two different spellings of it, and the alphabet — not the
    // dependency — decides which wins. Anything with a real order between them
    // belongs in ordering classes or explicit dependency edges; this
    // tiebreaker cannot express it.
    // Rows follow the foreign keys among the tables that declare them. The
    // declared side is the right one to read: a row being inserted is going
    // into the schema as it will be, not as it was.
    //
    // An attach brings rows into its parent and validates the parent's
    // foreign keys over them, so the parents this plan attaches to are
    // ordered with the tables that receive rows: a parent after every table
    // it references, whether rows or another attach fill that one (#1642
    // review).
    let attach_parents: BTreeMap<TableName, TableName> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Partition(PartitionChange::AttachPartition { table, parent, .. }) =
                &p.change
            {
                Some((table.clone(), parent.clone()))
            } else {
                None
            }
        })
        .collect();
    let data_rank = rank_of_tables(&if attach_parents.is_empty() {
        pbps_model::data::insertion_order(declared.schema)
    } else {
        pbps_model::data::supply_order(
            declared.schema,
            declared
                .schema
                .tables
                .iter()
                .filter(|(_, t)| t.data.is_some())
                .map(|(n, _)| n.clone())
                .chain(attach_parents.values().cloned())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        )
    });
    // Roles that are dropped together are ordered parent before member.
    let role_rank = member_depth(&planned);
    // The tables a `RenameColumn` in this plan claims a column name on. Both
    // this and the drops below carry `declared_table_name` (`diff_columns`),
    // so the two spellings meet.
    //
    // The *table*, and not the name, because the name is a question this
    // cannot answer. On SQL Server what makes two spellings one column name is
    // the database's collation, which a plan computed offline does not have
    // (SPEC 7.3) — and `Mssql::fold_ident` returns the identity for exactly
    // that reason. **Measured** on the pinned image, each in a database of the
    // named collation:
    //
    //     SQL_Latin1_General_CP1_CI_AS   note  vs Note  ->  one name, Msg 15335
    //     SQL_Latin1_General_CP1_CI_AI   café  vs cafe  ->  one name, Msg 15335
    //     Latin1_General_CS_AS           note  vs Note  ->  two names, both kept
    //
    // Case, then accents; width and kana sensitivity are two more flags on the
    // same collation name, and a binary collation is another answer again.
    // Folding the name here would be guessing at which of those the target
    // database chose, and each guess that comes up short refuses a valid plan
    // at the engine — the defect this is here to fix, one collation further
    // out. So the question asked is the one that is true under every
    // collation: a rename can only collide with a column of its own table.
    //
    // The cost is that a drop of a column nothing claims moves too, and
    // nothing between its old class and its new one can notice: a `Revoke`
    // names no column, and a column this plan drops is never a rename's
    // source, since the two come from different uids and no two baseline
    // columns share a name.
    let tables_claiming_a_column_name: BTreeSet<TableName> = planned
        .iter()
        .filter_map(|p| match &p.change {
            Change::Partition(PartitionChange::RebuildPartitionedTable(_))
            | Change::Index(IndexChange::BuildPartitionedIndex(_))
            | Change::Partition(PartitionChange::CopyTableRows(_))
            | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => None,
            Change::Column(ColumnChange::RenameColumn { table, .. }) => Some(table.clone()),
            Change::Table(_)
            | Change::Partition(PartitionChange::DetachPartition { .. })
            | Change::Partition(PartitionChange::AttachPartition { .. })
            | Change::Column(ColumnChange::AddColumn { .. })
            | Change::Column(ColumnChange::DropColumn { .. })
            | Change::Column(ColumnChange::AlterColumnType { .. })
            | Change::Column(ColumnChange::AlterColumnNullability { .. })
            | Change::Column(ColumnChange::AlterColumnDefault { .. })
            | Change::Column(ColumnChange::AlterColumnExpression { .. })
            | Change::Column(ColumnChange::AlterColumnNumbering { .. })
            | Change::Column(ColumnChange::SetColumnDeprecated { .. })
            | Change::TableProperty(_)
            | Change::Constraint(_)
            | Change::Index(IndexChange::SetIndexStorageParameters { .. })
            | Change::Partition(PartitionChange::SetPartitionDefault { .. })
            | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
            | Change::Policy(_)
            | Change::Index(IndexChange::AddIndex { .. })
            | Change::Column(ColumnChange::AddComputedColumn { .. })
            | Change::Column(ColumnChange::DropComputedColumn { .. })
            | Change::Index(IndexChange::DropIndex { .. })
            | Change::Row(_)
            | Change::Module(_)
            | Change::TypeValue(_)
            | Change::Role(_)
            | Change::Type(_)
            | Change::Search(_)
            | Change::Extension(_)
            | Change::Permission(_) => None,
        })
        .collect();
    // Whether this drop can be what frees a name a `RenameColumn` above is
    // waiting for. A column drop and nothing else: no other change gives up a
    // column name.
    let frees_a_renamed_column = |c: &Change| -> bool {
        match c {
            Change::Partition(PartitionChange::RebuildPartitionedTable(_))
            | Change::Index(IndexChange::BuildPartitionedIndex(_))
            | Change::Partition(PartitionChange::CopyTableRows(_))
            | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => false,
            Change::Column(ColumnChange::DropColumn { column, .. }) => {
                tables_claiming_a_column_name.contains(&column.table)
            }
            Change::Table(_)
            | Change::Partition(PartitionChange::DetachPartition { .. })
            | Change::Partition(PartitionChange::AttachPartition { .. })
            | Change::Column(ColumnChange::AddColumn { .. })
            | Change::Column(ColumnChange::RenameColumn { .. })
            | Change::Column(ColumnChange::AlterColumnType { .. })
            | Change::Column(ColumnChange::AlterColumnNullability { .. })
            | Change::Column(ColumnChange::AlterColumnDefault { .. })
            | Change::Column(ColumnChange::AlterColumnExpression { .. })
            | Change::Column(ColumnChange::AlterColumnNumbering { .. })
            | Change::Column(ColumnChange::SetColumnDeprecated { .. })
            | Change::TableProperty(_)
            | Change::Constraint(_)
            | Change::Index(IndexChange::SetIndexStorageParameters { .. })
            | Change::Partition(PartitionChange::SetPartitionDefault { .. })
            | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
            | Change::Policy(_)
            | Change::Index(IndexChange::AddIndex { .. })
            | Change::Column(ColumnChange::AddComputedColumn { .. })
            | Change::Column(ColumnChange::DropComputedColumn { .. })
            | Change::Index(IndexChange::DropIndex { .. })
            | Change::Row(_)
            | Change::Module(_)
            | Change::TypeValue(_)
            | Change::Role(_)
            | Change::Type(_)
            | Change::Search(_)
            | Change::Extension(_)
            | Change::Permission(_) => false,
        }
    };
    // A column rename into a name another rename of the same table vacates
    // runs after it: skipped revisions that rename `b` to `c` and then `a` to
    // `b` leave a chain the uids would otherwise order, and `a -> b` while `b`
    // still stands is refused by both engines (DEC-541.1). Its depth is how many
    // links it waits on; the chain's far end, whose target is free, goes
    // first. A cycle, two columns trading names, has no order an engine takes
    // without a temporary name, so the plan says so instead of emitting one.
    let vacates: BTreeMap<(TableName, ColumnName), ColumnName> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Column(ColumnChange::RenameColumn {
                table, from, to, ..
            }) = &p.change
            {
                Some(((table.clone(), from.clone()), to.clone()))
            } else {
                None
            }
        })
        .collect();
    let mut cycles: BTreeSet<(TableName, BTreeSet<ColumnName>)> = BTreeSet::new();
    for ((table, from), to) in &vacates {
        let mut path = vec![from.clone()];
        let mut next = to.clone();
        while let Some(after) = vacates.get(&(table.clone(), next.clone())) {
            // Only the links that close the loop: a chain can lead into one.
            if let Some(start) = path.iter().position(|seen| *seen == next) {
                cycles.insert((table.clone(), path[start..].iter().cloned().collect()));
                break;
            }
            path.push(next.clone());
            next = after.clone();
        }
    }
    // What each rename waits on: the rename vacating its target. Under a
    // case-insensitive collation `a -> B` also waits on `b -> c`, since the
    // engine reads `B` as held by `b` (DEC-981.3). Such a link is added after
    // every exact one and only where it closes no cycle: on a case-sensitive
    // database `a -> B` beside `b -> A` is a valid pair, not a swap, and a
    // folded link must not hide an exact one it would loop through.
    type Rename = (TableName, ColumnName);
    let mut waits: BTreeMap<&Rename, BTreeSet<&Rename>> = vacates
        .iter()
        .map(|(rename, to)| {
            let exact = vacates.get_key_value(&(rename.0.clone(), to.clone()));
            (rename, exact.map(|(k, _)| k).into_iter().collect())
        })
        .collect();
    fn reaches<'a>(
        waits: &BTreeMap<&'a Rename, BTreeSet<&'a Rename>>,
        from: &'a Rename,
        to: &Rename,
    ) -> bool {
        let mut seen = BTreeSet::new();
        let mut stack = vec![from];
        while let Some(at) = stack.pop() {
            if at == to {
                return true;
            }
            if seen.insert(at) {
                stack.extend(waits.get(at).into_iter().flatten().copied());
            }
        }
        false
    }
    for (rename, to) in &vacates {
        let folded = crate::order::rename_order::case_folded(to.raw());
        for vacating in vacates.keys() {
            if vacating != rename
                && vacating.0 == rename.0
                && vacating.1 != *to
                && crate::order::rename_order::case_folded(vacating.1.raw()) == folded
                && !reaches(&waits, vacating, rename)
            {
                waits
                    .get_mut(rename)
                    .expect("every rename has an entry")
                    .insert(vacating);
            }
        }
    }
    // Its depth is the longest run of renames it waits on; the chain's far
    // end, whose target is free, goes first. A rename on an exact cycle is
    // refused below, so its depth is only cut short, never relied on.
    fn depth<'a>(
        waits: &BTreeMap<&'a Rename, BTreeSet<&'a Rename>>,
        at: &'a Rename,
        open: &mut BTreeSet<&'a Rename>,
        memo: &mut BTreeMap<&'a Rename, usize>,
    ) -> usize {
        if let Some(&d) = memo.get(at) {
            return d;
        }
        if !open.insert(at) {
            return 0;
        }
        let d = waits
            .get(at)
            .into_iter()
            .flatten()
            .map(|&next| 1 + depth(waits, next, open, memo))
            .max()
            .unwrap_or(0);
        open.remove(at);
        memo.insert(at, d);
        d
    }
    let mut memo = BTreeMap::new();
    let chain_depth: BTreeMap<Rename, usize> = vacates
        .keys()
        .map(|rename| {
            (
                rename.clone(),
                depth(&waits, rename, &mut BTreeSet::new(), &mut memo),
            )
        })
        .collect();
    for (table, columns) in cycles {
        errs.push(DiffError::ColumnRenameCycle { table, columns });
    }
    // The class, and a rank inside it, so a change can sit between two
    // classes without a new ordinal shifting every one below it — the cost
    // `order_key`'s own doc names.
    // The base's generated columns, whose drops go first in their class: an
    // input column cannot be dropped while a generated column still reads it
    // (DEC-1168.1). By uid, not by name: a `DropColumn` names the column
    // under the declared table name, which a rename in the same plan makes
    // differ from the base's.
    let generated_in_base: BTreeSet<pbps_model::Uid> = base
        .schema
        .tables
        .iter()
        .flat_map(|(name, table)| {
            table
                .columns
                .iter()
                .filter(|(_, c)| match &c.value_source {
                    ValueSource::Generated(_) => true,
                    ValueSource::None
                    | ValueSource::Default(_)
                    | ValueSource::Identity(_)
                    | ValueSource::Sequence(_) => false,
                })
                .filter_map(|(column, _)| index.base.column_uid(&name.column(column)).cloned())
        })
        .collect();
    let row_tables: BTreeSet<TableName> = planned
        .iter()
        .filter_map(|p| row_table(&p.change).cloned())
        .collect();
    let identified = identities_put_on(planned.iter().map(|p| &p.change));
    let inserted = tables_with_inserts(planned.iter().map(|p| &p.change));
    let late = numbering_after_deletes(planned.iter().map(|p| &p.change));
    let installed = numbering_put_on(planned.iter().map(|p| &p.change));
    // A column this plan tightens or retypes: a conversion to an identity
    // waits for both, since the identity is made `NOT NULL`-only and in the
    // column's type at the time (#1930 review).
    let held_back: BTreeSet<ColumnRef> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Column(ColumnChange::AlterColumnNullability {
                column,
                to_nullable: false,
                ..
            })
            | Change::Column(ColumnChange::AlterColumnType { column, .. }) = &p.change
            {
                Some(column.clone())
            } else {
                None
            }
        })
        .collect();
    // A kept identity on a column this plan narrows: its sequence is
    // retuned before the retype (#2013 review, DEC-1921.1). The engine moves
    // an identity's sequence with its column, and measured on 16 and 18 it
    // refuses the retype while a bound set for the wider type stands. An
    // identity's column is an integer, so a retype that is not safe narrows
    // its range, and bounds declared for the narrower type fit the wider one
    // too; a widening keeps the retype first, where the old bounds fit.
    let bounds_first: BTreeSet<ColumnRef> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Column(ColumnChange::AlterColumnType {
                column, from, to, ..
            }) = &p.change
                && p.change.value_change().map_or_else(
                    || dialect.type_change_risk(from, to),
                    |values| dialect.value_change(&values, "").risk,
                ) != pbps_dialect::TypeChangeRisk::Safe
            {
                Some(column)
            } else {
                None
            }
        })
        .filter(|column| {
            planned.iter().any(|p| {
                matches!(
                    &p.change,
                    Change::Column(ColumnChange::AlterColumnNumbering {
                        column: c,
                        from: ValueSource::Identity(_),
                        to: ValueSource::Identity(_),
                        ..
                    }) if c == *column
                )
            })
        })
        .cloned()
        .collect();
    let (early_release, sequence_cycles) =
        sequence_release_depths(planned.iter().map(|p| &p.change), &held_back);
    for renames in sequence_cycles {
        errs.push(DiffError::SequenceRenameCycle { renames });
    }
    // An attach validates its parent's foreign keys over the rows it brings,
    // as `ADD FOREIGN KEY` does, so one whose parent references a table this
    // plan writes rows into, or attaches a table to, waits for them: among
    // the row changes, at its parent's rank, after every table the parent
    // references and before the rows of a table that references the parent.
    // Its own changes follow it there. Any other attach stays in class 7
    // (#1642 review).
    let late_attach: BTreeMap<TableName, isize> = attach_parents
        .iter()
        .filter(|(_, parent)| {
            declared.schema.tables.get(*parent).is_some_and(|t| {
                t.foreign_keys.values().any(|fk| {
                    fk.references_table != **parent
                        && (row_tables.contains(&fk.references_table)
                            || attach_parents.values().any(|p| *p == fk.references_table))
                })
            })
        })
        .map(|(table, parent)| {
            (
                table.clone(),
                data_rank.get(parent).map_or(0, |r| *r as isize),
            )
        })
        .collect();
    let follows_late_attach = |c: &Change| -> bool {
        if let Change::Partition(PartitionChange::SetPartitionDefault { table, .. })
        | Change::Partition(PartitionChange::SetPartitionNotNull { table, .. })
        | Change::TableProperty(TablePropertyChange::SetTablePersistence { table, .. })
        | Change::TableProperty(TablePropertyChange::SetStorageParameters { table, .. })
        | Change::Index(IndexChange::SetIndexStorageParameters { table, .. }) = c
        {
            late_attach.contains_key(table)
        } else {
            false
        }
    };
    // A function a computed column calls is dropped after the column, by a
    // connected plan's pass over the catalog's own edges
    // (`order_computed_by_edges`, DEC-1431.1). An offline plan is never
    // applied, and a text scan here took one function for another (#1174
    // review), so the differ moves nothing for it.
    // How deep each declared table sits in the foreign-key graph: 0 for one
    // that references no other, else one more than the deepest it does.
    // Switched to logged, the referenced table goes first, so by depth;
    // switched to unlogged, the referencing one, so by depth reversed: the
    // engine refuses either the other way round (measured on 16 and 18,
    // #1443). Self-references do not count, nor the keys inside a cycle,
    // which are dropped around the switches (#1488 review).
    // Only a persistence switch reads it, so a plan without one does not
    // build it (#1792).
    let fk_graph = planned
        .iter()
        .any(|p| {
            matches!(
                p.change,
                Change::TableProperty(TablePropertyChange::SetTablePersistence { .. })
            )
        })
        .then(|| ForeignKeyGraph::of(declared.schema));
    let deepest = fk_graph.as_ref().map_or(0, ForeignKeyGraph::deepest);
    // A parent's column default, numbering source or NULL-ness this plan
    // changes, which the engine recurses into every partition: its own on that
    // column is set after it, or the parent's would overwrite it (#1687).
    let parent_moved: BTreeSet<(TableName, ColumnName)> = planned
        .iter()
        .filter_map(|p| {
            // A retype carries its NOT NULL change with it (#1692 review).
            if let Change::Column(ColumnChange::AlterColumnDefault { column, .. })
            | Change::Column(ColumnChange::AlterColumnNumbering { column, .. })
            | Change::Column(ColumnChange::AlterColumnNullability { column, .. }) = &p.change
            {
                Some((column.table.clone(), column.name.clone()))
            } else if let Change::Column(ColumnChange::AlterColumnType {
                column,
                from_nullable,
                to_nullable,
                ..
            }) = &p.change
                && from_nullable != to_nullable
            {
                Some((column.table.clone(), column.name.clone()))
            } else {
                None
            }
        })
        .collect();
    // The parents whose columns this plan changes (#1687).
    let parent_columns_change: BTreeSet<TableName> = planned
        .iter()
        .filter(|p| parent_column(&p.change).is_some())
        .filter_map(|p| p.change.table())
        .filter(|t| {
            declared
                .schema
                .tables
                .get(*t)
                .is_some_and(|t| t.partition_by.is_some())
        })
        .cloned()
        .collect();
    // The DEFAULT partitions a create in this plan moves rows out of (#1547).
    let moved_out_of: BTreeSet<TableName> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Table(TableChange::CreateTable {
                beside_default: Some(default),
                ..
            }) = &p.change
            {
                Some(default.clone())
            } else {
                None
            }
        })
        .collect();
    let follows_its_parent = |c: &Change| -> bool {
        let (Change::Partition(PartitionChange::SetPartitionDefault { table, column, .. })
        | Change::Partition(PartitionChange::SetPartitionNotNull { table, column, .. })) = c
        else {
            return false;
        };
        declared
            .schema
            .tables
            .get(table)
            .and_then(|t| t.partition_of.as_ref())
            .is_some_and(|of| parent_moved.contains(&(of.parent.clone(), column.clone())))
    };
    let mut type_rank = BTreeMap::new();
    let mut drop_type_rank = BTreeMap::new();
    for (schema, ranks) in [
        (declared.schema, &mut type_rank),
        (base.schema, &mut drop_type_rank),
    ] {
        for name in schema.types.keys() {
            let mut at = name;
            let mut seen = BTreeSet::new();
            while let Some(pbps_model::ManagedType::Domain { base, .. }) = schema.types.get(at) {
                if !seen.insert(at.clone()) {
                    errs.push(DiffError::TypeLifecycleUnsupported {
                        name: name.clone(),
                        reason: "domain base dependency cycle".into(),
                    });
                    break;
                }
                let Some(next) = base.type_ref() else {
                    break;
                };
                at = next;
            }
            ranks.insert(name.clone(), seen.len() as isize);
        }
    }
    let sort_class = |c: &Change| -> (u8, usize) {
        if matches!(
            c,
            Change::Type(TypeChange::CreateType { .. })
                | Change::Search(SearchChange::CreateSearch { .. })
                | Change::Search(SearchChange::AlterSearchMapping { .. })
                | Change::Search(SearchChange::AlterSearchDictionary { .. })
                | Change::TypeValue(TypeValueChange::AddEnumValue { .. })
                | Change::TypeValue(TypeValueChange::RenameEnumValue { .. })
                | Change::TypeValue(TypeValueChange::EnumRebuild {
                    step: pbps_model::EnumRebuildStep::Create { .. },
                    ..
                })
        ) {
            return (7, 0);
        }
        // The columns move and the type is swapped among the type-changing
        // alterations of class 9, ranked with a retype (DEC-1635.7).
        if matches!(
            c,
            Change::TypeValue(TypeValueChange::EnumRebuild {
                step: pbps_model::EnumRebuildStep::Retype { .. }
                    | pbps_model::EnumRebuildStep::Swap { .. },
                ..
            })
        ) {
            return (COLUMN_ALTERATIONS, 1);
        }

        // After every module and its PUBLIC decision, then policies before
        // either flag. Keep the established class ordinals (DEC-1172.1).
        if matches!(c, Change::Policy(PolicyChange::CreatePolicy { .. })) {
            return (14, 2);
        }
        if matches!(c, Change::Policy(PolicyChange::SetRowSecurity { .. })) {
            return (14, 3);
        }
        if let Change::Column(ColumnChange::AlterColumnNumbering { column, .. }) = c
            && bounds_first.contains(column)
        {
            return (COLUMN_ALTERATIONS, 0);
        }
        if let Change::Partition(PartitionChange::AttachPartition { table, .. }) = c
            && late_attach.contains_key(table)
        {
            return (ROW_DELETIONS - 1, 1);
        }
        if follows_late_attach(c) {
            return (ROW_DELETIONS - 1, 2);
        }
        // A DEFAULT partition's own NOT NULL, tightened in a plan that moves
        // rows out of it, comes after every create that moves them, whatever
        // class the create lands in (7, or (9, 5) under a parent whose columns
        // change): the rows the move takes may be the ones holding NULL there,
        // and the engine scans what is left. Its own checks and uniques are of
        // class 13, after every create already (#1763 review).
        if let Change::Partition(PartitionChange::SetPartitionNotNull {
            table,
            not_null: true,
            ..
        }) = c
            && moved_out_of.contains(table)
        {
            return (COLUMN_ALTERATIONS, 6);
        }
        if follows_its_parent(c) {
            return (COLUMN_ALTERATIONS, 4);
        }
        if let Change::TableProperty(TablePropertyChange::SetTablePersistence {
            table,
            unlogged,
            ..
        }) = c
        {
            let depth = fk_graph.as_ref().map_or(0, |g| g.depth(table));
            return (
                COLUMN_ALTERATIONS,
                10 + if *unlogged { deepest - depth } else { depth },
            );
        }
        // A replica identity whose target stands before the plan is set
        // first, under the table's old name: before the drops of class 2 can
        // take its old index, which would leave the table identifying no row
        // (DEC-1444.1).
        if let Change::TableProperty(TablePropertyChange::SetReplicaIdentity { uid, to, .. }) = c
            && early_identity.contains(&(uid.clone(), to.clone()))
        {
            return (0, 1);
        }
        // A tightening of a table whose rows this plan writes or deletes runs
        // after them, at the end of the deletes' class: the rows may be what
        // fills or removes its NULLs, and no row change needs the column NOT
        // NULL first. Still before the additions of class 13, a primary key
        // among them, which SQL Server refuses over a nullable column (#1367).
        //
        // An identity this plan puts on follows its tightening there, since
        // the engine refuses one on a nullable column: after the deletes,
        // which may remove the rows holding NULL. Any source put on follows
        // them too, since it continues past the values left, and those a
        // delete removes may be the greatest. The table's own inserts follow
        // the source, since a declared row leaves the numbered column out to
        // take its number. Where the plan inserts rows into the table and
        // deletes none, the source, and an identity's tightening, stay in
        // class 9 ahead of the inserts, where a referencing table's rows can
        // still follow them (#1930 review).
        let early =
            |column: &ColumnRef| inserted.contains(&column.table) && !late.contains(&column.table);
        if let Change::Column(ColumnChange::AlterColumnNullability {
            column,
            to_nullable: false,
            ..
        }) = c
            && row_tables.contains(&column.table)
            && !(identified.contains(column) && early(column))
        {
            return (ROW_DELETIONS, 2);
        }
        if let Change::Column(ColumnChange::AlterColumnNumbering { column, .. }) = c
            && installed.contains(column)
            && row_tables.contains(&column.table)
            && !early(column)
        {
            return (ROW_DELETIONS, 3);
        }
        if let Change::Row(RowChange::InsertRow { table, .. }) = c
            && late.contains(table)
        {
            return (ROW_DELETIONS, 4);
        }
        // An owned sequence given up, or renamed, frees its name before the
        // tables and columns this plan creates, one of which may take it:
        // after the drops of class 6, which free names too, and before
        // `CreateTable` (#1930 review). None needs anything class 9 does
        // first: giving one up drops a default and a sequence, a rename keeps
        // the sequence, and a conversion to an identity needs the column
        // `NOT NULL`, which a column this plan does not tighten already is.
        // An identity put on a column retyped after it moves with it.
        //
        // Among them, a rename to a name another of them frees follows it,
        // down the chain (#1930 review).
        if let Change::Column(ColumnChange::AlterColumnNumbering { column, .. }) = c
            && let Some(depth) = early_release.get(column)
        {
            return (6, 1 + depth);
        }
        // A generated column is dropped before the ordinary columns of its
        // class, and added after every column change of the class beyond:
        // the additions, and the in-place alterations of class 9. Measured,
        // the engine refuses to drop a column a generated column reads,
        // refuses a generation expression over a column that is not there yet,
        // and refuses to retype one a generated column reads. A generated
        // column never reads another, and nothing in class 9 needs one that is
        // new, so one layer each way is the whole order (DEC-1168.1).
        let drops_generated = matches!(c, Change::Column(ColumnChange::DropColumn { uid, .. }) if generated_in_base.contains(uid));
        if frees_a_renamed_column(c) {
            // The layer holds here too: every drop of a table that renames a
            // column comes to this class, the generated one and its input
            // alike, so the generated one keeps its place ahead.
            if drops_generated {
                return (2, 2);
            }
            // Between the constraint and index drops of class 2 — a column a
            // check or an index names cannot be dropped while they stand —
            // and the `RenameColumn` of class 3 that is waiting for its name.
            //
            // Measured on the pinned images, this is the difference between a
            // reviewed plan and one the engine refuses: SQL Server answers
            // `sp_rename` with `Msg 15335, The new name 'note' is already in
            // use as a COLUMN name and would cause a duplicate that is not
            // permitted`, PostgreSQL with `column "note" of relation "s"
            // already exists` (DECISIONS 474, issue #398).
            //
            // Only the drop that frees a claimed name moves. A rename into a
            // name nothing here gives up is refused by `resolve` as an
            // occupied target and never reaches this sort, which is the
            // property `a_single_revision_cannot_rename_into_an_occupied_baseline_name`
            // holds.
            return (2, 3);
        }
        if drops_generated {
            return (order_key(c), 0);
        }
        if let Change::Column(ColumnChange::AddColumn { column, .. }) = c
            && (match &column.value_source {
                ValueSource::Generated(_) => true,
                ValueSource::None
                | ValueSource::Default(_)
                | ValueSource::Identity(_)
                | ValueSource::Sequence(_) => false,
            })
        {
            return (COLUMN_ALTERATIONS, 2);
        }
        // A computed column comes down after every index and constraint drop
        // of class 2, the ones that free a renamed column's name (2, 3)
        // included, and goes up after every alteration of class 9: it reads
        // the columns, in their final type, and nothing in either class reads
        // it (#1174).
        if matches!(c, Change::Column(ColumnChange::DropComputedColumn { .. })) {
            return (2, 4);
        }
        // A package's own objects are what a column, a domain or a module of
        // the plan may use (DEC-2447.12), and its create, update and move share
        // the type class with them, so they go first inside it: nothing else
        // there is ordered against them, and a tie would be decided by a random
        // uid.
        if matches!(
            c,
            Change::Extension(ExtensionChange::CreateExtension { .. })
                | Change::Extension(ExtensionChange::UpdateExtension { .. })
                | Change::Extension(ExtensionChange::SetExtensionSchema { .. })
        ) {
            return (order_key(c), 0);
        }
        if matches!(c, Change::Column(ColumnChange::AddComputedColumn { .. })) {
            return (COLUMN_ALTERATIONS, 3);
        }
        // A detach comes after every table drop of its class: it claims the
        // declared names of what it keeps, and a dropped table's index or
        // key may be what holds one now. A drop claims no name, so nothing
        // waits the other way (#1544).
        if matches!(
            c,
            Change::Partition(PartitionChange::DetachPartition { .. })
        ) {
            return (order_key(c), 2);
        }
        // A partition is created after every parent, whose columns, keys and
        // indexes the engine gives it as it is made (#1170). Under a parent
        // whose columns this plan changes, after those changes too: its own
        // default may name a column the parent adds, and a parent's default
        // set or dropped afterwards would overwrite its own (#1692 review).
        if let Change::Table(TableChange::CreateTable { table, .. }) = c
            && let Some(of) = &table.partition_of
        {
            if parent_columns_change.contains(&of.parent) {
                return (COLUMN_ALTERATIONS, 5);
            }
            return (order_key(c), 2);
        }
        // A partitioned parent's index first in its class, before any
        // partition's own: built after one, it would take it as its clone
        // (#1737 review, DEC-1688.1). First rather than the partition's own
        // last, which would put it after a replica identity naming it. A
        // parent's index reads only its columns, all of earlier classes.
        if let Change::Index(IndexChange::AddIndex { table, .. }) = c
            && declared
                .schema
                .tables
                .get(table)
                .is_some_and(|t| t.partition_by.is_some())
        {
            return (order_key(c), 0);
        }
        if let Change::Column(ColumnChange::RenameColumn { table, from, .. }) = c {
            let depth = chain_depth
                .get(&(table.clone(), from.clone()))
                .copied()
                .unwrap_or(0);
            (order_key(c), 1 + depth)
        } else {
            (order_key(c), 1)
        }
    };
    // Cached: `sort_by_key` computed this key, `Debug` text and all, on both
    // sides of every comparison, a third of a 4,000-table plan's CPU
    // (#1908). The cache keeps equal keys in their original order, as the
    // stable sort did, so the order is the same.
    let extension_rank = crate::extensions::depths(planned.iter().map(|p| &p.change));
    // A domain this plan creates may be built on a type a package of this plan
    // provides, created or moved here (the effects say so: its base needs the
    // package's change). The draft is what orders two changes of one class, and
    // the effects only check it, so the creation ranks behind the last package
    // change instead of tying with it, where the subject decided (#2696 ready
    // round, DEC-2447.12).
    fn built_domain(c: &Change) -> Option<&pbps_model::ManagedType> {
        let definition = if let Change::Type(TypeChange::CreateType { definition, .. })
        | Change::TypeValue(TypeValueChange::EnumRebuild {
            step: pbps_model::EnumRebuildStep::Create { definition, .. },
            ..
        }) = c
        {
            definition.as_ref()
        } else {
            return None;
        };
        matches!(definition, pbps_model::ManagedType::Domain { .. }).then_some(definition)
    }
    let package_floor: isize = planned
        .iter()
        .filter_map(|p| {
            if let Change::Extension(ExtensionChange::CreateExtension { uid, .. })
            | Change::Extension(ExtensionChange::UpdateExtension { uid, .. }) = &p.change
            {
                Some(extension_rank.get(uid).copied().unwrap_or(0) + 1)
            } else if let Change::Extension(ExtensionChange::SetExtensionSchema { uid, .. }) =
                &p.change
            {
                Some(extension_rank.get(uid).copied().unwrap_or(0) + 2)
            } else {
                None
            }
        })
        .max()
        .unwrap_or(0);
    // A dictionary is created before the configuration that maps it, and the configuration
    // dropped before the dictionary (DEC-2728.2): the depth of the references among the
    // objects the plan holds on each side. An object created here that maps a dictionary a
    // package may provide starts behind the packages of this plan (the effects:
    // `package_input`), and what maps it sits one rank behind that, so the floor is composed
    // into the depth, not added to the sort key afterwards.
    let created_search: BTreeSet<&pbps_model::SearchRef> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Search(SearchChange::CreateSearch { name, .. }) = &p.change {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    // One altered in place maps what it now maps, so it waits for the packages as a create does.
    let altered_search: BTreeSet<&pbps_model::SearchRef> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Search(
                SearchChange::AlterSearchMapping { name, .. }
                | SearchChange::AlterSearchDictionary { name, .. },
            ) = &p.change
            {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    let search_depths = |schema: &Schema, floor: isize| -> BTreeMap<pbps_model::SearchRef, isize> {
        let start = |name: &pbps_model::SearchRef, object: &pbps_model::SearchObject| {
            if floor > 0
                && (created_search.contains(name) || altered_search.contains(name))
                && object.dictionaries().iter().any(|d| {
                    d.name.schema() != "pg_catalog" && declared.ids.search_uid(d).is_none()
                })
            {
                floor
            } else {
                0
            }
        };
        schema
            .search
            .keys()
            .map(|name| {
                (
                    name.clone(),
                    search_depth(schema, name, &start, &mut BTreeSet::new()),
                )
            })
            .collect()
    };
    let (search_rank, drop_search_rank) = (
        search_depths(declared.schema, package_floor),
        search_depths(base.schema, 0),
    );
    // A domain's check or default may name a configuration this plan creates by a literal
    // (`'app.docs'::regconfig`), which the engine resolves when the domain is created: the
    // same literal scan the scratch order uses (`may_call`). The two share a class, so the
    // domains rank behind every created search object, all of them together, which keeps the
    // order among them (a chain of domains is ranked by its bases). A plan whose domains name
    // none leaves them where they were.
    let names_a_search = planned
        .iter()
        .filter_map(|p| built_domain(&p.change))
        .any(|domain| {
            let pbps_model::ManagedType::Domain {
                default, checks, ..
            } = domain
            else {
                return false;
            };
            created_search.iter().any(|created| {
                default
                    .iter()
                    .chain(checks.values())
                    .any(|text| dialect.generation_may_call(text, created.name.name()))
            })
        });
    let domain_floor = if names_a_search {
        1 + created_search
            .iter()
            .filter_map(|created| search_rank.get(*created))
            .max()
            .copied()
            .unwrap_or(0)
    } else {
        0
    };
    crate::order::normalization::sort_draft(&mut planned, |p| {
        let (class, within) = sort_class(&p.change);
        (
            class,
            within,
            if let Change::Partition(PartitionChange::AttachPartition { table, .. }) = &p.change
                && let Some(rank) = late_attach.get(table)
            {
                *rank
            } else if let Change::Extension(ExtensionChange::CreateExtension { uid, .. })
            | Change::Extension(ExtensionChange::DropExtension { uid, .. })
            | Change::Extension(ExtensionChange::UpdateExtension { uid, .. }) = &p.change
            {
                // Behind the packages of this plan it requires, or that require
                // it for a drop (DEC-2447.6, DEC-2447.8).
                extension_rank.get(uid).copied().unwrap_or(0)
            } else if let Change::Search(
                SearchChange::CreateSearch { name, .. }
                | SearchChange::AlterSearchMapping { name, .. }
                | SearchChange::AlterSearchDictionary { name, .. },
            ) = &p.change
            {
                // Behind the packages and the dictionaries of this plan it maps.
                search_rank.get(name).copied().unwrap_or(0)
            } else if let Change::Search(SearchChange::DropSearch { name, .. }) = &p.change {
                -drop_search_rank.get(name).copied().unwrap_or(0)
            } else if let Change::Extension(ExtensionChange::SetExtensionSchema { uid, .. }) =
                &p.change
            {
                // One rank behind the package's own update, whose result the
                // move starts from: the two rewrite one package, and the draft
                // is what orders two writes (DEC-2447.11).
                extension_rank.get(uid).copied().unwrap_or(0) + 1
            } else {
                dependency_rank(
                    &p.change,
                    &create_rank,
                    &drop_rank,
                    &data_rank,
                    &role_rank,
                    &type_rank,
                    &drop_type_rank,
                ) + if built_domain(&p.change).is_some() {
                    package_floor.max(domain_floor)
                } else {
                    0
                }
            },
            rebuild_subject(&p.change),
            // Columns added to one table in the order they are declared:
            // the engine appends each, so this is the table's column order,
            // which a pull writes and a positional `INSERT` binds to. Left
            // to the rendering below, the column's random uid decided it
            // (#2080).
            if let Change::Column(ColumnChange::AddColumn { table, name, .. }) = &p.change {
                declared
                    .schema
                    .tables
                    .get(table)
                    .and_then(|t| t.columns.get_index_of(name))
                    .unwrap_or(0)
            } else {
                0
            },
            tie_break_text(&p.change),
        )
    });
    for cycle in crate::order::rename_order::order(&mut planned, base, &renames, dialect) {
        errs.push(DiffError::TableRenameCycle { renames: cycle });
    }
    for (table, column, block) in
        crate::order::last_column::order(&mut planned, base, declared, dialect)
    {
        errs.push(DiffError::LastColumnReplacementUnorderable {
            table,
            column,
            block,
        });
    }
    let mut changes = ChangeSet { changes: planned };
    if let Err(reason) = crate::drop_requirements::assign(&mut changes, base.ids, declared.ids)
        .and_then(|()| crate::drop_requirements::validate(&changes, base.ids, declared.ids))
    {
        errs.push(DiffError::DropRequirement { reason });
    }
    // Replacement risk belongs to both policy parts, including in partial
    // reports. Reuse the same UID/name pairing as final plan admission.
    let risks = crate::order::generated::risks(&changes, declared.ids, dialect);
    for (part, risks) in changes.changes.iter_mut().zip(risks) {
        part.risks = risks;
    }
    errs.extend(crate::enum_labels::unsafe_uses(&changes, declared, dialect));
    Diffed {
        changes,
        errors: errs,
    }
}

/// What one comparison produced: the changes the differ could express, and the
/// differences it could not.
///
/// Not a `Result`, deliberately — the two are not alternatives. A comparison
/// can find both at once, and the type that says so is what stops a caller
/// discarding one to obtain the other.
#[derive(Debug, Default)]
pub struct Diffed {
    pub changes: ChangeSet,
    pub errors: Vec<DiffError>,
}

/// Columns of a surviving table that exist on both sides: compare attributes.
#[allow(clippy::too_many_arguments)]
fn diff_columns(
    index: &Indexes<'_>,
    types: &TypeResolver<'_>,
    base_table_name: &TableName,
    declared_table_name: &TableName,
    base_table: &Table,
    declared_table: &Table,
    dialect: &(impl Dialect + ?Sized),
    conversions: &BTreeMap<Uid, pbps_model::Conversion>,
    changes: &mut Vec<Change>,
    errs: &mut Vec<DiffError>,
) {
    let base_cols = columns_of(&index.base, base_table_name);
    let declared_cols = columns_of(&index.declared, declared_table_name);

    for (uid, declared_ref) in &declared_cols {
        let Some(base_ref) = base_cols.get(uid) else {
            // Present only on the declared side: a new column.
            if let Some(c) = declared_table.columns.get(&declared_ref.name) {
                // The change does not carry the table's persistence, and the
                // emitter creates the sequence logged (#1631).
                if let ValueSource::Sequence(_) = &c.value_source
                    && declared_table.unlogged
                {
                    errs.push(DiffError::SequenceChangeUnsupported {
                        column: declared_ref.clone(),
                    });
                }
                changes.push(Change::Column(ColumnChange::AddColumn {
                    uid: uid.clone(),
                    table: declared_table_name.clone(),
                    name: declared_ref.name.clone(),
                    column: Box::new(c.clone()),
                }));
            }
            continue;
        };

        if base_ref.name != declared_ref.name {
            changes.push(Change::Column(ColumnChange::RenameColumn {
                uid: uid.clone(),
                table: declared_table_name.clone(),
                from: base_ref.name.clone(),
                to: declared_ref.name.clone(),
                table_was: (base_table_name != declared_table_name)
                    .then(|| base_table_name.clone()),
            }));
        }

        let (Some(base_col), Some(col)) = (
            base_table.columns.get(&base_ref.name),
            declared_table.columns.get(&declared_ref.name),
        ) else {
            continue;
        };

        let norm = |t: &ColumnType| dialect.normalize_type(t).unwrap_or_else(|_| t.clone());
        let (mut from_ty, to_ty) = (norm(&base_col.ty), norm(&col.ty));
        if let (Some(old), Some(new)) = (from_ty.type_ref(), to_ty.type_ref())
            && types.same_declared(old, new)
            && from_ty.is_array() == to_ty.is_array()
        {
            from_ty = to_ty.clone();
        }
        let mut recomputed = false;
        // A numbering source changed in place (#1889, DEC-1889.1), where the
        // engine has an `ALTER` for it and the table takes it. A parent's
        // serial default recurses into its partitions (DEC-2219.1); an
        // identity still needs its partition handling (#1681). An owned
        // sequence is created logged,
        // which an `UNLOGGED` table's would not be (#1631).
        //
        // A plain default on either side is not the change's: one given up
        // or taken is an `AlterColumnDefault` beside it, from or to nothing,
        // so it is ordered as every default is, around a retype and after
        // the functions it calls (#1930 review).
        let numbering = |from: &ValueSource,
                         to: &ValueSource,
                         errs: &mut Vec<DiffError>,
                         changes: &mut Vec<Change>| {
            let identity =
                matches!(from, ValueSource::Identity(_)) || matches!(to, ValueSource::Identity(_));
            if !dialect.changes_numbering() {
                errs.push(if identity {
                    DiffError::IdentityChangeUnsupported {
                        column: declared_ref.clone(),
                    }
                } else {
                    DiffError::SequenceChangeUnsupported {
                        column: declared_ref.clone(),
                    }
                });
            } else if [base_table, declared_table]
                .iter()
                .any(|t| t.partition_of.is_some() || (identity && t.partition_by.is_some()))
            {
                errs.push(DiffError::NumberingChangeOnPartitionedTable {
                    column: declared_ref.clone(),
                });
            } else if matches!(to, ValueSource::Sequence(_))
                && !matches!(from, ValueSource::Sequence(_))
                && (base_table.unlogged || declared_table.unlogged)
            {
                errs.push(DiffError::SequenceChangeUnsupported {
                    column: declared_ref.clone(),
                });
            } else if let (
                ValueSource::Identity(pbps_model::Identity { increment: was, .. }),
                ValueSource::Identity(pbps_model::Identity { increment: now, .. }),
            )
            | (
                ValueSource::Sequence(pbps_model::OwnedSequence { increment: was, .. }),
                ValueSource::Sequence(pbps_model::OwnedSequence { increment: now, .. }),
            ) = (from, to)
                && (*was < 0) != (*now < 0)
            {
                // The sequence keeps the bounds of the direction it was
                // made for, which a start in the other refuses (#1930 review).
                errs.push(DiffError::NumberingDirectionChange {
                    column: declared_ref.clone(),
                });
            } else {
                changes.push(Change::Column(ColumnChange::AlterColumnNumbering {
                    uid: uid.clone(),
                    column: declared_ref.clone(),
                    ty: to_ty.clone(),
                    from: from.clone(),
                    to: to.clone(),
                }));
            }
        };
        // Compare the source as one property: ordinary defaults may change,
        // generated text may change in place, a numbering source changes as
        // one change, and a change to or from generated refuses.
        let (old_default, new_default) = match (&base_col.value_source, &col.value_source) {
            (ValueSource::None, ValueSource::None) => (None, None),
            (ValueSource::Default(old), ValueSource::Default(new)) => (Some(old), Some(new)),
            (ValueSource::None, ValueSource::Default(new)) => (None, Some(new)),
            (ValueSource::Default(old), ValueSource::None) => (Some(old), None),
            (ValueSource::Identity(old), ValueSource::Identity(new)) => {
                // The engine moves an identity's sequence to a new type on
                // its own (measured on 16 and 18).
                if old != new {
                    numbering(&base_col.value_source, &col.value_source, errs, changes);
                }
                (None, None)
            }
            (ValueSource::Generated(from), ValueSource::Generated(to)) => {
                if from.stored != to.stored {
                    errs.push(DiffError::GenerationChangeUnsupported {
                        column: declared_ref.clone(),
                    });
                } else if from.expression != to.expression {
                    recomputed = true;
                    changes.push(Change::Column(ColumnChange::AlterColumnExpression {
                        uid: uid.clone(),
                        column: declared_ref.clone(),
                        from: from.expression.clone(),
                        to: to.expression.clone(),
                    }));
                }
                (None, None)
            }
            (ValueSource::Generated(_), ValueSource::None | ValueSource::Default(_))
            | (ValueSource::None | ValueSource::Default(_), ValueSource::Generated(_)) => {
                errs.push(DiffError::GenerationChangeUnsupported {
                    column: declared_ref.clone(),
                });
                (None, None)
            }
            // A name is compared only where both sides give one. The
            // declaration's may be left out (`OwnedSequence::declares`), and
            // a base without one does not know it: a git baseline records
            // the name an earlier declaration generated, under whatever table
            // and column it was declared on then, and a rename keeps the
            // sequence's. So a renamed table may name the sequence it kept,
            // and a later pull's name for it is no change. The apply's own
            // read compares the name the engine holds. A retype moves the
            // sequence to the new type, which the engine does not do for an
            // owned sequence (measured on 16 and 18), and which the reader
            // holds a `serial` to (#1631).
            (ValueSource::Sequence(from), ValueSource::Sequence(to)) => {
                let from = pbps_model::OwnedSequence {
                    name: from.name.clone().or_else(|| to.name.clone()),
                    ..from.clone()
                };
                if !to.declares(&from) || from_ty != to_ty {
                    numbering(&base_col.value_source, &col.value_source, errs, changes);
                }
                (None, None)
            }
            (
                ValueSource::Identity(_) | ValueSource::Sequence(_),
                ValueSource::None | ValueSource::Identity(_) | ValueSource::Sequence(_),
            )
            | (ValueSource::None, ValueSource::Identity(_) | ValueSource::Sequence(_)) => {
                numbering(&base_col.value_source, &col.value_source, errs, changes);
                (None, None)
            }
            (ValueSource::Identity(_) | ValueSource::Sequence(_), ValueSource::Default(new)) => {
                numbering(&base_col.value_source, &ValueSource::None, errs, changes);
                (None, Some(new))
            }
            (ValueSource::Default(old), ValueSource::Identity(_) | ValueSource::Sequence(_)) => {
                numbering(&ValueSource::None, &col.value_source, errs, changes);
                (Some(old), None)
            }
            (ValueSource::Identity(_) | ValueSource::Sequence(_), ValueSource::Generated(_))
            | (ValueSource::Generated(_), ValueSource::Identity(_) | ValueSource::Sequence(_)) => {
                errs.push(DiffError::GenerationChangeUnsupported {
                    column: declared_ref.clone(),
                });
                (None, None)
            }
        };

        // A collation change is the same `ALTER COLUMN` as a type change, and
        // takes down the same dependents (#1175), so it is carried by the
        // same change with the type restated.
        let recollated = base_col.collation != col.collation;
        let retyped = from_ty != to_ty || recollated;
        // A type change subsumes a nullability change rather than sitting beside
        // one: `ALTER COLUMN` restates the whole definition, so two changes would
        // mean two statements where the second undoes half of the first.
        //
        // Except a tightening of a recomputed column. The type goes before the
        // new expression, so the values are computed in the final type, and
        // `NOT NULL` after it, so it is checked against the new values rather
        // than the old expression's (DEC-1168.1).
        let tightened_after = recomputed && base_col.nullable && !col.nullable;
        // The expression a human recorded for exactly this change (#2331,
        // DEC-2331.1), and only for it: this environment's start must be the
        // recorded one. An environment already on the declared type has no
        // change and never reads the record, so each applies it once. One
        // that starts elsewhere, and needs an expression to get there, is
        // refused by name rather than converted under text written for
        // another type.
        let mut using = None;
        let spelled = |ty: &ColumnType| ty.raw().to_string();
        if let Some(record) = conversions
            .get(uid)
            .filter(|r| r.to == spelled(&to_ty) && from_ty != to_ty)
        {
            if record.from == spelled(&from_ty) {
                if base_ref.name == declared_ref.name {
                    using = Some(record.using.clone());
                } else {
                    errs.push(DiffError::ConversionWithRename {
                        column: declared_ref.clone(),
                    });
                }
            } else if pbps_model::ValueChange::cast(&from_ty, &to_ty, None).map_or_else(
                || dialect.type_change_needs_using(&from_ty, &to_ty),
                |values| dialect.value_change(&values, "").needs_using.is_some(),
            ) {
                errs.push(DiffError::ConversionFromMismatch {
                    column: declared_ref.clone(),
                    recorded: record.from.clone(),
                    actual: spelled(&from_ty),
                    to: record.to.clone(),
                });
            }
        }
        let converting = using.is_some();
        if retyped {
            changes.push(Change::Column(ColumnChange::AlterColumnType {
                uid: uid.clone(),
                column: declared_ref.clone(),
                from: from_ty.clone(),
                to: to_ty.clone(),
                from_nullable: base_col.nullable,
                to_nullable: col.nullable || tightened_after,
                through_external_partitions: false,
                from_collation: base_col.collation.clone(),
                to_collation: col.collation.clone(),
                using,
            }));
            if tightened_after {
                changes.push(Change::Column(ColumnChange::AlterColumnNullability {
                    uid: uid.clone(),
                    column: declared_ref.clone(),
                    ty: to_ty,
                    to_nullable: false,
                    through_external_partitions: false,
                    collation: col.collation.clone(),
                }));
            }
        } else if base_col.nullable != col.nullable {
            changes.push(Change::Column(ColumnChange::AlterColumnNullability {
                uid: uid.clone(),
                column: declared_ref.clone(),
                ty: to_ty,
                to_nullable: col.nullable,
                through_external_partitions: false,
                collation: col.collation.clone(),
            }));
        }
        // A default the expression's result cannot take is refused by the
        // engine at the retype -- measured on 16 and 18, `default for column
        // "c" cannot be cast automatically` -- even where its text is
        // unchanged and the old one was cast across without a word. So a
        // conversion takes the default down and puts it back, as a changed
        // one does (#2331).
        if defaults_differ(old_default, new_default) || (converting && old_default.is_some()) {
            // A default that is *replaced* on a column this same plan retypes
            // is two changes, because the type change has to run between them.
            // Measured on SQL Server, an `ALTER COLUMN` that changes the type
            // is refused while any default constraint stands — 5074, "the
            // object 'df_dn2' is dependent on column 'n'", with 4922 behind it
            // — and the nullability form of the same statement is accepted, so
            // the dependency is the type's and not `ALTER COLUMN`'s.
            //
            // Only when the type moves, and only when there is an old default
            // *and* a new one. A default replaced on a column that keeps its
            // type needs no drop: `SET DEFAULT` replaces on PostgreSQL, and
            // the SQL Server emitter already drops and adds inside its one
            // statement. Splitting it there would put two lines at opposite
            // ends of the plan where one says it better (SPEC §14.1).
            let split = retyped && old_default.is_some() && new_default.is_some();
            changes.push(Change::Column(ColumnChange::AlterColumnDefault {
                uid: uid.clone(),
                column: declared_ref.clone(),
                from: old_default.cloned(),
                to: if split { None } else { new_default.cloned() },
            }));
            if split {
                changes.push(Change::Column(ColumnChange::AlterColumnDefault {
                    uid: uid.clone(),
                    column: declared_ref.clone(),
                    from: None,
                    to: new_default.cloned(),
                }));
            }
        }
        if base_col.deprecated != col.deprecated {
            changes.push(Change::Column(ColumnChange::SetColumnDeprecated {
                uid: uid.clone(),
                column: declared_ref.clone(),
                reason: col.deprecated.clone(),
            }));
        }
    }

    // Present only on the base side: a dropped column.
    for (uid, base_ref) in &base_cols {
        if !declared_cols.contains_key(uid) {
            changes.push(Change::Column(ColumnChange::DropColumn {
                uid: uid.clone(),
                // The **declared** table, with the column's base name. Only
                // the table is renamed here — a column this plan drops is
                // absent from the declarations, so it has no rename intent
                // and the database still knows it by the base name.
                //
                // `base_ref` carries the pre-rename table, and every sibling
                // in this loop is built from `declared_table_name`. Taken
                // whole it emitted `sp_rename 'dbo.customers', 'clients'`
                // and then `ALTER TABLE [dbo].[customers] DROP COLUMN ...`,
                // because `order_key` runs the rename first: a valid,
                // reviewed plan refused by the engine.
                column: declared_table_name.column(base_ref.name.clone()),
            }));
        }
    }
}

fn columns_of(ids: &pbps_model::IdsIndex<'_>, table: &TableName) -> BTreeMap<Uid, ColumnRef> {
    ids.columns_of(table)
        .map(|(u, c)| (u.clone(), c.clone()))
        .collect()
}

fn partition_columns_by_uid<'a, T: 'a>(
    columns: impl IntoIterator<Item = (&'a ColumnName, &'a T)>,
    before: &TableName,
    after: &TableName,
    index: &Indexes<'_>,
) -> BTreeMap<ColumnName, &'a T> {
    // What became of each of the parent's base columns, by uid: its declared
    // name, or `None` where the plan drops it. By uid and not by name, since
    // one plan can drop a column and rename another into its name (#1692
    // review). A column no identity names is taken as it stands.
    let now_by_uid = columns_of(&index.declared, after);
    let fate: BTreeMap<ColumnName, Option<ColumnName>> = columns_of(&index.base, before)
        .into_iter()
        .map(|(uid, c)| (c.name, now_by_uid.get(&uid).map(|d| d.name.clone())))
        .collect();
    let is_now = |c: &ColumnName| -> Option<ColumnName> {
        match fate.get(c) {
            Some(now) => now.clone(),
            None => Some(c.clone()),
        }
    };
    columns
        .into_iter()
        .filter_map(|(c, value)| Some((is_now(c)?, value)))
        .collect()
}

/// The renames this plan performs, as a map the model can bring a base-side
/// table's constraints forward through.
///
/// The uid is what says two differently-spelled names are one object, exactly
/// as it does for rows (see `base_name_of` at the call site). What the map is
/// then allowed to rewrite — and what it must not — is [`Renames::apply`]'s
/// business, and the measurement that decides it lives there.
fn renames_of(base: Side, declared: Side) -> Renames {
    let mut renames = Renames::default();
    for (uid, declared_name) in &declared.ids.tables {
        if let Some(base_name) = base.ids.tables.get(uid)
            && base_name != declared_name
        {
            renames.rename_table(base_name.clone(), declared_name.clone());
        }
    }
    for (uid, declared_ref) in &declared.ids.columns {
        if let Some(base_ref) = base.ids.columns.get(uid)
            && base_ref.name != declared_ref.name
        {
            renames.rename_column(base_ref.clone(), declared_ref.name.clone());
        }
    }
    renames
}

/// A standing FK is bound to one backing index, not merely to columns that
/// another surviving key also covers. Offline the binding is unknown, so make
/// every affected managed FK explicit in the reviewed plan. Connected guards
/// use actual catalog dependencies to refuse an unplanned or external removal.
fn recreate_referenced_foreign_keys(
    base: Side<'_>,
    declared: Side<'_>,
    renames: &Renames,
    changes: &mut Vec<Change>,
) {
    if !changes.iter().any(|c| {
        matches!(
            c,
            Change::Constraint(ConstraintChange::SetPrimaryKey { from: Some(_), .. })
                | Change::Constraint(ConstraintChange::DropUnique { .. })
                | Change::Index(IndexChange::DropIndex { .. })
        )
    }) {
        return;
    }
    let aligned: BTreeMap<_, _> = base
        .ids
        .tables
        .iter()
        .filter_map(|(uid, old_name)| {
            let new_name = declared.ids.tables.get(uid)?;
            let table = base.schema.tables.get(old_name)?;
            Some((new_name.clone(), renames.apply(table, old_name)))
        })
        .collect();
    let mut removed = BTreeSet::new();
    for change in changes.iter() {
        let (table, columns) = match change {
            Change::Partition(PartitionChange::RebuildPartitionedTable(_))
            | Change::Index(IndexChange::BuildPartitionedIndex(_))
            | Change::Partition(PartitionChange::CopyTableRows(_))
            | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => continue,
            // No foreign key references an exclusion constraint (measured:
            // 42830), so its drop frees no key (DEC-2448.3).
            Change::Constraint(ConstraintChange::SetExclusion { .. }) => continue,
            Change::Constraint(ConstraintChange::SetPrimaryKey {
                table,
                from: Some(key),
                ..
            }) => (table, key.columns.iter().cloned().collect::<BTreeSet<_>>()),
            Change::Constraint(ConstraintChange::DropUnique { table, name }) => {
                let Some(key) = aligned.get(table).and_then(|t| t.unique.get(name)) else {
                    continue;
                };
                (table, key.columns.iter().cloned().collect::<BTreeSet<_>>())
            }
            Change::Index(IndexChange::DropIndex { table, name }) => {
                // Only a unique index over plain columns can back a foreign
                // key: the engine refuses one over an expression (DEC-1169.2).
                let Some(columns) = aligned
                    .get(table)
                    .and_then(|t| t.indexes.get(name))
                    .filter(|i| i.unique && i.filter.is_none())
                    .and_then(|i| i.column_keys())
                else {
                    continue;
                };
                (table, columns.into_iter().collect::<BTreeSet<_>>())
            }
            Change::Table(_)
            | Change::Partition(PartitionChange::DetachPartition { .. })
            | Change::Partition(PartitionChange::AttachPartition { .. })
            | Change::Column(_)
            | Change::TableProperty(_)
            | Change::Constraint(ConstraintChange::SetPrimaryKey { .. })
            | Change::Constraint(ConstraintChange::AddUnique { .. })
            | Change::Constraint(ConstraintChange::AddForeignKey { .. })
            | Change::Constraint(ConstraintChange::DropForeignKey { .. })
            | Change::Constraint(ConstraintChange::AddCheck { .. })
            | Change::Constraint(ConstraintChange::DropCheck { .. })
            | Change::Index(IndexChange::AddIndex { .. })
            | Change::Row(_)
            | Change::Module(_)
            | Change::TypeValue(_)
            | Change::Role(_)
            | Change::Type(_)
            | Change::Search(_)
            | Change::Extension(_)
            | Change::Permission(_)
            | Change::Index(IndexChange::SetIndexStorageParameters { .. })
            | Change::Partition(PartitionChange::SetPartitionDefault { .. })
            | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
            | Change::Policy(_) => continue,
        };
        // PostgreSQL can bind a permutation of a composite candidate key.
        // SQL Server requires its order; a set conservatively covers both.
        removed.insert((table.clone(), columns));
    }
    for (table, before) in &aligned {
        let Some(after) = declared.schema.tables.get(table) else {
            continue;
        };
        for (name, fk) in &before.foreign_keys {
            let Some(wanted) = after.foreign_keys.get(name) else {
                continue;
            };
            if !removed.contains(&(fk.references_table.clone(), fk.references_columns.iter().cloned().collect()))
                || changes.iter().any(|c| matches!(c, Change::Constraint(ConstraintChange::DropForeignKey { table: on, name: n }) if on == table && n == name)) {
                continue;
            }
            changes.push(Change::Constraint(ConstraintChange::DropForeignKey {
                table: table.clone(),
                name: name.clone(),
            }));
            changes.push(Change::Constraint(ConstraintChange::AddForeignKey {
                table: table.clone(),
                name: name.clone(),
                constraint: Box::new(wanted.clone()),
            }));
        }
    }
}

/// A foreign key that reads the same by name on both sides can still point at
/// two tables: across skipped revisions, the table it referenced is dropped and
/// another is renamed into that name (DEC-536.1). The standing key blocks the
/// drop, and the engine never binds it to the new occupant, so the plan drops
/// and re-adds it visibly. Identity is compared by uid, read from the baseline
/// as spelled there, not through the plan's renames.
fn rebind_foreign_keys_to_a_new_occupant(
    base: Side<'_>,
    declared: Side<'_>,
    changes: &mut Vec<Change>,
) {
    let base_uid: BTreeMap<&TableName, &pbps_model::Uid> = base
        .ids
        .tables
        .iter()
        .map(|(uid, name)| (name, uid))
        .collect();
    let declared_uid: BTreeMap<&TableName, &pbps_model::Uid> = declared
        .ids
        .tables
        .iter()
        .map(|(uid, name)| (name, uid))
        .collect();
    let mut pairs = Vec::new();
    for (uid, old_name) in &base.ids.tables {
        let (Some(new_name), Some(before)) = (
            declared.ids.tables.get(uid),
            base.schema.tables.get(old_name),
        ) else {
            continue;
        };
        let Some(after) = declared.schema.tables.get(new_name) else {
            continue;
        };
        for (name, fk) in &before.foreign_keys {
            let Some(wanted) = after.foreign_keys.get(name) else {
                continue;
            };
            // Already replaced when the diff added it back. Matched by the add,
            // not the drop: a dropped table this plan's rename takes the name
            // of emits a drop of its own key at the same address, and on
            // PostgreSQL that key can share this one's name.
            if base_uid.get(&fk.references_table) == declared_uid.get(&wanted.references_table)
                || changes.iter().any(|c| {
                    matches!(c, Change::Constraint(ConstraintChange::AddForeignKey { table, name: n, .. })
                        if table == new_name && n == name)
                })
            {
                continue;
            }
            pairs.push(Change::Constraint(ConstraintChange::DropForeignKey {
                table: new_name.clone(),
                name: name.clone(),
            }));
            pairs.push(Change::Constraint(ConstraintChange::AddForeignKey {
                table: new_name.clone(),
                name: name.clone(),
                constraint: Box::new(wanted.clone()),
            }));
        }
    }
    changes.extend(pairs);
}

/// The tables whose rows this plan writes or deletes: a tightening of one of
/// their columns runs after those rows (#1367).
fn tables_with_row_changes(changes: &[Change]) -> BTreeSet<TableName> {
    changes
        .iter()
        .filter_map(|c| row_table(c).cloned())
        .collect()
}

/// The columns a plan puts an identity on that had none (#1889).
fn identities_put_on<'a>(changes: impl Iterator<Item = &'a Change>) -> BTreeSet<ColumnRef> {
    changes
        .filter_map(|c| {
            if let Change::Column(ColumnChange::AlterColumnNumbering {
                column,
                from: ValueSource::None | ValueSource::Default(_) | ValueSource::Sequence(_),
                to: ValueSource::Identity(_),
                ..
            }) = c
            {
                Some(column.clone())
            } else {
                None
            }
        })
        .collect()
}

/// Renames, from and to, that wait on each other's names.
type RenameCycle = Vec<(TableName, TableName)>;

/// The owned sequences a plan gives up, renames, or turns into an identity
/// on a column it neither tightens nor retypes, each with its place in the
/// chain of names they free: zero for one that takes no name another frees,
/// and one more than the change freeing the name it takes otherwise. A cycle
/// of renames has no order in which each name is free when taken, so each
/// cycle is returned too, as its renames, to be refused by name (#1930
/// review).
fn sequence_release_depths<'a>(
    changes: impl Iterator<Item = &'a Change>,
    held_back: &BTreeSet<ColumnRef>,
) -> (BTreeMap<ColumnRef, usize>, BTreeSet<RenameCycle>) {
    let mut released: BTreeMap<(String, String), ColumnRef> = BTreeMap::new();
    let mut claims: BTreeMap<ColumnRef, Option<(String, String)>> = BTreeMap::new();
    let mut renamed: BTreeMap<ColumnRef, (TableName, TableName)> = BTreeMap::new();
    for c in changes {
        let Change::Column(ColumnChange::AlterColumnNumbering {
            column,
            from: ValueSource::Sequence(was),
            to,
            ..
        }) = c
        else {
            continue;
        };
        let at = |name: &String| (column.table.schema.clone(), name.clone());
        let claim = match to {
            ValueSource::None => None,
            ValueSource::Sequence(now)
                if was.name.is_some() && now.name.is_some() && was.name != now.name =>
            {
                now.name.as_ref().map(at)
            }
            ValueSource::Identity(_) if !held_back.contains(column) => None,
            ValueSource::Sequence(_)
            | ValueSource::Identity(_)
            | ValueSource::Default(_)
            | ValueSource::Generated(_) => continue,
        };
        if let Some(name) = &was.name {
            released.insert(at(name), column.clone());
            if let Some((schema, to)) = &claim {
                renamed.insert(
                    column.clone(),
                    (TableName::new(schema, name), TableName::new(schema, to)),
                );
            }
        }
        claims.insert(column.clone(), claim);
    }
    let mut depths = BTreeMap::new();
    let mut cycles = BTreeSet::new();
    for start in claims.keys() {
        let mut depth = 0;
        let mut path = vec![start];
        let mut at = start;
        while let Some(Some(name)) = claims.get(at)
            && let Some(next) = released.get(name)
        {
            if let Some(from) = path.iter().position(|seen| *seen == next) {
                // Back to a member already walked: the renames from there
                // on wait on each other. Sorted, so each cycle is one entry
                // whichever member the walk started from.
                let mut cycle: Vec<_> = path[from..]
                    .iter()
                    .filter_map(|c| renamed.get(*c).cloned())
                    .collect();
                cycle.sort();
                cycles.insert(cycle);
                break;
            }
            depth += 1;
            path.push(next);
            at = next;
        }
        depths.insert(start.clone(), depth);
    }
    (depths, cycles)
}

/// The columns a plan puts a numbering source on, an identity or an owned
/// sequence they were not numbered by (#1889).
fn numbering_put_on<'a>(changes: impl Iterator<Item = &'a Change>) -> BTreeSet<ColumnRef> {
    changes
        .filter_map(|c| {
            if let Change::Column(ColumnChange::AlterColumnNumbering {
                column, from, to, ..
            }) = c
                && pbps_model::NumberingKind::installed(from, to)
            {
                Some(column.clone())
            } else {
                None
            }
        })
        .collect()
}

/// The tables a plan puts a numbering source on and both deletes and
/// inserts rows of: the deletes may remove the rows holding NULL, or the
/// greatest value, which must go before the source is put on, and the
/// inserts leave the numbered column out, so they follow it (#1930 review).
fn numbering_after_deletes<'a>(
    changes: impl Iterator<Item = &'a Change> + Clone,
) -> BTreeSet<TableName> {
    let inserted = tables_with_inserts(changes.clone());
    let deleted: BTreeSet<TableName> = changes
        .clone()
        .filter_map(|c| {
            if let Change::Row(RowChange::DeleteRow { table, .. }) = c {
                Some(table.clone())
            } else {
                None
            }
        })
        .collect();
    numbering_put_on(changes)
        .into_iter()
        .map(|column| column.table)
        .filter(|t| inserted.contains(t) && deleted.contains(t))
        .collect()
}

/// The rows of another table that reference one whose inserts follow an
/// identity put on it, at the end of the deletes' class: they are written in
/// class 11, before the rows they may reference exist, and the foreign key
/// refuses them. Refused by name; the row changes go in a plan of their own
/// first (#1930 review).
fn refuse_rows_referencing_late_inserts(
    declared: Side<'_>,
    changes: &[Change],
    errs: &mut Vec<DiffError>,
) {
    let late = numbering_after_deletes(changes.iter());
    if late.is_empty() {
        return;
    }
    // A key the plan adds, a created table's among them, is added in class
    // 13, after every row: nothing enforces it while they are written
    // (#1930 review).
    let added: BTreeSet<(&TableName, &str)> = changes
        .iter()
        .filter_map(|c| {
            if let Change::Constraint(ConstraintChange::AddForeignKey { table, name, .. }) = c {
                Some((table, name.raw()))
            } else {
                None
            }
        })
        .collect();
    let mut seen = BTreeSet::new();
    for change in changes {
        let (Change::Row(RowChange::InsertRow { table, .. })
        | Change::Row(RowChange::UpdateRow { table, .. })) = change
        else {
            continue;
        };
        let Some(t) = declared.schema.tables.get(table) else {
            continue;
        };
        for (name, fk) in &t.foreign_keys {
            if added.contains(&(table, name.raw())) {
                continue;
            }
            if fk.references_table != *table
                && late.contains(&fk.references_table)
                && seen.insert((table.clone(), fk.references_table.clone()))
            {
                errs.push(DiffError::RowsReferenceLateIdentityRows {
                    table: table.clone(),
                    referenced: fk.references_table.clone(),
                });
            }
        }
    }
}

/// The tables a plan inserts declared rows into.
fn tables_with_inserts<'a>(changes: impl Iterator<Item = &'a Change>) -> BTreeSet<TableName> {
    changes
        .filter_map(|c| {
            if let Change::Row(RowChange::InsertRow { table, .. }) = c {
                Some(table.clone())
            } else {
                None
            }
        })
        .collect()
}

/// The table a row change writes or deletes in.
fn row_table(change: &Change) -> Option<&TableName> {
    if let Change::Row(RowChange::InsertRow { table, .. })
    | Change::Row(RowChange::UpdateRow { table, .. })
    | Change::Row(RowChange::DeleteRow { table, .. }) = change
    {
        Some(table)
    } else {
        None
    }
}

/// A NOT NULL a parent with external partitions drops (#1533) reaches every
/// partition, but on 18 one a partition set of its own outlives it
/// (DEC-1687.1). A declared partition is brought back by its own change after
/// the parent's; an external one is listed nowhere, so the parent's change
/// carries the clearing itself (DEC-1533.1, #1833 review).
fn clear_external_partitions_after_loosening(declared: &Schema, changes: &mut [Change]) {
    let external = |table: &TableName| {
        declared
            .tables
            .get(table)
            .and_then(|t| t.partition_by.as_ref())
            .is_some_and(|by| by.external)
    };
    for change in changes {
        if let Change::Column(ColumnChange::AlterColumnNullability {
            column,
            to_nullable: true,
            through_external_partitions,
            ..
        })
        | Change::Column(ColumnChange::AlterColumnType {
            column,
            from_nullable: false,
            to_nullable: true,
            through_external_partitions,
            ..
        }) = change
        {
            *through_external_partitions = external(&column.table);
        }
    }
}

/// A tightening folded into a retype comes out of it when the plan writes or
/// deletes rows of its table. The retype keeps the column nullable, and an
/// `AlterColumnNullability` tightens it after the rows, which may be what
/// fills or removes its NULLs (#1367). The same split DEC-1168.1 makes for a
/// recomputed column, for the same reason: the check has to meet the values
/// the plan leaves, not the ones it found.
///
/// Not on a column the plan puts an identity on in a table it inserts rows
/// into: the column must be NOT NULL before the identity, and the identity
/// on before the rows (#1930 review).
fn split_tightenings_from_retypes(changes: &mut Vec<Change>) {
    let rows = tables_with_row_changes(changes);
    let identified = identities_put_on(changes.iter());
    let inserted = tables_with_inserts(changes.iter());
    let late = numbering_after_deletes(changes.iter());
    let mut split = Vec::new();
    for change in changes.iter_mut() {
        if let Change::Column(ColumnChange::AlterColumnType {
            uid,
            column,
            to,
            from_nullable: true,
            to_nullable,
            to_collation,
            ..
        }) = change
            && !*to_nullable
            && rows.contains(&column.table)
            && !(identified.contains(column)
                && inserted.contains(&column.table)
                && !late.contains(&column.table))
        {
            *to_nullable = true;
            split.push(Change::Column(ColumnChange::AlterColumnNullability {
                uid: uid.clone(),
                column: column.clone(),
                ty: to.clone(),
                to_nullable: false,
                through_external_partitions: false,
                collation: to_collation.clone(),
            }));
        }
    }
    changes.extend(split);
}

/// A foreign key a type change would leave between two types the engine cannot
/// join is named here, at plan time, where the apply would otherwise take the key
/// down for the change and be refused when it put it back (#2359, DEC-2359.1).
fn refuse_unjoinable_foreign_keys(
    declared: &Schema,
    dialect: &(impl Dialect + ?Sized),
    changes: &[Change],
    errs: &mut Vec<DiffError>,
) {
    for key in crate::foreign_keys::unjoinable(changes, declared, dialect) {
        let (referencing, referencing_type) = &key.referencing;
        let (referenced, referenced_type) = &key.referenced;
        let next = match (key.referencing_retyped, key.referenced_retyped) {
            (true, true) => format!(
                "Declare types for {} and {} that a key can join, or remove the foreign key.",
                referencing.shown(),
                referenced.shown()
            ),
            (changed_is_referencing, _) => {
                let (changed, other) = if changed_is_referencing {
                    (referencing, referenced)
                } else {
                    (referenced, referencing)
                };
                let record = if dialect.records_conversions() {
                    format!(
                        " (when the engine cannot make that change by itself, record its \
                         conversion with `pbps convert {} --using -`)",
                        other.shown()
                    )
                } else {
                    String::new()
                };
                format!(
                    "Change {other} to a type a key can join with it in the same plan{record}, \
                     or declare {changed} with the type it has today, or remove the foreign key.",
                    other = other.shown(),
                    changed = changed.shown(),
                )
            }
        };
        errs.push(DiffError::ForeignKeyTypesDiffer {
            table: key.table.clone(),
            constraint: key.constraint.clone(),
            referencing: referencing.clone(),
            referenced: referenced.clone(),
            referencing_type: referencing_type.raw().to_string(),
            referenced_type: referenced_type.raw().to_string(),
            next,
        });
    }
}

/// Dependency maintenance is visible in the saved plan, including its ordinary
/// drop/add risks (DECISIONS 461). The emitter must not discover extra
/// cross-table work after approval. Align identities first so the selection survives simultaneous
/// table and column renames; explicit replacements already have their own pair.
fn recreate_retyped_dependents(
    base: Side<'_>,
    declared: Side<'_>,
    renames: &Renames,
    dialect: &(impl Dialect + ?Sized),
    changes: &mut Vec<Change>,
) {
    let also = |dependents: &mut pbps_dialect::RetypeDependents,
                more: pbps_dialect::RetypeDependents| {
        dependents.keys_and_indexes |= more.keys_and_indexes;
        dependents.checks |= more.checks;
        dependents.filtered_indexes |= more.filtered_indexes;
        dependents.foreign_keys |= more.foreign_keys;
    };
    // A column can carry two of these, a retype and the tightening split out
    // of it (#1367), so the answers are merged rather than the last kept.
    let mut retyped: BTreeMap<ColumnRef, pbps_dialect::RetypeDependents> = BTreeMap::new();
    // A computed column dropped and added again under its name, or replacing
    // an ordinary column of that name, or replaced by one: what is over the
    // name has to come down first (4922, measured on 17.0) and go back after,
    // as around a retype. No foreign key is ever over a computed column
    // (validation), and a check or a filter names it in text this does not
    // parse (#1174).
    let removed = |column: &ColumnRef| -> Option<bool> {
        changes.iter().find_map(|c| {
            if let Change::Column(ColumnChange::DropComputedColumn { table, name, .. }) = c
                && *table == column.table
                && *name == column.name
            {
                Some(true)
            } else if let Change::Column(ColumnChange::DropColumn {
                column: dropped, ..
            }) = c
                && dropped == column
            {
                Some(false)
            } else {
                None
            }
        })
    };
    let recomputed: BTreeSet<ColumnRef> = changes
        .iter()
        .filter_map(|change| {
            if let Change::Column(ColumnChange::AddComputedColumn { table, name, .. }) = change {
                Some((table.column(name), true))
            } else if let Change::Column(ColumnChange::AddColumn { table, name, .. }) = change {
                Some((table.column(name), false))
            } else {
                None
            }
        })
        .filter(|(column, computed)| removed(column).is_some_and(|was| was || *computed))
        .map(|(column, _)| column)
        .collect();
    for column in recomputed {
        also(
            retyped.entry(column).or_default(),
            pbps_dialect::RetypeDependents {
                keys_and_indexes: true,
                checks: true,
                filtered_indexes: true,
                foreign_keys: false,
            },
        );
    }
    for (column, dependents) in changes.iter().filter_map(|change| match change {
        Change::Partition(PartitionChange::RebuildPartitionedTable(_))
        | Change::Index(IndexChange::BuildPartitionedIndex(_))
        | Change::Partition(PartitionChange::CopyTableRows(_))
        | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => None,
        Change::Column(ColumnChange::AlterColumnType {
            column,
            from,
            to,
            from_nullable,
            to_nullable,
            from_collation,
            to_collation,
            ..
        }) => {
            // A nullability change in the same statement brings its own
            // blockers, which a type change alone may not have (#1363).
            let dependents = pbps_dialect::RetypeDependents::of_retype(
                dialect,
                (from, to),
                from_collation != to_collation,
                (from_nullable != to_nullable).then_some(*to_nullable),
            );
            Some((column.clone(), dependents))
        }
        Change::Column(ColumnChange::AlterColumnNullability {
            column,
            to_nullable,
            ..
        }) => Some((column.clone(), dialect.nullability_dependents(*to_nullable))),
        // A conversion by name leaves constants of the old type in checks,
        // keys and partial indexes that the replacement cannot be compared
        // with. The engine's own assignment cast of a domain's base change
        // keeps them (measured on 16 and 18, DEC-1635.9), so nothing comes
        // down for it.
        Change::TypeValue(TypeValueChange::EnumRebuild {
            step: pbps_model::EnumRebuildStep::Retype { column, values, .. },
            ..
        }) => Some((
            column.clone(),
            match values.as_ref() {
                pbps_model::ValueChange::Relabel { .. } => dialect.enum_rebuild_dependents(),
                pbps_model::ValueChange::Cast { .. } => Default::default(),
            },
        )),
        Change::Table(_)
        | Change::Partition(PartitionChange::DetachPartition { .. })
        | Change::Partition(PartitionChange::AttachPartition { .. })
        | Change::Column(ColumnChange::AddColumn { .. })
        | Change::Column(ColumnChange::DropColumn { .. })
        | Change::Column(ColumnChange::RenameColumn { .. })
        | Change::Column(ColumnChange::AlterColumnDefault { .. })
        | Change::Column(ColumnChange::AlterColumnExpression { .. })
        | Change::Column(ColumnChange::AlterColumnNumbering { .. })
        | Change::Column(ColumnChange::SetColumnDeprecated { .. })
        | Change::TableProperty(_)
        | Change::Constraint(_)
        | Change::Index(IndexChange::SetIndexStorageParameters { .. })
        | Change::Partition(PartitionChange::SetPartitionDefault { .. })
        | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
        | Change::Policy(_)
        | Change::Index(IndexChange::AddIndex { .. })
        | Change::Column(ColumnChange::AddComputedColumn { .. })
        | Change::Column(ColumnChange::DropComputedColumn { .. })
        | Change::Index(IndexChange::DropIndex { .. })
        | Change::Row(_)
        | Change::Module(_)
        | Change::TypeValue(_)
        | Change::Role(_)
        | Change::Type(_)
        | Change::Search(_)
        | Change::Extension(_)
        | Change::Permission(_) => None,
    }) {
        let merged = retyped.entry(column).or_default();
        also(merged, dependents);
    }
    if retyped.is_empty() {
        return;
    }
    // A partition keeps its own CHECKs and filtered indexes over the columns
    // it takes from its parent, and the parent's retype reaches them.
    for (column, dependents) in retyped.clone() {
        let mut parents = vec![column.table.clone()];
        let mut reached = BTreeSet::from([column.table.clone()]);
        while let Some(parent) = parents.pop() {
            for (name, table) in &declared.schema.tables {
                if table
                    .partition_of
                    .as_ref()
                    .is_some_and(|of| of.parent == parent)
                    && reached.insert(name.clone())
                {
                    also(
                        retyped.entry(name.column(&column.name)).or_default(),
                        dependents,
                    );
                    parents.push(name.clone());
                }
            }
        }
    }
    for (uid, old_name) in &base.ids.tables {
        let Some(name) = declared.ids.tables.get(uid) else {
            continue;
        };
        let (Some(old), Some(after)) = (
            base.schema.tables.get(old_name),
            declared.schema.tables.get(name),
        ) else {
            continue;
        };
        let before = renames.apply(old, old_name);
        let key_column = |column: &ColumnName| {
            retyped
                .get(&name.column(column))
                .is_some_and(|d| d.keys_and_indexes)
        };
        let checks = retyped.iter().any(|(c, d)| &c.table == name && d.checks);
        let filters = retyped
            .iter()
            .any(|(c, d)| &c.table == name && d.filtered_indexes);
        if let (Some(key), Some(wanted)) = (&before.primary_key, &after.primary_key)
            && key.columns.iter().any(&key_column)
            && !changes
                .iter()
                .any(|c| matches!(c, Change::Constraint(ConstraintChange::SetPrimaryKey { table, .. }) if table == name))
        {
            changes.push(Change::Constraint(ConstraintChange::SetPrimaryKey {
                table: name.clone(),
                from: Some(key.clone()),
                to: None,
                nonclustered: false,
            }));
            changes.push(Change::Constraint(ConstraintChange::SetPrimaryKey {
                table: name.clone(),
                from: None,
                to: Some(wanted.clone()),
                nonclustered: !after.primary_key_is_clustered(),
            }));
        }
        // Only a constraint or index whose layout stays as it was: one whose
        // layout moves is already dropped and re-added by `diff_constraints`,
        // and a second pair here would drop it twice.
        for (n, key) in &before.unique {
            if after.unique.get(n) == Some(key)
                && before.unique_is_clustered(n.raw()) == after.unique_is_clustered(n.raw())
                && key.columns.iter().any(&key_column)
            {
                changes.push(Change::Constraint(ConstraintChange::DropUnique {
                    table: name.clone(),
                    name: n.clone(),
                }));
                changes.push(Change::Constraint(ConstraintChange::AddUnique {
                    table: name.clone(),
                    name: n.clone(),
                    constraint: key.clone(),
                    clustered: after.unique_is_clustered(n.raw()),
                }));
            }
        }
        for (n, check) in &before.checks {
            if checks && after.checks.get(n) == Some(check) {
                changes.push(Change::Constraint(ConstraintChange::DropCheck {
                    table: name.clone(),
                    name: n.clone(),
                }));
                changes.push(Change::Constraint(ConstraintChange::AddCheck {
                    table: name.clone(),
                    name: n.clone(),
                    constraint: check.clone(),
                }));
            }
        }
        for (n, index) in &before.indexes {
            if after.indexes.get(n) == Some(index)
                && before.index_is_clustered(n.raw()) == after.index_is_clustered(n.raw())
                && (index
                    .columns
                    .iter()
                    .any(|c| c.key.column().is_some_and(key_column))
                    || index.include.iter().any(&key_column)
                    // An expression names its columns inside text this does
                    // not parse, so it is treated as a filter is: rebuilt
                    // wherever the dialect rebuilds filtered indexes
                    // (DEC-1169.2).
                    || (filters
                        && (index.filter.is_some()
                            || index.columns.iter().any(|c| c.key.expression().is_some()))))
            {
                changes.push(Change::Index(IndexChange::DropIndex {
                    table: name.clone(),
                    name: n.clone(),
                }));
                changes.push(Change::Index(IndexChange::AddIndex {
                    table: name.clone(),
                    name: n.clone(),
                    index: Box::new(index.clone()),
                    clustered: after.index_is_clustered(n.raw()),
                }));
            }
        }
        for (n, fk) in &before.foreign_keys {
            let local = fk.columns.iter().map(|c| name.column(c));
            let referenced = fk
                .references_columns
                .iter()
                .map(|c| fk.references_table.column(c));
            if after.foreign_keys.get(n) == Some(fk)
                && local
                    .chain(referenced)
                    .any(|c| retyped.get(&c).is_some_and(|d| d.foreign_keys))
            {
                changes.push(Change::Constraint(ConstraintChange::DropForeignKey {
                    table: name.clone(),
                    name: n.clone(),
                }));
                changes.push(Change::Constraint(ConstraintChange::AddForeignKey {
                    table: name.clone(),
                    name: n.clone(),
                    constraint: Box::new(fk.clone()),
                }));
            }
        }
    }
}

/// The column a change renames, drops, retypes or tightens or relaxes on
/// `table`, under the name its expression would use, and what the change
/// does to it. Every one is an `ALTER COLUMN` or an `sp_rename` the engine
/// refuses on a computed column's input (#1174).
fn input_change<'a>(change: &'a Change, table: &TableName) -> Option<(&'a str, &'static str)> {
    if let Change::Column(ColumnChange::RenameColumn { table: t, from, .. }) = change
        && t == table
    {
        return Some((from.raw(), "renames"));
    }
    if let Change::Column(ColumnChange::DropColumn { column, .. }) = change
        && column.table == *table
    {
        return Some((column.name.raw(), "drops"));
    }
    if let Change::Column(ColumnChange::AlterColumnType { column, .. }) = change
        && column.table == *table
    {
        return Some((column.name.raw(), "retypes or recollates"));
    }
    if let Change::Column(ColumnChange::AlterColumnNullability { column, .. }) = change
        && column.table == *table
    {
        return Some((column.name.raw(), "changes the nullability of"));
    }
    None
}

/// The plans a standing SQL Server computed column cannot be part of, each
/// refused by name (DEC-1174.1):
///
/// - a rename, drop, retype or nullability change of a column its expression
///   may read: 15336 and 4922, measured on 17.0;
/// - an alter or drop of a module it may call: 3729, even without
///   SCHEMABINDING;
/// - and, for one this plan adds, a module this plan creates, which the
///   ordering puts after it (class 14 after class 9).
///
/// "May" by [`Dialect::may_name`], which over-approximates. A computed column
/// this plan drops, or changes and so drops and re-adds, is out of the way
/// before any of these and is not standing.
fn refuse_temporal_changes(
    base: Side<'_>,
    declared: Side<'_>,
    changes: &[Change],
    errs: &mut Vec<DiffError>,
) {
    let temporal = |schema: &Schema, name: &TableName| {
        schema
            .tables
            .get(name)
            .is_some_and(|t| t.system_time.is_some())
    };
    let mut refused: BTreeMap<TableName, Vec<String>> = BTreeMap::new();
    let mut refuse = |table: &TableName, what: String| {
        let list = refused.entry(table.clone()).or_default();
        if !list.contains(&what) {
            list.push(what);
        }
    };
    // A difference in `system_time` itself, which no change carries: without
    // this it would plan nothing and record the declaration as applied.
    for (uid, declared_name) in &declared.ids.tables {
        let Some(base_name) = base.ids.tables.get(uid) else {
            continue;
        };
        if let (Some(b), Some(d)) = (
            base.schema.tables.get(base_name),
            declared.schema.tables.get(declared_name),
        ) && b.system_time != d.system_time
        {
            refuse(declared_name, "change its `system_time`".to_owned());
        }
    }
    // A table this plan creates is created with every change it splits out
    // of the `CREATE` (its foreign keys, #1501 review): those are the
    // creation, not a change to a table that stands. By the change, not by
    // the base: a new table may take the name of one the plan renames or
    // drops. Whatever drops or renames a table at that name is still asked
    // about below, so a temporal table dropped and recreated under its own
    // name is still refused.
    let created: BTreeSet<&TableName> = changes
        .iter()
        .filter_map(|c| {
            if let Change::Table(TableChange::CreateTable { name, .. }) = c {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    // A grant or a trigger on the table is not a change to the pair, and
    // `table()` names neither; a rename is reached by both of its names.
    for change in changes {
        let ends_a_table = matches!(
            change,
            Change::Table(TableChange::DropTable { .. })
                | Change::Table(TableChange::RenameTable { .. })
        );
        if matches!(change, Change::Table(TableChange::CreateTable { .. }))
            || (!ends_a_table && change.table().is_some_and(|t| created.contains(t)))
        {
            continue;
        }
        // A nullable column the engine adds to the history beside the table,
        // NULL in every row of both, with versioning left on (DEC-1177.1,
        // measured on 17.0). A period column is declared through
        // `system_time`, whose difference is refused above.
        if let Change::Column(ColumnChange::AddColumn { column, .. }) = change
            && column.nullable
        {
            continue;
        }
        // A drop or a rename acts on the table the base has at that name, and
        // a rename brings it to the declared one: asked of those sides, so the
        // ordinary table a new temporal one replaces may still go.
        let asked: Vec<&TableName> =
            if let Change::Table(TableChange::DropTable { name, .. }) = change {
                vec![name]
                    .into_iter()
                    .filter(|n| temporal(base.schema, n))
                    .collect()
            } else if let Change::Table(TableChange::RenameTable { from, to, .. }) = change {
                [(from, base.schema), (to, declared.schema)]
                    .into_iter()
                    .filter(|(n, side)| temporal(side, n))
                    .map(|(n, _)| n)
                    .collect()
            } else {
                change
                    .table()
                    .into_iter()
                    .filter(|n| temporal(base.schema, n) || temporal(declared.schema, n))
                    .collect()
            };
        // The one kind admitted is refused only for its nullability: say so.
        let what = if matches!(change, Change::Column(ColumnChange::AddColumn { .. })) {
            "add a NOT NULL column".to_owned()
        } else {
            change_in_words(change)
        };
        for table in asked {
            refuse(table, what.clone());
        }
    }
    errs.extend(
        refused
            .into_iter()
            .map(|(table, what)| DiffError::TemporalTableChange { table, what }),
    );
}

/// What each of `parent`'s keys, constraints and indexes is called on the
/// partition declared as `declared` once detached, or what keeps the
/// declaration from being the shape a detach gives it: the parent's columns
/// in order, every key, constraint and index matched one to one by
/// definition, its name aside (#1544, DEC-1544.1), and the checks, indexes
/// (#1577) and foreign keys (#1632) `partition` has of its own, which a
/// detach leaves as they are, under their own names.
fn detached_names(
    parent: &Table,
    partition: &Table,
    declared: &Table,
    dialect: &(impl Dialect + ?Sized),
    pending_columns: bool,
) -> Result<Vec<DetachedName>, Vec<String>> {
    let mut names = Vec::new();
    let mut what = Vec::new();
    // Its clone of the parent's exclusion becomes its own on a detach, under a name
    // the engine chose (measured on 18.6), and no recipe names it yet (DEC-2448.6).
    if !parent.exclude.is_empty() {
        what.push(format!(
            "its parent holds exclusion constraint(s) {}, whose clone a detach would keep as \
             an own constraint under the engine's name; detaching a partition of such a parent \
             is not supported yet (DEC-2448.6, #2448)",
            parent
                .exclude
                .keys()
                .map(|n| format!("`{}`", n.shown()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    // Its own are matched by name before the parent's by definition: a
    // detach renames only what it gave the partition, so an own check or
    // index declared under another name, or changed, is a change this plan
    // does not make (#1581).
    // Its columns are its parent's with what it holds of them otherwise
    // (#1578): a detach keeps its own defaults and NOT NULLs. Its persistence
    // and storage parameters are its own, the parent having none (#1580),
    // and a detach keeps them too.
    let mut parent = parent.clone();
    parent.unlogged = partition.unlogged;
    parent
        .storage_parameters
        .clone_from(&partition.storage_parameters);
    if let Some(of) = &partition.partition_of {
        for (column, own) in &of.columns {
            if let Some(c) = parent.columns.get_mut(column) {
                if let Some(default) = &own.default {
                    c.value_source = pbps_model::ValueSource::Default(default.clone());
                }
                if own.not_null {
                    c.nullable = false;
                }
            }
        }
    }
    let parent = &parent;
    let mut declared = declared.clone();
    for (name, own) in &partition.checks {
        if declared.checks.get(name) == Some(own) {
            declared.checks.remove(name);
        } else {
            what.push(format!(
                "its own check `{name}` is not declared as it stands",
                name = name.shown(),
            ));
        }
    }
    for (name, own) in &partition.indexes {
        if declared.indexes.get(name) == Some(own) {
            declared.indexes.remove(name);
        } else {
            what.push(format!(
                "its own index `{name}` is not declared as it stands",
                name = name.shown(),
            ));
        }
    }
    // Its own foreign keys, as its own checks (#1632): measured on 16 and
    // 18, a detach keeps them under their names beside the parent's clones,
    // which it makes local.
    for (name, own) in &partition.foreign_keys {
        if declared.foreign_keys.get(name) == Some(own) {
            declared.foreign_keys.remove(name);
        } else {
            what.push(format!(
                "its own foreign key `{name}` is not declared as it stands",
                name = name.shown(),
            ));
        }
    }
    let declared = &declared;
    // Types in the engine's spelling, as `diff_columns` compares them: the
    // parent's base reads `integer` back where its file says `int`. A
    // description is prose `diff` does not compare, and a connected base
    // never holds one, so it is not compared here either.
    let columns = |t: &Table| -> Vec<(ColumnName, pbps_model::Column)> {
        t.columns
            .iter()
            .map(|(name, c)| {
                let ty = dialect
                    .normalize_type(&c.ty)
                    .unwrap_or_else(|_| c.ty.clone());
                let column = pbps_model::Column {
                    ty,
                    description: None,
                    ..c.clone()
                };
                (name.clone(), column)
            })
            .collect()
    };
    if !pending_columns && columns(parent) != columns(declared) {
        what.push("its columns are not its parent's, in its parent's order".to_owned());
    }
    match (&parent.primary_key, &declared.primary_key) {
        (None, None) => {}
        (Some(from), Some(to))
            if from.columns == to.columns && from.storage_parameters == to.storage_parameters =>
        {
            names.push(DetachedName {
                kind: DetachedKind::PrimaryKey,
                parent: from.name.clone().unwrap_or_default(),
                name: to.name.clone(),
            });
        }
        (Some(_), _) | (None, Some(_)) => {
            what.push("its primary key is not its parent's".to_owned());
        }
    }
    fn matched<V: PartialEq>(
        kind: DetachedKind,
        words: &str,
        parent: &BTreeMap<pbps_model::ObjectLocalName, V>,
        declared: &BTreeMap<pbps_model::ObjectLocalName, V>,
        names: &mut Vec<DetachedName>,
        what: &mut Vec<String>,
    ) {
        let mut unused: Vec<(&pbps_model::ObjectLocalName, &V)> = declared.iter().collect();
        for (from, definition) in parent {
            match unused.iter().position(|(_, d)| *d == definition) {
                Some(at) => {
                    let (to, _) = unused.remove(at);
                    names.push(DetachedName {
                        kind,
                        parent: from.clone(),
                        name: Some(to.clone()),
                    });
                }
                None => what.push(format!(
                    "its parent's {words} `{from}` is missing",
                    from = from.shown(),
                )),
            }
        }
        for (to, _) in unused {
            what.push(format!(
                "the {words} `{to}` is not its parent's",
                to = to.shown(),
            ));
        }
    }
    matched(
        DetachedKind::Unique,
        "unique constraint",
        &parent.unique,
        &declared.unique,
        &mut names,
        &mut what,
    );
    matched(
        DetachedKind::ForeignKey,
        "foreign key",
        &parent.foreign_keys,
        &declared.foreign_keys,
        &mut names,
        &mut what,
    );
    matched(
        DetachedKind::Check,
        "check",
        &parent.checks,
        &declared.checks,
        &mut names,
        &mut what,
    );
    matched(
        DetachedKind::Index,
        "index",
        &parent.indexes,
        &declared.indexes,
        &mut names,
        &mut what,
    );
    // Everything else is the parent's too, the partitioning aside.
    let rest = |t: &Table| Table {
        columns: Default::default(),
        primary_key: None,
        unique: Default::default(),
        foreign_keys: Default::default(),
        checks: Default::default(),
        indexes: Default::default(),
        partition_by: None,
        partition_of: None,
        description: None,
        ..t.clone()
    };
    if rest(parent) != rest(declared) {
        what.push(
            "it declares a setting or `data:` that is neither its parent's nor its own".to_owned(),
        );
    }
    if what.is_empty() {
        Ok(names)
    } else {
        Err(what)
    }
}

/// Column facts only: annotations and dependents do not make a meeting (DEC-2296.10).
fn parent_shape_changes(old: &Table, new: &Table) -> bool {
    old.computed != new.computed
        || !old
            .columns
            .iter()
            .map(|(n, c)| (n, &c.ty, &c.collation, c.nullable, &c.value_source))
            .eq(new
                .columns
                .iter()
                .map(|(n, c)| (n, &c.ty, &c.collation, c.nullable, &c.value_source)))
}

/// A table being attached as declared (#1545): itself as a partition the
/// moment the attach has run, holding only what the engine leaves it of its
/// own, and its columns without a default where the parent's has one.
struct Attached {
    table: Table,
    defaultless: Vec<ColumnName>,
    /// The table's indexes dropped before the attach: every one the engine
    /// could adopt but for the one left to it.
    displaced: Vec<pbps_model::ObjectLocalName>,
    /// The table's key and unique constraints dropped before the attach,
    /// whose index a plain unique index of the parent's could take.
    released: Vec<Change>,
}

/// What the ordinary table `table` is once attached to `of.parent`, or what
/// keeps it from being attached into a tree the model holds (#1545,
/// DEC-1545.1). Measured on 16 and 18:
///
/// - its columns must be its parent's, in its parent's order, with the same
///   types, collations, identities and generations, and NOT NULL wherever
///   the parent's are, or the engine refuses the attach; the order is the
///   reader's, which leaves a tree out over a partition in another;
/// - each of the parent's checks must be on it under the same name, and
///   becomes the inherited copy; the rest of its checks stay its own;
/// - an index, a key or a foreign key matching one of the parent's is
///   adopted as its clone, and the engine builds whichever is missing; a key
///   or a foreign key of its own would stay its own, which a partition does
///   not hold yet, while an index of its own stays as one;
/// - its defaults stay, and none of the parent's is given to a column
///   without one.
#[allow(clippy::too_many_arguments)]
fn attached(
    base: Side<'_>,
    base_name: &TableName,
    declared_name: &TableName,
    table: &Table,
    declared: &Table,
    of: &pbps_model::PartitionOf,
    dialect: &(impl Dialect + ?Sized),
    read_back: Option<&Schema>,
    boundary: Option<(&TableName, bool, &Table)>,
) -> Result<Attached, Vec<String>> {
    let mut what = Vec::new();
    let parent_name = boundary.map_or(&of.parent, |(name, _, _)| name);
    let pending_columns = boundary.is_some_and(|(_, pending, _)| pending);
    let goal_parent = boundary.map(|(_, _, goal)| goal);
    let Some(parent) = base
        .schema
        .tables
        .get(parent_name)
        .filter(|p| p.partition_by.is_some() && p.partition_of.is_none())
    else {
        return Err(vec![format!(
            "its parent {} is not a partitioned table before this plan; create the partition \
             with its parent, or attach the table in a later plan",
            of.parent.shown()
        )]);
    };
    if base_name != declared_name {
        what.push(format!(
            "it is renamed from {base_name} in the same plan; rename it in one plan and attach it \
             in another",
            base_name = base_name.shown()
        ));
    }
    if table.partition_by.is_some() {
        what.push("it is partitioned itself".to_owned());
    }
    // Types in the engine's spelling, as `diff_columns` compares them. A
    // default and NOT NULL may be its own, and are compared below.
    let columns = |t: &Table, is_parent: bool| -> Vec<(ColumnName, pbps_model::Column)> {
        t.columns
            .iter()
            .map(|(name, c)| {
                let ty = dialect
                    .normalize_type(&c.ty)
                    .unwrap_or_else(|_| c.ty.clone());
                // A deprecation is an annotation the catalog does not
                // hold, as a description is.
                let column = pbps_model::Column {
                    ty,
                    nullable: true,
                    value_source: match &c.value_source {
                        ValueSource::Default(_) | ValueSource::None => ValueSource::None,
                        // A partition takes its parent's identity. Its own is
                        // refused separately below (DEC-1681.2).
                        ValueSource::Identity(_) => ValueSource::None,
                        // The parent's serial is a default the partition
                        // copies, not a sequence the attached table owns.
                        // An ordinary table's own sequence still differs.
                        ValueSource::Sequence(_) if is_parent => ValueSource::None,
                        ValueSource::Sequence(sequence) => ValueSource::Sequence(sequence.clone()),
                        ValueSource::Generated(generated) => {
                            ValueSource::Generated(generated.clone())
                        }
                    },
                    description: None,
                    deprecated: None,
                    ..c.clone()
                };
                (name.clone(), column)
            })
            .collect()
    };
    // The engine matches a check and a generation expression on what they
    // parse to, which the recorded texts do not show: `n>0` and `n > 0` are
    // one. A connected plan has the engine's own spelling of both standing
    // objects, under the overlay of the recorded texts, and compares that;
    // the recorded texts stay the shape's. Offline there is no such spelling,
    // so the texts are left out and the two are matched by name and kind:
    // an offline plan is never applied (SPEC §7.3), and refusing there would
    // also keep it from writing the identities the connected plan needs
    // (#1642 review).
    let spelled = |t: &Table, name: &TableName| -> Table {
        let mut t = t.clone();
        let read = read_back.map(|r| r.tables.get(name));
        for (check, spec) in &mut t.checks {
            match read {
                Some(read) => {
                    if let Some(engine) = read.and_then(|r| r.checks.get(check)) {
                        spec.expression = engine.expression.clone();
                    }
                }
                None => spec.expression.clear(),
            }
        }
        for (column, spec) in &mut t.columns {
            let generated = match &mut spec.value_source {
                ValueSource::Generated(generated) => generated,
                ValueSource::None
                | ValueSource::Default(_)
                | ValueSource::Identity(_)
                | ValueSource::Sequence(_) => continue,
            };
            match read {
                Some(read) => {
                    if let Some(engine) = read.and_then(|r| r.columns.get(column)).and_then(|c| {
                        match &c.value_source {
                            ValueSource::Generated(value) => Some(value),
                            ValueSource::None
                            | ValueSource::Default(_)
                            | ValueSource::Identity(_)
                            | ValueSource::Sequence(_) => None,
                        }
                    }) {
                        generated.expression = engine.expression.clone();
                    }
                }
                None => generated.expression.clear(),
            }
        }
        t
    };
    let (table_spelled, parent_spelled) = (spelled(table, base_name), spelled(parent, parent_name));
    let (mine, theirs) = (
        columns(&table_spelled, false),
        columns(&parent_spelled, true),
    );
    let names = |c: &[(ColumnName, pbps_model::Column)]| {
        c.iter()
            .map(|(n, _)| n.shown().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    // The names themselves, not their joined spelling, which two lists can
    // share when a name holds `, ` (#1642 review).
    if !pending_columns
        && !mine
            .iter()
            .map(|(n, _)| n)
            .eq(theirs.iter().map(|(n, _)| n))
    {
        what.push(format!(
            "its columns are ({}), and its parent's are ({}), in that order",
            names(&mine),
            names(&theirs)
        ));
    } else if !pending_columns {
        for ((name, a), (_, b)) in mine.iter().zip(&theirs) {
            if a != b {
                what.push(format!(
                    "its column `{name}` is not its parent's in type, collation or \
                     generation",
                    name = name.shown(),
                ));
            } else if (match &a.value_source {
                ValueSource::Generated(_) => true,
                ValueSource::None
                | ValueSource::Default(_)
                | ValueSource::Identity(_)
                | ValueSource::Sequence(_) => false,
            }) && base_name.schema != of.parent.schema
            {
                // The engine does not compare generation expressions as it
                // attaches, and keeps the table's: in another schema the same
                // text may call another function, and each partition would
                // then compute the column its own way (measured on 18, #1642
                // review).
                what.push(format!(
                    "its column `{name}` is generated, and in another schema than its parent \
                     the same expression may call another function; move the table to its \
                     parent's schema first",
                    name = name.shown(),
                ));
            }
        }
    }
    if pending_columns
        && (base_name.schema != parent_name.schema || base_name.schema != of.parent.schema)
        && table
            .columns
            .values()
            .any(|c| matches!(c.value_source, ValueSource::Generated(_)))
    {
        what.push(
            "a generated column is in another schema; move the table to its parent's schema first"
                .into(),
        );
    }
    let mut own_columns = BTreeMap::new();
    let mut defaultless = Vec::new();
    for (name, c) in &table.columns {
        if matches!(c.value_source, ValueSource::Identity(_)) {
            what.push(format!(
                "its column `{name}` has its own identity; drop the identity in an earlier plan \
                 before attaching it",
                name = name.shown(),
            ));
        }
        let p = parent.columns.get(name);
        if p.is_none() && !pending_columns {
            continue;
        }
        if !pending_columns && c.nullable && p.is_some_and(|p| !p.nullable) {
            what.push(format!(
                "its column `{name}` is nullable, and its parent's is NOT NULL",
                name = name.shown(),
            ));
        }
        let own = pbps_model::PartitionColumn {
            // The same text as the parent's is the parent's, as the reader
            // reads it back, but only in the parent's schema: elsewhere an
            // unqualified name in it may be another schema's object, so it
            // is the table's own, and taking the parent's back sets the
            // parent's under the parent's path (DEC-1581.1).
            default: (match &c.value_source {
                ValueSource::Default(value) => Some(value.clone()),
                ValueSource::None
                | ValueSource::Identity(_)
                | ValueSource::Sequence(_)
                | ValueSource::Generated(_) => None,
            })
            .filter(|d| {
                pending_columns
                    || base_name.schema != of.parent.schema
                    || Some(d)
                        != (match p.map(|p| &p.value_source) {
                            Some(ValueSource::Default(value)) => Some(value),
                            None
                            | Some(
                                ValueSource::None
                                | ValueSource::Identity(_)
                                | ValueSource::Sequence(_)
                                | ValueSource::Generated(_),
                            ) => None,
                        })
            }),
            // This is the managed comparison template, not an ATTACH result.
            // The symbolic merge independently proves the actual local flag.
            not_null: !c.nullable
                && (if pending_columns {
                    goal_parent
                        .and_then(|p| p.columns.get(name))
                        .is_some_and(|p| p.nullable)
                } else {
                    p.is_some_and(|p| p.nullable)
                }),
        };
        if (match &c.value_source {
            ValueSource::Default(_) => false,
            ValueSource::None
            | ValueSource::Identity(_)
            | ValueSource::Sequence(_)
            | ValueSource::Generated(_) => true,
        }) && (match (if pending_columns {
            goal_parent.and_then(|p| p.columns.get(name))
        } else {
            p
        })
        .map(|p| &p.value_source)
        {
            Some(ValueSource::Default(_) | ValueSource::Sequence(_)) => true,
            None
            | Some(ValueSource::None | ValueSource::Identity(_) | ValueSource::Generated(_)) => {
                false
            }
        }) {
            defaultless.push(name.clone());
        }
        if own != pbps_model::PartitionColumn::default() {
            own_columns.insert(name.clone(), own);
        }
    }
    match (&table.primary_key, &parent.primary_key) {
        (None, _) => {}
        (Some(own), Some(p)) if own.columns == p.columns => {}
        (Some(_), _) => what.push("its primary key is not its parent's".to_owned()),
    }
    // One to one: a second match would stay its own.
    fn unmatched<'a, V: PartialEq>(
        own: &'a BTreeMap<pbps_model::ObjectLocalName, V>,
        parent: &BTreeMap<pbps_model::ObjectLocalName, V>,
    ) -> Vec<&'a pbps_model::ObjectLocalName> {
        let mut free: Vec<&V> = parent.values().collect();
        own.iter()
            .filter(|(_, d)| match free.iter().position(|p| p == d) {
                Some(at) => {
                    free.remove(at);
                    false
                }
                None => true,
            })
            .map(|(n, _)| n)
            .collect()
    }
    // By definition, storage parameters aside: measured on 16 and 18, the
    // engine adopts a key or an index whatever its own, which it keeps, and a
    // clone's are not held.
    let unique =
        |t: &Table| -> BTreeMap<pbps_model::ObjectLocalName, pbps_model::UniqueConstraint> {
            t.unique
                .iter()
                .map(|(n, u)| {
                    let mut u = u.clone();
                    u.storage_parameters.clear();
                    (n.clone(), u)
                })
                .collect()
        };
    // An index as the engine matches it to adopt it: storage parameters,
    // a key's class options and sort order aside, measured on 16 and 18
    // (`DESC` and `NULLS FIRST` adopted, another collation, operator class,
    // `INCLUDE` or method not). A BRIN key's `values_per_range` of 32 under
    // a parent's 16 is adopted all the same, and the clone keeps its 32
    // (DEC-2449.2), so the options are not part of the match: left in, an
    // own index declared over such a match would not be seen as adopted, and
    // the attach would turn it into the parent's clone and leave the declared
    // one absent.
    let index = |t: &Table| -> BTreeMap<pbps_model::ObjectLocalName, Index> {
        t.indexes
            .iter()
            .map(|(n, i)| {
                let mut i = i.clone();
                i.storage_parameters.clear();
                for c in &mut i.columns {
                    c.descending = false;
                    c.opclass_parameters.clear();
                }
                (n.clone(), i)
            })
            .collect()
    };
    for name in unmatched(&unique(table), &unique(parent)) {
        what.push(format!(
            "its unique constraint `{name}` is not its parent's, and a partition holds none of \
             its own yet",
            name = name.shown(),
        ));
    }
    // Measured on 16 and 18: one equal to a parent's is taken as the
    // parent's clone, and every other stays the partition's own, under its
    // name (#1632). The plan then compares those with what the partition
    // declares of its own, as for any table. Validation keeps the partition
    // from declaring one equal to a parent's, so none of those is its own.
    let own_foreign_keys: BTreeMap<pbps_model::ObjectLocalName, pbps_model::ForeignKey> = table
        .foreign_keys
        .iter()
        .filter(|(_, fk)| !parent.foreign_keys.values().any(|p| p == *fk))
        .map(|(name, fk)| (name.clone(), fk.clone()))
        .collect();
    let mut checks = table.checks.clone();
    for (name, p) in &parent_spelled.checks {
        checks.remove(name);
        if table_spelled.checks.get(name) != Some(p) {
            what.push(format!(
                "its parent's check `{name}` is not on it under that name, which the engine \
                 requires",
                name = name.shown(),
            ));
        }
    }
    // Which of several matches the engine adopts is the first by oid, which
    // no plan knows (`RelationGetIndexList`). So at most one is left for
    // each of the parent's indexes, and one the declaration does not keep
    // as its own; the others are dropped before the attach. An adopted
    // index keeps its name as the clone, so one whose name the declaration
    // gives an index of its own is never the one left (#1642 review).
    let (own_indexes, parents_indexes) = (index(table), index(parent));
    let mut taken: BTreeSet<&pbps_model::ObjectLocalName> = BTreeSet::new();
    let mut displaced = Vec::new();
    // An expression or a filter is matched by the engine on what it is
    // bound to and how it parses, which its text does not say: `n+1` and
    // `n + 1` are one, and in another schema the same text may call another
    // function. So every such index is dropped before the attach, and one
    // the declaration keeps added after it, in any schema (#1642 review).
    for (name, index) in &table.indexes {
        if index.filter.is_some()
            || index
                .columns
                .iter()
                .any(|c| matches!(c.key, pbps_model::IndexKey::Expression(_)))
        {
            taken.insert(name);
            displaced.push(name.clone());
        }
    }
    for definition in parents_indexes.values() {
        let matches: Vec<&pbps_model::ObjectLocalName> = own_indexes
            .iter()
            .filter(|(n, d)| *d == definition && !taken.contains(n))
            .map(|(n, _)| n)
            .collect();
        let left = matches
            .iter()
            .find(|n| !declared.indexes.contains_key(n.raw()))
            .copied();
        for n in matches {
            taken.insert(n);
            if Some(n) != left {
                displaced.push(n.clone());
            }
        }
    }
    let indexes = table
        .indexes
        .iter()
        .filter(|(n, _)| !taken.contains(n))
        .map(|(n, i)| (n.clone(), i.clone()))
        .collect();
    // A parent's plain index adopts a matching index whether or not a
    // constraint stands on it, while a parent's key or unique constraint
    // adopts only one that has one. Measured on 16 and 18, a parent's
    // `UNIQUE INDEX (a)` made before its `UNIQUE (a)` takes the index of the
    // table's `UNIQUE (a)`, the constraint stays the table's own, and the
    // parent's constraint is built a clone beside it: a tree the reader
    // refuses. Which comes first is the oid order no plan knows, so the
    // table's key or unique constraint such an index could take is dropped
    // before the attach, and the engine builds both clones (#1642 review).
    let backing = |columns: &[ColumnName]| Index {
        columns: columns
            .iter()
            .map(|c| pbps_model::IndexColumn {
                key: pbps_model::IndexKey::Column(c.clone()),
                descending: false,
                opclass: None,
                collation: None,
                opclass_parameters: Default::default(),
            })
            .collect(),
        include: Vec::new(),
        unique: true,
        filter: None,
        method: Default::default(),
        storage_parameters: Default::default(),
        filegroup: None,
    };
    let contested =
        |columns: &[ColumnName]| parents_indexes.values().any(|i| *i == backing(columns));
    let mut released = Vec::new();
    if let Some(key) = &table.primary_key
        && contested(&key.columns)
    {
        released.push(Change::Constraint(ConstraintChange::SetPrimaryKey {
            table: declared_name.clone(),
            from: Some(key.clone()),
            to: None,
            nonclustered: false,
        }));
    }
    for (name, u) in &table.unique {
        if contested(&u.columns) {
            released.push(Change::Constraint(ConstraintChange::DropUnique {
                table: declared_name.clone(),
                name: name.clone(),
            }));
        }
    }
    // A foreign key the engine takes for the parent's clone keeps its own
    // name (measured on 16 and 18), so a check or foreign key the partition
    // declares of its own under that name would meet it as `already exists`.
    // And of two equal to one of the parent's, the engine takes one by an
    // order the model does not hold (measured on 16 and 18: `a_fk`, created
    // after `z_fk`, was taken), and the other stays local under a name this
    // plan cannot tell. Each such one is dropped before the attach,
    // leaving the engine to clone the parent's under the parent's name, which
    // no own name takes (#1632 review).
    let twins = |fk: &pbps_model::ForeignKey| {
        table
            .foreign_keys
            .values()
            .filter(|other| *other == fk)
            .count()
            > 1
    };
    for (name, fk) in &table.foreign_keys {
        if !own_foreign_keys.contains_key(name)
            && (twins(fk)
                || declared.foreign_keys.contains_key(name)
                || declared.checks.contains_key(name))
        {
            released.push(Change::Constraint(ConstraintChange::DropForeignKey {
                table: declared_name.clone(),
                name: name.clone(),
            }));
        }
    }
    if table.replica_identity.is_some() {
        what.push(
            "it has a replica identity of its own, which a partition does not hold yet".to_owned(),
        );
    }
    if table.data.is_some() {
        what.push("it declares `data:`, which a partition does not".to_owned());
    }
    // Neither engine's other settings: SQL Server's, which a partition never
    // has, since the dialect holds no partitions.
    let rest = Table {
        description: None,
        columns: Default::default(),
        primary_key: None,
        unique: Default::default(),
        foreign_keys: Default::default(),
        checks: Default::default(),
        indexes: Default::default(),
        replica_identity: None,
        storage_parameters: Default::default(),
        unlogged: false,
        partition_by: None,
        partition_of: None,
        data: None,
        ..table.clone()
    };
    if rest != Table::default() {
        what.push("it holds a setting a partition does not".to_owned());
    }
    // Held by the schema rather than the table: what reaches it from
    // elsewhere would then reach a partition, which the model does not hold.
    // A trigger on it does not: a module on the partition, it stays its own,
    // or is dropped before the attach (class 0), and validation refuses one
    // the declarations keep under a name its parent's row triggers clone
    // onto it (DEC-1632.3).
    for (other, t) in &base.schema.tables {
        for (name, fk) in &t.foreign_keys {
            if &fk.references_table == base_name {
                what.push(format!(
                    "the foreign key `{name}` of {other} references it, and nothing references a \
                     partition yet",
                    other = other.shown(),
                    name = name.shown(),
                ));
            }
        }
    }
    if !what.is_empty() {
        return Err(what);
    }
    Ok(Attached {
        table: Table {
            checks,
            indexes,
            foreign_keys: own_foreign_keys,
            storage_parameters: table.storage_parameters.clone(),
            unlogged: table.unlogged,
            partition_of: Some(pbps_model::PartitionOf {
                parent: of.parent.clone(),
                bound: of.bound.clone(),
                columns: own_columns,
            }),
            ..Table::default()
        },
        defaultless,
        displaced,
        released,
    })
}

fn refuse_partition_changes(
    base: Side<'_>,
    declared: Side<'_>,
    index: &Indexes<'_>,
    hints: &Hints,
    dialect: &(impl Dialect + ?Sized),
    changes: &[Change],
    errs: &mut Vec<DiffError>,
) {
    let partitioned = |schema: &Schema, name: &TableName| {
        schema
            .tables
            .get(name)
            .is_some_and(|t| t.partition_by.is_some() || t.partition_of.is_some())
    };
    let label_renames = crate::enum_labels::label_renames(changes);
    let mut refused: BTreeMap<TableName, Vec<String>> = BTreeMap::new();
    // The tables this plan attaches under each parent or detaches from it.
    let mut moving: BTreeMap<TableName, Vec<TableName>> = BTreeMap::new();
    let mut changes_meet_moves: BTreeSet<TableName> = BTreeSet::new();
    let mut refuse = |table: &TableName, what: String| {
        let list = refused.entry(table.clone()).or_default();
        if !list.contains(&what) {
            list.push(what);
        }
    };
    // A standing parent this plan renames (#1690), by its declared name and
    // by its base one. Its partitions follow it under the engine's plain
    // `RENAME`, and its own changes in the plan name it as declared.
    let standing_parent = |schema: &Schema, name: &TableName| {
        schema
            .tables
            .get(name)
            .is_some_and(|t| t.partition_by.is_some() && t.partition_of.is_none())
    };
    let renamed_parents: BTreeMap<&TableName, &TableName> = changes
        .iter()
        .filter_map(|c| {
            if let Change::Table(TableChange::RenameTable { from, to, .. }) = c
                && standing_parent(base.schema, from)
                && standing_parent(declared.schema, to)
            {
                Some((to, from))
            } else {
                None
            }
        })
        .collect();
    let base_name = |declared_name: &TableName| -> TableName {
        renamed_parents
            .get(declared_name)
            .map_or_else(|| declared_name.clone(), |&from| from.clone())
    };
    let declared_parent = |base_name: &TableName| -> TableName {
        renamed_parents
            .iter()
            .find(|(_, from)| **from == base_name)
            .map_or_else(|| base_name.clone(), |(to, _)| (*to).clone())
    };
    // A difference in the key or the bound itself, which no change carries.
    for (uid, declared_name) in &declared.ids.tables {
        let Some(base_name) = base.ids.tables.get(uid) else {
            continue;
        };
        if let (Some(b), Some(d)) = (
            base.schema.tables.get(base_name),
            declared.schema.tables.get(declared_name),
        ) {
            // A partition declared as an ordinary table is detached (#1544),
            // and an ordinary table declared as a partition attached (#1545):
            // each is its own change, or its own refusal.
            let detached =
                b.partition_of.is_some() && d.partition_of.is_none() && d.partition_by.is_none();
            let attached = b.partition_of.is_none() && d.partition_of.is_some();
            let parent = if detached {
                b.partition_of.as_ref()
            } else if attached {
                d.partition_of.as_ref()
            } else {
                None
            };
            // Under the name the plan leaves the parent: a detach's is the
            // base's, brought forward; an attach's is already declared.
            if let Some(of) = parent {
                let parent = if detached {
                    declared_parent(&of.parent)
                } else {
                    of.parent.clone()
                };
                moving
                    .entry(parent)
                    .or_default()
                    .push(declared_name.clone());
            }
            // A partition's parent as declared: the base's brought forward
            // through the plan's renames, one it renames being the same
            // parent under its new name (#1690). The declared side is already
            // under the names the plan leaves; bringing it forward too would
            // move a name another rename has taken (#1751 review).
            let where_ = |t: &Table, forward: bool| {
                t.partition_of.as_ref().map(|of| {
                    (
                        if forward {
                            declared_parent(&of.parent)
                        } else {
                            of.parent.clone()
                        },
                        // The base's labels, as the plan's renames leave them
                        // (DEC-2346.1).
                        match base.schema.tables.get(&of.parent) {
                            Some(parent) if forward => crate::enum_labels::forwarded_bound(
                                &of.bound,
                                parent,
                                (&TypeResolver::new(base.ids, declared.ids), TypeSide::Base),
                                &label_renames,
                            ),
                            _ => of.bound.clone(),
                        },
                    )
                })
            };
            // A key column renamed is the same key: the engine renames it in
            // the key as it does in the table (measured on 16 and 18, #1687).
            // Another tool's partitions on one side and pbps's on the other
            // (#1533). No statement makes the switch, and a plan records only
            // what its changes do, so the setting is recorded as an adoption
            // is (#1170): by a baseline once the declarations say so. A
            // partition pbps holds is dropped or detached first, since
            // handing it over would leave the declarations nothing to say
            // about it.
            let external = |t: &Table| t.partition_by.as_ref().map(|by| by.external);
            if let (Some(was), Some(is)) = (external(b), external(d))
                && was != is
            {
                let held: Vec<String> = base
                    .schema
                    .tables
                    .iter()
                    .filter(|(_, t)| {
                        t.partition_of
                            .as_ref()
                            .is_some_and(|of| &of.parent == base_name)
                    })
                    .map(|(n, _)| n.shown().to_string())
                    .collect();
                if !is {
                    refuse(
                        declared_name,
                        "declare its external partitions; adopt them as partitions made by hand \
                         are, with `pbps pull` and `pbps baseline --reason …`"
                            .to_owned(),
                    );
                } else if !held.is_empty() {
                    refuse(
                        declared_name,
                        format!(
                            "make its partitions external while pbps holds {}; drop or detach \
                             them in a plan of their own first",
                            held.join(", ")
                        ),
                    );
                } else {
                    refuse(
                        declared_name,
                        "make its partitions external, which no statement does; record it with \
                         `pbps baseline --reason …` once the declarations say so"
                            .to_owned(),
                    );
                }
                continue;
            }
            let renamed = |by: &pbps_model::PartitionBy| {
                let names = renamed_columns(index, base_name, declared_name);
                pbps_model::PartitionBy {
                    external: by.external,
                    columns: by
                        .columns
                        .iter()
                        .map(|c| names.get(c).cloned().unwrap_or_else(|| c.clone()))
                        .collect(),
                    strategy: by.strategy,
                }
            };
            let changes_key = b.partition_by.as_ref().map(renamed) != d.partition_by;
            if (changes_key || where_(b, true) != where_(d, false)) && !detached && !attached {
                refuse(
                    declared_name,
                    if changes_key {
                        "change its partitioning (partition-key rebuild support: issue #2298)"
                    } else {
                        "change its partitioning"
                    }
                    .to_owned(),
                );
            }
        }
    }
    // As for a temporal table: the changes split out of a table this plan
    // creates are the creation; a drop or a rename is asked of the side it
    // acts on.
    let created: BTreeSet<&TableName> = changes
        .iter()
        .filter_map(|c| {
            if let Change::Table(TableChange::CreateTable { name, .. }) = c {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    let attaching: BTreeSet<&TableName> = changes
        .iter()
        .filter_map(|c| {
            if let Change::Partition(PartitionChange::AttachPartition { table, .. }) = c {
                Some(table)
            } else {
                None
            }
        })
        .collect();
    // A partition this plan drops under its standing parent: the foreign
    // keys of its own are dropped before it, as any table's are (#1632
    // review).
    let dropping: BTreeSet<&TableName> = changes
        .iter()
        .filter_map(|c| {
            if let Change::Table(TableChange::DropTable {
                name,
                detach_from: Some(_),
                ..
            }) = c
            {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    // The names the plan takes from a column, by a drop or a rename away,
    // and the name a change gives a column, by an addition or a rename into.
    // A partition dropped under its parent is a transition too: its rows
    // still stand when the pre-flight probes the parent's column (#1692
    // review).
    for change in changes {
        if let Change::Table(TableChange::DropTable {
            name,
            detach_from: Some(parent),
            ..
        }) = change
        {
            moving
                .entry(declared_parent(parent))
                .or_default()
                .push(name.clone());
        }
    }
    for change in changes {
        // A partition is created with its parent, or under a parent that
        // already stands (#1171); either way it is a creation, and whether
        // the parent's rows let it be is the connected probe's question.
        if matches!(change, Change::Table(TableChange::CreateTable { .. })) {
            continue;
        }
        // A partition is dropped, detached first, while its parent stands
        // (#1171), or detached and kept (#1544). Its parent's own drop is
        // still refused below.
        if matches!(
            change,
            Change::Table(TableChange::DropTable {
                detach_from: Some(_),
                ..
            }) | Change::Partition(PartitionChange::DetachPartition { .. })
                | Change::Partition(PartitionChange::AttachPartition { .. })
        ) {
            continue;
        }
        // A standing partition's own: its indexes and checks, which the
        // reader never confuses with its parent's clones (#1577), its
        // defaults and NOT NULLs, its storage parameters and its persistence
        // (#1581), and its foreign keys, a clone of the parent's being one
        // with a `conparentid` the reader never reads (#1632). A clone is its
        // parent's to change (#1546).
        let own = matches!(
            change,
            Change::Index(IndexChange::AddIndex { .. })
                | Change::Index(IndexChange::DropIndex { .. })
                | Change::Constraint(ConstraintChange::AddCheck { .. })
                | Change::Constraint(ConstraintChange::DropCheck { .. })
                | Change::Constraint(ConstraintChange::AddForeignKey { .. })
                | Change::Constraint(ConstraintChange::DropForeignKey { .. })
                | Change::Partition(PartitionChange::SetPartitionDefault { .. })
                | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
                | Change::Policy(_)
                | Change::TableProperty(TablePropertyChange::SetStorageParameters { .. })
                | Change::TableProperty(TablePropertyChange::SetTablePersistence { .. })
        );
        let standing = |schema: &Schema, name: &TableName| {
            schema
                .tables
                .get(name)
                .is_some_and(|t| t.partition_of.is_some() && t.partition_by.is_none())
        };
        // A table this plan attaches is a partition from the attach on, and
        // its own are brought to the declaration after it (#1545).
        if own
            && change.table().is_some_and(|t| {
                (standing(base.schema, t) || attaching.contains(t)) && standing(declared.schema, t)
            })
        {
            continue;
        }
        if let Change::Constraint(ConstraintChange::DropForeignKey { table, .. }) = change
            && standing(base.schema, table)
            && dropping.contains(table)
        {
            continue;
        }
        // Before the attach, still the ordinary table's: a key or unique
        // constraint whose index a parent's plain index could take (#1642
        // review). On a standing partition it is the parent's clone.
        if matches!(
            change,
            Change::Constraint(ConstraintChange::DropUnique { .. })
                | Change::Constraint(ConstraintChange::SetPrimaryKey { to: None, .. })
        ) && change.table().is_some_and(|t| attaching.contains(t))
        {
            continue;
        }
        // A standing parent's column (#1687): the engine recurses each of
        // these into every partition, and the partitions' own defaults and
        // NOT NULLs follow in `diff_partition_columns`. A key column is never
        // dropped or retyped (measured on 16 and 18), and an identity column
        // on a partitioned table is not read back yet (#1681).
        let parent_standing = |schema: &Schema, name: &TableName| {
            if std::ptr::eq(schema, base.schema) {
                standing_parent(schema, &base_name(name))
            } else {
                standing_parent(schema, name)
            }
        };
        // A standing parent's indexes (#1688), keys, checks and foreign keys
        // (#1689): the engine recurses each into every partition, a clone on
        // each that the reader leaves to the parent (DEC-1577.1). A new name
        // is a drop and an add, as on any table.
        if let Change::Index(IndexChange::AddIndex { table, .. })
        | Change::Index(IndexChange::DropIndex { table, .. })
        | Change::Constraint(ConstraintChange::SetPrimaryKey { table, .. })
        | Change::Constraint(ConstraintChange::AddUnique { table, .. })
        | Change::Constraint(ConstraintChange::DropUnique { table, .. })
        | Change::Constraint(ConstraintChange::AddCheck { table, .. })
        | Change::Constraint(ConstraintChange::DropCheck { table, .. })
        | Change::Constraint(ConstraintChange::AddForeignKey { table, .. })
        | Change::Constraint(ConstraintChange::DropForeignKey { table, .. })
        | Change::Constraint(ConstraintChange::SetExclusion { table, .. }) = change
            && parent_standing(base.schema, table)
            && parent_standing(declared.schema, table)
        {
            if let Some(tables) = moving.get(table) {
                // The same two plans as a column change (DEC-1687.1): a
                // detach's shape and a unique index's probe each read the
                // tree on one side of the other (#1737 review).
                let tables: Vec<String> =
                    tables.iter().map(|name| name.shown().to_string()).collect();
                changes_meet_moves.insert(table.clone());
                refuse(
                    table,
                    format!(
                        "{} while this plan attaches, detaches or drops {}; change the parent and \
                         the partitions in separate plans (support in one plan: issue #2296)",
                        change_in_words(change),
                        tables.join(", ")
                    ),
                );
            } else if let Some(why) =
                refuse_parent_index(table, change, base, declared, hints, changes)
            {
                refuse(table, why);
            }
            continue;
        }
        if let Some(column) = parent_column(change)
            && let Some(table) = change.table()
            && parent_standing(base.schema, table)
            && parent_standing(declared.schema, table)
        {
            let key = |schema: &Schema| {
                let name = if std::ptr::eq(schema, base.schema) {
                    base_name(table)
                } else {
                    table.clone()
                };
                schema
                    .tables
                    .get(&name)
                    .and_then(|t| t.partition_by.as_ref())
                    .is_some_and(|by| by.columns.iter().any(|c| c == column))
            };
            // A drop names the base column and a retype the declared one, so
            // each is asked of its own side's key: across one plan a name
            // can leave the key's column for another (#1692 review).
            if (matches!(change, Change::Column(ColumnChange::DropColumn { .. }))
                && key(base.schema))
                || (matches!(change, Change::Column(ColumnChange::AlterColumnType { .. }))
                    && key(declared.schema))
            {
                refuse(
                    table,
                    format!(
                        "{}, which is in its partition key (table rebuild support: issue #2298)",
                        change_in_words(change)
                    ),
                );
            } else if let Change::Column(ColumnChange::AddColumn { column: c, .. }) = change
                && (match &c.value_source {
                    ValueSource::Identity(_) => true,
                    ValueSource::None
                    | ValueSource::Default(_)
                    | ValueSource::Sequence(_)
                    | ValueSource::Generated(_) => false,
                })
            {
                refuse(
                    table,
                    format!(
                        "{}, an identity column, which a partitioned table's partitions are not \
                         read back with yet (#1681)",
                        change_in_words(change)
                    ),
                );
            } else if let Some(tables) = moving.get(table)
                && !dialect.partition_meetings()
                && !matches!(
                    change,
                    Change::Column(ColumnChange::SetColumnDeprecated { .. })
                )
            {
                // The capability's canonical symbolic schedule proves this
                // cut; engines without that contract keep the guard (DEC-2296.12).
                // An attach or a detach meets the parent's columns as they
                // stand at that statement, and the table on its other side
                // holds them as declared at the other end of the plan: each
                // column change would need its own place against each
                // transition (#1692 review). Two plans keep each simple.
                let tables: Vec<String> =
                    tables.iter().map(|name| name.shown().to_string()).collect();
                changes_meet_moves.insert(table.clone());
                refuse(
                    table,
                    format!(
                        "{} while this plan attaches, detaches or drops {}; change the columns and the \
                         partitions in separate plans (support in one plan: issue #2296)",
                        change_in_words(change),
                        tables.join(", ")
                    ),
                );
            }
            continue;
        }
        // A standing parent's rename (#1690): a plain `RENAME`, which its
        // partitions follow. A partition attached, detached or dropped under
        // it in the same plan is named on one side or the other of it.
        if let Change::Table(TableChange::RenameTable { from, to, .. }) = change
            && renamed_parents.get(to) == Some(&from)
        {
            let tables: Vec<String> = moving
                .get(to)
                .into_iter()
                .flatten()
                .map(|name| name.shown().to_string())
                .collect();
            if !tables.is_empty() && !dialect.partition_meetings() {
                changes_meet_moves.insert(to.clone());
                refuse(
                    to,
                    format!(
                        "{} while this plan attaches, detaches or drops {}; rename it and change \
                         the partitions in separate plans (support in one plan: issue #2296)",
                        change_in_words(change),
                        tables.join(", ")
                    ),
                );
            }
            continue;
        }
        let ends_a_table = matches!(
            change,
            Change::Table(TableChange::DropTable { .. })
                | Change::Table(TableChange::RenameTable { .. })
        );
        if !ends_a_table && change.table().is_some_and(|t| created.contains(t)) {
            continue;
        }
        let asked: Vec<&TableName> =
            if let Change::Table(TableChange::DropTable { name, .. }) = change {
                vec![name]
                    .into_iter()
                    .filter(|n| partitioned(base.schema, n))
                    .collect()
            } else if let Change::Table(TableChange::RenameTable { from, to, .. }) = change {
                [(from, base.schema), (to, declared.schema)]
                    .into_iter()
                    .filter(|(n, side)| partitioned(side, n))
                    .map(|(n, _)| n)
                    .collect()
            } else {
                change
                    .table()
                    .into_iter()
                    .filter(|n| partitioned(base.schema, n) || partitioned(declared.schema, n))
                    .collect()
            };
        for table in asked {
            refuse(table, change_in_words(change));
        }
    }
    // A detach refused above for its parent's column changes is not also a
    // shape the detach cannot take: its declaration holds the columns as the
    // refused changes leave them.
    errs.retain(|e| {
        !matches!(e, DiffError::DetachedShape { parent, .. } if changes_meet_moves.contains(parent))
    });
    errs.extend(
        refused
            .into_iter()
            .map(|(table, what)| DiffError::PartitionedTableChange { table, what }),
    );
}

/// Why a standing parent's index (#1688), key or check change (#1689) cannot
/// be planned, if it cannot. Each is measured on 16.15 and 18.6:
/// - `CONCURRENTLY` is refused on a partitioned table, building or dropping,
///   so `strategy: online` cannot be honoured, and leaving it off would lock
///   every partition the operator asked to keep writable;
/// - a unique index lacking a partition-key column is refused by the engine,
///   "unique constraint on partitioned table must include all partitioning
///   columns";
/// - a new index adopts a partition's own index that matches it (DEC-1577.1),
///   which then becomes the parent's clone, and the partition's own is gone
///   from every later read. Refused by name (leon, 2026-10-08), the remedy
///   being to drop or rename the partition's own first;
/// - a primary key or unique constraint lacking a partition-key column is
///   refused by the engine as a unique index is;
/// - a new check absorbs a partition's own check of its name.
///
/// `strategy: online` asks only of an index: a key, a check and a foreign
/// key drop the hint on every table, a partitioned one included (`emit.rs`).
fn refuse_parent_index(
    table: &TableName,
    change: &Change,
    base: Side<'_>,
    declared: Side<'_>,
    hints: &Hints,
    changes: &[Change],
) -> Option<String> {
    if matches!(
        change,
        Change::Index(IndexChange::AddIndex { .. }) | Change::Index(IndexChange::DropIndex { .. })
    ) && hints.strategies.get(table).is_some_and(|s| s.online)
    {
        return Some(format!(
            "{} with `strategy: online`, which PostgreSQL cannot build or drop concurrently on a \
             partitioned table; remove the table's `strategy: online` for this plan",
            change_in_words(change)
        ));
    }
    let parent = declared.schema.tables.get(table)?;
    let partitions = || {
        declared.schema.tables.iter().filter(|(_, t)| {
            t.partition_of
                .as_ref()
                .is_some_and(|of| of.parent == *table)
        })
    };
    // A key holds every partition-key column, or the engine refuses it, as
    // it does a unique index (#1689).
    let missing_key = |columns: &[ColumnName]| {
        parent.partition_by.as_ref().and_then(|by| {
            by.columns
                .iter()
                .find(|key| !columns.contains(key))
                .cloned()
        })
    };
    let without_key = |missing: ColumnName| {
        format!(
            "{} without the partition key column `{missing}`, which PostgreSQL refuses on a \
             partitioned table; add `{missing}` to its columns",
            change_in_words(change),
            missing = missing.shown(),
        )
    };
    if let Change::Constraint(ConstraintChange::SetPrimaryKey { to: Some(pk), .. }) = change {
        return missing_key(&pk.columns).map(without_key);
    }
    if let Change::Constraint(ConstraintChange::AddUnique { constraint, .. }) = change {
        return missing_key(&constraint.columns).map(without_key);
    }
    // A parent's new exclusion (PostgreSQL 18 only; DEC-2448.6): the engine requires
    // every partition-key column as a plain key with an equality, and takes a
    // partition's own exclusion of the same shape as the parent's clone, which the
    // parent's drop then removes. A drop has nothing to ask.
    if let Change::Constraint(ConstraintChange::SetExclusion {
        name,
        to: Some(exclusion),
        ..
    }) = change
    {
        if let Some(missing) = parent.partition_by.as_ref().and_then(|by| {
            by.columns
                .iter()
                .find(|key| !holds_key_with_equality(exclusion, key))
                .cloned()
        }) {
            return Some(format!(
                "add exclusion constraint `{name}` without the partition key column `{missing}` \
                 as a plain key excluded with `=`, which PostgreSQL refuses on a partitioned \
                 table; add `{missing}` WITH `=` to its keys",
                missing = missing.shown(),
                name = name.shown(),
            ));
        }
        let adopted: Vec<String> = partitions()
            .flat_map(|(partition, t)| {
                t.exclude
                    .iter()
                    .filter(|(_, own)| adopts_exclusion(exclusion, own))
                    .map(move |(own, _)| {
                        format!(
                            "{partition}'s `{own}`",
                            partition = partition.shown(),
                            own = own.shown(),
                        )
                    })
            })
            .collect();
        return (!adopted.is_empty()).then(|| {
            format!(
                "add exclusion constraint `{name}`, which can take {} as the partition's copy \
                 of it and leave the partition without its own; drop or rename the \
                 partition's own exclusion in an earlier plan",
                adopted.join(", "),
                name = name.shown(),
            )
        });
    }
    // A partition's own check under the parent's new check's name is
    // absorbed into it, measured on 16 and 18: the same expression makes it
    // the parent's clone, which the parent's later drop removes, and another
    // is refused by the engine. Refused by name (leon, 2026-10-08), the
    // remedy being to drop or rename the partition's own first.
    if let Change::Constraint(ConstraintChange::AddCheck { name, .. }) = change {
        let held: Vec<String> = partitions()
            .filter(|(_, t)| t.checks.contains_key(name.raw()))
            .map(|(partition, _)| partition.shown().to_string())
            .collect();
        return (!held.is_empty()).then(|| {
            format!(
                "add check `{name}`, a name {} holds as its own check, which the parent's would \
                 absorb; drop or rename the partition's own check in an earlier plan",
                held.join(", "),
                name = name.shown(),
            )
        });
    }
    let Change::Index(IndexChange::AddIndex { name, index, .. }) = change else {
        return None;
    };
    if index.unique
        && let Some(by) = &parent.partition_by
        && let Some(missing) = by
            .columns
            .iter()
            .find(|key| !index.columns.iter().any(|c| c.key.column() == Some(*key)))
    {
        return Some(format!(
            "add unique index `{name}` without the partition key column `{missing}`, which \
             PostgreSQL refuses on a partitioned table; add `{missing}` to its columns",
            missing = missing.shown(),
            name = name.shown(),
        ));
    }
    // Only an own index standing when the parent's is built: one the
    // partition held before the plan and the plan does not drop, or every
    // one of a partition the plan creates, which is created with them. One
    // the plan adds to a standing partition is built after its parent's, and
    // is its own. Held, not equal: across a column rename the index is
    // compared as renamed and kept, not dropped (#1737 review).
    let adopted: Vec<String> = partitions()
        .flat_map(|(partition, t)| {
            let before = declared
                .ids
                .tables
                .iter()
                .find(|(_, name)| *name == partition)
                .and_then(|(uid, _)| base.ids.tables.get(uid))
                .and_then(|name| base.schema.tables.get(name));
            t.indexes
                .iter()
                .filter(move |(own, _)| {
                    before.is_none_or(|b| {
                        b.indexes.contains_key(own.raw())
                            && !changes.iter().any(|c| {
                                matches!(c, Change::Index(IndexChange::DropIndex { table, name })
                                    if table == partition && name == *own)
                            })
                    })
                })
                .filter(|(_, own)| adopts(index, own))
                .map(move |(own, _)| {
                    format!(
                        "{partition}'s `{own}`",
                        partition = partition.shown(),
                        own = own.shown(),
                    )
                })
        })
        .collect();
    (!adopted.is_empty()).then(|| {
        format!(
            "add index `{name}`, which can take {} as the partition's copy of it and leave the \
             partition without its own; drop or rename the partition's own index in an earlier \
             plan",
            adopted.join(", "),
            name = name.shown(),
        )
    })
}

/// Whether a new parent index can take a partition's own index as its
/// clone. Measured on 16.15 and 18.6: the method, the keys in order with
/// their classes, uniqueness, the predicate and the `INCLUDE` columns must
/// match; a key's direction, its class options (`opclass_parameters`) and
/// the storage parameters need not (#1688, DEC-2449.2).
///
/// An expression key or a predicate is a match whatever its text, as the
/// attach path takes it (DEC-1545.1): the engine compares what each parses
/// and binds to, so `n+1` and `n + 1` are one index to it (#1737 review).
pub fn adopts(parent: &pbps_model::Index, own: &pbps_model::Index) -> bool {
    parent.method == own.method
        && parent.unique == own.unique
        && parent.filter.is_some() == own.filter.is_some()
        && parent.include == own.include
        && parent.columns.len() == own.columns.len()
        && parent.columns.iter().zip(&own.columns).all(|(p, o)| {
            match (p.key.column(), o.key.column()) {
                (Some(p_column), Some(o_column)) => p_column == o_column && p.opclass == o.opclass,
                _ => true,
            }
        })
}

/// Whether `key` is held by `exclusion` as a plain column key excluded with
/// `=`: what PostgreSQL 18 requires of each partition-key column of a
/// partitioned table's exclusion (DEC-2448.6). An `INCLUDE` column, a cast and
/// an expression over the column do not hold it (measured on 18.6).
pub fn holds_key_with_equality(
    exclusion: &pbps_model::ExclusionConstraint,
    key: &ColumnName,
) -> bool {
    exclusion
        .keys
        .iter()
        .any(|k| k.key.key.column() == Some(key) && k.operator.as_symbol() == "=")
}

/// Whether a parent's new exclusion can take a partition's own as its clone:
/// the one predicate the differ, the attach path and validation share
/// (DEC-2448.6). Measured on 18.6, on `ATTACH` and on a parent's `ADD` over a
/// standing partition: the method, the keys in order with their classes,
/// collations and operators, the `INCLUDE` columns and the predicate must
/// match; the timing, the storage parameters and the name need not. As for
/// an index, an expression key or a predicate matches whatever its text, since
/// the engine compares what each binds to.
pub fn adopts_exclusion(
    parent: &pbps_model::ExclusionConstraint,
    own: &pbps_model::ExclusionConstraint,
) -> bool {
    parent.method == own.method
        && parent.filter.is_some() == own.filter.is_some()
        && parent.include == own.include
        && parent.keys.len() == own.keys.len()
        && parent.keys.iter().zip(&own.keys).all(|(p, o)| {
            p.operator == o.operator
                && match (p.key.key.column(), o.key.key.column()) {
                    (Some(p_column), Some(o_column)) => {
                        p_column == o_column
                            && p.key.opclass == o.key.opclass
                            && p.key.collation == o.key.collation
                    }
                    _ => true,
                }
        })
}

/// The column a parent's column change names, for the kinds the engine
/// recurses into every partition (#1687): its name before a rename, as the
/// partition key holds it.
// The complement is every change that names no column of a table.
#[allow(clippy::wildcard_enum_match_arm)]
fn parent_column(change: &Change) -> Option<&ColumnName> {
    match change {
        Change::Column(ColumnChange::AddColumn { name, .. }) => Some(name),
        Change::Column(ColumnChange::RenameColumn { from, .. }) => Some(from),
        Change::TypeValue(TypeValueChange::EnumRebuild {
            step: pbps_model::EnumRebuildStep::Retype { column, .. },
            ..
        })
        | Change::Column(ColumnChange::DropColumn { column, .. })
        | Change::Column(ColumnChange::AlterColumnType { column, .. })
        | Change::Column(ColumnChange::AlterColumnNullability { column, .. })
        | Change::Column(ColumnChange::AlterColumnDefault { column, .. })
        | Change::Column(ColumnChange::AlterColumnNumbering { column, .. })
        | Change::Column(ColumnChange::SetColumnDeprecated { column, .. }) => Some(&column.name),
        _ => None,
    }
}

/// A table's columns this plan renames, base name to declared name, by uid.
fn renamed_columns(
    index: &Indexes<'_>,
    base_name: &TableName,
    declared_name: &TableName,
) -> BTreeMap<ColumnName, ColumnName> {
    let now = columns_of(&index.declared, declared_name);
    columns_of(&index.base, base_name)
        .into_iter()
        .filter_map(|(uid, was)| {
            now.get(&uid)
                .filter(|c| c.name != was.name)
                .map(|c| (was.name, c.name.clone()))
        })
        .collect()
}

/// A standing partition's own defaults and NOT NULLs (#1581, DEC-1581.1),
/// one change per column and kind. Its partitioning, when changed, is refused
/// by `refuse_partition_changes`; these are compared only where both sides
/// are a partition.
///
/// The parent's own column changes in the same plan are followed (#1687),
/// each recursed by the engine into every partition, measured on 16 and 18:
/// - a rename carries a partition's own default and NOT NULL with it;
/// - a drop takes them, and nothing is left to change;
/// - a default set or dropped on the parent overwrites the partition's own,
///   which is set again after it;
/// - a NOT NULL dropped on the parent leaves one of the partition's own on 18
///   and not on 16, so every partition is brought to its declaration after
///   it;
/// - a NOT NULL set on the parent holds every partition, so the partition's
///   own, which the declarations then leave out, is left to it.
#[allow(clippy::too_many_arguments)]
fn diff_partition_columns(
    uid: &Uid,
    name: &TableName,
    base_table: &Table,
    declared_table: &Table,
    base: Side<'_>,
    declared: Side<'_>,
    index: &Indexes<'_>,
    dialect: &(impl Dialect + ?Sized),
    changes: &mut Vec<Change>,
    errors: &mut Vec<DiffError>,
) {
    let (Some(was), Some(now)) = (&base_table.partition_of, &declared_table.partition_of) else {
        return;
    };
    let parent = declared.schema.tables.get(&now.parent);
    let base_parent = base.schema.tables.get(&was.parent);
    let was_columns = partition_columns_by_uid(&was.columns, &was.parent, &now.parent, index);
    let base_of = base_parent
        .map(|p| partition_columns_by_uid(&p.columns, &was.parent, &now.parent, index))
        .unwrap_or_default();
    let none = pbps_model::PartitionColumn::default();
    let loosened: BTreeSet<ColumnName> = parent
        .map(|p| {
            p.columns
                .iter()
                .filter(|(c, column)| {
                    column.nullable && base_of.get(*c).is_some_and(|b| !b.nullable)
                })
                .map(|(c, _)| c.clone())
                .collect()
        })
        .unwrap_or_default();
    let columns: BTreeSet<&ColumnName> = was_columns
        .keys()
        .chain(now.columns.keys())
        .chain(&loosened)
        .collect();
    for column in columns {
        let from = was_columns.get(column).copied().unwrap_or(&none);
        let to = now.columns.get(column).unwrap_or(&none);
        let theirs = parent.and_then(|p| p.columns.get(column));
        let theirs_was = base_of.get(column);
        let parent_default = |c: &pbps_model::Column| match &c.value_source {
            source @ (ValueSource::Default(_) | ValueSource::Sequence(_)) => Some(source.clone()),
            ValueSource::None | ValueSource::Identity(_) | ValueSource::Generated(_) => None,
        };
        let was_default = theirs_was.and_then(|c| parent_default(c));
        let now_default = theirs.and_then(parent_default);
        // An omitted sequence name is unknown in a declaration, not a
        // request to replace the name a read knows (DEC-1631.1).
        // A new column's own default is new too, so the comparison below
        // already restores it after AddColumn (DEC-2219.1).
        let default_moved = theirs_was.is_some()
            && if let (Some(ValueSource::Sequence(was)), Some(ValueSource::Sequence(now))) =
                (&was_default, &now_default)
            {
                let was = pbps_model::OwnedSequence {
                    name: was.name.clone().or_else(|| now.name.clone()),
                    ..was.clone()
                };
                !now.declares(&was)
            } else {
                was_default != now_default
            };
        if defaults_differ(from.default.as_ref(), to.default.as_ref())
            || (default_moved && to.default.is_some())
        {
            changes.push(Change::Partition(PartitionChange::SetPartitionDefault {
                uid: uid.clone(),
                table: name.clone(),
                parent: now.parent.clone(),
                column: column.clone(),
                from: from.default.clone(),
                to: to.default.clone(),
                // What the engine gave the partition when it was made, and
                // what a row written to it directly gets without one.
                fallback: partition_default(
                    dialect,
                    &now.parent,
                    column,
                    theirs,
                    theirs_was.copied(),
                    errors,
                ),
            }));
        }
        let tightened =
            theirs.is_some_and(|c| !c.nullable) && theirs_was.is_some_and(|b| b.nullable);
        if !tightened && (from.not_null != to.not_null || loosened.contains(column)) {
            changes.push(Change::Partition(PartitionChange::SetPartitionNotNull {
                uid: uid.clone(),
                table: name.clone(),
                column: column.clone(),
                not_null: to.not_null,
            }));
        }
    }
}

/// Both an attach and an override removed later take the parent's default.
/// Keep the serial expression in the dialect, while the saved change keeps
/// the same concrete fallback text as a plain default (DEC-2218.1).
fn partition_default(
    dialect: &(impl Dialect + ?Sized),
    parent: &TableName,
    column: &ColumnName,
    declared: Option<&pbps_model::Column>,
    previous: Option<&pbps_model::Column>,
    errors: &mut Vec<DiffError>,
) -> Option<String> {
    match &declared?.value_source {
        ValueSource::Default(value) => Some(value.clone()),
        ValueSource::Sequence(sequence) => {
            let name = previous.and_then(|c| match &c.value_source {
                ValueSource::Sequence(sequence) => sequence.name.clone(),
                ValueSource::None
                | ValueSource::Default(_)
                | ValueSource::Identity(_)
                | ValueSource::Generated(_) => None,
            });
            let sequence = pbps_model::OwnedSequence {
                name: sequence.name.clone().or(name),
                ..sequence.clone()
            };
            match dialect.owned_sequence_default(parent, column.raw(), &sequence) {
                Ok(value) => Some(value),
                Err(error) => {
                    errors.push(DiffError::PartitionDefault {
                        column: parent.column(column.clone()),
                        reason: error.to_string(),
                    });
                    None
                }
            }
        }
        ValueSource::None | ValueSource::Identity(_) | ValueSource::Generated(_) => None,
    }
}

/// The text that orders two changes tied in class, rank and subject: the kind and
/// its payload as `Debug` writes them, so changes of one kind sit together and
/// kinds sit in alphabetical order, across families (a saved plan's order
/// depends on it). A named function so that the move of the kinds into family
/// enums (#2790) cannot change it by changing `Debug`; `change_in_words` reads
/// the kind's own name from it.
fn tie_break_text(change: &Change) -> String {
    format!("{change:?}")
}

/// A change's kind as words, `drop column` for `DropColumn`: the variant's
/// own name, so a kind added later is named without anyone remembering to.
fn change_in_words(change: &Change) -> String {
    let debug = tie_break_text(change);
    let kind = debug
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or_default();
    let mut words = String::new();
    for (i, c) in kind.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            words.push(' ');
        }
        words.push(c.to_ascii_lowercase());
    }
    words
}

fn refuse_computed_dependencies(
    base: &Schema,
    declared: &Schema,
    dialect: &(impl Dialect + ?Sized),
    screen: Screen,
    changes: &[Change],
    errs: &mut Vec<DiffError>,
) {
    // A computed column calls functions and nothing else: a view or a
    // procedure that shares a name it uses is never what it calls.
    let function = |id: &ModuleId| {
        [base, declared].iter().any(|s| {
            s.modules
                .get(id)
                .is_some_and(|m| m.kind == pbps_model::ModuleKind::Function)
        })
    };
    let dropped = |table: &TableName, name: &ColumnName| {
        changes.iter().any(|c| {
            matches!(c, Change::Column(ColumnChange::DropComputedColumn { table: t, name: n, .. })
                if t == table && n == name)
        })
    };
    // Added by an `ADD`, or with its table by a `CREATE TABLE`: both come
    // before a module this plan creates (class 7 and 9, before 14).
    let added = |table: &TableName, name: &ColumnName| {
        changes.iter().any(|c| {
            matches!(c, Change::Column(ColumnChange::AddComputedColumn { table: t, name: n, .. })
                if t == table && n == name)
                || matches!(c, Change::Table(TableChange::CreateTable { name: t, table: created, .. })
                    if t == table && created.computed.contains_key(name))
        })
    };
    // A schema-bound module over a computed column the plan drops is the
    // catalog's to name (`sys.sql_modules.is_schema_bound`, DEC-1431.1): the
    // word in a module's text is no proof of the clause (#1439).
    for (table_name, table) in &declared.tables {
        for (name, computed) in &table.computed {
            // Standing: there before the plan and there throughout. One the
            // plan adds, new or again, comes at (9, 3), after every input
            // change, and one it drops is gone at (2, 4), before them.
            let standing = !dropped(table_name, name) && !added(table_name, name);
            // A connected plan's catalog edges judge a standing column, by
            // the database's collation (#1460).
            let screened = standing && screen == Screen::Text;
            let at = table_name.column(name);
            for change in changes {
                if screened
                    && let Some((column, what)) = input_change(change, table_name)
                    && dialect.may_name(&computed.expression, column)
                {
                    errs.push(DiffError::ComputedInputChanged {
                        computed: at.clone(),
                        column: column.into(),
                        change: what,
                    });
                }
                let module = match change {
                    Change::Partition(PartitionChange::RebuildPartitionedTable(_))
                    | Change::Index(IndexChange::BuildPartitionedIndex(_))
                    | Change::Partition(PartitionChange::CopyTableRows(_))
                    | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => None,
                    // Standing, or re-added: either way it calls the module
                    // when the module changes. One only dropped is out of the
                    // way first, its module's drop moved after it.
                    Change::Module(ModuleChange::AlterModule { id, .. })
                        if screened || added(table_name, name) =>
                    {
                        Some((id, "alters"))
                    }
                    Change::Module(ModuleChange::DropModule { id, .. })
                        if screened || added(table_name, name) =>
                    {
                        Some((id, "drops"))
                    }
                    Change::Module(ModuleChange::CreateModule { id, .. })
                        if added(table_name, name) =>
                    {
                        Some((id, "creates"))
                    }
                    Change::Table(_)
                    | Change::Partition(PartitionChange::DetachPartition { .. })
                    | Change::Partition(PartitionChange::AttachPartition { .. })
                    | Change::Column(_)
                    | Change::TableProperty(_)
                    | Change::Constraint(_)
                    | Change::Index(IndexChange::SetIndexStorageParameters { .. })
                    | Change::Partition(PartitionChange::SetPartitionDefault { .. })
                    | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
                    | Change::Policy(_)
                    | Change::Index(IndexChange::AddIndex { .. })
                    | Change::Index(IndexChange::DropIndex { .. })
                    | Change::Row(_)
                    | Change::Module(_)
                    | Change::TypeValue(_)
                    | Change::Role(_)
                    | Change::Type(_)
                    | Change::Search(_)
                    | Change::Extension(_)
                    | Change::Permission(_) => None,
                };
                if let Some((id, what)) = module
                    && function(id)
                {
                    let qualified = id.object_name();
                    let name = qualified.name.clone();
                    if dialect.may_name_qualified(
                        &computed.expression,
                        &qualified.schema,
                        &qualified.name,
                    ) {
                        errs.push(DiffError::ComputedFunctionChanged {
                            computed: at.clone(),
                            function: name,
                            change: what,
                        });
                    }
                }
            }
        }
    }
}

/// A table's computed columns (#1174, DEC-1174.1). Neither the expression nor
/// the persistence changes in place, so a column that differs is dropped and
/// added again, which `recreate_retyped_dependents` then rebuilds the indexes
/// and checks over. With no uid, a renamed one is the same pair under two
/// names, which the engine performs as well.
fn diff_computed(name: &TableName, base: &Table, declared: &Table, changes: &mut Vec<Change>) {
    // `declares`, not equality: a read-back holds `not_null` wherever the
    // engine reports the column not nullable, which it also does for an
    // expression that can never be NULL.
    let kept = |wanted: Option<&pbps_model::ComputedColumn>,
                was: Option<&pbps_model::ComputedColumn>| matches!((wanted, was), (Some(w), Some(b)) if w.declares(b));
    for (column, was) in &base.computed {
        if !kept(declared.computed.get(column), Some(was)) {
            changes.push(Change::Column(ColumnChange::DropComputedColumn {
                table: name.clone(),
                name: column.clone(),
                computed: was.clone(),
            }));
        }
    }
    for (column, wanted) in &declared.computed {
        if !kept(Some(wanted), base.computed.get(column)) {
            changes.push(Change::Column(ColumnChange::AddComputedColumn {
                table: name.clone(),
                name: column.clone(),
                computed: wanted.clone(),
            }));
        }
    }
}

/// The declared foreign-key graph's strongly connected components, and how
/// deep each table sits over them: 0 for a table that references no other,
/// else one more than the deepest table it references outside its own
/// component. A table outside the schema counts as depth 0, so a key to one
/// makes its table 1. Self-references do not count.
///
/// Found in one iterative Tarjan pass, linear in tables and keys (#1792). The
/// earlier version kept every table's full reachable set to answer the same
/// question, which was quadratic in memory on a chain of keys, and found each
/// depth by recursion, one stack frame per link. Tarjan completes a component
/// only after every component it reaches, so each member's depth is final
/// when its component is popped.
struct ForeignKeyGraph<'a> {
    index: BTreeMap<&'a TableName, usize>,
    component: Vec<usize>,
    depth: Vec<usize>,
}

impl<'a> ForeignKeyGraph<'a> {
    fn of(schema: &'a Schema) -> Self {
        let names: Vec<&TableName> = schema.tables.keys().collect();
        let index: BTreeMap<&TableName, usize> =
            names.iter().enumerate().map(|(i, t)| (*t, i)).collect();
        let n = names.len();
        let mut edges: Vec<Vec<usize>> = vec![Vec::new(); n];
        // A key to a table the schema does not declare: no edge, but depth 1.
        let mut external = vec![false; n];
        for (i, (name, table)) in schema.tables.iter().enumerate() {
            for fk in table.foreign_keys.values() {
                let to = &fk.references_table;
                if to == name {
                    continue;
                }
                match index.get(to) {
                    Some(&j) => edges[i].push(j),
                    None => external[i] = true,
                }
            }
        }

        const UNSEEN: usize = usize::MAX;
        let mut order = vec![UNSEEN; n];
        let mut low = vec![0; n];
        let mut on_stack = vec![false; n];
        let mut stack = Vec::new();
        let mut component = vec![UNSEEN; n];
        let mut depth = vec![0; n];
        let mut visited = 0;
        let mut components = 0;
        // The recursion, made explicit: each frame is a table and the index
        // of the next of its edges to follow.
        let mut frames: Vec<(usize, usize)> = Vec::new();
        for root in 0..n {
            if order[root] != UNSEEN {
                continue;
            }
            order[root] = visited;
            low[root] = visited;
            visited += 1;
            stack.push(root);
            on_stack[root] = true;
            frames.push((root, 0));
            while let Some(&(v, e)) = frames.last() {
                if let Some(&w) = edges[v].get(e) {
                    if let Some(frame) = frames.last_mut() {
                        frame.1 += 1;
                    }
                    if order[w] == UNSEEN {
                        order[w] = visited;
                        low[w] = visited;
                        visited += 1;
                        stack.push(w);
                        on_stack[w] = true;
                        frames.push((w, 0));
                    } else if on_stack[w] {
                        low[v] = low[v].min(order[w]);
                    }
                    continue;
                }
                frames.pop();
                if let Some(&(u, _)) = frames.last() {
                    low[u] = low[u].min(low[v]);
                }
                if low[v] != order[v] {
                    continue;
                }
                // `v` roots a component: it and everything above it on the
                // stack. Every component its members reach is complete.
                let at = stack.iter().rposition(|&x| x == v).unwrap_or(0);
                let members: Vec<usize> = stack.drain(at..).collect();
                for &m in &members {
                    on_stack[m] = false;
                    component[m] = components;
                }
                for &m in &members {
                    depth[m] = edges[m]
                        .iter()
                        .filter(|&&w| component[w] != components)
                        .map(|&w| 1 + depth[w])
                        .max()
                        .unwrap_or(0)
                        .max(usize::from(external[m]));
                }
                components += 1;
            }
        }
        Self {
            index,
            component,
            depth,
        }
    }

    /// Whether both are declared and reach each other through foreign keys.
    fn same_component(&self, a: &TableName, b: &TableName) -> bool {
        match (self.index.get(a), self.index.get(b)) {
            (Some(&i), Some(&j)) => self.component[i] == self.component[j],
            _ => false,
        }
    }

    /// 0 for a table the schema does not declare.
    fn depth(&self, table: &TableName) -> usize {
        self.index.get(table).map_or(0, |&i| self.depth[i])
    }

    fn deepest(&self) -> usize {
        self.depth.iter().copied().max().unwrap_or(0)
    }
}

/// Foreign keys inside a cycle of tables the plan switches between permanent
/// and unlogged, dropped before the switches and added back after them
/// (#1488 review). Inside a cycle no order works: whichever table switches
/// first breaks a key of the other, permanent referencing unlogged. A valid
/// declaration gives a cycle's tables one persistence (a permanent table
/// referencing an unlogged one is refused), so they switch together. A key
/// the plan already adds, new or rebuilt, stands neither side of the switch
/// and is left alone.
fn unlink_cycles_around_persistence_switches(declared: &Schema, changes: &mut Vec<Change>) {
    let switching: BTreeSet<TableName> = changes
        .iter()
        .filter_map(|c| {
            if let Change::TableProperty(TablePropertyChange::SetTablePersistence {
                table, ..
            }) = c
            {
                Some(table.clone())
            } else {
                None
            }
        })
        .collect();
    if switching.len() < 2 {
        return;
    }
    let graph = ForeignKeyGraph::of(declared);
    let mut unlinked = Vec::new();
    for (name, table) in &declared.tables {
        if !switching.contains(name) {
            continue;
        }
        for (key, fk) in &table.foreign_keys {
            let to = &fk.references_table;
            let in_cycle = to != name && switching.contains(to) && graph.same_component(name, to);
            let added = changes.iter().any(|c| {
                matches!(c, Change::Constraint(ConstraintChange::AddForeignKey { table: t, name: n, .. }) if t == name && n == key)
            });
            if in_cycle && !added {
                unlinked.push(Change::Constraint(ConstraintChange::DropForeignKey {
                    table: name.clone(),
                    name: key.clone(),
                }));
                unlinked.push(Change::Constraint(ConstraintChange::AddForeignKey {
                    table: name.clone(),
                    name: key.clone(),
                    constraint: Box::new(fk.clone()),
                }));
            }
        }
    }
    changes.extend(unlinked);
}

/// A PostgreSQL table's heap storage parameters (DEC-1441.1): one change
/// setting each declared value that differs from the base's, and resetting
/// each the base has and the declaration does not. Both sides are canonical,
/// so a respelling is no change.
fn diff_storage_parameters(
    uid: &Uid,
    name: &TableName,
    base: &Table,
    declared: &Table,
    changes: &mut Vec<Change>,
) {
    let set: BTreeMap<String, String> = declared
        .storage_parameters
        .iter()
        .filter(|(k, v)| base.storage_parameters.get(*k) != Some(*v))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let reset: BTreeSet<String> = base
        .storage_parameters
        .keys()
        .filter(|k| !declared.storage_parameters.contains_key(*k))
        .cloned()
        .collect();
    if !set.is_empty() || !reset.is_empty() {
        changes.push(Change::TableProperty(
            TablePropertyChange::SetStorageParameters {
                uid: uid.clone(),
                table: name.clone(),
                set,
                reset,
            },
        ));
    }
}

/// A PostgreSQL table's replica identity, for each table on both sides
/// (DEC-1444.1). Planned where it changes, and where the plan adds the object
/// whose index it names, new or rebuilt: PostgreSQL lets that index be
/// dropped and leaves the identity naming nothing, and an index created again
/// under the same name is not the identity until it is set again (measured on
/// 16 and 18, #1444).
///
/// Returns the changes, by table uid and target, that run before the plan's
/// drops: those whose target the table can take as it stands, which is every
/// target the plan does not add, when the columns it is over are NOT NULL
/// already. The rest wait for the additions of class 13.
///
/// One that waits while the plan drops the old identity's index is preceded
/// by `FULL`, which every table can take: between the drop and the final
/// setting the table would otherwise identify no row, which the reader leaves
/// out as unreadable, so a staged checkpoint there could not be resumed
/// (#1467 review).
fn diff_replica_identity(
    base: &Schema,
    declared: &Schema,
    declared_tables: &BTreeMap<Uid, TableName>,
    base_tables: &BTreeMap<Uid, TableName>,
    changes: &mut Vec<Change>,
) -> BTreeSet<(Uid, Option<ReplicaIdentity>)> {
    let mut early = BTreeSet::new();
    for (uid, declared_name) in declared_tables {
        let Some(base_name) = base_tables.get(uid) else {
            continue;
        };
        let (Some(was), Some(table)) = (
            base.tables.get(base_name),
            declared.tables.get(declared_name),
        ) else {
            continue;
        };
        let readded = changes.iter().any(|c| match (c, &table.replica_identity) {
            (
                Change::Constraint(ConstraintChange::SetPrimaryKey {
                    table: t,
                    to: Some(_),
                    ..
                }),
                Some(ReplicaIdentity::PrimaryKey),
            ) => t == declared_name,
            (
                Change::Constraint(ConstraintChange::AddUnique { table: t, name, .. }),
                Some(ReplicaIdentity::Unique(n)),
            )
            | (
                Change::Index(IndexChange::AddIndex { table: t, name, .. }),
                Some(ReplicaIdentity::Index(n)),
            ) => t == declared_name && name == n,
            _ => false,
        });
        if was.replica_identity == table.replica_identity && !readded {
            continue;
        }
        let mut probe = was.clone();
        probe.replica_identity.clone_from(&table.replica_identity);
        let now = !readded && probe.replica_identity_problems().is_empty();
        let on = |t: &TableName| t == base_name || t == declared_name;
        let old_index_dropped = changes.iter().any(|c| match (c, &was.replica_identity) {
            (
                Change::Constraint(ConstraintChange::SetPrimaryKey {
                    table: t,
                    from: Some(_),
                    ..
                }),
                Some(ReplicaIdentity::PrimaryKey),
            ) => on(t),
            (
                Change::Constraint(ConstraintChange::DropUnique { table: t, name }),
                Some(ReplicaIdentity::Unique(n)),
            )
            | (
                Change::Index(IndexChange::DropIndex { table: t, name }),
                Some(ReplicaIdentity::Index(n)),
            ) => on(t) && name == n,
            _ => false,
        });
        if now {
            early.insert((uid.clone(), table.replica_identity.clone()));
        } else if old_index_dropped {
            early.insert((uid.clone(), Some(ReplicaIdentity::Full)));
            changes.push(Change::TableProperty(
                TablePropertyChange::SetReplicaIdentity {
                    uid: uid.clone(),
                    table: base_name.clone(),
                    to: Some(ReplicaIdentity::Full),
                },
            ));
        }
        changes.push(Change::TableProperty(
            TablePropertyChange::SetReplicaIdentity {
                uid: uid.clone(),
                table: if now { base_name } else { declared_name }.clone(),
                to: table.replica_identity.clone(),
            },
        ));
    }
    early
}

/// `Some(None)` keeps a part without an edit; `None` replaces it. Keeping
/// those answers distinct lets storage parameters change without a rebuild.
fn by_name<T>(
    base: &BTreeMap<pbps_model::ObjectLocalName, T>,
    declared: &BTreeMap<pbps_model::ObjectLocalName, T>,
    kept: impl Fn(&pbps_model::ObjectLocalName, &T, &T) -> Option<Option<Change>>,
    drop: impl Fn(&pbps_model::ObjectLocalName) -> Change,
    add: impl Fn(&pbps_model::ObjectLocalName, &T) -> Change,
    changes: &mut Vec<Change>,
) {
    for (name, part) in declared {
        if let Some(edit) = base.get(name).and_then(|was| kept(name, was, part)) {
            changes.extend(edit);
        } else {
            if base.contains_key(name) {
                changes.push(drop(name));
            }
            changes.push(add(name, part));
        }
    }
    changes.extend(base.keys().filter(|n| !declared.contains_key(*n)).map(drop));
}

/// What a difference in an index's storage parameters asks of the plan, by
/// the verdicts of the method's policy (DEC-2448.1).
#[derive(Debug, PartialEq, Eq)]
enum ParameterEdit {
    Unchanged,
    /// One changed parameter cannot be altered in place: the part is replaced.
    Rebuild,
    /// One changed parameter is refused outright.
    Refuse {
        parameter: String,
    },
    Alter {
        set: BTreeMap<String, String>,
        reset: BTreeSet<String>,
    },
}

fn parameter_edit(
    policy: &pbps_model::storage::MethodPolicy,
    was: &BTreeMap<String, String>,
    now: &BTreeMap<String, String>,
) -> ParameterEdit {
    use pbps_model::storage::ParameterChange;
    let set: BTreeMap<String, String> = now
        .iter()
        .filter(|(k, v)| was.get(*k) != Some(*v))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let reset: BTreeSet<String> = was
        .keys()
        .filter(|k| !now.contains_key(*k))
        .cloned()
        .collect();
    if set.is_empty() && reset.is_empty() {
        return ParameterEdit::Unchanged;
    }
    let named = set.keys().chain(reset.iter()).map(String::as_str);
    match policy.change_of(named) {
        (ParameterChange::Alter, _) => ParameterEdit::Alter { set, reset },
        (ParameterChange::Rebuild, _) => ParameterEdit::Rebuild,
        (ParameterChange::Refuse, parameter) => ParameterEdit::Refuse {
            parameter: parameter.unwrap_or_default().to_owned(),
        },
    }
}

/// A SQL Server table's, key's and index's filegroup, for each that stays
/// (#2813, DEC-2813.1). **Every change is refused here, by name.** Moving the
/// data is a rebuild of the object, and the plan holds no design for moving
/// it, so a plan that would silently leave an object where it is, or drop and
/// create it elsewhere under a user's rows, is refused instead. An object the
/// plan adds is built where its declaration says; one it removes has nothing
/// to compare.
fn diff_filegroups(name: &TableName, base: &Table, declared: &Table, errs: &mut Vec<DiffError>) {
    let mut check = |target: String,
                     was: Option<&pbps_model::Filegroup>,
                     now: Option<&pbps_model::Filegroup>| {
        if was != now {
            let show = |f: Option<&pbps_model::Filegroup>| match f {
                Some(f) => format!("`{}`", pbps_model::escape::Escaped(f.as_str())),
                None => "none declared".to_owned(),
            };
            errs.push(DiffError::FilegroupChangeRefused {
                table: name.clone(),
                target,
                was: show(was),
                now: show(now),
            });
        }
    };
    check(
        "the table".to_owned(),
        base.filegroup.as_ref(),
        declared.filegroup.as_ref(),
    );
    if let (Some(was), Some(now)) = (&base.primary_key, &declared.primary_key) {
        check(
            "the primary key".to_owned(),
            was.filegroup.as_ref(),
            now.filegroup.as_ref(),
        );
    }
    for (n, was) in &base.unique {
        if let Some(now) = declared.unique.get(n) {
            check(
                format!("unique constraint {}", n.shown()),
                was.filegroup.as_ref(),
                now.filegroup.as_ref(),
            );
        }
    }
    for (n, was) in &base.indexes {
        if let Some(now) = declared.indexes.get(n) {
            check(
                format!("index {}", n.shown()),
                was.filegroup.as_ref(),
                now.filegroup.as_ref(),
            );
        }
    }
}

/// Constraints and indexes are always matched by name and never modified in
/// place — the database itself does drop + add, and pretending otherwise would
/// only give the emitter one more path that can fail.
fn diff_constraints(
    name: &TableName,
    base: &Table,
    declared: &Table,
    changes: &mut Vec<Change>,
    errs: &mut Vec<DiffError>,
) {
    diff_constraints_by(name, base, declared, changes, errs, &IndexMethod::policy);
}

/// The text search objects a plan drops: a baseline identity the declarations no longer
/// hold, whose name the baseline schema has. The one definition of "dropped" that the
/// differ's reference check and a package's drop analysis both read (DEC-2728.2).
pub(crate) fn dropped_search<'a>(
    base: Side<'a>,
    declared: Side<'_>,
) -> std::collections::BTreeSet<&'a pbps_model::SearchRef> {
    base.ids
        .search
        .iter()
        .filter(|(uid, name)| {
            base.schema.search.contains_key(*name) && !declared.ids.search.contains_key(*uid)
        })
        .map(|(_, name)| name)
        .collect()
}

/// The text search objects a plan frees from a package whose dictionaries live in
/// `package_schema` (DEC-2728.2, DEC-2728.3): the ones it drops, and a configuration it keeps
/// and alters away from a dictionary the project does not hold. The alteration (class 7) runs
/// before the package's removal. One that may still map such a dictionary stays refused.
pub(crate) fn released_search<'a>(
    base: Side<'a>,
    declared: Side<'_>,
    package_schema: &str,
) -> std::collections::BTreeSet<&'a pbps_model::SearchRef> {
    let held = |used: &pbps_model::SearchRef| {
        declared.schema.search.contains_key(used) || base.schema.search.contains_key(used)
    };
    let package_owned = |used: &pbps_model::SearchRef| {
        !held(used) && used.name.schema() != "pg_catalog" && used.name.schema() == package_schema
    };
    let mut released = dropped_search(base, declared);
    for (uid, name) in &base.ids.search {
        let (Some(was), Some(kept)) = (
            base.schema.search.get(name),
            declared
                .ids
                .search
                .get(uid)
                .and_then(|kept| declared.schema.search.get(kept)),
        ) else {
            continue;
        };
        let (was, now) = (was.dictionaries(), kept.dictionaries());
        if name.kind == pbps_model::SearchKind::Config
            && was.iter().any(&package_owned)
            && !now.iter().any(&package_owned)
        {
            released.insert(name);
        }
    }
    released
}

/// Text search configurations and dictionaries, by kind and name (DEC-2728.1, DEC-2728.2).
/// A declared object the target lacks is created and one the declarations gave up is
/// dropped, both by recorded identity, and a standing one whose mappings or options differ is
/// altered in place (DEC-2728.3). **Refused here, by name**: a changed parser or template
/// (no `ALTER` form exists), an in-place change whose dependents the target did not list or
/// the catalog proves stale, and a rename, which arrives in a later part. An object the
/// target has and the declarations do not, with no identity recorded, is left alone.
fn diff_search(
    base: Side<'_>,
    declared: Side<'_>,
    hints: &Hints,
    errs: &mut Vec<DiffError>,
) -> Vec<Change> {
    let target_read = hints.target_read;
    let mut changes = Vec::new();
    let mut refuse = |name: &pbps_model::SearchRef, reason: &str| {
        errs.push(DiffError::SearchRefused {
            name: name.clone(),
            reason: reason.to_owned(),
        });
    };
    // The objects this plan drops: a reference resolves only to one the plan keeps. The
    // baseline holding it is no answer, since the drop comes after the create (class 18)
    // and the engine's RESTRICT would refuse it while anything still maps it.
    let dropped = dropped_search(base, declared);
    // A dictionary the project neither declares nor the target holds, in a schema no package
    // of the project provides and that is not the engine's own, is one the engine would refuse
    // at `ADD MAPPING`.
    let unresolved = |object: &SearchObject| -> Vec<pbps_model::SearchRef> {
        object
            .dictionaries()
            .into_iter()
            .filter(|used| {
                !declared.schema.search.contains_key(used)
                    && !base.schema.search.contains_key(used)
                    && used.name.schema() != "pg_catalog"
                    && !declared
                        .schema
                        .extensions
                        .values()
                        .any(|package| package.schema == used.name.schema())
            })
            .collect()
    };
    let undeclared = |missing: &[pbps_model::SearchRef]| -> String {
        let shown: Vec<_> = missing.iter().map(|m| m.shown().to_string()).collect();
        format!(
            "uses {}, which the project does not declare and the target does not hold; declare \
             it, or leave the mapping out",
            shown.join(", ")
        )
    };
    for (name, now) in &declared.schema.search {
        let retired: Vec<String> = now
            .dictionaries()
            .iter()
            .filter(|used| dropped.contains(used))
            .map(|used| used.shown().to_string())
            .collect();
        if !retired.is_empty() {
            refuse(
                name,
                &format!(
                    "uses {}, which this plan drops; keep it declared, or leave the mapping out",
                    retired.join(", ")
                ),
            );
            continue;
        }
        // The identity first, as for a package: a name the base holds under one recorded
        // identity and the declarations under another is an object retired and declared
        // again before the drop reached the target. Equal definitions would skip the create
        // while the drop of the old identity still came, leaving a declared object dropped.
        if base.schema.search.contains_key(name)
            && let (Some(was), Some(now)) =
                (base.ids.search_uid(name), declared.ids.search_uid(name))
            && was != now
        {
            refuse(
                name,
                "was retired and is declared again before the drop reached the target; drop it \
                 in one plan and declare the name again in the next",
            );
            continue;
        }
        match base.schema.search.get(name) {
            Some(was) if was == now => {}
            Some(was) => {
                let Some(uid) = declared.ids.search_uid(name) else {
                    refuse(
                        name,
                        "has no recorded identity; run `pbps plan` to record it",
                    );
                    continue;
                };
                let what = match (was, now) {
                    (
                        SearchObject::Config { parser: a, .. },
                        SearchObject::Config { parser: b, .. },
                    ) if a != b => {
                        refuse(
                            name,
                            "its parser differs, and no `ALTER` form changes a configuration's \
                             parser; declare the new one under a new name and move what uses it",
                        );
                        continue;
                    }
                    (
                        SearchObject::Dictionary { template: a, .. },
                        SearchObject::Dictionary { template: b, .. },
                    ) if a != b => {
                        refuse(
                            name,
                            "its template differs, and no `ALTER` form changes a dictionary's \
                             template; declare the new one under a new name and move what \
                             uses it",
                        );
                        continue;
                    }
                    (SearchObject::Config { .. }, SearchObject::Config { .. }) => "mappings",
                    (SearchObject::Dictionary { .. }, SearchObject::Dictionary { .. }) => "options",
                    _ => {
                        refuse(
                            name,
                            "is declared as another kind of object than the target holds",
                        );
                        continue;
                    }
                };
                // What was computed with the old behaviour stays until rebuilt, and the
                // engine does not rebuild it: an index, a stored column or a check over the
                // object keeps answering as it did. The target's catalog lists them; a plan
                // without it cannot say there are none. What the catalog proves is refused
                // by name; what stands behind a node it cannot see into is carried by the
                // change, which the gated `stale-results` class approves (DEC-2728.3).
                let Some(stale) = hints.search_dependents.get(name).filter(|_| target_read) else {
                    refuse(
                        name,
                        &format!(
                            "its {what} differ, and what depends on it is the target's catalog \
                             to list; plan against the database (`--db`)"
                        ),
                    );
                    continue;
                };
                if !stale.stored.is_empty() {
                    refuse(
                        name,
                        &format!(
                            "its {what} differ, but {} would keep the old behaviour until \
                             rebuilt; declare the new behaviour under a new name and move the \
                             dependents, in two plans",
                            stale.stored.join(", ")
                        ),
                    );
                    continue;
                }
                // A reference the standing object already makes resolves: the target holds the
                // object with it, so the engine accepted it, whether or not the model holds what
                // it names (a package's or another tool's dictionary). Only what the alteration
                // adds can be one the engine would refuse.
                let held = was.dictionaries();
                let missing: Vec<_> = unresolved(now)
                    .into_iter()
                    .filter(|used| !held.contains(used))
                    .collect();
                if !missing.is_empty() {
                    refuse(name, &undeclared(&missing));
                    continue;
                }
                let (uid, name, opaque) = (uid.clone(), name.clone(), stale.opaque.clone());
                let (before, after) = (Box::new(was.clone()), Box::new(now.clone()));
                changes.push(Change::Search(if what == "mappings" {
                    SearchChange::AlterSearchMapping {
                        uid,
                        name,
                        before,
                        after,
                        opaque,
                    }
                } else {
                    SearchChange::AlterSearchDictionary {
                        uid,
                        name,
                        before,
                        after,
                        opaque,
                    }
                }));
            }
            None => {
                match declared.ids.search_uid(name) {
                    None => refuse(
                        name,
                        "has no recorded identity; run `pbps plan` to record it",
                    ),
                    Some(uid)
                        if base.ids.search.get(uid).is_some_and(|was| {
                            was != name && base.schema.search.contains_key(was)
                        }) =>
                    {
                        refuse(name, "renaming one arrives in a later part of #2728");
                    }
                    Some(uid) => {
                        // A dictionary the project neither declares nor the target holds, in a
                        // schema no package of the project provides and that is not the
                        // engine's own, is one the engine would refuse at `ADD MAPPING`.
                        // Against the target that is a refusal. Offline the baseline only
                        // stands for the target, and a dictionary made on the database
                        // outside pbps is not in it, so the plan goes on and says on its
                        // line that the database settles it (DEC-2728.2).
                        let missing = unresolved(now);
                        if missing.is_empty() || !target_read {
                            changes.push(Change::Search(SearchChange::CreateSearch {
                                uid: uid.clone(),
                                name: name.clone(),
                                definition: Box::new(now.clone()),
                                unresolved: missing,
                            }));
                        } else {
                            refuse(name, &undeclared(&missing));
                        }
                    }
                }
            }
        }
    }
    for (uid, name) in &base.ids.search {
        if let Some(definition) = base.schema.search.get(name)
            && !declared.ids.search.contains_key(uid)
        {
            changes.push(Change::Search(SearchChange::DropSearch {
                uid: uid.clone(),
                name: name.clone(),
                definition: Box::new(definition.clone()),
            }));
        }
    }
    changes
}

/// Standalone sequences, by name (DEC-2721.1). **Every difference is refused here, by
/// name**: this part reads and holds them and plans nothing for them. Creating one arrives
/// in the next part, and altering, renaming or dropping one after it, so a plan that would
/// leave a declaration unmet is refused rather than made without the change that says so.
/// A sequence the target has and the declarations do not is not this project's, and is left
/// alone. The counter is not compared: it is in no field of either side.
fn diff_sequences(
    base: &pbps_model::Schema,
    declared: &pbps_model::Schema,
    dialect: &(impl Dialect + ?Sized),
    errs: &mut Vec<DiffError>,
) {
    // Spellings of one type (`int`, `integer`) are one type to the engine.
    let normal = |sequence: &pbps_model::Sequence| {
        let mut sequence = sequence.clone();
        if let Ok(ty) = dialect.normalize_type(&sequence.ty) {
            sequence.ty = ty;
        }
        sequence
    };
    for (name, now) in &declared.sequences {
        let reason = match base.sequences.get(name) {
            None => {
                "the target has none; creating one arrives with the next part of #2721".to_owned()
            }
            Some(was) => {
                let (was, now) = (normal(was), normal(now));
                let differing = sequence_differences(&was, &now, ("declared", "target has"));
                if differing.is_empty() {
                    continue;
                }
                format!(
                    "it differs from the target's ({}); changing one arrives in a later part of \
                     #2721, and for now it stands as the target holds it",
                    differing.join("; ")
                )
            }
        };
        errs.push(DiffError::SequenceRefused {
            name: name.clone(),
            reason,
        });
    }
}

/// What differs between the held sequence and the new one, attribute by attribute, each as
/// `name: <words.0> new, <words.1> held` ("declared", "target has" in a plan; "now",
/// "recorded" in a drift report). A bound or a cache that is `None` is
/// the engine's default, and is said so.
fn sequence_differences(
    was: &pbps_model::Sequence,
    now: &pbps_model::Sequence,
    words: (&str, &str),
) -> Vec<String> {
    fn bound(value: Option<i64>) -> String {
        value.map_or_else(|| "the engine's default".to_owned(), |v| v.to_string())
    }
    let mut out = Vec::new();
    let mut note = |what: &str, declared: String, held: String| {
        if declared != held {
            out.push(format!(
                "{what}: {} {declared}, {} {held}",
                words.0, words.1
            ));
        }
    };
    note(
        "type",
        now.ty.shown().to_string(),
        was.ty.shown().to_string(),
    );
    note("start", now.start.to_string(), was.start.to_string());
    note(
        "increment",
        now.increment.to_string(),
        was.increment.to_string(),
    );
    note("min", bound(now.options.min), bound(was.options.min));
    note("max", bound(now.options.max), bound(was.options.max));
    note(
        "cycle",
        now.options.cycle.to_string(),
        was.options.cycle.to_string(),
    );
    note("cache", bound(now.options.cache), bound(was.options.cache));
    out
}

/// A table's exclusion constraints, by name (DEC-2448.3, DEC-2448.4).
///
/// - **Added**: a change of its own.
/// - **Options only** (storage parameters): `ALTER INDEX .. SET/RESET` on the
///   backing index, the constraint standing (measured on 16.15 and 18.6), as an
///   index's are, under the method's per-parameter verdicts.
/// - **Any other difference at one name**: a replacement, as two changes (the
///   old one's drop and the new one's add) so each takes the ordering class its
///   direction needs. It needs no reason: the name keeps its logical member,
///   and the plan states the drop and the validation.
/// - **Gone from the declaration** (which a rename is, with an add beside it):
///   a drop, and it needs the retirement a human recorded for *this*
///   definition, else it is refused with the command that records it. The
///   table's own drop never gets here: its intent covers its parts.
fn diff_exclusions(
    name: &TableName,
    base: &Table,
    declared: &Table,
    retired: &dyn Fn(&pbps_model::ObjectLocalName) -> bool,
    changes: &mut Vec<Change>,
    errs: &mut Vec<DiffError>,
) {
    let drop_half = |key: &pbps_model::ObjectLocalName, was: &pbps_model::ExclusionConstraint| {
        Change::Constraint(ConstraintChange::SetExclusion {
            table: name.clone(),
            name: key.clone(),
            from: Some(Box::new(was.clone())),
            to: None,
        })
    };
    let add_half = |key: &pbps_model::ObjectLocalName, now: &pbps_model::ExclusionConstraint| {
        Change::Constraint(ConstraintChange::SetExclusion {
            table: name.clone(),
            name: key.clone(),
            from: None,
            to: Some(Box::new(now.clone())),
        })
    };
    for (key, was) in &base.exclude {
        match declared.exclude.get(key) {
            Some(now) if now == was => {}
            Some(now) => {
                let bare = |x: &pbps_model::ExclusionConstraint| {
                    let mut x = x.clone();
                    x.storage_parameters.clear();
                    x
                };
                let edit = (bare(was) == bare(now)).then(|| {
                    parameter_edit(
                        was.method.policy(),
                        &was.storage_parameters,
                        &now.storage_parameters,
                    )
                });
                match edit {
                    // A partitioned parent's index takes no `ALTER INDEX SET`/`RESET`
                    // (42809, measured on 18.6 for its exclusion's index): the edit is a
                    // replacement, whose add carries the final options (DEC-2449.1).
                    Some(ParameterEdit::Alter { .. }) if declared.partition_by.is_some() => {
                        changes.push(drop_half(key, was));
                        changes.push(add_half(key, now));
                    }
                    Some(ParameterEdit::Alter { set, reset }) => {
                        changes.push(Change::Index(IndexChange::SetIndexStorageParameters {
                            table: name.clone(),
                            target: IndexPart::Exclusion(key.clone()),
                            method: was.method,
                            set,
                            reset,
                        }));
                    }
                    Some(ParameterEdit::Refuse { parameter }) => {
                        errs.push(DiffError::IndexParameterRefused {
                            table: name.clone(),
                            target: format!("exclusion constraint {}", key.shown()),
                            method: was.method.as_str(),
                            parameter,
                        });
                    }
                    Some(ParameterEdit::Unchanged) | Some(ParameterEdit::Rebuild) | None => {
                        changes.push(drop_half(key, was));
                        changes.push(add_half(key, now));
                    }
                }
            }
            None if retired(key) => changes.push(drop_half(key, was)),
            None => errs.push(DiffError::ExclusionRefused {
                table: name.clone(),
                name: key.clone(),
                reason: format!(
                    "dropping it (a rename is a drop and an add) needs a recorded reason: run \
                     `pbps drop-exclusion {} {} --reason \"..\"` and commit the identity file \
                     (`--since REV` names the revision that still declares it, if the \
                     declarations that dropped it are committed already)",
                    name.shown(),
                    key.shown()
                ),
            }),
        }
    }
    for (key, now) in &declared.exclude {
        if !base.exclude.contains_key(key) {
            changes.push(add_half(key, now));
        }
    }
}

/// A column change under a standing exclusion constraint (DEC-2448.3, DEC-2448.4,
/// DEC-2448.5). The engine drops an exclusion silently when a plain key column is
/// dropped, and silently rebuilds it, under an exclusive lock, when one is
/// retyped or a generated one has its expression set (measured on 16.15 and
/// 18.6): a guarantee given up, or a rebuild, that no plan states.
///
/// - A **drop** is allowed where the same plan drops the constraint, which is
///   the statement the engine would otherwise have made unseen (a pure drop has
///   passed the retirement gate, a replacement states the drop and the
///   validation). Alone it is refused by name.
/// - A **retype** or an **expression** change where the plan does not already
///   drop or replace the constraint is stated: the constraint's drop and its add
///   of the declared definition are two changes beside it, as a replacement's
///   are, in the classes that put the drop before the column change and the add
///   after it. A conflicting rebuild then fails as one transaction with the old
///   constraint standing, and the plan shows the `destructive` and `constraint`
///   risks the engine would have taken unseen. The definition is declared,
///   because a differing one is a replacement already.
///
/// A key that is an expression, or a predicate, names its columns in text this
/// model does not parse, so a column its text mentions counts as one it holds.
#[allow(clippy::wildcard_enum_match_arm)]
fn exclusion_hazards<'a>(
    name: &TableName,
    base: &Table,
    declared: Option<&Table>,
    changes: impl IntoIterator<Item = &'a Change>,
    dialect: &(impl Dialect + ?Sized),
    errs: &mut Vec<DiffError>,
) -> Vec<Change> {
    let mut stated = Vec::new();
    if base.exclude.is_empty() {
        return stated;
    }
    let changes: Vec<&Change> = changes.into_iter().collect();
    let released = |key: &pbps_model::ObjectLocalName| {
        changes.iter().any(|c| {
            matches!(c, Change::Constraint(ConstraintChange::SetExclusion { table, name: n, to: None, .. })
                if table == name && n == key)
        })
    };
    let handled = |key: &pbps_model::ObjectLocalName| {
        changes.iter().any(|c| {
            matches!(c, Change::Constraint(ConstraintChange::SetExclusion { table, name: n, .. })
                if table == name && n == key)
        })
    };
    for change in changes.iter().copied() {
        let (column, what) = match change {
            Change::Column(ColumnChange::DropColumn { column, .. }) => (column, "dropped"),
            Change::Column(ColumnChange::AlterColumnType { column, .. })
            | Change::Column(ColumnChange::AlterColumnExpression { column, .. }) => {
                (column, "retyped")
            }
            Change::TypeValue(TypeValueChange::EnumRebuild {
                step: pbps_model::EnumRebuildStep::Retype { column, .. },
                ..
            }) => (column, "retyped"),
            _ => continue,
        };
        for (key, x) in &base.exclude {
            if !exclusion_names(x, &column.name, dialect) {
                continue;
            }
            if what == "dropped" {
                if !released(key) {
                    errs.push(DiffError::ExclusionRefused {
                        table: name.clone(),
                        name: key.clone(),
                        reason: format!(
                            "column `{}` is dropped while the constraint holds it; drop the \
                             constraint in the same plan (`pbps drop-exclusion`), or the \
                             engine would drop it without a plan saying so",
                            column.name.shown()
                        ),
                    });
                }
            } else if !handled(key) && !stated.iter().any(|s| exclusion_change_of(s, name, key)) {
                // Gone from the declarations is the retirement gate's refusal, not
                // this one's; a definition that differs is a replacement, and
                // `handled` has seen it.
                if let Some(now) = declared.and_then(|t| t.exclude.get(key)) {
                    stated.push(Change::Constraint(ConstraintChange::SetExclusion {
                        table: name.clone(),
                        name: key.clone(),
                        from: Some(Box::new(x.clone())),
                        to: None,
                    }));
                    stated.push(Change::Constraint(ConstraintChange::SetExclusion {
                        table: name.clone(),
                        name: key.clone(),
                        from: None,
                        to: Some(Box::new(now.clone())),
                    }));
                }
            }
        }
    }
    stated
}

fn exclusion_change_of(
    change: &Change,
    table: &TableName,
    key: &pbps_model::ObjectLocalName,
) -> bool {
    matches!(change, Change::Constraint(ConstraintChange::SetExclusion { table: t, name: n, .. })
        if t == table && n == key)
}

/// Whether an exclusion constraint's keys, payload or predicate hold `column`.
///
/// A key's expression and the predicate are asked of the dialect's own column
/// scan, the one an index key's input columns are asked of
/// ([`Dialect::generation_may_read_column`]): it reads quoted identifiers by
/// the engine's rules (a hyphen, a space, a doubled quote, a mixed case), and
/// leaves string literals and comments out, so the name inside a literal is not
/// a reference. It over-approximates (a function or a field of the same name
/// counts), which is the safe side of a guard.
fn exclusion_names(
    x: &pbps_model::ExclusionConstraint,
    column: &ColumnName,
    dialect: &(impl Dialect + ?Sized),
) -> bool {
    let reads = |text: &str| dialect.generation_may_read_column(text, column);
    x.keys.iter().any(|k| match &k.key.key {
        pbps_model::IndexKey::Column(c) => c == column,
        pbps_model::IndexKey::Expression(text) => reads(text),
    }) || x.include.contains(column)
        || x.filter.as_deref().is_some_and(reads)
}

/// [`diff_constraints`] under a given policy table, so that a verdict no
/// method holds yet can be exercised end to end.
fn diff_constraints_by(
    name: &TableName,
    base: &Table,
    declared: &Table,
    changes: &mut Vec<Change>,
    errs: &mut Vec<DiffError>,
    policy_of: &dyn Fn(IndexMethod) -> &'static pbps_model::storage::MethodPolicy,
) {
    // Storage parameters change in place, every supported one of both
    // methods (measured on 16 and 18), so a part is rebuilt only where it
    // differs otherwise; a difference in them alone is one `ALTER INDEX`
    // (DEC-1442.1). A rebuilt part carries its declared ones in its `CREATE`.
    // That is each parameter's verdict in the method's policy (DEC-2448.1):
    // every one this model holds is `Alter`, and a method that gains a
    // parameter the engine cannot change in place says `Rebuild` or `Refuse`
    // in its own row and nothing here changes.
    fn bare<T: Clone>(part: &T, strip: impl Fn(&mut T)) -> T {
        let mut part = part.clone();
        strip(&mut part);
        part
    }
    let pk_bare = |pk: &PrimaryKey| bare(pk, |p| p.storage_parameters.clear());
    let unique_bare = |u: &UniqueConstraint| bare(u, |u| u.storage_parameters.clear());
    let index_bare = |ix: &Index| bare(ix, |ix| ix.storage_parameters.clear());
    let partitioned = declared.partition_by.is_some();
    let refused = std::cell::RefCell::new(Vec::new());
    // `None` replaces the part, `Some(None)` leaves it, `Some(Some(_))` alters it.
    let parameters = |target: IndexPart,
                      method: IndexMethod,
                      was: &BTreeMap<String, String>,
                      now: &BTreeMap<String, String>|
     -> Option<Option<Change>> {
        match parameter_edit(policy_of(method), was, now) {
            ParameterEdit::Unchanged => Some(None),
            ParameterEdit::Rebuild => None,
            // A partitioned parent's index takes no `ALTER INDEX SET`/`RESET`
            // of any method (42809, measured on 16.15 and 18.6, #2709), so an
            // option the engine would change in place on an ordinary table is
            // a rebuild here: the parent is dropped and created again with
            // the declared options, which its leaves are created from.
            ParameterEdit::Alter { .. } if partitioned => None,
            ParameterEdit::Refuse { parameter } => {
                refused.borrow_mut().push(DiffError::IndexParameterRefused {
                    table: name.clone(),
                    target: match &target {
                        IndexPart::PrimaryKey => "the primary key's index".to_owned(),
                        IndexPart::Unique(n) | IndexPart::Index(n) => {
                            format!("index {}", n.shown())
                        }
                        IndexPart::Exclusion(n) => format!("exclusion constraint {}", n.shown()),
                    },
                    method: method.as_str(),
                    parameter,
                });
                Some(None)
            }
            ParameterEdit::Alter { set, reset } => Some(Some(Change::Index(
                IndexChange::SetIndexStorageParameters {
                    table: name.clone(),
                    target,
                    method,
                    set,
                    reset,
                },
            ))),
        }
    };

    // A declaration that leaves the key unnamed (`primary_key: [id]`) leaves
    // the name to the engine, and the engine invents one (`PK__t__357D...`)
    // that the recorded state then carries. Comparing names there would
    // restate the key on every connected plan until somebody copied the
    // invented name into the file. So an unnamed declaration matches any
    // stored name and only the columns are compared; a *named* declaration is
    // compared in full, because renaming a constraint is a real change.
    //
    // A key that stays but changes layout — clustered to nonclustered or
    // back, because the table's `clustered` selector moved — is a different
    // index under the same constraint, and the engine has no statement that
    // moves a key between the two in place: `DROP_EXISTING` refuses to turn a
    // clustered index nonclustered (1925, measured), and the other direction
    // is refused while a foreign key references the key (1930). So it is
    // replaced like any other changed key (#1178).
    let mut pk_differs = match (&base.primary_key, &declared.primary_key) {
        (Some(b), Some(d)) if d.name.is_none() => b.columns != d.columns,
        (Some(b), Some(d)) => pk_bare(b) != pk_bare(d),
        (b, d) => b != d,
    } || (base.primary_key.is_some()
        && declared.primary_key.is_some()
        && base.primary_key_is_clustered() != declared.primary_key_is_clustered());
    if !pk_differs && let (Some(b), Some(d)) = (&base.primary_key, &declared.primary_key) {
        match parameters(
            IndexPart::PrimaryKey,
            IndexMethod::Btree,
            &b.storage_parameters,
            &d.storage_parameters,
        ) {
            Some(edit) => changes.extend(edit),
            None => pk_differs = true,
        }
    }
    if pk_differs {
        // A key that is *replaced* is emitted as two changes: the old one's
        // drop, and the new one's add. One change carrying both directions can
        // only be in one ordering class, and the two halves need opposite
        // ones — the drop before every column change a standing key blocks
        // (DECISIONS 269), the add after every column its new shape may name.
        // Measured, the plan that came out of one change was refused:
        // `PRIMARY KEY (id)` becoming `PRIMARY KEY (other)` while `id` is
        // relaxed ran `DROP NOT NULL` against a column `pk_t` still held —
        // `42P16` on PostgreSQL, 5074 with 4922 behind it on SQL Server.
        //
        // No model change and no new SQL: both emitters already emit the two
        // halves independently, so the statements are the ones they were and
        // only their positions move. `from: None` on the add half is accurate
        // where it runs, because the drop half has already taken the key away.
        //
        // The risk classes follow, and follow correctly: a replacement now
        // carries `Destructive` for the key it gives up as well as
        // `Constraint` for the key it takes, which is what happens to the
        // database (DECISIONS 270).
        let split = base.primary_key.is_some() && declared.primary_key.is_some();
        changes.push(Change::Constraint(ConstraintChange::SetPrimaryKey {
            table: name.clone(),
            from: base.primary_key.clone(),
            to: if split {
                None
            } else {
                declared.primary_key.clone()
            },
            // Only a key this change adds has a layout: a key that is only
            // dropped claims nothing, and PostgreSQL's emitter refuses a
            // change that says nonclustered.
            nonclustered: !split
                && declared.primary_key.is_some()
                && !declared.primary_key_is_clustered(),
        }));
        if split {
            changes.push(Change::Constraint(ConstraintChange::SetPrimaryKey {
                table: name.clone(),
                from: None,
                to: declared.primary_key.clone(),
                nonclustered: !declared.primary_key_is_clustered(),
            }));
        }
    }

    by_name(
        &base.foreign_keys,
        &declared.foreign_keys,
        |_, b, d| (b == d).then_some(None),
        |n| {
            Change::Constraint(ConstraintChange::DropForeignKey {
                table: name.clone(),
                name: n.clone(),
            })
        },
        |n, c| {
            Change::Constraint(ConstraintChange::AddForeignKey {
                table: name.clone(),
                name: n.clone(),
                constraint: Box::new(c.clone()),
            })
        },
        changes,
    );
    by_name(
        &base.checks,
        &declared.checks,
        |_, b, d| (b == d).then_some(None),
        |n| {
            Change::Constraint(ConstraintChange::DropCheck {
                table: name.clone(),
                name: n.clone(),
            })
        },
        |n, c| {
            Change::Constraint(ConstraintChange::AddCheck {
                table: name.clone(),
                name: n.clone(),
                constraint: c.clone(),
            })
        },
        changes,
    );
    by_name(
        &base.unique,
        &declared.unique,
        |n, b, d| {
            (unique_bare(b) == unique_bare(d)
                && base.unique_is_clustered(n.raw()) == declared.unique_is_clustered(n.raw()))
            .then(|| {
                parameters(
                    IndexPart::Unique(n.clone()),
                    IndexMethod::Btree,
                    &b.storage_parameters,
                    &d.storage_parameters,
                )
            })
            .flatten()
        },
        |n| {
            Change::Constraint(ConstraintChange::DropUnique {
                table: name.clone(),
                name: n.clone(),
            })
        },
        |n, c| {
            Change::Constraint(ConstraintChange::AddUnique {
                table: name.clone(),
                name: n.clone(),
                constraint: c.clone(),
                clustered: declared.unique_is_clustered(n.raw()),
            })
        },
        changes,
    );
    by_name(
        &base.indexes,
        &declared.indexes,
        |n, b, d| {
            (index_bare(b) == index_bare(d)
                && base.index_is_clustered(n.raw()) == declared.index_is_clustered(n.raw()))
            .then(|| {
                parameters(
                    IndexPart::Index(n.clone()),
                    d.method,
                    &b.storage_parameters,
                    &d.storage_parameters,
                )
            })
            .flatten()
        },
        |n| {
            Change::Index(IndexChange::DropIndex {
                table: name.clone(),
                name: n.clone(),
            })
        },
        |n, ix| {
            Change::Index(IndexChange::AddIndex {
                table: name.clone(),
                name: n.clone(),
                index: Box::new(ix.clone()),
                clustered: declared.index_is_clustered(n.raw()),
            })
        },
        changes,
    );
    errs.extend(refused.into_inner());
}

/// Declared reference data (ADR-0004).
///
/// The comparison is by primary-key value, because that is all the identity a
/// row has — no uid, no tombstone and no rename intent, since a row's entire
/// content is declared and recreating one is therefore lossless (the
/// generalized criterion of ADR-0005).
///
/// The asymmetry between the two modes is the whole point of having two:
/// `exact` claims the declaration is the table, so a row the declaration does
/// not mention is deleted; `ensure` claims only that the declared rows are
/// there, so an undeclared row is invisible — SPEC §8.2's managed set, at row
/// granularity.
fn diff_data(
    name: &TableName,
    base: &Table,
    declared: &Table,
    base_name_of: &BTreeMap<ColumnName, ColumnName>,
    changes: &mut Vec<Change>,
    errs: &mut Vec<DiffError>,
) {
    if base.data.as_ref().map(|d| d.mode) != declared.data.as_ref().map(|d| d.mode) {
        changes.push(Change::Row(RowChange::SetDataMode {
            table: name.clone(),
            from: base.data.as_ref().map(|d| d.mode),
            to: declared.data.as_ref().map(|d| d.mode),
        }));
    }

    // Every row change names the column its key goes in, so without one there
    // is nothing to emit and nothing to compare.
    let key_column = match &declared.primary_key {
        Some(pk) if pk.columns.len() == 1 => pk.columns[0].clone(),
        _ if declared.data.is_some() => {
            errs.push(DiffError::DataWithoutKey {
                table: name.clone(),
            });
            return;
        }
        _ => return,
    };

    let identity_key = declared
        .columns
        .get(&key_column)
        .is_some_and(|c| match &c.value_source {
            ValueSource::Identity(_) => true,
            // A `serial` key is refused by its dialect (#1631).
            ValueSource::None
            | ValueSource::Default(_)
            | ValueSource::Sequence(_)
            | ValueSource::Generated(_) => false,
        });

    let Some(declared_data) = &declared.data else {
        // The block was removed. Nothing is deleted and nothing is inserted:
        // removing the opt-in means pbps stops managing these rows, and reading
        // it as "delete them all" would make deleting a *declaration* destroy
        // data — the one thing this tool must never do quietly.
        return;
    };

    // No block on the base side means the table was not managed for rows
    // before, so every declared row is compared against nothing and inserted.
    // `exact` still deletes nothing here: what is in the table is unknown to
    // the baseline, and only the connected drift comparison can see it.
    let base_rows = base.data.as_ref().map(|d| &d.rows);

    // The row keys on each side are values of *that side's* key column. They
    // can only be matched when it is the same column — the same uid — on both
    // sides; renamed is fine, since the values did not move. A key that moved
    // to a different column leaves two sets of keys with nothing in common,
    // and matching them by text would update and delete the wrong rows.
    if base_rows.is_some() {
        let error = match BaselineDataKey::of(base) {
            BaselineDataKey::Absent => {
                // The constraint diff runs first. Retained rows can still be
                // compared using the declared key when this very plan restores
                // it on a column the baseline already has (DECISIONS 538).
                let restored = changes.iter().any(|change| {
                    matches!(change,
                    Change::Constraint(ConstraintChange::SetPrimaryKey { table, from: None, to: Some(pk), .. })
                    if table == name && pk.columns == [key_column.clone()])
                });
                if !restored {
                    Some(DiffError::DataBaselineKeyAbsent {
                        table: name.clone(),
                    })
                } else if !base_name_of
                    .get(&key_column)
                    .is_some_and(|column| base.columns.contains_key(column))
                {
                    // Adding a new key column cannot recover the identity of
                    // retained rows: their map keys predate that column.
                    Some(DiffError::DataBaselineKeyOnNewColumn {
                        table: name.clone(),
                        column: key_column.clone(),
                    })
                } else {
                    None
                }
            }
            BaselineDataKey::Multiple(columns) => Some(DiffError::DataBaselineKeyNotSingle {
                table: name.clone(),
                columns: columns.to_vec(),
            }),
            BaselineDataKey::Single(base_key) => {
                (base_name_of.get(&key_column).map(ColumnName::raw) != Some(base_key.raw())).then(
                    || DiffError::DataKeyColumnChanged {
                        table: name.clone(),
                    },
                )
            }
        };
        if let Some(error) = error {
            errs.push(error);
            return;
        }
    }

    for (key, row) in &declared_data.rows {
        match base_rows.and_then(|r| r.get(key)) {
            None => {
                let (defaults, types) = omitted_defaults(declared, key_column.raw(), row);
                changes.push(Change::Row(RowChange::InsertRow {
                    table: name.clone(),
                    key_column: key_column.clone(),
                    identity_key,
                    key: key.clone(),
                    row: row.clone(),
                    defaults,
                    types,
                }))
            }
            Some(before) => {
                // Only the columns that differ. An UPDATE restating a column
                // that did not change would overwrite a value the declaration
                // and the database already agree on, and would make plan.sql
                // claim a change that is not one.
                let mut columns = BTreeMap::new();
                let mut unchanged = BTreeMap::new();
                let mut types = BTreeMap::new();
                let mut after_types = BTreeMap::new();
                for (column, spec) in &declared.columns {
                    // The key lives in the map key, not in either row, so it
                    // has nothing to compare — and resolving it through the
                    // omission rule would read a default added to the key
                    // column as "set every key to DEFAULT".
                    if *column == key_column {
                        continue;
                    }
                    // A non-key `IDENTITY` column, or a generated one
                    // (DEC-1168.1), is the engine's: never
                    // written by a row (`validate` refuses it) and never read
                    // back (DECISIONS 94), so both sides resolve it to NULL
                    // and it is neither a change nor a cell the row can be
                    // held to — the engine assigned it.
                    if match spec.value_source {
                        pbps_model::ValueSource::Identity(_)
                        | pbps_model::ValueSource::Sequence(_)
                        | pbps_model::ValueSource::Generated(_) => true,
                        pbps_model::ValueSource::None | pbps_model::ValueSource::Default(_) => {
                            false
                        }
                    } {
                        continue;
                    }
                    // Each side against *its own* table, and the base side
                    // under the name the base knew the column by: a renamed
                    // column keeps its values, and looking it up by the new
                    // name would find nothing and restate every row. A column
                    // the base does not have at all is NULL there. A column
                    // that gains a default in this same plan resolves to NULL
                    // on the base and to the default on the declared side,
                    // which is an UPDATE — and the right one: adding a default
                    // does not backfill existing rows, and the declaration
                    // says the row should hold it.
                    let base_spec = base_name_of
                        .get(column)
                        .and_then(|base_column| base.columns.get(base_column));
                    let b = match base_name_of.get(column) {
                        Some(base_column) => {
                            cell(before, base_column.raw(), base.columns.get(base_column))
                        }
                        None => Cell::Value(Value::Null),
                    };
                    let d = cell(row, column.raw(), Some(spec));
                    // The base column's type, so the emitter can compare each
                    // cell by the rendering that read it (DECISIONS 122). A
                    // column the base lacks has no recorded cell to hold the
                    // update to.
                    //
                    // A column this plan *retypes* is carried too, and its
                    // new type goes into `after_types` below: the pair is
                    // what lets the emitter hold the row without spelling
                    // the converted value itself. 146 carried neither and
                    // held nothing, because the recorded text is the old
                    // type's spelling of a value the `AlterColumnType` has
                    // since converted — measured, a `decimal(5,2)` holding
                    // `1.50` reads back as `1` once the column is `int`, and
                    // comparing `N'1.50'` against it is a mismatch either
                    // way round. But dropping the predicate dropped the
                    // stale-row guard with it: a cell another session changed
                    // between the plan's read and the apply is converted by
                    // the `ALTER` and then overwritten by this `UPDATE` with
                    // nothing saying so. The engine can answer what the tool
                    // cannot — it converts the recorded text the same way it
                    // converted the column (DECISIONS 149).
                    if let Some(base_spec) = base_spec {
                        types.insert(column.clone(), base_spec.ty.clone());
                    }
                    // And the type it has once this plan has run, where the
                    // two differ: the column this plan adds, and the column
                    // whose type it changes. Both are in place by the time
                    // the row changes run, so the write's *postcondition*
                    // reads the cell back the way the column will actually
                    // hold it — where carrying only the base type left an
                    // added column held to nothing at all (DECISIONS 140).
                    if base_spec.map(|s| &s.ty) != Some(&spec.ty) {
                        after_types.insert(column.clone(), spec.ty.clone());
                    }
                    if b == d {
                        // Not restated, but still held: the declaration
                        // claims this cell as much as the changed ones, and
                        // the statement checks the whole row (DECISIONS 136).
                        unchanged.insert(column.clone(), d);
                    } else {
                        columns.insert(column.clone(), (b, d));
                    }
                }
                if !columns.is_empty() {
                    changes.push(Change::Row(RowChange::UpdateRow {
                        table: name.clone(),
                        key_column: key_column.clone(),
                        key: key.clone(),
                        columns,
                        unchanged,
                        types,
                        after_types,
                        // The declared type: any retype of the key sorts
                        // before the row changes, so it is the one the
                        // `WHERE` meets (DEC-1564.2).
                        key_type: declared.columns.get(&key_column).map(|c| c.ty.clone()),
                    }));
                }
            }
        }
    }

    // `exact` only. `ensure` never emits a DELETE — that is the promise the
    // mode makes to a table the application also writes to.
    if declared_data.mode == DataMode::Exact
        && let Some(base_rows) = base_rows
    {
        // The baseline names a surviving identity still has, asked once for
        // the table: the answer is the same for every deleted row, and a scan
        // per baseline column of every row made an `exact` table's deletes
        // cost rows × columns × columns in string comparisons (#402).
        let surviving: BTreeSet<&str> = base_name_of.values().map(ColumnName::raw).collect();
        for (key, before) in base_rows {
            if !declared_data.rows.contains_key(key) {
                // The recorded row travels with the delete, so the statement
                // removes what the reviewer saw rather than whatever holds
                // the key when it runs (DECISIONS 143).
                //
                // Values come from the *base* row, under the names the base
                // knew them by; the predicate names them as the table has
                // them when the DELETE runs, which is the declared name.
                // Every column change sorts before the row changes
                // (`order_key`), so a column renamed by this same plan is
                // already renamed by then, and one this plan drops is gone —
                // it holds nothing, and naming it would be a predicate on a
                // column that no longer exists.
                let mut row = BTreeMap::new();
                let mut types = BTreeMap::new();
                let mut after_types = BTreeMap::new();
                for (declared_column, base_column) in base_name_of {
                    // The key is the map key, and a non-key IDENTITY is the
                    // engine's — neither is a cell a row can be held to, for
                    // the same reasons as in an update.
                    if *declared_column == key_column {
                        continue;
                    }
                    let Some(spec) = base.columns.get(base_column) else {
                        continue;
                    };
                    if match spec.value_source {
                        pbps_model::ValueSource::Identity(_)
                        | pbps_model::ValueSource::Sequence(_)
                        | pbps_model::ValueSource::Generated(_) => true,
                        pbps_model::ValueSource::None | pbps_model::ValueSource::Default(_) => {
                            false
                        }
                    } {
                        continue;
                    }
                    row.insert(
                        declared_column.clone(),
                        cell(before, base_column.raw(), Some(spec)),
                    );
                    // The type the recorded text was read in, and — where
                    // this plan retypes the column — the type the column has
                    // by the time the `DELETE` runs, since `AlterColumnType`
                    // sorts before every row change. The emitter needs both
                    // to hold the row: one spelling of the cell is the
                    // recorded one and the other is the stored one, and only
                    // the engine can turn the first into the second
                    // (DECISIONS 149, where 146 held nothing at all).
                    //
                    // Only surviving identities have predicate types. Dropped
                    // columns are carried separately under their base names.
                    let Some(declared_ty) = declared.columns.get(declared_column).map(|d| &d.ty)
                    else {
                        continue;
                    };
                    types.insert(declared_column.clone(), spec.ty.clone());
                    if *declared_ty != spec.ty {
                        after_types.insert(declared_column.clone(), declared_ty.clone());
                    }
                }
                // The UID intersection above deliberately has no name for a
                // dropped column. Keep its baseline cell for review under the
                // only name it had, without inventing a declared identity or
                // a predicate on a column that is already gone. A historical
                // rename may reuse this base name for a surviving column, so
                // reviewer-only cells need their own map (DECISIONS 442).
                let mut dropped = BTreeMap::new();
                for (base_column, spec) in &base.columns {
                    if (match spec.value_source {
                        pbps_model::ValueSource::Identity(_)
                        | pbps_model::ValueSource::Sequence(_)
                        | pbps_model::ValueSource::Generated(_) => true,
                        pbps_model::ValueSource::None | pbps_model::ValueSource::Default(_) => {
                            false
                        }
                    }) || surviving.contains(base_column.raw())
                    {
                        continue;
                    }
                    dropped.insert(
                        base_column.clone(),
                        cell(before, base_column.raw(), Some(spec)),
                    );
                }
                changes.push(Change::Row(RowChange::DeleteRow {
                    table: name.clone(),
                    key_column: key_column.clone(),
                    key: key.clone(),
                    cause: DeleteCause::Undeclared,
                    row,
                    dropped,
                    types,
                    after_types,
                    key_type: declared.columns.get(&key_column).map(|c| c.ty.clone()),
                }));
            }
        }
    }
}

/// Position in the dependency order, by table name.
fn rank_of_tables(order: &[TableName]) -> BTreeMap<TableName, usize> {
    order
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), i))
        .collect()
}

/// Position in the dependency order, by identity.
fn rank_of(order: &[ModuleId]) -> BTreeMap<ModuleId, usize> {
    order
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), i))
        .collect()
}

/// How deep among the roles this plan drops each dropped role sits: a role
/// no other dropped role holds is 0, and each membership step adds one.
///
/// A connected plan removes a dropped role's members by name before its
/// `DROP ROLE`, and the engine refuses `ALTER ROLE [z] DROP MEMBER [a]` once
/// `a` is gone — measured, along with the fact that dropping `a` while it is
/// a member of `z` succeeds and takes the membership with it. So a role
/// holding another dropped role has to go *first*, and with every drop at
/// rank 0 the order was left to the name tiebreaker: dropping `a` before `z`
/// rolled the whole apply back (DECISIONS 127).
///
/// Wildcarded deliberately: no change other than a role drop can put a role
/// into this order, and one added later could not without also being a drop.
///
/// Membership among roles cannot be cyclic, and the walk is bounded by the
/// number of drops regardless, so a catalog that somehow held a cycle costs
/// a wrong order rather than a hang.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn member_depth(planned: &[PlannedChange]) -> BTreeMap<pbps_model::RoleName, usize> {
    let dropped: BTreeMap<&pbps_model::RoleName, &[pbps_model::RoleName]> = planned
        .iter()
        .filter_map(|p| match &p.change {
            Change::Role(RoleChange::DropRole { name, members, .. }) => {
                Some((name, members.as_slice()))
            }
            _ => None,
        })
        .collect();
    let mut depth: BTreeMap<pbps_model::RoleName, usize> =
        dropped.keys().map(|n| ((*n).to_owned(), 0)).collect();
    for _ in 0..dropped.len() {
        let mut moved = false;
        for (holder, members) in &dropped {
            let above = depth[*holder];
            for member in *members {
                if let Some(d) = depth.get_mut(member)
                    && *d <= above
                {
                    *d = above + 1;
                    moved = true;
                }
            }
        }
        if !moved {
            break;
        }
    }
    depth
}

/// How many dictionaries deep `name` is among the text search objects of `schema`, at least
/// what `start` gives it: a dictionary mapping or naming none is 0. A cycle is the engine's
/// to refuse, so a reference back to an object being measured counts nothing.
fn search_depth(
    schema: &Schema,
    name: &pbps_model::SearchRef,
    start: &dyn Fn(&pbps_model::SearchRef, &pbps_model::SearchObject) -> isize,
    seen: &mut BTreeSet<pbps_model::SearchRef>,
) -> isize {
    if !seen.insert(name.clone()) {
        return 0;
    }
    let depth = schema.search.get(name).map_or(0, |object| {
        object
            .dictionaries()
            .iter()
            .filter(|used| schema.search.contains_key(*used))
            .map(|used| 1 + search_depth(schema, used, start, seen))
            .max()
            .unwrap_or(0)
            .max(start(name, object))
    });
    seen.remove(name);
    depth
}

/// Where a change sorts *within* its ordering class.
///
/// Modules, reference rows and foreign keys each have a dependency order among
/// their class; everything else is zero and keeps the tiebreakers that were
/// already there. In every family the removing direction is the reverse of the
/// creating one, so a dependent goes before the thing it depends on.
///
/// Modules and rows carry a computed rank because their dependencies form a
/// graph. A foreign key's is a two-level layering — it depends on candidate
/// keys, and nothing depends on it — so a constant says it exactly.
fn dependency_rank(
    change: &Change,
    create_rank: &BTreeMap<ModuleId, usize>,
    drop_rank: &BTreeMap<ModuleId, usize>,
    data_rank: &BTreeMap<TableName, usize>,
    role_rank: &BTreeMap<pbps_model::RoleName, usize>,
    type_rank: &BTreeMap<pbps_model::TypeRef, isize>,
    drop_type_rank: &BTreeMap<pbps_model::TypeRef, isize>,
) -> isize {
    match change {
        Change::Partition(PartitionChange::RebuildPartitionedTable(_))
        | Change::Index(IndexChange::BuildPartitionedIndex(_))
        | Change::Partition(PartitionChange::CopyTableRows(_))
        | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => {
            unreachable!("rebuild operations are expanded by order, never ranked by the differ")
        }
        Change::Module(ModuleChange::CreateModule { id, .. }) | Change::Module(ModuleChange::AlterModule { id, .. }) => {
            create_rank.get(id).map_or(0, |r| *r as isize)
        }
        Change::Module(ModuleChange::DropModule { id, .. }) => -(drop_rank.get(id).map_or(0, |r| *r as isize)),
        // Module ranks can be arbitrarily deep. A fixed -1 would leave a
        // policy behind a dropped routine it calls (DEC-1173.2).
        Change::Policy(PolicyChange::DropPolicy { .. }) => -(drop_rank.len() as isize) - 1,
        Change::TypeValue(TypeValueChange::AddEnumValue { ordinal, .. }) | Change::TypeValue(TypeValueChange::RenameEnumValue { ordinal, .. }) => {
            *ordinal as isize
        }
        // After every label change of class 7, which it may name.
        Change::TypeValue(TypeValueChange::AlterDomain {
            half: pbps_model::DomainHalf::Loosen,
            from,
            to,
            ..
        }) if domain_sets_default(from, to) => isize::MAX,
        Change::TypeValue(TypeValueChange::AlterDomain { .. }) => 0,
        Change::TypeValue(TypeValueChange::EnumRebuild {
            step: pbps_model::EnumRebuildStep::Create { ordinal, .. },
            ..
        }) => *ordinal as isize,
        // With a retype: after the defaults taken off for it (-2) and before
        // the ones written for the type it ends with (0). The swap follows
        // every column of its type because they share a subject and rank and
        // the rendering that breaks the tie names `Retype` before `Swap`
        // (pinned by `the_whole_plan_drops_and_restores_what_names_a_moved_column_and_orders_the_steps`).
        Change::TypeValue(TypeValueChange::EnumRebuild { .. }) => -1,
        Change::Type(TypeChange::CreateType { name, .. }) | Change::Type(TypeChange::RenameType { to: name, .. }) => {
            *type_rank.get(name).unwrap_or(&0)
        }
        Change::Type(TypeChange::DropType { name, .. }) => -drop_type_rank.get(name).copied().unwrap_or(0),
        Change::Table(TableChange::CreateTable { .. })
        // Ranked by their depth where the sort key is built (`search_rank`).
        | Change::Search(SearchChange::CreateSearch { .. })
        | Change::Search(SearchChange::AlterSearchMapping { .. })
        | Change::Search(SearchChange::AlterSearchDictionary { .. })
        | Change::Search(SearchChange::DropSearch { .. })
        | Change::Extension(ExtensionChange::CreateExtension { .. })
        | Change::Extension(ExtensionChange::DropExtension { .. })
        | Change::Extension(ExtensionChange::UpdateExtension { .. })
        | Change::Extension(ExtensionChange::SetExtensionSchema { .. })
        | Change::Table(TableChange::DropTable { .. })
        | Change::Partition(PartitionChange::DetachPartition { .. })
        | Change::Partition(PartitionChange::AttachPartition { .. })
        | Change::Table(TableChange::RenameTable { .. })
        | Change::Column(ColumnChange::AddColumn { .. })
        | Change::Column(ColumnChange::DropColumn { .. })
        | Change::Column(ColumnChange::RenameColumn { .. })
        | Change::Column(ColumnChange::SetColumnDeprecated { .. })
        | Change::TableProperty(TablePropertyChange::SetComment { .. })
        | Change::Index(IndexChange::SetIndexStorageParameters { .. })
        | Change::TableProperty(TablePropertyChange::SetTablePersistence { .. })
        | Change::Policy(PolicyChange::CreatePolicy { .. })
        | Change::Policy(PolicyChange::SetRowSecurity { .. })
        | Change::TableProperty(TablePropertyChange::SetStorageParameters { .. })
        | Change::Constraint(ConstraintChange::DropUnique { .. })
        | Change::Constraint(ConstraintChange::SetExclusion { .. })
        | Change::Constraint(ConstraintChange::AddCheck { .. })
        | Change::Constraint(ConstraintChange::DropCheck { .. })
        | Change::Index(IndexChange::DropIndex { .. })
        | Change::Column(ColumnChange::AddComputedColumn { .. })
        | Change::Column(ColumnChange::DropComputedColumn { .. })
        | Change::Row(RowChange::SetDataMode { .. })
        | Change::Row(RowChange::SetEnsureKeys { .. }) => 0,
        // The clustered index goes in ahead of the rest of the addition
        // class. Building it rebuilds every nonclustered index already on the
        // table, whose row locators become its key, so an index added before
        // it is built twice (#1178). Correctness needs nothing here: the old
        // clustered index is gone by now, since every drop is in class 2.
        //
        // A key with no layout to speak of — every PostgreSQL key, whose
        // `nonclustered` is always false — moves ahead of its siblings too,
        // which orders nothing that depends on it.
        Change::Constraint(ConstraintChange::SetPrimaryKey {
            to: Some(_),
            nonclustered: false,
            ..
        })
        | Change::Constraint(ConstraintChange::AddUnique {
            clustered: true, ..
        })
        | Change::Index(IndexChange::AddIndex {
            clustered: true, ..
        }) => -1,
        Change::Constraint(ConstraintChange::SetPrimaryKey { .. }) | Change::Constraint(ConstraintChange::AddUnique { .. }) | Change::Index(IndexChange::AddIndex { .. }) => 0,
        // After every addition of its class, the index it names among them,
        // and the foreign keys too: nothing in the class reads it.
        Change::TableProperty(TablePropertyChange::SetReplicaIdentity { .. }) => 2,
        // A foreign key needs the key it references to exist, so it goes last
        // in the addition class and first in the drop class. Measured on the
        // pinned image, both halves are real: added before its key, the engine
        // refuses with 1776, "There are no primary or candidate keys in the
        // referenced table ... that match the referencing column list"; and
        // the key cannot be dropped while the foreign key stands (3727 for a
        // `UNIQUE` constraint, 3723 for a unique index).
        //
        // A constant, not a rank computed from the referenced columns, because
        // the dependency here is a total layering rather than a graph: every
        // supplier of a candidate key — `SetPrimaryKey`, `AddUnique` and
        // `AddIndex`, since a foreign key may reference a plain unique index —
        // is above every foreign key, and no foreign key supplies a key to
        // another. Matching suppliers by name would add a way to fail (a
        // supplier not recognised, and the ordering silently back) and buy
        // nothing. Everything else in both classes keeps rank 0 and the
        // tiebreaker it had.
        Change::Constraint(ConstraintChange::AddForeignKey { .. }) => 1,
        Change::Constraint(ConstraintChange::DropForeignKey { .. }) => -1,
        // A column's new type goes in before a default written for it. They
        // share class 9 and the tiebreaker below is the change's rendering,
        // which sorts `AlterColumnDefault` ahead of `AlterColumnType` by the
        // alphabet — so a default that only fits the new type was set against
        // the old one. Measured, PostgreSQL refuses `SET DEFAULT 'abc'` on an
        // `integer` column with "invalid input syntax for type integer", and
        // the plan that widened the column to `text` in the next statement
        // rolled back.
        //
        // A constant, like the foreign key's above and for the same reason:
        // the dependency is a layering, not a graph. A type change never needs
        // a default that is already there, and the nullability travels inside
        // the type change rather than beside it.
        Change::Column(ColumnChange::AlterColumnType { .. }) => -1,
        // And the old default goes before the type, which makes the three
        // phases a column can need one rank each: drop the default the old
        // type gave meaning to, change the type, install the default written
        // for the new one. Measured, SQL Server refuses the middle statement
        // while the first one's constraint stands (5074 with 4922 behind it),
        // and `diff_columns` splits a *replaced* default into these two halves
        // when the type moves, so both ranks have something to order.
        Change::Column(ColumnChange::AlterColumnDefault { to: None, .. }) => -2,
        Change::Column(ColumnChange::AlterColumnDefault { to: Some(_), .. }) => 0,
        // After any type change, as a new default is: the expression is
        // recomputed under the column's final type (DEC-1168.1).
        Change::Column(ColumnChange::AlterColumnExpression { .. }) => 0,
        // A numbering source (#1889, DEC-1889.1). One given up, and an
        // identity turned into a sequence, go first: ahead of the relaxation
        // its column may also take, since measured on 16 and 18 `DROP NOT
        // NULL` on an identity column is refused ("is an identity column"),
        // and ahead of the plain default it may give way to, which is set at
        // 0 and which giving up a sequence's default would drop (#1930
        // review). Anything else goes last: an identity
        // is added only to a column already `NOT NULL` ("must be declared
        // NOT NULL before identity can be added"), so after the tightening,
        // and a sequence is moved past the column's values in the type the
        // column ends with, so after the retype.
        Change::Column(ColumnChange::AlterColumnNumbering {
            from: ValueSource::Identity(_) | ValueSource::Sequence(_),
            to: ValueSource::None,
            ..
        })
        | Change::Column(ColumnChange::AlterColumnNumbering {
            from: ValueSource::Identity(_),
            to: ValueSource::Sequence(_),
            ..
        }) => -3,
        Change::Column(ColumnChange::AlterColumnNumbering { .. }) => 2,
        // A recomputation is checked against the column's nullability as it
        // stands, so a relaxation goes before one and a tightening after it.
        // Measured on 17.11, `SET EXPRESSION` that yields a NULL is refused
        // under `NOT NULL` ("contains null values") and accepted once
        // `DROP NOT NULL` has run. Nothing else in the class reads nullability.
        Change::Column(ColumnChange::AlterColumnNullability {
            to_nullable: true, ..
        }) => -1,
        Change::Column(ColumnChange::AlterColumnNullability {
            to_nullable: false, ..
        }) => 1,
        // A partition's own, alike: its column's type is its parent's, which
        // no change here moves (#1581).
        Change::Partition(PartitionChange::SetPartitionDefault { .. }) => 0,
        Change::Partition(PartitionChange::SetPartitionNotNull {
            not_null: false, ..
        }) => -1,
        Change::Partition(PartitionChange::SetPartitionNotNull { not_null: true, .. }) => 1,
        // Rows follow the foreign keys between their tables: a referenced
        // table's rows go in first, and out last.
        Change::Row(RowChange::InsertRow { table, .. }) | Change::Row(RowChange::UpdateRow { table, .. }) => {
            data_rank.get(table).map_or(0, |r| *r as isize)
        }
        Change::Row(RowChange::DeleteRow { table, .. }) => -(data_rank.get(table).map_or(0, |r| *r as isize)),
        // Two roles dropped together are ordered by membership: see
        // [`member_depth`].
        Change::Role(RoleChange::DropRole { name, .. }) => role_rank.get(name).map_or(0, |r| *r as isize),
        // The rest depend on nothing among themselves; a grant's target is
        // ordered by the class of the change, not by rank.
        Change::Role(RoleChange::CreateRole { .. })
        | Change::Role(RoleChange::RenameRole { .. })
        | Change::Permission(GrantChange::Grant { .. })
        | Change::Permission(GrantChange::Revoke { .. }) => 0,
        // The rank of the routine it settles, so it sorts beside that
        // routine's `CREATE` in class 14 (see `order_key`, #687).
        Change::Permission(GrantChange::PublicExecution { routine, .. }) => create_rank
            .get(&ModuleId::Routine(routine.clone()))
            .map_or(0, |r| *r as isize),
    }
}

/// Roles are matched by **uid** (ADR-0005), and their grants by target after
/// the base side's object names have been brought forward through this plan's
/// table renames — a grant follows its object through `sp_rename`, so a
/// renamed table must not come out as a revoke on the old name plus a grant
/// on the new one.
///
/// A revoke on an object this same plan drops is not emitted: the drop takes
/// the permission with it, and a `REVOKE` that ran after it would fail on an
/// object that is gone (and one ordered before it would be noise).
fn diff_roles(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl Dialect + ?Sized),
    changes: &mut Vec<Change>,
) {
    // Whether the *principal* is this tool's at all (ADR-0010 §3,
    // DECISIONS 211). A SQL Server database role lives in the one database
    // this connection is to, so its existence is managed here; a PostgreSQL
    // role is a cluster object, and a plan that created, renamed or dropped
    // one would reach every other database in the cluster.
    //
    // What a role *holds in this database* is managed either way, so only the
    // three identity changes are conditional and every `Grant` and `Revoke`
    // below is built the same on both engines.
    let manages_roles = dialect.manages_roles();
    // The table marker can stay equal while its set of sequences grows. Its
    // statement discovers that set after the column changes (DEC-1892.1).
    let sequence_creates: BTreeSet<_> = changes
        .iter()
        .flat_map(|change| {
            if let Change::Column(ColumnChange::AddColumn {
                table,
                name,
                column,
                ..
            }) = change
            {
                return matches!(column.value_source, ValueSource::Sequence(_))
                    .then(|| table.column(name))
                    .into_iter()
                    .collect::<Vec<_>>();
            }
            if let Change::Column(ColumnChange::AlterColumnNumbering {
                column, from, to, ..
            }) = change
            {
                return (matches!(to, ValueSource::Sequence(_))
                    && !matches!(from, ValueSource::Sequence(_)))
                .then(|| column.clone())
                .into_iter()
                .collect();
            }
            if let Change::Table(TableChange::CreateTable { name, table, .. }) = change {
                return table
                    .columns
                    .iter()
                    .filter(|(_, c)| matches!(c.value_source, ValueSource::Sequence(_)))
                    .map(|(column, _)| name.column(column))
                    .collect();
            }
            Vec::new()
        })
        .collect();
    let detaching: BTreeSet<_> = changes
        .iter()
        .filter_map(|change| {
            if let Change::Partition(PartitionChange::DetachPartition { uid, .. }) = change {
                Some(uid.clone())
            } else {
                None
            }
        })
        .collect();
    let effective = |role: &pbps_model::Role, schema: &Schema| {
        let mut grants = pbps_model::role::effective_grants(role, &schema.tables);
        // Keep the compact marker as the existing Grant statement. Individual
        // column USAGE entries below are the union used for comparisons.
        for (target, rights) in &role.grants {
            if rights.contains(&Permission::SerialSequences) {
                grants
                    .entry(target.clone())
                    .or_default()
                    .insert(Permission::SerialSequences);
            }
        }
        grants
    };
    let mut dropped: Vec<pbps_model::Dropped> = changes.iter().filter_map(Change::drops).collect();
    // `Change::drops` answers `None` for `AlterModule` — the object is
    // restated, not removed, and every other reader of it wants exactly that
    // (DECISIONS 158's caller, and the staged-run rewind in
    // `pbps-cli::deploy`, both ask "is this identity gone for good"). But a
    // dialect that rebuilds a module on every edit (`rebuilds_modules`,
    // ADR-0009 §3, #248) takes the object's grants with it too, precisely as
    // a `DropModule` does — so *this* comparison, and only this one, treats
    // an `AlterModule` the same way: a grant unchanged by the declarations is
    // still gone from the rebuilt object and has to be written into the plan
    // again, or it silently does not come back.
    if dialect.rebuilds_modules() {
        dropped.extend(changes.iter().filter_map(|c| {
            if let Change::Module(ModuleChange::AlterModule { id, .. }) = c {
                Some(pbps_model::Dropped::Module(id.clone()))
            } else {
                None
            }
        }));
    }
    // Base table name -> the name it has after this plan, by uid.
    let renamed: BTreeMap<&TableName, &TableName> = base
        .ids
        .tables
        .iter()
        .filter_map(|(uid, name)| declared.ids.tables.get(uid).map(|to| (name, to)))
        .filter(|(from, to)| from != to)
        .collect();
    // A name that passes to another table (DEC-1118.1). The baseline's grants
    // under it belonged to a table this plan drops, and went with it: the
    // occupant's own grants are the ones carried here by `forward`, so the
    // name is compared as the occupant's, not as a dropped object's.
    let changing_hands = names_changing_hands(base, declared);
    let base_uid: BTreeMap<&TableName, &Uid> = base
        .ids
        .tables
        .iter()
        .map(|(uid, name)| (name, uid))
        .collect();
    let detached_usage = |role: &pbps_model::Role, target: &GrantTarget| {
        let GrantTarget::Sequence(column) = target else {
            return false;
        };
        role.grants.iter().any(|(target, rights)| {
            let GrantTarget::Object(table) = target else {
                return false;
            };
            rights.contains(&Permission::SerialSequences)
                && base_uid
                    .get(table)
                    .is_some_and(|uid| detaching.contains(*uid))
                && pbps_model::role::serial_grant_columns(table, &base.schema.tables)
                    .contains(column)
        })
    };
    let dropped_with_its_table = |target: &GrantTarget| -> bool {
        match target {
            GrantTarget::Object(o) => base_uid
                .get(o)
                .is_some_and(|uid| !declared.ids.tables.contains_key(*uid)),
            GrantTarget::Sequence(c) => base.ids.columns.iter().any(|(uid, from)| {
                // Keeping the column UID does not keep its serial sequence:
                // changing to a default or identity removes the old sequence
                // and its grants, so no separate revoke is needed.
                from == c
                    && declared
                        .ids
                        .columns
                        .get(uid)
                        .and_then(|column| {
                            declared
                                .schema
                                .tables
                                .get(&column.table)
                                .and_then(|table| table.columns.get(&column.name))
                        })
                        .is_none_or(|column| {
                            !matches!(column.value_source, ValueSource::Sequence(_))
                        })
            }),
            GrantTarget::Routine(_) | GrantTarget::Schema(_) => false,
        }
    };
    let changing_hands_target = |target: &GrantTarget| match target {
        GrantTarget::Object(table) => changing_hands.contains(table),
        GrantTarget::Sequence(column) => changing_hands.contains(&column.table),
        GrantTarget::Routine(_) | GrantTarget::Schema(_) => false,
    };
    let forward = |target: &GrantTarget| -> GrantTarget {
        match target {
            GrantTarget::Object(o) => match renamed.get(o) {
                Some(to) => GrantTarget::Object((*to).clone()),
                None => target.clone(),
            },
            // A routine's identity moves with its name, so a rename of the
            // object carries the signature with it unchanged: only tables are
            // renamed by uid here, and a routine is not a table.
            GrantTarget::Sequence(column) => {
                let final_column = base
                    .ids
                    .columns
                    .iter()
                    .find(|(_, c)| *c == column)
                    .and_then(|(uid, _)| declared.ids.columns.get(uid))
                    .cloned()
                    .unwrap_or_else(|| {
                        let table = renamed.get(&column.table).copied().unwrap_or(&column.table);
                        table.column(&column.name)
                    });
                GrantTarget::Sequence(final_column)
            }
            GrantTarget::Routine(_) | GrantTarget::Schema(_) => target.clone(),
        }
    };

    for (uid, name) in &base.ids.roles {
        if declared.ids.roles.contains_key(uid) {
            continue;
        }
        if manages_roles {
            changes.push(Change::Role(RoleChange::DropRole {
                uid: uid.clone(),
                name: name.clone(),
                // Only a connected plan can know them; see `Change::DropRole`.
                members: Vec::new(),
            }));
            continue;
        }
        // The role stays; what it was granted **here** does not. Anything
        // else would be one of the two silent wrong answers: a plan that says
        // "drop role" and runs nothing leaves a principal still holding every
        // permission pbps was managing, and a plan that really dropped it
        // would take the role out of every other database in the cluster.
        //
        // The revokes are the same shape as a narrowing of a role that stays,
        // so the risk classification and the approval gate see them as what
        // they are: access being taken away.
        let Some(held) = base.schema.roles.get(name) else {
            continue;
        };
        for (target, permissions) in &pbps_model::role::effective_grants(held, &base.schema.tables)
        {
            if dropped_with_its_table(target) {
                continue;
            }
            let mut permissions = permissions.clone();
            // Detach removes the implication, not the root's sequence ACL.
            // Leave that access to the live read's explicit column target
            // (DEC-2199.1), rather than revoking through the departed partition.
            if detached_usage(held, target) {
                permissions.remove(&Permission::Usage);
            }
            let target = forward(target);
            // A `REVOKE` on an object this same plan drops would fail on an
            // object that is gone — the same rule the per-target comparison
            // below applies, and for the same reason.
            let target_dropped =
                dropped.iter().any(|d| d.takes(&target)) && !changing_hands_target(&target);
            if permissions.is_empty() || target_dropped {
                continue;
            }
            changes.push(Change::Permission(GrantChange::Revoke {
                role: name.clone(),
                target,
                permissions,
            }));
        }
    }

    for (uid, name) in &declared.ids.roles {
        let Some(role) = declared.schema.roles.get(name) else {
            continue;
        };
        let Some(base_name) = base.ids.roles.get(uid) else {
            // Where the principal is not this tool's, whether the cluster
            // actually has this role is a question only a connection can
            // answer, and it is asked there: `pbps_pg::roles::missing_roles`
            // refuses a declared role the cluster lacks with the `CREATE ROLE`
            // to run by hand. Emitting a `CreateRole` here instead would
            // refuse the ordinary case as well — a DBA creating the role and
            // the project then declaring it, which is the only way a role ever
            // comes under management on that engine.
            if manages_roles {
                changes.push(Change::Role(RoleChange::CreateRole {
                    uid: uid.clone(),
                    name: name.clone(),
                }));
            }
            for (target, permissions) in &role.grants {
                if !permissions.is_empty() {
                    changes.push(Change::Permission(GrantChange::Grant {
                        role: name.clone(),
                        target: target.clone(),
                        permissions: permissions.clone(),
                    }));
                }
            }
            continue;
        };
        // A rename, where the principal is the cluster's, is a rename a human
        // performed there — and on this engine an ACL entry holds the role's
        // oid rather than its name, so every grant followed it and nothing has
        // to be re-granted.
        //
        // **This elision is sound only behind the connected check**
        // (`pbps_pg::roles::rename_evidence`), and the check is not "does the
        // new name exist". If the *old* name is still there as well, the two
        // are two principals: emitting nothing would leave the old one holding
        // everything pbps was managing while the declared one holds nothing,
        // and the plan would then record the declared one as holding it all.
        // The evidence that makes the rename a rename is the old name's
        // **absence**; and if the old role was dropped and a new one created
        // instead, its grants went with it, the pull shows the new role
        // holding nothing, and every declared grant is planned here anyway.
        if base_name != name && manages_roles {
            changes.push(Change::Role(RoleChange::RenameRole {
                uid: uid.clone(),
                from: base_name.clone(),
                to: name.clone(),
            }));
        }
        // The base grants, keyed by the name the target has after this plan.
        let mut before: BTreeMap<GrantTarget, BTreeSet<Permission>> = BTreeMap::new();
        let mut detached = BTreeSet::new();
        if let Some(b) = base.schema.roles.get(base_name) {
            for (target, permissions) in &effective(b, base.schema) {
                if dropped_with_its_table(target) {
                    continue;
                }
                if detached_usage(b, target) {
                    detached.insert(forward(target));
                }
                before
                    .entry(forward(target))
                    .or_default()
                    .extend(permissions.iter().copied());
            }
        }
        let desired = effective(role, declared.schema);
        let targets: BTreeSet<&GrantTarget> = before.keys().chain(desired.keys()).collect();
        for target in targets {
            // By whole identity: where routines overload, the drop of one
            // takes its own grants and leaves the sibling's to be compared.
            let target_dropped =
                dropped.iter().any(|d| d.takes(target)) && !changing_hands_target(target);
            // A DROP takes the object's permissions with it. An object this
            // plan drops and creates again under the same name — a table
            // replaced by a new one, a module changing kind — therefore has
            // *no* grants after the DROP, whatever the base held, and every
            // declared permission on it is a GRANT to write after the CREATE.
            // Comparing the two grant sets as text called them equal and
            // left the role without its access until a later plan noticed.
            let b = if target_dropped {
                BTreeSet::new()
            } else {
                before.get(target).cloned().unwrap_or_default()
            };
            let d = desired.get(target).cloned().unwrap_or_default();
            let mut added: BTreeSet<Permission> = d.difference(&b).copied().collect();
            if d.contains(&Permission::SerialSequences)
                && matches!(target, GrantTarget::Object(table) if !pbps_model::role::serial_grant_columns(table, &declared.schema.tables).is_disjoint(&sequence_creates))
            {
                added.insert(Permission::SerialSequences);
            }
            if let GrantTarget::Sequence(column) = target {
                if sequence_creates.contains(column) {
                    added.extend(d.iter().copied());
                }
                if role
                    .grants
                    .iter()
                    .any(|(target, rights)| matches!(target, GrantTarget::Object(table)
                        if rights.contains(&Permission::SerialSequences)
                        && pbps_model::role::serial_grant_columns(table, &declared.schema.tables).contains(column)))
                {
                    // The table marker grants this same ACL entry once.
                    added.remove(&Permission::Usage);
                }
            }
            let mut removed: BTreeSet<Permission> = b.difference(&d).copied().collect();
            // A marker's narrowing is settled per column: retained explicit
            // USAGE must never be revoked by a table-wide statement.
            removed.remove(&Permission::SerialSequences);
            if detached.contains(target) {
                removed.remove(&Permission::Usage);
            }
            if !added.is_empty() {
                changes.push(Change::Permission(GrantChange::Grant {
                    role: name.clone(),
                    target: target.clone(),
                    permissions: added,
                }));
            }
            if !removed.is_empty() && !target_dropped {
                changes.push(Change::Permission(GrantChange::Revoke {
                    role: name.clone(),
                    target: target.clone(),
                    permissions: removed,
                }));
            }
        }
    }
}

/// Settles what happens to the engine's default `EXECUTE` to `PUBLIC` on
/// every routine this plan brings into being (ADR-0010 §5, DECISIONS 371).
///
/// # Why this is a separate pass and not part of `diff_modules`
///
/// Because "brings into being" has three spellings and they are produced in
/// three different places: a routine the declarations have newly added, a
/// routine whose *kind* changed and which `diff_modules` therefore emits as a
/// drop plus a create, and — on an engine that rebuilds modules (ADR-0009 §3)
/// — every `AlterModule`, including the ones `rebound_modules` synthesizes
/// after the declarations were compared. All three end with a `CREATE`, and a
/// `CREATE` is what restores the default. Reading the built change list is
/// the only place that sees all three; a hook inside the comparison would
/// have seen the first.
///
/// # Why the opt-in is a hint and the revoke is the default
///
/// `PUBLIC` is context, never a compared grant, so neither side of the
/// comparison holds it and the differ cannot be *asked* for convergence here
/// — it can only be told what to write. Told nothing, it writes the revoke:
/// the routine may be `SECURITY DEFINER`, in which case the engine default
/// means "any principal that can reach this schema may act as the owner", and
/// which routines are definer routines is inside a body this tool never
/// parses. A declaration that wants the default back says so by name, in a
/// line that goes through the merge request and into the plan's checksum like
/// everything else.
fn revoke_public_execution(
    base: &Schema,
    dialect: &(impl Dialect + ?Sized),
    hints: &Hints,
    changes: &mut Vec<Change>,
) {
    if !dialect.creates_public_executable_routines() {
        return;
    }
    // `AlterModule` is only a create on an engine that rebuilds: where the
    // edit is a `CREATE OR ALTER`, the object keeps the ACL it had and a
    // revoke here would take away a state this plan was not asked to change.
    let rebuilds = dialect.rebuilds_modules();
    let mut decided = Vec::new();
    for change in changes.iter() {
        // Written as `if let` rather than a match with a wildcard: every
        // other kind of change ends with no `CREATE`, so there is nothing to
        // enumerate and a list of them would need maintaining for nothing.
        let id = if let Change::Module(ModuleChange::CreateModule { id, .. }) = change {
            id
        } else if let Change::Module(ModuleChange::AlterModule { id, .. }) = change
            && rebuilds
        {
            id
        } else {
            continue;
        };
        let ModuleId::Routine(routine) = id else {
            continue;
        };
        // The declaration's answer, written into the plan either way. Saying
        // nothing where it asks for the default back would leave the
        // connected rebuild guard unable to tell that plan from one with no
        // opinion, and it refuses the missing default it was asked to restore
        // (ADR-0009 §3).
        let access = if hints.public_execute.contains(id) {
            PublicAccess::Kept
        } else {
            PublicAccess::Revoked
        };
        // Whether the object was there before, not which variant said so: a
        // kind change arrives here as a `CreateModule` and is still a routine
        // somebody may have been executing a moment ago.
        let origin = if base.modules.contains_key(id) {
            RoutineOrigin::Rebuilt
        } else {
            RoutineOrigin::Created
        };
        decided.push(Change::Permission(GrantChange::PublicExecution {
            routine: routine.clone(),
            access,
            origin,
        }));
    }
    changes.extend(decided);
}

/// Modules are matched by **name**, never by uid: they carry no data, so they
/// carry no identity (ADR-0002).
///
/// Two of the three comparisons are ordinary. The third is the interesting one:
/// a module whose *kind* or trigger table changed is not an alteration at all —
/// `CREATE OR ALTER` cannot turn a view into a procedure, or move a trigger to
/// another table — so it is emitted as a drop followed by a create.
/// Table names held by one table in the baseline and another after this plan:
/// a table dropped, or renamed away, and another renamed or created into its
/// name, across skipped revisions (DEC-1118.1). Anything keyed by such a name
/// compares equal across the two tables while belonging to different ones.
fn names_changing_hands(base: Side<'_>, declared: Side<'_>) -> BTreeSet<TableName> {
    let before: BTreeMap<&TableName, &Uid> = base
        .ids
        .tables
        .iter()
        .map(|(uid, name)| (name, uid))
        .collect();
    declared
        .ids
        .tables
        .iter()
        .filter(|(uid, name)| before.get(name).is_some_and(|was| was != uid))
        .map(|(_, name)| name.clone())
        .collect()
}

// A trigger's table name can stay equal while its owner UID changes. Both
// the differ and restoration exemption must recognize that boundary (DEC-1118.1).
fn trigger_changes_hands(id: &ModuleId, changing_hands: &BTreeSet<TableName>) -> bool {
    matches!(id, ModuleId::Trigger { on, .. }
        if changing_hands.contains(&TableName::new(on.schema.clone(), on.name.clone())))
}

/// The logical module change, shared with plan-time consumers (SPEC §8.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleDelta {
    Unchanged,
    Alter,
    Rebuild,
}

/// Descriptions are declaration metadata. Kind changes cannot use ALTER;
/// options and dialect-normalized definitions determine whether to restate it.
pub fn module_delta(
    before: &Module,
    after: &Module,
    dialect: &(impl Dialect + ?Sized),
) -> ModuleDelta {
    // A trigger moved to another table needs no case of its own any
    // more: its table is part of its identity, so the move is a key
    // that is gone and a key that is new, and the two loops here
    // already spell that as a drop and a create (ADR-0009 §1). What is
    // left is the kind, which the key does not hold: `CREATE OR ALTER`
    // cannot turn a view into a procedure.
    if before.kind != after.kind {
        ModuleDelta::Rebuild
    } else if dialect.normalize_definition(&before.definition)
        // The definition is compared after the dialect's lightweight
        // normalization, never by understanding it (SPEC §8.2). What
        // survives that is re-stated in full, which is idempotent and
        // keeps the permissions granted on the object.
        != dialect.normalize_definition(&after.definition)
        // The option is part of what the module is: the engine
        // keeps it, refuses changes to what it reads while it
        // holds, and `CREATE OR ALTER` sets it either way.
        || before.options != after.options
    {
        ModuleDelta::Alter
    } else {
        ModuleDelta::Unchanged
    }
}

// Physical teardown for a conversion or a dependency is separate from whether
// the declaration changes its default. SQL expressions stay opaque (SPEC §8.2).
fn defaults_differ(before: Option<&String>, after: Option<&String>) -> bool {
    before != after
}

fn table_by_uid<'a>(side: Side<'a>, uid: &Uid) -> Option<&'a Table> {
    side.schema.tables.get(side.ids.tables.get(uid)?)
}

fn ordinary_default<'a>(side: Side<'a>, uid: &Uid) -> Option<Option<&'a String>> {
    let at = side.ids.columns.get(uid)?;
    let column = side.schema.tables.get(&at.table)?.columns.get(&at.name)?;
    Some(match &column.value_source {
        ValueSource::Default(text) => Some(text),
        ValueSource::None
        | ValueSource::Identity(_)
        | ValueSource::Sequence(_)
        | ValueSource::Generated(_) => None,
    })
}

/// Compare logical restorations through the differ's own decisions (SPEC §8.2).
/// Parameter-only index edits do not replace a part; physical dependent rebuilds
/// must not turn them into new interface restrictions. Prepare once per plan.
pub fn restoration_comparison<'a>(
    base: Side<'a>,
    declared: Side<'a>,
    dialect: &'a (impl Dialect + ?Sized),
) -> impl Fn(&Change, Option<&Uid>) -> bool + 'a {
    let renames = renames_of(base, declared);
    let changing_hands = names_changing_hands(base, declared);
    let index = Indexes {
        base: base.ids.index(),
        declared: declared.ids.index(),
    };
    let mut changed = BTreeSet::new();
    let mut partition_defaults = BTreeSet::new();
    for (uid, old) in &base.ids.tables {
        let Some(new) = declared.ids.tables.get(uid) else {
            continue;
        };
        let (Some(before), Some(after)) = (table_by_uid(base, uid), table_by_uid(declared, uid))
        else {
            continue;
        };
        let mut edits = Vec::new();
        let renamed = renames.apply(before, old);
        // A refusal is the plan's to report; this comparison only asks which
        // parts a plan would edit, so it has nowhere to put one.
        diff_constraints(new, &renamed, after, &mut edits, &mut Vec::new());
        if let (Some(was), Some(now)) = (&before.partition_of, &after.partition_of) {
            let was_columns =
                partition_columns_by_uid(&was.columns, &was.parent, &now.parent, &index);
            for column in declared
                .schema
                .tables
                .get(&now.parent)
                .into_iter()
                .flat_map(|t| t.columns.keys())
            {
                let existed = declared
                    .ids
                    .column_uid(&now.parent.column(column.clone()))
                    .is_some_and(|id| base.ids.columns.contains_key(id));
                if existed
                    && !defaults_differ(
                        was_columns.get(column).and_then(|c| c.default.as_ref()),
                        now.columns.get(column).and_then(|c| c.default.as_ref()),
                    )
                {
                    partition_defaults.insert((uid.clone(), column.clone()));
                }
            }
        }
        diff_computed(new, before, after, &mut edits);
        for edit in edits {
            if let Some(part) = edit.constraints() {
                changed.insert((uid.clone(), part.part(), part.name.map(str::to_owned)));
            }
        }
    }
    move |change: &Change, uid: Option<&Uid>| {
        if let Change::Column(ColumnChange::AlterColumnDefault { uid, .. }) = change {
            return ordinary_default(base, uid)
                .zip(ordinary_default(declared, uid))
                .is_some_and(|(b, d)| !defaults_differ(b, d));
        }
        if let Change::Partition(PartitionChange::SetPartitionDefault { column, .. }) = change {
            return uid.is_some_and(|u| partition_defaults.contains(&(u.clone(), column.clone())));
        }
        if let Some(id) = change.module_id() {
            return !trigger_changes_hands(id, &changing_hands)
                && base
                    .schema
                    .modules
                    .get(id)
                    .zip(declared.schema.modules.get(id))
                    .is_some_and(|(b, d)| module_delta(b, d, dialect) == ModuleDelta::Unchanged);
        }
        let Some(part) = change.constraints() else {
            return false;
        };
        let Some(uid) = uid else { return false };
        let (Some(before), Some(after)) = (table_by_uid(base, uid), table_by_uid(declared, uid))
        else {
            return false;
        };
        let present = |t: &Table| match part.part() {
            pbps_model::Part::PrimaryKey => t.primary_key.is_some(),
            pbps_model::Part::Computed => part.name.is_some_and(|n| t.computed.contains_key(n)),
            pbps_model::Part::Unique => part.name.is_some_and(|n| t.unique.contains_key(n)),
            pbps_model::Part::Check => part.name.is_some_and(|n| t.checks.contains_key(n)),
            pbps_model::Part::ForeignKey => {
                part.name.is_some_and(|n| t.foreign_keys.contains_key(n))
            }
            pbps_model::Part::Index => part.name.is_some_and(|n| t.indexes.contains_key(n)),
            pbps_model::Part::Exclusion => part.name.is_some_and(|n| t.exclude.contains_key(n)),
            // RLS policy changes have no consumer restoration exemption.
            pbps_model::Part::Policy => false,
        };
        present(before)
            && present(after)
            && !changed.contains(&(uid.clone(), part.part(), part.name.map(str::to_owned)))
    }
}

fn diff_modules(
    base: &Schema,
    declared: &Schema,
    changing_hands: &BTreeSet<TableName>,
    dialect: &(impl Dialect + ?Sized),
    changes: &mut Vec<Change>,
) {
    for (id, module) in &declared.modules {
        match base.modules.get(id) {
            None => changes.push(Change::Module(ModuleChange::CreateModule {
                id: id.clone(),
                module: Box::new(module.clone()),
            })),
            // A trigger on a name that passes to another table: one id for
            // two triggers, the doomed or departed table's and the one this
            // plan declares on the new occupant. The first goes before the
            // table changes and the second is created after them, exactly as
            // for a trigger that changes kind (DEC-1118.1).
            Some(before) if trigger_changes_hands(id, changing_hands) => {
                changes.push(Change::Module(ModuleChange::DropModule {
                    id: id.clone(),
                    kind: before.kind,
                }));
                changes.push(Change::Module(ModuleChange::CreateModule {
                    id: id.clone(),
                    module: Box::new(module.clone()),
                }));
            }
            Some(before) => match module_delta(before, module, dialect) {
                ModuleDelta::Unchanged => {}
                ModuleDelta::Alter => changes.push(Change::Module(ModuleChange::AlterModule {
                    id: id.clone(),
                    module: Box::new(module.clone()),
                })),
                ModuleDelta::Rebuild => {
                    changes.push(Change::Module(ModuleChange::DropModule {
                        id: id.clone(),
                        kind: before.kind,
                    }));
                    changes.push(Change::Module(ModuleChange::CreateModule {
                        id: id.clone(),
                        module: Box::new(module.clone()),
                    }));
                }
            },
        }
    }

    for (id, module) in &base.modules {
        if !declared.modules.contains_key(id) {
            changes.push(Change::Module(ModuleChange::DropModule {
                id: id.clone(),
                kind: module.kind,
            }));
        }
    }
}

/// The order of application.
///
/// Ordinarily the table rename comes first, so later steps use current names.
/// The specific freeing-drop graph may order some drops around it and supplies
/// their execution addresses (DECISIONS 496). Remaining constraint and index drops come next: they must precede both the
/// column renames they would otherwise block and the column drops they
/// reference. Adding them back must follow adding columns.
/// Inserting a class shifts every class below it, and these ordinals are
/// quoted in prose that uses them to justify behaviour: DECISIONS 140, 146,
/// 151, 174 and 237, `docs/PITFALLS.md`, `preflight.rs` and `deploy.rs`. A new
/// class means renumbering those in the same commit — a stale ordinal there
/// reads as a statement about the code and is not checked against it.
/// The class of the changes that alter an existing column in place: its
/// type, nullability, default or generation expression. Named because a
/// generated column's addition is placed after it (DEC-1168.1).
const COLUMN_ALTERATIONS: u8 = 9;

/// The class of row deletions, the last row changes. Named because a
/// tightening of a table with row changes is placed at its end (#1367).
const ROW_DELETIONS: u8 = 12;

/// Whether going from `from` to `to` sets a domain's default, as against
/// dropping or keeping it.
fn domain_sets_default(from: &pbps_model::ManagedType, to: &pbps_model::ManagedType) -> bool {
    pbps_model::DomainDelta::of(from, to).is_some_and(|d| matches!(d.default, Some(Some(_))))
}

/// The subject a change is sorted by within its class and rank. A rebuilt
/// type's steps lead with their phase, so that every retype of every rebuilt
/// type comes before the first swap, and a swap before the swap of the type
/// it is built on: the old base cannot be dropped while a domain over it
/// stands (DEC-1635.9). Every other change sorts by its own subject.
fn rebuild_subject(change: &Change) -> String {
    use pbps_model::EnumRebuildStep as Step;
    let Change::TypeValue(TypeValueChange::EnumRebuild { step, .. }) = change else {
        return change.subject();
    };
    match step {
        Step::Retype { .. } => format!("0 {}", change.subject()),
        // Fixed width, reversed: a deeper swap sorts first at any depth.
        Step::Swap { depth, .. } => format!("1 {:020} {}", usize::MAX - *depth, change.subject()),
        Step::Create { .. } => change.subject(),
    }
}

fn order_key(c: &Change) -> u8 {
    match c {
        Change::Partition(PartitionChange::RebuildPartitionedTable(_))
        | Change::Index(IndexChange::BuildPartitionedIndex(_))
        | Change::Partition(PartitionChange::CopyTableRows(_))
        | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => {
            unreachable!("rebuild operations are expanded by order, never ranked by the differ")
        }
        Change::TypeValue(TypeValueChange::AddEnumValue { .. })
        | Change::TypeValue(TypeValueChange::RenameEnumValue { .. })
        | Change::TypeValue(TypeValueChange::EnumRebuild {
            step: pbps_model::EnumRebuildStep::Create { .. },
            ..
        })
        // A package's own objects are what a later type, module or table of
        // the plan may use, so it goes with the type creates; the order among
        // packages is the `requires` edge, not this number (DEC-2447.6).
        | Change::Type(TypeChange::CreateType { .. })
        // Created before the tables, defaults, indexes and checks that name them by a
        // string literal, and a dictionary before the configuration that maps it
        // (DEC-2728.2). One altered in place goes first of its dependents too (DEC-2728.3).
        | Change::Search(SearchChange::CreateSearch { .. })
        | Change::Search(SearchChange::AlterSearchMapping { .. })
        | Change::Search(SearchChange::AlterSearchDictionary { .. })
        | Change::Extension(ExtensionChange::CreateExtension { .. })
        | Change::Extension(ExtensionChange::UpdateExtension { .. })
        | Change::Extension(ExtensionChange::SetExtensionSchema { .. }) => 7,
        // A domain's checks and NOT NULL are dropped with a table's, before
        // the plan's rows, and added with them after the rows (DEC-1635.8).
        // A default the half sets can name a label the plan renames or adds,
        // so that half waits for the type changes (class 7); it still
        // precedes every column and row.
        Change::TypeValue(TypeValueChange::AlterDomain {
            half: pbps_model::DomainHalf::Loosen,
            from,
            to,
            ..
        }) => {
            if domain_sets_default(from, to) {
                7
            } else {
                2
            }
        }
        Change::TypeValue(TypeValueChange::AlterDomain {
            half: pbps_model::DomainHalf::Tighten,
            ..
        }) => 13,
        Change::TypeValue(TypeValueChange::EnumRebuild { .. }) => COLUMN_ALTERATIONS,
        // After every module this plan drops and the packages it requires
        // are gone: late, like the type drops (DEC-2447.8).
        Change::Type(TypeChange::DropType { .. }) | Change::Search(SearchChange::DropSearch { .. }) | Change::Extension(ExtensionChange::DropExtension { .. }) => 18,
        Change::Type(TypeChange::RenameType { .. }) => 1,
        // A policy releases its bindings before module and column teardown.
        Change::Policy(PolicyChange::DropPolicy { .. }) => 0,
        // Modules go first and last, and both ends are load-bearing. A
        // SCHEMABINDING view blocks a rename of the column it binds, so every
        // module that is going has to go before the table changes; and a view
        // can only be created once the columns it selects exist.
        Change::Module(ModuleChange::DropModule { .. }) => 0,
        // A role drop needs nothing else gone first, and a plan that also
        // recreates the name wants the old one out of the way early.
        Change::Role(RoleChange::DropRole { .. }) => 0,
        // A table rename, and a role rename that depends on nothing here.
        Change::Table(TableChange::RenameTable { .. }) | Change::Role(RoleChange::RenameRole { .. }) => 1,
        // Before the column renames, and after the table rename that gives
        // these drops the name they use.
        //
        // The engine refuses `sp_rename` on a column a check constraint or a
        // filtered index's predicate names — 15336 for the check, 5074 with
        // 4922 behind it for the index, measured — so for those two the drop
        // is what makes the rename possible, and running it after was a valid,
        // reviewed plan the engine would not perform. The re-add stays in the
        // constraint class far below, where the new spelling can be written.
        //
        // The whole group moves, not the two kinds that need it. Measured, a
        // rename is never blocked by a constraint that does not name the
        // column, so moving the rest costs nothing — and the alternative is
        // asking which columns a check's expression names, which this tool
        // deliberately never parses (DECISIONS 174). `DropForeignKey`'s rank
        // travels with them and still puts it ahead of the key it references.
        //
        // A primary key that is *only* dropped is one of these drops and
        // travels with them. It was left in the addition class below because
        // one variant carries both directions, and a key that is still there
        // blocks the column changes every class from here down can make:
        // measured, `DROP NOT NULL` on a key column is `42P16` on PostgreSQL
        // and 5074 with 4922 behind it on SQL Server, and dropping the column
        // outright is the same 5074. So a declaration that gives up a key and
        // relaxes its column produced a plan neither engine would perform.
        //
        // Only `to: None`, and that is not a partial answer: a key being
        // *replaced* is emitted as two changes, its drop and its add (see
        // `diff_constraints`), so the drop arrives here and the add stays
        // below, where the columns its new shape may name have been added.
        //
        // `dependency_rank` already keeps a foreign-key drop ahead of it
        // inside this class, which is the order the engine requires and the
        // reason that rank exists.
        Change::Index(IndexChange::DropIndex { .. })
        | Change::Constraint(ConstraintChange::DropUnique { .. })
        | Change::Constraint(ConstraintChange::DropForeignKey { .. })
        | Change::Constraint(ConstraintChange::DropCheck { .. })
        | Change::Constraint(ConstraintChange::SetExclusion { to: None, .. })
        | Change::Constraint(ConstraintChange::SetPrimaryKey { to: None, .. }) => 2,
        // After the drops above (`sort_class` places it at the class's end),
        // since an index or check over it blocks it (4922), and before every
        // rename, drop or retype of a column its expression reads, which it
        // blocks in turn (15336, 4922; measured on 17.0, #1174).
        Change::Column(ColumnChange::DropComputedColumn { .. }) => 2,
        // A class of its own, after the table renames: `sp_rename` on a column
        // names the table, and `resolve_columns` iterates the *declared*
        // schema, so a `RenameColumn` always carries the post-rename table.
        //
        // Not sharing class 1 with `RenameTable`, which is where it was. The
        // two are not peers — one depends on the other — and sharing a class
        // left `subject()` to separate them. `RenameTable` answers with its
        // `from`, the old name, while the column rename carries the new one,
        // so the two were sorted against two different names for one table and
        // the alphabet decided: `dbo.customers` -> `dbo.clients` put the
        // column rename first, against a table that did not exist yet.
        Change::Column(ColumnChange::RenameColumn { .. }) => 3,
        // After the renames, so a revoke names the role and the object as
        // they now are. It shared a class with the drops above until they
        // moved ahead of the renames; it has the opposite need, so it stayed
        // and took a class of its own. A revoke on an object this plan drops
        // is never emitted (see `diff_roles`).
        Change::Permission(GrantChange::Revoke { .. }) => 4,
        Change::Column(ColumnChange::DropColumn { .. }) => 5,
        // A detach frees its range, as a drop does, before a partition is
        // created over it in class 7 (#1544).
        Change::Table(TableChange::DropTable { .. }) | Change::Partition(PartitionChange::DetachPartition { .. }) => 6,
        // An attach takes its range as a created partition does, and once
        // a detach or a drop has freed it (#1545).
        Change::Table(TableChange::CreateTable { .. }) | Change::Partition(PartitionChange::AttachPartition { .. }) => 7,
        Change::Column(ColumnChange::AddColumn { .. }) => 8,
        Change::Column(ColumnChange::AlterColumnType { .. })
        | Change::Column(ColumnChange::AlterColumnNullability { .. })
        | Change::Column(ColumnChange::AlterColumnDefault { .. })
        | Change::Column(ColumnChange::AlterColumnNumbering { .. })
        | Change::Column(ColumnChange::AlterColumnExpression { .. })
        | Change::Partition(PartitionChange::SetPartitionDefault { .. })
        | Change::Partition(PartitionChange::SetPartitionNotNull { .. }) => COLUMN_ALTERATIONS,
        // Once every column its expression reads is there in its final type
        // (`sort_class` places it after the class's alterations), and before
        // the indexes and checks of class 13 that may be over it (#1174).
        Change::Column(ColumnChange::AddComputedColumn { .. }) => COLUMN_ALTERATIONS,
        Change::Column(ColumnChange::SetColumnDeprecated { .. }) => 10,
        // A comment is metadata too, on the object as the plan leaves it: after its
        // create (7, 8), alterations (9) and every rename (1), under its final name
        // (DEC-2729.1, measured: a drop and a create lose a comment, a rename keeps it).
        Change::TableProperty(TablePropertyChange::SetComment { .. }) => 10,
        // Metadata too: no statement reads a storage parameter, and it runs
        // under the table's final name (DEC-1441.1).
        Change::TableProperty(TablePropertyChange::SetStorageParameters { .. }) => 10,
        // After the foreign-key drops of class 2 and before the adds of 13:
        // the engine checks the keys that stand when it switches (#1443).
        Change::TableProperty(TablePropertyChange::SetTablePersistence { .. }) => COLUMN_ALTERATIONS,
        // In place too, and nothing reads one (DEC-1442.1).
        Change::Index(IndexChange::SetIndexStorageParameters { .. }) => 10,
        // Rows arrive once every column they name exists and has its final
        // type, and before the constraints below: ADR-0004's "create table ->
        // insert rows -> add the foreign key that references them".
        Change::Row(RowChange::InsertRow { .. }) | Change::Row(RowChange::UpdateRow { .. }) => 11,
        // Rows leave after every insert and update, and after the foreign keys
        // that could block them are gone. No single order satisfies every
        // shape — a delete-then-insert on a table with a UNIQUE elsewhere
        // wants the delete first — but this is the order whose failure is
        // *loud*: the engine refuses inside the transaction and the plan rolls
        // back. Deleting first fails silently: a child row that moves its
        // foreign key to another parent in this same plan is still pointing
        // at the old one when the old one goes, and `ON DELETE CASCADE` takes
        // the child with it, after which the update touches zero rows and
        // nothing says so.
        Change::Row(RowChange::DeleteRow { .. }) => ROW_DELETIONS,
        Change::Constraint(ConstraintChange::SetPrimaryKey { .. })
        | Change::Constraint(ConstraintChange::AddUnique { .. })
        | Change::Constraint(ConstraintChange::AddForeignKey { .. })
        | Change::Constraint(ConstraintChange::AddCheck { .. })
        | Change::Constraint(ConstraintChange::SetExclusion { to: Some(_), .. })
        | Change::Index(IndexChange::AddIndex { .. }) => 13,
        // Late, it follows the index it names (DEC-1444.1); `sort_class`
        // moves the rest to the front.
        Change::TableProperty(TablePropertyChange::SetReplicaIdentity { .. }) => 13,
        Change::Policy(PolicyChange::CreatePolicy { .. })
        | Change::Policy(PolicyChange::SetRowSecurity { .. })
        | Change::Module(ModuleChange::CreateModule { .. })
        | Change::Module(ModuleChange::AlterModule { .. }) => 14,
        // The `PUBLIC` decision for a routine travels with the routine: class
        // 14 at that routine's own create rank (`dependency_rank`), and its
        // subject is the routine's, so the sort puts it immediately after its
        // `CREATE` or `ALTER` with nothing between them. In a transactional
        // apply nothing outside sees the gap either way; in the autocommit
        // script `--sql` renders (DECISIONS 259) a routine created at 14 and
        // revoked at 16 held the engine's default `EXECUTE` for `PUBLIC` for
        // the rest of the modules and every role create (#687). A class of its
        // own after the modules would still leave a later routine's `CREATE`
        // in between, and would renumber every ordinal below (DEC-687.1).
        Change::Permission(GrantChange::PublicExecution { .. }) => 14,
        // A grant names an object, so it comes after every object exists —
        // and after the role does.
        Change::Role(RoleChange::CreateRole { .. }) => 15,
        Change::Permission(GrantChange::Grant { .. }) => 16,
        // Emits nothing; it exists so the recorded state matches the file. Last
        // keeps it out of the way of everything that does emit.
        Change::Row(RowChange::SetDataMode { .. }) | Change::Row(RowChange::SetEnsureKeys { .. }) => 17,
    }
}
/// The defaults an inserted row is left to: every column the row omits, the
/// key aside, that the table gives a default. An `IDENTITY` column is the
/// engine's own and never one of these (DECISIONS 94, 117).
///
/// The types cover every non-key column, spelled or omitted: a spelled cell
/// is held by the rendering that reads it back (DECISIONS 137), a defaulted
/// one to its default (133), and a column the table gives no default is
/// left at NULL and held to that (136). An `IDENTITY` column is none of
/// these.
fn omitted_defaults(
    table: &Table,
    key_column: &str,
    row: &pbps_model::Row,
) -> (
    BTreeMap<ColumnName, String>,
    BTreeMap<ColumnName, ColumnType>,
) {
    let types = table
        .row_columns(key_column)
        .map(|(c, spec)| (c.clone(), spec.ty.clone()))
        .collect();
    let defaults = omitted_columns(table, key_column, row)
        .filter_map(|(c, spec)| match &spec.value_source {
            ValueSource::Default(value) => Some((c.clone(), value.clone())),
            ValueSource::None
            | ValueSource::Identity(_)
            | ValueSource::Sequence(_)
            | ValueSource::Generated(_) => None,
        })
        .collect();
    (defaults, types)
}

/// The columns an insert leaves to the table: not the key, not spelled by
/// the row, and not an `IDENTITY` column, which is the engine's own.
fn omitted_columns<'a>(
    table: &'a Table,
    key_column: &'a str,
    row: &'a pbps_model::Row,
) -> impl Iterator<Item = (&'a ColumnName, &'a pbps_model::Column)> {
    table
        .row_columns(key_column)
        .filter(move |(c, _)| !row.0.contains_key(*c))
}

#[cfg(test)]
#[allow(clippy::wildcard_enum_match_arm)]
mod tests;

#[cfg(test)]
mod managed_type_tests;

#[cfg(test)]
mod type_replacement_tests;

#[cfg(test)]
#[allow(clippy::wildcard_enum_match_arm)]
mod meeting_shape_tests;

/// Text search objects through the whole differ: creation and drop order by depth, the
/// recorded reason a drop needs, and the dictionary a mapping must be able to name
/// (DEC-2728.2).
#[cfg(test)]
#[allow(clippy::wildcard_enum_match_arm)]
mod search_tests;

#[cfg(test)]
mod split_pin_tests;
