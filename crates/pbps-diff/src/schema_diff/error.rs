//! The errors the differ reports.

use super::*;

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
