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

use std::collections::{BTreeMap, BTreeSet};

use pbps_dialect::Dialect;
use pbps_model::change::DeleteCause;
use pbps_model::data::cell;
use pbps_model::{
    Cell, Change, ChangeSet, ColumnRef, ColumnType, DataMode, DetachedKind, DetachedName,
    GrantTarget, Hints, IdsFile, Index, IndexMethod, IndexPart, ModuleId, Permission,
    PlannedChange, PrimaryKey, PublicAccess, Renames, ReplicaIdentity, RoutineOrigin, Schema,
    Table, TableName, Uid, UniqueConstraint, Value,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiffError {
    /// In most databases IDENTITY cannot be changed with ALTER; the whole table
    /// has to be rebuilt. That is beyond what declarative automation should do on
    /// its own — a human has to choose the migration strategy.
    #[error(
        "the IDENTITY property of {column} changed, but IDENTITY cannot be modified with ALTER"
    )]
    IdentityChangeUnsupported { column: ColumnRef },

    /// A column that becomes generated, stops being generated, or changes
    /// its generation kind (DEC-1168.1). The engine has no in-place form for
    /// any of those: an ordinary column cannot be given an expression, and
    /// dropping one would keep today's computed values as ordinary data. Only
    /// the expression of a column that stays generated is changed in place.
    #[error(
        "column {column}: a change between generated and not generated, or between stored and \
         virtual, has no in-place form. To change it, add a new column and drop this one."
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
         change its expression, in a plan of its own first, then this one."
    )]
    ComputedInputChanged {
        computed: ColumnRef,
        column: String,
        change: &'static str,
    },

    /// A plan that alters or drops a function a SQL Server computed column
    /// may call while the column stands (3729, measured on 17.0), or adds a
    /// computed column calling one the plan creates, which comes after it
    /// (#1174).
    #[error(
        "computed column {computed} may call `{function}`, which this plan {change}. Apply \
         the function change and the computed column change in separate plans."
    )]
    ComputedFunctionChanged {
        computed: ColumnRef,
        function: String,
        change: &'static str,
    },

    /// A `data:` block on a table whose primary key cannot key its rows
    /// (ADR-0004). `validate` says the same thing against the file and the
    /// line; this is here so that a differ reached another way never quietly
    /// produces a plan with the rows left out of it.
    #[error(
        "{table} declares `data:` but has no single-column primary key, so its rows have no identity"
    )]
    DataWithoutKey { table: TableName },

    /// A permanent table whose foreign key references an unlogged one, which
    /// PostgreSQL refuses (`constraints on permanent tables may reference only
    /// permanent tables`, measured on 16 and 18, #1443).
    #[error(
        "{table}: its foreign key `{key}` references {target}, which is unlogged, and \
         PostgreSQL lets only an unlogged table reference an unlogged one. Make {table} \
         unlogged too, or {target} permanent."
    )]
    PermanentReferencesUnlogged {
        table: TableName,
        key: String,
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
         {partition} permanent, or {table} unlogged."
    )]
    PermanentReferencesUnloggedPartition {
        table: TableName,
        key: String,
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
        what.join(", ")
    )]
    TemporalTableChange { table: TableName, what: Vec<String> },

    /// A change to a partitioned table or a partition, on either side, other
    /// than creating it (#1170, DEC-1170.1): the changes a hierarchy takes
    /// are #1171's to qualify, and some reach every partition under a lock.
    #[error(
        "{table} is a partitioned table or a partition, and pbps can create one but not change \
         it yet; this plan would {}. Declare {table} as it is recorded, and make the change by \
         hand.",
        what.join(", ")
    )]
    PartitionedTableChange { table: TableName, what: Vec<String> },

    /// A partition declared as an ordinary table whose shape is not its
    /// parent's, names aside (#1544): the detach gives it the parent's shape,
    /// and anything more is a second change to make in a later revision.
    #[error(
        "{table} is detached from {parent}, and declared as other than its parent's shape: {}. \
         Declare it with its parent's columns, keys, constraints and indexes (under any names), \
         and change it in a later revision.",
        what.join("; ")
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
        what.join("; ")
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
         then declare the rows again — or rename the column instead of replacing it"
    )]
    DataKeyColumnChanged { table: TableName },

    #[error(
        "{table} has no baseline primary key and this plan does not restore the declared key, so its data rows cannot be matched"
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
        columns.iter().map(|c| format!("`{c}`")).collect::<Vec<_>>().join(", ")
    )]
    ColumnRenameCycle {
        table: TableName,
        columns: BTreeSet<String>,
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
         the key change, then declare the rows again"
    )]
    DataBaselineKeyOnNewColumn { table: TableName, column: String },

    #[error("{table} has a baseline primary key with {} columns ({columns:?}); pbps matches data rows only on a single-column key", columns.len())]
    DataBaselineKeyNotSingle {
        table: TableName,
        columns: Vec<String>,
    },
}

enum BaselineDataKey<'a> {
    Absent,
    Single(&'a str),
    Multiple(&'a [String]),
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
    dialect: &dyn Dialect,
    hints: &Hints,
) -> Result<ChangeSet, Vec<DiffError>> {
    diff_rebuilding(base, declared, dialect, hints, &BTreeSet::new())
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
    dialect: &dyn Dialect,
    hints: &Hints,
    also: &BTreeSet<ModuleId>,
) -> Result<ChangeSet, Vec<DiffError>> {
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
    dialect: &dyn Dialect,
    hints: &Hints,
    also: &BTreeSet<ModuleId>,
    screen: Screen,
    read_back: &Schema,
) -> Result<ChangeSet, Vec<DiffError>> {
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
    dialect: &dyn Dialect,
    hints: &Hints,
    also: &BTreeSet<ModuleId>,
) -> Result<ChangeSet, Vec<DiffError>> {
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
    dialect: &dyn Dialect,
    hints: &Hints,
    also: &BTreeSet<ModuleId>,
    rebinding: Rebinding,
    screen: Screen,
    read_back: Option<&Schema>,
) -> Result<ChangeSet, Vec<DiffError>> {
    let d = diff_partial_rebuilding(
        base, declared, dialect, hints, also, rebinding, screen, read_back,
    );
    if d.errors.is_empty() {
        Ok(d.changes)
    } else {
        Err(d.errors)
    }
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
    dialect: &dyn Dialect,
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

#[allow(clippy::too_many_arguments)]
fn diff_partial_rebuilding(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &dyn Dialect,
    hints: &Hints,
    also: &BTreeSet<ModuleId>,
    rebinding: Rebinding,
    screen: Screen,
    read_back: Option<&Schema>,
) -> Diffed {
    let mut changes = Vec::new();
    let mut errs = Vec::new();

    let base_tables = &base.ids.tables;
    let declared_tables = &declared.ids.tables;

    // Table present only in the base: dropped. Its foreign keys are dropped
    // first as separate changes — two dropped tables that reference each other
    // would otherwise fail or succeed depending on which DROP TABLE runs first.
    for (uid, name) in base_tables {
        if !declared_tables.contains_key(uid) {
            if let Some(t) = base.schema.tables.get(name) {
                for fk_name in t.foreign_keys.keys() {
                    changes.push(Change::DropForeignKey {
                        table: name.clone(),
                        name: fk_name.clone(),
                    });
                }
            }
            changes.push(Change::DropTable {
                uid: uid.clone(),
                name: name.clone(),
                // A partition is detached from its parent before it goes
                // (#1171, DEC-1171.1).
                detach_from: base
                    .schema
                    .tables
                    .get(name)
                    .and_then(|t| t.partition_of.as_ref())
                    .map(|of| of.parent.clone()),
            });
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
                changes.push(Change::AddForeignKey {
                    table: name.clone(),
                    name: fk_name,
                    constraint: Box::new(fk),
                });
            }
            // Rows are split out of the CREATE for the same reason the
            // foreign keys above are: they are separate statements in a
            // separate ordering class, and a `CreateTable` carrying rows would
            // give the emitter a second place to produce DML.
            match (&table.data, table.primary_key.as_ref()) {
                (Some(data), Some(pk)) if pk.columns.len() == 1 => {
                    let identity_key = table
                        .columns
                        .get(&pk.columns[0])
                        .is_some_and(|c| c.identity.is_some());
                    for (key, row) in &data.rows {
                        let (defaults, types) = omitted_defaults(&table, &pk.columns[0], row);
                        changes.push(Change::InsertRow {
                            table: name.clone(),
                            key_column: pk.columns[0].clone(),
                            identity_key,
                            key: key.clone(),
                            row: row.clone(),
                            defaults,
                            types,
                        });
                    }
                }
                (Some(_), _) => errs.push(DiffError::DataWithoutKey {
                    table: name.clone(),
                }),
                (None, _) => {}
            }
            changes.push(Change::CreateTable {
                uid: uid.clone(),
                name: name.clone(),
                table: Box::new(table),
            });
        }
    }

    // Table present on both sides: possibly renamed, and its contents must be
    // compared.
    let renames = renames_of(base, declared);
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
                        .filter(|(_, c)| c.default.is_some())
                        .map(|(name, _)| name.clone())
                        .collect()
                })
                .unwrap_or_default();
            changes.push(Change::RenameTable {
                uid: uid.clone(),
                from: base_name.clone(),
                to: declared_name.clone(),
                defaults,
            });
        }
        let (Some(base_table), Some(declared_table)) = (
            base.schema.tables.get(base_name),
            declared.schema.tables.get(declared_name),
        ) else {
            continue;
        };
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
                .ok_or_else(|| vec![format!("its parent {} is not in the base", of.parent)])
                // The parent as this plan leaves it: a foreign key to a
                // table renamed in the same plan is declared under the new
                // name, as `diff_constraints` reads it below.
                .and_then(|parent| {
                    detached_names(
                        &renames.apply(parent, &of.parent),
                        base_table,
                        declared_table,
                        dialect,
                    )
                }) {
                Ok(names) => changes.push(Change::DetachPartition {
                    uid: uid.clone(),
                    table: declared_name.clone(),
                    parent: of.parent.clone(),
                    names,
                    shape: Box::new(declared_table.clone()),
                }),
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
                        changes.push(Change::DropIndex {
                            table: declared_name.clone(),
                            name,
                        });
                    }
                    changes.push(Change::AttachPartition {
                        uid: uid.clone(),
                        table: declared_name.clone(),
                        parent: of.parent.clone(),
                        bound: of.bound.clone(),
                        shape: Box::new(declared_table.clone()),
                    });
                    diff_constraints(declared_name, &table, declared_table, &mut changes);
                    diff_storage_parameters(
                        uid,
                        declared_name,
                        &table,
                        declared_table,
                        &mut changes,
                    );
                    if table.unlogged != declared_table.unlogged {
                        changes.push(Change::SetTablePersistence {
                            uid: uid.clone(),
                            table: declared_name.clone(),
                            unlogged: declared_table.unlogged,
                        });
                    }
                    diff_partition_columns(
                        uid,
                        declared_name,
                        &table,
                        declared_table,
                        base,
                        declared,
                        &mut changes,
                    );
                    // A column without a default where its parent's has one:
                    // the attach gives it none, which no declaration can say
                    // and the reader does not hold, so it takes its parent's
                    // unless it declares its own, which the comparison above
                    // sets (measured on 16 and 18).
                    let parent = declared.schema.tables.get(&of.parent);
                    for column in defaultless {
                        let own = of.columns.get(&column).and_then(|c| c.default.as_ref());
                        let fallback = parent
                            .and_then(|p| p.columns.get(&column))
                            .and_then(|c| c.default.clone());
                        if let (None, Some(fallback)) = (own, fallback) {
                            changes.push(Change::SetPartitionDefault {
                                uid: uid.clone(),
                                table: declared_name.clone(),
                                parent: of.parent.clone(),
                                column,
                                from: None,
                                to: None,
                                fallback: Some(fallback),
                            });
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
            base,
            declared,
            base_name,
            declared_name,
            base_table,
            declared_table,
            dialect,
            &mut changes,
            &mut errs,
        );
        diff_constraints(
            declared_name,
            &renames.apply(base_table, base_name),
            declared_table,
            &mut changes,
        );
        diff_computed(declared_name, base_table, declared_table, &mut changes);
        diff_storage_parameters(uid, declared_name, base_table, declared_table, &mut changes);
        if base_table.unlogged != declared_table.unlogged {
            changes.push(Change::SetTablePersistence {
                uid: uid.clone(),
                table: declared_name.clone(),
                unlogged: declared_table.unlogged,
            });
        }
        diff_partition_columns(
            uid,
            declared_name,
            base_table,
            declared_table,
            base,
            declared,
            &mut changes,
        );
        // Rows are compared by column *name* on each side, and a rename in
        // this same plan means the two sides know one column by two names.
        // The uid is what says they are the same column.
        let base_cols = columns_of(base.ids, base_name);
        let base_name_of: BTreeMap<String, String> = columns_of(declared.ids, declared_name)
            .iter()
            .filter_map(|(uid, d)| base_cols.get(uid).map(|b| (d.name.clone(), b.name.clone())))
            .collect();
        diff_data(
            declared_name,
            base_table,
            declared_table,
            &base_name_of,
            &mut changes,
            &mut errs,
        );
    }

    split_tightenings_from_retypes(&mut changes);
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
    refuse_partition_changes(base, declared, hints, &changes, &mut errs);
    // A module declaration can stay byte-for-byte identical while a new
    // overload or shadow changes what it should bind to. Ask the dialect
    // before sorting, rather than appending unreviewed SQL at apply time.
    let arriving: Vec<ModuleId> = changes
        .iter()
        .filter_map(|change| match change {
            Change::CreateModule { id, .. } => Some(id.clone()),
            Change::CreateTable { name, .. } => Some(ModuleId::Named(name.clone())),
            Change::RenameTable { to, .. } => Some(ModuleId::Named(to.clone())),
            Change::DropTable { .. }
            | Change::DetachPartition { .. }
            | Change::AttachPartition { .. }
            | Change::AddColumn { .. }
            | Change::DropColumn { .. }
            | Change::RenameColumn { .. }
            | Change::AlterColumnType { .. }
            | Change::AlterColumnNullability { .. }
            | Change::AlterColumnDefault { .. }
            | Change::AlterColumnExpression { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::SetPrimaryKey { .. }
            | Change::SetIndexStorageParameters { .. }
            | Change::SetTablePersistence { .. }
            | Change::SetPartitionDefault { .. }
            | Change::SetPartitionNotNull { .. }
            | Change::SetStorageParameters { .. }
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
            | Change::InsertRow { .. }
            | Change::UpdateRow { .. }
            | Change::DeleteRow { .. }
            | Change::SetDataMode { .. }
            | Change::AlterModule { .. }
            | Change::DropModule { .. }
            | Change::CreateRole { .. }
            | Change::DropRole { .. }
            | Change::RenameRole { .. }
            | Change::Grant { .. }
            | Change::Revoke { .. }
            | Change::PublicExecution { .. } => None,
        })
        .collect();
    let changed: BTreeSet<ModuleId> = changes
        .iter()
        .filter_map(|change| match change {
            Change::CreateModule { id, .. }
            | Change::AlterModule { id, .. }
            | Change::DropModule { id, .. } => Some(id.clone()),
            Change::CreateTable { .. }
            | Change::DropTable { .. }
            | Change::DetachPartition { .. }
            | Change::AttachPartition { .. }
            | Change::RenameTable { .. }
            | Change::AddColumn { .. }
            | Change::DropColumn { .. }
            | Change::RenameColumn { .. }
            | Change::AlterColumnType { .. }
            | Change::AlterColumnNullability { .. }
            | Change::AlterColumnDefault { .. }
            | Change::AlterColumnExpression { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::SetPrimaryKey { .. }
            | Change::SetIndexStorageParameters { .. }
            | Change::SetTablePersistence { .. }
            | Change::SetPartitionDefault { .. }
            | Change::SetPartitionNotNull { .. }
            | Change::SetStorageParameters { .. }
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
        .collect();
    // One set, so a module both the dialect and the caller ask for is rebuilt
    // once.
    let candidates = match rebinding {
        Rebinding::Candidates => dialect.rebound_modules(declared.schema, &arriving, &changed),
        Rebinding::Evidence => BTreeSet::new(),
    };
    let rebound: BTreeSet<ModuleId> = candidates.into_iter().chain(also.iter().cloned()).collect();
    for id in rebound {
        if !changed.contains(&id)
            && base.schema.modules.contains_key(&id)
            && let Some(module) = declared.schema.modules.get(&id)
        {
            changes.push(Change::AlterModule {
                id,
                module: Box::new(module.clone()),
            });
        }
    }
    diff_roles(base, declared, dialect, &mut changes);
    revoke_public_execution(base.schema, dialect, hints, &mut changes);

    // The ordering and risk pass below runs whether or not there are errors:
    // it is pure computation over the changes already built, and a caller that
    // is going to *report* the partial set needs it sorted and classified
    // exactly as a plan would be.
    // After every pass that rebuilds an index: its `CREATE` carries the
    // declared parameters (#1483 review).
    pbps_model::change::drop_parameter_changes_of_rebuilt_indexes(&mut changes, |c| c);
    let mut planned: Vec<PlannedChange> = changes.into_iter().map(PlannedChange::new).collect();
    for p in &mut planned {
        p.risks = dialect.change_risks(&p.change);
        // Attached here, not looked up at emit time: the plan file is the
        // artifact the deployment gate reviews, and a hint resolved later
        // against a YAML file the deployment host may not have is a hint
        // nobody read (ADR-0003).
        if let Some(strategy) = p.change.table().and_then(|t| hints.strategies.get(t)) {
            p.strategy = *strategy;
        }
    }

    // Modules are ordered among themselves by dependency: a view over a view
    // has to be created second, and dropped first (ADR-0002).
    // Lexed by the dialect: where a literal ends is the engine's rule, and an
    // edge read out of a literal the engine had not closed put a view before
    // the one it selects from (DECISIONS 315).
    let lex = |definition: &str| dialect.code_only(definition);
    let rank = |from: &str, to: &str| dialect.bare_name_rank(from, to);
    let lexis = pbps_model::module::Lexis {
        code_only: &lex,
        continues_ident: dialect.lexicon().identifier_continues,
        reserved: dialect.lexicon().reserved,
        bare_rank: &rank,
    };
    let create_rank = rank_of(&pbps_model::module::creation_order_with(
        &declared.schema.modules,
        &hints.module_deps,
        &lexis,
    ));
    let drop_rank = rank_of(&pbps_model::module::creation_order_with(
        &base.schema.modules,
        &hints.module_deps,
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
            if let Change::AttachPartition { table, parent, .. } = &p.change {
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
            Change::RenameColumn { table, .. } => Some(table.clone()),
            Change::CreateTable { .. }
            | Change::DropTable { .. }
            | Change::DetachPartition { .. }
            | Change::AttachPartition { .. }
            | Change::RenameTable { .. }
            | Change::AddColumn { .. }
            | Change::DropColumn { .. }
            | Change::AlterColumnType { .. }
            | Change::AlterColumnNullability { .. }
            | Change::AlterColumnDefault { .. }
            | Change::AlterColumnExpression { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::SetPrimaryKey { .. }
            | Change::SetIndexStorageParameters { .. }
            | Change::SetTablePersistence { .. }
            | Change::SetPartitionDefault { .. }
            | Change::SetPartitionNotNull { .. }
            | Change::SetStorageParameters { .. }
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
            | Change::PublicExecution { .. } => None,
        })
        .collect();
    // Whether this drop can be what frees a name a `RenameColumn` above is
    // waiting for. A column drop and nothing else: no other change gives up a
    // column name.
    let frees_a_renamed_column = |c: &Change| -> bool {
        match c {
            Change::DropColumn { column, .. } => {
                tables_claiming_a_column_name.contains(&column.table)
            }
            Change::CreateTable { .. }
            | Change::DropTable { .. }
            | Change::DetachPartition { .. }
            | Change::AttachPartition { .. }
            | Change::RenameTable { .. }
            | Change::AddColumn { .. }
            | Change::RenameColumn { .. }
            | Change::AlterColumnType { .. }
            | Change::AlterColumnNullability { .. }
            | Change::AlterColumnDefault { .. }
            | Change::AlterColumnExpression { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::SetPrimaryKey { .. }
            | Change::SetIndexStorageParameters { .. }
            | Change::SetTablePersistence { .. }
            | Change::SetPartitionDefault { .. }
            | Change::SetPartitionNotNull { .. }
            | Change::SetStorageParameters { .. }
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
            | Change::PublicExecution { .. } => false,
        }
    };
    // A column rename into a name another rename of the same table vacates
    // runs after it: skipped revisions that rename `b` to `c` and then `a` to
    // `b` leave a chain the uids would otherwise order, and `a -> b` while `b`
    // still stands is refused by both engines (DEC-541.1). Its depth is how many
    // links it waits on; the chain's far end, whose target is free, goes
    // first. A cycle, two columns trading names, has no order an engine takes
    // without a temporary name, so the plan says so instead of emitting one.
    let vacates: BTreeMap<(TableName, String), String> = planned
        .iter()
        .filter_map(|p| {
            if let Change::RenameColumn {
                table, from, to, ..
            } = &p.change
            {
                Some(((table.clone(), from.clone()), to.clone()))
            } else {
                None
            }
        })
        .collect();
    let mut cycles: BTreeSet<(TableName, BTreeSet<String>)> = BTreeSet::new();
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
    type Rename = (TableName, String);
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
        let folded = crate::rename_order::case_folded(to);
        for vacating in vacates.keys() {
            if vacating != rename
                && vacating.0 == rename.0
                && vacating.1 != *to
                && crate::rename_order::case_folded(&vacating.1) == folded
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
                .filter(|(_, c)| c.generated.is_some())
                .filter_map(move |(column, _)| base.ids.column_uid(&name.column(column)).cloned())
        })
        .collect();
    let row_tables: BTreeSet<TableName> = planned
        .iter()
        .filter_map(|p| row_table(&p.change).cloned())
        .collect();
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
        if let Change::SetPartitionDefault { table, .. }
        | Change::SetPartitionNotNull { table, .. }
        | Change::SetTablePersistence { table, .. }
        | Change::SetStorageParameters { table, .. }
        | Change::SetIndexStorageParameters { table, .. } = c
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
    let components = foreign_key_components(declared.schema);
    let depth_of = {
        fn depth(
            t: &TableName,
            schema: &Schema,
            components: &BTreeMap<TableName, TableName>,
            memo: &mut BTreeMap<TableName, usize>,
        ) -> usize {
            if let Some(d) = memo.get(t) {
                return *d;
            }
            let d = schema.tables.get(t).map_or(0, |table| {
                table
                    .foreign_keys
                    .values()
                    // Not within its own cycle, whose keys are dropped
                    // around the switches: what is left is acyclic.
                    .filter(|fk| {
                        fk.references_table != *t
                            && components.get(&fk.references_table) != components.get(t)
                    })
                    .map(|fk| 1 + depth(&fk.references_table, schema, components, memo))
                    .max()
                    .unwrap_or(0)
            });
            memo.insert(t.clone(), d);
            d
        }
        let mut memo = BTreeMap::new();
        let mut depths = BTreeMap::new();
        for t in declared.schema.tables.keys() {
            depths.insert(t.clone(), depth(t, declared.schema, &components, &mut memo));
        }
        depths
    };
    let deepest = depth_of.values().copied().max().unwrap_or(0);
    // A parent's column default or NULL-ness this plan changes, which the
    // engine recurses into every partition: a partition's own on that
    // column is set after it, or the parent's would overwrite it (#1687).
    let parent_moved: BTreeSet<(TableName, String)> = planned
        .iter()
        .filter_map(|p| {
            // A retype carries its NOT NULL change with it (#1692 review).
            if let Change::AlterColumnDefault { column, .. }
            | Change::AlterColumnNullability { column, .. } = &p.change
            {
                Some((column.table.clone(), column.name.clone()))
            } else if let Change::AlterColumnType {
                column,
                from_nullable,
                to_nullable,
                ..
            } = &p.change
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
    let follows_its_parent = |c: &Change| -> bool {
        let (Change::SetPartitionDefault { table, column, .. }
        | Change::SetPartitionNotNull { table, column, .. }) = c
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
    let sort_class = |c: &Change| -> (u8, usize) {
        if let Change::AttachPartition { table, .. } = c
            && late_attach.contains_key(table)
        {
            return (ROW_DELETIONS - 1, 1);
        }
        if follows_late_attach(c) {
            return (ROW_DELETIONS - 1, 2);
        }
        if follows_its_parent(c) {
            return (COLUMN_ALTERATIONS, 4);
        }
        if let Change::SetTablePersistence {
            table, unlogged, ..
        } = c
        {
            let depth = depth_of.get(table).copied().unwrap_or(0);
            return (
                COLUMN_ALTERATIONS,
                10 + if *unlogged { deepest - depth } else { depth },
            );
        }
        // A replica identity whose target stands before the plan is set
        // first, under the table's old name: before the drops of class 2 can
        // take its old index, which would leave the table identifying no row
        // (DEC-1444.1).
        if let Change::SetReplicaIdentity { uid, to, .. } = c
            && early_identity.contains(&(uid.clone(), to.clone()))
        {
            return (0, 1);
        }
        // A tightening of a table whose rows this plan writes or deletes runs
        // after them, at the end of the deletes' class: the rows may be what
        // fills or removes its NULLs, and no row change needs the column NOT
        // NULL first. Still before the additions of class 13, a primary key
        // among them, which SQL Server refuses over a nullable column (#1367).
        if let Change::AlterColumnNullability {
            column,
            to_nullable: false,
            ..
        } = c
            && row_tables.contains(&column.table)
        {
            return (ROW_DELETIONS, 2);
        }
        // A generated column is dropped before the ordinary columns of its
        // class, and added after every column change of the class beyond:
        // the additions, and the in-place alterations of class 9. Measured,
        // the engine refuses to drop a column a generated column reads,
        // refuses a generation expression over a column that is not there yet,
        // and refuses to retype one a generated column reads. A generated
        // column never reads another, and nothing in class 9 needs one that is
        // new, so one layer each way is the whole order (DEC-1168.1).
        let drops_generated =
            matches!(c, Change::DropColumn { uid, .. } if generated_in_base.contains(uid));
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
        if let Change::AddColumn { column, .. } = c
            && column.generated.is_some()
        {
            return (COLUMN_ALTERATIONS, 2);
        }
        // A computed column comes down after every index and constraint drop
        // of class 2, the ones that free a renamed column's name (2, 3)
        // included, and goes up after every alteration of class 9: it reads
        // the columns, in their final type, and nothing in either class reads
        // it (#1174).
        if matches!(c, Change::DropComputedColumn { .. }) {
            return (2, 4);
        }
        if matches!(c, Change::AddComputedColumn { .. }) {
            return (COLUMN_ALTERATIONS, 3);
        }
        // A detach comes after every table drop of its class: it claims the
        // declared names of what it keeps, and a dropped table's index or
        // key may be what holds one now. A drop claims no name, so nothing
        // waits the other way (#1544).
        if matches!(c, Change::DetachPartition { .. }) {
            return (order_key(c), 2);
        }
        // A partition is created after every parent, whose columns, keys and
        // indexes the engine gives it as it is made (#1170). Under a parent
        // whose columns this plan changes, after those changes too: its own
        // default may name a column the parent adds, and a parent's default
        // set or dropped afterwards would overwrite its own (#1692 review).
        if let Change::CreateTable { table, .. } = c
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
        if let Change::AddIndex { table, .. } = c
            && declared
                .schema
                .tables
                .get(table)
                .is_some_and(|t| t.partition_by.is_some())
        {
            return (order_key(c), 0);
        }
        if let Change::RenameColumn { table, from, .. } = c {
            let depth = chain_depth
                .get(&(table.clone(), from.clone()))
                .copied()
                .unwrap_or(0);
            (order_key(c), 1 + depth)
        } else {
            (order_key(c), 1)
        }
    };
    planned.sort_by_key(|p| {
        let (class, within) = sort_class(&p.change);
        (
            class,
            within,
            if let Change::AttachPartition { table, .. } = &p.change
                && let Some(rank) = late_attach.get(table)
            {
                *rank
            } else {
                dependency_rank(&p.change, &create_rank, &drop_rank, &data_rank, &role_rank)
            },
            p.change.subject(),
            format!("{:?}", p.change),
        )
    });
    crate::rename_order::order(&mut planned, base, &renames, dialect);
    Diffed {
        changes: ChangeSet { changes: planned },
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
    base: Side<'_>,
    declared: Side<'_>,
    base_table_name: &TableName,
    declared_table_name: &TableName,
    base_table: &Table,
    declared_table: &Table,
    dialect: &dyn Dialect,
    changes: &mut Vec<Change>,
    errs: &mut Vec<DiffError>,
) {
    let base_cols = columns_of(base.ids, base_table_name);
    let declared_cols = columns_of(declared.ids, declared_table_name);

    for (uid, declared_ref) in &declared_cols {
        let Some(base_ref) = base_cols.get(uid) else {
            // Present only on the declared side: a new column.
            if let Some(c) = declared_table.columns.get(&declared_ref.name) {
                changes.push(Change::AddColumn {
                    uid: uid.clone(),
                    table: declared_table_name.clone(),
                    name: declared_ref.name.clone(),
                    column: Box::new(c.clone()),
                });
            }
            continue;
        };

        if base_ref.name != declared_ref.name {
            changes.push(Change::RenameColumn {
                uid: uid.clone(),
                table: declared_table_name.clone(),
                from: base_ref.name.clone(),
                to: declared_ref.name.clone(),
                table_was: (base_table_name != declared_table_name)
                    .then(|| base_table_name.clone()),
            });
        }

        let (Some(base_col), Some(col)) = (
            base_table.columns.get(&base_ref.name),
            declared_table.columns.get(&declared_ref.name),
        ) else {
            continue;
        };

        if base_col.identity != col.identity {
            errs.push(DiffError::IdentityChangeUnsupported {
                column: declared_ref.clone(),
            });
        }
        let mut recomputed = false;
        match (&base_col.generated, &col.generated) {
            (None, None) => {}
            (Some(from), Some(to)) if from.stored == to.stored => {
                if from.expression != to.expression {
                    recomputed = true;
                    changes.push(Change::AlterColumnExpression {
                        uid: uid.clone(),
                        column: declared_ref.clone(),
                        from: from.expression.clone(),
                        to: to.expression.clone(),
                    });
                }
            }
            _ => errs.push(DiffError::GenerationChangeUnsupported {
                column: declared_ref.clone(),
            }),
        }

        let norm = |t: &ColumnType| dialect.normalize_type(t).unwrap_or_else(|_| t.clone());
        let (from_ty, to_ty) = (norm(&base_col.ty), norm(&col.ty));
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
        if retyped {
            changes.push(Change::AlterColumnType {
                uid: uid.clone(),
                column: declared_ref.clone(),
                from: from_ty.clone(),
                to: to_ty.clone(),
                from_nullable: base_col.nullable,
                to_nullable: col.nullable || tightened_after,
                from_collation: base_col.collation.clone(),
                to_collation: col.collation.clone(),
            });
            if tightened_after {
                changes.push(Change::AlterColumnNullability {
                    uid: uid.clone(),
                    column: declared_ref.clone(),
                    ty: to_ty,
                    to_nullable: false,
                    collation: col.collation.clone(),
                });
            }
        } else if base_col.nullable != col.nullable {
            changes.push(Change::AlterColumnNullability {
                uid: uid.clone(),
                column: declared_ref.clone(),
                ty: to_ty,
                to_nullable: col.nullable,
                collation: col.collation.clone(),
            });
        }
        if base_col.default != col.default {
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
            let split = retyped && base_col.default.is_some() && col.default.is_some();
            changes.push(Change::AlterColumnDefault {
                uid: uid.clone(),
                column: declared_ref.clone(),
                from: base_col.default.clone(),
                to: if split { None } else { col.default.clone() },
            });
            if split {
                changes.push(Change::AlterColumnDefault {
                    uid: uid.clone(),
                    column: declared_ref.clone(),
                    from: None,
                    to: col.default.clone(),
                });
            }
        }
        if base_col.deprecated != col.deprecated {
            changes.push(Change::SetColumnDeprecated {
                uid: uid.clone(),
                column: declared_ref.clone(),
                reason: col.deprecated.clone(),
            });
        }
    }

    // Present only on the base side: a dropped column.
    for (uid, base_ref) in &base_cols {
        if !declared_cols.contains_key(uid) {
            changes.push(Change::DropColumn {
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
            });
        }
    }
}

fn columns_of(ids: &IdsFile, table: &TableName) -> BTreeMap<Uid, ColumnRef> {
    ids.columns
        .iter()
        .filter(|(_, c)| &c.table == table)
        .map(|(u, c)| (u.clone(), c.clone()))
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
            Change::SetPrimaryKey { from: Some(_), .. }
                | Change::DropUnique { .. }
                | Change::DropIndex { .. }
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
            Change::SetPrimaryKey {
                table,
                from: Some(key),
                ..
            } => (table, key.columns.iter().cloned().collect::<BTreeSet<_>>()),
            Change::DropUnique { table, name } => {
                let Some(key) = aligned.get(table).and_then(|t| t.unique.get(name)) else {
                    continue;
                };
                (table, key.columns.iter().cloned().collect::<BTreeSet<_>>())
            }
            Change::DropIndex { table, name } => {
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
            Change::CreateTable { .. }
            | Change::DropTable { .. }
            | Change::DetachPartition { .. }
            | Change::AttachPartition { .. }
            | Change::RenameTable { .. }
            | Change::AddColumn { .. }
            | Change::DropColumn { .. }
            | Change::RenameColumn { .. }
            | Change::AlterColumnType { .. }
            | Change::AlterColumnNullability { .. }
            | Change::AlterColumnDefault { .. }
            | Change::AlterColumnExpression { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::SetPrimaryKey { .. }
            | Change::AddUnique { .. }
            | Change::AddForeignKey { .. }
            | Change::DropForeignKey { .. }
            | Change::AddCheck { .. }
            | Change::DropCheck { .. }
            | Change::AddIndex { .. }
            | Change::AddComputedColumn { .. }
            | Change::DropComputedColumn { .. }
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
            | Change::PublicExecution { .. }
            | Change::SetIndexStorageParameters { .. }
            | Change::SetTablePersistence { .. }
            | Change::SetPartitionDefault { .. }
            | Change::SetPartitionNotNull { .. }
            | Change::SetStorageParameters { .. }
            | Change::SetReplicaIdentity { .. } => continue,
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
                || changes.iter().any(|c| matches!(c, Change::DropForeignKey { table: on, name: n } if on == table && n == name)) {
                continue;
            }
            changes.push(Change::DropForeignKey {
                table: table.clone(),
                name: name.clone(),
            });
            changes.push(Change::AddForeignKey {
                table: table.clone(),
                name: name.clone(),
                constraint: Box::new(wanted.clone()),
            });
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
                    matches!(c, Change::AddForeignKey { table, name: n, .. }
                        if table == new_name && n == name)
                })
            {
                continue;
            }
            pairs.push(Change::DropForeignKey {
                table: new_name.clone(),
                name: name.clone(),
            });
            pairs.push(Change::AddForeignKey {
                table: new_name.clone(),
                name: name.clone(),
                constraint: Box::new(wanted.clone()),
            });
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

/// The table a row change writes or deletes in.
fn row_table(change: &Change) -> Option<&TableName> {
    if let Change::InsertRow { table, .. }
    | Change::UpdateRow { table, .. }
    | Change::DeleteRow { table, .. } = change
    {
        Some(table)
    } else {
        None
    }
}

/// A tightening folded into a retype comes out of it when the plan writes or
/// deletes rows of its table. The retype keeps the column nullable, and an
/// `AlterColumnNullability` tightens it after the rows, which may be what
/// fills or removes its NULLs (#1367). The same split DEC-1168.1 makes for a
/// recomputed column, for the same reason: the check has to meet the values
/// the plan leaves, not the ones it found.
fn split_tightenings_from_retypes(changes: &mut Vec<Change>) {
    let rows = tables_with_row_changes(changes);
    let mut split = Vec::new();
    for change in changes.iter_mut() {
        if let Change::AlterColumnType {
            uid,
            column,
            to,
            from_nullable: true,
            to_nullable,
            to_collation,
            ..
        } = change
            && !*to_nullable
            && rows.contains(&column.table)
        {
            *to_nullable = true;
            split.push(Change::AlterColumnNullability {
                uid: uid.clone(),
                column: column.clone(),
                ty: to.clone(),
                to_nullable: false,
                collation: to_collation.clone(),
            });
        }
    }
    changes.extend(split);
}

/// Dependency maintenance is visible in the saved plan, including its ordinary
/// drop/add risks (DECISIONS 461). The emitter must not discover extra
/// cross-table work after approval. Align identities first so the selection survives simultaneous
/// table and column renames; explicit replacements already have their own pair.
fn recreate_retyped_dependents(
    base: Side<'_>,
    declared: Side<'_>,
    renames: &Renames,
    dialect: &dyn Dialect,
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
            if let Change::DropComputedColumn { table, name, .. } = c
                && *table == column.table
                && *name == column.name
            {
                Some(true)
            } else if let Change::DropColumn {
                column: dropped, ..
            } = c
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
            if let Change::AddComputedColumn { table, name, .. } = change {
                Some((table.column(name), true))
            } else if let Change::AddColumn { table, name, .. } = change {
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
        Change::AlterColumnType {
            column,
            from,
            to,
            from_nullable,
            to_nullable,
            from_collation,
            to_collation,
            ..
        } => {
            let mut dependents = dialect.retype_dependents(from, to);
            if from_collation != to_collation {
                also(&mut dependents, dialect.recollate_dependents());
            }
            // A nullability change in the same statement brings its own
            // blockers, which a type change alone may not have (#1363).
            if from_nullable != to_nullable {
                also(
                    &mut dependents,
                    dialect.nullability_dependents(*to_nullable),
                );
            }
            Some((column.clone(), dependents))
        }
        Change::AlterColumnNullability {
            column,
            to_nullable,
            ..
        } => Some((column.clone(), dialect.nullability_dependents(*to_nullable))),
        Change::CreateTable { .. }
        | Change::DropTable { .. }
        | Change::DetachPartition { .. }
        | Change::AttachPartition { .. }
        | Change::RenameTable { .. }
        | Change::AddColumn { .. }
        | Change::DropColumn { .. }
        | Change::RenameColumn { .. }
        | Change::AlterColumnDefault { .. }
        | Change::AlterColumnExpression { .. }
        | Change::SetColumnDeprecated { .. }
        | Change::SetPrimaryKey { .. }
        | Change::SetIndexStorageParameters { .. }
        | Change::SetTablePersistence { .. }
        | Change::SetPartitionDefault { .. }
        | Change::SetPartitionNotNull { .. }
        | Change::SetStorageParameters { .. }
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
        | Change::PublicExecution { .. } => None,
    }) {
        let merged = retyped.entry(column).or_default();
        also(merged, dependents);
    }
    if retyped.is_empty() {
        return;
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
        let key_column = |column: &str| {
            retyped
                .get(&name.column(column))
                .is_some_and(|d| d.keys_and_indexes)
        };
        let checks = retyped.iter().any(|(c, d)| &c.table == name && d.checks);
        let filters = retyped
            .iter()
            .any(|(c, d)| &c.table == name && d.filtered_indexes);
        if let (Some(key), Some(wanted)) = (&before.primary_key, &after.primary_key)
            && key.columns.iter().any(|c| key_column(c))
            && !changes
                .iter()
                .any(|c| matches!(c, Change::SetPrimaryKey { table, .. } if table == name))
        {
            changes.push(Change::SetPrimaryKey {
                table: name.clone(),
                from: Some(key.clone()),
                to: None,
                nonclustered: false,
            });
            changes.push(Change::SetPrimaryKey {
                table: name.clone(),
                from: None,
                to: Some(wanted.clone()),
                nonclustered: !after.primary_key_is_clustered(),
            });
        }
        // Only a constraint or index whose layout stays as it was: one whose
        // layout moves is already dropped and re-added by `diff_constraints`,
        // and a second pair here would drop it twice.
        for (n, key) in &before.unique {
            if after.unique.get(n) == Some(key)
                && before.unique_is_clustered(n) == after.unique_is_clustered(n)
                && key.columns.iter().any(|c| key_column(c))
            {
                changes.push(Change::DropUnique {
                    table: name.clone(),
                    name: n.clone(),
                });
                changes.push(Change::AddUnique {
                    table: name.clone(),
                    name: n.clone(),
                    constraint: key.clone(),
                    clustered: after.unique_is_clustered(n),
                });
            }
        }
        for (n, check) in &before.checks {
            if checks && after.checks.get(n) == Some(check) {
                changes.push(Change::DropCheck {
                    table: name.clone(),
                    name: n.clone(),
                });
                changes.push(Change::AddCheck {
                    table: name.clone(),
                    name: n.clone(),
                    constraint: check.clone(),
                });
            }
        }
        for (n, index) in &before.indexes {
            if after.indexes.get(n) == Some(index)
                && before.index_is_clustered(n) == after.index_is_clustered(n)
                && (index
                    .columns
                    .iter()
                    .any(|c| c.key.column().is_some_and(key_column))
                    || index.include.iter().any(|c| key_column(c))
                    // An expression names its columns inside text this does
                    // not parse, so it is treated as a filter is: rebuilt
                    // wherever the dialect rebuilds filtered indexes
                    // (DEC-1169.2).
                    || (filters
                        && (index.filter.is_some()
                            || index.columns.iter().any(|c| c.key.expression().is_some()))))
            {
                changes.push(Change::DropIndex {
                    table: name.clone(),
                    name: n.clone(),
                });
                changes.push(Change::AddIndex {
                    table: name.clone(),
                    name: n.clone(),
                    index: Box::new(index.clone()),
                    clustered: after.index_is_clustered(n),
                });
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
                changes.push(Change::DropForeignKey {
                    table: name.clone(),
                    name: n.clone(),
                });
                changes.push(Change::AddForeignKey {
                    table: name.clone(),
                    name: n.clone(),
                    constraint: Box::new(fk.clone()),
                });
            }
        }
    }
}

/// Constraints and indexes are always matched by name and never modified in
/// place — the database itself does drop + add, and pretending otherwise would
/// only give the emitter one more path that can fail.
/// The column a change renames, drops, retypes or tightens or relaxes on
/// `table`, under the name its expression would use, and what the change
/// does to it. Every one is an `ALTER COLUMN` or an `sp_rename` the engine
/// refuses on a computed column's input (#1174).
fn input_change<'a>(change: &'a Change, table: &TableName) -> Option<(&'a str, &'static str)> {
    if let Change::RenameColumn { table: t, from, .. } = change
        && t == table
    {
        return Some((from, "renames"));
    }
    if let Change::DropColumn { column, .. } = change
        && column.table == *table
    {
        return Some((&column.name, "drops"));
    }
    if let Change::AlterColumnType { column, .. } = change
        && column.table == *table
    {
        return Some((&column.name, "retypes or recollates"));
    }
    if let Change::AlterColumnNullability { column, .. } = change
        && column.table == *table
    {
        return Some((&column.name, "changes the nullability of"));
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
            if let Change::CreateTable { name, .. } = c {
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
            Change::DropTable { .. } | Change::RenameTable { .. }
        );
        if matches!(change, Change::CreateTable { .. })
            || (!ends_a_table && change.table().is_some_and(|t| created.contains(t)))
        {
            continue;
        }
        // A nullable column the engine adds to the history beside the table,
        // NULL in every row of both, with versioning left on (DEC-1177.1,
        // measured on 17.0). A period column is declared through
        // `system_time`, whose difference is refused above.
        if let Change::AddColumn { column, .. } = change
            && column.nullable
        {
            continue;
        }
        // A drop or a rename acts on the table the base has at that name, and
        // a rename brings it to the declared one: asked of those sides, so the
        // ordinary table a new temporal one replaces may still go.
        let asked: Vec<&TableName> = if let Change::DropTable { name, .. } = change {
            vec![name]
                .into_iter()
                .filter(|n| temporal(base.schema, n))
                .collect()
        } else if let Change::RenameTable { from, to, .. } = change {
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
        let what = if matches!(change, Change::AddColumn { .. }) {
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
/// definition, its name aside (#1544, DEC-1544.1), and the checks and indexes
/// `partition` has of its own, which a detach leaves as they are, under their
/// own names (#1577).
fn detached_names(
    parent: &Table,
    partition: &Table,
    declared: &Table,
    dialect: &dyn Dialect,
) -> Result<Vec<DetachedName>, Vec<String>> {
    let mut names = Vec::new();
    let mut what = Vec::new();
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
                    c.default = Some(default.clone());
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
                "its own check `{name}` is not declared as it stands"
            ));
        }
    }
    for (name, own) in &partition.indexes {
        if declared.indexes.get(name) == Some(own) {
            declared.indexes.remove(name);
        } else {
            what.push(format!(
                "its own index `{name}` is not declared as it stands"
            ));
        }
    }
    let declared = &declared;
    // Types in the engine's spelling, as `diff_columns` compares them: the
    // parent's base reads `integer` back where its file says `int`. A
    // description is prose `diff` does not compare, and a connected base
    // never holds one, so it is not compared here either.
    let columns = |t: &Table| -> Vec<(String, pbps_model::Column)> {
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
    if columns(parent) != columns(declared) {
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
        parent: &BTreeMap<String, V>,
        declared: &BTreeMap<String, V>,
        names: &mut Vec<DetachedName>,
        what: &mut Vec<String>,
    ) {
        let mut unused: Vec<(&String, &V)> = declared.iter().collect();
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
                None => what.push(format!("its parent's {words} `{from}` is missing")),
            }
        }
        for (to, _) in unused {
            what.push(format!("the {words} `{to}` is not its parent's"));
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

/// A table being attached as declared (#1545): itself as a partition the
/// moment the attach has run, holding only what the engine leaves it of its
/// own, and its columns without a default where the parent's has one.
struct Attached {
    table: Table,
    defaultless: Vec<String>,
    /// The table's indexes dropped before the attach: every one the engine
    /// could adopt but for the one left to it.
    displaced: Vec<String>,
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
    dialect: &dyn Dialect,
    read_back: Option<&Schema>,
) -> Result<Attached, Vec<String>> {
    let mut what = Vec::new();
    let Some(parent) = base
        .schema
        .tables
        .get(&of.parent)
        .filter(|p| p.partition_by.is_some() && p.partition_of.is_none())
    else {
        return Err(vec![format!(
            "its parent {} is not a partitioned table before this plan; create the partition \
             with its parent, or attach the table in a later plan",
            of.parent
        )]);
    };
    if base_name != declared_name {
        what.push(format!(
            "it is renamed from {base_name} in the same plan; rename it in one plan and attach it \
             in another"
        ));
    }
    if table.partition_by.is_some() {
        what.push("it is partitioned itself".to_owned());
    }
    if of.bound == pbps_model::PartitionBound::Default {
        what.push(
            "it would be the DEFAULT partition, which only a range is attached as here; create \
             the DEFAULT partition and move the rows into it"
                .to_owned(),
        );
    }
    // Types in the engine's spelling, as `diff_columns` compares them. A
    // default and NOT NULL may be its own, and are compared below.
    let columns = |t: &Table| -> Vec<(String, pbps_model::Column)> {
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
                    default: None,
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
            let Some(generated) = &mut spec.generated else {
                continue;
            };
            match read {
                Some(read) => {
                    if let Some(engine) = read
                        .and_then(|r| r.columns.get(column))
                        .and_then(|c| c.generated.as_ref())
                    {
                        generated.expression = engine.expression.clone();
                    }
                }
                None => generated.expression.clear(),
            }
        }
        t
    };
    let (table_spelled, parent_spelled) = (spelled(table, base_name), spelled(parent, &of.parent));
    let (mine, theirs) = (columns(&table_spelled), columns(&parent_spelled));
    let names = |c: &[(String, pbps_model::Column)]| {
        c.iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    // The names themselves, not their joined spelling, which two lists can
    // share when a name holds `, ` (#1642 review).
    if !mine
        .iter()
        .map(|(n, _)| n)
        .eq(theirs.iter().map(|(n, _)| n))
    {
        what.push(format!(
            "its columns are ({}), and its parent's are ({}), in that order",
            names(&mine),
            names(&theirs)
        ));
    } else {
        for ((name, a), (_, b)) in mine.iter().zip(&theirs) {
            if a != b {
                what.push(format!(
                    "its column `{name}` is not its parent's in type, collation, identity or \
                     generation"
                ));
            } else if a.generated.is_some() && base_name.schema != of.parent.schema {
                // The engine does not compare generation expressions as it
                // attaches, and keeps the table's: in another schema the same
                // text may call another function, and each partition would
                // then compute the column its own way (measured on 18, #1642
                // review).
                what.push(format!(
                    "its column `{name}` is generated, and in another schema than its parent \
                     the same expression may call another function; move the table to its \
                     parent's schema first"
                ));
            }
        }
    }
    let mut own_columns = BTreeMap::new();
    let mut defaultless = Vec::new();
    for (name, c) in &table.columns {
        let Some(p) = parent.columns.get(name) else {
            continue;
        };
        if c.nullable && !p.nullable {
            what.push(format!(
                "its column `{name}` is nullable, and its parent's is NOT NULL"
            ));
        }
        let own = pbps_model::PartitionColumn {
            // The same text as the parent's is the parent's, as the reader
            // reads it back, but only in the parent's schema: elsewhere an
            // unqualified name in it may be another schema's object, so it
            // is the table's own, and taking the parent's back sets the
            // parent's under the parent's path (DEC-1581.1).
            default: c
                .default
                .clone()
                .filter(|d| base_name.schema != of.parent.schema || Some(d) != p.default.as_ref()),
            not_null: !c.nullable && p.nullable,
        };
        if c.default.is_none() && p.default.is_some() {
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
        own: &'a BTreeMap<String, V>,
        parent: &BTreeMap<String, V>,
    ) -> Vec<&'a String> {
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
    let unique = |t: &Table| -> BTreeMap<String, pbps_model::UniqueConstraint> {
        t.unique
            .iter()
            .map(|(n, u)| {
                let mut u = u.clone();
                u.storage_parameters.clear();
                (n.clone(), u)
            })
            .collect()
    };
    // An index as the engine matches it to adopt it: storage parameters
    // and sort order aside, measured on 16 and 18 (`DESC` and `NULLS FIRST`
    // adopted, another collation, operator class, `INCLUDE` or method not).
    let index = |t: &Table| -> BTreeMap<String, Index> {
        t.indexes
            .iter()
            .map(|(n, i)| {
                let mut i = i.clone();
                i.storage_parameters.clear();
                for c in &mut i.columns {
                    c.descending = false;
                }
                (n.clone(), i)
            })
            .collect()
    };
    for name in unmatched(&unique(table), &unique(parent)) {
        what.push(format!(
            "its unique constraint `{name}` is not its parent's, and a partition holds none of \
             its own yet"
        ));
    }
    for name in unmatched(&table.foreign_keys, &parent.foreign_keys) {
        what.push(format!(
            "its foreign key `{name}` is not its parent's, and a partition holds none of its own \
             yet"
        ));
    }
    let mut checks = table.checks.clone();
    for (name, p) in &parent_spelled.checks {
        checks.remove(name);
        if table_spelled.checks.get(name) != Some(p) {
            what.push(format!(
                "its parent's check `{name}` is not on it under that name, which the engine \
                 requires"
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
    let mut taken: BTreeSet<&String> = BTreeSet::new();
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
        let matches: Vec<&String> = own_indexes
            .iter()
            .filter(|(n, d)| *d == definition && !taken.contains(n))
            .map(|(n, _)| n)
            .collect();
        let left = matches
            .iter()
            .find(|n| !declared.indexes.contains_key(n.as_str()))
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
    let backing = |columns: &[String]| Index {
        columns: columns
            .iter()
            .map(|c| pbps_model::IndexColumn {
                key: pbps_model::IndexKey::Column(c.clone()),
                descending: false,
                opclass: None,
            })
            .collect(),
        include: Vec::new(),
        unique: true,
        filter: None,
        method: Default::default(),
        storage_parameters: Default::default(),
    };
    let contested = |columns: &[String]| parents_indexes.values().any(|i| *i == backing(columns));
    let mut released = Vec::new();
    if let Some(key) = &table.primary_key
        && contested(&key.columns)
    {
        released.push(Change::SetPrimaryKey {
            table: declared_name.clone(),
            from: Some(key.clone()),
            to: None,
            nonclustered: false,
        });
    }
    for (name, u) in &table.unique {
        if contested(&u.columns) {
            released.push(Change::DropUnique {
                table: declared_name.clone(),
                name: name.clone(),
            });
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
    for id in base.schema.modules.keys() {
        if let pbps_model::ModuleId::Trigger { on, name } = id
            && on == base_name
        {
            what.push(format!(
                "the trigger `{name}` is on it, and a partition holds none yet"
            ));
        }
    }
    for (other, t) in &base.schema.tables {
        for (name, fk) in &t.foreign_keys {
            if &fk.references_table == base_name {
                what.push(format!(
                    "the foreign key `{name}` of {other} references it, and nothing references a \
                     partition yet"
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
    hints: &Hints,
    changes: &[Change],
    errs: &mut Vec<DiffError>,
) {
    let partitioned = |schema: &Schema, name: &TableName| {
        schema
            .tables
            .get(name)
            .is_some_and(|t| t.partition_by.is_some() || t.partition_of.is_some())
    };
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
            if let Some(of) = parent {
                moving
                    .entry(of.parent.clone())
                    .or_default()
                    .push(declared_name.clone());
            }
            fn where_(t: &Table) -> Option<(&TableName, &pbps_model::PartitionBound)> {
                t.partition_of.as_ref().map(|of| (&of.parent, &of.bound))
            }
            // A key column renamed is the same key: the engine renames it in
            // the key as it does in the table (measured on 16 and 18, #1687).
            let renamed = |by: &pbps_model::PartitionBy| {
                let names = renamed_columns(base.ids, base_name, declared.ids, declared_name);
                pbps_model::PartitionBy {
                    columns: by
                        .columns
                        .iter()
                        .map(|c| names.get(c).cloned().unwrap_or_else(|| c.clone()))
                        .collect(),
                }
            };
            if (b.partition_by.as_ref().map(renamed) != d.partition_by || where_(b) != where_(d))
                && !detached
                && !attached
            {
                refuse(declared_name, "change its partitioning".to_owned());
            }
        }
    }
    // As for a temporal table: the changes split out of a table this plan
    // creates are the creation; a drop or a rename is asked of the side it
    // acts on.
    let created: BTreeSet<&TableName> = changes
        .iter()
        .filter_map(|c| {
            if let Change::CreateTable { name, .. } = c {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    let attaching: BTreeSet<&TableName> = changes
        .iter()
        .filter_map(|c| {
            if let Change::AttachPartition { table, .. } = c {
                Some(table)
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
        if let Change::DropTable {
            name,
            detach_from: Some(parent),
            ..
        } = change
        {
            moving.entry(parent.clone()).or_default().push(name.clone());
        }
    }
    for change in changes {
        // A partition is created with its parent, or under a parent that
        // already stands (#1171); either way it is a creation, and whether
        // the parent's rows let it be is the connected probe's question.
        if matches!(change, Change::CreateTable { .. }) {
            continue;
        }
        // A partition is dropped, detached first, while its parent stands
        // (#1171), or detached and kept (#1544). Its parent's own drop is
        // still refused below.
        if matches!(
            change,
            Change::DropTable {
                detach_from: Some(_),
                ..
            } | Change::DetachPartition { .. }
                | Change::AttachPartition { .. }
        ) {
            continue;
        }
        // A standing partition's own: its indexes and checks, which the
        // reader never confuses with its parent's clones (#1577), its
        // defaults and NOT NULLs, its storage parameters and its persistence
        // (#1581). A clone is its parent's to change (#1546).
        let own = matches!(
            change,
            Change::AddIndex { .. }
                | Change::DropIndex { .. }
                | Change::AddCheck { .. }
                | Change::DropCheck { .. }
                | Change::SetPartitionDefault { .. }
                | Change::SetPartitionNotNull { .. }
                | Change::SetStorageParameters { .. }
                | Change::SetTablePersistence { .. }
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
        // Before the attach, still the ordinary table's: a key or unique
        // constraint whose index a parent's plain index could take (#1642
        // review). On a standing partition it is the parent's clone.
        if matches!(
            change,
            Change::DropUnique { .. } | Change::SetPrimaryKey { to: None, .. }
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
            schema
                .tables
                .get(name)
                .is_some_and(|t| t.partition_by.is_some() && t.partition_of.is_none())
        };
        // A standing parent's indexes (#1688), keys, checks and foreign keys
        // (#1689): the engine recurses each into every partition, a clone on
        // each that the reader leaves to the parent (DEC-1577.1). A new name
        // is a drop and an add, as on any table.
        if let Change::AddIndex { table, .. }
        | Change::DropIndex { table, .. }
        | Change::SetPrimaryKey { table, .. }
        | Change::AddUnique { table, .. }
        | Change::DropUnique { table, .. }
        | Change::AddCheck { table, .. }
        | Change::DropCheck { table, .. }
        | Change::AddForeignKey { table, .. }
        | Change::DropForeignKey { table, .. } = change
            && parent_standing(base.schema, table)
            && parent_standing(declared.schema, table)
        {
            if let Some(tables) = moving.get(table) {
                // The same two plans as a column change (DEC-1687.1): a
                // detach's shape and a unique index's probe each read the
                // tree on one side of the other (#1737 review).
                let tables: Vec<String> = tables.iter().map(ToString::to_string).collect();
                changes_meet_moves.insert(table.clone());
                refuse(
                    table,
                    format!(
                        "{} while this plan attaches, detaches or drops {}; change the parent and \
                         the partitions in separate plans",
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
                schema
                    .tables
                    .get(table)
                    .and_then(|t| t.partition_by.as_ref())
                    .is_some_and(|by| by.columns.iter().any(|c| c == column))
            };
            // A drop names the base column and a retype the declared one, so
            // each is asked of its own side's key: across one plan a name
            // can leave the key's column for another (#1692 review).
            if (matches!(change, Change::DropColumn { .. }) && key(base.schema))
                || (matches!(change, Change::AlterColumnType { .. }) && key(declared.schema))
            {
                refuse(
                    table,
                    format!("{}, which is in its partition key", change_in_words(change)),
                );
            } else if let Change::AddColumn { column: c, .. } = change
                && c.identity.is_some()
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
                && !matches!(change, Change::SetColumnDeprecated { .. })
            {
                // An attach or a detach meets the parent's columns as they
                // stand at that statement, and the table on its other side
                // holds them as declared at the other end of the plan: each
                // column change would need its own place against each
                // transition (#1692 review). Two plans keep each simple.
                let tables: Vec<String> = tables.iter().map(ToString::to_string).collect();
                changes_meet_moves.insert(table.clone());
                refuse(
                    table,
                    format!(
                        "{} while this plan attaches, detaches or drops {}; change the columns and the \
                         partitions in separate plans",
                        change_in_words(change),
                        tables.join(", ")
                    ),
                );
            }
            continue;
        }
        let ends_a_table = matches!(
            change,
            Change::DropTable { .. } | Change::RenameTable { .. }
        );
        if !ends_a_table && change.table().is_some_and(|t| created.contains(t)) {
            continue;
        }
        let asked: Vec<&TableName> = if let Change::DropTable { name, .. } = change {
            vec![name]
                .into_iter()
                .filter(|n| partitioned(base.schema, n))
                .collect()
        } else if let Change::RenameTable { from, to, .. } = change {
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
    if matches!(change, Change::AddIndex { .. } | Change::DropIndex { .. })
        && hints.strategies.get(table).is_some_and(|s| s.online)
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
    let missing_key = |columns: &[String]| {
        parent.partition_by.as_ref().and_then(|by| {
            by.columns
                .iter()
                .find(|key| !columns.contains(key))
                .cloned()
        })
    };
    let without_key = |missing: String| {
        format!(
            "{} without the partition key column `{missing}`, which PostgreSQL refuses on a \
             partitioned table; add `{missing}` to its columns",
            change_in_words(change)
        )
    };
    if let Change::SetPrimaryKey { to: Some(pk), .. } = change {
        return missing_key(&pk.columns).map(without_key);
    }
    if let Change::AddUnique { constraint, .. } = change {
        return missing_key(&constraint.columns).map(without_key);
    }
    // A partition's own check under the parent's new check's name is
    // absorbed into it, measured on 16 and 18: the same expression makes it
    // the parent's clone, which the parent's later drop removes, and another
    // is refused by the engine. Refused by name (leon, 2026-10-08), the
    // remedy being to drop or rename the partition's own first.
    if let Change::AddCheck { name, .. } = change {
        let held: Vec<String> = partitions()
            .filter(|(_, t)| t.checks.contains_key(name.as_str()))
            .map(|(partition, _)| partition.to_string())
            .collect();
        return (!held.is_empty()).then(|| {
            format!(
                "add check `{name}`, a name {} holds as its own check, which the parent's would \
                 absorb; drop or rename the partition's own check in an earlier plan",
                held.join(", ")
            )
        });
    }
    let Change::AddIndex { name, index, .. } = change else {
        return None;
    };
    if index.unique
        && let Some(by) = &parent.partition_by
        && let Some(missing) = by.columns.iter().find(|key| {
            !index
                .columns
                .iter()
                .any(|c| c.key.column() == Some(key.as_str()))
        })
    {
        return Some(format!(
            "add unique index `{name}` without the partition key column `{missing}`, which \
             PostgreSQL refuses on a partitioned table; add `{missing}` to its columns"
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
                        b.indexes.contains_key(own.as_str())
                            && !changes.iter().any(|c| {
                                matches!(c, Change::DropIndex { table, name }
                                    if table == partition && name == *own)
                            })
                    })
                })
                .filter(|(_, own)| adopts(index, own))
                .map(move |(own, _)| format!("{partition}'s `{own}`"))
        })
        .collect();
    (!adopted.is_empty()).then(|| {
        format!(
            "add index `{name}`, which can take {} as the partition's copy of it and leave the \
             partition without its own; drop or rename the partition's own index in an earlier \
             plan",
            adopted.join(", ")
        )
    })
}

/// Whether a new parent index can take a partition's own index as its
/// clone. Measured on 16.15 and 18.6: the method, the keys in order with
/// their classes, uniqueness, the predicate and the `INCLUDE` columns must
/// match; a key's direction and the storage parameters need not (#1688).
///
/// An expression key or a predicate is a match whatever its text, as the
/// attach path takes it (DEC-1545.1): the engine compares what each parses
/// and binds to, so `n+1` and `n + 1` are one index to it (#1737 review).
fn adopts(parent: &pbps_model::Index, own: &pbps_model::Index) -> bool {
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

/// The column a parent's column change names, for the kinds the engine
/// recurses into every partition (#1687): its name before a rename, as the
/// partition key holds it.
// The complement is every change that names no column of a table.
#[allow(clippy::wildcard_enum_match_arm)]
fn parent_column(change: &Change) -> Option<&String> {
    match change {
        Change::AddColumn { name, .. } => Some(name),
        Change::RenameColumn { from, .. } => Some(from),
        Change::DropColumn { column, .. }
        | Change::AlterColumnType { column, .. }
        | Change::AlterColumnNullability { column, .. }
        | Change::AlterColumnDefault { column, .. }
        | Change::SetColumnDeprecated { column, .. } => Some(&column.name),
        _ => None,
    }
}

/// A table's columns this plan renames, base name to declared name, by uid.
fn renamed_columns(
    base: &IdsFile,
    base_name: &TableName,
    declared: &IdsFile,
    declared_name: &TableName,
) -> BTreeMap<String, String> {
    let now = columns_of(declared, declared_name);
    columns_of(base, base_name)
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
fn diff_partition_columns(
    uid: &Uid,
    name: &TableName,
    base_table: &Table,
    declared_table: &Table,
    base: Side<'_>,
    declared: Side<'_>,
    changes: &mut Vec<Change>,
) {
    let (Some(was), Some(now)) = (&base_table.partition_of, &declared_table.partition_of) else {
        return;
    };
    let parent = declared.schema.tables.get(&now.parent);
    let base_parent = base.schema.tables.get(&was.parent);
    // What became of each of the parent's base columns, by uid: its declared
    // name, or `None` where the plan drops it. By uid and not by name, since
    // one plan can drop a column and rename another into its name (#1692
    // review). A column no identity names is taken as it stands.
    let now_by_uid = columns_of(declared.ids, &now.parent);
    let fate: BTreeMap<String, Option<String>> = columns_of(base.ids, &was.parent)
        .into_iter()
        .map(|(uid, c)| (c.name, now_by_uid.get(&uid).map(|d| d.name.clone())))
        .collect();
    let is_now = |c: &String| -> Option<String> {
        match fate.get(c) {
            Some(now) => now.clone(),
            None => Some(c.clone()),
        }
    };
    let was_columns: BTreeMap<String, &pbps_model::PartitionColumn> = was
        .columns
        .iter()
        .filter_map(|(c, own)| Some((is_now(c)?, own)))
        .collect();
    let base_of: BTreeMap<String, &pbps_model::Column> = base_parent
        .map(|p| {
            p.columns
                .iter()
                .filter_map(|(c, column)| Some((is_now(c)?, column)))
                .collect()
        })
        .unwrap_or_default();
    let none = pbps_model::PartitionColumn::default();
    let loosened: BTreeSet<String> = parent
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
    let columns: BTreeSet<&String> = was_columns
        .keys()
        .chain(now.columns.keys())
        .chain(&loosened)
        .collect();
    for column in columns {
        let from = was_columns.get(column).copied().unwrap_or(&none);
        let to = now.columns.get(column).unwrap_or(&none);
        let theirs = parent.and_then(|p| p.columns.get(column));
        let theirs_was = base_of.get(column);
        let default_moved =
            theirs_was.is_some_and(|b| Some(&b.default) != theirs.map(|c| &c.default));
        if from.default != to.default || (default_moved && to.default.is_some()) {
            changes.push(Change::SetPartitionDefault {
                uid: uid.clone(),
                table: name.clone(),
                parent: now.parent.clone(),
                column: column.clone(),
                from: from.default.clone(),
                to: to.default.clone(),
                // What the engine gave the partition when it was made, and
                // what a row written to it directly gets without one.
                fallback: parent
                    .and_then(|p| p.columns.get(column))
                    .and_then(|c| c.default.clone()),
            });
        }
        let tightened =
            theirs.is_some_and(|c| !c.nullable) && theirs_was.is_some_and(|b| b.nullable);
        if !tightened && (from.not_null != to.not_null || loosened.contains(column)) {
            changes.push(Change::SetPartitionNotNull {
                uid: uid.clone(),
                table: name.clone(),
                column: column.clone(),
                not_null: to.not_null,
            });
        }
    }
}

/// A change's kind as words, `drop column` for `DropColumn`: the variant's
/// own name, so a kind added later is named without anyone remembering to.
fn change_in_words(change: &Change) -> String {
    let debug = format!("{change:?}");
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
    dialect: &dyn Dialect,
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
    let dropped = |table: &TableName, name: &str| {
        changes.iter().any(|c| {
            matches!(c, Change::DropComputedColumn { table: t, name: n, .. }
                if t == table && n == name)
        })
    };
    // Added by an `ADD`, or with its table by a `CREATE TABLE`: both come
    // before a module this plan creates (class 7 and 9, before 14).
    let added = |table: &TableName, name: &str| {
        changes.iter().any(|c| {
            matches!(c, Change::AddComputedColumn { table: t, name: n, .. }
                if t == table && n == name)
                || matches!(c, Change::CreateTable { name: t, table: created, .. }
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
                        column: column.to_owned(),
                        change: what,
                    });
                }
                let module = match change {
                    // Standing, or re-added: either way it calls the module
                    // when the module changes. One only dropped is out of the
                    // way first, its module's drop moved after it.
                    Change::AlterModule { id, .. } if screened || added(table_name, name) => {
                        Some((id, "alters"))
                    }
                    Change::DropModule { id, .. } if screened || added(table_name, name) => {
                        Some((id, "drops"))
                    }
                    Change::CreateModule { id, .. } if added(table_name, name) => {
                        Some((id, "creates"))
                    }
                    Change::CreateTable { .. }
                    | Change::DropTable { .. }
                    | Change::DetachPartition { .. }
                    | Change::AttachPartition { .. }
                    | Change::RenameTable { .. }
                    | Change::AddColumn { .. }
                    | Change::DropColumn { .. }
                    | Change::RenameColumn { .. }
                    | Change::AlterColumnType { .. }
                    | Change::AlterColumnNullability { .. }
                    | Change::AlterColumnDefault { .. }
                    | Change::AlterColumnExpression { .. }
                    | Change::AddComputedColumn { .. }
                    | Change::DropComputedColumn { .. }
                    | Change::SetColumnDeprecated { .. }
                    | Change::SetPrimaryKey { .. }
                    | Change::SetIndexStorageParameters { .. }
                    | Change::SetTablePersistence { .. }
                    | Change::SetPartitionDefault { .. }
                    | Change::SetPartitionNotNull { .. }
                    | Change::SetStorageParameters { .. }
                    | Change::SetReplicaIdentity { .. }
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
                    | Change::Revoke { .. }
                    | Change::PublicExecution { .. } => None,
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
            changes.push(Change::DropComputedColumn {
                table: name.clone(),
                name: column.clone(),
                computed: was.clone(),
            });
        }
    }
    for (column, wanted) in &declared.computed {
        if !kept(Some(wanted), base.computed.get(column)) {
            changes.push(Change::AddComputedColumn {
                table: name.clone(),
                name: column.clone(),
                computed: wanted.clone(),
            });
        }
    }
}

/// Each declared table's strongly connected component in the foreign-key
/// graph, by the component's least member: tables that reach each other
/// through foreign keys share one. Self-references do not count.
fn foreign_key_components(schema: &Schema) -> BTreeMap<TableName, TableName> {
    let edges = |t: &TableName| -> Vec<TableName> {
        schema.tables.get(t).map_or_else(Vec::new, |table| {
            table
                .foreign_keys
                .values()
                .map(|fk| fk.references_table.clone())
                .filter(|to| to != t && schema.tables.contains_key(to))
                .collect()
        })
    };
    // Which tables each table reaches; two share a component where each
    // reaches the other. Quadratic, over a schema's tables, once a plan.
    let mut reach: BTreeMap<TableName, BTreeSet<TableName>> = BTreeMap::new();
    for t in schema.tables.keys() {
        let mut seen = BTreeSet::new();
        let mut stack = edges(t);
        while let Some(next) = stack.pop() {
            if seen.insert(next.clone()) {
                stack.extend(edges(&next));
            }
        }
        reach.insert(t.clone(), seen);
    }
    schema
        .tables
        .keys()
        .map(|t| {
            let least = schema
                .tables
                .keys()
                .find(|u| *u == t || (reach[t].contains(*u) && reach[*u].contains(t)))
                .unwrap_or(t);
            (t.clone(), least.clone())
        })
        .collect()
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
            if let Change::SetTablePersistence { table, .. } = c {
                Some(table.clone())
            } else {
                None
            }
        })
        .collect();
    if switching.len() < 2 {
        return;
    }
    let components = foreign_key_components(declared);
    let mut unlinked = Vec::new();
    for (name, table) in &declared.tables {
        if !switching.contains(name) {
            continue;
        }
        for (key, fk) in &table.foreign_keys {
            let to = &fk.references_table;
            let in_cycle = to != name
                && switching.contains(to)
                && components.contains_key(name)
                && components.get(name) == components.get(to);
            let added = changes.iter().any(|c| {
                matches!(c, Change::AddForeignKey { table: t, name: n, .. } if t == name && n == key)
            });
            if in_cycle && !added {
                unlinked.push(Change::DropForeignKey {
                    table: name.clone(),
                    name: key.clone(),
                });
                unlinked.push(Change::AddForeignKey {
                    table: name.clone(),
                    name: key.clone(),
                    constraint: Box::new(fk.clone()),
                });
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
        changes.push(Change::SetStorageParameters {
            uid: uid.clone(),
            table: name.clone(),
            set,
            reset,
        });
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
                Change::SetPrimaryKey {
                    table: t,
                    to: Some(_),
                    ..
                },
                Some(ReplicaIdentity::PrimaryKey),
            ) => t == declared_name,
            (Change::AddUnique { table: t, name, .. }, Some(ReplicaIdentity::Unique(n)))
            | (Change::AddIndex { table: t, name, .. }, Some(ReplicaIdentity::Index(n))) => {
                t == declared_name && name == n
            }
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
                Change::SetPrimaryKey {
                    table: t,
                    from: Some(_),
                    ..
                },
                Some(ReplicaIdentity::PrimaryKey),
            ) => on(t),
            (Change::DropUnique { table: t, name }, Some(ReplicaIdentity::Unique(n)))
            | (Change::DropIndex { table: t, name }, Some(ReplicaIdentity::Index(n))) => {
                on(t) && name == n
            }
            _ => false,
        });
        if now {
            early.insert((uid.clone(), table.replica_identity.clone()));
        } else if old_index_dropped {
            early.insert((uid.clone(), Some(ReplicaIdentity::Full)));
            changes.push(Change::SetReplicaIdentity {
                uid: uid.clone(),
                table: base_name.clone(),
                to: Some(ReplicaIdentity::Full),
            });
        }
        changes.push(Change::SetReplicaIdentity {
            uid: uid.clone(),
            table: if now { base_name } else { declared_name }.clone(),
            to: table.replica_identity.clone(),
        });
    }
    early
}

fn diff_constraints(name: &TableName, base: &Table, declared: &Table, changes: &mut Vec<Change>) {
    // Storage parameters change in place, every supported one of both
    // methods (measured on 16 and 18), so a part is rebuilt only where it
    // differs otherwise; a difference in them alone is one `ALTER INDEX`
    // (DEC-1442.1). A rebuilt part carries its declared ones in its `CREATE`.
    fn bare<T: Clone>(part: &T, strip: impl Fn(&mut T)) -> T {
        let mut part = part.clone();
        strip(&mut part);
        part
    }
    let pk_bare = |pk: &PrimaryKey| bare(pk, |p| p.storage_parameters.clear());
    let unique_bare = |u: &UniqueConstraint| bare(u, |u| u.storage_parameters.clear());
    let index_bare = |ix: &Index| bare(ix, |ix| ix.storage_parameters.clear());
    let parameters = |target: IndexPart,
                      method: IndexMethod,
                      was: &BTreeMap<String, String>,
                      now: &BTreeMap<String, String>,
                      changes: &mut Vec<Change>| {
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
        if !set.is_empty() || !reset.is_empty() {
            changes.push(Change::SetIndexStorageParameters {
                table: name.clone(),
                target,
                method,
                set,
                reset,
            });
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
    let pk_differs = match (&base.primary_key, &declared.primary_key) {
        (Some(b), Some(d)) if d.name.is_none() => b.columns != d.columns,
        (Some(b), Some(d)) => pk_bare(b) != pk_bare(d),
        (b, d) => b != d,
    } || (base.primary_key.is_some()
        && declared.primary_key.is_some()
        && base.primary_key_is_clustered() != declared.primary_key_is_clustered());
    if !pk_differs && let (Some(b), Some(d)) = (&base.primary_key, &declared.primary_key) {
        parameters(
            IndexPart::PrimaryKey,
            IndexMethod::Btree,
            &b.storage_parameters,
            &d.storage_parameters,
            changes,
        );
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
        changes.push(Change::SetPrimaryKey {
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
        });
        if split {
            changes.push(Change::SetPrimaryKey {
                table: name.clone(),
                from: None,
                to: declared.primary_key.clone(),
                nonclustered: !declared.primary_key_is_clustered(),
            });
        }
    }

    macro_rules! by_name {
        ($field:ident, $add:ident, $drop:ident, $wrap:expr) => {
            for (n, c) in &declared.$field {
                if base.$field.get(n) != Some(c) {
                    if base.$field.contains_key(n) {
                        changes.push(Change::$drop {
                            table: name.clone(),
                            name: n.clone(),
                        });
                    }
                    changes.push(Change::$add {
                        table: name.clone(),
                        name: n.clone(),
                        constraint: $wrap(c.clone()),
                    });
                }
            }
            for n in base
                .$field
                .keys()
                .filter(|n| !declared.$field.contains_key(*n))
            {
                changes.push(Change::$drop {
                    table: name.clone(),
                    name: n.clone(),
                });
            }
        };
    }

    by_name!(foreign_keys, AddForeignKey, DropForeignKey, Box::new);
    by_name!(checks, AddCheck, DropCheck, std::convert::identity);

    // Out of the macro because these two carry a layout: a constraint or an
    // index whose definition is unchanged but which becomes, or stops being,
    // the clustered one is rebuilt, for the reason a key is above.
    for (n, c) in &declared.unique {
        let clustered = declared.unique_is_clustered(n);
        if let Some(b) = base.unique.get(n)
            && unique_bare(b) == unique_bare(c)
            && base.unique_is_clustered(n) == clustered
        {
            parameters(
                IndexPart::Unique(n.clone()),
                IndexMethod::Btree,
                &b.storage_parameters,
                &c.storage_parameters,
                changes,
            );
            continue;
        }
        if base.unique.get(n) != Some(c) || base.unique_is_clustered(n) != clustered {
            if base.unique.contains_key(n) {
                changes.push(Change::DropUnique {
                    table: name.clone(),
                    name: n.clone(),
                });
            }
            changes.push(Change::AddUnique {
                table: name.clone(),
                name: n.clone(),
                constraint: c.clone(),
                clustered,
            });
        }
    }
    for n in base
        .unique
        .keys()
        .filter(|n| !declared.unique.contains_key(*n))
    {
        changes.push(Change::DropUnique {
            table: name.clone(),
            name: n.clone(),
        });
    }

    for (n, ix) in &declared.indexes {
        let clustered = declared.index_is_clustered(n);
        if let Some(b) = base.indexes.get(n)
            && index_bare(b) == index_bare(ix)
            && base.index_is_clustered(n) == clustered
        {
            parameters(
                IndexPart::Index(n.clone()),
                ix.method,
                &b.storage_parameters,
                &ix.storage_parameters,
                changes,
            );
            continue;
        }
        if base.indexes.get(n) != Some(ix) || base.index_is_clustered(n) != clustered {
            if base.indexes.contains_key(n) {
                changes.push(Change::DropIndex {
                    table: name.clone(),
                    name: n.clone(),
                });
            }
            changes.push(Change::AddIndex {
                table: name.clone(),
                name: n.clone(),
                index: Box::new(ix.clone()),
                clustered,
            });
        }
    }
    for n in base
        .indexes
        .keys()
        .filter(|n| !declared.indexes.contains_key(*n))
    {
        changes.push(Change::DropIndex {
            table: name.clone(),
            name: n.clone(),
        });
    }
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
    base_name_of: &BTreeMap<String, String>,
    changes: &mut Vec<Change>,
    errs: &mut Vec<DiffError>,
) {
    if base.data.as_ref().map(|d| d.mode) != declared.data.as_ref().map(|d| d.mode) {
        changes.push(Change::SetDataMode {
            table: name.clone(),
            from: base.data.as_ref().map(|d| d.mode),
            to: declared.data.as_ref().map(|d| d.mode),
        });
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
        .is_some_and(|c| c.identity.is_some());

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
                    Change::SetPrimaryKey { table, from: None, to: Some(pk), .. }
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
                (base_name_of.get(&key_column).map(String::as_str) != Some(base_key)).then(|| {
                    DiffError::DataKeyColumnChanged {
                        table: name.clone(),
                    }
                })
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
                let (defaults, types) = omitted_defaults(declared, &key_column, row);
                changes.push(Change::InsertRow {
                    table: name.clone(),
                    key_column: key_column.clone(),
                    identity_key,
                    key: key.clone(),
                    row: row.clone(),
                    defaults,
                    types,
                })
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
                    if spec.engine_assigned() {
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
                            cell(before, base_column, base.columns.get(base_column))
                        }
                        None => Cell::Value(Value::Null),
                    };
                    let d = cell(row, column, Some(spec));
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
                    changes.push(Change::UpdateRow {
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
                    });
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
        let surviving: BTreeSet<&str> = base_name_of.values().map(String::as_str).collect();
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
                    if spec.engine_assigned() {
                        continue;
                    }
                    row.insert(
                        declared_column.clone(),
                        cell(before, base_column, Some(spec)),
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
                    if spec.engine_assigned() || surviving.contains(base_column.as_str()) {
                        continue;
                    }
                    dropped.insert(base_column.clone(), cell(before, base_column, Some(spec)));
                }
                changes.push(Change::DeleteRow {
                    table: name.clone(),
                    key_column: key_column.clone(),
                    key: key.clone(),
                    cause: DeleteCause::Undeclared,
                    row,
                    dropped,
                    types,
                    after_types,
                    key_type: declared.columns.get(&key_column).map(|c| c.ty.clone()),
                });
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
fn member_depth(planned: &[PlannedChange]) -> BTreeMap<String, usize> {
    let dropped: BTreeMap<&str, &[String]> = planned
        .iter()
        .filter_map(|p| match &p.change {
            Change::DropRole { name, members, .. } => Some((name.as_str(), members.as_slice())),
            _ => None,
        })
        .collect();
    let mut depth: BTreeMap<String, usize> = dropped.keys().map(|n| ((*n).to_owned(), 0)).collect();
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

/// Re-orders the role drops of a change set parent before member, once their
/// members are known.
///
/// The differ ranks them by [`member_depth`] when it sorts, but a `DropRole`
/// leaves the differ with no members — `plan --db` reads them from the
/// environment afterwards and writes them into the plan — so the rank the
/// differ used was every role at depth zero, and the name tiebreaker decided.
/// This is the same ranking applied again, over the positions the drops
/// already hold, after the members are in: nothing else moves, and a plan
/// whose dropped roles hold none of each other keeps the order it had
/// (DECISIONS 127, 139).
pub fn order_role_drops(cs: &mut ChangeSet) {
    let depth = member_depth(&cs.changes);
    let slots: Vec<usize> = cs
        .changes
        .iter()
        .enumerate()
        .filter(|(_, p)| matches!(p.change, Change::DropRole { .. }))
        .map(|(i, _)| i)
        .collect();
    let mut drops: Vec<PlannedChange> = slots.iter().rev().map(|&i| cs.changes.remove(i)).collect();
    drops.reverse();
    // Stable: two drops at one depth keep the order the differ gave them.
    drops.sort_by_key(|p| {
        let Change::DropRole { name, .. } = &p.change else {
            return 0;
        };
        depth.get(name).copied().unwrap_or(0)
    });
    for (slot, drop) in slots.into_iter().zip(drops) {
        cs.changes.insert(slot, drop);
    }
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
    role_rank: &BTreeMap<String, usize>,
) -> isize {
    match change {
        Change::CreateModule { id, .. } | Change::AlterModule { id, .. } => {
            create_rank.get(id).map_or(0, |r| *r as isize)
        }
        Change::DropModule { id, .. } => -(drop_rank.get(id).map_or(0, |r| *r as isize)),
        Change::CreateTable { .. }
        | Change::DropTable { .. }
        | Change::DetachPartition { .. }
        | Change::AttachPartition { .. }
        | Change::RenameTable { .. }
        | Change::AddColumn { .. }
        | Change::DropColumn { .. }
        | Change::RenameColumn { .. }
        | Change::SetColumnDeprecated { .. }
        | Change::SetIndexStorageParameters { .. }
        | Change::SetTablePersistence { .. }
        | Change::SetStorageParameters { .. }
        | Change::DropUnique { .. }
        | Change::AddCheck { .. }
        | Change::DropCheck { .. }
        | Change::DropIndex { .. }
        | Change::AddComputedColumn { .. }
        | Change::DropComputedColumn { .. }
        | Change::SetDataMode { .. } => 0,
        // The clustered index goes in ahead of the rest of the addition
        // class. Building it rebuilds every nonclustered index already on the
        // table, whose row locators become its key, so an index added before
        // it is built twice (#1178). Correctness needs nothing here: the old
        // clustered index is gone by now, since every drop is in class 2.
        //
        // A key with no layout to speak of — every PostgreSQL key, whose
        // `nonclustered` is always false — moves ahead of its siblings too,
        // which orders nothing that depends on it.
        Change::SetPrimaryKey {
            to: Some(_),
            nonclustered: false,
            ..
        }
        | Change::AddUnique {
            clustered: true, ..
        }
        | Change::AddIndex {
            clustered: true, ..
        } => -1,
        Change::SetPrimaryKey { .. } | Change::AddUnique { .. } | Change::AddIndex { .. } => 0,
        // After every addition of its class, the index it names among them,
        // and the foreign keys too: nothing in the class reads it.
        Change::SetReplicaIdentity { .. } => 2,
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
        Change::AddForeignKey { .. } => 1,
        Change::DropForeignKey { .. } => -1,
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
        Change::AlterColumnType { .. } => -1,
        // And the old default goes before the type, which makes the three
        // phases a column can need one rank each: drop the default the old
        // type gave meaning to, change the type, install the default written
        // for the new one. Measured, SQL Server refuses the middle statement
        // while the first one's constraint stands (5074 with 4922 behind it),
        // and `diff_columns` splits a *replaced* default into these two halves
        // when the type moves, so both ranks have something to order.
        Change::AlterColumnDefault { to: None, .. } => -2,
        Change::AlterColumnDefault { to: Some(_), .. } => 0,
        // After any type change, as a new default is: the expression is
        // recomputed under the column's final type (DEC-1168.1).
        Change::AlterColumnExpression { .. } => 0,
        // A recomputation is checked against the column's nullability as it
        // stands, so a relaxation goes before one and a tightening after it.
        // Measured on 17.11, `SET EXPRESSION` that yields a NULL is refused
        // under `NOT NULL` ("contains null values") and accepted once
        // `DROP NOT NULL` has run. Nothing else in the class reads nullability.
        Change::AlterColumnNullability {
            to_nullable: true, ..
        } => -1,
        Change::AlterColumnNullability {
            to_nullable: false, ..
        } => 1,
        // A partition's own, alike: its column's type is its parent's, which
        // no change here moves (#1581).
        Change::SetPartitionDefault { .. } => 0,
        Change::SetPartitionNotNull {
            not_null: false, ..
        } => -1,
        Change::SetPartitionNotNull { not_null: true, .. } => 1,
        // Rows follow the foreign keys between their tables: a referenced
        // table's rows go in first, and out last.
        Change::InsertRow { table, .. } | Change::UpdateRow { table, .. } => {
            data_rank.get(table).map_or(0, |r| *r as isize)
        }
        Change::DeleteRow { table, .. } => -(data_rank.get(table).map_or(0, |r| *r as isize)),
        // Two roles dropped together are ordered by membership: see
        // [`member_depth`].
        Change::DropRole { name, .. } => role_rank.get(name).map_or(0, |r| *r as isize),
        // The rest depend on nothing among themselves; a grant's target is
        // ordered by the class of the change, not by rank.
        Change::CreateRole { .. }
        | Change::RenameRole { .. }
        | Change::Grant { .. }
        | Change::Revoke { .. } => 0,
        // The rank of the routine it settles, so it sorts beside that
        // routine's `CREATE` in class 14 (see `order_key`, #687).
        Change::PublicExecution { routine, .. } => create_rank
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
    dialect: &dyn Dialect,
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
            if let Change::AlterModule { id, .. } = c {
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
    let dropped_with_its_table = |target: &GrantTarget| -> bool {
        matches!(target, GrantTarget::Object(o)
            if base_uid.get(o).is_some_and(|uid| !declared.ids.tables.contains_key(*uid)))
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
            GrantTarget::Routine(_) | GrantTarget::Schema(_) => target.clone(),
        }
    };

    for (uid, name) in &base.ids.roles {
        if declared.ids.roles.contains_key(uid) {
            continue;
        }
        if manages_roles {
            changes.push(Change::DropRole {
                uid: uid.clone(),
                name: name.clone(),
                // Only a connected plan can know them; see `Change::DropRole`.
                members: Vec::new(),
            });
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
        for (target, permissions) in &held.grants {
            if dropped_with_its_table(target) {
                continue;
            }
            let target = forward(target);
            // A `REVOKE` on an object this same plan drops would fail on an
            // object that is gone — the same rule the per-target comparison
            // below applies, and for the same reason.
            let target_dropped = dropped.iter().any(|d| d.takes(&target))
                && !matches!(&target, GrantTarget::Object(o) if changing_hands.contains(o));
            if permissions.is_empty() || target_dropped {
                continue;
            }
            changes.push(Change::Revoke {
                role: name.clone(),
                target,
                permissions: permissions.clone(),
            });
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
                changes.push(Change::CreateRole {
                    uid: uid.clone(),
                    name: name.clone(),
                });
            }
            for (target, permissions) in &role.grants {
                if !permissions.is_empty() {
                    changes.push(Change::Grant {
                        role: name.clone(),
                        target: target.clone(),
                        permissions: permissions.clone(),
                    });
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
            changes.push(Change::RenameRole {
                uid: uid.clone(),
                from: base_name.clone(),
                to: name.clone(),
            });
        }
        // The base grants, keyed by the name the target has after this plan.
        let mut before: BTreeMap<GrantTarget, BTreeSet<Permission>> = BTreeMap::new();
        if let Some(b) = base.schema.roles.get(base_name) {
            for (target, permissions) in &b.grants {
                if dropped_with_its_table(target) {
                    continue;
                }
                before
                    .entry(forward(target))
                    .or_default()
                    .extend(permissions.iter().copied());
            }
        }
        let targets: BTreeSet<&GrantTarget> = before.keys().chain(role.grants.keys()).collect();
        for target in targets {
            // By whole identity: where routines overload, the drop of one
            // takes its own grants and leaves the sibling's to be compared.
            let target_dropped = dropped.iter().any(|d| d.takes(target))
                && !matches!(target, GrantTarget::Object(o) if changing_hands.contains(o));
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
            let d = role.grants.get(target).cloned().unwrap_or_default();
            let added: BTreeSet<Permission> = d.difference(&b).copied().collect();
            let removed: BTreeSet<Permission> = b.difference(&d).copied().collect();
            if !added.is_empty() {
                changes.push(Change::Grant {
                    role: name.clone(),
                    target: target.clone(),
                    permissions: added,
                });
            }
            if !removed.is_empty() && !target_dropped {
                changes.push(Change::Revoke {
                    role: name.clone(),
                    target: target.clone(),
                    permissions: removed,
                });
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
    dialect: &dyn Dialect,
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
        let id = if let Change::CreateModule { id, .. } = change {
            id
        } else if let Change::AlterModule { id, .. } = change
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
        decided.push(Change::PublicExecution {
            routine: routine.clone(),
            access,
            origin,
        });
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

fn diff_modules(
    base: &Schema,
    declared: &Schema,
    changing_hands: &BTreeSet<TableName>,
    dialect: &dyn Dialect,
    changes: &mut Vec<Change>,
) {
    for (id, module) in &declared.modules {
        match base.modules.get(id) {
            None => changes.push(Change::CreateModule {
                id: id.clone(),
                module: Box::new(module.clone()),
            }),
            // A trigger on a name that passes to another table: one id for
            // two triggers, the doomed or departed table's and the one this
            // plan declares on the new occupant. The first goes before the
            // table changes and the second is created after them, exactly as
            // for a trigger that changes kind (DEC-1118.1).
            Some(before)
                if matches!(id, ModuleId::Trigger { on, .. }
                    if changing_hands.contains(&TableName::new(on.schema.clone(), on.name.clone()))) =>
            {
                changes.push(Change::DropModule {
                    id: id.clone(),
                    kind: before.kind,
                });
                changes.push(Change::CreateModule {
                    id: id.clone(),
                    module: Box::new(module.clone()),
                });
            }
            // A trigger moved to another table needs no case of its own any
            // more: its table is part of its identity, so the move is a key
            // that is gone and a key that is new, and the two loops here
            // already spell that as a drop and a create (ADR-0009 §1). What is
            // left is the kind, which the key does not hold: `CREATE OR ALTER`
            // cannot turn a view into a procedure.
            Some(before) if before.kind != module.kind => {
                changes.push(Change::DropModule {
                    id: id.clone(),
                    kind: before.kind,
                });
                changes.push(Change::CreateModule {
                    id: id.clone(),
                    module: Box::new(module.clone()),
                });
            }
            Some(before) => {
                // The definition is compared after the dialect's lightweight
                // normalization, never by understanding it (SPEC §8.2). What
                // survives that is re-stated in full, which is idempotent and
                // keeps the permissions granted on the object.
                if dialect.normalize_definition(&before.definition)
                    != dialect.normalize_definition(&module.definition)
                {
                    changes.push(Change::AlterModule {
                        id: id.clone(),
                        module: Box::new(module.clone()),
                    });
                }
            }
        }
    }

    for (id, module) in &base.modules {
        if !declared.modules.contains_key(id) {
            changes.push(Change::DropModule {
                id: id.clone(),
                kind: module.kind,
            });
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

fn order_key(c: &Change) -> u8 {
    match c {
        // Modules go first and last, and both ends are load-bearing. A
        // SCHEMABINDING view blocks a rename of the column it binds, so every
        // module that is going has to go before the table changes; and a view
        // can only be created once the columns it selects exist.
        Change::DropModule { .. } => 0,
        // A role drop needs nothing else gone first, and a plan that also
        // recreates the name wants the old one out of the way early.
        Change::DropRole { .. } => 0,
        // A table rename, and a role rename that depends on nothing here.
        Change::RenameTable { .. } | Change::RenameRole { .. } => 1,
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
        Change::DropIndex { .. }
        | Change::DropUnique { .. }
        | Change::DropForeignKey { .. }
        | Change::DropCheck { .. }
        | Change::SetPrimaryKey { to: None, .. } => 2,
        // After the drops above (`sort_class` places it at the class's end),
        // since an index or check over it blocks it (4922), and before every
        // rename, drop or retype of a column its expression reads, which it
        // blocks in turn (15336, 4922; measured on 17.0, #1174).
        Change::DropComputedColumn { .. } => 2,
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
        Change::RenameColumn { .. } => 3,
        // After the renames, so a revoke names the role and the object as
        // they now are. It shared a class with the drops above until they
        // moved ahead of the renames; it has the opposite need, so it stayed
        // and took a class of its own. A revoke on an object this plan drops
        // is never emitted (see `diff_roles`).
        Change::Revoke { .. } => 4,
        Change::DropColumn { .. } => 5,
        // A detach frees its range, as a drop does, before a partition is
        // created over it in class 7 (#1544).
        Change::DropTable { .. } | Change::DetachPartition { .. } => 6,
        // An attach takes its range as a created partition does, and once
        // a detach or a drop has freed it (#1545).
        Change::CreateTable { .. } | Change::AttachPartition { .. } => 7,
        Change::AddColumn { .. } => 8,
        Change::AlterColumnType { .. }
        | Change::AlterColumnNullability { .. }
        | Change::AlterColumnDefault { .. }
        | Change::AlterColumnExpression { .. }
        | Change::SetPartitionDefault { .. }
        | Change::SetPartitionNotNull { .. } => COLUMN_ALTERATIONS,
        // Once every column its expression reads is there in its final type
        // (`sort_class` places it after the class's alterations), and before
        // the indexes and checks of class 13 that may be over it (#1174).
        Change::AddComputedColumn { .. } => COLUMN_ALTERATIONS,
        Change::SetColumnDeprecated { .. } => 10,
        // Metadata too: no statement reads a storage parameter, and it runs
        // under the table's final name (DEC-1441.1).
        Change::SetStorageParameters { .. } => 10,
        // After the foreign-key drops of class 2 and before the adds of 13:
        // the engine checks the keys that stand when it switches (#1443).
        Change::SetTablePersistence { .. } => COLUMN_ALTERATIONS,
        // In place too, and nothing reads one (DEC-1442.1).
        Change::SetIndexStorageParameters { .. } => 10,
        // Rows arrive once every column they name exists and has its final
        // type, and before the constraints below: ADR-0004's "create table ->
        // insert rows -> add the foreign key that references them".
        Change::InsertRow { .. } | Change::UpdateRow { .. } => 11,
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
        Change::DeleteRow { .. } => ROW_DELETIONS,
        Change::SetPrimaryKey { .. }
        | Change::AddUnique { .. }
        | Change::AddForeignKey { .. }
        | Change::AddCheck { .. }
        | Change::AddIndex { .. } => 13,
        // Late, it follows the index it names (DEC-1444.1); `sort_class`
        // moves the rest to the front.
        Change::SetReplicaIdentity { .. } => 13,
        Change::CreateModule { .. } | Change::AlterModule { .. } => 14,
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
        Change::PublicExecution { .. } => 14,
        // A grant names an object, so it comes after every object exists —
        // and after the role does.
        Change::CreateRole { .. } => 15,
        Change::Grant { .. } => 16,
        // Emits nothing; it exists so the recorded state matches the file. Last
        // keeps it out of the way of everything that does emit.
        Change::SetDataMode { .. } => 17,
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
) -> (BTreeMap<String, String>, BTreeMap<String, ColumnType>) {
    let types = table
        .row_columns(key_column)
        .map(|(c, spec)| (c.clone(), spec.ty.clone()))
        .collect();
    let defaults = omitted_columns(table, key_column, row)
        .filter_map(|(c, spec)| spec.default.clone().map(|d| (c.clone(), d)))
        .collect();
    (defaults, types)
}

/// The columns an insert leaves to the table: not the key, not spelled by
/// the row, and not an `IDENTITY` column, which is the engine's own.
fn omitted_columns<'a>(
    table: &'a Table,
    key_column: &'a str,
    row: &'a pbps_model::Row,
) -> impl Iterator<Item = (&'a String, &'a pbps_model::Column)> {
    table
        .row_columns(key_column)
        .filter(move |(c, _)| !row.0.contains_key(*c))
}

#[cfg(test)]
#[allow(clippy::wildcard_enum_match_arm)]
mod tests {
    use super::omitted_defaults;

    /// The key is never one of them, an identity column is the engine's,
    /// and a column the row spells needs no default; every other defaulted
    /// column the row omits travels with the insert (DECISIONS 117).
    #[test]
    fn an_inserted_row_carries_the_defaults_of_the_columns_it_omits() {
        use pbps_model::{Column, ColumnType, Row, Table, Value};
        use std::str::FromStr;
        let mut t = Table::default();
        let mut col = |name: &str, ty: &str, default: Option<&str>, identity: bool| {
            let mut c = Column::new(ColumnType::from_str(ty).unwrap());
            c.default = default.map(str::to_owned);
            if identity {
                c.identity = Some(pbps_model::Identity {
                    seed: 1,
                    increment: 1,
                });
            }
            t.columns.insert(name.to_owned(), c);
        };
        col("id", "int", Some("(1)"), false);
        col("status_code", "varchar(10)", Some("('old')"), false);
        col("label", "nvarchar(50)", Some("N'x'"), false);
        col("rank", "int", None, false);
        col("seq", "int", Some("(0)"), true);
        let mut row = Row::default();
        row.0.insert("label".into(), Value::Text("spelled".into()));
        let (defaults, types) = omitted_defaults(&t, "id", &row);
        assert_eq!(
            defaults.into_iter().collect::<Vec<_>>(),
            [("status_code".to_owned(), "('old')".to_owned())]
        );
        // And the type of every non-key column, so the emitter can hold the
        // row to the default it left a column at (DECISIONS 133), to NULL
        // where the table gives none (136), and to a spelled cell by the
        // rendering that reads it back (137). Not the key, not the identity
        // column.
        assert_eq!(
            types.keys().collect::<Vec<_>>(),
            [
                &"label".to_owned(),
                &"rank".to_owned(),
                &"status_code".to_owned()
            ]
        );
    }
    /// The invariant the SQL Server preflight reads, stated as a property
    /// rather than as a list.
    ///
    /// For a non-key column, **absent from `types` means `IDENTITY`**, and
    /// `pbps_mssql::preflight` uses exactly that to tell a column an insert
    /// leaves at NULL from one the engine assigns. Projecting NULL for an
    /// identity column made two inserted rows identical there, so a unique
    /// constraint over it reported a duplicate the engine would never produce
    /// and a valid plan was refused.
    ///
    /// So narrowing `types` for some other reason has to fail here. The test
    /// above pins the list this table produces; this one pins what the list
    /// *means*, which is the part a reader updating the list would otherwise
    /// step over.
    #[test]
    fn an_inserts_types_name_every_non_key_column_except_the_identity_ones() {
        use pbps_model::{Column, ColumnType, Row, Table, Value};
        use std::collections::BTreeSet;
        use std::str::FromStr;
        let mut t = Table::default();
        let mut col = |name: &str, ty: &str, identity: bool| {
            let mut c = Column::new(ColumnType::from_str(ty).unwrap());
            if identity {
                c.identity = Some(pbps_model::Identity {
                    seed: 1,
                    increment: 1,
                });
            }
            t.columns.insert(name.to_owned(), c);
        };
        col("code", "varchar(10)", false);
        col("label", "nvarchar(50)", false);
        col("seq", "int", true);
        let mut row = Row::default();
        row.0.insert("label".into(), Value::Text("spelled".into()));

        let (_, types) = omitted_defaults(&t, "code", &row);
        let expected: BTreeSet<String> = t
            .columns
            .iter()
            .filter(|(c, spec)| c.as_str() != "code" && spec.identity.is_none())
            .map(|(c, _)| c.clone())
            .collect();
        assert_eq!(types.keys().cloned().collect::<BTreeSet<_>>(), expected);

        // The negative half, which is the one the preflight actually reads: a
        // spelled column is present, and the identity column is the absence.
        assert!(types.contains_key("label"));
        assert!(!types.contains_key("seq"));
        assert!(!types.contains_key("code"));
    }

    use super::*;
    use crate::identity::Context;
    use indexmap::IndexMap;
    use pbps_dialect::MinimalDialect;
    use pbps_model::{
        CheckConstraint, Column, ColumnType, ForeignKey, IdsFile, Index, IndexColumn, Intent,
        PrimaryKey, ReferentialAction, RiskClass, Row, Uid, UniqueConstraint,
    };

    fn ctx() -> Context {
        Context {
            operator: "leon".into(),
            today: "2026-08-30".into(),
        }
    }

    fn ty(s: &str) -> ColumnType {
        s.parse().unwrap()
    }

    fn table(cols: &[(&str, Column)]) -> Table {
        let mut columns = IndexMap::new();
        for (n, c) in cols {
            columns.insert((*n).to_string(), c.clone());
        }
        Table {
            columns,
            ..Default::default()
        }
    }

    fn schema_of(name: &str, t: Table) -> Schema {
        let mut s = Schema::default();
        s.tables.insert(name.parse().unwrap(), t);
        s
    }

    /// Reproduces the real flow: the base side's identity file is the one from
    /// that version, and the declared side's is the resolved result.
    fn run(base: &Schema, declared: &Schema, intents: &[Intent]) -> ChangeSet {
        run_with(&MinimalDialect, base, declared, intents)
    }

    /// [`run`], against a caller-chosen dialect — for the one shape whose
    /// ordering answer differs by dialect: whether an index shares the
    /// schema's relation namespace with tables at all
    /// (`Dialect::indexes_share_namespace_with_tables`, issue #176).
    fn run_with(
        dialect: &dyn Dialect,
        base: &Schema,
        declared: &Schema,
        intents: &[Intent],
    ) -> ChangeSet {
        let base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let declared_ids = crate::resolve(declared, &base_ids, intents, &ctx())
            .unwrap()
            .ids;
        diff(
            Side {
                schema: base,
                ids: &base_ids,
            },
            Side {
                schema: declared,
                ids: &declared_ids,
            },
            dialect,
            &Hints::default(),
        )
        .unwrap()
    }

    /// `MinimalDialect`, except that `int` and `integer` are one type, as
    /// they are to PostgreSQL.
    struct Aliases;

    impl Dialect for Aliases {
        fn name(&self) -> &'static str {
            "aliases"
        }
        fn quote_ident(&self, ident: &str) -> Result<String, pbps_dialect::DialectError> {
            MinimalDialect.quote_ident(ident)
        }
        fn emit(
            &self,
            change: &Change,
            strategy: pbps_model::Strategy,
        ) -> Result<Vec<pbps_dialect::Statement>, pbps_dialect::DialectError> {
            MinimalDialect.emit(change, strategy)
        }
        fn normalize_type(
            &self,
            ty: &pbps_model::ColumnType,
        ) -> Result<pbps_model::ColumnType, pbps_dialect::DialectError> {
            let mut ty = ty.clone();
            if ty.base == "integer" {
                ty.base = "int".into();
            }
            Ok(ty)
        }
        fn type_change_risk(
            &self,
            from: &pbps_model::ColumnType,
            to: &pbps_model::ColumnType,
        ) -> pbps_dialect::TypeChangeRisk {
            MinimalDialect.type_change_risk(from, to)
        }
        fn fold_ident<'a>(&self, ident: &'a str) -> std::borrow::Cow<'a, str> {
            MinimalDialect.fold_ident(ident)
        }
        fn lexicon(&self) -> pbps_dialect::Lexicon {
            MinimalDialect.lexicon()
        }
        fn validate_table(
            &self,
            name: &pbps_model::TableName,
            table: &Table,
        ) -> Vec<pbps_dialect::DialectError> {
            MinimalDialect.validate_table(name, table)
        }
        fn transaction_framing(&self) -> pbps_dialect::TransactionFraming {
            MinimalDialect.transaction_framing()
        }
        fn probe_framing(&self) -> Option<pbps_dialect::TransactionFraming> {
            MinimalDialect.probe_framing()
        }
    }

    /// A dialect whose collation changes take every dependent down, and whose
    /// nullability changes take down the filtered indexes, and on tightening
    /// the indexes and unique constraints over the column, as SQL Server's do
    /// (#1175, #1363). Everything else is `MinimalDialect`'s, so a type change
    /// alone rebuilds nothing.
    struct Recollates;

    impl Dialect for Recollates {
        fn name(&self) -> &'static str {
            "recollates"
        }
        fn recollate_dependents(&self) -> pbps_dialect::RetypeDependents {
            pbps_dialect::RetypeDependents {
                keys_and_indexes: true,
                checks: true,
                filtered_indexes: true,
                foreign_keys: true,
            }
        }
        fn nullability_dependents(&self, to_nullable: bool) -> pbps_dialect::RetypeDependents {
            pbps_dialect::RetypeDependents {
                keys_and_indexes: !to_nullable,
                checks: false,
                filtered_indexes: true,
                foreign_keys: false,
            }
        }
        fn quote_ident(&self, ident: &str) -> Result<String, pbps_dialect::DialectError> {
            MinimalDialect.quote_ident(ident)
        }
        fn emit(
            &self,
            change: &Change,
            strategy: pbps_model::Strategy,
        ) -> Result<Vec<pbps_dialect::Statement>, pbps_dialect::DialectError> {
            MinimalDialect.emit(change, strategy)
        }
        fn normalize_type(
            &self,
            ty: &pbps_model::ColumnType,
        ) -> Result<pbps_model::ColumnType, pbps_dialect::DialectError> {
            MinimalDialect.normalize_type(ty)
        }
        fn type_change_risk(
            &self,
            from: &pbps_model::ColumnType,
            to: &pbps_model::ColumnType,
        ) -> pbps_dialect::TypeChangeRisk {
            MinimalDialect.type_change_risk(from, to)
        }
        fn fold_ident<'a>(&self, ident: &'a str) -> std::borrow::Cow<'a, str> {
            MinimalDialect.fold_ident(ident)
        }
        fn lexicon(&self) -> pbps_dialect::Lexicon {
            MinimalDialect.lexicon()
        }
        fn validate_table(
            &self,
            name: &pbps_model::TableName,
            table: &Table,
        ) -> Vec<pbps_dialect::DialectError> {
            MinimalDialect.validate_table(name, table)
        }
        fn transaction_framing(&self) -> pbps_dialect::TransactionFraming {
            MinimalDialect.transaction_framing()
        }
        fn probe_framing(&self) -> Option<pbps_dialect::TransactionFraming> {
            MinimalDialect.probe_framing()
        }
    }

    /// A computed column's expression or persistence change is a drop and an
    /// add, in their classes, with the index over it rebuilt around them; a
    /// read-back's `not_null` is held only where the declaration says it
    /// (#1174, DEC-1174.1).
    #[test]
    fn a_computed_column_change_is_a_drop_and_an_add_around_its_index() {
        let computed =
            |expression: &str, persisted: bool, not_null: bool| pbps_model::ComputedColumn {
                expression: expression.into(),
                persisted,
                not_null,
            };
        let with = |c: pbps_model::ComputedColumn| {
            let mut t = table(&[
                ("id", Column::new(ty("int")).not_null()),
                ("a", Column::new(ty("int"))),
            ]);
            t.computed.insert("c".into(), c);
            t.indexes.insert(
                "ix_c".into(),
                Index {
                    columns: vec![IndexColumn {
                        key: pbps_model::IndexKey::Column("c".into()),
                        descending: false,
                        opclass: None,
                    }],
                    include: Vec::new(),
                    unique: false,
                    filter: None,
                    method: Default::default(),
                    storage_parameters: Default::default(),
                },
            );
            schema_of("dbo.t", t)
        };
        let base = with(computed("a * 2", true, false));
        assert_eq!(
            kinds(&run(&base, &with(computed("a * 3", true, false)), &[])),
            [
                "DropIndex",
                "DropComputedColumn",
                "AddComputedColumn",
                "AddIndex"
            ]
        );
        assert_eq!(
            kinds(&run(&base, &with(computed("a * 2", false, false)), &[])),
            [
                "DropIndex",
                "DropComputedColumn",
                "AddComputedColumn",
                "AddIndex"
            ]
        );
        // The read-back says NOT NULL wherever a persisted column is not
        // nullable; a declaration that leaves it out is the same column.
        let read_not_null = with(computed("a * 2", true, true));
        assert!(run(&read_not_null, &base, &[]).changes.is_empty());
        // A declared one is held to it.
        assert_eq!(
            kinds(&run(&base, &read_not_null, &[])),
            [
                "DropIndex",
                "DropComputedColumn",
                "AddComputedColumn",
                "AddIndex"
            ]
        );
        // Negative: an unchanged one is no change.
        assert!(run(&base, &base, &[]).changes.is_empty());
        // An ordinary column `c` replaced by a computed one, and back, with
        // the index over `c` unchanged: it comes down and goes back around
        // the swap (#1174 review).
        let ordinary = {
            let mut t = table(&[
                ("id", Column::new(ty("int")).not_null()),
                ("a", Column::new(ty("int"))),
                ("c", Column::new(ty("int"))),
            ]);
            t.indexes = with(computed("a * 2", false, false)).tables
                [&"dbo.t".parse::<TableName>().unwrap()]
                .indexes
                .clone();
            schema_of("dbo.t", t)
        };
        let drop_c = [Intent::DropColumn {
            column: "dbo.t.c".parse().unwrap(),
            reason: "computed now".into(),
        }];
        assert_eq!(
            kinds(&run(
                &ordinary,
                &with(computed("a * 2", false, false)),
                &drop_c
            )),
            ["DropIndex", "DropColumn", "AddComputedColumn", "AddIndex"]
        );
        assert_eq!(
            kinds(&run(&with(computed("a * 2", false, false)), &ordinary, &[])),
            ["DropIndex", "DropComputedColumn", "AddColumn", "AddIndex"]
        );
    }

    /// A connected SQL Server plan leaves a standing computed column to the
    /// catalog's edges (`Screen::Catalog`, #1460): the text screen folds
    /// case, and refused a retype of `A2` beside a column reading `a2` in a
    /// case-sensitive database. A re-added one has no edge for its new text
    /// and is still screened, and an offline plan screens both.
    #[test]
    fn a_connected_plan_leaves_a_standing_computed_column_to_the_catalog() {
        let errors_of = |base: &Schema, want: &Schema, screen: Screen| {
            let base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
                .unwrap()
                .ids;
            let ids = crate::resolve(want, &base_ids, &[], &ctx()).unwrap().ids;
            rebuilding_by(
                Side {
                    schema: base,
                    ids: &base_ids,
                },
                Side {
                    schema: want,
                    ids: &ids,
                },
                &MinimalDialect,
                &Hints::default(),
                &BTreeSet::new(),
                Rebinding::Candidates,
                screen,
                None,
            )
            .err()
            .unwrap_or_default()
        };
        let shaped = |a: Column, expression: &str| {
            let mut t = table(&[("id", Column::new(ty("int")).not_null()), ("A2", a)]);
            t.computed.insert(
                "c".into(),
                pbps_model::ComputedColumn {
                    expression: expression.into(),
                    persisted: false,
                    not_null: false,
                },
            );
            schema_of("dbo.t", t)
        };
        let base = shaped(Column::new(ty("int")), "a2 * 2");
        let retyped = shaped(Column::new(ty("bigint")), "a2 * 2");
        assert!(errors_of(&base, &retyped, Screen::Catalog).is_empty());
        assert!(matches!(
            errors_of(&base, &retyped, Screen::Text).as_slice(),
            [DiffError::ComputedInputChanged { .. }]
        ));

        let calls = |expression: &str, definition: &str| {
            with_functions(
                shaped(Column::new(ty("int")), expression),
                &[("dbo.f", definition)],
            )
        };
        // Standing while the function it calls is altered.
        let before = calls("dbo.f(a2)", "one");
        let altered = calls("dbo.f(a2)", "two");
        assert!(errors_of(&before, &altered, Screen::Catalog).is_empty());
        assert!(errors_of(&before, &altered, Screen::Text).iter().any(
            |e| matches!(e, DiffError::ComputedFunctionChanged { change, .. }
                    if *change == "alters")
        ));
        // Re-added around the alter: still the screen's, connected or not.
        let readded = calls("dbo.f(a2) + 1", "two");
        for screen in [Screen::Catalog, Screen::Text] {
            assert!(
                errors_of(&before, &readded, screen).iter().any(
                    |e| matches!(e, DiffError::ComputedFunctionChanged { change, .. }
                        if *change == "alters")
                ),
                "{screen:?}"
            );
        }
    }

    /// A plan that renames, drops, retypes or changes the nullability of a
    /// column a standing computed column may read, or alters a module it may
    /// call, is refused by name; one that changes the computed column too is
    /// not, since it is out of the way first (#1174, DEC-1174.1).
    #[test]
    fn a_standing_computed_column_refuses_a_change_to_what_it_reads() {
        let errors_of = |base: &Schema, want: &Schema, intents: &[Intent]| {
            let base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
                .unwrap()
                .ids;
            let ids = crate::resolve(want, &base_ids, intents, &ctx())
                .unwrap()
                .ids;
            diff_partial(
                Side {
                    schema: base,
                    ids: &base_ids,
                },
                Side {
                    schema: want,
                    ids: &ids,
                },
                &MinimalDialect,
                &Hints::default(),
            )
            .errors
        };
        let shaped = |a: Column, expression: &str| {
            let mut t = table(&[("id", Column::new(ty("int")).not_null()), ("A2", a)]);
            t.computed.insert(
                "c".into(),
                pbps_model::ComputedColumn {
                    expression: expression.into(),
                    persisted: false,
                    not_null: false,
                },
            );
            schema_of("dbo.t", t)
        };
        // Named in another case: the engine reads it all the same.
        let base = shaped(Column::new(ty("int")), "a2 * 2");
        for (changed, what) in [
            (Column::new(ty("bigint")), "retypes"),
            (Column::new(ty("int")).not_null(), "nullability"),
        ] {
            let errors = errors_of(&base, &shaped(changed, "a2 * 2"), &[]);
            assert!(
                matches!(errors.as_slice(), [DiffError::ComputedInputChanged { column, .. }] if column == "A2"),
                "{what}: {errors:?}"
            );
        }
        // Added by this plan, it comes after the input's change: no refusal
        // (#1174 review).
        let bare = schema_of(
            "dbo.t",
            table(&[
                ("id", Column::new(ty("int")).not_null()),
                ("A2", Column::new(ty("int"))),
            ]),
        );
        let errors = errors_of(&bare, &shaped(Column::new(ty("bigint")), "a2 * 2"), &[]);
        assert!(errors.is_empty(), "{errors:?}");
        // Changed together with its expression, it is dropped first and
        // re-added after: no refusal.
        let errors = errors_of(&base, &shaped(Column::new(ty("bigint")), "a2 * 3"), &[]);
        assert!(errors.is_empty(), "{errors:?}");
        // Negative: a column it does not read, or names only in a literal.
        let base = shaped(Column::new(ty("int")), "id + len('a2')");
        let errors = errors_of(
            &base,
            &shaped(Column::new(ty("bigint")), "id + len('a2')"),
            &[],
        );
        assert!(errors.is_empty(), "{errors:?}");
        // A module it may call, altered while it stands.
        let calls = |definition: &str| {
            with_functions(
                shaped(Column::new(ty("int")), "dbo.f(a2)"),
                &[("dbo.f", definition)],
            )
        };
        let errors = errors_of(&calls("one"), &calls("two"), &[]);
        assert!(
            errors.iter().any(|e| matches!(e,
                DiffError::ComputedFunctionChanged { function, change, .. }
                    if function == "f" && *change == "alters")),
            "{errors:?}"
        );
        // Re-added around the alter, it calls the module when the module
        // changes: refused too (#1174 review).
        let errors = errors_of(
            &calls("one"),
            &with_functions(
                shaped(Column::new(ty("int")), "dbo.f(a2) + 1"),
                &[("dbo.f", "two")],
            ),
            &[],
        );
        assert!(
            errors.iter().any(|e| matches!(e,
                DiffError::ComputedFunctionChanged { change, .. } if *change == "alters")),
            "{errors:?}"
        );
        // Created with its table and the module it calls in one plan: the
        // module comes after the table, so it is refused.
        let fresh = Schema::default();
        let errors = errors_of(&fresh, &calls("one"), &[]);
        assert!(
            errors.iter().any(|e| matches!(e,
                DiffError::ComputedFunctionChanged { change, .. } if *change == "creates")),
            "{errors:?}"
        );
        // Dropped together with the module it calls: no refusal offline. The
        // order between them is the connected pass's, by the catalog's edges
        // (DEC-1431.1).
        let neither = schema_of(
            "dbo.t",
            table(&[
                ("id", Column::new(ty("int")).not_null()),
                ("A2", Column::new(ty("int"))),
            ]),
        );
        assert!(errors_of(&calls("one"), &neither, &[]).is_empty());
        // Nor is such a view's alter refused.
        let view = |definition: &str| {
            with_modules(
                shaped(Column::new(ty("int")), "a2 * 2"),
                &[("dbo.a2", definition)],
            )
        };
        let errors = errors_of(&view("one"), &view("two"), &[]);
        assert!(errors.is_empty(), "{errors:?}");
        // Negative: an altered module it does not name.
        let plain = shaped(Column::new(ty("int")), "a2 * 2");
        let errors = errors_of(
            &with_functions(plain.clone(), &[("dbo.f", "one")]),
            &with_functions(plain, &[("dbo.f", "two")]),
            &[],
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// A nullability change takes down and puts back what the dialect says
    /// blocks it, alone or inside a type change whose own dependents do not
    /// include them: on tightening the indexes and unique constraints over the
    /// column, and in both directions a filtered index. A check is not one of
    /// them (#1363).
    #[test]
    fn a_nullability_change_rebuilds_what_the_dialect_says_blocks_it() {
        let index = |column: &str| Index {
            columns: vec![IndexColumn {
                key: pbps_model::IndexKey::Column(column.into()),
                descending: false,
                opclass: None,
            }],
            include: Vec::new(),
            unique: false,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
        };
        let shaped = |nullable: bool, v: &str| {
            let col = |t: &str| {
                let c = Column::new(ty(t));
                if nullable { c } else { c.not_null() }
            };
            let mut t = table(&[
                ("id", Column::new(ty("int")).not_null()),
                ("k", col("int")),
                ("u", col("int")),
                ("c", col("int")),
                ("v", col(v)),
                ("other", Column::new(ty("int"))),
            ]);
            t.indexes.insert("ix_k".into(), index("k"));
            t.indexes.insert("ix_v".into(), index("v"));
            t.indexes.insert("ix_other".into(), index("other"));
            t.indexes.insert(
                "ix_filt".into(),
                Index {
                    filter: Some("other > 0".into()),
                    ..index("id")
                },
            );
            t.unique.insert(
                "uq_u".into(),
                UniqueConstraint {
                    columns: vec!["u".into()],
                    storage_parameters: Default::default(),
                },
            );
            t.checks.insert(
                "ck_c".into(),
                CheckConstraint {
                    expression: "c > 0".into(),
                },
            );
            schema_of("dbo.t", t)
        };
        let rebuilt = |cs: &ChangeSet| -> Vec<String> {
            let mut names: Vec<String> = cs
                .changes
                .iter()
                .filter_map(|p| match &p.change {
                    Change::DropIndex { name, .. }
                    | Change::DropUnique { name, .. }
                    | Change::DropCheck { name, .. } => Some(name.clone()),
                    _ => None,
                })
                .collect();
            names.sort();
            names
        };
        // `v` widens from varchar(10) to varchar(20) as it tightens: a type
        // change the dialect rebuilds nothing for, so its index comes down for
        // the tightening alone.
        let (nullable, not_null) = (shaped(true, "varchar(10)"), shaped(false, "varchar(20)"));
        let cs = run_with(&Recollates, &nullable, &not_null, &[]);
        // Filters are opaque text, so every filtered index of the table is
        // rebuilt, as for a type change (DEC-1169.2).
        assert_eq!(rebuilt(&cs), ["ix_filt", "ix_k", "ix_v", "uq_u"]);
        let k = kinds(&cs);
        for kind in ["AddIndex", "AddUnique"] {
            assert!(k.contains(&kind.to_owned()), "{kind}: {k:?}");
        }
        // Relaxing takes down the filtered index alone.
        let relaxed = shaped(true, "varchar(30)");
        let cs = run_with(&Recollates, &not_null, &relaxed, &[]);
        assert_eq!(rebuilt(&cs), ["ix_filt"]);
        // Negative: a dialect that tightens in place rebuilds nothing.
        let cs = run(&nullable, &not_null, &[]);
        assert!(rebuilt(&cs).is_empty(), "{:?}", kinds(&cs));
    }

    /// A collation change is the `ALTER COLUMN` a type change is, with both
    /// collations carried and the type restated, and it takes down and puts
    /// back every key, index and check the dialect says blocks it (#1175).
    #[test]
    fn a_collation_change_is_a_column_alter_that_rebuilds_its_dependents() {
        use pbps_model::Collation;
        let keyed = |collation: Option<&str>| {
            let mut code = Column::new(ty("varchar(10)")).not_null();
            code.collation = collation.map(Collation::new);
            let mut t = table(&[("id", Column::new(ty("int")).not_null()), ("code", code)]);
            t.primary_key = Some(PrimaryKey {
                name: Some("pk_t".into()),
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            });
            t.unique.insert(
                "uq_code".into(),
                UniqueConstraint {
                    columns: vec!["code".into()],
                    storage_parameters: Default::default(),
                },
            );
            t.checks.insert(
                "ck_code".into(),
                CheckConstraint {
                    expression: "code <> ''".into(),
                },
            );
            schema_of("dbo.t", t)
        };
        let (base, declared) = (keyed(None), keyed(Some("Latin1_General_CS_AS")));
        let cs = run_with(&Recollates, &base, &declared, &[]);
        let alter = cs
            .changes
            .iter()
            .find_map(|p| match &p.change {
                Change::AlterColumnType {
                    from,
                    to,
                    from_collation,
                    to_collation,
                    ..
                } => Some((
                    from.clone(),
                    to.clone(),
                    from_collation.clone(),
                    to_collation.clone(),
                )),
                _ => None,
            })
            .expect("an ALTER COLUMN");
        assert_eq!(alter.0, alter.1, "the type is restated, not changed");
        assert_eq!(
            (alter.2, alter.3),
            (None, Some(Collation::new("latin1_general_cs_as")))
        );
        let k = kinds(&cs);
        for kind in ["DropUnique", "AddUnique", "DropCheck", "AddCheck"] {
            assert!(k.contains(&kind.to_owned()), "{kind}: {k:?}");
        }
        // The key on `id` names no recollated column and stays.
        assert!(!k.contains(&"SetPrimaryKey".to_owned()), "{k:?}");
        // Negative: a dialect that needs nothing rebuilt gets the alter alone.
        assert_eq!(kinds(&run(&base, &declared, &[])), ["AlterColumnType"]);
        // And an unchanged collation, spelled in another case, is no change.
        assert!(
            run_with(
                &Recollates,
                &declared,
                &keyed(Some("LATIN1_general_cs_as")),
                &[]
            )
            .changes
            .is_empty()
        );
    }

    /// A nullability change restates the column's collation, which an
    /// `ALTER COLUMN` without it would reset to the database default (#1175).
    #[test]
    fn a_nullability_change_restates_the_collation() {
        use pbps_model::Collation;
        let with = |nullable: bool| {
            let mut c = Column::new(ty("varchar(10)"));
            c.nullable = nullable;
            c.collation = Some(Collation::new("Latin1_General_CS_AS"));
            schema_of("dbo.t", table(&[("c", c)]))
        };
        let cs = run(&with(true), &with(false), &[]);
        assert!(
            matches!(
                &cs.changes[..],
                [PlannedChange {
                    change: Change::AlterColumnNullability {
                        collation: Some(_),
                        ..
                    },
                    ..
                }]
            ),
            "{cs:?}"
        );
    }

    /// A table with a key, a UNIQUE constraint and an index, clustered on
    /// whichever `layout` names (#1178).
    /// An index's, a unique constraint's or a key's parameters alone change
    /// in place, one `SetIndexStorageParameters` each and no rebuild; a part
    /// that changes otherwise is rebuilt with its declared parameters in its
    /// `CREATE` (#1442).
    #[test]
    fn index_parameters_change_in_place_and_ride_a_rebuild() {
        let with = |ix_params: &[(&str, &str)], pk_params: &[(&str, &str)], descending: bool| {
            let mut t = table(&[
                ("id", Column::new(ty("int")).not_null()),
                ("v", Column::new(ty("int"))),
            ]);
            let map = |p: &[(&str, &str)]| {
                p.iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect()
            };
            t.primary_key = Some(PrimaryKey {
                name: Some("t_pkey".into()),
                columns: vec!["id".into()],
                storage_parameters: map(pk_params),
            });
            t.indexes.insert(
                "ix_v".into(),
                Index {
                    columns: vec![IndexColumn {
                        key: pbps_model::IndexKey::Column("v".into()),
                        descending,
                        opclass: None,
                    }],
                    include: Vec::new(),
                    unique: false,
                    filter: None,
                    method: Default::default(),
                    storage_parameters: map(ix_params),
                },
            );
            schema_of("public.t", t)
        };
        let base = with(&[("fillfactor", "70")], &[], false);
        let cs = run(
            &base,
            &with(
                &[("deduplicate_items", "false")],
                &[("fillfactor", "80")],
                false,
            ),
            &[],
        );
        let shown: Vec<String> = cs
            .changes
            .iter()
            .map(|p| match &p.change {
                Change::SetIndexStorageParameters {
                    target, set, reset, ..
                } => {
                    format!("{target:?} set {set:?} reset {reset:?}")
                }
                other => format!("{other:?}"),
            })
            .collect();
        // Their order between them is a tie, and none is needed.
        let mut shown = shown;
        shown.sort();
        assert_eq!(
            shown,
            [
                r#"Index("ix_v") set {"deduplicate_items": "false"} reset {"fillfactor"}"#,
                r#"PrimaryKey set {"fillfactor": "80"} reset {}"#,
            ]
        );
        // A definition change rebuilds, and the `CREATE` carries them.
        let cs = run(&base, &with(&[("fillfactor", "70")], &[], true), &[]);
        let added = cs.changes.iter().find_map(|p| match &p.change {
            Change::AddIndex { index, .. } => Some(index.storage_parameters.clone()),
            _ => None,
        });
        assert_eq!(
            added,
            Some([("fillfactor".to_owned(), "70".to_owned())].into())
        );
        assert!(
            !cs.changes
                .iter()
                .any(|p| matches!(p.change, Change::SetIndexStorageParameters { .. }))
        );
        // Negative: the same parameters plan nothing.
        assert!(run(&base, &base, &[]).changes.is_empty());
    }

    /// A persistence switch is one change, `destructive` to unlogged and
    /// unclassed back; switched together, linked tables go in the order the
    /// engine takes (to unlogged, referencing first; to logged, referenced
    /// first), and a permanent table referencing an unlogged one is refused
    /// (measured on 16 and 18, #1443).
    #[test]
    fn persistence_switches_follow_the_foreign_keys() {
        let pair = |unlogged: bool| {
            let mut s = Schema::default();
            let mut parent = table(&[("id", Column::new(ty("int")).not_null())]);
            parent.primary_key = Some(PrimaryKey {
                name: Some("pa_pkey".into()),
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            });
            parent.unlogged = unlogged;
            let mut child = table(&[("pa", Column::new(ty("int")))]);
            child.foreign_keys.insert(
                "ch_pa".into(),
                pbps_model::ForeignKey {
                    columns: vec!["pa".into()],
                    references_table: "public.pa".parse().unwrap(),
                    references_columns: vec!["id".into()],
                    on_delete: Default::default(),
                    on_update: Default::default(),
                },
            );
            child.unlogged = unlogged;
            s.tables.insert("public.pa".parse().unwrap(), parent);
            s.tables.insert("public.ch".parse().unwrap(), child);
            s
        };
        let order = |from: bool, to: bool| -> Vec<String> {
            run(&pair(from), &pair(to), &[])
                .changes
                .iter()
                .map(|p| match &p.change {
                    Change::SetTablePersistence {
                        table, unlogged, ..
                    } => format!("{table} {unlogged}"),
                    other => format!("{other:?}"),
                })
                .collect()
        };
        assert_eq!(order(false, true), ["public.ch true", "public.pa true"]);
        assert_eq!(order(true, false), ["public.pa false", "public.ch false"]);
        let cs = run(&pair(false), &pair(true), &[]);
        assert!(
            cs.changes[0]
                .risks
                .contains(&pbps_model::RiskClass::Destructive)
        );
        let cs = run(&pair(true), &pair(false), &[]);
        assert!(cs.changes[0].risks.is_empty(), "{:?}", cs.changes[0].risks);
        // Negative: a permanent child of an unlogged parent is refused.
        let mut mixed = pair(false);
        mixed
            .tables
            .get_mut(&"public.pa".parse::<TableName>().unwrap())
            .unwrap()
            .unlogged = true;
        let base_ids = crate::resolve(&pair(false), &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let ids = crate::resolve(&mixed, &base_ids, &[], &ctx()).unwrap().ids;
        let errors = diff_partial(
            Side {
                schema: &pair(false),
                ids: &base_ids,
            },
            Side {
                schema: &mixed,
                ids: &ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .errors;
        assert!(
            errors.iter().any(|e| matches!(e, DiffError::PermanentReferencesUnlogged { key, .. } if key == "ch_pa")),
            "{errors:?}"
        );
    }

    /// A partition tree is created parent first and every partition after
    /// it, whatever the names' order (#1170). A partition is then added under
    /// the standing parent, or dropped with intent and detached from it first
    /// (#1171); any other change to a partitioned table or a partition is
    /// refused by name: its bound, a parent's column, a rename, the parent's
    /// drop. A change to another table, even one referencing the parent, is
    /// not refused.
    /// A standing range-partitioned parent's columns change (#1687): an
    /// addition, a drop, a retype, a rename (a key column's too, which the
    /// key follows), a default and NOT NULL each one change to the parent,
    /// which the engine recurses into every partition. A partition's own
    /// default and NOT NULL follow the parent's change on their column:
    /// - carried through a rename;
    /// - taken with a drop;
    /// - set again after a default change and after a NOT NULL drop, every
    ///   partition brought to its declaration;
    /// - left to the parent's tightening.
    ///
    /// A key column's drop or retype, and an identity column added, are
    /// refused by name.
    /// A standing parent's indexes change as a table's do (#1688): added,
    /// dropped, and a new name as a drop and an add, the engine recursing
    /// each into every partition. Refused by name: `strategy: online`, a
    /// unique index without a partition-key column, and a new index that
    /// would take a partition's own as its clone (measured on 16 and 18).
    #[test]
    fn a_partitioned_parents_indexes_change_and_a_partitions_own_is_not_taken() {
        use pbps_model::{PartitionBound, PartitionBy, PartitionOf};
        let ev: TableName = "app.ev".parse().unwrap();
        let index = |columns: &[&str], unique: bool, filter: Option<&str>| Index {
            columns: columns
                .iter()
                .map(|c| pbps_model::IndexColumn::column(*c))
                .collect(),
            include: Vec::new(),
            unique,
            filter: filter.map(Into::into),
            method: Default::default(),
            storage_parameters: Default::default(),
        };
        let tree = |parent_indexes: &[(&str, Index)], own: &[(&str, Index)], created: bool| {
            let mut parent = table(&[
                ("id", Column::new(ty("int")).not_null()),
                ("ts", Column::new(ty("date")).not_null()),
                ("n", Column::new(ty("int"))),
            ]);
            parent.partition_by = Some(PartitionBy {
                columns: vec!["ts".into()],
            });
            for (name, ix) in parent_indexes {
                parent.indexes.insert((*name).into(), ix.clone());
            }
            let mut s = schema_of("app.ev", parent);
            let partition = |bound: PartitionBound, own: &[(&str, Index)]| {
                let mut t = Table {
                    partition_of: Some(PartitionOf {
                        parent: ev.clone(),
                        bound,
                        columns: Default::default(),
                    }),
                    ..Default::default()
                };
                for (name, ix) in own {
                    t.indexes.insert((*name).into(), ix.clone());
                }
                t
            };
            let (standing, new) = if created {
                (&[][..], own)
            } else {
                (own, &[][..])
            };
            s.tables.insert(
                "app.ev_1".parse().unwrap(),
                partition(PartitionBound::Default, standing),
            );
            if created {
                s.tables.insert(
                    "app.ev_new".parse().unwrap(),
                    partition(
                        PartitionBound::Range {
                            from: vec![pbps_model::BoundDatum::Value("2030-01-01".into())],
                            to: vec![pbps_model::BoundDatum::Value("2031-01-01".into())],
                        },
                        new,
                    ),
                );
            }
            s
        };
        let planned = |base: &Schema, declared: &Schema, online: bool| {
            let base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
                .unwrap()
                .ids;
            // A partition left out is dropped, which takes a reason.
            let dropped: Vec<Intent> = base
                .tables
                .keys()
                .filter(|t| !declared.tables.contains_key(*t))
                .map(|t| Intent::DropTable {
                    table: t.clone(),
                    reason: "gone".into(),
                })
                .collect();
            let declared_ids = crate::resolve(declared, &base_ids, &dropped, &ctx())
                .unwrap()
                .ids;
            let mut hints = Hints::default();
            if online {
                hints
                    .strategies
                    .insert(ev.clone(), pbps_model::Strategy { online: true });
            }
            diff(
                Side {
                    schema: base,
                    ids: &base_ids,
                },
                Side {
                    schema: declared,
                    ids: &declared_ids,
                },
                &MinimalDialect,
                &hints,
            )
            .map_err(|e| e.iter().map(ToString::to_string).collect::<Vec<_>>())
        };
        let outcome = |base: &Schema, declared: &Schema, online: bool| {
            planned(base, declared, online).map(|cs| kinds(&cs))
        };
        let plain = tree(&[], &[], false);
        let indexed = tree(&[("ev_n", index(&["n"], false, None))], &[], false);
        let renamed = tree(&[("ev_n2", index(&["n"], false, None))], &[], false);
        assert_eq!(
            outcome(&plain, &indexed, false),
            Ok(vec!["AddIndex".to_owned()])
        );
        assert_eq!(
            outcome(&indexed, &plain, false),
            Ok(vec!["DropIndex".to_owned()])
        );
        assert_eq!(
            outcome(&indexed, &renamed, false),
            Ok(vec!["DropIndex".to_owned(), "AddIndex".to_owned()])
        );
        let refused = |result: Result<Vec<String>, Vec<String>>, what: &str| {
            let errors = result.expect_err(what);
            assert!(errors.iter().any(|e| e.contains(what)), "{errors:?}");
        };
        // Online, either way.
        refused(outcome(&plain, &indexed, true), "with `strategy: online`");
        refused(outcome(&indexed, &plain, true), "with `strategy: online`");
        // Unique without the key column; with it, planned.
        refused(
            outcome(
                &plain,
                &tree(&[("ev_u", index(&["id"], true, None))], &[], false),
                false,
            ),
            "without the partition key column `ts`",
        );
        assert!(
            outcome(
                &plain,
                &tree(&[("ev_u", index(&["id", "ts"], true, None))], &[], false),
                false
            )
            .is_ok()
        );
        // Adoption: a standing partition's own index, and one created by the
        // same plan, a key's direction notwithstanding.
        let own = [("ev_1_n", index(&["n"], false, None))];
        refused(
            outcome(
                &tree(&[], &own, false),
                &tree(&[("ev_n", index(&["n"], false, None))], &own, false),
                false,
            ),
            "which can take app.ev_1's `ev_1_n`",
        );
        let mut descending = index(&["n"], false, None);
        descending.columns[0].descending = true;
        refused(
            outcome(
                &plain,
                &tree(
                    &[("ev_n", index(&["n"], false, None))],
                    &[("own", descending)],
                    true,
                ),
                false,
            ),
            "which can take app.ev_new's `own`",
        );
        // An own index this plan adds to a standing partition is built after
        // its parent's, whatever the names, and stays its own (#1737 review).
        // A partition whose name sorts before its parent's takes the rule,
        // not the tie-break, to come second.
        let renamed = |mut s: Schema, to: &str| {
            let t = s
                .tables
                .remove(&"app.ev_1".parse::<TableName>().unwrap())
                .unwrap();
            s.tables.insert(to.parse().unwrap(), t);
            s
        };
        for (partition, partition_index, parent_index) in
            [("app.ev_1", "ev_1_n", "ev_n"), ("app.aa", "a", "z")]
        {
            let both = tree(
                &[(parent_index, index(&["n"], false, None))],
                &[(partition_index, index(&["n"], false, None))],
                false,
            );
            let cs = planned(
                &renamed(plain.clone(), partition),
                &renamed(both, partition),
                false,
            )
            .expect("planned");
            let tables: Vec<String> = cs
                .changes
                .iter()
                .map(|p| p.change.table().unwrap().to_string())
                .collect();
            assert_eq!(tables, ["app.ev", partition], "{:?}", cs.changes);
        }
        // Refused by name: an index change in a plan that drops a partition
        // under the parent, whose rows the unique probe would still read and
        // whose shape a detach would check against the parent's indexes as
        // they stood (#1737 review).
        let mut without_partition = indexed.clone();
        without_partition
            .tables
            .remove(&"app.ev_1".parse::<TableName>().unwrap());
        let mut unique_without = tree(&[("ev_u", index(&["id", "ts"], true, None))], &[], false);
        unique_without
            .tables
            .remove(&"app.ev_1".parse::<TableName>().unwrap());
        let mut plain_without = plain.clone();
        plain_without
            .tables
            .remove(&"app.ev_1".parse::<TableName>().unwrap());
        refused(
            outcome(&plain, &unique_without, false),
            "change the parent and the partitions in separate plans",
        );
        refused(
            outcome(&indexed, &plain_without, false),
            "change the parent and the partitions in separate plans",
        );
        // Negative: the partition dropped alone is planned.
        assert!(outcome(&indexed, &without_partition, false).is_ok());
        // A predicate or an expression key is a match whatever its text: the
        // engine compares what it parses (#1737 review).
        let mut spaced = index(&["n"], false, None);
        spaced.columns[0].key = pbps_model::IndexKey::Expression("n + 1".into());
        let mut packed = spaced.clone();
        packed.columns[0].key = pbps_model::IndexKey::Expression("n+1".into());
        for (own_index, parent_index) in [
            (
                index(&["n"], false, Some("n>0")),
                index(&["n"], false, Some("n > 0")),
            ),
            (packed, spaced),
        ] {
            let own = [("ev_1_x", own_index)];
            refused(
                outcome(
                    &tree(&[], &own, false),
                    &tree(&[("ev_x", parent_index)], &own, false),
                    false,
                ),
                "which can take app.ev_1's `ev_1_x`",
            );
        }
        // Negative: an own index the plan redefines is dropped before the
        // parent's is built and added after it, so it is its own.
        let redefined = planned(
            &tree(
                &[],
                &[("ev_1_x", index(&["n"], false, Some("n > 0")))],
                false,
            ),
            &tree(
                &[("ev_n", index(&["n"], false, None))],
                &[("ev_1_x", index(&["n"], false, None))],
                false,
            ),
            false,
        )
        .expect("planned");
        let order: Vec<String> = redefined
            .changes
            .iter()
            .map(|p| {
                format!(
                    "{} {}",
                    change_in_words(&p.change),
                    p.change.table().unwrap()
                )
            })
            .collect();
        assert_eq!(
            order,
            [
                "drop index app.ev_1",
                "add index app.ev",
                "add index app.ev_1"
            ],
            "{order:?}"
        );
        // An own index kept across its parent's column rename stands when
        // the parent's is built: compared as renamed, it is not dropped
        // (#1737 review).
        {
            let own = |column: &str| [("ev_1_n", index(&[column], false, None))];
            let base = tree(&[], &own("n"), false);
            let mut declared = tree(&[("ev_n2", index(&["n2"], false, None))], &own("n2"), false);
            let parent = declared.tables.get_mut(&ev).unwrap();
            let n = parent.columns.shift_remove("n").unwrap();
            parent.columns.insert("n2".into(), n);
            let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
                .unwrap()
                .ids;
            let rename = [Intent::RenameColumn {
                table: ev.clone(),
                from: "n".into(),
                to: "n2".into(),
            }];
            let declared_ids = crate::resolve(&declared, &base_ids, &rename, &ctx())
                .unwrap()
                .ids;
            let errors = diff(
                Side {
                    schema: &base,
                    ids: &base_ids,
                },
                Side {
                    schema: &declared,
                    ids: &declared_ids,
                },
                &MinimalDialect,
                &Hints::default(),
            )
            .expect_err("refused");
            assert!(
                errors
                    .iter()
                    .any(|e| e.to_string().contains("which can take app.ev_1's `ev_1_n`")),
                "{errors:?}"
            );
        }
        // Negative: an own index the engine does not adopt, by uniqueness or
        // by predicate, leaves the parent's planned.
        for other in [
            index(&["n", "ts"], true, None),
            index(&["n"], false, Some("n > 0")),
        ] {
            let own = [("ev_1_x", other)];
            assert_eq!(
                outcome(
                    &tree(&[], &own, false),
                    &tree(&[("ev_n", index(&["n"], false, None))], &own, false),
                    false,
                ),
                Ok(vec!["AddIndex".to_owned()])
            );
        }
    }

    /// A standing parent's keys, checks and foreign keys change as a
    /// table's do (#1689), the engine recursing each into every partition,
    /// and a foreign key may reference it. Refused by name: a key without a
    /// partition-key column, a check under a name a partition holds as its
    /// own, and any of them in a plan that drops a partition under it.
    /// `strategy: online` is no refusal here: only an index honours it.
    #[test]
    fn a_partitioned_parents_keys_checks_and_foreign_keys_change() {
        use pbps_model::{
            CheckConstraint, ForeignKey, PartitionBound, PartitionBy, PartitionOf, PrimaryKey,
            UniqueConstraint,
        };
        let ev: TableName = "app.ev".parse().unwrap();
        let ev_1: TableName = "app.ev_1".parse().unwrap();
        let check = |e: &str| CheckConstraint {
            expression: e.into(),
        };
        let key = |columns: &[&str]| columns.iter().map(|c| (*c).to_owned()).collect::<Vec<_>>();
        let tree = |edit: &dyn Fn(&mut Table, &mut Table, &mut Schema)| {
            let mut parent = table(&[
                ("id", Column::new(ty("int")).not_null()),
                ("ts", Column::new(ty("date")).not_null()),
                ("r", Column::new(ty("int"))),
            ]);
            parent.partition_by = Some(PartitionBy {
                columns: vec!["ts".into()],
            });
            let mut partition = Table {
                partition_of: Some(PartitionOf {
                    parent: ev.clone(),
                    bound: PartitionBound::Default,
                    columns: Default::default(),
                }),
                ..Default::default()
            };
            let mut s = schema_of("app.ref", {
                let mut t = table(&[("id", Column::new(ty("int")).not_null())]);
                t.primary_key = Some(PrimaryKey {
                    name: None,
                    columns: key(&["id"]),
                    storage_parameters: Default::default(),
                });
                t
            });
            s.tables.insert(
                "app.child".parse().unwrap(),
                table(&[
                    ("id", Column::new(ty("int")).not_null()),
                    ("ts", Column::new(ty("date")).not_null()),
                ]),
            );
            edit(&mut parent, &mut partition, &mut s);
            s.tables.insert(ev.clone(), parent);
            s.tables.insert(ev_1.clone(), partition);
            s
        };
        let outcome = |base: &Schema, declared: &Schema, online: bool| {
            let base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
                .unwrap()
                .ids;
            let dropped: Vec<Intent> = base
                .tables
                .keys()
                .filter(|t| !declared.tables.contains_key(*t))
                .map(|t| Intent::DropTable {
                    table: t.clone(),
                    reason: "gone".into(),
                })
                .collect();
            let declared_ids = crate::resolve(declared, &base_ids, &dropped, &ctx())
                .unwrap()
                .ids;
            let mut hints = Hints::default();
            if online {
                hints
                    .strategies
                    .insert(ev.clone(), pbps_model::Strategy { online: true });
            }
            diff(
                Side {
                    schema: base,
                    ids: &base_ids,
                },
                Side {
                    schema: declared,
                    ids: &declared_ids,
                },
                &MinimalDialect,
                &hints,
            )
            .map(|cs| kinds(&cs))
            .map_err(|e| e.iter().map(ToString::to_string).collect::<Vec<_>>())
        };
        let refused = |result: Result<Vec<String>, Vec<String>>, what: &str| {
            let errors = result.expect_err(what);
            assert!(errors.iter().any(|e| e.contains(what)), "{errors:?}");
        };
        let plain = tree(&|_, _, _| {});
        let pk = |columns: &'static [&'static str]| {
            move |p: &mut Table, _: &mut Table, _: &mut Schema| {
                p.primary_key = Some(PrimaryKey {
                    name: None,
                    columns: key(columns),
                    storage_parameters: Default::default(),
                });
            }
        };
        let unique = |columns: &'static [&'static str]| {
            move |p: &mut Table, _: &mut Table, _: &mut Schema| {
                p.unique.insert(
                    "ev_u".into(),
                    UniqueConstraint {
                        columns: key(columns),
                        storage_parameters: Default::default(),
                    },
                );
            }
        };
        let checked = tree(&|p, _, _| {
            p.checks.insert("ev_ck".into(), check("r > 0"));
        });
        let fk = tree(&|p, _, _| {
            p.foreign_keys.insert(
                "ev_r_fk".into(),
                ForeignKey {
                    columns: key(&["r"]),
                    references_table: "app.ref".parse().unwrap(),
                    references_columns: key(&["id"]),
                    on_delete: Default::default(),
                    on_update: Default::default(),
                },
            );
        });
        // Each added and dropped.
        for (declared, kind) in [
            (tree(&pk(&["id", "ts"])), "SetPrimaryKey"),
            (tree(&unique(&["id", "ts"])), "AddUnique"),
            (checked.clone(), "AddCheck"),
            (fk.clone(), "AddForeignKey"),
        ] {
            assert_eq!(outcome(&plain, &declared, false), Ok(vec![kind.to_owned()]));
            assert!(outcome(&declared, &plain, false).is_ok(), "{kind} dropped");
        }
        // A foreign key referencing the parent's key.
        let keyed = tree(&pk(&["id", "ts"]));
        let referenced = tree(&|p, q, s| {
            pk(&["id", "ts"])(p, q, s);
            s.tables
                .get_mut(&"app.child".parse::<TableName>().unwrap())
                .unwrap()
                .foreign_keys
                .insert(
                    "child_ev_fk".into(),
                    ForeignKey {
                        columns: key(&["id", "ts"]),
                        references_table: ev.clone(),
                        references_columns: key(&["id", "ts"]),
                        on_delete: Default::default(),
                        on_update: Default::default(),
                    },
                );
        });
        assert_eq!(
            outcome(&keyed, &referenced, false),
            Ok(vec!["AddForeignKey".to_owned()])
        );
        // Without the partition key column.
        refused(
            outcome(&plain, &tree(&pk(&["id"])), false),
            "without the partition key column `ts`",
        );
        refused(
            outcome(&plain, &tree(&unique(&["id"])), false),
            "without the partition key column `ts`",
        );
        // A check under a name the partition holds as its own; under another
        // name, planned.
        let own = |name: &'static str| {
            move |_: &mut Table, q: &mut Table, _: &mut Schema| {
                q.checks.insert(name.into(), check("r > 1"));
            }
        };
        refused(
            outcome(
                &tree(&own("ev_ck")),
                &tree(&|p, q, s| {
                    own("ev_ck")(p, q, s);
                    p.checks.insert("ev_ck".into(), check("r > 0"));
                }),
                false,
            ),
            "a name app.ev_1 holds as its own check",
        );
        assert_eq!(
            outcome(
                &tree(&own("ev_1_ck")),
                &tree(&|p, q, s| {
                    own("ev_1_ck")(p, q, s);
                    p.checks.insert("ev_ck".into(), check("r > 0"));
                }),
                false,
            ),
            Ok(vec!["AddCheck".to_owned()])
        );
        // In a plan that drops a partition under the parent.
        let mut checked_without = checked.clone();
        checked_without.tables.remove(&ev_1);
        refused(
            outcome(&plain, &checked_without, false),
            "change the parent and the partitions in separate plans",
        );
        // `strategy: online` asks nothing of a check or a key.
        assert_eq!(
            outcome(&plain, &checked, true),
            Ok(vec!["AddCheck".to_owned()])
        );
    }

    #[test]
    fn a_partitioned_parents_columns_change_and_its_partitions_keep_their_own() {
        use pbps_model::{BoundDatum, PartitionBound, PartitionBy, PartitionColumn, PartitionOf};
        let parent = || {
            let mut t = table(&[
                ("id", Column::new(ty("int")).not_null()),
                ("ts", Column::new(ty("date")).not_null()),
                ("n", Column::new(ty("int")).not_null()),
                ("m", Column::new(ty("int"))),
            ]);
            t.partition_by = Some(PartitionBy {
                columns: vec!["ts".into()],
            });
            t
        };
        let partition = |from: &str, to: &str, own: &[(&str, PartitionColumn)]| Table {
            partition_of: Some(PartitionOf {
                parent: "app.ev".parse().unwrap(),
                bound: PartitionBound::Range {
                    from: vec![BoundDatum::Value(from.into())],
                    to: vec![BoundDatum::Value(to.into())],
                },
                columns: own
                    .iter()
                    .map(|(c, o)| ((*c).to_owned(), o.clone()))
                    .collect(),
            }),
            ..Default::default()
        };
        let own_default = |d: &str| PartitionColumn {
            default: Some(d.into()),
            not_null: false,
        };
        let own_not_null = PartitionColumn {
            default: None,
            not_null: true,
        };
        let tree = |parent: Table, a: &[(&str, PartitionColumn)], b: &[(&str, PartitionColumn)]| {
            let mut s = schema_of("app.ev", parent);
            s.tables.insert(
                "app.a".parse().unwrap(),
                partition("2024-01-01", "2025-01-01", a),
            );
            s.tables.insert(
                "app.b".parse().unwrap(),
                partition("2025-01-01", "2026-01-01", b),
            );
            s
        };
        let base = tree(
            parent(),
            &[("m", own_default("7"))],
            &[("m", own_not_null.clone())],
        );
        let outcome = |declared: &Schema, intents: &[Intent]| {
            let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
                .unwrap()
                .ids;
            let declared_ids = crate::resolve(declared, &base_ids, intents, &ctx())
                .unwrap()
                .ids;
            diff(
                Side {
                    schema: &base,
                    ids: &base_ids,
                },
                Side {
                    schema: declared,
                    ids: &declared_ids,
                },
                &MinimalDialect,
                &Hints::default(),
            )
        };
        let planned = |declared: &Schema, intents: &[Intent]| -> Vec<Change> {
            outcome(declared, intents)
                .unwrap_or_else(|e| panic!("refused: {e:?}"))
                .changes
                .into_iter()
                .map(|p| p.change)
                .collect()
        };
        let refused = |declared: &Schema, intents: &[Intent]| -> Vec<String> {
            match outcome(declared, intents) {
                Ok(cs) => panic!("planned {:?}", kinds(&cs)),
                Err(e) => e.iter().map(ToString::to_string).collect(),
            }
        };
        let ev: TableName = "app.ev".parse().unwrap();
        let with = |edit: &dyn Fn(&mut Table),
                    a: &[(&str, PartitionColumn)],
                    b: &[(&str, PartitionColumn)]| {
            let mut p = parent();
            edit(&mut p);
            tree(p, a, b)
        };
        let keep_a = [("m", own_default("7"))];
        let keep_b = [("m", own_not_null.clone())];

        // Added: the parent's change alone.
        let added = planned(
            &with(
                &|p| {
                    p.columns.insert("note".into(), Column::new(ty("text")));
                },
                &keep_a,
                &keep_b,
            ),
            &[],
        );
        assert!(
            matches!(added.as_slice(), [Change::AddColumn { name, .. }] if name == "note"),
            "{added:?}"
        );
        // Retyped: the parent's change alone.
        let retyped = planned(
            &with(
                &|p| {
                    p.columns.get_mut("n").unwrap().ty = ty("bigint");
                },
                &keep_a,
                &keep_b,
            ),
            &[],
        );
        assert!(
            matches!(retyped.as_slice(), [Change::AlterColumnType { .. }]),
            "{retyped:?}"
        );
        // Dropped with intent: the partitions' own on it go with it.
        let drop_m = [Intent::DropColumn {
            column: ColumnRef {
                table: ev.clone(),
                name: "m".into(),
            },
            reason: "gone".into(),
        }];
        let dropped = planned(
            &with(
                &|p| {
                    p.columns.shift_remove("m");
                },
                &[],
                &[],
            ),
            &drop_m,
        );
        assert!(
            matches!(dropped.as_slice(), [Change::DropColumn { .. }]),
            "{dropped:?}"
        );
        // Renamed, the key column too: the key and the partitions' own follow.
        let renames = [
            Intent::RenameColumn {
                table: ev.clone(),
                from: "m".into(),
                to: "m2".into(),
            },
            Intent::RenameColumn {
                table: ev.clone(),
                from: "ts".into(),
                to: "at".into(),
            },
        ];
        let renamed = planned(
            &with(
                &|p| {
                    // In place: a rename keeps the column's position.
                    p.columns = std::mem::take(&mut p.columns)
                        .into_iter()
                        .map(|(c, column)| match c.as_str() {
                            "m" => ("m2".to_owned(), column),
                            "ts" => ("at".to_owned(), column),
                            _ => (c, column),
                        })
                        .collect();
                    p.partition_by = Some(PartitionBy {
                        columns: vec!["at".into()],
                    });
                },
                &[("m2", own_default("7"))],
                &[("m2", own_not_null.clone())],
            ),
            &renames,
        );
        assert!(
            renamed.len() == 2
                && renamed
                    .iter()
                    .all(|c| matches!(c, Change::RenameColumn { .. })),
            "{renamed:?}"
        );
        // A default set on the parent: its own default set again after it on
        // `a`; `b` has none, and takes the parent's.
        let defaulted = planned(
            &with(
                &|p| {
                    p.columns.get_mut("m").unwrap().default = Some("1".into());
                },
                &keep_a,
                &keep_b,
            ),
            &[],
        );
        assert!(
            matches!(
                defaulted.as_slice(),
                [
                    Change::AlterColumnDefault { .. },
                    Change::SetPartitionDefault { table, to: Some(own), fallback: Some(f), .. },
                ] if table.to_string() == "app.a" && own == "7" && f == "1"
            ),
            "{defaulted:?}"
        );
        // NOT NULL dropped on the parent: every partition brought to its
        // declaration after it, `a` without one and `b` with its own.
        let loosened = planned(
            &with(
                &|p| {
                    p.columns.get_mut("n").unwrap().nullable = true;
                },
                &keep_a,
                &[("m", own_not_null.clone()), ("n", own_not_null.clone())],
            ),
            &[],
        );
        assert!(
            matches!(
                loosened.first(),
                Some(Change::AlterColumnNullability {
                    to_nullable: true,
                    ..
                })
            ),
            "{loosened:?}"
        );
        let set: Vec<(String, bool)> = loosened[1..]
            .iter()
            .filter_map(|c| match c {
                Change::SetPartitionNotNull {
                    table,
                    column,
                    not_null,
                    ..
                } if column == "n" => Some((table.to_string(), *not_null)),
                _ => None,
            })
            .collect();
        assert_eq!(
            set,
            [("app.a".to_owned(), false), ("app.b".to_owned(), true)],
            "{loosened:?}"
        );
        assert_eq!(loosened.len(), 3, "{loosened:?}");
        // NOT NULL set on the parent: `b`'s own, which the declaration then
        // leaves out, is left to it.
        let tightened = planned(
            &with(
                &|p| {
                    p.columns.get_mut("m").unwrap().nullable = false;
                },
                &keep_a,
                &[],
            ),
            &[],
        );
        assert!(
            matches!(
                tightened.as_slice(),
                [Change::AlterColumnNullability {
                    to_nullable: false,
                    ..
                }]
            ),
            "{tightened:?}"
        );

        // Negative: a key column dropped or retyped, and an identity column
        // added, are refused by name.
        let drop_ts = [Intent::DropColumn {
            column: ColumnRef {
                table: ev.clone(),
                name: "ts".into(),
            },
            reason: "gone".into(),
        }];
        let mut no_key = with(
            &|p| {
                p.columns.shift_remove("ts");
            },
            &keep_a,
            &keep_b,
        );
        no_key.tables.get_mut(&ev).unwrap().partition_by = Some(PartitionBy {
            columns: vec!["ts".into()],
        });
        let e = refused(&no_key, &drop_ts);
        assert!(
            e.iter()
                .any(|m| m.contains("which is in its partition key")),
            "{e:?}"
        );
        let e = refused(
            &with(
                &|p| {
                    p.columns.get_mut("ts").unwrap().ty = ty("timestamp");
                },
                &keep_a,
                &keep_b,
            ),
            &[],
        );
        assert!(
            e.iter()
                .any(|m| m.contains("which is in its partition key")),
            "{e:?}"
        );
        let e = refused(
            &with(
                &|p| {
                    let mut c = Column::new(ty("int")).not_null();
                    c.identity = Some(pbps_model::Identity {
                        seed: 1,
                        increment: 1,
                    });
                    p.columns.insert("seq".into(), c);
                },
                &keep_a,
                &keep_b,
            ),
            &[],
        );
        assert!(
            e.iter()
                .any(|m| m.contains("an identity column") && m.contains("#1681")),
            "{e:?}"
        );

        // From another base, through revisions: one revision cannot rename
        // into a name it also drops, so a swap takes two.
        let from_outcome = |base: &Schema, revisions: &[(&Schema, &[Intent])]| {
            let base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
                .unwrap()
                .ids;
            let mut declared_ids = base_ids.clone();
            for (schema, intents) in revisions {
                declared_ids = crate::resolve(schema, &declared_ids, intents, &ctx())
                    .unwrap()
                    .ids;
            }
            let declared = revisions.last().unwrap().0;
            diff(
                Side {
                    schema: base,
                    ids: &base_ids,
                },
                Side {
                    schema: declared,
                    ids: &declared_ids,
                },
                &MinimalDialect,
                &Hints::default(),
            )
            .map(|cs| cs.changes.into_iter().map(|p| p.change).collect::<Vec<_>>())
            .map_err(|e| e.iter().map(ToString::to_string).collect::<Vec<_>>())
        };
        let from = |base: &Schema, revisions: &[(&Schema, &[Intent])]| {
            from_outcome(base, revisions).unwrap_or_else(|e| panic!("refused: {e:?}"))
        };
        // A name passing from one column to another under a partitioned
        // parent is planned as a plain table's: the pre-flight, the guard and
        // the connected passes follow the parent's columns into the
        // partitions (DEC-1699.1), where the interim plan refused it (#1692).
        let reuse_planned = |outcome: Result<Vec<Change>, Vec<String>>, from: &str, to: &str| {
            let changes = outcome.unwrap_or_else(|e| panic!("refused: {e:?}"));
            assert!(
                changes.iter().any(|c| matches!(c,
                    Change::RenameColumn { from: f, to: t, .. } if f == from && t == to)),
                "{changes:?}"
            );
        };
        // `n` dropped and `m` renamed into its name.
        let handoff_base = tree(
            parent(),
            &[("m", own_default("7")), ("n", own_default("8"))],
            &[],
        );
        let handoff = with(
            &|p| {
                p.columns.shift_remove("n");
                p.columns = std::mem::take(&mut p.columns)
                    .into_iter()
                    .map(|(c, col)| (if c == "m" { "n".to_owned() } else { c }, col))
                    .collect();
            },
            &[("n", own_default("8"))],
            &[],
        );
        let without_n = with(
            &|p| {
                p.columns.shift_remove("n");
            },
            &[("m", own_default("7"))],
            &[],
        );
        let handed_off = from_outcome(
            &handoff_base,
            &[
                (
                    &without_n,
                    &[Intent::DropColumn {
                        column: ColumnRef {
                            table: ev.clone(),
                            name: "n".into(),
                        },
                        reason: "gone".into(),
                    }],
                ),
                (
                    &handoff,
                    &[Intent::RenameColumn {
                        table: ev.clone(),
                        from: "m".into(),
                        to: "n".into(),
                    }],
                ),
            ],
        );
        reuse_planned(handed_off, "m", "n");
        // A retype that drops NOT NULL with it: the partitions' NOT NULLs
        // follow the retype, which carries the parent's change (#1692
        // review).
        let retype_loosened = planned(
            &with(
                &|p| {
                    let n = p.columns.get_mut("n").unwrap();
                    n.ty = ty("bigint");
                    n.nullable = true;
                },
                &keep_a,
                &[("m", own_not_null.clone()), ("n", own_not_null.clone())],
            ),
            &[],
        );
        let retype_at = retype_loosened
            .iter()
            .position(|c| matches!(c, Change::AlterColumnType { .. }))
            .expect("the retype");
        assert!(
            retype_loosened
                .iter()
                .enumerate()
                .filter(|(_, c)| matches!(c, Change::SetPartitionNotNull { .. }))
                .all(|(i, _)| i > retype_at)
                && retype_loosened
                    .iter()
                    .filter(|c| matches!(c, Change::SetPartitionNotNull { .. }))
                    .count()
                    == 2,
            "{retype_loosened:?}"
        );
        // A partition created with its own default on a column its parent
        // adds in the same plan comes after the addition (#1692 review).
        let mut grown = with(
            &|p| {
                p.columns.insert("extra".into(), Column::new(ty("int")));
            },
            &keep_a,
            &keep_b,
        );
        grown.tables.insert(
            "app.c".parse().unwrap(),
            partition("2026-01-01", "2027-01-01", &[("extra", own_default("5"))]),
        );
        let grew = planned(&grown, &[]);
        let at = |kind: &dyn Fn(&Change) -> bool| grew.iter().position(kind).unwrap();
        assert!(
            at(&|c| matches!(c, Change::AddColumn { .. }))
                < at(&|c| matches!(c, Change::CreateTable { .. })),
            "{grew:?}"
        );
        // A non-key column dropped and the key column renamed into its name.
        let spare_base = with(
            &|p| {
                p.columns.insert("spare".into(), Column::new(ty("int")));
            },
            &keep_a,
            &keep_b,
        );
        let spare = with(
            &|p| {
                p.columns = std::mem::take(&mut p.columns)
                    .into_iter()
                    .map(|(c, col)| (if c == "ts" { "spare".to_owned() } else { c }, col))
                    .collect();
                p.partition_by = Some(PartitionBy {
                    columns: vec!["spare".into()],
                });
            },
            &keep_a,
            &keep_b,
        );
        let without_spare = with(&|_| {}, &keep_a, &keep_b);
        let swapped = from_outcome(
            &spare_base,
            &[
                (
                    &without_spare,
                    &[Intent::DropColumn {
                        column: ColumnRef {
                            table: ev.clone(),
                            name: "spare".into(),
                        },
                        reason: "gone".into(),
                    }],
                ),
                (
                    &spare,
                    &[Intent::RenameColumn {
                        table: ev.clone(),
                        from: "ts".into(),
                        to: "spare".into(),
                    }],
                ),
            ],
        );
        reuse_planned(swapped, "ts", "spare");
        // The key column moved out of `ts` and a non-key column into it,
        // then retyped: the retype is the non-key column's (#1692 review).
        let rename = |p: &mut Table, from: &str, to: &str| {
            p.columns = std::mem::take(&mut p.columns)
                .into_iter()
                .map(|(c, col)| (if c == from { to.to_owned() } else { c }, col))
                .collect();
        };
        let moved_key = with(
            &|p| {
                p.columns.insert("spare".into(), Column::new(ty("int")));
                rename(p, "ts", "k");
                p.partition_by = Some(PartitionBy {
                    columns: vec!["k".into()],
                });
            },
            &keep_a,
            &keep_b,
        );
        let moved_in = with(
            &|p| {
                p.columns.insert("spare".into(), Column::new(ty("int")));
                rename(p, "ts", "k");
                rename(p, "spare", "ts");
                p.columns.get_mut("ts").unwrap().ty = ty("bigint");
                p.partition_by = Some(PartitionBy {
                    columns: vec!["k".into()],
                });
            },
            &keep_a,
            &keep_b,
        );
        let renaming = |from: &str, to: &str| Intent::RenameColumn {
            table: ev.clone(),
            from: from.into(),
            to: to.into(),
        };
        // The key's old name passes to another column.
        let retyped = from_outcome(
            &spare_base,
            &[
                (&moved_key, &[renaming("ts", "k")]),
                (&moved_in, &[renaming("spare", "ts")]),
            ],
        );
        reuse_planned(retyped, "spare", "ts");
        // A name renamed away and given to a new column (round 8): the
        // partition's own NOT NULL probe no longer reads the old column.
        let renamed_away = with(&|p| rename(p, "n", "n_old"), &keep_a, &keep_b);
        let readded = with(
            &|p| {
                rename(p, "n", "n_old");
                p.columns.insert("n".into(), Column::new(ty("int")));
            },
            &keep_a,
            &keep_b,
        );
        reuse_planned(
            from_outcome(
                &with(&|_| {}, &keep_a, &keep_b),
                &[(&renamed_away, &[renaming("n", "n_old")]), (&readded, &[])],
            ),
            "n",
            "n_old",
        );
        // A partition's own index on the renamed column is carried by the
        // engine's rename, and not dropped and added again (#1692 review).
        let own_index = |s: &mut Schema, column: &str| {
            s.tables
                .get_mut(&"app.a".parse::<TableName>().unwrap())
                .unwrap()
                .indexes
                .insert(
                    "a_m".into(),
                    Index {
                        columns: vec![pbps_model::IndexColumn {
                            key: pbps_model::IndexKey::Column(column.into()),
                            descending: false,
                            opclass: None,
                        }],
                        include: Vec::new(),
                        unique: false,
                        filter: None,
                        method: Default::default(),
                        storage_parameters: Default::default(),
                    },
                );
        };
        let mut indexed_base = with(&|_| {}, &keep_a, &keep_b);
        own_index(&mut indexed_base, "m");
        let mut indexed_renamed = with(
            &|p| rename(p, "m", "m2"),
            &[("m2", own_default("7"))],
            &[("m2", own_not_null.clone())],
        );
        own_index(&mut indexed_renamed, "m2");
        let carried = from(&indexed_base, &[(&indexed_renamed, &[renaming("m", "m2")])]);
        assert!(
            matches!(carried.as_slice(), [Change::RenameColumn { .. }]),
            "{carried:?}"
        );
        // A partition's own check or index beside its parent's change to the
        // column it reads: planned, the parent's change first. The pre-flight
        // takes the parent's retype or rename as the partition's own
        // (DEC-1699.1), where the interim plan refused it (#1692).
        let retype_n = |p: &mut Table| p.columns.get_mut("n").unwrap().ty = ty("bigint");
        let a: TableName = "app.a".parse().unwrap();
        let reads = |s: &mut Schema, expression: &str| {
            s.tables.get_mut(&a).unwrap().checks.insert(
                "a_n".into(),
                pbps_model::CheckConstraint {
                    expression: expression.into(),
                },
            );
        };
        let own_unique = |s: &mut Schema, unique: bool| {
            own_index(s, "n");
            s.tables
                .get_mut(&a)
                .unwrap()
                .indexes
                .get_mut("a_m")
                .unwrap()
                .unique = unique;
        };
        fn index_of<'s>(s: &'s mut Schema, a: &TableName) -> &'s mut pbps_model::Index {
            s.tables.get_mut(a).unwrap().indexes.get_mut("a_m").unwrap()
        }
        let mut checked = with(&retype_n, &keep_a, &keep_b);
        reads(&mut checked, "n > 0");
        let mut escaped = with(&retype_n, &keep_a, &keep_b);
        reads(&mut escaped, r#"U&"\006E" > 0"#);
        let mut unique = with(&retype_n, &keep_a, &keep_b);
        own_unique(&mut unique, true);
        let mut filtered = with(&retype_n, &keep_a, &keep_b);
        own_unique(&mut filtered, false);
        index_of(&mut filtered, &a).filter = Some("n > 0".into());
        let mut expression = with(&retype_n, &keep_a, &keep_b);
        own_unique(&mut expression, true);
        index_of(&mut expression, &a).columns[0].key =
            pbps_model::IndexKey::Expression("(n + 1)".into());
        let mut renamed_checked = with(
            &|p| rename(p, "m", "m2"),
            &[("m2", own_default("7"))],
            &[("m2", own_not_null.clone())],
        );
        reads(&mut renamed_checked, "\"M2\" > 0");
        let mut defaulted = with(
            &|p| p.columns.get_mut("n").unwrap().default = Some("5".into()),
            &keep_a,
            &keep_b,
        );
        own_unique(&mut defaulted, true);
        let mut loosened = with(
            &|p| p.columns.get_mut("n").unwrap().nullable = true,
            &keep_a,
            &keep_b,
        );
        reads(&mut loosened, "n > 0");
        let rename_m = [renaming("m", "m2")];
        let parents = |c: &Change| {
            matches!(
                c,
                Change::AlterColumnType { .. }
                    | Change::RenameColumn { .. }
                    | Change::AlterColumnDefault { .. }
                    | Change::AlterColumnNullability { .. }
            )
        };
        for (what, declared, intents) in [
            ("a check on the retyped column", &checked, &[][..]),
            ("an escaped name", &escaped, &[][..]),
            ("a unique index", &unique, &[][..]),
            ("a filtered index", &filtered, &[][..]),
            ("a unique index over an expression", &expression, &[][..]),
            (
                "a check on the renamed column",
                &renamed_checked,
                &rename_m[..],
            ),
            ("a unique index beside a default", &defaulted, &[][..]),
            ("a check beside a nullability change", &loosened, &[][..]),
        ] {
            let changes = planned(declared, intents);
            let own = changes
                .iter()
                .position(|c| {
                    matches!(c, Change::AddCheck { table, .. } | Change::AddIndex { table, .. }
                        if *table == a)
                })
                .unwrap_or_else(|| panic!("{what}: {changes:?}"));
            let parent = changes
                .iter()
                .position(parents)
                .unwrap_or_else(|| panic!("{what}: {changes:?}"));
            assert!(parent < own, "{what}: {changes:?}");
        }
        // A partition dropped while its parent's column tightens: the
        // pre-flight would count the doomed partition's rows (round 7).
        let mut dropped_b = with(
            &|p| p.columns.get_mut("m").unwrap().nullable = false,
            &[("m", own_default("7"))],
            &[],
        );
        dropped_b
            .tables
            .remove(&"app.b".parse::<TableName>().unwrap());
        let errors = refused(
            &dropped_b,
            &[Intent::DropTable {
                table: "app.b".parse().unwrap(),
                reason: "gone".into(),
            }],
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("while this plan attaches, detaches or drops app.b")),
            "{errors:?}"
        );
        // Negative: the partition's own check alone is its change alone.
        let mut unretyped = with(&|_| {}, &keep_a, &keep_b);
        reads(&mut unretyped, "n > 0");
        assert!(
            matches!(
                planned(&unretyped, &[]).as_slice(),
                [Change::AddCheck { .. }]
            ),
            "the check alone"
        );
    }

    #[test]
    fn a_partition_tree_is_created_then_gains_and_loses_partitions_only() {
        use pbps_model::{BoundDatum, PartitionBound, PartitionBy, PartitionOf};
        let mut parent = table(&[
            ("id", Column::new(ty("int")).not_null()),
            ("ts", Column::new(ty("date")).not_null()),
        ]);
        parent.partition_by = Some(PartitionBy {
            columns: vec!["ts".into()],
        });
        let partition = |bound: PartitionBound| Table {
            partition_of: Some(PartitionOf {
                parent: "app.ev".parse().unwrap(),
                bound,
                columns: Default::default(),
            }),
            ..Default::default()
        };
        let range = |from: &str, to: &str| PartitionBound::Range {
            from: vec![BoundDatum::Value(from.into())],
            to: vec![BoundDatum::Value(to.into())],
        };
        // `a_old` sorts before its parent by name, and must not be created
        // before it.
        let mut tree = schema_of("app.ev", parent.clone());
        tree.tables.insert(
            "app.a_old".parse().unwrap(),
            partition(range("2024-01-01", "2025-01-01")),
        );
        tree.tables.insert(
            "app.z_rest".parse().unwrap(),
            partition(PartitionBound::Default),
        );
        let outcome = |base: &Schema, declared: &Schema, intents: &[Intent]| {
            let base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
                .unwrap()
                .ids;
            let declared_ids = crate::resolve(declared, &base_ids, intents, &ctx())
                .unwrap()
                .ids;
            diff(
                Side {
                    schema: base,
                    ids: &base_ids,
                },
                Side {
                    schema: declared,
                    ids: &declared_ids,
                },
                &MinimalDialect,
                &Hints::default(),
            )
        };
        let refused = |base: &Schema, declared: &Schema, intents: &[Intent]| -> Vec<String> {
            match outcome(base, declared, intents) {
                Ok(cs) => panic!("planned {:?}", kinds(&cs)),
                Err(errors) => errors
                    .iter()
                    .filter(|e| matches!(e, DiffError::PartitionedTableChange { .. }))
                    .map(ToString::to_string)
                    .collect(),
            }
        };

        let created = outcome(&Schema::default(), &tree, &[]).expect("a tree is created");
        let order: Vec<String> = created
            .changes
            .iter()
            .filter_map(|p| match &p.change {
                Change::CreateTable { name, .. } => Some(name.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(order[0], "app.ev", "{order:?}");
        assert_eq!(order.len(), 3, "{order:?}");
        // Unchanged, nothing to plan.
        assert!(outcome(&tree, &tree, &[]).unwrap().changes.is_empty());

        let mut column = tree.clone();
        column
            .tables
            .get_mut(&"app.ev".parse().unwrap())
            .unwrap()
            .columns
            .insert("note".into(), Column::new(ty("int")));
        let mut bound = tree.clone();
        bound.tables.insert(
            "app.a_old".parse().unwrap(),
            partition(range("2023-01-01", "2025-01-01")),
        );
        let mut more = tree.clone();
        more.tables.insert(
            "app.m_more".parse().unwrap(),
            partition(range("2025-01-01", "2026-01-01")),
        );
        let mut fewer = tree.clone();
        fewer
            .tables
            .remove(&"app.z_rest".parse::<TableName>().unwrap());
        let drop = [Intent::DropTable {
            table: "app.z_rest".parse().unwrap(),
            reason: "gone".into(),
        }];
        // A partition added under the standing parent is its creation.
        let added = outcome(&tree, &more, &[]).expect("a partition is added");
        assert_eq!(kinds(&added), ["CreateTable"]);
        // A partition removed without intent is refused as any table is.
        let tree_ids = crate::resolve(&tree, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let silent = crate::resolve(&fewer, &tree_ids, &[], &ctx())
            .expect_err("a drop without intent is refused");
        assert!(
            format!("{silent:?}").contains("DropTableNeedsReason"),
            "{silent:?}"
        );
        // A partition dropped with intent is detached from its parent first.
        let dropped = outcome(&tree, &fewer, &drop).expect("a partition is dropped");
        assert!(
            dropped.changes.iter().any(|p| matches!(
                &p.change,
                Change::DropTable { name, detach_from: Some(parent), .. }
                    if name.to_string() == "app.z_rest" && parent.to_string() == "app.ev"
            )),
            "{:?}",
            dropped.changes
        );
        let mut renamed = tree.clone();
        let moved = renamed
            .tables
            .remove(&"app.z_rest".parse::<TableName>().unwrap())
            .unwrap();
        renamed.tables.insert("app.z_other".parse().unwrap(), moved);
        let rename = [Intent::RenameTable {
            from: "app.z_rest".parse().unwrap(),
            to: "app.z_other".parse().unwrap(),
        }];
        // The whole tree gone: the parent's own drop is still refused.
        let no_parent = Schema::default();
        let drop_all = [
            Intent::DropTable {
                table: "app.ev".parse().unwrap(),
                reason: "gone".into(),
            },
            Intent::DropTable {
                table: "app.a_old".parse().unwrap(),
                reason: "gone".into(),
            },
            Intent::DropTable {
                table: "app.z_rest".parse().unwrap(),
                reason: "gone".into(),
            },
        ];
        // A parent's column is the parent's change, which the engine
        // recurses (#1687).
        assert_eq!(
            kinds(&outcome(&tree, &column, &[]).expect("a parent's column")),
            ["AddColumn"]
        );
        for (declared, intents, expected) in [
            (&bound, &[][..], "change its partitioning"),
            (&renamed, &rename[..], "rename table"),
            (&no_parent, &drop_all[..], "app.ev is a partitioned table"),
        ] {
            let found = refused(&tree, declared, intents);
            assert!(
                found.iter().any(|e| e.contains(expected)),
                "{expected}: {found:?}"
            );
        }

        // Negative: a new table beside the tree, referencing its parent, is
        // planned.
        let mut beside = tree.clone();
        let mut referencing = table(&[
            ("ev_id", Column::new(ty("int"))),
            ("ev_ts", Column::new(ty("date"))),
        ]);
        referencing.foreign_keys.insert(
            "fk_ev".into(),
            pbps_model::ForeignKey {
                columns: vec!["ev_id".into(), "ev_ts".into()],
                references_table: "app.ev".parse().unwrap(),
                references_columns: vec!["id".into(), "ts".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
        beside.tables.insert("app.r".parse().unwrap(), referencing);
        let planned = outcome(&tree, &beside, &[]).expect("a referencing table is planned");
        assert!(
            kinds(&planned).contains(&"CreateTable".to_owned()),
            "{:?}",
            kinds(&planned)
        );
    }

    /// An ordinary table declared as a partition of a standing parent is
    /// attached and keeps its rows (#1545, DEC-1545.1). Its matching key,
    /// foreign key and index become the parent's clones and the parent's
    /// check its inherited copy, so none of them is planned; what it keeps of
    /// its own is brought to the declaration after the attach, a column with
    /// no default taking its parent's back. Its column uids leave the ids
    /// file, and its parent's stay. Every shape the engine would refuse, or
    /// would attach into something the model does not hold, is refused by
    /// name before anything is planned.
    #[test]
    fn an_ordinary_table_declared_as_a_partition_is_attached() {
        use pbps_model::{BoundDatum, PartitionBound, PartitionBy, PartitionColumn, PartitionOf};
        let index_on = |column: &str| Index {
            columns: vec![pbps_model::IndexColumn {
                key: pbps_model::IndexKey::Column(column.to_owned()),
                descending: false,
                opclass: None,
            }],
            include: Vec::new(),
            unique: false,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
        };
        let fk = pbps_model::ForeignKey {
            columns: vec!["n".into()],
            references_table: "app.r".parse().unwrap(),
            references_columns: vec!["id".into()],
            on_delete: pbps_model::ReferentialAction::NoAction,
            on_update: pbps_model::ReferentialAction::NoAction,
        };
        let check = |expression: &str| pbps_model::CheckConstraint {
            expression: expression.into(),
        };
        let key = |name: &str, columns: &[&str]| PrimaryKey {
            name: Some(name.into()),
            columns: columns.iter().map(|c| (*c).to_owned()).collect(),
            storage_parameters: Default::default(),
        };
        let columns = || {
            table(&[
                ("id", Column::new(ty("int")).not_null()),
                ("ts", Column::new(ty("date")).not_null()),
                ("n", Column::new(ty("int"))),
            ])
        };
        let mut parent = columns();
        parent.columns["n"].default = Some("0".into());
        parent.primary_key = Some(key("ev_pk", &["id", "ts"]));
        parent.checks.insert("ev_n_ck".into(), check("n > 0"));
        parent.indexes.insert("ev_n".into(), index_on("n"));
        parent.foreign_keys.insert("ev_n_fk".into(), fk.clone());
        let unique = |fillfactor: Option<&str>| pbps_model::UniqueConstraint {
            columns: vec!["n".into(), "ts".into()],
            storage_parameters: fillfactor
                .map(|f| ("fillfactor".to_owned(), f.to_owned()))
                .into_iter()
                .collect(),
        };
        parent.unique.insert("ev_u".into(), unique(None));
        parent.partition_by = Some(PartitionBy {
            columns: vec!["ts".into()],
        });
        let range = |from: &str, to: &str| PartitionBound::Range {
            from: vec![BoundDatum::Value(from.into())],
            to: vec![BoundDatum::Value(to.into())],
        };
        let mut tree = schema_of("app.ev", parent);
        let mut referenced = table(&[("id", Column::new(ty("int")).not_null())]);
        referenced.primary_key = Some(key("r_pk", &["id"]));
        tree.tables.insert("app.r".parse().unwrap(), referenced);
        tree.tables.insert(
            "app.ev_2025".parse().unwrap(),
            Table {
                partition_of: Some(PartitionOf {
                    parent: "app.ev".parse().unwrap(),
                    bound: range("2025-01-01", "2026-01-01"),
                    columns: Default::default(),
                }),
                ..Default::default()
            },
        );
        // The table as it stands: the parent's columns in the parent's
        // order, `n` NOT NULL of its own and without the parent's default,
        // the parent's key, foreign key, check and index under names of its
        // own, plus a check and an index of its own.
        let ordinary = |f: &dyn Fn(&mut Table)| {
            let mut t = columns();
            t.columns["n"].nullable = false;
            t.primary_key = Some(key("t_pk", &["id", "ts"]));
            t.foreign_keys.insert("t_fk".into(), fk.clone());
            t.unique.insert("t_u".into(), unique(None));
            t.checks.insert("ev_n_ck".into(), check("n > 0"));
            t.checks.insert("t_small".into(), check("n < 100"));
            t.indexes.insert("t_n".into(), index_on("n"));
            t.indexes.insert("t_id".into(), index_on("id"));
            f(&mut t);
            let mut base = tree.clone();
            base.tables.insert("app.t".parse().unwrap(), t);
            base
        };
        // Declared as a partition keeping its own check and NOT NULL, and
        // not its own index.
        let attached = |bound: PartitionBound| {
            let mut declared = tree.clone();
            declared.tables.insert(
                "app.t".parse().unwrap(),
                Table {
                    checks: [("t_small".to_owned(), check("n < 100"))].into(),
                    partition_of: Some(PartitionOf {
                        parent: "app.ev".parse().unwrap(),
                        bound,
                        columns: [(
                            "n".to_owned(),
                            PartitionColumn {
                                default: None,
                                not_null: true,
                            },
                        )]
                        .into(),
                    }),
                    ..Default::default()
                },
            );
            declared
        };
        let declared = attached(range("2024-01-01", "2025-01-01"));
        let outcome = |base: &Schema, declared: &Schema, intents: &[Intent]| {
            let base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
                .unwrap()
                .ids;
            let declared_ids = crate::resolve(declared, &base_ids, intents, &ctx())
                .unwrap()
                .ids;
            let cs = diff(
                Side {
                    schema: base,
                    ids: &base_ids,
                },
                Side {
                    schema: declared,
                    ids: &declared_ids,
                },
                &MinimalDialect,
                &Hints::default(),
            );
            (base_ids, declared_ids, cs)
        };
        let refused = |base: &Schema, declared: &Schema, intents: &[Intent]| -> Vec<String> {
            match outcome(base, declared, intents).2 {
                Ok(cs) => panic!("planned {:?}", kinds(&cs)),
                Err(errors) => errors.iter().map(ToString::to_string).collect(),
            }
        };

        let base = ordinary(&|_| {});
        let (base_ids, declared_ids, planned) = outcome(&base, &declared, &[]);
        let planned = planned.expect("an attach is planned");
        assert_eq!(
            kinds(&planned),
            ["DropIndex", "AttachPartition", "SetPartitionDefault"],
            "{:?}",
            planned.changes
        );
        let Change::AttachPartition {
            table: t,
            parent: p,
            bound,
            shape,
            ..
        } = &planned.changes[1].change
        else {
            panic!("{:?}", planned.changes)
        };
        assert_eq!(
            (t.to_string(), p.to_string()),
            ("app.t".into(), "app.ev".into())
        );
        assert_eq!(*bound, range("2024-01-01", "2025-01-01"));
        assert_eq!(
            **shape,
            declared.tables[&"app.t".parse::<TableName>().unwrap()]
        );
        assert!(matches!(
            &planned.changes[0].change,
            Change::DropIndex { name, .. } if name == "t_id"
        ));
        assert!(matches!(
            &planned.changes[2].change,
            Change::SetPartitionDefault { column, from: None, to: None, fallback: Some(f), .. }
                if column == "n" && f == "0"
        ));
        assert!(planned.changes[1].risks.contains(&RiskClass::Constraint));
        // The table keeps its uid, its columns' leave, and the parent's stay.
        let on = |ids: &IdsFile, name: &str| -> Vec<Uid> {
            ids.columns
                .iter()
                .filter(|(_, c)| c.table.to_string() == name)
                .map(|(u, _)| u.clone())
                .collect()
        };
        assert_eq!(on(&base_ids, "app.t").len(), 3);
        assert!(on(&declared_ids, "app.t").is_empty());
        assert_eq!(on(&base_ids, "app.ev"), on(&declared_ids, "app.ev"));
        assert_eq!(
            base_ids
                .tables
                .iter()
                .find(|(_, n)| n.to_string() == "app.t")
                .map(|(u, _)| u),
            declared_ids
                .tables
                .iter()
                .find(|(_, n)| n.to_string() == "app.t")
                .map(|(u, _)| u),
        );

        // A default of its own is kept, and none is asked of the parent.
        let own_default = ordinary(&|t| t.columns["n"].default = Some("7".into()));
        let mut keeps = declared.clone();
        keeps
            .tables
            .get_mut(&"app.t".parse::<TableName>().unwrap())
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap()
            .columns
            .get_mut("n")
            .unwrap()
            .default = Some("7".into());
        let kept = outcome(&own_default, &keeps, &[])
            .2
            .expect("its own default is kept");
        assert_eq!(kinds(&kept), ["DropIndex", "AttachPartition"]);

        // A key or an index of the parent's under other storage parameters
        // is adopted all the same, as the engine adopts it.
        let tuned = ordinary(&|t| {
            t.unique.insert("t_u".into(), unique(Some("70")));
            t.indexes.get_mut("t_n").unwrap().storage_parameters =
                [("fillfactor".to_owned(), "60".to_owned())].into();
        });
        let adopted = outcome(&tuned, &declared, &[]).2.expect("adopted");
        // An index of its own declared under the name of one the attach
        // would adopt, as another index or the same: the adopted one goes
        // before the attach, which builds the parent's clone under a name of
        // the engine's, and the declared one is added after.
        for definition in [index_on("id"), index_on("n")] {
            let mut reusing = declared.clone();
            reusing
                .tables
                .get_mut(&"app.t".parse::<TableName>().unwrap())
                .unwrap()
                .indexes
                .insert("t_n".into(), definition);
            let reclaimed = outcome(&base, &reusing, &[]).2.expect("the name is freed");
            let order: Vec<String> = reclaimed
                .changes
                .iter()
                .map(|p| match &p.change {
                    Change::DropIndex { name, .. } => format!("drop {name}"),
                    Change::AddIndex { name, .. } => format!("add {name}"),
                    other => format!("{other:?}").split(' ').next().unwrap().to_owned(),
                })
                .collect();
            assert_eq!(
                order,
                [
                    "drop t_id",
                    "drop t_n",
                    "AttachPartition",
                    "SetPartitionDefault",
                    "add t_n"
                ],
                "{:?}",
                reclaimed.changes
            );
        }
        // The attach validates the parent's foreign key over the rows it
        // brings, so it waits for the rows the plan writes into the table that
        // key references, and goes before the rows of a table referencing
        // the parent; its own changes after it (#1642 review).
        let with_rows = |schema: &Schema| {
            let mut schema = schema.clone();
            let rows = |entries: Vec<(&str, Row)>| pbps_model::TableData {
                mode: pbps_model::DataMode::Ensure,
                rows: entries
                    .into_iter()
                    .map(|(k, row)| (pbps_model::RowKey::from(k), row))
                    .collect(),
            };
            schema
                .tables
                .get_mut(&"app.r".parse::<TableName>().unwrap())
                .unwrap()
                .data = Some(rows(vec![("7", Row::default())]));
            let mut q = table(&[
                ("id", Column::new(ty("int")).not_null()),
                ("eid", Column::new(ty("int"))),
                ("ets", Column::new(ty("date"))),
            ]);
            q.primary_key = Some(key("q_pk", &["id"]));
            q.foreign_keys.insert(
                "q_ev".into(),
                pbps_model::ForeignKey {
                    columns: vec!["eid".into(), "ets".into()],
                    references_table: "app.ev".parse().unwrap(),
                    references_columns: vec!["id".into(), "ts".into()],
                    on_delete: pbps_model::ReferentialAction::NoAction,
                    on_update: pbps_model::ReferentialAction::NoAction,
                },
            );
            q.data = Some(rows(vec![(
                "1",
                [
                    ("eid".to_owned(), Value::Int(1)),
                    ("ets".to_owned(), Value::Text("2024-06-01".into())),
                ]
                .into_iter()
                .collect(),
            )]));
            schema.tables.insert("app.q".parse().unwrap(), q);
            schema
        };
        let mut base_with_q = with_rows(&base);
        for t in ["app.r", "app.q"] {
            let t = base_with_q
                .tables
                .get_mut(&t.parse::<TableName>().unwrap())
                .unwrap();
            t.data.as_mut().unwrap().rows.clear();
        }
        let waits = outcome(&base_with_q, &with_rows(&declared), &[])
            .2
            .expect("attached after the rows");
        let order: Vec<String> = waits
            .changes
            .iter()
            .map(|p| {
                if let Change::InsertRow { table, .. } = &p.change {
                    format!("insert {table}")
                } else {
                    format!("{:?}", p.change)
                        .split([' ', '{'])
                        .next()
                        .unwrap()
                        .to_owned()
                }
            })
            .filter(|k| k != "SetDataMode")
            .collect();
        assert_eq!(
            order,
            [
                "DropIndex",
                "insert app.r",
                "AttachPartition",
                "insert app.q",
                "SetPartitionDefault"
            ],
            "{:?}",
            waits.changes
        );
        // Negative: with no rows to wait for, it stays with the creates.
        assert_eq!(
            kinds(&outcome(&base, &declared, &[]).2.expect("attached")),
            ["DropIndex", "AttachPartition", "SetPartitionDefault"]
        );

        // A parent's plain unique index over the columns of the table's
        // unique constraint or key could take its index: both go before the
        // attach, and the engine builds the clones (#1642 review).
        let unique_on = |columns: &[&str]| Index {
            columns: columns
                .iter()
                .map(|c| pbps_model::IndexColumn {
                    key: pbps_model::IndexKey::Column((*c).to_owned()),
                    descending: false,
                    opclass: None,
                })
                .collect(),
            unique: true,
            ..index_on("n")
        };
        let contested = |schema: &Schema| {
            let mut schema = schema.clone();
            let parent = schema
                .tables
                .get_mut(&"app.ev".parse::<TableName>().unwrap())
                .unwrap();
            parent
                .indexes
                .insert("ev_n_ts".into(), unique_on(&["n", "ts"]));
            parent
                .indexes
                .insert("ev_id_ts".into(), unique_on(&["id", "ts"]));
            schema
        };
        let released = outcome(&contested(&base), &contested(&declared), &[])
            .2
            .expect("released");
        let before: Vec<String> = released
            .changes
            .iter()
            .take_while(|p| !matches!(p.change, Change::AttachPartition { .. }))
            .map(|p| {
                if let Change::DropUnique { name, .. } = &p.change {
                    format!("drop unique {name}")
                } else if let Change::SetPrimaryKey { to: None, .. } = &p.change {
                    "drop key".to_owned()
                } else if let Change::DropIndex { name, .. } = &p.change {
                    format!("drop {name}")
                } else {
                    format!("{:?}", p.change)
                }
            })
            .collect();
        assert_eq!(
            before,
            ["drop t_id", "drop unique t_u", "drop key"],
            "{:?}",
            released.changes
        );
        // Negative: with no such index, the key and constraint are adopted.
        let plain = outcome(&base, &declared, &[]).2.expect("adopted");
        assert!(
            !plain.changes.iter().any(|p| matches!(
                p.change,
                Change::DropUnique { .. } | Change::SetPrimaryKey { to: None, .. }
            )),
            "{:?}",
            plain.changes
        );

        // An attach that fills the table another attach's parent references
        // goes first, whatever the names: `app.z` into `app.r`, which `app.ev`
        // references, before `app.t` into `app.ev` (#1642 review).
        let r_partitioned = |schema: &Schema| {
            let mut schema = schema.clone();
            schema
                .tables
                .get_mut(&"app.r".parse::<TableName>().unwrap())
                .unwrap()
                .partition_by = Some(PartitionBy {
                columns: vec!["id".into()],
            });
            schema
        };
        let mut z = table(&[("id", Column::new(ty("int")).not_null())]);
        z.primary_key = Some(key("z_pk", &["id"]));
        let mut both_base = r_partitioned(&base);
        both_base.tables.insert("app.z".parse().unwrap(), z);
        let mut both_declared = r_partitioned(&declared);
        both_declared.tables.insert(
            "app.z".parse().unwrap(),
            Table {
                partition_of: Some(PartitionOf {
                    parent: "app.r".parse().unwrap(),
                    bound: PartitionBound::Range {
                        from: vec![BoundDatum::Value("0".into())],
                        to: vec![BoundDatum::Value("100".into())],
                    },
                    columns: Default::default(),
                }),
                ..Default::default()
            },
        );
        let attaches: Vec<String> = outcome(&both_base, &both_declared, &[])
            .2
            .expect("both attached")
            .changes
            .iter()
            .filter_map(|p| {
                if let Change::AttachPartition { table, .. } = &p.change {
                    Some(table.to_string())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(attaches, ["app.z", "app.t"]);

        // The engine's match: a descending index is its parent's all the
        // same, and of two matches neither is assumed adopted. One is left,
        // one the declaration does not keep, and the other goes first.
        let order_of = |cs: &ChangeSet| -> Vec<String> {
            cs.changes
                .iter()
                .filter_map(|p| {
                    if let Change::DropIndex { name, .. } = &p.change {
                        Some(format!("drop {name}"))
                    } else if let Change::AddIndex { name, .. } = &p.change {
                        Some(format!("add {name}"))
                    } else if let Change::AttachPartition { .. } = &p.change {
                        Some("attach".to_owned())
                    } else {
                        None
                    }
                })
                .collect()
        };
        let descending = ordinary(&|t| {
            t.indexes.get_mut("t_n").unwrap().columns[0].descending = true;
        });
        let planned = outcome(&descending, &declared, &[]).2.expect("adopted");
        assert_eq!(order_of(&planned), ["drop t_id", "attach"]);
        let twice = ordinary(&|t| {
            t.indexes.insert("a_n".into(), index_on("n"));
        });
        let planned = outcome(&twice, &declared, &[]).2.expect("one is left");
        assert_eq!(order_of(&planned), ["drop t_id", "drop t_n", "attach"]);
        let mut keeps_a_n = declared.clone();
        keeps_a_n
            .tables
            .get_mut(&"app.t".parse::<TableName>().unwrap())
            .unwrap()
            .indexes
            .insert("a_n".into(), index_on("n"));
        let planned = outcome(&twice, &keeps_a_n, &[])
            .2
            .expect("the other is left");
        assert_eq!(
            order_of(&planned),
            ["drop a_n", "drop t_id", "attach", "add a_n"]
        );

        // A deprecation is an annotation, not the column's shape.
        let annotated = ordinary(&|t| t.columns["n"].deprecated = Some("old".into()));
        outcome(&annotated, &declared, &[])
            .2
            .expect("a deprecated column attaches");
        assert_eq!(
            kinds(&adopted),
            ["DropIndex", "AttachPartition", "SetPartitionDefault"]
        );

        // Negative: every shape the engine refuses, or that would attach
        // into something the model does not hold, is refused by name.
        let mut r2 = table(&[
            ("t_id", Column::new(ty("int"))),
            ("t_ts", Column::new(ty("date"))),
        ]);
        r2.foreign_keys.insert(
            "r2_t".into(),
            pbps_model::ForeignKey {
                columns: vec!["t_id".into(), "t_ts".into()],
                references_table: "app.t".parse().unwrap(),
                references_columns: vec!["id".into(), "ts".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
        let mut referencing_base = ordinary(&|_| {});
        referencing_base
            .tables
            .insert("app.r2".parse().unwrap(), r2.clone());
        let mut referencing_declared = declared.clone();
        referencing_declared
            .tables
            .insert("app.r2".parse().unwrap(), r2);
        let trigger = (
            pbps_model::ModuleId::Trigger {
                on: "app.t".parse().unwrap(),
                name: "audit".to_owned(),
            },
            pbps_model::Module {
                kind: pbps_model::ModuleKind::Trigger,
                description: None,
                definition: "AFTER INSERT AS SELECT 1".to_owned(),
            },
        );
        let mut triggered_base = ordinary(&|_| {});
        triggered_base.modules.extend([trigger.clone()]);
        let mut triggered = declared.clone();
        triggered.modules.extend([trigger]);
        let mut unpartitioned = ordinary(&|_| {});
        unpartitioned
            .tables
            .get_mut(&"app.ev".parse::<TableName>().unwrap())
            .unwrap()
            .partition_by = None;
        let mut renamed = declared.clone();
        let moved = renamed
            .tables
            .remove(&"app.t".parse::<TableName>().unwrap())
            .unwrap();
        renamed.tables.insert("app.t2".parse().unwrap(), moved);
        let rename = [Intent::RenameTable {
            from: "app.t".parse().unwrap(),
            to: "app.t2".parse().unwrap(),
        }];
        let swap = |t: &mut Table| {
            let n = t.columns.shift_remove("n").unwrap();
            t.columns.shift_insert(0, "n".into(), n);
        };
        for (base, declared, intents, expected) in [
            (
                ordinary(&swap),
                &declared,
                &[][..],
                "its columns are (n, id, ts), and its parent's are (id, ts, n)",
            ),
            (
                ordinary(&|t| {
                    t.columns.insert("extra".into(), Column::new(ty("text")));
                }),
                &declared,
                &[][..],
                "its columns are (id, ts, n, extra)",
            ),
            (
                ordinary(&|t| t.columns["n"].ty = ty("bigint")),
                &declared,
                &[][..],
                "its column `n` is not its parent's in type",
            ),
            (
                ordinary(&|t| t.columns["id"].nullable = true),
                &declared,
                &[][..],
                "its column `id` is nullable",
            ),
            (
                ordinary(&|t| t.primary_key = Some(key("t_pk", &["id"]))),
                &declared,
                &[][..],
                "its primary key is not its parent's",
            ),
            (
                ordinary(&|t| {
                    t.unique.insert(
                        "t_u".into(),
                        pbps_model::UniqueConstraint {
                            columns: vec!["n".into()],
                            storage_parameters: Default::default(),
                        },
                    );
                }),
                &declared,
                &[][..],
                "its unique constraint `t_u` is not its parent's",
            ),
            (
                ordinary(&|t| {
                    t.foreign_keys.get_mut("t_fk").unwrap().on_delete =
                        pbps_model::ReferentialAction::Cascade;
                }),
                &declared,
                &[][..],
                "its foreign key `t_fk` is not its parent's",
            ),
            (
                ordinary(&|t| {
                    t.checks.remove("ev_n_ck");
                }),
                &declared,
                &[][..],
                "its parent's check `ev_n_ck` is not on it",
            ),
            (
                ordinary(&|t| {
                    t.replica_identity = Some(pbps_model::ReplicaIdentity::Full);
                }),
                &declared,
                &[][..],
                "replica identity",
            ),
            (
                ordinary(&|t| {
                    t.data = Some(pbps_model::TableData {
                        mode: pbps_model::DataMode::Exact,
                        rows: Default::default(),
                    });
                }),
                &declared,
                &[][..],
                "`data:`",
            ),
            (
                ordinary(&|_| {}),
                &attached(PartitionBound::Default),
                &[][..],
                "DEFAULT partition",
            ),
            (
                unpartitioned,
                &declared,
                &[][..],
                "is not a partitioned table before this plan",
            ),
            (
                ordinary(&|_| {}),
                &renamed,
                &rename[..],
                "renamed from app.t",
            ),
            (
                triggered_base,
                &triggered,
                &[][..],
                "the trigger `audit` is on it",
            ),
            (
                referencing_base,
                &referencing_declared,
                &[][..],
                "the foreign key `r2_t` of app.r2 references it",
            ),
        ] {
            let found = refused(&base, declared, intents);
            assert!(
                found
                    .iter()
                    .any(|e| e.contains("is attached to app.ev") && e.contains(expected)),
                "{expected}: {found:?}"
            );
        }

        // In another schema, a default of the parent's text may name another
        // schema's object: it is the table's own, and the parent's is set
        // back, under the parent's path.
        let elsewhere = |schema: &Schema| {
            let mut moved = schema.clone();
            let t = moved
                .tables
                .remove(&"app.t".parse::<TableName>().unwrap())
                .unwrap();
            moved.tables.insert("x.t".parse().unwrap(), t);
            moved
        };
        let other = elsewhere(&ordinary(&|t| t.columns["n"].default = Some("0".into())));
        let other_declared = elsewhere(&declared);
        let set_back = outcome(&other, &other_declared, &[]).2.expect("attached");
        assert!(
            set_back.changes.iter().any(|p| matches!(
                &p.change,
                Change::SetPartitionDefault { table, column, from: Some(f), to: None, .. }
                    if table.to_string() == "x.t" && column == "n" && f == "0"
            )),
            "{:?}",
            set_back.changes
        );
        // Negative: in the parent's schema the same text is the parent's.
        let same = outcome(
            &ordinary(&|t| t.columns["n"].default = Some("0".into())),
            &declared,
            &[],
        )
        .2
        .expect("attached");
        assert_eq!(kinds(&same), ["DropIndex", "AttachPartition"]);
        // A check or a generation expression the table spells otherwise than
        // its parent, as `n>0` beside `n > 0`: offline matched by name and
        // kind, the texts unseen; connected, compared in the engine's own
        // spelling of both, which is one text (#1642 review).
        let connected = |base: &Schema, declared: &Schema, read_back: &Schema| {
            let base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
                .unwrap()
                .ids;
            let declared_ids = crate::resolve(declared, &base_ids, &[], &ctx())
                .unwrap()
                .ids;
            crate::diff_read_back(
                Side {
                    schema: base,
                    ids: &base_ids,
                },
                Side {
                    schema: declared,
                    ids: &declared_ids,
                },
                &MinimalDialect,
                &Hints::default(),
                &BTreeSet::new(),
                Screen::Text,
                read_back,
            )
        };
        let respelled = ordinary(&|t| {
            t.checks.insert("ev_n_ck".into(), check("n>0"));
        });
        outcome(&respelled, &declared, &[])
            .2
            .expect("offline, matched by name");
        let mut engine = respelled.clone();
        for t in ["app.ev", "app.t"] {
            engine
                .tables
                .get_mut(&t.parse::<TableName>().unwrap())
                .unwrap()
                .checks
                .insert("ev_n_ck".into(), check("(n > 0)"));
        }
        connected(&respelled, &declared, &engine).expect("one check to the engine");
        // Negative: a read-back that spells them apart still refuses.
        let mut apart = engine.clone();
        apart
            .tables
            .get_mut(&"app.t".parse::<TableName>().unwrap())
            .unwrap()
            .checks
            .insert("ev_n_ck".into(), check("(n > 1)"));
        let e = connected(&respelled, &declared, &apart).expect_err("another check");
        assert!(
            e.iter().any(|m| m
                .to_string()
                .contains("its parent's check `ev_n_ck` is not on it")),
            "{e:?}"
        );
        // The same for a generation expression, in the parent's schema.
        let generate = |schema: &Schema, spellings: &[(&str, &str)]| {
            let mut schema = schema.clone();
            for (t, expression) in spellings {
                let mut g = Column::new(ty("int"));
                g.generated = Some(pbps_model::Generated {
                    expression: (*expression).into(),
                    stored: true,
                });
                schema
                    .tables
                    .get_mut(&t.parse::<TableName>().unwrap())
                    .unwrap()
                    .columns
                    .insert("g".into(), g);
            }
            schema
        };
        let g_respelled = generate(&base, &[("app.ev", "n + 1"), ("app.t", "n+1")]);
        let g_declared = generate(&declared, &[("app.ev", "n + 1")]);
        outcome(&g_respelled, &g_declared, &[])
            .2
            .expect("offline, matched by kind");
        let g_engine = generate(&base, &[("app.ev", "(n + 1)"), ("app.t", "(n + 1)")]);
        connected(&g_respelled, &g_declared, &g_engine).expect("one expression to the engine");
        // Column names are compared as names: `a, b` then `c` is not `a`
        // then `b, c`, though both lists join to one text.
        let names = |schema: &Schema, table: &str, columns: &[&str]| {
            let mut schema = schema.clone();
            let t = schema
                .tables
                .get_mut(&table.parse::<TableName>().unwrap())
                .unwrap();
            for c in columns {
                t.columns.insert((*c).into(), Column::new(ty("int")));
            }
            schema
        };
        let e = refused(
            &names(
                &names(&base, "app.ev", &["a, b", "c"]),
                "app.t",
                &["a", "b, c"],
            ),
            &names(&declared, "app.ev", &["a, b", "c"]),
            &[],
        );
        assert!(e.iter().any(|m| m.contains("in that order")), "{e:?}");

        // A generated column: the engine keeps the table's expression, so in
        // another schema the attach is refused, and in the parent's it plans.
        let generated = |schema: &Schema, tables: &[&str]| {
            let mut schema = schema.clone();
            for t in tables {
                let mut g = Column::new(ty("int"));
                g.generated = Some(pbps_model::Generated {
                    expression: "f(n)".into(),
                    stored: true,
                });
                schema
                    .tables
                    .get_mut(&t.parse::<TableName>().unwrap())
                    .unwrap()
                    .columns
                    .insert("g".into(), g);
            }
            schema
        };
        let g_base = generated(&base, &["app.ev", "app.t"]);
        let g_declared = generated(&declared, &["app.ev"]);
        let e = refused(&elsewhere(&g_base), &elsewhere(&g_declared), &[]);
        assert!(
            e.iter()
                .any(|m| m.contains("its column `g` is generated, and in another schema")),
            "{e:?}"
        );
        outcome(&g_base, &g_declared, &[])
            .2
            .expect("a generated column attaches in its parent's schema");
        // An expression index kept as its own is dropped before the attach
        // and built again after, in another schema and in the parent's.
        let expression = |t: &mut Table| {
            t.indexes.insert(
                "t_f".into(),
                Index {
                    columns: vec![pbps_model::IndexColumn {
                        key: pbps_model::IndexKey::Expression("f(n)".into()),
                        descending: false,
                        opclass: None,
                    }],
                    ..index_on("n")
                },
            );
        };
        let mut keeps_t_f = declared.clone();
        expression(
            keeps_t_f
                .tables
                .get_mut(&"app.t".parse::<TableName>().unwrap())
                .unwrap(),
        );
        let with_t_f = ordinary(&|t| expression(t));
        let rebuilt = outcome(&elsewhere(&with_t_f), &elsewhere(&keeps_t_f), &[])
            .2
            .expect("rebuilt");
        assert_eq!(
            order_of(&rebuilt),
            ["drop t_f", "drop t_id", "attach", "add t_f"]
        );
        // In the parent's schema too: `f(n)` and the engine's own spelling of
        // it are not told apart by text.
        let kept = outcome(&with_t_f, &keeps_t_f, &[]).2.expect("kept");
        assert_eq!(
            order_of(&kept),
            ["drop t_f", "drop t_id", "attach", "add t_f"]
        );

        // Negative: an ordinary table that stays one keeps its column uids,
        // and plans nothing.
        let (_, stay_ids, stays) = outcome(&base, &base, &[]);
        assert!(stays.expect("nothing changes").changes.is_empty());
        assert_eq!(on(&stay_ids, "app.t").len(), 3);
    }

    /// A partition declared as an ordinary table of its parent's shape is
    /// detached, under the names the declaration gives each of its parent's
    /// objects, and nothing else is planned for it (#1544). Any other shape
    /// is refused by name, and so is a detach and a rename at once. A type in
    /// another spelling, and a foreign key to a table this plan renames
    /// declared under the new name, are still the parent's shape.
    #[test]
    fn a_partition_declared_as_its_parents_shape_is_detached() {
        use pbps_model::{PartitionBound, PartitionBy, PartitionOf};
        let mut parent = table(&[
            ("id", Column::new(ty("int")).not_null()),
            ("ts", Column::new(ty("date")).not_null()),
            ("n", Column::new(ty("int"))),
        ]);
        parent.primary_key = Some(PrimaryKey {
            name: Some("ev_pk".into()),
            columns: vec!["id".into(), "ts".into()],
            storage_parameters: Default::default(),
        });
        parent.checks.insert(
            "ev_n_ck".into(),
            pbps_model::CheckConstraint {
                expression: "n > 0".into(),
            },
        );
        parent.indexes.insert(
            "ev_n".into(),
            Index {
                columns: vec![pbps_model::IndexColumn {
                    key: pbps_model::IndexKey::Column("n".to_owned()),
                    descending: false,
                    opclass: None,
                }],
                include: Vec::new(),
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        parent.foreign_keys.insert(
            "ev_n_fk".into(),
            pbps_model::ForeignKey {
                columns: vec!["n".into()],
                references_table: "app.r".parse().unwrap(),
                references_columns: vec!["id".into()],
                on_delete: pbps_model::ReferentialAction::NoAction,
                on_update: pbps_model::ReferentialAction::NoAction,
            },
        );
        parent.partition_by = Some(PartitionBy {
            columns: vec!["ts".into()],
        });
        let mut tree = schema_of("app.ev", parent.clone());
        let mut referenced = table(&[("id", Column::new(ty("int")).not_null())]);
        referenced.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        });
        tree.tables.insert("app.r".parse().unwrap(), referenced);
        tree.tables.insert(
            "app.ev_1".parse().unwrap(),
            Table {
                partition_of: Some(PartitionOf {
                    parent: "app.ev".parse().unwrap(),
                    bound: PartitionBound::Default,
                    columns: Default::default(),
                }),
                ..Default::default()
            },
        );
        let shape = |f: &dyn Fn(&mut Table)| {
            let mut t = Table {
                partition_by: None,
                ..parent.clone()
            };
            t.primary_key.as_mut().unwrap().name = None;
            let check = t.checks.remove("ev_n_ck").unwrap();
            t.checks.insert("arch_ck".into(), check);
            let index = t.indexes.remove("ev_n").unwrap();
            t.indexes.insert("arch_n".into(), index);
            f(&mut t);
            let mut declared = tree.clone();
            declared.tables.insert("app.ev_1".parse().unwrap(), t);
            declared
        };
        let outcome_in = |declared: &Schema, intents: &[Intent], dialect: &dyn Dialect| {
            let base_ids = crate::resolve(&tree, &IdsFile::default(), &[], &ctx())
                .unwrap()
                .ids;
            let declared_ids = crate::resolve(declared, &base_ids, intents, &ctx())
                .unwrap()
                .ids;
            diff(
                Side {
                    schema: &tree,
                    ids: &base_ids,
                },
                Side {
                    schema: declared,
                    ids: &declared_ids,
                },
                dialect,
                &Hints::default(),
            )
        };
        let outcome =
            |declared: &Schema, intents: &[Intent]| outcome_in(declared, intents, &MinimalDialect);
        let planned = outcome(&shape(&|_| {}), &[]).expect("a detach is planned");
        assert_eq!(planned.changes.len(), 1, "{:?}", planned.changes);
        let Change::DetachPartition {
            table: detached,
            parent: from,
            names,
            ..
        } = &planned.changes[0].change
        else {
            panic!("{:?}", planned.changes)
        };
        assert_eq!(detached.to_string(), "app.ev_1");
        assert_eq!(from.to_string(), "app.ev");
        let pairs: Vec<(DetachedKind, &str, Option<&str>)> = names
            .iter()
            .map(|n| (n.kind, n.parent.as_str(), n.name.as_deref()))
            .collect();
        assert_eq!(
            pairs,
            [
                (DetachedKind::PrimaryKey, "ev_pk", None),
                (DetachedKind::ForeignKey, "ev_n_fk", Some("ev_n_fk")),
                (DetachedKind::Check, "ev_n_ck", Some("arch_ck")),
                (DetachedKind::Index, "ev_n", Some("arch_n")),
            ]
        );

        // A detach claiming a name a dropped table's index holds runs after
        // the drop, though the drop's table sorts later by name.
        let mut handed = shape(&|t| {
            let index = t.indexes.remove("arch_n").unwrap();
            t.indexes.insert("old_n".into(), index);
        });
        handed
            .tables
            .remove(&"app.z_old".parse::<TableName>().unwrap());
        let mut with_old = tree.clone();
        let mut old = table(&[("n", Column::new(ty("int")))]);
        old.indexes
            .insert("old_n".into(), parent.indexes["ev_n"].clone());
        with_old.tables.insert("app.z_old".parse().unwrap(), old);
        let base_ids = crate::resolve(&with_old, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let handed_ids = crate::resolve(
            &handed,
            &base_ids,
            &[Intent::DropTable {
                table: "app.z_old".parse().unwrap(),
                reason: "archived".into(),
            }],
            &ctx(),
        )
        .unwrap()
        .ids;
        let planned = diff(
            Side {
                schema: &with_old,
                ids: &base_ids,
            },
            Side {
                schema: &handed,
                ids: &handed_ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .expect("a detach beside a drop");
        let kinds: Vec<&str> = planned
            .changes
            .iter()
            .map(|p| match p.change {
                Change::DetachPartition { .. } => "detach",
                Change::DropTable { .. } => "drop",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["drop", "detach"], "{:?}", planned.changes);

        // A description is prose, and differs from the parent's freely: a
        // connected base never holds one.
        let described = shape(&|t| {
            t.description = Some("last year's events".into());
            t.columns["n"].description = Some("a count".into());
        });
        let planned = outcome(&described, &[]).expect("a description is not shape");
        assert!(
            matches!(
                planned.changes.as_slice(),
                [p] if matches!(p.change, Change::DetachPartition { .. })
            ),
            "{:?}",
            planned.changes
        );

        // A type in another spelling is the parent's type, in a dialect that
        // spells both alike; one that does not tells them apart.
        let respelled = shape(&|t| t.columns["id"].ty = ty("integer"));
        let planned = outcome_in(&respelled, &[], &Aliases).expect("an alias is the same type");
        assert!(
            matches!(
                planned.changes.as_slice(),
                [p] if matches!(p.change, Change::DetachPartition { .. })
            ),
            "{:?}",
            planned.changes
        );
        let errors = outcome(&respelled, &[]).expect_err("two types are refused");
        assert!(
            errors
                .iter()
                .any(|e| e.to_string().contains("its columns are not its parent's")),
            "{errors:?}"
        );

        // A foreign key to a table this plan renames is declared under the
        // new name, and is still the parent's.
        let mut moved = shape(&|t| {
            t.foreign_keys.get_mut("ev_n_fk").unwrap().references_table = "app.r2".parse().unwrap();
        });
        let rename_r = |s: &mut Schema| {
            let r = s
                .tables
                .remove(&"app.r".parse::<TableName>().unwrap())
                .unwrap();
            s.tables.insert("app.r2".parse().unwrap(), r);
            s.tables
                .get_mut(&"app.ev".parse::<TableName>().unwrap())
                .unwrap()
                .foreign_keys
                .get_mut("ev_n_fk")
                .unwrap()
                .references_table = "app.r2".parse().unwrap();
        };
        rename_r(&mut moved);
        let planned = outcome(
            &moved,
            &[Intent::RenameTable {
                from: "app.r".parse().unwrap(),
                to: "app.r2".parse().unwrap(),
            }],
        )
        .expect("a detach beside a rename of what it references");
        let mut kinds: Vec<&str> = planned
            .changes
            .iter()
            .map(|p| match p.change {
                Change::DetachPartition { .. } => "detach",
                Change::RenameTable { .. } => "rename",
                _ => "other",
            })
            .collect();
        kinds.sort_unstable();
        assert_eq!(kinds, ["detach", "rename"], "{:?}", planned.changes);
        // Negative: the old name is no longer the parent's reference.
        let mut stale = shape(&|_| {});
        rename_r(&mut stale);
        let errors = outcome(
            &stale,
            &[Intent::RenameTable {
                from: "app.r".parse().unwrap(),
                to: "app.r2".parse().unwrap(),
            }],
        )
        .expect_err("a key still naming the old table is not the parent's");
        assert!(
            errors
                .iter()
                .any(|e| e.to_string().contains("foreign key `ev_n_fk` is missing")),
            "{errors:?}"
        );

        for (declared, expected) in [
            (
                shape(&|t| {
                    let n = t.columns.shift_remove("n").unwrap();
                    t.columns.shift_insert(0, "n".into(), n);
                }),
                "its columns are not its parent's",
            ),
            (
                shape(&|t| {
                    t.indexes.clear();
                }),
                "index `ev_n` is missing",
            ),
            (
                shape(&|t| {
                    t.unique.insert(
                        "extra".into(),
                        UniqueConstraint {
                            columns: vec!["n".into()],
                            storage_parameters: Default::default(),
                        },
                    );
                }),
                "unique constraint `extra` is not its parent's",
            ),
            (
                shape(&|t| {
                    t.data = Some(pbps_model::TableData {
                        mode: DataMode::Exact,
                        rows: Default::default(),
                    });
                }),
                "neither its parent's nor its own",
            ),
        ] {
            let errors = outcome(&declared, &[]).expect_err(expected);
            let said: Vec<String> = errors.iter().map(ToString::to_string).collect();
            assert!(
                said.iter()
                    .any(|e| e.contains("is detached from app.ev") && e.contains(expected)),
                "{expected}: {said:?}"
            );
        }

        // Negative: a detach and a rename at once is the rename's refusal.
        let mut renamed = shape(&|_| {});
        let moved = renamed
            .tables
            .remove(&"app.ev_1".parse::<TableName>().unwrap())
            .unwrap();
        renamed.tables.insert("app.ev_old".parse().unwrap(), moved);
        let errors = outcome(
            &renamed,
            &[Intent::RenameTable {
                from: "app.ev_1".parse().unwrap(),
                to: "app.ev_old".parse().unwrap(),
            }],
        )
        .expect_err("a rename of a partition is refused");
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, DiffError::PartitionedTableChange { .. })),
            "{errors:?}"
        );
    }

    /// A partition's own column defaults and NOT NULLs (#1578) are created
    /// with it, are changed on a standing partition one column and kind at a
    /// time (#1581), with its parent's default as what it takes back, and
    /// are its detached table's columns: a detach keeps them.
    #[test]
    fn a_partitions_own_column_overrides_are_its_own_through_a_detach() {
        use pbps_model::{PartitionBound, PartitionBy, PartitionColumn, PartitionOf};
        let mut parent = table(&[
            ("ts", Column::new(ty("date")).not_null()),
            ("n", Column::new(ty("int"))),
        ]);
        parent.partition_by = Some(PartitionBy {
            columns: vec!["ts".into()],
        });
        let own = |default: &str| Table {
            partition_of: Some(PartitionOf {
                parent: "app.ev".parse().unwrap(),
                bound: PartitionBound::Default,
                columns: [(
                    "n".to_owned(),
                    PartitionColumn {
                        default: Some(default.to_owned()),
                        not_null: true,
                    },
                )]
                .into_iter()
                .collect(),
            }),
            ..Default::default()
        };
        let p: TableName = "app.p".parse().unwrap();
        let mut tree = schema_of("app.ev", parent.clone());
        tree.tables.insert(p.clone(), own("7"));
        let outcome = |base: &Schema, declared: &Schema| {
            let base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
                .unwrap()
                .ids;
            let declared_ids = crate::resolve(declared, &base_ids, &[], &ctx())
                .unwrap()
                .ids;
            diff(
                Side {
                    schema: base,
                    ids: &base_ids,
                },
                Side {
                    schema: declared,
                    ids: &declared_ids,
                },
                &MinimalDialect,
                &Hints::default(),
            )
        };

        let created = outcome(&Schema::default(), &tree).expect("a tree is created");
        assert!(created.changes.iter().any(|c| matches!(
            &c.change,
            Change::CreateTable { name, table, .. } if *name == p && **table == own("7")
        )));
        assert!(outcome(&tree, &tree).expect("unchanged").changes.is_empty());

        // Changed on a standing partition: the default alone, from its own
        // to its own.
        let mut changed = tree.clone();
        changed.tables.insert(p.clone(), own("8"));
        let planned = outcome(&tree, &changed).expect("a standing partition's default changes");
        assert!(
            matches!(
                planned.changes.as_slice(),
                [c] if matches!(&c.change, Change::SetPartitionDefault {
                    table, column, from: Some(from), to: Some(to), fallback: None, ..
                } if *table == p && column == "n" && from == "7" && to == "8")
            ),
            "{:?}",
            planned.changes
        );
        // Both overrides gone: its parent's again, a default to take back
        // where the parent has one.
        let parents = |declared: &mut Schema| {
            declared
                .tables
                .get_mut(&"app.ev".parse::<TableName>().unwrap())
                .unwrap()
                .columns
                .get_mut("n")
                .unwrap()
                .default = Some("1".into());
        };
        let mut was = tree.clone();
        parents(&mut was);
        let mut dropped = was.clone();
        dropped.tables.insert(
            p.clone(),
            Table {
                partition_of: Some(PartitionOf {
                    columns: BTreeMap::new(),
                    ..own("7").partition_of.unwrap()
                }),
                ..Default::default()
            },
        );
        let planned = outcome(&was, &dropped).expect("both overrides dropped");
        let kinds: Vec<&Change> = planned.changes.iter().map(|c| &c.change).collect();
        assert!(
            matches!(
                kinds.as_slice(),
                [
                    Change::SetPartitionNotNull { not_null: false, .. },
                    Change::SetPartitionDefault { from: Some(_), to: None, fallback: Some(f), .. },
                ] if f == "1"
            ),
            "{kinds:?}"
        );
        // The parent's own default is the parent's change, which the engine
        // recurses into every partition over the partition's own default:
        // that is set again after it (#1687).
        let mut parent_default = tree.clone();
        parents(&mut parent_default);
        let planned = outcome(&tree, &parent_default).expect("a partitioned parent's default");
        let kinds: Vec<&Change> = planned.changes.iter().map(|c| &c.change).collect();
        assert!(
            matches!(
                kinds.as_slice(),
                [
                    Change::AlterColumnDefault { to: Some(d), .. },
                    Change::SetPartitionDefault { from: Some(a), to: Some(b), fallback: Some(f), .. },
                ] if d == "1" && a == "7" && b == "7" && f == "1"
            ),
            "{kinds:?}"
        );

        // Detached: its columns are the parent's with its own default and
        // NOT NULL.
        let detached = |n: Column| {
            let mut t = Table {
                partition_by: None,
                ..parent.clone()
            };
            t.columns.insert("n".into(), n);
            let mut declared = tree.clone();
            declared.tables.insert(p.clone(), t);
            declared
        };
        let mut kept = Column::new(ty("int")).not_null();
        kept.default = Some("7".into());
        let planned = outcome(&tree, &detached(kept)).expect("a detach");
        assert!(
            matches!(
                planned.changes.as_slice(),
                [c] if matches!(c.change, Change::DetachPartition { .. })
            ),
            "{:?}",
            planned.changes
        );
        // Negative: declared as the parent's, it is not what the detach keeps.
        let errors: Vec<String> = outcome(&tree, &detached(Column::new(ty("int"))))
            .expect_err("not the detached shape")
            .iter()
            .map(ToString::to_string)
            .collect();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("its columns are not its parent's")),
            "{errors:?}"
        );
        // The parent's columns changed while a partition is detached or
        // attached in the same plan: refused by name, with the two-plan
        // remedy, and not also as a detached shape (#1692 review).
        let mut kept = Column::new(ty("int")).not_null();
        kept.default = Some("7".into());
        let grown = |s: &mut Schema| {
            for t in s.tables.values_mut() {
                if t.partition_of.is_none() {
                    t.columns.insert("x".into(), Column::new(ty("int")));
                }
            }
        };
        let mut detached_grown = detached(kept.clone());
        grown(&mut detached_grown);
        // Attached as a range: the DEFAULT partition is not attached here.
        let mut ranged = tree.clone();
        if let Some(of) = ranged
            .tables
            .get_mut(&p)
            .and_then(|t| t.partition_of.as_mut())
        {
            of.bound = PartitionBound::Range {
                from: vec![pbps_model::BoundDatum::Value("2024-01-01".into())],
                to: vec![pbps_model::BoundDatum::Value("2025-01-01".into())],
            };
        }
        let mut grown_tree = ranged.clone();
        grown(&mut grown_tree);
        for (base, declared) in [
            (&tree, &detached_grown),
            (&detached(kept.clone()), &grown_tree),
        ] {
            let errors: Vec<String> = outcome(base, declared)
                .expect_err("columns changed across an attach or a detach")
                .iter()
                .map(ToString::to_string)
                .collect();
            assert!(
                errors.iter().any(|e| e.contains(
                    "while this plan attaches, detaches or drops app.p; change the columns and the \
                     partitions in separate plans"
                )) && !errors.iter().any(|e| e.contains("not its parent's")),
                "{errors:?}"
            );
        }
        // Negative: each half alone is planned.
        outcome(&ranged, &grown_tree).expect("the parent's column alone");
        outcome(&detached(kept.clone()), &ranged).expect("the attach alone");
    }

    /// A partition's own checks and indexes (#1577) are created with it, are
    /// added, dropped and redefined on a standing partition alone, its
    /// parent's still refused (#1581), and stay under their own names through
    /// a detach, which renames only the
    /// parent's clones: an own one declared otherwise is a change the detach
    /// does not make, and is refused by name.
    #[test]
    fn a_partitions_own_checks_and_indexes_are_its_own_through_a_detach() {
        use pbps_model::{PartitionBound, PartitionBy, PartitionOf};
        let index = |column: &str| Index {
            columns: vec![pbps_model::IndexColumn {
                key: pbps_model::IndexKey::Column(column.to_owned()),
                descending: false,
                opclass: None,
            }],
            include: Vec::new(),
            unique: false,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
        };
        let check = |expression: &str| pbps_model::CheckConstraint {
            expression: expression.into(),
        };
        let mut parent = table(&[
            ("ts", Column::new(ty("date")).not_null()),
            ("n", Column::new(ty("int"))),
        ]);
        parent.indexes.insert("ev_n".into(), index("n"));
        parent.partition_by = Some(PartitionBy {
            columns: vec!["ts".into()],
        });
        let mut own = Table {
            partition_of: Some(PartitionOf {
                parent: "app.ev".parse().unwrap(),
                bound: PartitionBound::Default,
                columns: Default::default(),
            }),
            ..Default::default()
        };
        own.checks.insert("p_ck".into(), check("n < 10"));
        own.indexes.insert("p_ts".into(), index("ts"));
        let p: TableName = "app.p".parse().unwrap();
        let mut tree = schema_of("app.ev", parent.clone());
        tree.tables.insert(p.clone(), own.clone());
        let outcome = |base: &Schema, declared: &Schema| {
            let base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
                .unwrap()
                .ids;
            let declared_ids = crate::resolve(declared, &base_ids, &[], &ctx())
                .unwrap()
                .ids;
            diff(
                Side {
                    schema: base,
                    ids: &base_ids,
                },
                Side {
                    schema: declared,
                    ids: &declared_ids,
                },
                &MinimalDialect,
                &Hints::default(),
            )
        };
        let said = |errors: Vec<DiffError>| -> Vec<String> {
            errors.iter().map(ToString::to_string).collect()
        };

        // Created with its own, in the one change that creates it.
        let created = outcome(&Schema::default(), &tree).expect("a tree is created");
        let made = created
            .changes
            .iter()
            .find_map(|c| match &c.change {
                Change::CreateTable { name, table, .. } if *name == p => Some(table),
                _ => None,
            })
            .expect("the partition is created");
        assert_eq!(**made, own);
        assert!(
            !created.changes.iter().any(|c| matches!(
                c.change,
                Change::AddCheck { .. } | Change::AddIndex { .. }
            ) && c.change.table() == Some(&p)),
            "{:?}",
            created.changes
        );

        // An own one added, dropped or changed on a standing partition is
        // planned on the partition alone (#1581).
        for (edit, expected) in [
            (
                &(|t: &mut Table| {
                    t.indexes.insert("p_n".into(), index("n"));
                }) as &dyn Fn(&mut Table),
                &["add index"] as &[&str],
            ),
            (
                &|t: &mut Table| {
                    t.checks.clear();
                },
                &["drop check"],
            ),
            (
                &|t: &mut Table| {
                    t.indexes.insert("p_ts".into(), index("n"));
                },
                &["drop index", "add index"],
            ),
        ] {
            let mut declared = tree.clone();
            edit(declared.tables.get_mut(&p).unwrap());
            let planned = outcome(&tree, &declared).expect("planned");
            let kinds: Vec<String> = planned
                .changes
                .iter()
                .map(|c| {
                    assert_eq!(c.change.table(), Some(&p), "{:?}", c.change);
                    change_in_words(&c.change)
                })
                .collect();
            assert_eq!(kinds, expected, "{:?}", planned.changes);
        }
        // Negative: one added to the parent is the parent's change, and one
        // the partition's own `p_ts` would be adopted into as its clone is
        // refused by name (#1688, DEC-1688.1).
        let mut declared = tree.clone();
        declared
            .tables
            .get_mut(&"app.ev".parse::<TableName>().unwrap())
            .unwrap()
            .indexes
            .insert("ev_ts".into(), index("ts"));
        let errors = said(outcome(&tree, &declared).expect_err("refused"));
        assert!(
            errors.iter().any(
                |e| e.starts_with("app.ev is a partitioned table or a partition")
                    && e.contains(&format!("which can take {p}'s `p_ts`"))
            ),
            "{errors:?}"
        );

        // Detached: the parent's index renamed to the declared name, its own
        // left alone under theirs.
        let shape = |f: &dyn Fn(&mut Table)| {
            let mut t = Table {
                partition_by: None,
                ..parent.clone()
            };
            let clone = t.indexes.remove("ev_n").unwrap();
            t.indexes.insert("arch_n".into(), clone);
            t.checks.insert("p_ck".into(), check("n < 10"));
            t.indexes.insert("p_ts".into(), index("ts"));
            f(&mut t);
            let mut declared = tree.clone();
            declared.tables.insert(p.clone(), t);
            declared
        };
        let planned = outcome(&tree, &shape(&|_| {})).expect("a detach");
        let [planned] = planned.changes.as_slice() else {
            panic!("{:?}", planned.changes)
        };
        let Change::DetachPartition { names, .. } = &planned.change else {
            panic!("{planned:?}")
        };
        let pairs: Vec<(&str, Option<&str>)> = names
            .iter()
            .map(|n| (n.parent.as_str(), n.name.as_deref()))
            .collect();
        assert_eq!(pairs, [("ev_n", Some("arch_n"))]);

        // Negative: an own one gone, renamed or changed in the declaration.
        for (edit, expected) in [
            (
                &(|t: &mut Table| {
                    t.indexes.remove("p_ts");
                }) as &dyn Fn(&mut Table),
                "its own index `p_ts` is not declared as it stands",
            ),
            (
                &|t: &mut Table| {
                    let i = t.indexes.remove("p_ts").unwrap();
                    t.indexes.insert("p_ts2".into(), i);
                },
                "its own index `p_ts` is not declared as it stands",
            ),
            (
                &|t: &mut Table| {
                    t.checks.insert("p_ck".into(), check("n < 11"));
                },
                "its own check `p_ck` is not declared as it stands",
            ),
        ] {
            let errors = said(outcome(&tree, &shape(edit)).expect_err(expected));
            assert!(errors.iter().any(|e| e.contains(expected)), "{errors:?}");
        }
    }

    /// A partition's persistence and storage parameters (#1580) are created
    /// with it, in the one change that creates it; changed on a standing
    /// partition alone (#1581), a switch to unlogged under a permanent
    /// table's key refused as a creation is; and a detach keeps them,
    /// so the table it leaves is declared with them or the plan is refused.
    #[test]
    fn a_partitions_own_persistence_and_storage_are_its_own_through_a_detach() {
        use pbps_model::{PartitionBound, PartitionBy, PartitionOf};
        let mut parent = table(&[("ts", Column::new(ty("date")).not_null())]);
        parent.partition_by = Some(PartitionBy {
            columns: vec!["ts".into()],
        });
        let own = Table {
            partition_of: Some(PartitionOf {
                parent: "app.ev".parse().unwrap(),
                bound: PartitionBound::Default,
                columns: Default::default(),
            }),
            unlogged: true,
            storage_parameters: [("fillfactor".to_owned(), "70".to_owned())].into(),
            ..Default::default()
        };
        let p: TableName = "app.p".parse().unwrap();
        let mut tree = schema_of("app.ev", parent.clone());
        tree.tables.insert(p.clone(), own.clone());
        let outcome = |base: &Schema, declared: &Schema| {
            let base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
                .unwrap()
                .ids;
            let declared_ids = crate::resolve(declared, &base_ids, &[], &ctx())
                .unwrap()
                .ids;
            diff(
                Side {
                    schema: base,
                    ids: &base_ids,
                },
                Side {
                    schema: declared,
                    ids: &declared_ids,
                },
                &MinimalDialect,
                &Hints::default(),
            )
        };
        let said = |errors: Vec<DiffError>| -> Vec<String> {
            errors.iter().map(ToString::to_string).collect()
        };

        let created = outcome(&Schema::default(), &tree).expect("a tree is created");
        let made = created
            .changes
            .iter()
            .find_map(|c| match &c.change {
                Change::CreateTable { name, table, .. } if *name == p => Some(table),
                _ => None,
            })
            .expect("the partition is created");
        assert_eq!(**made, own);
        assert!(
            !created.changes.iter().any(|c| matches!(
                c.change,
                Change::SetTablePersistence { .. } | Change::SetStorageParameters { .. }
            )),
            "{:?}",
            created.changes
        );

        // A permanent table's key to the parent reaches the unlogged
        // partition, which the engine does not refuse: refused here, created
        // with the tree or added under a standing one (#1580 review).
        let r: TableName = "app.r".parse().unwrap();
        let referencing = |unlogged: bool| {
            let mut t = table(&[("ts", Column::new(ty("date")))]);
            t.foreign_keys.insert(
                "r_ev".into(),
                ForeignKey {
                    columns: vec!["ts".into()],
                    references_table: "app.ev".parse().unwrap(),
                    references_columns: vec!["ts".into()],
                    on_delete: Default::default(),
                    on_update: Default::default(),
                },
            );
            t.unlogged = unlogged;
            t
        };
        let mut keyed = tree.clone();
        keyed.tables.insert(r.clone(), referencing(false));
        let mut standing = keyed.clone();
        standing.tables.remove(&p);
        for base in [Schema::default(), standing] {
            let errors = outcome(&base, &keyed).expect_err("refused");
            assert!(
                errors.iter().any(|e| matches!(
                    e,
                    DiffError::PermanentReferencesUnloggedPartition { table, partition, .. }
                        if *table == r && *partition == p
                )),
                "{errors:?}"
            );
        }
        // Negative: an unlogged referencing table, or a permanent partition.
        let mut unlogged_ref = tree.clone();
        unlogged_ref.tables.insert(r.clone(), referencing(true));
        outcome(&Schema::default(), &unlogged_ref).expect("an unlogged table may reference it");
        let mut permanent = keyed.clone();
        permanent.tables.get_mut(&p).unwrap().unlogged = false;
        outcome(&Schema::default(), &permanent).expect("every partition permanent");
        // And switched to unlogged on a standing partition (#1581).
        let errors = outcome(&permanent, &keyed).expect_err("refused");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                DiffError::PermanentReferencesUnloggedPartition { table, partition, .. }
                    if *table == r && *partition == p
            )),
            "{errors:?}"
        );

        // Changed on a standing partition: planned on it alone (#1581).
        for (edit, expected) in [
            (
                &(|t: &mut Table| t.unlogged = false) as &dyn Fn(&mut Table),
                "set table persistence",
            ),
            (
                &|t: &mut Table| {
                    t.storage_parameters
                        .insert("fillfactor".into(), "80".into());
                },
                "set storage parameters",
            ),
        ] {
            let mut declared = tree.clone();
            edit(declared.tables.get_mut(&p).unwrap());
            let planned = outcome(&tree, &declared).expect("planned");
            assert!(
                matches!(
                    planned.changes.as_slice(),
                    [c] if c.change.table() == Some(&p) && change_in_words(&c.change) == expected
                ),
                "{:?}",
                planned.changes
            );
        }

        // Detached and declared with them: the detach alone.
        let detached = |unlogged: bool| {
            let mut declared = tree.clone();
            declared.tables.insert(
                p.clone(),
                Table {
                    partition_by: None,
                    unlogged,
                    storage_parameters: own.storage_parameters.clone(),
                    ..parent.clone()
                },
            );
            declared
        };
        let planned = outcome(&tree, &detached(true)).expect("a detach");
        assert!(
            matches!(
                planned.changes.as_slice(),
                [c] if matches!(c.change, Change::DetachPartition { .. })
            ),
            "{:?}",
            planned.changes
        );
        // Negative: declared permanent, which the detach does not make it.
        let errors = said(outcome(&tree, &detached(false)).expect_err("refused"));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("app.p is detached") && e.contains("nor its own")),
            "{errors:?}"
        );
    }

    /// A table with `system_time` is created whole, and every change to it
    /// is refused by name until #1177: a column added, its period or history
    /// changed (which no change would carry, and would otherwise plan nothing
    /// and record the declaration as applied), the table dropped or renamed.
    /// A change to another table, even one referencing it, is not refused.
    #[test]
    fn a_table_with_system_time_is_created_but_never_changed() {
        let versioned = |retention: Option<&str>| {
            let mut t = table(&[
                ("id", Column::new(ty("int")).not_null()),
                ("vf", Column::new(ty("datetime2")).not_null()),
                ("vt", Column::new(ty("datetime2")).not_null()),
            ]);
            t.primary_key = Some(PrimaryKey {
                name: None,
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            });
            t.system_time = Some(pbps_model::SystemTime {
                start: "vf".into(),
                end: "vt".into(),
                hidden: false,
                versioning: Some(pbps_model::SystemVersioning {
                    history: "app.t_history".parse().unwrap(),
                    retention: retention.map(|r| r.parse().unwrap()),
                }),
            });
            t
        };
        let outcome = |base: &Schema, declared: &Schema, intents: &[Intent]| {
            let base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
                .unwrap()
                .ids;
            let declared_ids = crate::resolve(declared, &base_ids, intents, &ctx())
                .unwrap()
                .ids;
            diff(
                Side {
                    schema: base,
                    ids: &base_ids,
                },
                Side {
                    schema: declared,
                    ids: &declared_ids,
                },
                &MinimalDialect,
                &Hints::default(),
            )
        };
        let refused = |base: &Schema, declared: &Schema, intents: &[Intent]| -> Vec<String> {
            match outcome(base, declared, intents) {
                Ok(cs) => panic!("planned {:?}", kinds(&cs)),
                Err(errors) => errors
                    .iter()
                    .filter(|e| matches!(e, DiffError::TemporalTableChange { .. }))
                    .map(ToString::to_string)
                    .collect(),
            }
        };
        let base = schema_of("app.t", versioned(None));

        // Created whole, in one change.
        let created = outcome(&Schema::default(), &base, &[]).expect("a create is planned");
        assert_eq!(kinds(&created), ["CreateTable"]);
        // With the foreign keys the differ splits out of its `CREATE`, which
        // are the creation too (#1501 review).
        let mut keyed = base.clone();
        keyed.tables.insert(
            "app.parent".parse().unwrap(),
            table(&[("id", Column::new(ty("int")).not_null())]),
        );
        keyed
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .foreign_keys
            .insert(
                "fk_t_parent".into(),
                pbps_model::ForeignKey {
                    columns: vec!["id".into()],
                    references_table: "app.parent".parse().unwrap(),
                    references_columns: vec!["id".into()],
                    on_delete: Default::default(),
                    on_update: Default::default(),
                },
            );
        let created = outcome(&Schema::default(), &keyed, &[]).expect("a keyed create is planned");
        assert!(
            kinds(&created).contains(&"AddForeignKey".to_owned()),
            "{:?}",
            kinds(&created)
        );
        // And under the name of an ordinary table an earlier revision of the
        // same plan dropped: the new table has a uid of its own.
        let mut ordinary = keyed.clone();
        ordinary.tables.insert(
            "app.t".parse().unwrap(),
            table(&[("id", Column::new(ty("int")).not_null())]),
        );
        let mut vacated = keyed.clone();
        vacated
            .tables
            .remove(&"app.t".parse::<TableName>().unwrap());
        let base_ids = crate::resolve(&ordinary, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let between = crate::resolve(
            &vacated,
            &base_ids,
            &[Intent::DropTable {
                table: "app.t".parse().unwrap(),
                reason: "replaced".into(),
            }],
            &ctx(),
        )
        .unwrap()
        .ids;
        let reused_ids = crate::resolve(&keyed, &between, &[], &ctx()).unwrap().ids;
        let reused = diff(
            Side {
                schema: &ordinary,
                ids: &base_ids,
            },
            Side {
                schema: &keyed,
                ids: &reused_ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .expect("a create under a name an earlier revision dropped is planned");
        assert!(
            kinds(&reused).contains(&"DropTable".to_owned())
                && kinds(&reused).contains(&"AddForeignKey".to_owned()),
            "{:?}",
            kinds(&reused)
        );
        // Negative: a temporal table dropped and created again under its own
        // name is still a drop of a temporal table.
        let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let between = crate::resolve(
            &Schema::default(),
            &base_ids,
            &[Intent::DropTable {
                table: "app.t".parse().unwrap(),
                reason: "replaced".into(),
            }],
            &ctx(),
        )
        .unwrap()
        .ids;
        let again_ids = crate::resolve(&base, &between, &[], &ctx()).unwrap().ids;
        let errors = diff(
            Side {
                schema: &base,
                ids: &base_ids,
            },
            Side {
                schema: &base,
                ids: &again_ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .expect_err("a temporal table dropped and recreated is refused");
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, DiffError::TemporalTableChange { .. })),
            "{errors:?}"
        );
        // Unchanged, nothing to plan.
        assert!(outcome(&base, &base, &[]).unwrap().changes.is_empty());

        let mut added = base.clone();
        added
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .columns
            .insert("note".into(), Column::new(ty("int")).not_null());
        let retained = schema_of("app.t", versioned(Some("6 months")));
        let mut renamed_history = base.clone();
        renamed_history
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .system_time
            .as_mut()
            .unwrap()
            .versioning
            .as_mut()
            .unwrap()
            .history = "app.t_audit".parse().unwrap();
        let renamed = schema_of("app.u", versioned(None));
        for (what, declared, intents) in [
            ("a NOT NULL column added", &added, vec![]),
            ("its retention", &retained, vec![]),
            ("its history renamed", &renamed_history, vec![]),
            (
                "the table dropped",
                &Schema::default(),
                vec![Intent::DropTable {
                    table: "app.t".parse().unwrap(),
                    reason: "retired".into(),
                }],
            ),
            (
                "the table renamed",
                &renamed,
                vec![Intent::RenameTable {
                    from: "app.t".parse().unwrap(),
                    to: "app.u".parse().unwrap(),
                }],
            ),
        ] {
            let errors = refused(&base, declared, &intents);
            assert!(
                !errors.is_empty()
                    && errors
                        .iter()
                        .all(|e| e.contains("the only change pbps makes")),
                "{what}: {errors:?}"
            );
        }
        // A nullable column is the one change it takes (#1177), with or
        // without a default; the refusal names every other change it meets.
        for column in [
            Column::new(ty("int")),
            Column {
                default: Some("5".into()),
                ..Column::new(ty("int"))
            },
        ] {
            let mut grown = base.clone();
            grown
                .tables
                .get_mut(&"app.t".parse().unwrap())
                .unwrap()
                .columns
                .insert("note".into(), column);
            let cs = outcome(&base, &grown, &[]).expect("a nullable column is added");
            assert_eq!(kinds(&cs), ["AddColumn"]);
        }
        let mut mixed = retained.clone();
        let t = mixed.tables.get_mut(&"app.t".parse().unwrap()).unwrap();
        t.columns.insert("note".into(), Column::new(ty("int")));
        t.columns
            .insert("must".into(), Column::new(ty("int")).not_null());
        t.columns
            .insert("also".into(), Column::new(ty("int")).not_null());
        let errors = refused(&base, &mixed, &[]);
        assert!(
            errors.len() == 1
                && errors[0]
                    .contains("would also change its `system_time`, add a NOT NULL column.")
                && !errors[0].contains("NOT NULL column, add a NOT NULL"),
            "{errors:?}"
        );
        // Negative: an ordinary table referencing it changes freely.
        let mut referencing = base.clone();
        let mut other = table(&[("t_id", Column::new(ty("int")))]);
        other.foreign_keys.insert(
            "fk_other_t".into(),
            pbps_model::ForeignKey {
                columns: vec!["t_id".into()],
                references_table: "app.t".parse().unwrap(),
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
        referencing
            .tables
            .insert("app.other".parse().unwrap(), other);
        let cs = outcome(&base, &referencing, &[]).expect("another table is not refused");
        assert!(
            kinds(&cs).contains(&"CreateTable".to_owned()),
            "{:?}",
            kinds(&cs)
        );
    }

    /// Tables referencing each other in a cycle have no order to switch in:
    /// whichever goes first breaks the other's key. The keys inside the
    /// cycle are dropped before the switches and added back after, and the
    /// tables outside it keep their order (#1488 review).
    #[test]
    fn a_foreign_key_cycle_is_unlinked_around_its_persistence_switch() {
        let fk = |to: &str, column: &str| pbps_model::ForeignKey {
            columns: vec![column.into()],
            references_table: to.parse().unwrap(),
            references_columns: vec!["id".into()],
            on_delete: Default::default(),
            on_update: Default::default(),
        };
        let cycle = |unlogged: bool| {
            let mut s = Schema::default();
            for (name, other) in [("public.a", "public.b"), ("public.b", "public.a")] {
                let mut t = table(&[
                    ("id", Column::new(ty("int")).not_null()),
                    ("other", Column::new(ty("int"))),
                ]);
                t.primary_key = Some(PrimaryKey {
                    name: None,
                    columns: vec!["id".into()],
                    storage_parameters: Default::default(),
                });
                t.foreign_keys
                    .insert(format!("{}_other", &name[7..]), fk(other, "other"));
                t.unlogged = unlogged;
                s.tables.insert(name.parse().unwrap(), t);
            }
            s
        };
        let cs = run(&cycle(false), &cycle(true), &[]);
        let at = |pred: &dyn Fn(&Change) -> bool| -> Vec<usize> {
            cs.changes
                .iter()
                .enumerate()
                .filter(|(_, p)| pred(&p.change))
                .map(|(i, _)| i)
                .collect()
        };
        let drops = at(&|c| matches!(c, Change::DropForeignKey { .. }));
        let switches = at(&|c| matches!(c, Change::SetTablePersistence { .. }));
        let adds = at(&|c| matches!(c, Change::AddForeignKey { .. }));
        assert_eq!(
            (drops.len(), switches.len(), adds.len()),
            (2, 2, 2),
            "{:?}",
            kinds(&cs)
        );
        assert!(
            drops.iter().max() < switches.iter().min(),
            "{:?}",
            kinds(&cs)
        );
        assert!(
            switches.iter().max() < adds.iter().min(),
            "{:?}",
            kinds(&cs)
        );
        // Negative: a chain, not a cycle, keeps its keys and its order.
        let chain = |unlogged: bool| {
            let mut s = cycle(unlogged);
            s.tables
                .get_mut(&"public.a".parse::<TableName>().unwrap())
                .unwrap()
                .foreign_keys
                .clear();
            s
        };
        let cs = run(&chain(false), &chain(true), &[]);
        assert!(
            !cs.changes
                .iter()
                .any(|p| matches!(p.change, Change::DropForeignKey { .. })),
            "{:?}",
            kinds(&cs)
        );
    }

    /// One change per table, setting what differs and resetting what the
    /// declaration drops; a parameter left alone is not restated, and an
    /// unchanged set plans nothing (#1441).
    #[test]
    fn storage_parameters_set_what_differs_and_reset_what_goes() {
        let with = |pairs: &[(&str, &str)]| {
            let mut t = table(&[("id", Column::new(ty("int")).not_null())]);
            t.storage_parameters = pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            schema_of("public.t", t)
        };
        let base = with(&[
            ("fillfactor", "70"),
            ("autovacuum_enabled", "false"),
            ("parallel_workers", "2"),
        ]);
        let cs = run(
            &base,
            &with(&[
                ("fillfactor", "80"),
                ("parallel_workers", "2"),
                ("vacuum_truncate", "false"),
            ]),
            &[],
        );
        let [p] = cs.changes.as_slice() else {
            panic!("{cs:#?}");
        };
        let Change::SetStorageParameters { set, reset, .. } = &p.change else {
            panic!("{cs:#?}");
        };
        assert_eq!(
            set.iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect::<Vec<_>>(),
            [("fillfactor", "80"), ("vacuum_truncate", "false")]
        );
        assert_eq!(
            reset.iter().map(String::as_str).collect::<Vec<_>>(),
            ["autovacuum_enabled"]
        );
        // Negative: the same parameters plan nothing.
        assert!(run(&base, &base, &[]).changes.is_empty());
    }

    /// `public.t` with unique indexes `ix_code` over a NOT NULL column and
    /// `ix_v` over a nullable one, and the given identity.
    fn identified_by(identity: Option<pbps_model::ReplicaIdentity>) -> Table {
        let unique_on = |column: &str| Index {
            columns: vec![IndexColumn {
                key: pbps_model::IndexKey::Column(column.into()),
                descending: false,
                opclass: None,
            }],
            include: Vec::new(),
            unique: true,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
        };
        let mut t = table(&[
            ("id", Column::new(ty("int")).not_null()),
            ("code", Column::new(ty("int")).not_null()),
            ("v", Column::new(ty("int"))),
        ]);
        t.indexes.insert("ix_code".into(), unique_on("code"));
        t.indexes.insert("ix_v".into(), unique_on("v"));
        t.replica_identity = identity;
        t
    }

    /// The order of a plan's replica identity against its index changes, by
    /// the rule of DEC-1444.1: first, at (0, 1), where the table can take
    /// the target as it stands, which is before the old identity's index is
    /// dropped; last in class 13 where the plan adds the index it names, new
    /// or rebuilt, or makes its columns NOT NULL. Unchanged and not rebuilt,
    /// it is not planned at all.
    #[test]
    fn a_replica_identity_is_set_before_its_old_index_goes_and_after_its_new_one_comes() {
        use pbps_model::ReplicaIdentity as R;
        let shown = |cs: &ChangeSet| -> Vec<String> {
            cs.changes
                .iter()
                .map(|p| match &p.change {
                    Change::SetReplicaIdentity { table, to, .. } => format!(
                        "identity {table} {}",
                        to.as_ref()
                            .map_or("default".to_owned(), ToString::to_string)
                    ),
                    Change::AddIndex { name, .. } => format!("add {name}"),
                    Change::DropIndex { name, .. } => format!("drop {name}"),
                    Change::AlterColumnNullability { column, .. } => {
                        format!("tighten {}", column.name)
                    }
                    other => format!("{other:?}"),
                })
                .collect()
        };
        let plan = |was: Table, now: Table| {
            shown(&run(
                &schema_of("public.t", was),
                &schema_of("public.t", now),
                &[],
            ))
        };

        // The old identity's index is dropped: the identity moves first.
        let mut gone = identified_by(Some(R::Full));
        gone.indexes.remove("ix_code");
        assert_eq!(
            plan(identified_by(Some(R::Index("ix_code".into()))), gone),
            ["identity public.t full", "drop ix_code"]
        );

        // Its index rebuilt: FULL first, which the table can always take,
        // so no read between the drop and the add finds it identifying no
        // row; then dropped, added, and the identity set again (#1467
        // review).
        let mut rebuilt = identified_by(Some(R::Index("ix_code".into())));
        rebuilt.indexes.get_mut("ix_code").unwrap().columns[0].descending = true;
        assert_eq!(
            plan(identified_by(Some(R::Index("ix_code".into()))), rebuilt),
            [
                "identity public.t full",
                "drop ix_code",
                "add ix_code",
                "identity public.t {index: ix_code}"
            ]
        );

        // A new index, and one over a column this plan makes NOT NULL: after.
        let mut new = identified_by(Some(R::Index("ix_new".into())));
        new.indexes
            .insert("ix_new".into(), new.indexes["ix_code"].clone());
        assert_eq!(
            plan(identified_by(None), new),
            ["add ix_new", "identity public.t {index: ix_new}"]
        );
        let mut tightened = identified_by(Some(R::Index("ix_v".into())));
        tightened.columns.get_mut("v").unwrap().nullable = false;
        assert_eq!(
            plan(identified_by(None), tightened),
            ["tighten v", "identity public.t {index: ix_v}"]
        );

        // Negative: unchanged and not rebuilt, nothing is planned; a change
        // elsewhere in the table does not restate it.
        let mut elsewhere = identified_by(Some(R::Index("ix_code".into())));
        elsewhere.indexes.get_mut("ix_v").unwrap().columns[0].descending = true;
        assert_eq!(
            plan(identified_by(Some(R::Index("ix_code".into()))), elsewhere),
            ["drop ix_v", "add ix_v"]
        );
        assert!(plan(identified_by(Some(R::Full)), identified_by(Some(R::Full))).is_empty());
    }

    fn clustered_on(layout: Option<pbps_model::Clustered>) -> Table {
        let mut t = table(&[
            ("id", Column::new(ty("int")).not_null()),
            ("code", Column::new(ty("int")).not_null()),
            ("v", Column::new(ty("int"))),
        ]);
        t.primary_key = Some(PrimaryKey {
            name: Some("pk_t".into()),
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        });
        t.unique.insert(
            "uq_code".into(),
            UniqueConstraint {
                columns: vec!["code".into()],
                storage_parameters: Default::default(),
            },
        );
        t.indexes.insert(
            "ix_v".into(),
            Index {
                columns: vec![IndexColumn {
                    key: pbps_model::IndexKey::Column("v".into()),
                    descending: false,
                    opclass: None,
                }],
                include: Vec::new(),
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        t.clustered = layout;
        t
    }

    /// Moving the clustered index rebuilds the object that gives it up and
    /// the one that takes it, each with its new layout, and nothing else;
    /// the new clustered index is built before any other addition, so the
    /// key re-added nonclustered is not built twice.
    #[test]
    fn a_layout_change_rebuilds_exactly_the_two_objects_it_moves_between() {
        use pbps_model::Clustered;
        let cs = run(
            &schema_of("dbo.t", clustered_on(None)),
            &schema_of("dbo.t", clustered_on(Some(Clustered::Index("ix_v".into())))),
            &[],
        );
        let shown: Vec<String> = cs
            .changes
            .iter()
            .map(|p| match &p.change {
                Change::SetPrimaryKey {
                    from,
                    to,
                    nonclustered,
                    ..
                } => format!(
                    "pk {}->{} nonclustered={nonclustered}",
                    from.is_some(),
                    to.is_some()
                ),
                Change::AddIndex {
                    name, clustered, ..
                } => format!("add {name} clustered={clustered}"),
                Change::DropIndex { name, .. } => format!("drop {name}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            shown,
            [
                "drop ix_v",
                "pk true->false nonclustered=false",
                "add ix_v clustered=true",
                "pk false->true nonclustered=true",
            ]
        );

        // And back: the key takes it again, and the index is rebuilt plain —
        // after the key, although `AddIndex` sorts ahead of `SetPrimaryKey`
        // everywhere else in the addition class.
        let cs = run(
            &schema_of("dbo.t", clustered_on(Some(Clustered::Index("ix_v".into())))),
            &schema_of("dbo.t", clustered_on(None)),
            &[],
        );
        let key = cs.changes.iter().position(|p| {
            matches!(
                &p.change,
                Change::SetPrimaryKey {
                    to: Some(_),
                    nonclustered: false,
                    ..
                }
            )
        });
        let index = cs.changes.iter().position(|p| {
            matches!(
                &p.change,
                Change::AddIndex {
                    clustered: false,
                    ..
                }
            )
        });
        assert!(
            matches!((key, index), (Some(k), Some(i)) if k < i),
            "{cs:?}"
        );
        assert!(!kinds(&cs).iter().any(|k| k.contains("Unique")), "{cs:?}");

        // Negative: an unchanged layout is no change at all.
        for layout in [
            None,
            Some(Clustered::Heap),
            Some(Clustered::Unique("uq_code".into())),
        ] {
            let t = schema_of("dbo.t", clustered_on(layout.clone()));
            assert!(run(&t, &t, &[]).changes.is_empty(), "{layout:?}");
        }
    }

    /// A key that is only dropped claims no layout, whatever the declaration
    /// says about the table: `nonclustered` describes the key a change adds.
    /// PostgreSQL's emitter refuses a change that claims one, so a drop that
    /// did would stop every plan giving up a key there.
    #[test]
    fn a_key_that_is_only_dropped_claims_no_layout() {
        let base = clustered_on(None);
        let mut declared = base.clone();
        declared.primary_key = None;
        let cs = run(
            &schema_of("dbo.t", base),
            &schema_of("dbo.t", declared),
            &[],
        );
        assert!(
            cs.changes.iter().any(|p| matches!(
                &p.change,
                Change::SetPrimaryKey {
                    to: None,
                    nonclustered: false,
                    ..
                }
            )),
            "{cs:?}"
        );
        assert!(
            !cs.changes.iter().any(|p| matches!(
                &p.change,
                Change::SetPrimaryKey {
                    nonclustered: true,
                    ..
                }
            )),
            "{cs:?}"
        );
    }

    /// A key that stays but becomes nonclustered is still a different
    /// index, so the foreign keys bound to it are dropped and re-added
    /// around it, as they are for any replaced key.
    #[test]
    fn a_key_moved_to_the_heap_takes_its_foreign_keys_through_the_rebuild() {
        let mut base = schema_of("dbo.t", clustered_on(None));
        let mut child = table(&[
            ("id", Column::new(ty("int")).not_null()),
            ("t_id", Column::new(ty("int"))),
        ]);
        child.foreign_keys.insert(
            "fk_child_t".into(),
            ForeignKey {
                columns: vec!["t_id".into()],
                references_table: "dbo.t".parse().unwrap(),
                references_columns: vec!["id".into()],
                on_delete: ReferentialAction::NoAction,
                on_update: ReferentialAction::NoAction,
            },
        );
        base.tables.insert("dbo.child".parse().unwrap(), child);
        let mut declared = base.clone();
        declared
            .tables
            .get_mut(&"dbo.t".parse::<TableName>().unwrap())
            .unwrap()
            .clustered = Some(pbps_model::Clustered::Heap);
        let k = kinds(&run(&base, &declared, &[]));
        assert_eq!(
            k,
            [
                "DropForeignKey",
                "SetPrimaryKey",
                "SetPrimaryKey",
                "AddForeignKey"
            ]
        );
    }

    /// A renamed clustered index is a drop and an add under the new name,
    /// and the selector written with the new name makes the new one the
    /// clustered one; the key, nonclustered on both sides, is not touched.
    #[test]
    fn a_renamed_clustered_index_is_rebuilt_clustered_under_its_new_name() {
        use pbps_model::Clustered;
        let base = clustered_on(Some(Clustered::Index("ix_v".into())));
        let mut declared = base.clone();
        let index = declared.indexes.remove("ix_v").unwrap();
        declared.indexes.insert("cx_v".into(), index);
        declared.clustered = Some(Clustered::Index("cx_v".into()));
        let cs = run(
            &schema_of("dbo.t", base),
            &schema_of("dbo.t", declared),
            &[],
        );
        let shown: Vec<String> = cs
            .changes
            .iter()
            .map(|p| match &p.change {
                Change::AddIndex {
                    name, clustered, ..
                } => format!("add {name} clustered={clustered}"),
                Change::DropIndex { name, .. } => format!("drop {name}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(shown, ["drop ix_v", "add cx_v clustered=true"]);
    }

    /// A key that references itself goes through its own rebuild: the
    /// foreign key is dropped before the key and re-added after it.
    #[test]
    fn a_self_referencing_key_rebuilt_for_its_layout_keeps_its_foreign_key() {
        let mut t = clustered_on(None);
        t.columns.insert("parent".into(), Column::new(ty("int")));
        t.foreign_keys.insert(
            "fk_t_parent".into(),
            ForeignKey {
                columns: vec!["parent".into()],
                references_table: "dbo.t".parse().unwrap(),
                references_columns: vec!["id".into()],
                on_delete: ReferentialAction::NoAction,
                on_update: ReferentialAction::NoAction,
            },
        );
        let mut heap = t.clone();
        heap.clustered = Some(pbps_model::Clustered::Heap);
        let k = kinds(&run(&schema_of("dbo.t", t), &schema_of("dbo.t", heap), &[]));
        assert_eq!(
            k,
            [
                "DropForeignKey",
                "SetPrimaryKey",
                "SetPrimaryKey",
                "AddForeignKey"
            ]
        );
    }

    /// Why a comparison against a live database has to identify the live side
    /// by what is there (`crate::observed_ids`), not by the mapping the
    /// declarations carry.
    ///
    /// Matching is by uid. Handing both sides the same identity file makes
    /// every uid present on both, so a table one side does not hold is not an
    /// addition — it is a pair whose model lookup fails, and the loop skips it.
    /// The two findings that matter most (something the plan failed to create,
    /// something a hand added) are exactly the two that vanish. `plan --dev`
    /// and the drift check both depend on this.
    #[test]
    fn a_shared_identity_file_hides_what_one_side_is_missing() {
        let declared = schema_of("dbo.t", table(&[("id", Column::new(ty("int")))]));
        let ids = crate::resolve(&declared, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        // The engine built nothing at all.
        let engine = Schema::default();

        let shared = diff(
            Side {
                schema: &engine,
                ids: &ids,
            },
            Side {
                schema: &declared,
                ids: &ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .unwrap();
        assert!(
            shared.changes.is_empty(),
            "the trap this test exists for has moved: {:?}",
            kinds(&shared)
        );

        let observed = crate::observed_ids(&engine, &ids);
        let honest = diff(
            Side {
                schema: &engine,
                ids: &observed,
            },
            Side {
                schema: &declared,
                ids: &ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .unwrap();
        assert_eq!(kinds(&honest), vec!["CreateTable"]);
    }

    // ---- Reference data (ADR-0004) ----

    fn lookup(mode: DataMode, rows: &[(&str, &str)]) -> Table {
        let mut t = table(&[
            ("code", Column::new(ty("varchar(20)")).not_null()),
            ("label", Column::new(ty("nvarchar(50)"))),
        ]);
        t.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["code".to_owned()],
            storage_parameters: Default::default(),
        });
        t.data = Some(pbps_model::TableData {
            mode,
            rows: rows
                .iter()
                .map(|(k, label)| {
                    (
                        pbps_model::RowKey::from(*k),
                        [("label".to_owned(), Value::Text((*label).to_owned()))]
                            .into_iter()
                            .collect::<Row>(),
                    )
                })
                .collect(),
        });
        t
    }

    /// A tightening of a table whose rows this plan writes or deletes runs
    /// after them, since they may be what fills or removes its NULLs, and a
    /// tightening folded into a retype comes out of it to do so. A table
    /// without row changes keeps its tightening among the column alterations,
    /// whatever another table's rows do (#1367).
    #[test]
    #[allow(clippy::wildcard_enum_match_arm)]
    fn a_tightening_runs_after_the_rows_of_its_table() {
        let required = |mut t: Table, ty_: &str| {
            let label = t.columns.get_mut("label").expect("the lookup's label");
            label.ty = ty(ty_);
            label.nullable = false;
            t
        };
        let order = |cs: &ChangeSet| -> Vec<String> {
            cs.changes
                .iter()
                .filter_map(|p| match &p.change {
                    Change::AlterColumnNullability { column, .. } => {
                        Some(format!("tighten {}", column.table))
                    }
                    Change::AlterColumnType {
                        column,
                        to_nullable,
                        ..
                    } => Some(format!("retype {} nullable={to_nullable}", column.table)),
                    Change::UpdateRow { table, .. } => Some(format!("update {table}")),
                    Change::DeleteRow { table, .. } => Some(format!("delete {table}")),
                    _ => None,
                })
                .collect()
        };
        // An update fills the NULL: the tightening follows it.
        let base = schema_of("dbo.s", lookup(DataMode::Ensure, &[("a", "x")]));
        let filled = schema_of(
            "dbo.s",
            required(lookup(DataMode::Ensure, &[("a", "y")]), "nvarchar(50)"),
        );
        assert_eq!(
            order(&run(&base, &filled, &[])),
            ["update dbo.s", "tighten dbo.s"]
        );
        // An exact table deletes the row that held it: after the delete.
        let base = schema_of("dbo.s", lookup(DataMode::Exact, &[("a", "x"), ("b", "y")]));
        let removed = schema_of(
            "dbo.s",
            required(lookup(DataMode::Exact, &[("a", "x")]), "nvarchar(50)"),
        );
        assert_eq!(
            order(&run(&base, &removed, &[])),
            ["delete dbo.s", "tighten dbo.s"]
        );
        // Folded into a retype, the tightening comes out of it and follows the
        // rows; the retype keeps the column nullable.
        let widened = schema_of(
            "dbo.s",
            required(lookup(DataMode::Ensure, &[("a", "y")]), "nvarchar(80)"),
        );
        let base = schema_of("dbo.s", lookup(DataMode::Ensure, &[("a", "x")]));
        assert_eq!(
            order(&run(&base, &widened, &[])),
            [
                "retype dbo.s nullable=true",
                "update dbo.s",
                "tighten dbo.s"
            ]
        );
        // Negative: another table's rows move nothing. `dbo.s` writes no row,
        // so its tightening stays in class 9, ahead of `dbo.o`'s update, and a
        // retype keeps its tightening folded in.
        let pair = |s: Table, o: Table| {
            let mut schema = schema_of("dbo.s", s);
            schema.tables.insert("dbo.o".parse().unwrap(), o);
            schema
        };
        let base = pair(
            lookup(DataMode::Ensure, &[("a", "x")]),
            lookup(DataMode::Ensure, &[("a", "x")]),
        );
        let declared = pair(
            required(lookup(DataMode::Ensure, &[("a", "x")]), "nvarchar(50)"),
            lookup(DataMode::Ensure, &[("a", "y")]),
        );
        assert_eq!(
            order(&run(&base, &declared, &[])),
            ["tighten dbo.s", "update dbo.o"]
        );
        let declared = pair(
            required(lookup(DataMode::Ensure, &[("a", "x")]), "nvarchar(80)"),
            lookup(DataMode::Ensure, &[("a", "y")]),
        );
        assert_eq!(
            order(&run(&base, &declared, &[])),
            ["retype dbo.s nullable=false", "update dbo.o"]
        );
    }

    /// A column whose tightening was split out of its retype carries two
    /// changes, and the dependents both bring down are rebuilt: the check a
    /// collation change takes down, and the unique key a tightening does
    /// (#1363, #1367).
    #[test]
    fn a_split_tightening_keeps_the_dependents_of_its_retype() {
        let shaped = |collation: Option<&str>, nullable: bool, label: &str| {
            let mut t = lookup(DataMode::Ensure, &[("a", label)]);
            let column = t.columns.get_mut("label").expect("the lookup's label");
            column.collation = collation.map(pbps_model::Collation::new);
            column.nullable = nullable;
            t.unique.insert(
                "uq_label".into(),
                UniqueConstraint {
                    columns: vec!["label".into()],
                    storage_parameters: Default::default(),
                },
            );
            t.checks.insert(
                "ck_label".into(),
                CheckConstraint {
                    expression: "label <> ''".into(),
                },
            );
            schema_of("dbo.s", t)
        };
        let base = shaped(None, true, "x");
        let declared = shaped(Some("Latin1_General_CS_AS"), false, "y");
        let k = kinds(&run_with(&Recollates, &base, &declared, &[]));
        for kind in [
            "AlterColumnType",
            "AlterColumnNullability",
            "DropCheck",
            "AddCheck",
            "DropUnique",
            "AddUnique",
        ] {
            assert!(k.contains(&kind.to_owned()), "{kind}: {k:?}");
        }
    }

    fn row_ops(cs: &ChangeSet) -> Vec<String> {
        cs.changes
            .iter()
            .filter_map(|p| match &p.change {
                Change::InsertRow { key, .. } => Some(format!("insert {key}")),
                Change::UpdateRow { key, columns, .. } => Some(format!(
                    "update {key} [{}]",
                    columns.keys().cloned().collect::<Vec<_>>().join(",")
                )),
                Change::DeleteRow { key, cause, .. } => Some(format!("delete {key} {cause:?}")),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_declared_row_that_is_not_there_is_inserted() {
        let base = schema_of("dbo.s", lookup(DataMode::Exact, &[]));
        let declared = schema_of("dbo.s", lookup(DataMode::Exact, &[("new", "New")]));
        assert_eq!(row_ops(&run(&base, &declared, &[])), ["insert new"]);
    }

    #[test]
    fn only_the_columns_that_differ_reach_the_update() {
        let base = schema_of(
            "dbo.s",
            lookup(DataMode::Exact, &[("new", "New"), ("shipped", "Shipped")]),
        );
        let declared = schema_of(
            "dbo.s",
            lookup(
                DataMode::Exact,
                &[("new", "Opened"), ("shipped", "Shipped")],
            ),
        );
        // `shipped` is untouched, and `new` restates only `label`: an UPDATE
        // that also set the columns which agree would overwrite values nobody
        // asked to change.
        assert_eq!(row_ops(&run(&base, &declared, &[])), ["update new [label]"]);
    }

    /// The cells the plan leaves alone travel beside the changed ones, with
    /// their base types, so the emitter can hold the whole declared row —
    /// never restate it (DECISIONS 136). A column the row omits is carried as
    /// the default it resolves to.
    #[test]
    fn an_update_carries_the_cells_it_leaves_alone() {
        let mut base_t = lookup(DataMode::Exact, &[("new", "New")]);
        base_t
            .columns
            .insert("note".to_owned(), Column::new(ty("nvarchar(50)")));
        let mut sort = Column::new(ty("int"));
        sort.default = Some("(0)".to_owned());
        base_t.columns.insert("sort".to_owned(), sort);
        // The engine's own: never read back, so never a cell to hold the
        // row to (DECISIONS 94) — holding it to NULL would refuse every
        // update on the table.
        let mut seq = Column::new(ty("int")).not_null();
        seq.identity = Some(pbps_model::Identity {
            seed: 1,
            increment: 1,
        });
        base_t.columns.insert("seq".to_owned(), seq);
        base_t
            .data
            .as_mut()
            .unwrap()
            .rows
            .get_mut(&pbps_model::RowKey::from("new"))
            .unwrap()
            .0
            .insert("note".to_owned(), Value::Text("kept".to_owned()));
        let mut declared_t = base_t.clone();
        declared_t
            .data
            .as_mut()
            .unwrap()
            .rows
            .get_mut(&pbps_model::RowKey::from("new"))
            .unwrap()
            .0
            .insert("label".to_owned(), Value::Text("Opened".to_owned()));

        let cs = run(
            &schema_of("dbo.s", base_t),
            &schema_of("dbo.s", declared_t),
            &[],
        );
        let Some(Change::UpdateRow {
            columns,
            unchanged,
            types,
            ..
        }) = cs
            .changes
            .iter()
            .map(|p| &p.change)
            .find(|c| matches!(c, Change::UpdateRow { .. }))
        else {
            panic!("{:?}", row_ops(&cs));
        };
        assert_eq!(columns.keys().collect::<Vec<_>>(), ["label"]);
        assert_eq!(
            unchanged.iter().collect::<Vec<_>>(),
            [
                (
                    &"note".to_owned(),
                    &Cell::Value(Value::Text("kept".to_owned()))
                ),
                (&"sort".to_owned(), &Cell::Default("(0)".to_owned())),
            ]
        );
        // The key is neither: it lives in the map key, not in the row. Nor
        // is the identity column.
        assert_eq!(types.keys().collect::<Vec<_>>(), ["label", "note", "sort"]);
        assert!(!unchanged.contains_key("seq"), "{unchanged:?}");
    }

    /// A cell needs two types when the same plan changes the column under it.
    /// `types` is what the *recorded* state holds — the emitter's
    /// precondition — and it carries a column only where this plan leaves its
    /// type alone: a column the base lacks has no recorded cell at all, and a
    /// column this plan retypes has one the engine has since converted out of
    /// the spelling that was recorded (146).
    /// `after_types` is what the column will be once this plan's `AddColumn`
    /// and `AlterColumnType` have run, which is what the write is held to
    /// afterwards; carrying only the first left an added cell held by nothing,
    /// so a trigger could rewrite it and be recorded as the plan's own result
    /// (DECISIONS 140).
    #[test]
    fn an_update_carries_the_type_each_cell_will_have_as_well_as_the_one_it_had() {
        let base_t = lookup(DataMode::Exact, &[("new", "New")]);
        let mut declared_t = base_t.clone();
        // Added and populated in this same revision.
        declared_t
            .columns
            .insert("note".to_owned(), Column::new(ty("nvarchar(50)")));
        // Retyped in this same revision, and its cell left alone.
        declared_t
            .columns
            .insert("label".to_owned(), Column::new(ty("nvarchar(80)")));
        let row = declared_t
            .data
            .as_mut()
            .unwrap()
            .rows
            .get_mut(&pbps_model::RowKey::from("new"))
            .unwrap();
        row.0
            .insert("note".to_owned(), Value::Text("fresh".to_owned()));

        let cs = run(
            &schema_of("dbo.s", base_t),
            &schema_of("dbo.s", declared_t),
            &[],
        );
        let Some(Change::UpdateRow {
            types, after_types, ..
        }) = cs
            .changes
            .iter()
            .map(|p| &p.change)
            .find(|c| matches!(c, Change::UpdateRow { .. }))
        else {
            panic!("{:?}", row_ops(&cs));
        };
        // `note` is held by nothing before the write: the base has no such
        // column, so there is no recorded cell to hold the row to. `label`
        // is — under the type its recorded text was *read* in, paired with
        // the type below, which is what lets the emitter ask the engine for
        // the conversion instead of spelling it (DECISIONS 149, where 146
        // carried neither and held nothing).
        assert_eq!(types.keys().collect::<Vec<_>>(), ["label"]);
        assert_eq!(types["label"], ty("nvarchar(50)"));
        // Both the added column and the retyped one carry what they will be;
        // a column neither added nor retyped carries nothing here, and the
        // emitter falls back to `types`.
        assert_eq!(after_types.keys().collect::<Vec<_>>(), ["label", "note"]);
        assert_eq!(after_types["label"], ty("nvarchar(80)"));
        assert_eq!(after_types["note"], ty("nvarchar(50)"));
    }

    #[test]
    fn exact_deletes_a_row_the_declaration_dropped() {
        let base = schema_of(
            "dbo.s",
            lookup(DataMode::Exact, &[("new", "New"), ("old", "Old")]),
        );
        let declared = schema_of("dbo.s", lookup(DataMode::Exact, &[("new", "New")]));
        assert_eq!(
            row_ops(&run(&base, &declared, &[])),
            ["delete old Undeclared"]
        );
    }

    /// The promise `ensure` makes, and the reason the mode exists at all: the
    /// application writes to this table too, and pbps must not remove what it
    /// put there.
    #[test]
    fn ensure_never_deletes() {
        let base = schema_of(
            "dbo.s",
            lookup(DataMode::Ensure, &[("new", "New"), ("old", "Old")]),
        );
        let declared = schema_of("dbo.s", lookup(DataMode::Ensure, &[("new", "New")]));
        assert!(row_ops(&run(&base, &declared, &[])).is_empty());
    }

    /// Removing the *declaration* must never delete the rows. Reading a removed
    /// block as "delete them all" would make deleting a file destroy data.
    #[test]
    fn removing_the_block_deletes_nothing() {
        let base = schema_of("dbo.s", lookup(DataMode::Exact, &[("new", "New")]));
        let mut without = lookup(DataMode::Exact, &[]);
        without.data = None;
        let declared = schema_of("dbo.s", without);
        let cs = run(&base, &declared, &[]);
        assert!(row_ops(&cs).is_empty(), "{:?}", row_ops(&cs));
        // It is still reported, so the recorded state stops claiming a mode the
        // file no longer declares.
        assert!(
            cs.changes
                .iter()
                .any(|p| matches!(&p.change, Change::SetDataMode { to: None, .. }))
        );
    }

    #[test]
    fn identical_rows_produce_no_change() {
        let base = schema_of("dbo.s", lookup(DataMode::Exact, &[("new", "New")]));
        let declared = schema_of("dbo.s", lookup(DataMode::Exact, &[("new", "New")]));
        assert!(run(&base, &declared, &[]).changes.is_empty());
    }

    /// An omitted column and an explicit `null` are two spellings of one row.
    /// Comparing them unequal would put an UPDATE that changes nothing into
    /// every plan, for ever.
    #[test]
    fn an_omitted_column_equals_an_explicit_null() {
        let mut base_t = lookup(DataMode::Exact, &[("new", "x")]);
        base_t.data.as_mut().unwrap().rows = [(pbps_model::RowKey::from("new"), Row::default())]
            .into_iter()
            .collect();
        let mut declared_t = lookup(DataMode::Exact, &[("new", "x")]);
        declared_t.data.as_mut().unwrap().rows = [(
            pbps_model::RowKey::from("new"),
            [("label".to_owned(), Value::Null)]
                .into_iter()
                .collect::<Row>(),
        )]
        .into_iter()
        .collect();
        assert!(
            run(
                &schema_of("dbo.s", base_t),
                &schema_of("dbo.s", declared_t),
                &[]
            )
            .changes
            .is_empty()
        );
    }

    #[test]
    fn a_new_table_gets_its_rows_after_the_create() {
        let declared = schema_of("dbo.s", lookup(DataMode::Exact, &[("new", "New")]));
        let cs = run(&Schema::default(), &declared, &[]);
        let k = kinds(&cs);
        let create = k.iter().position(|c| c == "CreateTable").unwrap();
        let insert = k.iter().position(|c| c == "InsertRow").unwrap();
        assert!(create < insert, "{k:?}");
    }

    /// The bug this ordering exists to prevent, one level down from the foreign
    /// key between two new tables that a live test had to find: `sub` points at
    /// `status`, so `status`'s rows have to be in before `sub`'s go in — and out
    /// after `sub`'s come out.
    #[test]
    fn rows_follow_the_foreign_keys_between_their_tables() {
        let status = lookup(DataMode::Exact, &[("new", "New")]);
        let mut sub = lookup(DataMode::Exact, &[("a", "A")]);
        sub.columns.insert(
            "parent".to_owned(),
            Column::new(ty("varchar(20)")).not_null(),
        );
        sub.foreign_keys.insert(
            "fk_sub_status".to_owned(),
            pbps_model::ForeignKey {
                columns: vec!["parent".to_owned()],
                references_table: "dbo.status".parse().unwrap(),
                references_columns: vec!["code".to_owned()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );

        let mut empty_status = status.clone();
        empty_status.data.as_mut().unwrap().rows.clear();
        let mut empty_sub = sub.clone();
        empty_sub.data.as_mut().unwrap().rows.clear();

        let mut base = Schema::default();
        base.tables
            .insert("dbo.status".parse().unwrap(), empty_status);
        base.tables.insert("dbo.sub".parse().unwrap(), empty_sub);
        let mut declared = Schema::default();
        declared
            .tables
            .insert("dbo.status".parse().unwrap(), status.clone());
        declared
            .tables
            .insert("dbo.sub".parse().unwrap(), sub.clone());

        let cs = run(&base, &declared, &[]);
        let inserts: Vec<&TableName> = cs
            .changes
            .iter()
            .filter_map(|p| match &p.change {
                Change::InsertRow { table, .. } => Some(table),
                _ => None,
            })
            .collect();
        assert_eq!(
            inserts.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["dbo.status", "dbo.sub"],
            "the referenced table's rows must go in first"
        );

        // And the mirror image: taking them out runs the other way round.
        let cs = run(&declared, &base, &[]);
        let deletes: Vec<String> = cs
            .changes
            .iter()
            .filter_map(|p| match &p.change {
                Change::DeleteRow { table, .. } => Some(table.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(deletes, ["dbo.sub", "dbo.status"]);
    }

    /// The second review's first P1. An existing row that stops writing a
    /// column with a default is asking for the default, and the UPDATE has to
    /// say so: `= NULL` fails a NOT NULL column and stores NULL in a nullable
    /// one.
    #[test]
    fn dropping_a_written_value_in_favour_of_the_default_updates_to_default() {
        let mut base_t = lookup(DataMode::Exact, &[("new", "x")]);
        base_t.columns.get_mut("label").unwrap().default = Some("''".to_owned());
        let mut declared_t = base_t.clone();
        declared_t.data.as_mut().unwrap().rows =
            [(pbps_model::RowKey::from("new"), Row::default())]
                .into_iter()
                .collect();
        let cs = run(
            &schema_of("dbo.s", base_t),
            &schema_of("dbo.s", declared_t),
            &[],
        );
        let Change::UpdateRow { columns, .. } = &cs.changes[0].change else {
            panic!("{:?}", kinds(&cs));
        };
        assert_eq!(
            columns["label"],
            (
                pbps_model::Cell::Value(Value::Text("x".to_owned())),
                pbps_model::Cell::Default("''".to_owned())
            )
        );
    }

    /// The engine respells every expression it stores — measured on SQL
    /// Server, `n > 0 AND label <> 'none'` comes back `([n]>(0) AND
    /// [label]<>'none')` and `GETDATE()` as `getdate()` — so a differ that
    /// compared the declaration against the read-back restated an unchanged
    /// check and rebuilt an unchanged filtered index on every connected plan.
    /// With the declared texts recorded and laid over the read-back, an
    /// unchanged declaration plans nothing, a changed one plans the change,
    /// and an object nothing was recorded for is compared as it always was
    /// (ADR-0013 §4, DECISIONS 207–208).
    #[test]
    fn a_declared_expression_the_engine_respelled_is_not_restated_when_unchanged() {
        let declared_t = {
            let mut t = table(&[
                ("id", Column::new(ty("int")).not_null()),
                ("n", Column::new(ty("int"))),
                ("d", Column::new(ty("date"))),
            ]);
            t.columns.get_mut("n").unwrap().default = Some("0".to_owned());
            t.columns.get_mut("d").unwrap().default = Some("GETDATE()".to_owned());
            t.checks.insert(
                "ck_n".to_owned(),
                pbps_model::CheckConstraint {
                    expression: "n > 0 AND n < 10".to_owned(),
                },
            );
            t.indexes.insert(
                "ix_n".to_owned(),
                pbps_model::Index {
                    columns: vec![pbps_model::IndexColumn {
                        key: pbps_model::IndexKey::Column("n".to_owned()),
                        descending: false,
                        opclass: None,
                    }],
                    include: Vec::new(),
                    unique: false,
                    filter: Some("n > 0".to_owned()),
                    method: Default::default(),
                    storage_parameters: Default::default(),
                },
            );
            t
        };
        let read_back_t = {
            let mut t = declared_t.clone();
            t.columns.get_mut("n").unwrap().default = Some("(0)".to_owned());
            t.columns.get_mut("d").unwrap().default = Some("getdate()".to_owned());
            t.checks.get_mut("ck_n").unwrap().expression = "[n]>(0) AND [n]<(10)".to_owned();
            t.indexes.get_mut("ix_n").unwrap().filter = Some("[n]>(0)".to_owned());
            t
        };
        let declared = schema_of("dbo.t", declared_t.clone());
        let read_back = schema_of("dbo.t", read_back_t);

        // Compared against the read-back, as before the record existed: the
        // restatement, with its drop-and-add of the check and the index.
        let restated = kinds(&run(&read_back, &declared, &[]));
        assert!(
            restated.contains(&"AlterColumnDefault".to_owned()),
            "{restated:?}"
        );
        assert!(restated.contains(&"DropIndex".to_owned()), "{restated:?}");

        // Compared against what was declared when it was written: nothing.
        let recorded = pbps_model::Declared::from_schema(&declared);
        let base = recorded.overlay(&read_back);
        assert!(run(&base, &declared, &[]).changes.is_empty());

        // A changed declaration is still a change — of that expression only.
        let mut edited = declared.clone();
        edited
            .tables
            .get_mut(&"dbo.t".parse().unwrap())
            .unwrap()
            .checks
            .get_mut("ck_n")
            .unwrap()
            .expression = "n > 1 AND n < 10".to_owned();
        assert_eq!(kinds(&run(&base, &edited, &[])), ["DropCheck", "AddCheck"]);
    }

    /// Each side resolves against its own table: a column that *gains* a
    /// default in this plan holds NULL in every existing row (adding a default
    /// does not backfill), and the declaration says the row should hold the
    /// default — so that is an UPDATE, and the right one.
    #[test]
    fn a_column_gaining_a_default_updates_omitted_rows_to_it() {
        let base_t = {
            let mut t = lookup(DataMode::Exact, &[("new", "x")]);
            t.data.as_mut().unwrap().rows = [(pbps_model::RowKey::from("new"), Row::default())]
                .into_iter()
                .collect();
            t
        };
        let mut declared_t = base_t.clone();
        declared_t.columns.get_mut("label").unwrap().default = Some("''".to_owned());
        let cs = run(
            &schema_of("dbo.s", base_t),
            &schema_of("dbo.s", declared_t),
            &[],
        );
        assert_eq!(row_ops(&cs), ["update new [label]"]);
        // And it runs after the default exists.
        let k = kinds(&cs);
        let alter = k.iter().position(|c| c == "AlterColumnDefault").unwrap();
        let update = k.iter().position(|c| c == "UpdateRow").unwrap();
        assert!(alter < update, "{k:?}");
    }

    /// The negative case: two omissions on a column with a default are the
    /// same statement, and must not produce an UPDATE on every plan.
    #[test]
    fn two_omissions_of_a_defaulted_column_are_equal() {
        let mut t = lookup(DataMode::Exact, &[("new", "x")]);
        t.columns.get_mut("label").unwrap().default = Some("''".to_owned());
        t.data.as_mut().unwrap().rows = [(pbps_model::RowKey::from("new"), Row::default())]
            .into_iter()
            .collect();
        assert!(
            run(&schema_of("dbo.s", t.clone()), &schema_of("dbo.s", t), &[])
                .changes
                .is_empty()
        );
    }

    fn rename_column(from: &str, to: &str) -> Intent {
        Intent::RenameColumn {
            table: "dbo.s".parse().unwrap(),
            from: from.to_owned(),
            to: to.to_owned(),
        }
    }

    /// The second review's second round. The key lives in the map key, not in
    /// either row, so a default added to the key column has nothing to compare
    /// against — reading it through the omission rule produced
    /// `SET [code] = DEFAULT` for every row, which for a GUID default rewrites
    /// every identity.
    #[test]
    fn a_default_on_the_key_column_never_updates_the_key() {
        let base_t = lookup(DataMode::Exact, &[("new", "New")]);
        let mut declared_t = base_t.clone();
        declared_t.columns.get_mut("code").unwrap().default = Some("NEWID()".to_owned());
        let cs = run(
            &schema_of("dbo.s", base_t),
            &schema_of("dbo.s", declared_t),
            &[],
        );
        assert!(row_ops(&cs).is_empty(), "{:?}", row_ops(&cs));
        // The negative case: the default itself is still a change.
        assert!(
            kinds(&cs).contains(&"AlterColumnDefault".to_owned()),
            "{:?}",
            kinds(&cs)
        );
    }

    /// An omitted column means *the* default — and when the default changes,
    /// so does what the row should hold. `ALTER` does not backfill, so the
    /// plan has to restate it, and two undifferentiated "default" cells would
    /// have compared equal.
    #[test]
    fn a_changed_default_restates_the_rows_that_omit_the_column() {
        let mut base_t = lookup(DataMode::Exact, &[("new", "x")]);
        base_t.columns.get_mut("label").unwrap().default = Some("'a'".to_owned());
        base_t.data.as_mut().unwrap().rows = [(pbps_model::RowKey::from("new"), Row::default())]
            .into_iter()
            .collect();
        let mut declared_t = base_t.clone();
        declared_t.columns.get_mut("label").unwrap().default = Some("'b'".to_owned());

        let cs = run(
            &schema_of("dbo.s", base_t),
            &schema_of("dbo.s", declared_t),
            &[],
        );
        let Some(Change::UpdateRow { columns, .. }) = cs
            .changes
            .iter()
            .map(|p| &p.change)
            .find(|c| matches!(c, Change::UpdateRow { .. }))
        else {
            panic!("{:?}", kinds(&cs));
        };
        assert_eq!(
            columns["label"],
            (
                pbps_model::Cell::Default("'a'".to_owned()),
                pbps_model::Cell::Default("'b'".to_owned())
            )
        );
        // And after the new default exists.
        let k = kinds(&cs);
        let alter = k.iter().position(|c| c == "AlterColumnDefault").unwrap();
        let update = k.iter().position(|c| c == "UpdateRow").unwrap();
        assert!(alter < update, "{k:?}");
    }

    /// A renamed column keeps its values. Looking the base row up by the new
    /// name found nothing, and restated every row behind the `data-update`
    /// gate for a change that was not one.
    #[test]
    fn a_renamed_column_with_unchanged_values_is_not_an_update() {
        let base_t = lookup(DataMode::Exact, &[("new", "New")]);
        let mut declared_t = base_t.clone();
        let label = declared_t.columns.shift_remove("label").unwrap();
        declared_t.columns.insert("caption".to_owned(), label);
        declared_t.data.as_mut().unwrap().rows = [(
            pbps_model::RowKey::from("new"),
            [("caption".to_owned(), Value::Text("New".to_owned()))]
                .into_iter()
                .collect::<Row>(),
        )]
        .into_iter()
        .collect();

        let cs = run(
            &schema_of("dbo.s", base_t),
            &schema_of("dbo.s", declared_t),
            &[rename_column("label", "caption")],
        );
        assert!(row_ops(&cs).is_empty(), "{:?}", row_ops(&cs));
        assert!(
            kinds(&cs).contains(&"RenameColumn".to_owned()),
            "{:?}",
            kinds(&cs)
        );
    }

    #[test]
    fn absent_baseline_key_is_restored_with_exact_row_changes() {
        let mut base_t = lookup(DataMode::Exact, &[("new", "Old"), ("gone", "Gone")]);
        base_t.primary_key = None;
        let declared_t = lookup(DataMode::Exact, &[("new", "New"), ("added", "Added")]);
        let cs = run(
            &schema_of("dbo.s", base_t),
            &schema_of("dbo.s", declared_t),
            &[],
        );
        for kind in ["SetPrimaryKey", "UpdateRow", "InsertRow", "DeleteRow"] {
            assert!(kinds(&cs).contains(&kind.to_owned()), "{:?}", kinds(&cs));
        }
    }

    #[test]
    fn an_added_key_column_cannot_match_retained_baseline_rows() {
        let mut base = lookup(DataMode::Exact, &[("new", "Old")]);
        base.primary_key = None;
        let mut declared = lookup(DataMode::Exact, &[("new", "New")]);
        declared
            .columns
            .insert("new_code".to_owned(), declared.columns["code"].clone());
        declared.primary_key.as_mut().unwrap().columns = vec!["new_code".to_owned()];
        let name: TableName = "dbo.s".parse().unwrap();
        let mut changes = vec![Change::SetPrimaryKey {
            table: name.clone(),
            from: None,
            to: declared.primary_key.clone(),
            nonclustered: false,
        }];
        let mut errors = Vec::new();
        let mapping = [
            ("code".to_owned(), "code".to_owned()),
            ("label".to_owned(), "label".to_owned()),
        ]
        .into();
        diff_data(&name, &base, &declared, &mapping, &mut changes, &mut errors);
        assert_eq!(
            errors,
            vec![DiffError::DataBaselineKeyOnNewColumn {
                table: name,
                column: "new_code".to_owned(),
            }]
        );
        // The baseline had no key, so nothing moved: the diagnostic must not
        // say it did, nor advise a rename that has nothing to rename (#808).
        let message = errors[0].to_string();
        assert!(!message.contains("moved"), "{message}");
        assert!(!message.contains("rename"), "{message}");
        assert!(message.contains("`new_code`"), "{message}");
        assert_eq!(changes.len(), 1, "{changes:?}");
    }

    #[test]
    fn composite_baseline_key_reports_its_columns_without_moved_key_advice() {
        let declared = lookup(DataMode::Exact, &[("new", "New")]);
        let mut base = declared.clone();
        base.primary_key
            .as_mut()
            .unwrap()
            .columns
            .push("label".to_owned());
        let mut changes = Vec::new();
        let mut errors = Vec::new();
        let mapping = [
            ("code".to_owned(), "code".to_owned()),
            ("label".to_owned(), "label".to_owned()),
        ]
        .into();
        diff_data(
            &"dbo.s".parse().unwrap(),
            &base,
            &declared,
            &mapping,
            &mut changes,
            &mut errors,
        );
        assert_eq!(
            errors,
            vec![DiffError::DataBaselineKeyNotSingle {
                table: "dbo.s".parse().unwrap(),
                columns: vec!["code".to_owned(), "label".to_owned()],
            }]
        );
        let message = errors[0].to_string();
        assert!(
            message.contains("2 columns") && message.contains("single-column"),
            "{message}"
        );
        assert!(!message.contains("Remove the block"), "{message}");
        assert!(changes.is_empty());
    }

    #[test]
    fn absent_baseline_key_without_restoration_is_refused_accurately() {
        let declared = lookup(DataMode::Exact, &[("new", "New")]);
        let mut base = declared.clone();
        base.primary_key = None;
        let mut changes = Vec::new();
        let mut errors = Vec::new();
        diff_data(
            &"dbo.s".parse().unwrap(),
            &base,
            &declared,
            &BTreeMap::new(),
            &mut changes,
            &mut errors,
        );
        assert_eq!(
            errors,
            vec![DiffError::DataBaselineKeyAbsent {
                table: "dbo.s".parse().unwrap()
            }]
        );
        assert!(changes.is_empty());
    }

    /// The row keys on each side are values of that side's key column. When
    /// the key moves to a *different* column the two key sets have nothing in
    /// common, and matching them by text would update and delete the wrong
    /// rows — so it is refused, not guessed.
    #[test]
    fn moving_the_primary_key_to_another_column_is_refused() {
        let base_t = lookup(DataMode::Exact, &[("new", "New")]);
        let mut declared_t = base_t.clone();
        declared_t.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["label".to_owned()],
            storage_parameters: Default::default(),
        });
        // The rows now key on `label`, and say nothing about `code`.
        declared_t.columns.get_mut("code").unwrap().nullable = true;
        declared_t.data.as_mut().unwrap().rows =
            [(pbps_model::RowKey::from("New"), Row::default())]
                .into_iter()
                .collect();

        let base = schema_of("dbo.s", base_t);
        let declared = schema_of("dbo.s", declared_t);
        let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let declared_ids = crate::resolve(&declared, &base_ids, &[], &ctx())
            .unwrap()
            .ids;
        let d = diff_partial(
            Side {
                schema: &base,
                ids: &base_ids,
            },
            Side {
                schema: &declared,
                ids: &declared_ids,
            },
            &MinimalDialect,
            &Hints::default(),
        );
        assert!(
            d.errors
                .iter()
                .any(|e| matches!(e, DiffError::DataKeyColumnChanged { .. })),
            "{:?}",
            d.errors
        );
        assert!(row_ops(&d.changes).is_empty(), "{:?}", row_ops(&d.changes));
    }

    /// The negative case for the refusal: *renaming* the key column is the
    /// same column by uid, the values did not move, and the row changes simply
    /// use the new name.
    #[test]
    fn renaming_the_primary_key_column_keeps_the_rows_matched() {
        let base_t = lookup(DataMode::Exact, &[("new", "New")]);
        let mut declared_t = base_t.clone();
        let code = declared_t.columns.shift_remove("code").unwrap();
        declared_t.columns.insert("kode".to_owned(), code);
        declared_t.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["kode".to_owned()],
            storage_parameters: Default::default(),
        });
        declared_t.data.as_mut().unwrap().rows = [(
            pbps_model::RowKey::from("new"),
            [("label".to_owned(), Value::Text("Renamed".to_owned()))]
                .into_iter()
                .collect::<Row>(),
        )]
        .into_iter()
        .collect();

        let cs = run(
            &schema_of("dbo.s", base_t),
            &schema_of("dbo.s", declared_t),
            &[rename_column("code", "kode")],
        );
        assert_eq!(row_ops(&cs), ["update new [label]"]);
        let Some(Change::UpdateRow { key_column, .. }) = cs
            .changes
            .iter()
            .map(|p| &p.change)
            .find(|c| matches!(c, Change::UpdateRow { .. }))
        else {
            unreachable!()
        };
        assert_eq!(key_column, "kode", "the statement runs after the rename");
    }

    /// The third review's P1. A parent row leaves an `exact` table while a
    /// child row moves its foreign key to another parent. Deleting first is
    /// wrong both ways the engine can go: `NO ACTION` refuses a delete the
    /// child still points at, and `ON DELETE CASCADE` takes the child with it —
    /// silently, with the later update then touching zero rows and the dev
    /// rehearsal, which compares structure only, calling that convergence.
    #[test]
    fn a_child_row_moves_away_before_its_former_parent_is_deleted() {
        let parent = lookup(DataMode::Exact, &[("p1", "P1"), ("p2", "P2")]);
        let mut child = lookup(DataMode::Exact, &[("c", "C")]);
        child.columns.insert(
            "parent".to_owned(),
            Column::new(ty("varchar(20)")).not_null(),
        );
        child.foreign_keys.insert(
            "fk_child_parent".to_owned(),
            pbps_model::ForeignKey {
                columns: vec!["parent".to_owned()],
                references_table: "dbo.parent".parse().unwrap(),
                references_columns: vec!["code".to_owned()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
        let pointing_at = |p: &str| -> Table {
            let mut t = child.clone();
            t.data.as_mut().unwrap().rows = [(
                pbps_model::RowKey::from("c"),
                [
                    ("label".to_owned(), Value::Text("C".to_owned())),
                    ("parent".to_owned(), Value::Text(p.to_owned())),
                ]
                .into_iter()
                .collect::<Row>(),
            )]
            .into_iter()
            .collect();
            t
        };

        let mut base = Schema::default();
        base.tables
            .insert("dbo.parent".parse().unwrap(), parent.clone());
        base.tables
            .insert("dbo.child".parse().unwrap(), pointing_at("p1"));
        let mut declared = Schema::default();
        declared.tables.insert(
            "dbo.parent".parse().unwrap(),
            lookup(DataMode::Exact, &[("p2", "P2")]),
        );
        declared
            .tables
            .insert("dbo.child".parse().unwrap(), pointing_at("p2"));

        let cs = run(&base, &declared, &[]);
        let k = kinds(&cs);
        let update = k
            .iter()
            .position(|c| c == "UpdateRow")
            .unwrap_or_else(|| panic!("{k:?}"));
        let delete = k
            .iter()
            .position(|c| c == "DeleteRow")
            .unwrap_or_else(|| panic!("{k:?}"));
        assert!(update < delete, "{k:?}");
    }

    /// The row a delete removes travels with it: `apply` checks the baseline
    /// and then runs, and a row rewritten in between is not the row the
    /// reviewer approved (DECISIONS 143).
    #[test]
    fn a_deleted_row_carries_the_cells_and_types_the_baseline_recorded() {
        let base = schema_of("dbo.s", lookup(DataMode::Exact, &[("old", "Old")]));
        let declared = schema_of("dbo.s", lookup(DataMode::Exact, &[]));
        let cs = run(&base, &declared, &[]);
        let delete = cs
            .changes
            .iter()
            .find_map(|p| match &p.change {
                Change::DeleteRow { row, types, .. } => Some((row, types)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{:?}", kinds(&cs)));
        assert_eq!(
            delete.0.get("label"),
            Some(&pbps_model::Cell::Value(Value::Text("Old".to_owned())))
        );
        // The key is the delete's own key, not a cell; the type is the base's,
        // so the predicate compares each cell the way the read-back rendered
        // it.
        assert!(!delete.0.contains_key("code"), "{:?}", delete.0);
        assert_eq!(delete.1.get("label"), Some(&ty("nvarchar(50)")));
    }

    /// Every column change sorts before the row changes, so the delete's
    /// predicate has to name the columns as the table has them by then: the
    /// new name for one this plan renames, and nothing at all for one it
    /// drops — a predicate on a column that no longer exists fails an
    /// otherwise valid apply (DECISIONS 143).
    #[test]
    fn a_delete_names_the_columns_the_table_has_when_it_runs() {
        let with_label = |label: &str, extra: Option<&str>| {
            let mut t = lookup(DataMode::Exact, &[]);
            let held = t.columns.shift_remove("label").expect("the lookup's label");
            t.columns.insert(label.to_owned(), held);
            if let Some(extra) = extra {
                t.columns
                    .insert(extra.to_owned(), Column::new(ty("nvarchar(50)")));
            }
            t
        };
        let mut base_table = with_label("label", Some("note"));
        base_table.data = Some(pbps_model::TableData {
            mode: DataMode::Exact,
            rows: [(
                pbps_model::RowKey::from("old"),
                [
                    ("label".to_owned(), Value::Text("Old".to_owned())),
                    ("note".to_owned(), Value::Text("dropped".to_owned())),
                ]
                .into_iter()
                .collect::<Row>(),
            )]
            .into_iter()
            .collect(),
        });
        let base = schema_of("dbo.s", base_table);
        let declared = schema_of("dbo.s", with_label("caption", None));
        let intents = vec![
            Intent::RenameColumn {
                table: "dbo.s".parse().unwrap(),
                from: "label".into(),
                to: "caption".into(),
            },
            Intent::DropColumn {
                column: "dbo.s.note".parse().unwrap(),
                reason: "gone".to_owned(),
            },
        ];
        let cs = run(&base, &declared, &intents);
        let (row, types, dropped) = cs
            .changes
            .iter()
            .find_map(|p| match &p.change {
                Change::DeleteRow {
                    row,
                    types,
                    dropped,
                    ..
                } => Some((row, types, dropped)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{:?}", kinds(&cs)));
        assert_eq!(
            row.get("caption"),
            Some(&pbps_model::Cell::Value(Value::Text("Old".to_owned()))),
            "{row:?}"
        );
        assert!(!row.contains_key("label"), "{row:?}");
        assert!(!row.contains_key("note"), "{row:?}");
        assert_eq!(
            dropped.get("note"),
            Some(&pbps_model::Cell::Value(Value::Text("dropped".to_owned()))),
            "the dropped cell remains visible to the reviewer: {row:?}"
        );
        assert!(
            !types.contains_key("note"),
            "a dropped column cannot be held: {types:?}"
        );
        assert!(
            types.contains_key("caption"),
            "the renamed survivor is still held: {types:?}"
        );
        assert!(
            !row.contains_key("code"),
            "the primary key is carried separately: {row:?}"
        );
    }

    #[test]
    fn a_replaced_column_keeps_its_deleted_rows_baseline_cell() {
        let base = schema_of("dbo.s", lookup(DataMode::Exact, &[("old", "Old")]));
        let declared = schema_of("dbo.s", lookup(DataMode::Exact, &[]));
        let mut intermediate = declared.clone();
        intermediate
            .tables
            .get_mut(&"dbo.s".parse().unwrap())
            .unwrap()
            .columns
            .shift_remove("label");
        let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let intermediate_ids = crate::resolve(
            &intermediate,
            &base_ids,
            &[Intent::DropColumn {
                column: "dbo.s.label".parse().unwrap(),
                reason: "replace".into(),
            }],
            &ctx(),
        )
        .unwrap()
        .ids;
        let declared_ids = crate::resolve(&declared, &intermediate_ids, &[], &ctx())
            .unwrap()
            .ids;
        let cs = diff(
            Side {
                schema: &base,
                ids: &base_ids,
            },
            Side {
                schema: &declared,
                ids: &declared_ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .unwrap();
        assert!(cs.changes.iter().any(
            |p| matches!(&p.change, Change::DropColumn { column, .. } if column.name == "label")
        ));
        assert!(
            cs.changes
                .iter()
                .any(|p| matches!(&p.change, Change::AddColumn { name, .. } if name == "label"))
        );
        let (row, types, dropped) = cs
            .changes
            .iter()
            .find_map(|p| match &p.change {
                Change::DeleteRow {
                    row,
                    types,
                    dropped,
                    ..
                } => Some((row, types, dropped)),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            dropped.get("label"),
            Some(&pbps_model::Cell::Value(Value::Text("Old".into())))
        );
        assert!(!types.contains_key("label"));
        assert!(!row.contains_key("label"));
    }

    /// #402: an `exact` table whose declaration drops some columns and all of
    /// its rows, wide and long enough that a per-column scan of the surviving
    /// names per deleted row was the planning cost. Every deleted row still
    /// carries its dropped cells for the reviewer, and no surviving column's
    /// cell is ever moved into that reviewer-only map — the negative case the
    /// one-set-per-table answer must not break (DECISIONS 442).
    #[test]
    fn every_deleted_row_of_a_wide_table_splits_dropped_and_surviving_cells() {
        const SURVIVING: usize = 40;
        const DROPPED: usize = 5;
        const ROWS: usize = 400;
        let mut base_table = lookup(DataMode::Exact, &[]);
        for i in 0..SURVIVING {
            base_table
                .columns
                .insert(format!("keep{i}"), Column::new(ty("nvarchar(50)")));
        }
        for i in 0..DROPPED {
            base_table
                .columns
                .insert(format!("gone{i}"), Column::new(ty("nvarchar(50)")));
        }
        let rows = &mut base_table.data.as_mut().unwrap().rows;
        for r in 0..ROWS {
            let mut row = Row::default();
            for i in 0..SURVIVING {
                row.0
                    .insert(format!("keep{i}"), Value::Text(format!("k{r}")));
            }
            for i in 0..DROPPED {
                row.0
                    .insert(format!("gone{i}"), Value::Text(format!("g{r}")));
            }
            rows.insert(pbps_model::RowKey::from(format!("row{r}").as_str()), row);
        }
        let base = schema_of("dbo.s", base_table.clone());
        let mut declared_table = base_table;
        declared_table.data.as_mut().unwrap().rows.clear();
        for i in 0..DROPPED {
            declared_table.columns.shift_remove(&format!("gone{i}"));
        }
        let declared = schema_of("dbo.s", declared_table);
        let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let drops: Vec<Intent> = (0..DROPPED)
            .map(|i| Intent::DropColumn {
                column: format!("dbo.s.gone{i}").parse().unwrap(),
                reason: "gone".into(),
            })
            .collect();
        let declared_ids = crate::resolve(&declared, &base_ids, &drops, &ctx())
            .unwrap()
            .ids;
        let cs = diff(
            Side {
                schema: &base,
                ids: &base_ids,
            },
            Side {
                schema: &declared,
                ids: &declared_ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .unwrap();

        let mut deleted = 0;
        for p in &cs.changes {
            let Change::DeleteRow { row, dropped, .. } = &p.change else {
                continue;
            };
            deleted += 1;
            let gone: Vec<&str> = dropped.keys().map(String::as_str).collect();
            let mut expected: Vec<String> = (0..DROPPED).map(|i| format!("gone{i}")).collect();
            expected.sort();
            assert_eq!(
                gone,
                expected.iter().map(String::as_str).collect::<Vec<_>>()
            );
            for i in 0..SURVIVING {
                let keep = format!("keep{i}");
                assert!(
                    !dropped.contains_key(&keep),
                    "{keep} moved to the reviewer map"
                );
                assert!(row.contains_key(&keep), "{keep} missing from the predicate");
            }
        }
        assert_eq!(deleted, ROWS);
    }

    #[test]
    fn a_historical_drop_then_rename_can_reuse_a_baseline_cell_name() {
        let mut base_table = lookup(DataMode::Exact, &[("old", "surviving")]);
        base_table
            .columns
            .insert("note".into(), Column::new(ty("nvarchar(50)")));
        base_table
            .data
            .as_mut()
            .unwrap()
            .rows
            .get_mut(&pbps_model::RowKey::from("old"))
            .unwrap()
            .0
            .insert("note".into(), Value::Text("dropped".into()));
        let base = schema_of("dbo.s", base_table);
        let intermediate = schema_of("dbo.s", lookup(DataMode::Exact, &[]));
        let mut declared_table = lookup(DataMode::Exact, &[]);
        let label = declared_table.columns.shift_remove("label").unwrap();
        declared_table.columns.insert("note".into(), label);
        let declared = schema_of("dbo.s", declared_table);
        let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let intermediate_ids = crate::resolve(
            &intermediate,
            &base_ids,
            &[Intent::DropColumn {
                column: "dbo.s.note".parse().unwrap(),
                reason: "gone".into(),
            }],
            &ctx(),
        )
        .unwrap()
        .ids;
        let declared_ids = crate::resolve(
            &declared,
            &intermediate_ids,
            &[Intent::RenameColumn {
                table: "dbo.s".parse().unwrap(),
                from: "label".into(),
                to: "note".into(),
            }],
            &ctx(),
        )
        .unwrap()
        .ids;
        let cs = diff(
            Side {
                schema: &base,
                ids: &base_ids,
            },
            Side {
                schema: &declared,
                ids: &declared_ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .unwrap();
        let (row, types, dropped) = cs
            .changes
            .iter()
            .find_map(|p| match &p.change {
                Change::DeleteRow {
                    row,
                    types,
                    dropped,
                    ..
                } => Some((row, types, dropped)),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            row.get("note"),
            Some(&pbps_model::Cell::Value(Value::Text("surviving".into())))
        );
        assert!(types.contains_key("note"));
        assert_eq!(
            dropped.get("note"),
            Some(&pbps_model::Cell::Value(Value::Text("dropped".into())))
        );
    }

    /// The same two revisions, and the order the engine needs.
    ///
    /// `order_key` puts `RenameColumn` at 3 and `DropColumn` at 5, so the plan
    /// reached the engine with the rename first — into a name the doomed
    /// column still held. **Measured** on the pinned images: SQL Server 2025
    /// answers `sp_rename` with `Msg 15335, The new name 'note' is already in
    /// use as a COLUMN name and would cause a duplicate that is not
    /// permitted`, and PostgreSQL 18.6 answers `ALTER TABLE s RENAME COLUMN
    /// label TO note` with `column "note" of relation "s" already exists`.
    /// Both take the two statements in the other order.
    ///
    /// The drop that frees the name moves, and only that one: a rename into a
    /// name nothing in this plan gives up is still refused outright rather
    /// than implicitly dropped, which is
    /// `a_single_revision_cannot_rename_into_an_occupied_baseline_name`.
    #[test]
    fn a_dropped_columns_name_is_free_before_the_rename_that_reuses_it() {
        let mut base_table = lookup(DataMode::Exact, &[("old", "surviving")]);
        base_table
            .columns
            .insert("note".into(), Column::new(ty("nvarchar(50)")));
        base_table
            .data
            .as_mut()
            .unwrap()
            .rows
            .get_mut(&pbps_model::RowKey::from("old"))
            .unwrap()
            .0
            .insert("note".into(), Value::Text("dropped".into()));
        let base = schema_of("dbo.s", base_table);
        let intermediate = schema_of("dbo.s", lookup(DataMode::Exact, &[]));
        let mut declared_table = lookup(DataMode::Exact, &[]);
        let label = declared_table.columns.shift_remove("label").unwrap();
        declared_table.columns.insert("note".into(), label);
        let declared = schema_of("dbo.s", declared_table);
        let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let intermediate_ids = crate::resolve(
            &intermediate,
            &base_ids,
            &[Intent::DropColumn {
                column: "dbo.s.note".parse().unwrap(),
                reason: "gone".into(),
            }],
            &ctx(),
        )
        .unwrap()
        .ids;
        let declared_ids = crate::resolve(
            &declared,
            &intermediate_ids,
            &[Intent::RenameColumn {
                table: "dbo.s".parse().unwrap(),
                from: "label".into(),
                to: "note".into(),
            }],
            &ctx(),
        )
        .unwrap()
        .ids;
        let cs = diff(
            Side {
                schema: &base,
                ids: &base_ids,
            },
            Side {
                schema: &declared,
                ids: &declared_ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .unwrap();
        let at = |f: fn(&Change) -> bool| {
            cs.changes
                .iter()
                .position(|p| f(&p.change))
                .unwrap_or_else(|| panic!("{:?}", kinds(&cs)))
        };
        let drop_at =
            at(|c| matches!(c, Change::DropColumn { column, .. } if column.name == "note"));
        let rename_at = at(|c| matches!(c, Change::RenameColumn { to, .. } if to == "note"));
        assert!(
            drop_at < rename_at,
            "the drop must free the name first: {:?}",
            kinds(&cs)
        );
    }

    /// And where the two names are one name only to the database.
    ///
    /// `fold_ident` is the dialect's answer to "are these one name", and on
    /// SQL Server it is the identity — because the answer there belongs to the
    /// *database's* collation, which a plan computed offline does not have.
    /// **Measured** on the pinned image, each in a database of the named
    /// collation:
    ///
    /// ```text
    /// SQL_Latin1_General_CP1_CI_AS   note vs Note  ->  one name, Msg 15335
    /// SQL_Latin1_General_CP1_CI_AI   café vs cafe  ->  one name, Msg 15335
    /// Latin1_General_CS_AS           note vs Note  ->  two names, both kept
    /// ```
    ///
    /// Case here; the accent case is the same fixture with the same answer,
    /// and width and kana sensitivity are two more flags on the same collation
    /// name. The ordering does not fold at all — see
    /// `every_column_drop_on_a_renamed_columns_table_runs_first` for the rule
    /// that makes all of them one case.
    #[test]
    fn a_dropped_columns_name_is_free_before_a_rename_that_differs_only_by_case() {
        let mut base_table = lookup(DataMode::Exact, &[("old", "surviving")]);
        base_table
            .columns
            .insert("note".into(), Column::new(ty("nvarchar(50)")));
        base_table
            .data
            .as_mut()
            .unwrap()
            .rows
            .get_mut(&pbps_model::RowKey::from("old"))
            .unwrap()
            .0
            .insert("note".into(), Value::Text("dropped".into()));
        let base = schema_of("dbo.s", base_table);
        let intermediate = schema_of("dbo.s", lookup(DataMode::Exact, &[]));
        let mut declared_table = lookup(DataMode::Exact, &[]);
        let label = declared_table.columns.shift_remove("label").unwrap();
        declared_table.columns.insert("Note".into(), label);
        let declared = schema_of("dbo.s", declared_table);
        let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let intermediate_ids = crate::resolve(
            &intermediate,
            &base_ids,
            &[Intent::DropColumn {
                column: "dbo.s.note".parse().unwrap(),
                reason: "gone".into(),
            }],
            &ctx(),
        )
        .unwrap()
        .ids;
        let declared_ids = crate::resolve(
            &declared,
            &intermediate_ids,
            &[Intent::RenameColumn {
                table: "dbo.s".parse().unwrap(),
                from: "label".into(),
                to: "Note".into(),
            }],
            &ctx(),
        )
        .unwrap()
        .ids;
        let cs = diff(
            Side {
                schema: &base,
                ids: &base_ids,
            },
            Side {
                schema: &declared,
                ids: &declared_ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .unwrap();
        let at = |f: fn(&Change) -> bool| {
            cs.changes
                .iter()
                .position(|p| f(&p.change))
                .unwrap_or_else(|| panic!("{:?}", kinds(&cs)))
        };
        let drop_at =
            at(|c| matches!(c, Change::DropColumn { column, .. } if column.name == "note"));
        let rename_at = at(|c| matches!(c, Change::RenameColumn { to, .. } if to == "Note"));
        assert!(
            drop_at < rename_at,
            "the engine reads one name where the model reads two: {:?}",
            kinds(&cs)
        );
    }

    /// The rule the two cases above are instances of, stated where it can be
    /// broken: on a table this plan renames a column of, **every** column drop
    /// runs first — including one whose name nothing claims.
    ///
    /// Narrowing it to a name comparison is what the two cases above each
    /// refuted in turn, one collation further out every time. Which spellings
    /// are one column name is the target database's to say, and a plan is
    /// computed offline (SPEC 7.3), so the only question that is true under
    /// every collation is "same table". The over-match is free: nothing
    /// between the drop's old class and its new one can notice, because a
    /// `Revoke` names no column and a column this plan drops is never a
    /// rename's source.
    #[test]
    fn every_column_drop_on_a_renamed_columns_table_runs_first() {
        let base = schema_of(
            "dbo.t",
            table(&[
                ("old", Column::new(ty("int"))),
                ("unrelated", Column::new(ty("int"))),
            ]),
        );
        let declared = schema_of("dbo.t", table(&[("new", Column::new(ty("int")))]));
        let cs = run(
            &base,
            &declared,
            &[
                Intent::RenameColumn {
                    table: "dbo.t".parse().unwrap(),
                    from: "old".into(),
                    to: "new".into(),
                },
                Intent::DropColumn {
                    column: "dbo.t.unrelated".parse().unwrap(),
                    reason: "no longer in use".into(),
                },
            ],
        );
        assert_eq!(
            kinds(&cs),
            ["DropColumn", "RenameColumn"],
            "`unrelated` claims nothing `new` wants, and still goes first: {cs:?}"
        );

        // And a drop on *another* table stays where it was: the rename can
        // collide with a column of its own table and no other.
        let mut base = base;
        base.tables.insert(
            "dbo.other".parse().unwrap(),
            table(&[("gone", Column::new(ty("int")))]),
        );
        let mut declared = declared;
        declared
            .tables
            .insert("dbo.other".parse().unwrap(), table(&[]));
        let cs = run(
            &base,
            &declared,
            &[
                Intent::RenameColumn {
                    table: "dbo.t".parse().unwrap(),
                    from: "old".into(),
                    to: "new".into(),
                },
                Intent::DropColumn {
                    column: "dbo.t.unrelated".parse().unwrap(),
                    reason: "no longer in use".into(),
                },
                Intent::DropColumn {
                    column: "dbo.other.gone".parse().unwrap(),
                    reason: "no longer in use".into(),
                },
            ],
        );
        assert_eq!(
            kinds(&cs),
            ["DropColumn", "RenameColumn", "DropColumn"],
            "only the renamed table's own drops move: {cs:?}"
        );
    }

    #[test]
    fn a_single_revision_cannot_rename_into_an_occupied_baseline_name() {
        let mut base_table = lookup(DataMode::Exact, &[("old", "surviving")]);
        base_table
            .columns
            .insert("note".into(), Column::new(ty("nvarchar(50)")));
        base_table
            .data
            .as_mut()
            .unwrap()
            .rows
            .get_mut(&pbps_model::RowKey::from("old"))
            .unwrap()
            .0
            .insert("note".into(), Value::Text("dropped".into()));
        let base = schema_of("dbo.s", base_table);
        let mut declared_table = lookup(DataMode::Exact, &[]);
        let label = declared_table.columns.shift_remove("label").unwrap();
        declared_table.columns.insert("note".into(), label);
        let declared = schema_of("dbo.s", declared_table);
        let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let errors = crate::resolve(
            &declared,
            &base_ids,
            &[
                Intent::DropColumn {
                    column: "dbo.s.note".parse().unwrap(),
                    reason: "gone".into(),
                },
                Intent::RenameColumn {
                    table: "dbo.s".parse().unwrap(),
                    from: "label".into(),
                    to: "note".into(),
                },
            ],
            &ctx(),
        )
        .unwrap_err();
        assert!(errors.iter().any(|error| matches!(error, crate::Blocker::RenameTargetExists { target } if target == "dbo.s.note")), "{errors:?}");
    }

    /// Two revisions resolved in turn, one dropping `app.target` and the next
    /// renaming `app.old` into its name, then diffed from the undeployed
    /// baseline as a plan that carries both (#536).
    fn a_dropped_tables_name_reused_by_a_later_rename(
        dialect: &dyn Dialect,
        base: &Schema,
        intermediate: &Schema,
        declared: &Schema,
    ) -> ChangeSet {
        let base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let intermediate_ids = crate::resolve(
            intermediate,
            &base_ids,
            &[Intent::DropTable {
                table: "app.target".parse().unwrap(),
                reason: "gone".into(),
            }],
            &ctx(),
        )
        .unwrap()
        .ids;
        let declared_ids = crate::resolve(
            declared,
            &intermediate_ids,
            &[Intent::RenameTable {
                from: "app.old".parse().unwrap(),
                to: "app.target".parse().unwrap(),
            }],
            &ctx(),
        )
        .unwrap()
        .ids;
        diff_with(
            Side {
                schema: base,
                ids: &base_ids,
            },
            Side {
                schema: declared,
                ids: &declared_ids,
            },
            dialect,
        )
    }

    fn diff_with(base: Side<'_>, declared: Side<'_>, dialect: &dyn Dialect) -> ChangeSet {
        diff(base, declared, dialect, &Hints::default()).unwrap()
    }

    /// DEC-981.3, the other two release paths: a table rename vacating a name
    /// a later rename claims in another case, and a check drop freeing a name
    /// a rename claims in another case. Each goes first.
    #[test]
    fn every_release_path_orders_a_claim_spelled_differently_in_case() {
        let t = |n: &str| table(&[(n, Column::new(ty("int")))]);
        let tables = |list: &[(&str, &str)]| {
            let mut s = Schema::default();
            for (name, column) in list {
                s.tables.insert(name.parse().unwrap(), t(column));
            }
            s
        };
        let renames = |cs: &ChangeSet| -> Vec<String> {
            cs.changes
                .iter()
                .filter_map(|p| match &p.change {
                    Change::RenameTable { from, to, .. } => Some(format!("{from} -> {to}")),
                    Change::DropCheck { name, .. } => Some(format!("drop check {name}")),
                    _ => None,
                })
                .collect()
        };
        // `app.b -> app.c`, then `app.a -> app.B`: `a` sorts first by name.
        let cs = across_revisions(
            &tables(&[("app.a", "ay"), ("app.b", "bee")]),
            &[
                (
                    tables(&[("app.a", "ay"), ("app.c", "bee")]),
                    vec![Intent::RenameTable {
                        from: "app.b".parse().unwrap(),
                        to: "app.c".parse().unwrap(),
                    }],
                ),
                (
                    tables(&[("app.B", "ay"), ("app.c", "bee")]),
                    vec![Intent::RenameTable {
                        from: "app.a".parse().unwrap(),
                        to: "app.B".parse().unwrap(),
                    }],
                ),
            ],
        );
        assert_eq!(renames(&cs), ["app.b -> app.c", "app.a -> app.B"], "{cs:?}");

        // A check `Target` on another table, dropped, and `app.old` renamed to
        // `app.target` (constraints share the namespace here).
        let old_t = table(&[("id", Column::new(ty("int")))]);
        let mut other = table(&[("n", Column::new(ty("int")))]);
        other.checks.insert(
            "Target".into(),
            pbps_model::schema::CheckConstraint {
                expression: "n > 0".into(),
            },
        );
        let base = two_tables(("app.old", old_t.clone()), ("app.other", other));
        let declared = two_tables(
            ("app.target", old_t),
            ("app.other", table(&[("n", Column::new(ty("int")))])),
        );
        let cs = run(
            &base,
            &declared,
            &[Intent::RenameTable {
                from: "app.old".parse().unwrap(),
                to: "app.target".parse().unwrap(),
            }],
        );
        assert_eq!(
            renames(&cs),
            ["drop check Target", "app.old -> app.target"],
            "{cs:?}"
        );
    }

    /// DEC-981.3: dropping `app.Target` and renaming `app.old` to
    /// `app.target` across skipped revisions is one name to a case-insensitive
    /// collation, so the drop goes first there too (`sp_rename` is Msg 15335
    /// otherwise). On a case-sensitive database the extra order is harmless.
    #[test]
    fn a_dropped_table_frees_a_name_spelled_differently_in_case() {
        let t = |n: &str| table(&[(n, Column::new(ty("int")))]);
        let tables = |list: &[(&str, &str)]| {
            let mut s = Schema::default();
            for (name, column) in list {
                s.tables.insert(name.parse().unwrap(), t(column));
            }
            s
        };
        let order = |cs: &ChangeSet| -> Vec<String> {
            cs.changes
                .iter()
                .filter_map(|p| match &p.change {
                    Change::DropTable { name, .. } => Some(format!("drop {name}")),
                    Change::RenameTable { from, to, .. } => Some(format!("{from} -> {to}")),
                    _ => None,
                })
                .collect()
        };
        let cs = across_revisions(
            &tables(&[("app.Target", "doomed"), ("app.old", "kept")]),
            &[
                (
                    tables(&[("app.old", "kept")]),
                    vec![Intent::DropTable {
                        table: "app.Target".parse().unwrap(),
                        reason: "gone".into(),
                    }],
                ),
                (
                    tables(&[("app.target", "kept")]),
                    vec![Intent::RenameTable {
                        from: "app.old".parse().unwrap(),
                        to: "app.target".parse().unwrap(),
                    }],
                ),
            ],
        );
        assert_eq!(
            order(&cs),
            ["drop app.Target", "app.old -> app.target"],
            "{cs:?}"
        );
    }

    /// Measured on both engines, the rename is refused while the doomed table
    /// still holds the name: Msg 15335 on SQL Server, `42P07` on PostgreSQL.
    /// So the drop moves ahead of it, on a dialect where nothing else shares
    /// the namespace as well as on one where indexes do.
    /// #981's original case: `s1.old` carries a check `c` into `s2` while the
    /// plan drops the table `s2.c`. A dropped table releases its own name
    /// (DEC-536.1), so the drop runs before the move that claims it; before
    /// that, `DropTable` was class 6 and the transfer was refused.
    #[test]
    fn a_dropped_table_frees_a_name_a_moved_table_carries_in() {
        let mut moving = table(&[("n", Column::new(ty("int")))]);
        moving.checks.insert(
            "c".into(),
            pbps_model::schema::CheckConstraint {
                expression: "n > 0".into(),
            },
        );
        let base = two_tables(
            ("s1.old", moving.clone()),
            ("s2.c", table(&[("id", Column::new(ty("int")))])),
        );
        let declared = schema_of("s2.new", moving);
        let cs = run(
            &base,
            &declared,
            &[
                Intent::RenameTable {
                    from: "s1.old".parse().unwrap(),
                    to: "s2.new".parse().unwrap(),
                },
                Intent::DropTable {
                    table: "s2.c".parse().unwrap(),
                    reason: "gone".into(),
                },
            ],
        );
        assert_eq!(kinds(&cs), ["DropTable", "RenameTable"], "{cs:?}");
    }

    /// A move to another schema takes the constraints it carries out of the
    /// source schema, so a rename into one of their names there runs after
    /// it, under the exact spelling and under one differing only in case
    /// (review of #1346). `a` sorts first by name, so without the link the
    /// rename into `c` ran while the check still held it.
    #[test]
    fn a_rename_into_a_name_a_moved_table_carries_away_runs_after_the_move() {
        let mut moving = table(&[("n", Column::new(ty("int")))]);
        moving.checks.insert(
            "c".into(),
            pbps_model::schema::CheckConstraint {
                expression: "n > 0".into(),
            },
        );
        let other = table(&[("id", Column::new(ty("int")))]);
        let base = two_tables(("s1.old", moving.clone()), ("s1.a", other.clone()));
        for into in ["s1.c", "s1.C"] {
            let declared = two_tables(("s2.new", moving.clone()), (into, other.clone()));
            let cs = run(
                &base,
                &declared,
                &[
                    Intent::RenameTable {
                        from: "s1.old".parse().unwrap(),
                        to: "s2.new".parse().unwrap(),
                    },
                    Intent::RenameTable {
                        from: "s1.a".parse().unwrap(),
                        to: into.parse().unwrap(),
                    },
                ],
            );
            let order: Vec<String> = cs
                .changes
                .iter()
                .filter_map(|p| match &p.change {
                    Change::RenameTable { from, .. } => Some(from.to_string()),
                    _ => None,
                })
                .collect();
            assert_eq!(order, ["s1.old", "s1.a"], "{into}: {cs:?}");
        }
    }

    /// A generated default constraint leaves the source schema with its
    /// table too, so a rename into its name there runs after the move
    /// (review of #1346). A table without a default releases no such name.
    #[test]
    fn a_rename_into_a_moved_tables_generated_default_name_runs_after_the_move() {
        let other = table(&[("id", Column::new(ty("int")))]);
        let order = |moving: &Table| -> Vec<String> {
            let base = two_tables(("s1.old", moving.clone()), ("s1.a", other.clone()));
            let declared = two_tables(("s2.new", moving.clone()), ("s1.df_old_x", other.clone()));
            run_with(
                &NamesItsDefaults,
                &base,
                &declared,
                &[
                    Intent::RenameTable {
                        from: "s1.old".parse().unwrap(),
                        to: "s2.new".parse().unwrap(),
                    },
                    Intent::RenameTable {
                        from: "s1.a".parse().unwrap(),
                        to: "s1.df_old_x".parse().unwrap(),
                    },
                ],
            )
            .changes
            .iter()
            .filter_map(|p| match &p.change {
                Change::RenameTable { from, .. } => Some(from.to_string()),
                _ => None,
            })
            .collect()
        };
        let mut x = Column::new(ty("int"));
        x.default = Some("0".into());
        assert_eq!(order(&table(&[("x", x)])), ["s1.old", "s1.a"]);
        // Nothing frees the name here, so the name order stands.
        assert_eq!(
            order(&table(&[("x", Column::new(ty("int")))])),
            ["s1.a", "s1.old"]
        );
    }

    /// A generated default name is one of the alternatives the table may
    /// hold, so its edge is weak. `a.new` moves to `z.df_old_x` and `z.old`
    /// takes `a.new`: the real edge puts `a.new`'s move first. `z.old`'s
    /// default may hold `z.df_old_x`, or an alternative, in which case the
    /// plan is valid; the generated edge would close a cycle, and it is the
    /// one that gives way (review of #1346).
    #[test]
    fn a_generated_default_name_never_displaces_a_real_release_edge() {
        let mut x = Column::new(ty("int"));
        x.default = Some("0".into());
        let old = table(&[("id", Column::new(ty("int"))), ("x", x)]);
        let other = table(&[("id", Column::new(ty("int")))]);
        // Two revisions, skipped: `a.new` leaves first, then `z.old` takes it.
        let base = two_tables(("z.old", old.clone()), ("a.new", other.clone()));
        let cs = across_revisions_with(
            &NamesItsDefaults,
            &base,
            &[
                (
                    two_tables(("z.old", old.clone()), ("z.df_old_x", other.clone())),
                    vec![Intent::RenameTable {
                        from: "a.new".parse().unwrap(),
                        to: "z.df_old_x".parse().unwrap(),
                    }],
                ),
                (
                    two_tables(("a.new", old), ("z.df_old_x", other)),
                    vec![Intent::RenameTable {
                        from: "z.old".parse().unwrap(),
                        to: "a.new".parse().unwrap(),
                    }],
                ),
            ],
        );
        let order: Vec<String> = cs
            .changes
            .iter()
            .filter_map(|p| match &p.change {
                Change::RenameTable { from, .. } => Some(from.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(order, ["a.new", "z.old"], "{cs:?}");

        // Nor a case-folded one: `a.other` leaves, then `z.old` takes
        // `a.Other`, one name to a case-insensitive collation. That edge
        // outranks the generated name's, whichever was found first.
        let other = table(&[("id", Column::new(ty("int")))]);
        let mut x = Column::new(ty("int"));
        x.default = Some("0".into());
        let old = table(&[("id", Column::new(ty("int"))), ("x", x)]);
        let cs = across_revisions_with(
            &NamesItsDefaults,
            &two_tables(("z.old", old.clone()), ("a.other", other.clone())),
            &[
                (
                    two_tables(("z.old", old.clone()), ("z.df_old_x", other.clone())),
                    vec![Intent::RenameTable {
                        from: "a.other".parse().unwrap(),
                        to: "z.df_old_x".parse().unwrap(),
                    }],
                ),
                (
                    two_tables(("a.Other", old), ("z.df_old_x", other)),
                    vec![Intent::RenameTable {
                        from: "z.old".parse().unwrap(),
                        to: "a.Other".parse().unwrap(),
                    }],
                ),
            ],
        );
        let order: Vec<String> = cs
            .changes
            .iter()
            .filter_map(|p| match &p.change {
                Change::RenameTable { from, .. } => Some(from.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(order, ["a.other", "z.old"], "{cs:?}");
    }

    /// A rename within its schema renames its generated defaults too, so it
    /// releases the old table name's and claims the new one's (review of
    /// #1346). `dbo.a` into `dbo.df_old_x` waits for `dbo.old`'s rename, and
    /// `dbo.b`'s rename to `dbo.c` waits for `dbo.df_c_x` to leave. Without a
    /// default neither name is involved, and the name order stands.
    #[test]
    fn a_rename_in_its_schema_releases_and_claims_generated_default_names() {
        let other = table(&[("id", Column::new(ty("int")))]);
        let mut x = Column::new(ty("int"));
        x.default = Some("0".into());
        let defaulted = table(&[("id", Column::new(ty("int"))), ("x", x)]);
        let plain = table(&[
            ("id", Column::new(ty("int"))),
            ("x", Column::new(ty("int"))),
        ]);
        let order = |moving: &Table, from: &str, to: &str, other_from: &str, other_to: &str| {
            let cs = run_with(
                &NamesItsDefaults,
                &two_tables((from, moving.clone()), (other_from, other.clone())),
                &two_tables((to, moving.clone()), (other_to, other.clone())),
                &[
                    Intent::RenameTable {
                        from: from.parse().unwrap(),
                        to: to.parse().unwrap(),
                    },
                    Intent::RenameTable {
                        from: other_from.parse().unwrap(),
                        to: other_to.parse().unwrap(),
                    },
                ],
            );
            cs.changes
                .iter()
                .filter_map(|p| match &p.change {
                    Change::RenameTable { from, .. } => Some(from.to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        // Released: `df_old_x` leaves with `dbo.old`'s rename.
        assert_eq!(
            order(&defaulted, "dbo.old", "dbo.new", "dbo.a", "dbo.df_old_x"),
            ["dbo.old", "dbo.a"]
        );
        assert_eq!(
            order(&plain, "dbo.old", "dbo.new", "dbo.a", "dbo.df_old_x"),
            ["dbo.a", "dbo.old"]
        );
        // Claimed: `dbo.b -> dbo.c` names its default `df_c_x`.
        assert_eq!(
            order(&defaulted, "dbo.b", "dbo.c", "dbo.df_c_x", "dbo.z"),
            ["dbo.df_c_x", "dbo.b"]
        );
        assert_eq!(
            order(&plain, "dbo.b", "dbo.c", "dbo.df_c_x", "dbo.z"),
            ["dbo.b", "dbo.df_c_x"]
        );
    }

    /// A check the plan drops is gone before its table moves, so the move
    /// does not release its name: `z.old`'s check `c` is dropped, `a.x` moves
    /// to `z.c`, and `z.old` then takes `a.x`. Only `a.x`'s move freeing its
    /// own name orders the two (review of #1346).
    #[test]
    fn a_dropped_check_neither_leaves_nor_arrives_with_its_tables_move() {
        let other = table(&[("id", Column::new(ty("int")))]);
        let mut old = table(&[("id", Column::new(ty("int")))]);
        old.checks.insert(
            "c".into(),
            pbps_model::schema::CheckConstraint {
                expression: "id > 0".into(),
            },
        );
        let plain_old = table(&[("id", Column::new(ty("int")))]);
        let cs = across_revisions(
            &two_tables(("z.old", old.clone()), ("a.x", other.clone())),
            &[
                (
                    two_tables(("z.old", old), ("z.c", other.clone())),
                    vec![Intent::RenameTable {
                        from: "a.x".parse().unwrap(),
                        to: "z.c".parse().unwrap(),
                    }],
                ),
                (
                    two_tables(("a.x", plain_old), ("z.c", other)),
                    vec![Intent::RenameTable {
                        from: "z.old".parse().unwrap(),
                        to: "a.x".parse().unwrap(),
                    }],
                ),
            ],
        );
        let order: Vec<String> = cs
            .changes
            .iter()
            .filter_map(|p| match &p.change {
                Change::RenameTable { from, .. } => Some(from.to_string()),
                Change::DropCheck { name, .. } => Some(format!("drop {name}")),
                _ => None,
            })
            .collect();
        let at = |what: &str| order.iter().position(|o| o == what).unwrap();
        assert!(at("drop c") < at("a.x"), "{order:?}");
        assert!(at("a.x") < at("z.old"), "{order:?}");

        // Nor does it claim the name in the destination: `z.old` drops `c`
        // and moves to `z2.x`, then `z2.c` takes `z.old` (review of #1346).
        let mut old = table(&[("id", Column::new(ty("int")))]);
        old.checks.insert(
            "c".into(),
            pbps_model::schema::CheckConstraint {
                expression: "id > 0".into(),
            },
        );
        let other = table(&[("id", Column::new(ty("int")))]);
        let plain_old = table(&[("id", Column::new(ty("int")))]);
        let cs = across_revisions(
            &two_tables(("z.old", old), ("z2.c", other.clone())),
            &[
                (
                    two_tables(("z2.x", plain_old.clone()), ("z2.c", other.clone())),
                    vec![Intent::RenameTable {
                        from: "z.old".parse().unwrap(),
                        to: "z2.x".parse().unwrap(),
                    }],
                ),
                (
                    two_tables(("z2.x", plain_old), ("z.old", other)),
                    vec![Intent::RenameTable {
                        from: "z2.c".parse().unwrap(),
                        to: "z.old".parse().unwrap(),
                    }],
                ),
            ],
        );
        let order: Vec<String> = cs
            .changes
            .iter()
            .filter_map(|p| match &p.change {
                Change::RenameTable { from, .. } => Some(from.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(order, ["z.old", "z2.c"], "{cs:?}");
    }

    #[test]
    fn a_dropped_tables_name_is_free_before_a_later_rename_reuses_it() {
        let old_t = table(&[("id", Column::new(ty("int")))]);
        let target_t = table(&[("n", Column::new(ty("varchar(10)")))]);
        let base = two_tables(("app.old", old_t.clone()), ("app.target", target_t));
        let intermediate = schema_of("app.old", old_t.clone());
        let declared = schema_of("app.target", old_t);
        for dialect in [&MinimalDialect as &dyn Dialect, &SharesIndexNamespace] {
            let cs = a_dropped_tables_name_reused_by_a_later_rename(
                dialect,
                &base,
                &intermediate,
                &declared,
            );
            assert_eq!(
                kinds(&cs),
                ["DropTable", "RenameTable"],
                "{}: the drop must free the name first: {cs:?}",
                dialect.name()
            );
        }
    }

    /// The doomed table cannot go while another table's key still names it,
    /// and its own keys are dropped first as they always are. Both move with
    /// it. Its own key keeps the doomed table's address: the rename would
    /// otherwise rekey it onto the renamed table, which never had it.
    #[test]
    fn the_keys_around_a_reused_tables_drop_move_ahead_of_it() {
        let old_t = table(&[("id", Column::new(ty("int")).not_null())]);
        let mut target_t = table(&[
            ("id", Column::new(ty("int")).not_null()),
            ("parent_id", Column::new(ty("int"))),
        ]);
        target_t.foreign_keys.insert(
            "fk_target_parent".into(),
            fk(&["parent_id"], "app.parent", &["id"]),
        );
        let parent = table(&[("id", Column::new(ty("int")).not_null())]);
        let mut child = table(&[("target_id", Column::new(ty("int")))]);
        child.foreign_keys.insert(
            "fk_child_target".into(),
            fk(&["target_id"], "app.target", &["id"]),
        );
        let bare_child = table(&[("target_id", Column::new(ty("int")))]);

        let mut base = two_tables(("app.old", old_t.clone()), ("app.target", target_t));
        base.tables
            .insert("app.parent".parse().unwrap(), parent.clone());
        base.tables.insert("app.child".parse().unwrap(), child);
        let mut intermediate = two_tables(("app.old", old_t.clone()), ("app.parent", parent));
        intermediate
            .tables
            .insert("app.child".parse().unwrap(), bare_child);
        let mut declared = intermediate.clone();
        let renamed = declared
            .tables
            .remove(&"app.old".parse::<TableName>().unwrap())
            .unwrap();
        declared
            .tables
            .insert("app.target".parse().unwrap(), renamed);

        let cs = a_dropped_tables_name_reused_by_a_later_rename(
            &MinimalDialect,
            &base,
            &intermediate,
            &declared,
        );
        let at = |f: &dyn Fn(&Change) -> bool| {
            cs.changes
                .iter()
                .position(|p| f(&p.change))
                .unwrap_or_else(|| panic!("{:?}", cs.changes))
        };
        let child_key = at(&|c| {
            matches!(c, Change::DropForeignKey { table, name }
                if table.to_string() == "app.child" && name == "fk_child_target")
        });
        let own_key = at(&|c| {
            matches!(c, Change::DropForeignKey { table, name }
                if table.to_string() == "app.target" && name == "fk_target_parent")
        });
        let drop = at(&|c| matches!(c, Change::DropTable { .. }));
        let rename = at(&|c| matches!(c, Change::RenameTable { .. }));
        assert!(child_key < drop, "{:?}", cs.changes);
        assert!(own_key < drop, "{:?}", cs.changes);
        assert!(drop < rename, "{:?}", cs.changes);
    }

    /// A key that reads the same by name in the first and last revision can
    /// point at two tables: `child`'s key referenced the doomed `target`, is
    /// dropped with it, and a later revision re-adds it under the same name to
    /// the table renamed into `target`. Compared by name alone, nothing
    /// changed, and the standing key would block the drop. It is dropped
    /// ahead of the doomed table and added back after the rename.
    #[test]
    fn a_key_to_a_reused_name_is_rebound_to_its_new_occupant() {
        let keyed = || table(&[("id", Column::new(ty("int")).not_null())]);
        let mut child = table(&[("target_id", Column::new(ty("int")))]);
        child.foreign_keys.insert(
            "fk_child_target".into(),
            fk(&["target_id"], "app.target", &["id"]),
        );
        let bare_child = table(&[("target_id", Column::new(ty("int")))]);

        let mut base = two_tables(("app.old", keyed()), ("app.target", keyed()));
        base.tables
            .insert("app.child".parse().unwrap(), child.clone());
        let intermediate = two_tables(("app.old", keyed()), ("app.child", bare_child));
        let declared = two_tables(("app.target", keyed()), ("app.child", child));

        let cs = a_dropped_tables_name_reused_by_a_later_rename(
            &MinimalDialect,
            &base,
            &intermediate,
            &declared,
        );
        let at = |f: &dyn Fn(&Change) -> bool| {
            cs.changes
                .iter()
                .position(|p| f(&p.change))
                .unwrap_or_else(|| panic!("{:?}", cs.changes))
        };
        let drop_key =
            at(&|c| matches!(c, Change::DropForeignKey { name, .. } if name == "fk_child_target"));
        let drop = at(&|c| matches!(c, Change::DropTable { .. }));
        let rename = at(&|c| matches!(c, Change::RenameTable { .. }));
        let add_key =
            at(&|c| matches!(c, Change::AddForeignKey { name, .. } if name == "fk_child_target"));
        assert!(
            drop_key < drop && drop < rename && rename < add_key,
            "{:?}",
            cs.changes
        );

        // A key whose table keeps its identity is left alone.
        let mut kept = base.clone();
        kept.tables.remove(&"app.old".parse::<TableName>().unwrap());
        let cs = run(&kept, &kept, &[]);
        assert!(cs.changes.is_empty(), "{:?}", cs.changes);
    }

    /// The same, where the doomed table has a key of that name too — a key
    /// name is the table's own on PostgreSQL — and the renamed table's key
    /// pointed at the doomed one. Two identical drops at one address are two
    /// keys, and the renamed table's still has to go and come back.
    #[test]
    fn a_rebound_key_is_not_mistaken_for_the_doomed_tables_key_of_its_name() {
        let keyed = || table(&[("id", Column::new(ty("int")).not_null())]);
        let mut old = table(&[
            ("id", Column::new(ty("int")).not_null()),
            ("target_id", Column::new(ty("int"))),
        ]);
        old.foreign_keys
            .insert("fk".into(), fk(&["target_id"], "app.target", &["id"]));
        let mut target = table(&[
            ("id", Column::new(ty("int")).not_null()),
            ("parent_id", Column::new(ty("int"))),
        ]);
        target
            .foreign_keys
            .insert("fk".into(), fk(&["parent_id"], "app.parent", &["id"]));
        let mut base = two_tables(("app.old", old.clone()), ("app.target", target));
        base.tables.insert("app.parent".parse().unwrap(), keyed());
        let mut bare = old.clone();
        bare.foreign_keys.clear();
        let intermediate = two_tables(("app.old", bare), ("app.parent", keyed()));
        // The renamed table's key now points at itself, under its new name.
        let declared = two_tables(("app.target", old), ("app.parent", keyed()));

        let cs = a_dropped_tables_name_reused_by_a_later_rename(
            &SharesIndexNamespace,
            &base,
            &intermediate,
            &declared,
        );
        let drops: Vec<usize> = cs
            .changes
            .iter()
            .enumerate()
            .filter(
                |(_, p)| matches!(&p.change, Change::DropForeignKey { name, .. } if name == "fk"),
            )
            .map(|(i, _)| i)
            .collect();
        let at = |f: &dyn Fn(&Change) -> bool| {
            cs.changes
                .iter()
                .position(|p| f(&p.change))
                .unwrap_or_else(|| panic!("{:?}", cs.changes))
        };
        let drop = at(&|c| matches!(c, Change::DropTable { .. }));
        let rename = at(&|c| matches!(c, Change::RenameTable { .. }));
        let add = at(&|c| matches!(c, Change::AddForeignKey { name, .. } if name == "fk"));
        assert_eq!(drops.len(), 2, "{:?}", cs.changes);
        // One at each table's address as it stands when it runs: the doomed
        // table's, and the renamed table's source.
        let addresses: BTreeSet<String> = drops
            .iter()
            .filter_map(|d| cs.changes[*d].change.table().map(ToString::to_string))
            .collect();
        assert_eq!(
            addresses,
            BTreeSet::from(["app.old".to_owned(), "app.target".to_owned()]),
            "{:?}",
            cs.changes
        );
        assert!(drops.iter().all(|d| *d < drop), "{:?}", cs.changes);
        assert!(drop < rename && rename < add, "{:?}", cs.changes);
    }

    /// A key on another table this plan drops, pointing at the doomed one, is
    /// that table's own key and is dropped separately like any other; it
    /// still has to go before the doomed table does, and the other table's
    /// drop keeps its class.
    #[test]
    fn another_dropped_tables_key_to_the_reused_name_goes_first() {
        let keyed = || table(&[("id", Column::new(ty("int")).not_null())]);
        let mut other = table(&[("target_id", Column::new(ty("int")))]);
        other.foreign_keys.insert(
            "fk_other_target".into(),
            fk(&["target_id"], "app.target", &["id"]),
        );
        let mut base = two_tables(("app.old", keyed()), ("app.target", keyed()));
        base.tables.insert("app.other".parse().unwrap(), other);
        let intermediate = schema_of("app.old", keyed());
        let declared = schema_of("app.target", keyed());
        let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let intermediate_ids = crate::resolve(
            &intermediate,
            &base_ids,
            &[
                Intent::DropTable {
                    table: "app.target".parse().unwrap(),
                    reason: "gone".into(),
                },
                Intent::DropTable {
                    table: "app.other".parse().unwrap(),
                    reason: "gone".into(),
                },
            ],
            &ctx(),
        )
        .unwrap()
        .ids;
        let declared_ids = crate::resolve(
            &declared,
            &intermediate_ids,
            &[Intent::RenameTable {
                from: "app.old".parse().unwrap(),
                to: "app.target".parse().unwrap(),
            }],
            &ctx(),
        )
        .unwrap()
        .ids;
        let cs = diff_with(
            Side {
                schema: &base,
                ids: &base_ids,
            },
            Side {
                schema: &declared,
                ids: &declared_ids,
            },
            &MinimalDialect,
        );
        let at = |f: &dyn Fn(&Change) -> bool| {
            cs.changes
                .iter()
                .position(|p| f(&p.change))
                .unwrap_or_else(|| panic!("{:?}", cs.changes))
        };
        let key = at(&|c| {
            matches!(c, Change::DropForeignKey { table, name }
                if table.to_string() == "app.other" && name == "fk_other_target")
        });
        let target = at(
            &|c| matches!(c, Change::DropTable { name, .. } if name.to_string() == "app.target"),
        );
        let rename = at(&|c| matches!(c, Change::RenameTable { .. }));
        let other =
            at(&|c| matches!(c, Change::DropTable { name, .. } if name.to_string() == "app.other"));
        assert!(
            key < target && target < rename && rename < other,
            "{:?}",
            cs.changes
        );
    }

    /// DEC-981.3: under a case-insensitive collation `b -> c` then `a -> B` is
    /// a chain, because the engine reads `B` as held by `b`. The fold only
    /// orders: `a -> B` beside `b -> A` is a valid pair on a case-sensitive
    /// database, and a cycle closed through the fold is not reported.
    #[test]
    fn a_column_rename_chain_linked_only_by_case_still_orders() {
        let cols = |c: &[(&str, &str)]| {
            schema_of(
                "dbo.t",
                table(
                    &c.iter()
                        .map(|(n, t)| (*n, Column::new(ty(t))))
                        .collect::<Vec<_>>(),
                ),
            )
        };
        let rename = |from: &str, to: &str| Intent::RenameColumn {
            table: "dbo.t".parse().unwrap(),
            from: from.into(),
            to: to.into(),
        };
        let order = |cs: &ChangeSet| -> Vec<String> {
            cs.changes
                .iter()
                .filter_map(|p| match &p.change {
                    Change::RenameColumn { from, to, .. } => Some(format!("{from} -> {to}")),
                    _ => None,
                })
                .collect()
        };
        // Fresh uids each time: without the link the order was theirs.
        for _ in 0..32 {
            let cs = across_revisions(
                &cols(&[("a", "int"), ("b", "bigint")]),
                &[
                    (
                        cols(&[("a", "int"), ("c", "bigint")]),
                        vec![rename("b", "c")],
                    ),
                    (
                        cols(&[("B", "int"), ("c", "bigint")]),
                        vec![rename("a", "B")],
                    ),
                ],
            );
            assert_eq!(order(&cs), ["b -> c", "a -> B"], "{cs:?}");
        }

        // `a -> B` and `b -> A`: a cycle only through the fold. Both renames
        // are planned, and no cycle is reported.
        let diffed = revisions_diffed(
            &cols(&[("a", "int"), ("b", "bigint")]),
            &[
                (
                    cols(&[("x", "int"), ("b", "bigint")]),
                    vec![rename("a", "x")],
                ),
                (
                    cols(&[("x", "int"), ("A", "bigint")]),
                    vec![rename("b", "A")],
                ),
                (
                    cols(&[("B", "int"), ("A", "bigint")]),
                    vec![rename("x", "B")],
                ),
            ],
        );
        assert!(diffed.errors.is_empty(), "{:?}", diffed.errors);
        assert_eq!(order(&diffed.changes).len(), 2, "{:?}", diffed.changes);

        // `a -> B`, `b -> c`, `c -> A`: a cycle only through two folded
        // links, around the exact one that `b -> c` waits on `c -> A`. The
        // link that would close the cycle is dropped, and the exact one keeps
        // its order.
        for _ in 0..32 {
            let diffed = revisions_diffed(
                &cols(&[("a", "int"), ("b", "bigint"), ("c", "smallint")]),
                &[
                    (
                        cols(&[("a", "int"), ("b", "bigint"), ("A", "smallint")]),
                        vec![rename("c", "A")],
                    ),
                    (
                        cols(&[("a", "int"), ("c", "bigint"), ("A", "smallint")]),
                        vec![rename("b", "c")],
                    ),
                    (
                        cols(&[("B", "int"), ("c", "bigint"), ("A", "smallint")]),
                        vec![rename("a", "B")],
                    ),
                ],
            );
            assert!(diffed.errors.is_empty(), "{:?}", diffed.errors);
            let order = order(&diffed.changes);
            let at = |r: &str| order.iter().position(|o| o == r).unwrap();
            assert!(at("c -> A") < at("b -> c"), "{order:?}");
        }
    }

    /// Revisions resolved in turn from `base`, each with its own intents,
    /// then diffed from the undeployed baseline.
    /// A chain longer than a byte counts: each link keeps its own rank, so a
    /// rename never runs before the one that vacates its target.
    #[test]
    fn a_column_rename_chain_longer_than_a_byte_keeps_every_link_in_order() {
        const LINKS: usize = 300;
        let cols = |names: &[String]| {
            schema_of(
                "dbo.t",
                table(
                    &names
                        .iter()
                        .map(|n| (n.as_str(), Column::new(ty("int"))))
                        .collect::<Vec<_>>(),
                ),
            )
        };
        let name = |i: usize| format!("c{i:03}");
        // Baseline `c000`..`c299`. Revision k renames `c(299-k)` to
        // `c(300-k)`, the name the revision before it vacated.
        let mut names: Vec<String> = (0..LINKS).map(name).collect();
        let base = cols(&names);
        let mut revisions = Vec::new();
        for k in 0..LINKS {
            let from = LINKS - 1 - k;
            names[from] = name(from + 1);
            revisions.push((
                cols(&names),
                vec![Intent::RenameColumn {
                    table: "dbo.t".parse().unwrap(),
                    from: name(from),
                    to: name(from + 1),
                }],
            ));
        }
        let cs = across_revisions(&base, &revisions);
        let order: Vec<String> = cs
            .changes
            .iter()
            .filter_map(|p| match &p.change {
                Change::RenameColumn { from, .. } => Some(from.clone()),
                _ => None,
            })
            .collect();
        let expected: Vec<String> = (0..LINKS).rev().map(name).collect();
        assert_eq!(order, expected);
    }

    /// The base schema and every revision resolved in turn, without the
    /// diff: for a case whose diff reports an error rather than a plan.
    fn revisions_diffed(base: &Schema, revisions: &[(Schema, Vec<Intent>)]) -> Diffed {
        let base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let mut ids = base_ids.clone();
        for (schema, intents) in revisions {
            ids = crate::resolve(schema, &ids, intents, &ctx()).unwrap().ids;
        }
        diff_partial_rebuilding(
            Side {
                schema: base,
                ids: &base_ids,
            },
            Side {
                schema: &revisions.last().unwrap().0,
                ids: &ids,
            },
            &MinimalDialect,
            &Hints::default(),
            &BTreeSet::new(),
            Rebinding::Candidates,
            Screen::Text,
            None,
        )
    }

    /// #541: a column rename into a name another rename of the same table
    /// vacates runs after it. Two minted uids used to decide, and `a -> b`
    /// ran while `b` still stood — `Msg 15335` on SQL Server and `column "b"
    /// of relation "t" already exists` on PostgreSQL.
    #[test]
    fn a_column_rename_into_a_name_another_rename_vacates_runs_after_it() {
        let cols = |c: &[(&str, &str)]| {
            schema_of(
                "dbo.t",
                table(
                    &c.iter()
                        .map(|(n, t)| (*n, Column::new(ty(t))))
                        .collect::<Vec<_>>(),
                ),
            )
        };
        let rename = |from: &str, to: &str| Intent::RenameColumn {
            table: "dbo.t".parse().unwrap(),
            from: from.into(),
            to: to.into(),
        };
        let order = |cs: &ChangeSet| -> Vec<String> {
            cs.changes
                .iter()
                .filter_map(|p| match &p.change {
                    Change::RenameColumn { from, to, .. } => Some(format!("{from} -> {to}")),
                    _ => None,
                })
                .collect()
        };
        // Resolved afresh each time: the uids are minted, so without the
        // chain order the renames came out in either order, and one
        // resolution alone would pass half the time for the wrong reason.
        for _ in 0..32 {
            let cs = across_revisions(
                &cols(&[("a", "int"), ("b", "bigint")]),
                &[
                    (
                        cols(&[("a", "int"), ("c", "bigint")]),
                        vec![rename("b", "c")],
                    ),
                    (
                        cols(&[("b", "int"), ("c", "bigint")]),
                        vec![rename("a", "b")],
                    ),
                ],
            );
            assert_eq!(order(&cs), ["b -> c", "a -> b"], "{cs:?}");

            // Three links: the far end first, whatever the uids say.
            let cs = across_revisions(
                &cols(&[("a", "int"), ("b", "bigint"), ("c", "text")]),
                &[
                    (
                        cols(&[("a", "int"), ("b", "bigint"), ("d", "text")]),
                        vec![rename("c", "d")],
                    ),
                    (
                        cols(&[("a", "int"), ("c", "bigint"), ("d", "text")]),
                        vec![rename("b", "c")],
                    ),
                    (
                        cols(&[("b", "int"), ("c", "bigint"), ("d", "text")]),
                        vec![rename("a", "b")],
                    ),
                ],
            );
            assert_eq!(order(&cs), ["c -> d", "b -> c", "a -> b"], "{cs:?}");
        }

        // Two columns trading names through a third across three revisions:
        // `resolve` accepts every step, and no order of the net renames runs.
        // The plan says so instead of emitting one.
        let diffed = revisions_diffed(
            &cols(&[("a", "int"), ("b", "bigint")]),
            &[
                (
                    cols(&[("c", "int"), ("b", "bigint")]),
                    vec![rename("a", "c")],
                ),
                (
                    cols(&[("c", "int"), ("a", "bigint")]),
                    vec![rename("b", "a")],
                ),
                (
                    cols(&[("b", "int"), ("a", "bigint")]),
                    vec![rename("c", "b")],
                ),
            ],
        );
        assert!(
            diffed.errors.iter().any(|e| matches!(e,
                DiffError::ColumnRenameCycle { table, columns }
                    if table.to_string() == "dbo.t"
                        && columns == &BTreeSet::from(["a".to_owned(), "b".to_owned()]))),
            "{:?}",
            diffed.errors
        );
    }

    fn across_revisions(base: &Schema, revisions: &[(Schema, Vec<Intent>)]) -> ChangeSet {
        across_revisions_with(&MinimalDialect, base, revisions)
    }

    /// [`across_revisions`], against a caller-chosen dialect.
    fn across_revisions_with(
        dialect: &dyn Dialect,
        base: &Schema,
        revisions: &[(Schema, Vec<Intent>)],
    ) -> ChangeSet {
        let base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let mut ids = base_ids.clone();
        for (schema, intents) in revisions {
            ids = crate::resolve(schema, &ids, intents, &ctx()).unwrap().ids;
        }
        diff_with(
            Side {
                schema: base,
                ids: &base_ids,
            },
            Side {
                schema: &revisions.last().unwrap().0,
                ids: &ids,
            },
            dialect,
        )
    }

    /// A rename releases its source name as it runs, so a later revision's
    /// rename into that name runs after it. `y` sorts before `z`, so without
    /// the edge the alphabet ran `y -> z` while `z` still stood.
    #[test]
    fn a_rename_into_a_name_another_rename_vacates_runs_after_it() {
        let t = |n: &str| table(&[(n, Column::new(ty("int")))]);
        let rename = |from: &str, to: &str| Intent::RenameTable {
            from: from.parse().unwrap(),
            to: to.parse().unwrap(),
        };
        let schema = |tables: &[(&str, &str)]| {
            let mut s = Schema::default();
            for (name, column) in tables {
                s.tables.insert(name.parse().unwrap(), t(column));
            }
            s
        };
        let order = |cs: &ChangeSet| -> Vec<String> {
            cs.changes
                .iter()
                .filter_map(|p| match &p.change {
                    Change::DropTable { name, .. } => Some(format!("drop {name}")),
                    Change::RenameTable { from, to, .. } => Some(format!("{from} -> {to}")),
                    _ => None,
                })
                .collect()
        };

        // With a drop at the head of the chain.
        let cs = across_revisions(
            &schema(&[("app.a", "doomed"), ("app.z", "zed"), ("app.y", "why")]),
            &[
                (
                    schema(&[("app.z", "zed"), ("app.y", "why")]),
                    vec![Intent::DropTable {
                        table: "app.a".parse().unwrap(),
                        reason: "gone".into(),
                    }],
                ),
                (
                    schema(&[("app.a", "zed"), ("app.y", "why")]),
                    vec![rename("app.z", "app.a")],
                ),
                (
                    schema(&[("app.a", "zed"), ("app.z", "why")]),
                    vec![rename("app.y", "app.z")],
                ),
            ],
        );
        assert_eq!(
            order(&cs),
            ["drop app.a", "app.z -> app.a", "app.y -> app.z"],
            "{cs:?}"
        );

        // And without one: a chain of renames alone.
        let cs = across_revisions(
            &schema(&[("app.z", "zed"), ("app.y", "why")]),
            &[
                (
                    schema(&[("app.a", "zed"), ("app.y", "why")]),
                    vec![rename("app.z", "app.a")],
                ),
                (
                    schema(&[("app.a", "zed"), ("app.z", "why")]),
                    vec![rename("app.y", "app.z")],
                ),
            ],
        );
        assert_eq!(order(&cs), ["app.z -> app.a", "app.y -> app.z"], "{cs:?}");

        // Across schemas: a move passes through its source name in the
        // destination schema, so `s1.z -> s2.a` holds `s2.z` for a moment,
        // and `s1.y -> s2.z` waits for it.
        let cs = across_revisions(
            &schema(&[("s1.z", "zed"), ("s1.y", "why")]),
            &[
                (
                    schema(&[("s2.a", "zed"), ("s1.y", "why")]),
                    vec![rename("s1.z", "s2.a")],
                ),
                (
                    schema(&[("s2.a", "zed"), ("s2.z", "why")]),
                    vec![rename("s1.y", "s2.z")],
                ),
            ],
        );
        assert_eq!(order(&cs), ["s1.z -> s2.a", "s1.y -> s2.z"], "{cs:?}");

        // Two tables trading names through a third across three revisions
        // net to a cycle. No order serves it, and the graph must not loop or
        // panic looking for one: both renames are still planned.
        let cs = across_revisions(
            &schema(&[("app.a", "ay"), ("app.b", "bee")]),
            &[
                (
                    schema(&[("app.c", "ay"), ("app.b", "bee")]),
                    vec![rename("app.a", "app.c")],
                ),
                (
                    schema(&[("app.c", "ay"), ("app.a", "bee")]),
                    vec![rename("app.b", "app.a")],
                ),
                (
                    schema(&[("app.b", "ay"), ("app.a", "bee")]),
                    vec![rename("app.c", "app.b")],
                ),
            ],
        );
        assert_eq!(order(&cs).len(), 2, "{cs:?}");
    }

    /// DEC-1118.1: a trigger keyed by a name that passes to another table is
    /// two triggers under one id. The doomed table's goes first; the one
    /// declared on the new occupant is created after the rename. Compared by
    /// id alone, nothing changed, and the occupant never got its trigger.
    #[test]
    fn a_trigger_on_a_name_that_changes_hands_is_dropped_and_created() {
        let keyed = || table(&[("id", Column::new(ty("int")).not_null())]);
        let trigger = || {
            (
                pbps_model::ModuleId::Trigger {
                    on: "app.target".parse().unwrap(),
                    name: "audit".to_owned(),
                },
                pbps_model::Module {
                    kind: pbps_model::ModuleKind::Trigger,
                    description: None,
                    definition: "AFTER INSERT AS SELECT 1".to_owned(),
                },
            )
        };
        let mut base = two_tables(("app.old", keyed()), ("app.target", keyed()));
        base.modules.extend([trigger()]);
        let intermediate = schema_of("app.old", keyed());
        let mut declared = schema_of("app.target", keyed());
        declared.modules.extend([trigger()]);

        let cs = a_dropped_tables_name_reused_by_a_later_rename(
            &MinimalDialect,
            &base,
            &intermediate,
            &declared,
        );
        let at = |f: &dyn Fn(&Change) -> bool| {
            cs.changes
                .iter()
                .position(|p| f(&p.change))
                .unwrap_or_else(|| panic!("{:?}", cs.changes))
        };
        let dropped = at(&|c| matches!(c, Change::DropModule { .. }));
        let table_dropped = at(&|c| matches!(c, Change::DropTable { .. }));
        let rename = at(&|c| matches!(c, Change::RenameTable { .. }));
        let created = at(&|c| matches!(c, Change::CreateModule { .. }));
        assert!(
            dropped < table_dropped && rename < created,
            "{:?}",
            cs.changes
        );

        // The same trigger on a table that keeps its identity is untouched.
        let cs = run(&declared, &declared, &[]);
        assert!(cs.changes.is_empty(), "{:?}", cs.changes);
    }

    /// DEC-1118.1: a grant keyed by a name that passes to another table is
    /// compared as the occupant's. The doomed table's grants go with it; the
    /// occupant keeps what it held under its old name, gains what is declared
    /// and loses what is not. The last was skipped: the name read as a
    /// dropped object, whose grants nothing revokes.
    #[test]
    fn grants_on_a_name_that_changes_hands_are_compared_as_the_occupants() {
        use pbps_model::{GrantTarget, Permission, Role};
        let keyed = || table(&[("id", Column::new(ty("int")).not_null())]);
        let role = |grants: &[(&str, Permission)]| {
            let mut r = Role::default();
            for (target, permission) in grants {
                r.grants
                    .entry(target.parse::<GrantTarget>().unwrap())
                    .or_default()
                    .insert(*permission);
            }
            r
        };
        let with_role = |mut s: Schema, r: Role| {
            s.roles.insert("app_reader".to_owned(), r);
            s
        };
        let base = with_role(
            two_tables(("app.old", keyed()), ("app.target", keyed())),
            role(&[
                ("app.target", Permission::Select),
                ("app.old", Permission::Insert),
            ]),
        );
        let intermediate = with_role(
            schema_of("app.old", keyed()),
            role(&[("app.old", Permission::Insert)]),
        );
        let declared = with_role(
            schema_of("app.target", keyed()),
            role(&[("app.target", Permission::Select)]),
        );

        let cs = a_dropped_tables_name_reused_by_a_later_rename(
            &MinimalDialect,
            &base,
            &intermediate,
            &declared,
        );
        let grants: BTreeSet<String> = cs
            .changes
            .iter()
            .filter_map(|p| match &p.change {
                Change::Grant {
                    target,
                    permissions,
                    ..
                } => Some(format!("grant {target} {permissions:?}")),
                Change::Revoke {
                    target,
                    permissions,
                    ..
                } => Some(format!("revoke {target} {permissions:?}")),
                _ => None,
            })
            .collect();
        assert_eq!(
            grants,
            BTreeSet::from([
                "grant app.target {Select}".to_owned(),
                "revoke app.target {Insert}".to_owned(),
            ]),
            "{:?}",
            cs.changes
        );
    }

    /// Only a drop whose name a rename claims moves. Another table's drop
    /// keeps its class, after the renames.
    #[test]
    fn a_dropped_table_nothing_renames_into_stays_after_the_renames() {
        let old_t = table(&[("id", Column::new(ty("int")))]);
        let base = two_tables(("app.old", old_t.clone()), ("app.gone", old_t.clone()));
        let declared = schema_of("app.new", old_t);
        let cs = run(
            &base,
            &declared,
            &[
                Intent::DropTable {
                    table: "app.gone".parse().unwrap(),
                    reason: "gone".into(),
                },
                Intent::RenameTable {
                    from: "app.old".parse().unwrap(),
                    to: "app.new".parse().unwrap(),
                },
            ],
        );
        assert_eq!(kinds(&cs), ["RenameTable", "DropTable"], "{cs:?}");
    }

    /// And a name still held by a surviving table is refused, not implicitly
    /// freed: only a drop some revision recorded can release it.
    #[test]
    fn a_rename_into_a_surviving_tables_name_is_refused() {
        let old_t = table(&[("id", Column::new(ty("int")))]);
        let base = two_tables(("app.old", old_t.clone()), ("app.target", old_t.clone()));
        let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let declared = schema_of("app.target", old_t);
        let errors = crate::resolve(
            &declared,
            &base_ids,
            &[Intent::RenameTable {
                from: "app.old".parse().unwrap(),
                to: "app.target".parse().unwrap(),
            }],
            &ctx(),
        )
        .unwrap_err();
        assert!(
            errors.iter().any(|error| matches!(error,
                crate::Blocker::RenameTargetExists { target } if target == "app.target")),
            "{errors:?}"
        );
    }

    /// And the same for a delete: `AlterColumnType` sorts before the row
    /// changes, so a cell whose column this plan retypes carries both types —
    /// the one its recorded text was read in and the one the column has when
    /// the `DELETE` runs. Together they are a predicate the engine can
    /// answer; either alone compares two spellings of one value, which is
    /// why 146 carried neither and left the row held by its key alone
    /// (DECISIONS 149).
    #[test]
    fn a_delete_holds_a_retyped_cell_by_both_of_its_types() {
        let base = schema_of("dbo.s", lookup(DataMode::Exact, &[("old", "Old")]));
        let mut declared_t = lookup(DataMode::Exact, &[]);
        declared_t
            .columns
            .insert("label".to_owned(), Column::new(ty("varchar(50)")));
        let declared = schema_of("dbo.s", declared_t);

        let cs = run(&base, &declared, &[]);
        let (row, types, after_types) = cs
            .changes
            .iter()
            .find_map(|p| match &p.change {
                Change::DeleteRow {
                    row,
                    types,
                    after_types,
                    ..
                } => Some((row, types, after_types)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{:?}", kinds(&cs)));
        assert!(row.contains_key("label"), "{row:?}");
        assert_eq!(types["label"], ty("nvarchar(50)"), "{types:?}");
        assert_eq!(after_types["label"], ty("varchar(50)"), "{after_types:?}");
    }

    /// A row write carries its key's type, the declared one the `WHERE`
    /// meets once any retype has run, so the emitter can find the row by the
    /// engine's own `=` without the session's path (DEC-1564.2). The key is
    /// not a cell: it stays out of `types`, which holds the row's cells.
    #[test]
    fn a_row_write_carries_its_keys_declared_type_and_not_as_a_cell() {
        let base = schema_of(
            "dbo.s",
            lookup(DataMode::Exact, &[("old", "Old"), ("kept", "Before")]),
        );
        let mut declared_t = lookup(DataMode::Exact, &[("kept", "After")]);
        declared_t
            .columns
            .insert("code".to_owned(), Column::new(ty("varchar(40)")).not_null());
        let declared = schema_of("dbo.s", declared_t);

        let cs = run(&base, &declared, &[]);
        let mut seen = 0;
        for p in &cs.changes {
            let (key_type, types) = match &p.change {
                Change::UpdateRow {
                    key_type, types, ..
                }
                | Change::DeleteRow {
                    key_type, types, ..
                } => (key_type, types),
                _ => continue,
            };
            seen += 1;
            assert_eq!(
                key_type.as_ref(),
                Some(&ty("varchar(40)")),
                "{:?}",
                kinds(&cs)
            );
            assert!(!types.contains_key("code"), "{types:?}");
        }
        assert_eq!(seen, 2, "an update and a delete: {:?}", kinds(&cs));
    }

    /// And the far more common case, which must stay a single type: a column
    /// nobody retyped goes in `types` and nowhere else, so the emitter
    /// compares the recorded text against the column and asks the engine for
    /// no conversion at all.
    #[test]
    fn a_delete_carries_one_type_for_a_column_this_plan_leaves_alone() {
        let base = schema_of("dbo.s", lookup(DataMode::Exact, &[("old", "Old")]));
        let declared = schema_of("dbo.s", lookup(DataMode::Exact, &[]));

        let cs = run(&base, &declared, &[]);
        let (types, after_types) = cs
            .changes
            .iter()
            .find_map(|p| match &p.change {
                Change::DeleteRow {
                    types, after_types, ..
                } => Some((types, after_types)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{:?}", kinds(&cs)));
        assert_eq!(types["label"], ty("nvarchar(50)"), "{types:?}");
        assert!(after_types.is_empty(), "{after_types:?}");
    }

    /// A `data:` block whose rows have no identity is refused, not silently
    /// dropped from the plan.
    #[test]
    fn a_data_block_without_a_single_column_key_is_an_error() {
        let mut t = lookup(DataMode::Exact, &[("new", "New")]);
        t.primary_key = None;
        let declared = schema_of("dbo.s", t.clone());
        let base_ids = crate::resolve(&declared, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let d = diff_partial(
            Side {
                schema: &Schema::default(),
                ids: &IdsFile::default(),
            },
            Side {
                schema: &declared,
                ids: &base_ids,
            },
            &MinimalDialect,
            &Hints::default(),
        );
        assert!(
            d.errors
                .iter()
                .any(|e| matches!(e, DiffError::DataWithoutKey { .. })),
            "{:?}",
            d.errors
        );
    }

    fn kinds(cs: &ChangeSet) -> Vec<String> {
        cs.changes
            .iter()
            .map(|p| {
                format!("{:?}", p.change)
                    .split_whitespace()
                    .next()
                    .unwrap_or("?")
                    .trim_end_matches('{')
                    .trim()
                    .to_string()
            })
            .collect()
    }

    /// Found by the live convergence test: a new table's FK referenced another
    /// new table, and whether the referenced CREATE ran first depended on the
    /// random uid order. The FK must always come out as its own change, after
    /// every CreateTable — from either direction of the reference.
    #[test]
    fn a_foreign_key_between_two_new_tables_sorts_after_both_creates() {
        // Run it both ways round: customer -> region and region2 -> aaa. With
        // the bug, one of the two directions fails depending on name order.
        for (referencing, referenced) in [("dbo.customer", "dbo.region"), ("dbo.aaa", "dbo.zzz")] {
            let mut fk_table = table(&[("other_id", Column::new(ty("int")))]);
            fk_table.foreign_keys.insert(
                "fk_link".into(),
                pbps_model::ForeignKey {
                    columns: vec!["other_id".into()],
                    references_table: referenced.parse().unwrap(),
                    references_columns: vec!["id".into()],
                    on_delete: Default::default(),
                    on_update: Default::default(),
                },
            );
            let mut declared = schema_of(referencing, fk_table);
            declared.tables.insert(
                referenced.parse().unwrap(),
                table(&[("id", Column::new(ty("int")).not_null())]),
            );

            let cs = run(&Schema::default(), &declared, &[]);
            let ks = kinds(&cs);
            assert_eq!(
                ks,
                ["CreateTable", "CreateTable", "AddForeignKey"],
                "{referencing} -> {referenced}: {ks:?}"
            );
            // And the CreateTable no longer smuggles the FK along.
            assert!(
                cs.changes.iter().all(|p| match &p.change {
                    Change::CreateTable { table, .. } => table.foreign_keys.is_empty(),
                    _ => true,
                }),
                "the FK must not also ride inside the CREATE"
            );
        }
    }

    /// The mirror image: dropping two tables that reference each other must
    /// shed the foreign keys before either DROP TABLE runs.
    #[test]
    fn foreign_keys_of_dropped_tables_are_dropped_before_the_tables() {
        let mut fk_table = table(&[("other_id", Column::new(ty("int")))]);
        fk_table.foreign_keys.insert(
            "fk_link".into(),
            pbps_model::ForeignKey {
                columns: vec!["other_id".into()],
                references_table: "dbo.zzz".parse().unwrap(),
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
        let mut base = schema_of("dbo.aaa", fk_table);
        base.tables.insert(
            "dbo.zzz".parse().unwrap(),
            table(&[("id", Column::new(ty("int")).not_null())]),
        );

        let intents = [
            Intent::DropTable {
                table: "dbo.aaa".parse().unwrap(),
                reason: "test".into(),
            },
            Intent::DropTable {
                table: "dbo.zzz".parse().unwrap(),
                reason: "test".into(),
            },
        ];
        let cs = run(&base, &Schema::default(), &intents);
        assert_eq!(
            kinds(&cs),
            ["DropForeignKey", "DropTable", "DropTable"],
            "{:?}",
            kinds(&cs)
        );
    }

    /// A table name that is still declared is still that table: `resolve`
    /// binds a name on both sides to the uid it already has, so no intent can
    /// retire a uid and hand its name to a new object in one revision.
    ///
    /// Pinned because a caller depends on it. `refuse_unplanned_movement`
    /// pairs the baseline read with the read-back **by name**, and a plan that
    /// could drop `dbo.t` and put a different `dbo.t` back would make that
    /// pairing compare two unrelated tables with no change of the plan naming
    /// the difference — every such apply refused. The guard needs no case for
    /// it because this rule makes it unrepresentable, which is the better half
    /// of that trade; if this test ever fails, that guard is what to revisit
    /// (DECISIONS 170).
    #[test]
    fn a_declared_name_cannot_be_dropped_and_reoccupied_in_one_revision() {
        let base = schema_of("dbo.t", table(&[("a", Column::new(ty("int")))]));
        let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;

        // Declared again under its own name, with drop intent: the intent is
        // unused, because the name still resolves to the uid it had.
        let replaced = schema_of("dbo.t", table(&[("a", Column::new(ty("int")))]));
        let blockers = crate::resolve(
            &replaced,
            &base_ids,
            &[Intent::DropTable {
                table: "dbo.t".parse().unwrap(),
                reason: "replaced".into(),
            }],
            &ctx(),
        )
        .expect_err("a declared name cannot also be dropped");
        assert!(
            blockers
                .iter()
                .any(|b| matches!(b, crate::Blocker::UnusedIntent { .. })),
            "{blockers:?}"
        );

        // And the same for handing the name to another table by rename.
        let mut two = schema_of("dbo.t", table(&[("a", Column::new(ty("int")))]));
        two.tables.insert(
            "dbo.a".parse().unwrap(),
            table(&[("a", Column::new(ty("int")))]),
        );
        let two_ids = crate::resolve(&two, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let blockers = crate::resolve(
            &replaced,
            &two_ids,
            &[
                Intent::DropTable {
                    table: "dbo.t".parse().unwrap(),
                    reason: "replaced".into(),
                },
                Intent::RenameTable {
                    from: "dbo.a".parse().unwrap(),
                    to: "dbo.t".parse().unwrap(),
                },
            ],
            &ctx(),
        )
        .expect_err("a rename cannot take an occupied name either");
        assert!(
            blockers.iter().any(|b| matches!(
                b,
                crate::Blocker::RenameTargetExists { target } if target == "dbo.t"
            )),
            "{blockers:?}"
        );
    }

    /// A declaration that gives up a primary key and relaxes the column it
    /// held produces a plan that runs the key's drop first, because neither
    /// engine will relax a column a key still names.
    ///
    /// Measured, both refuse: PostgreSQL answers `42P16`, "column \"id\" is in
    /// a primary key", and SQL Server answers 5074 with 4922 behind it. The
    /// plan was valid, reviewed and unapplicable — the shape this ordering
    /// exists to prevent.
    ///
    /// The same class also puts the drop ahead of `DropColumn` at 5, which is
    /// the other half: SQL Server refuses to drop a column its key names, with
    /// the same 5074.
    #[test]
    fn a_key_is_dropped_before_the_column_it_held_is_relaxed() {
        let mut base_t = table(&[("id", Column::new(ty("int")).not_null())]);
        base_t.primary_key = Some(pbps_model::PrimaryKey {
            name: Some("pk_t".to_owned()),
            columns: vec!["id".to_owned()],
            storage_parameters: Default::default(),
        });
        let base = schema_of("dbo.t", base_t);
        let declared = schema_of("dbo.t", table(&[("id", Column::new(ty("int")))]));

        let cs = run(&base, &declared, &[]);
        let order: Vec<_> = cs
            .changes
            .iter()
            .map(|p| std::mem::discriminant(&p.change))
            .collect();
        assert_eq!(
            order,
            vec![
                std::mem::discriminant(&Change::SetPrimaryKey {
                    table: "dbo.t".parse().unwrap(),
                    from: None,
                    to: None,
                    nonclustered: false,
                }),
                std::mem::discriminant(&Change::AlterColumnNullability {
                    uid: pbps_model::Uid::generate(pbps_model::UidKind::Column),
                    column: pbps_model::ColumnRef {
                        table: "dbo.t".parse().unwrap(),
                        name: "id".to_owned(),
                    },
                    ty: ty("int"),
                    to_nullable: true,
                    collation: None,
                }),
            ],
            "the key has to go first: {:#?}",
            cs.changes
        );
    }

    /// A column with a default and a new type is three phases, one rank each:
    /// the old default out, the type changed, the new default in.
    ///
    /// Both kinds are class 9, so the tiebreaker decided the order, and the
    /// tiebreaker is the change's rendering — which puts `AlterColumnDefault`
    /// ahead of `AlterColumnType` by the alphabet and nothing else. Each end of
    /// that is refused by an engine, and both are measured:
    ///
    /// - the new default first: `SET DEFAULT 'abc'` on an `integer` column is
    ///   "invalid input syntax for type integer" on PostgreSQL;
    /// - the type first: `ALTER COLUMN n bigint` is refused by SQL Server while
    ///   a default constraint stands on the column — 5074, with 4922 behind it.
    ///   The nullability form of the same statement is *accepted*, so the
    ///   dependency belongs to the type change and not to `ALTER COLUMN`.
    ///
    /// So a replaced default on a retyped column is split into its two halves
    /// (`diff_columns`), and the three ranks put the type between them.
    #[test]
    fn a_retyped_column_drops_its_old_default_changes_type_then_takes_the_new_one() {
        let with = |t: &str, d: &str| Column {
            default: Some(d.to_owned()),
            ..Column::new(ty(t))
        };
        let base = schema_of("dbo.t", table(&[("n", with("int", "0"))]));
        let declared = schema_of("dbo.t", table(&[("n", with("nvarchar(10)", "'abc'"))]));
        let cs = run(&base, &declared, &[]);
        let at = |f: fn(&Change) -> bool| {
            cs.changes
                .iter()
                .position(|p| f(&p.change))
                .unwrap_or_else(|| panic!("not planned: {:#?}", cs.changes))
        };
        let out = at(|c| matches!(c, Change::AlterColumnDefault { to: None, .. }));
        let retype = at(|c| matches!(c, Change::AlterColumnType { .. }));
        let into = at(|c| matches!(c, Change::AlterColumnDefault { to: Some(_), .. }));
        assert!(
            out < retype && retype < into,
            "out {out}, retype {retype}, into {into}: {:#?}",
            cs.changes
        );
    }

    /// The negative half: a default replaced on a column that keeps its type
    /// stays one change. Nothing has to run between the halves, `SET DEFAULT`
    /// replaces on PostgreSQL, and SQL Server's emitter already drops and adds
    /// inside its own statement — so splitting would put two lines at opposite
    /// ends of a plan where one says it better.
    #[test]
    fn a_default_replaced_without_a_retype_stays_one_change() {
        let with = |t: &str, d: &str| Column {
            default: Some(d.to_owned()),
            ..Column::new(ty(t))
        };
        let base = schema_of("dbo.t", table(&[("n", with("int", "0"))]));
        let declared = schema_of("dbo.t", table(&[("n", with("int", "1"))]));
        let cs = run(&base, &declared, &[]);
        assert_eq!(
            cs.changes.len(),
            1,
            "one change, not two: {:#?}",
            cs.changes
        );
        assert!(
            matches!(
                cs.changes[0].change,
                Change::AlterColumnDefault {
                    from: Some(_),
                    to: Some(_),
                    ..
                }
            ),
            "{:#?}",
            cs.changes
        );
    }

    /// A key that is **replaced** is two changes, and they go to opposite ends
    /// of the plan: the old one's drop with the constraint drops, the new
    /// one's add after every column its shape may name.
    ///
    /// One change cannot do it. The add may name a column this same plan is
    /// still adding at class 8 — so a replacement could not simply join the
    /// drops — and the drop has to precede every column change a standing key
    /// blocks. The halves need opposite classes, so they are separate changes;
    /// both emitters already emitted the two statements independently, and
    /// only their positions move.
    #[test]
    fn a_replaced_key_is_dropped_before_its_old_columns_and_added_after_its_new_ones() {
        let mut base_t = table(&[("id", Column::new(ty("int")).not_null())]);
        base_t.primary_key = Some(pbps_model::PrimaryKey {
            name: Some("pk_t".to_owned()),
            columns: vec!["id".to_owned()],
            storage_parameters: Default::default(),
        });
        let base = schema_of("dbo.t", base_t);
        // The declaration replaces the key, adds the column the new key names,
        // and relaxes the column the old key held — every dependency in one
        // plan.
        let mut declared_t = table(&[
            ("id", Column::new(ty("int"))),
            ("other", Column::new(ty("int")).not_null()),
        ]);
        declared_t.primary_key = Some(pbps_model::PrimaryKey {
            name: Some("pk_t".to_owned()),
            columns: vec!["other".to_owned()],
            storage_parameters: Default::default(),
        });
        let declared = schema_of("dbo.t", declared_t);

        let cs = run(&base, &declared, &[]);
        let at = |f: fn(&Change) -> bool| {
            cs.changes
                .iter()
                .position(|p| f(&p.change))
                .unwrap_or_else(|| panic!("not planned: {:#?}", cs.changes))
        };
        let drop = at(|c| matches!(c, Change::SetPrimaryKey { to: None, .. }));
        let relaxed = at(|c| matches!(c, Change::AlterColumnNullability { .. }));
        let added = at(|c| matches!(c, Change::AddColumn { .. }));
        let add = at(|c| matches!(c, Change::SetPrimaryKey { from: None, .. }));
        assert!(
            drop < relaxed && added < add,
            "drop {drop}, relaxed {relaxed}, added {added}, add {add}: {:#?}",
            cs.changes
        );
    }

    /// A plan that renames a table **and** drops one of its columns names the
    /// column's table as the declaration does, because by the time the drop
    /// runs the rename has already happened.
    ///
    /// `order_key` puts `RenameTable` at 1 and `DropColumn` at 5, so the
    /// statements are emitted in that order. Built from the base side's
    /// `ColumnRef` — which every sibling change in that loop is not — the plan
    /// read `sp_rename 'dbo.customers', 'clients'` and then
    /// `ALTER TABLE [dbo].[customers] DROP COLUMN [doomed]`, which the engine
    /// refuses with `Invalid object name`. A valid, reviewed plan, refused.
    ///
    /// The column's *own* name stays the base one: a column this plan drops is
    /// absent from the declarations, so nothing renames it and the database
    /// still knows it by that name.
    #[test]
    fn a_dropped_column_names_the_table_the_rename_has_already_produced() {
        let base = schema_of(
            "dbo.customers",
            table(&[
                ("id", Column::new(ty("int"))),
                ("doomed", Column::new(ty("int"))),
            ]),
        );
        let declared = schema_of("dbo.clients", table(&[("id", Column::new(ty("int")))]));
        let cs = run(
            &base,
            &declared,
            &[
                Intent::RenameTable {
                    from: "dbo.customers".parse().unwrap(),
                    to: "dbo.clients".parse().unwrap(),
                },
                // Spelled with the *declared* table: `resolve_columns`
                // iterates the declarations, and the table rename is already
                // applied to the ids by the time it runs. Which is the same
                // reason the change below must carry that name.
                Intent::DropColumn {
                    column: "dbo.clients.doomed".parse().unwrap(),
                    reason: "no longer used".into(),
                },
            ],
        );

        let Some(Change::DropColumn { column, .. }) = cs
            .changes
            .iter()
            .map(|c| &c.change)
            .find(|c| matches!(c, Change::DropColumn { .. }))
        else {
            panic!("no DropColumn in {cs:?}");
        };
        assert_eq!(
            column.table,
            "dbo.clients".parse::<TableName>().unwrap(),
            "the drop must name the table the rename has already produced"
        );
        assert_eq!(column.name, "doomed", "the column itself is not renamed");

        // And the rename really does sort first, which is what makes the name
        // above the only correct one — the assertion is worthless if the
        // ordering ever reverses.
        let kinds: Vec<&str> = cs
            .changes
            .iter()
            .map(|c| match &c.change {
                Change::RenameTable { .. } => "RenameTable",
                Change::DropColumn { .. } => "DropColumn",
                _ => "other",
            })
            .collect();
        // Each position is unwrapped before they are compared. Left as
        // `Option`s, a plan that stopped emitting the rename would satisfy
        // `None < Some(_)` and pass — the absent rename is the very failure
        // this is here to catch, so it must not be the one shape that slips
        // through.
        let Some(rename) = kinds.iter().position(|k| *k == "RenameTable") else {
            panic!("no RenameTable in {kinds:?}");
        };
        let Some(drop) = kinds.iter().position(|k| *k == "DropColumn") else {
            panic!("no DropColumn in {kinds:?}");
        };
        assert!(rename < drop, "{kinds:?}");
    }

    /// A table renamed beside a column of its own: the plan's kinds, in order.
    ///
    /// The column rename is spelled with the table's **declared** name because
    /// `resolve_columns` iterates the declared schema, which is the whole
    /// reason the two changes are sorted against two different names for one
    /// table.
    fn rename_pair(from: &str, to: &str) -> Vec<String> {
        let base = schema_of(
            from,
            table(&[
                ("id", Column::new(ty("int"))),
                ("old", Column::new(ty("int"))),
            ]),
        );
        let declared = schema_of(
            to,
            table(&[
                ("id", Column::new(ty("int"))),
                ("new", Column::new(ty("int"))),
            ]),
        );
        let cs = run(
            &base,
            &declared,
            &[
                Intent::RenameTable {
                    from: from.parse().unwrap(),
                    to: to.parse().unwrap(),
                },
                Intent::RenameColumn {
                    table: to.parse().unwrap(),
                    from: "old".into(),
                    to: "new".into(),
                },
            ],
        );
        kinds(&cs)
    }

    /// A column rename runs after the rename of the table it names, even when
    /// the table's new name sorts before its old one.
    ///
    /// `RenameTable` answers `subject()` with its **`from`** — the old name —
    /// while `RenameColumn` carries the declared, post-rename table. Sharing
    /// one ordering class, the two were separated by that tiebreaker, so the
    /// alphabet decided: `"dbo.clients" < "dbo.customers"` put the column
    /// rename first and the plan emitted
    /// `sp_rename 'dbo.clients.old', 'new', 'COLUMN'` against a table that did
    /// not exist yet. A valid, reviewed plan the engine refuses.
    #[test]
    fn a_column_rename_follows_its_table_rename_though_the_new_name_sorts_first() {
        assert_eq!(
            rename_pair("dbo.customers", "dbo.clients"),
            ["RenameTable", "RenameColumn"],
            "the column rename must name a table that already exists"
        );
    }

    /// The same pair with the names the other way round, where the tiebreaker
    /// happened to give the right answer already.
    ///
    /// This is the case that passed before the fix, and it is here so the fix
    /// is not read as a coincidence of names: the order must come from the
    /// dependency, not from which spelling sorts lower.
    #[test]
    fn a_column_rename_follows_its_table_rename_though_the_old_name_sorts_first() {
        assert_eq!(
            rename_pair("dbo.clients", "dbo.customers"),
            ["RenameTable", "RenameColumn"],
            "the order must not depend on the alphabet"
        );
    }

    #[test]
    fn identical_schemas_produce_no_changes() {
        let s = schema_of("dbo.t", table(&[("a", Column::new(ty("int")))]));
        assert!(run(&s, &s, &[]).is_empty());
    }

    #[test]
    fn widening_a_type_carries_no_risk() {
        let base = schema_of("dbo.t", table(&[("a", Column::new(ty("nvarchar(50)")))]));
        let want = schema_of("dbo.t", table(&[("a", Column::new(ty("nvarchar(100)")))]));
        let cs = run(&base, &want, &[]);

        assert_eq!(cs.changes.len(), 1);
        assert!(cs.risks().is_empty(), "widening a length needs no approval");
    }

    #[test]
    fn narrowing_a_type_requires_approval() {
        let base = schema_of("dbo.t", table(&[("a", Column::new(ty("nvarchar(100)")))]));
        let want = schema_of("dbo.t", table(&[("a", Column::new(ty("nvarchar(50)")))]));
        let cs = run(&base, &want, &[]);

        assert!(cs.risks().contains(&RiskClass::Narrowing));
        assert_eq!(
            cs.unapproved_risks(&Default::default()),
            [RiskClass::Narrowing].into_iter().collect()
        );
    }

    /// A case difference is not a change, or retyping a type would produce a
    /// phantom diff every time.
    #[test]
    fn type_case_difference_is_not_a_change() {
        let base = schema_of("dbo.t", table(&[("a", Column::new(ty("NVARCHAR(100)")))]));
        let want = schema_of("dbo.t", table(&[("a", Column::new(ty("nvarchar(100)")))]));
        assert!(run(&base, &want, &[]).is_empty());
    }

    #[test]
    fn tightening_nullability_requires_approval() {
        let base = schema_of("dbo.t", table(&[("a", Column::new(ty("int")))]));
        let want = schema_of("dbo.t", table(&[("a", Column::new(ty("int")).not_null())]));
        let cs = run(&base, &want, &[]);

        assert!(cs.risks().contains(&RiskClass::NotNull));
    }

    #[test]
    fn loosening_nullability_is_safe() {
        let base = schema_of("dbo.t", table(&[("a", Column::new(ty("int")).not_null())]));
        let want = schema_of("dbo.t", table(&[("a", Column::new(ty("int")))]));
        assert!(run(&base, &want, &[]).risks().is_empty());
    }

    #[test]
    fn dropping_a_column_is_destructive() {
        let base = schema_of(
            "dbo.t",
            table(&[("a", Column::new(ty("int"))), ("b", Column::new(ty("int")))]),
        );
        let want = schema_of("dbo.t", table(&[("a", Column::new(ty("int")))]));
        let intents = vec![Intent::DropColumn {
            column: "dbo.t.b".parse().unwrap(),
            reason: "no longer in use".into(),
        }];
        let cs = run(&base, &want, &intents);

        assert_eq!(kinds(&cs), ["DropColumn"]);
        assert!(cs.risks().contains(&RiskClass::Destructive));
    }

    /// A rename must produce exactly one RenameColumn, never an add and a drop
    /// alongside it.
    #[test]
    fn renaming_produces_exactly_one_change() {
        let base = schema_of("dbo.t", table(&[("old", Column::new(ty("int")))]));
        let want = schema_of("dbo.t", table(&[("new", Column::new(ty("int")))]));
        let intents = vec![Intent::RenameColumn {
            table: "dbo.t".parse().unwrap(),
            from: "old".into(),
            to: "new".into(),
        }];
        let cs = run(&base, &want, &intents);

        assert_eq!(kinds(&cs), ["RenameColumn"]);
        assert!(cs.risks().contains(&RiskClass::Rename));
    }

    /// A rename plus a type change: with identity mapped correctly, the type
    /// comparison must be against the old column.
    #[test]
    fn rename_and_retype_are_both_detected() {
        let base = schema_of("dbo.t", table(&[("old", Column::new(ty("nvarchar(100)")))]));
        let want = schema_of("dbo.t", table(&[("new", Column::new(ty("nvarchar(50)")))]));
        let intents = vec![Intent::RenameColumn {
            table: "dbo.t".parse().unwrap(),
            from: "old".into(),
            to: "new".into(),
        }];
        let cs = run(&base, &want, &intents);

        assert_eq!(kinds(&cs), ["RenameColumn", "AlterColumnType"]);
        assert!(cs.risks().contains(&RiskClass::Narrowing));
    }

    /// A new table's columns ride along in CreateTable; no per-column AddColumn
    /// should be emitted as well.
    #[test]
    fn new_table_does_not_also_emit_add_column() {
        let base = Schema::default();
        let want = schema_of(
            "dbo.t",
            table(&[("a", Column::new(ty("int"))), ("b", Column::new(ty("int")))]),
        );
        assert_eq!(kinds(&run(&base, &want, &[])), ["CreateTable"]);
    }

    #[test]
    fn changed_index_becomes_drop_then_add() {
        let ix = |c: &str| Index {
            columns: vec![IndexColumn {
                key: pbps_model::IndexKey::Column(c.into()),
                descending: false,
                opclass: None,
            }],
            include: vec![],
            unique: false,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
        };
        let mut base_t = table(&[("a", Column::new(ty("int"))), ("b", Column::new(ty("int")))]);
        base_t.indexes.insert("ix_t".into(), ix("a"));
        let mut want_t = base_t.clone();
        want_t.indexes.insert("ix_t".into(), ix("b"));

        let cs = run(
            &schema_of("dbo.t", base_t),
            &schema_of("dbo.t", want_t),
            &[],
        );
        assert_eq!(kinds(&cs), ["DropIndex", "AddIndex"]);
    }

    /// The order has to be safely executable: renames first, dropping constraints
    /// before dropping columns, adding constraints last.
    /// A schema of two tables, for the foreign-key ordering cases below.
    fn two_tables(a: (&str, Table), b: (&str, Table)) -> Schema {
        let mut s = Schema::default();
        s.tables.insert(a.0.parse().unwrap(), a.1);
        s.tables.insert(b.0.parse().unwrap(), b.1);
        s
    }

    /// A table with one `sku` column, plus whatever the caller adds.
    fn sku_table() -> Table {
        table(&[
            ("id", Column::new(ty("int"))),
            ("sku", Column::new(ty("int"))),
        ])
    }

    fn unique(columns: &[&str]) -> UniqueConstraint {
        UniqueConstraint {
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            storage_parameters: Default::default(),
        }
    }

    fn fk(columns: &[&str], to: &str, to_columns: &[&str]) -> ForeignKey {
        ForeignKey {
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            references_table: to.parse().unwrap(),
            references_columns: to_columns.iter().map(|c| (*c).to_string()).collect(),
            on_delete: ReferentialAction::default(),
            on_update: ReferentialAction::default(),
        }
    }

    #[test]
    fn replacing_a_referenced_key_surrounds_it_with_visible_foreign_key_changes() {
        for kind in ["index", "unique", "primary"] {
            let mut parent = sku_table();
            if kind == "primary" {
                parent.primary_key = Some(PrimaryKey {
                    name: Some("old_key".into()),
                    columns: vec!["id".into()],
                    storage_parameters: Default::default(),
                });
            } else if kind == "index" {
                parent.indexes.insert(
                    "old_key".into(),
                    pbps_model::Index {
                        columns: vec![pbps_model::IndexColumn {
                            key: pbps_model::IndexKey::Column("id".into()),
                            descending: false,
                            opclass: None,
                        }],
                        include: vec![],
                        unique: true,
                        filter: None,
                        method: Default::default(),
                        storage_parameters: Default::default(),
                    },
                );
            } else {
                parent.unique.insert("old_key".into(), unique(&["id"]));
            }
            parent.unique.insert("unrelated".into(), unique(&["sku"]));
            let mut child = sku_table();
            child
                .foreign_keys
                .insert("fk_kept".into(), fk(&["id"], "dbo.parent", &["id"]));
            child
                .foreign_keys
                .insert("fk_other".into(), fk(&["sku"], "dbo.parent", &["sku"]));
            let base = two_tables(("dbo.parent", parent), ("dbo.child", child));
            assert!(run(&base, &base, &[]).is_empty());
            let mut declared = base.clone();
            let parent = declared
                .tables
                .get_mut(&"dbo.parent".parse().unwrap())
                .unwrap();
            if kind == "primary" {
                parent.primary_key.as_mut().unwrap().name = Some("new_key".into());
            } else if kind == "index" {
                let index = parent.indexes.remove("old_key").unwrap();
                parent.indexes.insert("new_key".into(), index);
            } else {
                let key = parent.unique.remove("old_key").unwrap();
                parent.unique.insert("new_key".into(), key);
            }
            let cs = run(&base, &declared, &[]);
            assert_eq!(
                kinds(&cs),
                if kind == "primary" {
                    vec![
                        "DropForeignKey",
                        "SetPrimaryKey",
                        "SetPrimaryKey",
                        "AddForeignKey",
                    ]
                } else if kind == "index" {
                    vec!["DropForeignKey", "DropIndex", "AddIndex", "AddForeignKey"]
                } else {
                    vec!["DropForeignKey", "DropUnique", "AddUnique", "AddForeignKey"]
                },
                "{cs:?}"
            );
            assert!(
                matches!(&cs.changes[0].change, Change::DropForeignKey {name, ..} if name == "fk_kept")
            );
            assert!(
                matches!(&cs.changes[3].change, Change::AddForeignKey {name, constraint, ..}
                if name == "fk_kept" && **constraint == base.tables[&"dbo.child".parse().unwrap()].foreign_keys["fk_kept"])
            );
            assert_eq!(cs.changes[0].risks, cs.changes[0].change.intrinsic_risks());
            assert!(
                cs.changes[3]
                    .change
                    .intrinsic_risks()
                    .contains(&pbps_model::RiskClass::Constraint)
            );

            // An explicit FK removal must not be duplicated or recreated.
            declared
                .tables
                .get_mut(&"dbo.child".parse().unwrap())
                .unwrap()
                .foreign_keys
                .remove("fk_kept");
            let removed = run(&base, &declared, &[]);
            assert_eq!(
                removed
                    .changes
                    .iter()
                    .filter(|p| matches!(p.change, Change::DropForeignKey { .. }))
                    .count(),
                1
            );
            assert!(
                !removed
                    .changes
                    .iter()
                    .any(|p| matches!(p.change, Change::AddForeignKey { .. }))
            );
        }
    }

    #[test]
    fn replacing_indexes_that_cannot_back_foreign_keys_keeps_the_foreign_keys() {
        for (unique, filter) in [(false, None), (true, Some("id > 0"))] {
            let mut parent = sku_table();
            parent.primary_key = Some(PrimaryKey {
                name: Some("parent_pk".into()),
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            });
            parent.indexes.insert(
                "old_index".into(),
                pbps_model::Index {
                    columns: vec![pbps_model::IndexColumn {
                        key: pbps_model::IndexKey::Column("id".into()),
                        descending: false,
                        opclass: None,
                    }],
                    include: vec![],
                    unique,
                    filter: filter.map(str::to_owned),
                    method: Default::default(),
                    storage_parameters: Default::default(),
                },
            );
            let mut child = sku_table();
            child
                .foreign_keys
                .insert("child_fk".into(), fk(&["id"], "dbo.parent", &["id"]));
            let base = two_tables(("dbo.parent", parent), ("dbo.child", child));
            let mut declared = base.clone();
            let parent = declared
                .tables
                .get_mut(&"dbo.parent".parse().unwrap())
                .unwrap();
            let index = parent.indexes.remove("old_index").unwrap();
            parent.indexes.insert("new_index".into(), index);
            let cs = run(&base, &declared, &[]);
            assert_eq!(kinds(&cs), vec!["DropIndex", "AddIndex"], "{cs:?}");
        }
    }

    #[test]
    fn composite_and_self_references_follow_table_and_column_identity_during_key_replacement() {
        let mut parent = sku_table();
        parent
            .unique
            .insert("old_key".into(), unique(&["id", "sku"]));
        parent.foreign_keys.insert(
            "self_fk".into(),
            fk(&["id", "sku"], "dbo.parent", &["id", "sku"]),
        );
        let mut child = sku_table();
        child.foreign_keys.insert(
            "child_fk".into(),
            fk(&["id", "sku"], "dbo.parent", &["sku", "id"]),
        );
        let base = two_tables(("dbo.parent", parent), ("dbo.child", child));
        let intents = [
            Intent::RenameTable {
                from: "dbo.parent".parse().unwrap(),
                to: "dbo.parent_new".parse().unwrap(),
            },
            Intent::RenameTable {
                from: "dbo.child".parse().unwrap(),
                to: "dbo.child_new".parse().unwrap(),
            },
            Intent::RenameColumn {
                table: "dbo.parent_new".parse().unwrap(),
                from: "id".into(),
                to: "code".into(),
            },
        ];
        for replace in [false, true] {
            let mut parent = base.tables[&"dbo.parent".parse().unwrap()].clone();
            let column = parent.columns.shift_remove("id").unwrap();
            parent.columns.insert("code".into(), column);
            parent.unique.clear();
            parent.unique.insert(
                if replace { "new_key" } else { "old_key" }.into(),
                unique(&["code", "sku"]),
            );
            parent.foreign_keys.insert(
                "self_fk".into(),
                fk(&["code", "sku"], "dbo.parent_new", &["code", "sku"]),
            );
            let mut child = base.tables[&"dbo.child".parse().unwrap()].clone();
            child.foreign_keys.insert(
                "child_fk".into(),
                fk(&["id", "sku"], "dbo.parent_new", &["sku", "code"]),
            );
            let declared = two_tables(("dbo.parent_new", parent), ("dbo.child_new", child));
            let cs = run(&base, &declared, &intents);
            let drops: Vec<_> = cs
                .changes
                .iter()
                .enumerate()
                .filter(|(_, p)| matches!(p.change, Change::DropForeignKey { .. }))
                .collect();
            let adds: Vec<_> = cs
                .changes
                .iter()
                .enumerate()
                .filter(|(_, p)| matches!(p.change, Change::AddForeignKey { .. }))
                .collect();
            assert_eq!(drops.len(), if replace { 2 } else { 0 }, "{cs:?}");
            assert_eq!(adds.len(), drops.len());
            if replace {
                let key_drop = cs
                    .changes
                    .iter()
                    .position(|p| matches!(p.change, Change::DropUnique { .. }))
                    .unwrap();
                let key_add = cs
                    .changes
                    .iter()
                    .position(|p| matches!(p.change, Change::AddUnique { .. }))
                    .unwrap();
                assert!(drops.iter().all(|(i, _)| *i < key_drop));
                for (i, p) in adds {
                    assert!(i > key_add);
                    let Change::AddForeignKey {
                        table,
                        name,
                        constraint,
                    } = &p.change
                    else {
                        unreachable!()
                    };
                    assert_eq!(
                        constraint.as_ref(),
                        &declared.tables[table].foreign_keys[name]
                    );
                }
            }
        }
    }

    /// A foreign key is added after the key it references, even when the
    /// referencing table's name sorts first.
    ///
    /// Both changes are in the addition class and both took
    /// `dependency_rank` 0, so `subject()` — the table name — decided:
    /// `"dbo.order_line" < "dbo.product"` put the foreign key first, and the
    /// engine refused it with 1776, "There are no primary or candidate keys in
    /// the referenced table ... that match the referencing column list"
    /// (measured on the pinned image).
    #[test]
    fn a_foreign_key_is_added_after_the_key_it_references() {
        let base = two_tables(
            ("dbo.product", sku_table()),
            ("dbo.order_line", sku_table()),
        );

        let mut product = sku_table();
        product
            .unique
            .insert("uq_product_sku".into(), unique(&["sku"]));
        let mut order_line = sku_table();
        order_line.foreign_keys.insert(
            "fk_ol_product".into(),
            fk(&["sku"], "dbo.product", &["sku"]),
        );
        let declared = two_tables(("dbo.product", product), ("dbo.order_line", order_line));

        assert_eq!(
            kinds(&run(&base, &declared, &[])),
            ["AddUnique", "AddForeignKey"],
            "the key must exist before the foreign key names it"
        );
    }

    /// A unique **index** is a supplier too. Measured: SQL Server accepts a
    /// foreign key referencing a plain `CREATE UNIQUE INDEX`, with no `UNIQUE`
    /// constraint anywhere — so ranking only the constraint pair would leave
    /// this half of it broken.
    #[test]
    fn a_foreign_key_is_added_after_the_unique_index_it_references() {
        let base = two_tables(
            ("dbo.product", sku_table()),
            ("dbo.order_line", sku_table()),
        );

        let mut product = sku_table();
        product.indexes.insert(
            "ux_product_sku".into(),
            Index {
                columns: vec![IndexColumn {
                    key: pbps_model::IndexKey::Column("sku".into()),
                    descending: false,
                    opclass: None,
                }],
                include: vec![],
                unique: true,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        let mut order_line = sku_table();
        order_line.foreign_keys.insert(
            "fk_ol_product".into(),
            fk(&["sku"], "dbo.product", &["sku"]),
        );
        let declared = two_tables(("dbo.product", product), ("dbo.order_line", order_line));

        assert_eq!(
            kinds(&run(&base, &declared, &[])),
            ["AddIndex", "AddForeignKey"],
            "a unique index is a candidate key, so it must come first too"
        );
    }

    /// The mirror: a foreign key is dropped before the key it references.
    ///
    /// Names chosen so the alphabet gives the wrong answer —
    /// `"dbo.customer" < "dbo.order_x"` — which is what put the `DropUnique`
    /// first. Measured, the engine refuses that with 3727 (and 3723 for the
    /// index form): the constraint is being referenced by the foreign key.
    #[test]
    fn a_foreign_key_is_dropped_before_the_key_it_references() {
        let mut customer = sku_table();
        customer
            .unique
            .insert("uq_customer_sku".into(), unique(&["sku"]));
        let mut order_x = sku_table();
        order_x.foreign_keys.insert(
            "fk_ox_customer".into(),
            fk(&["sku"], "dbo.customer", &["sku"]),
        );
        let base = two_tables(("dbo.customer", customer), ("dbo.order_x", order_x));

        let declared = two_tables(("dbo.customer", sku_table()), ("dbo.order_x", sku_table()));

        assert_eq!(
            kinds(&run(&base, &declared, &[])),
            ["DropForeignKey", "DropUnique"],
            "the referencing constraint must go before the key it holds down"
        );
    }

    /// A check constraint has no such relation, and must not be dragged around
    /// by the rank that orders the foreign keys.
    ///
    /// The negative case: with the foreign key ranked, everything else in the
    /// class has to keep the tiebreaker it had, or the rank has quietly become
    /// a reordering of changes that never depended on each other.
    #[test]
    fn a_check_constraint_keeps_the_order_its_name_gives_it() {
        let base = two_tables(("dbo.aaa", sku_table()), ("dbo.zzz", sku_table()));

        let mut aaa = sku_table();
        aaa.checks.insert(
            "ck_aaa".into(),
            CheckConstraint {
                expression: "sku > 0".into(),
            },
        );
        let mut zzz = sku_table();
        zzz.checks.insert(
            "ck_zzz".into(),
            CheckConstraint {
                expression: "sku > 0".into(),
            },
        );
        let declared = two_tables(("dbo.aaa", aaa), ("dbo.zzz", zzz));

        let cs = run(&base, &declared, &[]);
        let tables: Vec<String> = cs.changes.iter().map(|p| p.change.subject()).collect();
        assert_eq!(
            tables,
            ["dbo.aaa", "dbo.zzz"],
            "two unrelated checks keep the table-name tiebreaker"
        );
    }

    /// A column rename does not restate the primary key that names it.
    ///
    /// `diff_constraints` compares column *lists*, and the base side's are the
    /// pre-rename names, so renaming a key column made `pk_differs` true and
    /// the plan carried a `SetPrimaryKey` beside the rename. The emitter
    /// renders that as `DROP CONSTRAINT` + `ADD PRIMARY KEY`, which the engine
    /// refuses outright when a foreign key references the key (3727,
    /// measured), adds `Constraint` risk to a plan approved as a rename, and
    /// re-mints the name the engine invented for an unnamed key — the very
    /// churn the unnamed-match rule above `pk_differs` exists to prevent.
    ///
    /// Measured: `sp_rename` on a key column is accepted and the primary key
    /// follows it, keeping its invented name.
    #[test]
    fn renaming_a_key_column_does_not_restate_the_primary_key() {
        let mut base_t = sku_table();
        base_t.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        });
        let mut want_t = table(&[
            ("cust_id", Column::new(ty("int"))),
            ("sku", Column::new(ty("int"))),
        ]);
        want_t.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["cust_id".into()],
            storage_parameters: Default::default(),
        });

        let cs = run(
            &schema_of("dbo.t", base_t),
            &schema_of("dbo.t", want_t),
            &[Intent::RenameColumn {
                table: "dbo.t".parse().unwrap(),
                from: "id".into(),
                to: "cust_id".into(),
            }],
        );
        assert_eq!(kinds(&cs), ["RenameColumn"], "{cs:?}");
    }

    /// The same for an index and a unique constraint, and the risk the plan
    /// carries is the point: `DropIndex` is `Destructive`, so a rename-only
    /// revision demanded `--allow destructive` at the gate for a change the
    /// user never asked for.
    ///
    /// Measured: a column rename is accepted and the index — key and
    /// `INCLUDE` alike — and the unique constraint follow it.
    #[test]
    fn renaming_an_indexed_column_does_not_restate_the_index() {
        let mut base_t = table(&[
            ("id", Column::new(ty("int"))),
            ("old", Column::new(ty("int"))),
            ("note", Column::new(ty("int"))),
        ]);
        base_t.indexes.insert(
            "ix_t_old".into(),
            Index {
                columns: vec![IndexColumn {
                    key: pbps_model::IndexKey::Column("old".into()),
                    descending: false,
                    opclass: None,
                }],
                include: vec!["note".into()],
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        base_t.unique.insert("uq_t_old".into(), unique(&["old"]));

        let mut want_t = table(&[
            ("id", Column::new(ty("int"))),
            ("new", Column::new(ty("int"))),
            ("note", Column::new(ty("int"))),
        ]);
        want_t.indexes.insert(
            "ix_t_old".into(),
            Index {
                columns: vec![IndexColumn {
                    key: pbps_model::IndexKey::Column("new".into()),
                    descending: false,
                    opclass: None,
                }],
                include: vec!["note".into()],
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        want_t.unique.insert("uq_t_old".into(), unique(&["new"]));

        let cs = run(
            &schema_of("dbo.t", base_t),
            &schema_of("dbo.t", want_t),
            &[Intent::RenameColumn {
                table: "dbo.t".parse().unwrap(),
                from: "old".into(),
                to: "new".into(),
            }],
        );
        assert_eq!(kinds(&cs), ["RenameColumn"], "{cs:?}");
        let risks: Vec<RiskClass> = cs.changes.iter().flat_map(|p| p.risks.clone()).collect();
        assert_eq!(
            risks,
            [RiskClass::Rename],
            "a rename-only revision must not ask for --allow destructive"
        );
    }

    /// A **table** rename does it one level out: `references_table` changes on
    /// every child, so tables the revision never mentions got a foreign key
    /// dropped and re-added.
    ///
    /// Measured: `sp_rename` of a table carries the child foreign keys — the
    /// child reports the new name immediately after.
    #[test]
    fn renaming_a_table_does_not_restate_the_foreign_keys_that_reference_it() {
        let mut child = sku_table();
        child
            .foreign_keys
            .insert("fk_c".into(), fk(&["sku"], "dbo.parent", &["sku"]));
        let mut parent = sku_table();
        parent.unique.insert("uq_p".into(), unique(&["sku"]));
        let base = two_tables(("dbo.parent", parent.clone()), ("dbo.child", child));

        let mut child2 = sku_table();
        child2
            .foreign_keys
            .insert("fk_c".into(), fk(&["sku"], "dbo.ancestor", &["sku"]));
        let declared = two_tables(("dbo.ancestor", parent), ("dbo.child", child2));

        let cs = run(
            &base,
            &declared,
            &[Intent::RenameTable {
                from: "dbo.parent".parse().unwrap(),
                to: "dbo.ancestor".parse().unwrap(),
            }],
        );
        assert_eq!(kinds(&cs), ["RenameTable"], "{cs:?}");
    }

    /// The negative case: a genuine change of membership still produces the
    /// restatement, and a check constraint whose expression names the renamed
    /// column still produces one too.
    ///
    /// The rebased comparison must suppress only the spelling. A check's
    /// expression is opaque text this tool never rewrites, and measured, the
    /// engine refuses `sp_rename` on a column a check names at all (15336), so
    /// its drop and re-add is the only way the rename can happen — not churn.
    #[test]
    fn a_real_membership_change_and_a_check_still_restate_themselves() {
        let mut base_t = table(&[
            ("id", Column::new(ty("int"))),
            ("old", Column::new(ty("int"))),
            ("other", Column::new(ty("int"))),
        ]);
        base_t.unique.insert("uq_t".into(), unique(&["old"]));
        base_t.checks.insert(
            "ck_t".into(),
            CheckConstraint {
                expression: "old > 0".into(),
            },
        );

        let mut want_t = table(&[
            ("id", Column::new(ty("int"))),
            ("new", Column::new(ty("int"))),
            ("other", Column::new(ty("int"))),
        ]);
        // The unique now covers a second column: a real change, not a spelling.
        want_t
            .unique
            .insert("uq_t".into(), unique(&["new", "other"]));
        want_t.checks.insert(
            "ck_t".into(),
            CheckConstraint {
                expression: "new > 0".into(),
            },
        );

        let cs = run(
            &schema_of("dbo.t", base_t),
            &schema_of("dbo.t", want_t),
            &[Intent::RenameColumn {
                table: "dbo.t".parse().unwrap(),
                from: "old".into(),
                to: "new".into(),
            }],
        );
        assert_eq!(
            kinds(&cs),
            [
                "DropCheck",
                "DropUnique",
                "RenameColumn",
                "AddCheck",
                "AddUnique"
            ],
            "{cs:?}"
        );
    }

    /// A check constraint is dropped **before** the rename it blocks.
    ///
    /// `sp_rename` on a column a check names is refused outright — 15336,
    /// `the object participates in enforced dependencies` (measured) — so the
    /// drop and re-add #123 leaves in place is not enough on its own: it has
    /// to happen in that order. With `DropCheck` after `RenameColumn` the plan
    /// renamed first and the engine refused the statement, which is a valid,
    /// reviewed plan refused.
    ///
    /// The re-add stays where it was, in the constraint class after the
    /// renames, which is where the new expression can be written.
    #[test]
    fn a_check_is_dropped_before_the_rename_it_blocks() {
        let mut base_t = table(&[
            ("id", Column::new(ty("int"))),
            ("old", Column::new(ty("int"))),
        ]);
        base_t.checks.insert(
            "ck_t".into(),
            CheckConstraint {
                expression: "old > 0".into(),
            },
        );
        let mut want_t = table(&[
            ("id", Column::new(ty("int"))),
            ("new", Column::new(ty("int"))),
        ]);
        want_t.checks.insert(
            "ck_t".into(),
            CheckConstraint {
                expression: "new > 0".into(),
            },
        );

        let cs = run(
            &schema_of("dbo.t", base_t),
            &schema_of("dbo.t", want_t),
            &[Intent::RenameColumn {
                table: "dbo.t".parse().unwrap(),
                from: "old".into(),
                to: "new".into(),
            }],
        );
        assert_eq!(
            kinds(&cs),
            ["DropCheck", "RenameColumn", "AddCheck"],
            "the check has to be gone before the engine will rename the column"
        );
    }

    /// The same for a filtered index, which the engine refuses with 5074,
    /// `the index is dependent on column`, and 4922 behind it, `one or more
    /// objects access this column`. A driver sees the first, `TRY`/`CATCH` the
    /// last; both are the same refusal.
    ///
    /// The predicate is opaque text this tool never rewrites, so the two sides
    /// differ on it and the drop-and-add is real; only its order was wrong.
    #[test]
    fn a_filtered_index_is_dropped_before_the_rename_it_blocks() {
        let ix = |column: &str| Index {
            columns: vec![IndexColumn {
                key: pbps_model::IndexKey::Column(column.into()),
                descending: false,
                opclass: None,
            }],
            include: vec![],
            unique: false,
            filter: Some(format!("[{column}] IS NOT NULL")),
            method: Default::default(),
            storage_parameters: Default::default(),
        };
        let mut base_t = table(&[
            ("id", Column::new(ty("int"))),
            ("old", Column::new(ty("int"))),
        ]);
        base_t.indexes.insert("ix_t".into(), ix("old"));
        let mut want_t = table(&[
            ("id", Column::new(ty("int"))),
            ("new", Column::new(ty("int"))),
        ]);
        want_t.indexes.insert("ix_t".into(), ix("new"));

        let cs = run(
            &schema_of("dbo.t", base_t),
            &schema_of("dbo.t", want_t),
            &[Intent::RenameColumn {
                table: "dbo.t".parse().unwrap(),
                from: "old".into(),
                to: "new".into(),
            }],
        );
        assert_eq!(
            kinds(&cs),
            ["DropIndex", "RenameColumn", "AddIndex"],
            "the filtered index has to be gone before the rename"
        );
    }

    /// And the drops still come *after* the table rename, because they name
    /// the table: measured, a table rename is not blocked by either a check or
    /// a filtered index, so there is nothing to gain by moving them ahead of
    /// it — and a drop emitted before `sp_rename` would name a table that no
    /// longer exists, which is the shape #118 fixed.
    #[test]
    fn the_drops_that_unblock_a_rename_still_follow_the_table_rename() {
        let mut base_t = table(&[
            ("id", Column::new(ty("int"))),
            ("old", Column::new(ty("int"))),
        ]);
        base_t.checks.insert(
            "ck_t".into(),
            CheckConstraint {
                expression: "old > 0".into(),
            },
        );
        let mut want_t = table(&[
            ("id", Column::new(ty("int"))),
            ("new", Column::new(ty("int"))),
        ]);
        want_t.checks.insert(
            "ck_t".into(),
            CheckConstraint {
                expression: "new > 0".into(),
            },
        );

        let cs = run(
            &schema_of("dbo.customers", base_t),
            &schema_of("dbo.clients", want_t),
            &[
                Intent::RenameTable {
                    from: "dbo.customers".parse().unwrap(),
                    to: "dbo.clients".parse().unwrap(),
                },
                Intent::RenameColumn {
                    table: "dbo.clients".parse().unwrap(),
                    from: "old".into(),
                    to: "new".into(),
                },
            ],
        );
        assert_eq!(
            kinds(&cs),
            ["RenameTable", "DropCheck", "RenameColumn", "AddCheck"],
            "the drop names the table, so it follows the rename that gives it \
             that name"
        );
    }

    /// A dialect that keeps an index in the schema's relation namespace —
    /// PostgreSQL's answer (issue #176). Everything else is `MinimalDialect`'s,
    /// so what a test using it shows is exactly the difference this one
    /// capability makes to the plan's statement order.
    #[derive(Debug, Clone, Copy, Default)]
    struct SharesIndexNamespace;

    impl Dialect for SharesIndexNamespace {
        fn name(&self) -> &'static str {
            "shares-index-namespace"
        }
        fn indexes_share_namespace_with_tables(&self) -> bool {
            true
        }
        fn constraints_share_namespace_with_tables(&self) -> bool {
            false
        }
        fn quote_ident(&self, ident: &str) -> Result<String, pbps_dialect::DialectError> {
            MinimalDialect.quote_ident(ident)
        }
        fn emit(
            &self,
            change: &Change,
            strategy: pbps_model::Strategy,
        ) -> Result<Vec<pbps_dialect::Statement>, pbps_dialect::DialectError> {
            MinimalDialect.emit(change, strategy)
        }
        fn normalize_type(
            &self,
            ty: &pbps_model::ColumnType,
        ) -> Result<pbps_model::ColumnType, pbps_dialect::DialectError> {
            MinimalDialect.normalize_type(ty)
        }
        fn type_change_risk(
            &self,
            from: &pbps_model::ColumnType,
            to: &pbps_model::ColumnType,
        ) -> pbps_dialect::TypeChangeRisk {
            MinimalDialect.type_change_risk(from, to)
        }
        fn fold_ident<'a>(&self, ident: &'a str) -> std::borrow::Cow<'a, str> {
            MinimalDialect.fold_ident(ident)
        }
        fn lexicon(&self) -> pbps_dialect::Lexicon {
            MinimalDialect.lexicon()
        }
        fn validate_table(
            &self,
            name: &pbps_model::TableName,
            table: &Table,
        ) -> Vec<pbps_dialect::DialectError> {
            MinimalDialect.validate_table(name, table)
        }
        fn transaction_framing(&self) -> pbps_dialect::TransactionFraming {
            MinimalDialect.transaction_framing()
        }
        fn probe_framing(&self) -> Option<pbps_dialect::TransactionFraming> {
            MinimalDialect.probe_framing()
        }
    }

    /// A dialect where constraints share the schema's namespace and a column
    /// default's constraint is named by the engine, `df_<table>_<column>`:
    /// SQL Server's shape, without its digests. Everything else is
    /// `MinimalDialect`'s.
    #[derive(Debug, Clone, Copy, Default)]
    struct NamesItsDefaults;

    impl Dialect for NamesItsDefaults {
        fn name(&self) -> &'static str {
            "names-its-defaults"
        }
        fn indexes_share_namespace_with_tables(&self) -> bool {
            false
        }
        fn constraints_share_namespace_with_tables(&self) -> bool {
            true
        }
        fn generated_constraint_names(&self, name: &TableName, table: &Table) -> Vec<String> {
            table
                .columns
                .iter()
                .filter(|(_, c)| c.default.is_some())
                .map(|(column, _)| format!("df_{}_{column}", name.name))
                .collect()
        }
        fn quote_ident(&self, ident: &str) -> Result<String, pbps_dialect::DialectError> {
            MinimalDialect.quote_ident(ident)
        }
        fn emit(
            &self,
            change: &Change,
            strategy: pbps_model::Strategy,
        ) -> Result<Vec<pbps_dialect::Statement>, pbps_dialect::DialectError> {
            MinimalDialect.emit(change, strategy)
        }
        fn normalize_type(
            &self,
            ty: &pbps_model::ColumnType,
        ) -> Result<pbps_model::ColumnType, pbps_dialect::DialectError> {
            MinimalDialect.normalize_type(ty)
        }
        fn type_change_risk(
            &self,
            from: &pbps_model::ColumnType,
            to: &pbps_model::ColumnType,
        ) -> pbps_dialect::TypeChangeRisk {
            MinimalDialect.type_change_risk(from, to)
        }
        fn fold_ident<'a>(&self, ident: &'a str) -> std::borrow::Cow<'a, str> {
            MinimalDialect.fold_ident(ident)
        }
        fn lexicon(&self) -> pbps_dialect::Lexicon {
            MinimalDialect.lexicon()
        }
        fn validate_table(
            &self,
            name: &pbps_model::TableName,
            table: &Table,
        ) -> Vec<pbps_dialect::DialectError> {
            MinimalDialect.validate_table(name, table)
        }
        fn transaction_framing(&self) -> pbps_dialect::TransactionFraming {
            MinimalDialect.transaction_framing()
        }
        fn probe_framing(&self) -> Option<pbps_dialect::TransactionFraming> {
            MinimalDialect.probe_framing()
        }
    }

    /// The finding from round 3 of #462 (issue #176): a table renamed onto a
    /// name a *different* table's dropped index vacates in the same plan is a
    /// declaration whose final schema `check_index_names` sees no collision
    /// in at all — and until this test, the plan sorter put `RenameTable`
    /// (class 1) ahead of `DropIndex` (class 2) regardless, so the emitted
    /// `ALTER TABLE ... RENAME TO "target"` ran while `app.other`'s index
    /// `target` still existed.
    ///
    /// Measured end to end on PostgreSQL 18.6: bootstrap `app.old` and
    /// `app.other` (with its index `target`), then plan renaming `app.old` to
    /// `app.target` while dropping that index — the live apply failed with
    /// `42P07: relation "target" already exists` before this fix, and
    /// succeeds after it, in the order this test pins.
    ///
    /// On SQL Server an index name is scoped to its own table
    /// (`indexes_share_namespace_with_tables` is `false`), so the same plan
    /// never collides there and the drop stays in its ordinary class-2 place
    /// — `MinimalDialect` is that answer, and the second assertion pins it.
    #[test]
    fn a_drop_that_frees_a_renamed_targets_name_precedes_the_rename_where_indexes_share_it() {
        let old_t = table(&[("id", Column::new(ty("int")))]);
        let mut other_base = table(&[("n", Column::new(ty("int")))]);
        other_base.indexes.insert(
            "target".into(),
            Index {
                columns: vec![IndexColumn {
                    key: pbps_model::IndexKey::Column("n".into()),
                    descending: false,
                    opclass: None,
                }],
                include: vec![],
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        let base = two_tables(("app.old", old_t.clone()), ("app.other", other_base));

        let target_t = old_t;
        let other_declared = table(&[("n", Column::new(ty("int")))]);
        let declared = two_tables(("app.target", target_t), ("app.other", other_declared));

        let intents = [Intent::RenameTable {
            from: "app.old".parse().unwrap(),
            to: "app.target".parse().unwrap(),
        }];

        let cs = run_with(&SharesIndexNamespace, &base, &declared, &intents);
        assert_eq!(
            kinds(&cs),
            ["DropIndex", "RenameTable"],
            "the index has to be gone before the engine will let the rename claim its name: {cs:?}"
        );

        let cs = run_with(&MinimalDialect, &base, &declared, &intents);
        assert_eq!(
            kinds(&cs),
            ["RenameTable", "DropIndex"],
            "an index name is only unique per table on SQL Server, so nothing moves: {cs:?}"
        );
    }

    /// Review of #969: on SQL Server a check or foreign key is a schema object
    /// beside the tables (DEC-496.1), so dropping one frees its name for a
    /// table renamed onto it, and the drop has to run first. Measured on
    /// 17.0.4075.5: `sp_rename 'dbo.old', 'c'` is refused (Msg 15335) while
    /// another table still has a check `c`, and succeeds once it is dropped.
    /// On PostgreSQL a check name is the table's own, and nothing moves.
    #[test]
    fn a_dropped_constraint_that_frees_a_renamed_targets_name_precedes_the_rename_where_constraints_share_it()
     {
        let old_t = table(&[("id", Column::new(ty("int")))]);
        let mut other_base = table(&[("n", Column::new(ty("int")))]);
        other_base.checks.insert(
            "target".into(),
            pbps_model::schema::CheckConstraint {
                expression: "n > 0".into(),
            },
        );
        let base = two_tables(("app.old", old_t.clone()), ("app.other", other_base));
        let declared = two_tables(
            ("app.target", old_t),
            ("app.other", table(&[("n", Column::new(ty("int")))])),
        );
        let intents = [Intent::RenameTable {
            from: "app.old".parse().unwrap(),
            to: "app.target".parse().unwrap(),
        }];

        let cs = run_with(&MinimalDialect, &base, &declared, &intents);
        assert_eq!(
            kinds(&cs),
            ["DropCheck", "RenameTable"],
            "the check has to be gone before the engine lets the rename take its name: {cs:?}"
        );

        let cs = run_with(&SharesIndexNamespace, &base, &declared, &intents);
        assert_eq!(
            kinds(&cs),
            ["RenameTable", "DropCheck"],
            "a check name is the table's own on PostgreSQL, so nothing moves: {cs:?}"
        );
    }

    /// Review of #969: a table moved to another schema carries its constraints
    /// (SQL Server) or indexes (PostgreSQL) there, and a name another object
    /// holds in the destination refuses the whole move. Measured on
    /// 17.0.4075.5: `ALTER SCHEMA s2 TRANSFER s1.old` is refused (Msg 15530)
    /// while `old` has a check `c` and `s2.c` is a table. One the plan drops
    /// anyway goes first, addressed by the source name. A rename within one
    /// schema carries nothing, and its drop stays after it under the declared
    /// name.
    #[test]
    fn a_moved_tables_dropped_constraint_or_index_runs_before_the_transfer() {
        let mut old_t = table(&[("id", Column::new(ty("int")))]);
        old_t.checks.insert(
            "c".into(),
            pbps_model::schema::CheckConstraint {
                expression: "id > 0".into(),
            },
        );
        old_t.indexes.insert(
            "ix".into(),
            Index {
                columns: vec![IndexColumn {
                    key: pbps_model::IndexKey::Column("id".into()),
                    descending: false,
                    opclass: None,
                }],
                include: vec![],
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        let bare = table(&[("id", Column::new(ty("int")))]);
        let base = two_tables(("s1.old", old_t), ("s2.c", bare.clone()));
        let declared = two_tables(("s2.new", bare.clone()), ("s2.c", bare.clone()));
        let moved = [Intent::RenameTable {
            from: "s1.old".parse().unwrap(),
            to: "s2.new".parse().unwrap(),
        }];
        let first_before_rename = |cs: &ChangeSet, kind: &str| {
            let at = |k: &str| kinds(cs).iter().position(|x| x == k).unwrap();
            assert!(at(kind) < at("RenameTable"), "{kind} first: {cs:?}");
            let table = cs.changes[at(kind)].change.table().unwrap().clone();
            assert_eq!(
                table,
                "s1.old".parse().unwrap(),
                "by the source name: {cs:?}"
            );
        };

        let cs = run_with(&MinimalDialect, &base, &declared, &moved);
        first_before_rename(&cs, "DropCheck");
        let cs = run_with(&SharesIndexNamespace, &base, &declared, &moved);
        first_before_rename(&cs, "DropIndex");

        // Within one schema nothing is carried, and nothing moves.
        let base = two_tables(
            ("s1.old", {
                let mut t = bare.clone();
                t.checks.insert(
                    "c".into(),
                    pbps_model::schema::CheckConstraint {
                        expression: "id > 0".into(),
                    },
                );
                t
            }),
            ("s2.c", bare.clone()),
        );
        let declared = two_tables(("s1.new", bare.clone()), ("s2.c", bare));
        let cs = run_with(
            &MinimalDialect,
            &base,
            &declared,
            &[Intent::RenameTable {
                from: "s1.old".parse().unwrap(),
                to: "s1.new".parse().unwrap(),
            }],
        );
        assert_eq!(kinds(&cs), ["RenameTable", "DropCheck"], "{cs:?}");
    }

    /// Review of #969: the move carries a check the table keeps into its
    /// destination, where the plan drops another table's check of that name.
    /// That drop has to go first, and it is addressed as it is.
    #[test]
    fn a_destination_constraint_a_moved_table_would_collide_with_is_dropped_first() {
        let check = || pbps_model::schema::CheckConstraint {
            expression: "id > 0".into(),
        };
        let mut old_t = table(&[("id", Column::new(ty("int")))]);
        old_t.checks.insert("c".into(), check());
        let mut other = table(&[("id", Column::new(ty("int")))]);
        other.checks.insert("c".into(), check());
        let base = two_tables(("s1.old", old_t.clone()), ("s2.other", other));
        let declared = two_tables(
            ("s2.new", old_t),
            ("s2.other", table(&[("id", Column::new(ty("int")))])),
        );
        let cs = run_with(
            &MinimalDialect,
            &base,
            &declared,
            &[Intent::RenameTable {
                from: "s1.old".parse().unwrap(),
                to: "s2.new".parse().unwrap(),
            }],
        );
        assert_eq!(kinds(&cs), ["DropCheck", "RenameTable"], "{cs:?}");
        assert_eq!(
            cs.changes[0].change.table().unwrap().clone(),
            "s2.other".parse().unwrap(),
            "{cs:?}"
        );
    }

    /// #975: a rename carries the columns that have a default when it runs, by
    /// the names they have then, so the emitter can move each generated default
    /// with its table. A column the same plan renames is listed by its old name,
    /// since the table's rename runs first.
    #[test]
    fn a_table_rename_lists_the_columns_that_carry_a_default_by_their_names_then() {
        let mut with_default = Column::new(ty("int"));
        with_default.default = Some("0".into());
        let base = two_tables(
            (
                "app.old",
                table(&[("a", with_default.clone()), ("b", Column::new(ty("int")))]),
            ),
            ("app.other", table(&[("n", Column::new(ty("int")))])),
        );
        let declared = two_tables(
            (
                "app.new",
                table(&[("a", with_default), ("b", Column::new(ty("int")))]),
            ),
            ("app.other", table(&[("n", Column::new(ty("int")))])),
        );
        let cs = run_with(
            &MinimalDialect,
            &base,
            &declared,
            &[Intent::RenameTable {
                from: "app.old".parse().unwrap(),
                to: "app.new".parse().unwrap(),
            }],
        );
        let Change::RenameTable { defaults, .. } = &cs.changes[0].change else {
            panic!("{cs:?}");
        };
        assert_eq!(defaults, &["a".to_owned()]);
    }

    /// A column rename in a plan that also renames its table names the old
    /// table, so the emitter can find a generated default the table rename
    /// had to leave under the old table's name (#975). Without a table rename
    /// it names none.
    #[test]
    fn a_column_rename_names_the_old_table_only_when_the_table_is_renamed_too() {
        let other = ("app.other", table(&[("n", Column::new(ty("int")))]));
        let base = two_tables(
            ("app.old", table(&[("a", Column::new(ty("int")))])),
            other.clone(),
        );
        let renamed =
            |t: &str| two_tables((t, table(&[("b", Column::new(ty("int")))])), other.clone());
        let column = |t: &str| Intent::RenameColumn {
            table: t.parse().unwrap(),
            from: "a".into(),
            to: "b".into(),
        };
        let table_was = |cs: &ChangeSet| {
            cs.changes
                .iter()
                .find_map(|p| match &p.change {
                    Change::RenameColumn { table_was, .. } => Some(table_was.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{cs:?}"))
        };

        let both = run_with(
            &MinimalDialect,
            &base,
            &renamed("app.new"),
            &[
                Intent::RenameTable {
                    from: "app.old".parse().unwrap(),
                    to: "app.new".parse().unwrap(),
                },
                column("app.new"),
            ],
        );
        assert_eq!(table_was(&both), Some("app.old".parse().unwrap()));

        let alone = run_with(
            &MinimalDialect,
            &base,
            &renamed("app.old"),
            &[column("app.old")],
        );
        assert_eq!(table_was(&alone), None);
    }

    /// The same for a foreign key, the other constraint no index backs.
    #[test]
    fn a_dropped_foreign_key_that_frees_a_renamed_targets_name_precedes_the_rename() {
        let old_t = table(&[("id", Column::new(ty("int")))]);
        let parent = table(&[("id", Column::new(ty("int")))]);
        let mut child = table(&[("pid", Column::new(ty("int")))]);
        child.foreign_keys.insert(
            "target".into(),
            pbps_model::schema::ForeignKey {
                columns: vec!["pid".into()],
                references_table: "app.parent".parse().unwrap(),
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
        let mut base = two_tables(("app.old", old_t.clone()), ("app.child", child));
        base.tables
            .insert("app.parent".parse().unwrap(), parent.clone());
        let mut declared = two_tables(
            ("app.target", old_t),
            ("app.child", table(&[("pid", Column::new(ty("int")))])),
        );
        declared
            .tables
            .insert("app.parent".parse().unwrap(), parent);

        let cs = run_with(
            &MinimalDialect,
            &base,
            &declared,
            &[Intent::RenameTable {
                from: "app.old".parse().unwrap(),
                to: "app.target".parse().unwrap(),
            }],
        );
        assert_eq!(kinds(&cs), ["DropForeignKey", "RenameTable"], "{cs:?}");
    }

    /// The same shape for a named unique constraint, which is backed by an
    /// index of that name (the review-widened half of #176): dropping it
    /// frees the relation name just as dropping a plain index does.
    #[test]
    fn a_dropped_unique_constraint_that_frees_a_renamed_targets_name_precedes_the_rename() {
        let old_t = table(&[("id", Column::new(ty("int")))]);
        let mut other_base = table(&[("n", Column::new(ty("int")))]);
        other_base.unique.insert("target".into(), unique(&["n"]));
        let base = two_tables(("app.old", old_t.clone()), ("app.other", other_base));

        let declared = two_tables(
            ("app.target", old_t),
            ("app.other", table(&[("n", Column::new(ty("int")))])),
        );

        let cs = run_with(
            &SharesIndexNamespace,
            &base,
            &declared,
            &[Intent::RenameTable {
                from: "app.old".parse().unwrap(),
                to: "app.target".parse().unwrap(),
            }],
        );
        assert_eq!(kinds(&cs), ["DropUnique", "RenameTable"], "{cs:?}");
    }

    /// The same, where the key on the freeing constraint belongs to a table
    /// this plan drops. The dropped table's own key drop still has to precede
    /// the unique it references, under the dropped table's name.
    #[test]
    fn a_dropped_tables_key_to_a_freeing_unique_precedes_it() {
        let old_t = table(&[("id", Column::new(ty("int")))]);
        let mut other_base = table(&[("n", Column::new(ty("int")))]);
        other_base.unique.insert("target".into(), unique(&["n"]));
        let mut child = table(&[("other_n", Column::new(ty("int")))]);
        child.foreign_keys.insert(
            "fk_child_other".into(),
            fk(&["other_n"], "app.other", &["n"]),
        );
        let mut base = two_tables(("app.old", old_t.clone()), ("app.other", other_base));
        base.tables.insert("app.child".parse().unwrap(), child);

        let declared = two_tables(
            ("app.target", old_t),
            ("app.other", table(&[("n", Column::new(ty("int")))])),
        );

        let cs = run_with(
            &SharesIndexNamespace,
            &base,
            &declared,
            &[
                Intent::RenameTable {
                    from: "app.old".parse().unwrap(),
                    to: "app.target".parse().unwrap(),
                },
                Intent::DropTable {
                    table: "app.child".parse().unwrap(),
                    reason: "gone".into(),
                },
            ],
        );
        let at = |f: &dyn Fn(&Change) -> bool| {
            cs.changes
                .iter()
                .position(|p| f(&p.change))
                .unwrap_or_else(|| panic!("{:?}", cs.changes))
        };
        let key = at(&|c| {
            matches!(c, Change::DropForeignKey { table, name }
                if table.to_string() == "app.child" && name == "fk_child_other")
        });
        let unique = at(&|c| matches!(c, Change::DropUnique { .. }));
        let rename = at(&|c| matches!(c, Change::RenameTable { .. }));
        assert!(key < unique && unique < rename, "{:?}", cs.changes);
    }

    /// Finding 1 from PR #462's round-2 review: the guard used to disable all
    /// reordering the moment *any* `DropForeignKey` was anywhere in the plan
    /// — even one that itself depends on the very constraint a freeing drop
    /// is about to lift, which left this exact plan refused. Measured on
    /// `pbps-test-pg-cw`: dropping `fk_c` (which references `app.other.n`,
    /// the column `target` constrains) before `target`, before the rename,
    /// is what PostgreSQL actually needs — and takes.
    #[test]
    fn a_foreign_key_drop_moves_with_the_freeing_drop_it_depends_on() {
        let old_t = table(&[("id", Column::new(ty("int")))]);
        let mut other_base = table(&[("n", Column::new(ty("int")))]);
        other_base.unique.insert("target".into(), unique(&["n"]));
        let mut child_base = table(&[("n", Column::new(ty("int")))]);
        child_base
            .foreign_keys
            .insert("fk_c".into(), fk(&["n"], "app.other", &["n"]));
        let mut base = two_tables(("app.old", old_t.clone()), ("app.other", other_base));
        base.tables.insert("app.child".parse().unwrap(), child_base);

        let mut declared = two_tables(
            ("app.target", old_t),
            ("app.other", table(&[("n", Column::new(ty("int")))])),
        );
        declared.tables.insert(
            "app.child".parse().unwrap(),
            table(&[("n", Column::new(ty("int")))]),
        );

        let cs = run_with(
            &SharesIndexNamespace,
            &base,
            &declared,
            &[Intent::RenameTable {
                from: "app.old".parse().unwrap(),
                to: "app.target".parse().unwrap(),
            }],
        );
        assert_eq!(
            kinds(&cs),
            ["DropForeignKey", "DropUnique", "RenameTable"],
            "the foreign key has to go before the constraint it depends on, \
             which has to go before the rename that claims its name: {cs:?}"
        );
    }

    /// An unrelated FK stays after its owner's rename without pinning the
    /// other table's freeing unique drop (issue #467).
    #[test]
    fn an_unrelated_foreign_key_keeps_its_owner_name_without_blocking_other_drops() {
        let mut old_t = table(&[
            ("id", Column::new(ty("int"))),
            ("parent_id", Column::new(ty("int"))),
        ]);
        old_t.foreign_keys.insert(
            "fk_old_parent".into(),
            fk(&["parent_id"], "app.parent", &["id"]),
        );
        let mut other_base = table(&[("n", Column::new(ty("int")))]);
        other_base.unique.insert("target".into(), unique(&["n"]));
        let mut base = two_tables(("app.old", old_t.clone()), ("app.other", other_base));
        base.tables.insert(
            "app.parent".parse().unwrap(),
            table(&[("id", Column::new(ty("int")))]),
        );

        let target_t = table(&[
            ("id", Column::new(ty("int"))),
            ("parent_id", Column::new(ty("int"))),
        ]);
        let mut declared = two_tables(
            ("app.target", target_t),
            ("app.other", table(&[("n", Column::new(ty("int")))])),
        );
        declared.tables.insert(
            "app.parent".parse().unwrap(),
            table(&[("id", Column::new(ty("int")))]),
        );

        let cs = run_with(
            &SharesIndexNamespace,
            &base,
            &declared,
            &[Intent::RenameTable {
                from: "app.old".parse().unwrap(),
                to: "app.target".parse().unwrap(),
            }],
        );
        assert_eq!(
            kinds(&cs),
            ["DropUnique", "RenameTable", "DropForeignKey"],
            "only a referencing FK must precede the freeing key: {cs:?}"
        );
    }

    /// Finding 2 from PR #462's round-2 review, the review-widened half of
    /// #176: `pbps-pg`'s `emit.rs` renders `DropIndex` as
    /// `DROP INDEX <schema>.<name>` — it never names the table at all, so it
    /// can move ahead of a rename even when it is *that very table's own*
    /// index being dropped. The
    /// guard used to exclude any drop whose own table was a rename target,
    /// which is right for `DropUnique`/the primary-key drop (both need
    /// `ALTER TABLE`) but was wrong for `DropIndex`, and left this plan
    /// refused. Measured on `pbps-test-pg-cw`: `app.old` owns a plain index
    /// literally named `target` and renames to `app.target` in the same
    /// plan; the drop has to run first regardless of whose index it is.
    #[test]
    fn a_table_can_free_its_own_renamed_targets_name_by_dropping_its_own_index() {
        let mut old_t = table(&[("id", Column::new(ty("int")))]);
        old_t.indexes.insert(
            "target".into(),
            Index {
                columns: vec![IndexColumn {
                    key: pbps_model::IndexKey::Column("id".into()),
                    descending: false,
                    opclass: None,
                }],
                include: vec![],
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        let base = schema_of("app.old", old_t);
        let declared = schema_of("app.target", table(&[("id", Column::new(ty("int")))]));

        let cs = run_with(
            &SharesIndexNamespace,
            &base,
            &declared,
            &[Intent::RenameTable {
                from: "app.old".parse().unwrap(),
                to: "app.target".parse().unwrap(),
            }],
        );
        assert_eq!(
            kinds(&cs),
            ["DropIndex", "RenameTable"],
            "dropping an index never needs the table's current name, so it \
             can free even its own table's name: {cs:?}"
        );
    }

    /// Round-3 review finding: `pbps-pg`'s `emit.rs::rename_table` runs a
    /// cross-schema rename as two statements, not one — `ALTER TABLE s1.old
    /// SET SCHEMA s2;` lands the table at the *intermediate* name `s2.old`
    /// first, and only `ALTER TABLE s2.old RENAME TO target;` after that
    /// gives it its declared name `s2.target`. The name set the guard used
    /// to check only ever held the second name. Measured on
    /// `pbps-test-pg-cw`: `s2.sibling`, an unrelated table, owns a plain
    /// index literally named `old`; the first statement collides with it —
    /// `relation "old" already exists in schema "s2"` — even though the
    /// plan's *final* declared schema has no collision at all, and dropping
    /// that index first is what the engine actually needs.
    #[test]
    fn a_cross_schema_rename_also_frees_the_schema_it_passes_through() {
        let old_t = table(&[("id", Column::new(ty("int")))]);
        let mut sibling_base = table(&[("n", Column::new(ty("int")))]);
        sibling_base.indexes.insert(
            "old".into(),
            Index {
                columns: vec![IndexColumn {
                    key: pbps_model::IndexKey::Column("n".into()),
                    descending: false,
                    opclass: None,
                }],
                include: vec![],
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        let base = two_tables(("s1.old", old_t.clone()), ("s2.sibling", sibling_base));

        let declared = two_tables(
            ("s2.target", old_t),
            ("s2.sibling", table(&[("n", Column::new(ty("int")))])),
        );

        let cs = run_with(
            &SharesIndexNamespace,
            &base,
            &declared,
            &[Intent::RenameTable {
                from: "s1.old".parse().unwrap(),
                to: "s2.target".parse().unwrap(),
            }],
        );
        assert_eq!(
            kinds(&cs),
            ["DropIndex", "RenameTable"],
            "the index has to be gone before the cross-schema rename's first \
             statement claims the intermediate name it collides with: {cs:?}"
        );
    }

    /// Moving a filtered index drop ahead of its own cross-schema rename
    /// must update the typed address, including the recorded filter removal.
    #[test]
    fn a_cross_schema_renames_own_index_uses_its_source_address_before_the_rename() {
        let mut old_t = table(&[("id", Column::new(ty("int")))]);
        old_t.indexes.insert(
            "target".into(),
            Index {
                columns: vec![IndexColumn {
                    key: pbps_model::IndexKey::Column("id".into()),
                    descending: false,
                    opclass: None,
                }],
                include: vec![],
                unique: false,
                filter: Some("id > 0".into()),
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        let base = schema_of("s1.old", old_t);
        let declared = schema_of("s2.target", table(&[("id", Column::new(ty("int")))]));

        let cs = run_with(
            &SharesIndexNamespace,
            &base,
            &declared,
            &[Intent::RenameTable {
                from: "s1.old".parse().unwrap(),
                to: "s2.target".parse().unwrap(),
            }],
        );
        assert_eq!(
            kinds(&cs),
            ["DropIndex", "RenameTable"],
            "the drop must precede SET SCHEMA and carry its source address: {cs:?}"
        );
        assert!(
            matches!(&cs.changes[0].change, Change::DropIndex { table, .. } if table.to_string() == "s1.old")
        );
        let mut recorded = pbps_model::Declared::from_schema(&base);
        recorded.advance(&cs);
        assert!(recorded.expressions.filters.is_empty(), "{recorded:?}");
    }

    #[test]
    fn a_referencing_foreign_key_uses_its_source_name_before_freeing_its_rename() {
        let mut owner = table(&[("id", Column::new(ty("int")))]);
        owner.unique.insert("target".into(), unique(&["id"]));
        let mut child = table(&[("id", Column::new(ty("int")))]);
        child
            .foreign_keys
            .insert("fk_child".into(), fk(&["id"], "app.owner", &["id"]));
        let base = two_tables(("app.owner", owner), ("app.old", child));
        let declared = two_tables(
            ("app.owner", table(&[("id", Column::new(ty("int")))])),
            ("app.target", table(&[("id", Column::new(ty("int")))])),
        );
        let cs = run_with(
            &SharesIndexNamespace,
            &base,
            &declared,
            &[Intent::RenameTable {
                from: "app.old".parse().unwrap(),
                to: "app.target".parse().unwrap(),
            }],
        );
        assert_eq!(kinds(&cs), ["DropForeignKey", "DropUnique", "RenameTable"]);
        assert!(
            matches!(&cs.changes[0].change, Change::DropForeignKey { table, .. } if table.to_string() == "app.old")
        );
    }

    #[test]
    fn a_freeing_constraint_drop_runs_between_its_owner_and_claimant_renames() {
        for primary in [false, true] {
            let mut owner = table(&[("id", Column::new(ty("int")).not_null())]);
            if primary {
                owner.primary_key = Some(PrimaryKey {
                    name: Some("target".into()),
                    columns: vec!["id".into()],
                    storage_parameters: Default::default(),
                });
            } else {
                owner.unique.insert("target".into(), unique(&["id"]));
            }
            let base = two_tables(
                ("app.owner_old", owner),
                ("app.claimant_old", table(&[("id", Column::new(ty("int")))])),
            );
            let declared = two_tables(
                (
                    "app.owner_new",
                    table(&[("id", Column::new(ty("int")).not_null())]),
                ),
                ("app.target", table(&[("id", Column::new(ty("int")))])),
            );
            let cs = run_with(
                &SharesIndexNamespace,
                &base,
                &declared,
                &[
                    Intent::RenameTable {
                        from: "app.owner_old".parse().unwrap(),
                        to: "app.owner_new".parse().unwrap(),
                    },
                    Intent::RenameTable {
                        from: "app.claimant_old".parse().unwrap(),
                        to: "app.target".parse().unwrap(),
                    },
                ],
            );
            assert_eq!(
                kinds(&cs),
                [
                    "RenameTable",
                    if primary {
                        "SetPrimaryKey"
                    } else {
                        "DropUnique"
                    },
                    "RenameTable"
                ],
                "{cs:?}"
            );
            assert!(
                matches!(&cs.changes[0].change, Change::RenameTable { to, .. } if to.to_string() == "app.owner_new")
            );
            assert_eq!(
                cs.changes[1].change.table().unwrap().to_string(),
                "app.owner_new"
            );
        }
    }

    #[test]
    fn a_foreign_key_on_other_columns_does_not_move_with_a_freeing_key() {
        let mut owner = table(&[
            ("id", Column::new(ty("int"))),
            ("other", Column::new(ty("int"))),
        ]);
        owner.unique.insert("target".into(), unique(&["id"]));
        owner.unique.insert("other_key".into(), unique(&["other"]));
        let mut child = table(&[("id", Column::new(ty("int")))]);
        child
            .foreign_keys
            .insert("fk_child".into(), fk(&["id"], "app.owner", &["other"]));
        let base = two_tables(("app.owner", owner.clone()), ("app.old", child));
        owner.unique.remove("target");
        let declared = two_tables(
            ("app.owner", owner),
            ("app.target", table(&[("id", Column::new(ty("int")))])),
        );
        let cs = run_with(
            &SharesIndexNamespace,
            &base,
            &declared,
            &[Intent::RenameTable {
                from: "app.old".parse().unwrap(),
                to: "app.target".parse().unwrap(),
            }],
        );
        assert_eq!(
            kinds(&cs),
            ["DropUnique", "RenameTable", "DropForeignKey"],
            "{cs:?}"
        );
        assert_eq!(
            cs.changes[2].change.table().unwrap().to_string(),
            "app.target"
        );
    }

    #[test]
    fn mutually_blocking_owner_preferences_use_an_executable_source_address() {
        let mut a = table(&[("id", Column::new(ty("int")))]);
        a.unique.insert("new_b".into(), unique(&["id"]));
        let mut b = table(&[("id", Column::new(ty("int")))]);
        b.unique.insert("new_a".into(), unique(&["id"]));
        let base = two_tables(("app.old_a", a), ("app.old_b", b));
        let declared = two_tables(
            ("app.new_a", table(&[("id", Column::new(ty("int")))])),
            ("app.new_b", table(&[("id", Column::new(ty("int")))])),
        );
        let cs = run_with(
            &SharesIndexNamespace,
            &base,
            &declared,
            &[
                Intent::RenameTable {
                    from: "app.old_a".parse().unwrap(),
                    to: "app.new_a".parse().unwrap(),
                },
                Intent::RenameTable {
                    from: "app.old_b".parse().unwrap(),
                    to: "app.new_b".parse().unwrap(),
                },
            ],
        );
        assert_eq!(
            kinds(&cs),
            ["DropUnique", "RenameTable", "DropUnique", "RenameTable"],
            "{cs:?}"
        );
        assert!(
            cs.changes[0]
                .change
                .table()
                .unwrap()
                .name
                .starts_with("old_")
        );
        assert!(
            cs.changes[2]
                .change
                .table()
                .unwrap()
                .name
                .starts_with("new_")
        );
    }

    #[test]
    fn changes_are_ordered_for_execution() {
        let mut base_t = table(&[
            ("old", Column::new(ty("int"))),
            ("doomed", Column::new(ty("int"))),
        ]);
        base_t.indexes.insert(
            "ix_doomed".into(),
            Index {
                columns: vec![IndexColumn {
                    key: pbps_model::IndexKey::Column("doomed".into()),
                    descending: false,
                    opclass: None,
                }],
                include: vec![],
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
        let want_t = table(&[("new", Column::new(ty("int")))]);

        let intents = vec![
            Intent::RenameColumn {
                table: "dbo.t".parse().unwrap(),
                from: "old".into(),
                to: "new".into(),
            },
            Intent::DropColumn {
                column: "dbo.t.doomed".parse().unwrap(),
                reason: "no longer in use".into(),
            },
        ];
        let cs = run(
            &schema_of("dbo.t", base_t),
            &schema_of("dbo.t", want_t),
            &intents,
        );

        assert_eq!(
            kinds(&cs),
            ["DropIndex", "DropColumn", "RenameColumn"],
            "an index must be dropped before the column it references, and a \
             column drop before the rename of a column of its own table — \
             `doomed` claims nothing `new` wants here, and moves anyway, \
             because which spellings are one name is the target database's \
             collation to say and not this plan's (DECISIONS 474)"
        );
    }

    /// description affects documentation, not structure, so Phase 1 deliberately
    /// produces no change for it.
    #[test]
    fn description_change_is_not_a_structural_change() {
        let base = schema_of("dbo.t", table(&[("a", Column::new(ty("int")))]));
        let mut c = Column::new(ty("int"));
        c.description = Some("Customer identifier".into());
        let want = schema_of("dbo.t", table(&[("a", c)]));
        assert!(run(&base, &want, &[]).is_empty());
    }

    #[test]
    fn deprecating_a_column_is_a_change_but_not_a_risk() {
        let base = schema_of("dbo.t", table(&[("a", Column::new(ty("int")))]));
        let mut c = Column::new(ty("int"));
        c.deprecated = Some("superseded by email".into());
        let want = schema_of("dbo.t", table(&[("a", c)]));
        let cs = run(&base, &want, &[]);

        assert_eq!(kinds(&cs), ["SetColumnDeprecated"]);
        assert!(cs.risks().is_empty());
    }

    /// IDENTITY cannot be modified with ALTER, so it must be blocked explicitly
    /// rather than emitting invalid SQL.
    #[test]
    fn identity_change_is_rejected() {
        let base = schema_of("dbo.t", table(&[("a", Column::new(ty("int")))]));
        let mut c = Column::new(ty("int"));
        c.identity = Some(pbps_model::Identity {
            seed: 1,
            increment: 1,
        });
        let want = schema_of("dbo.t", table(&[("a", c)]));

        let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let declared_ids = crate::resolve(&want, &base_ids, &[], &ctx()).unwrap().ids;
        let err = diff(
            Side {
                schema: &base,
                ids: &base_ids,
            },
            Side {
                schema: &want,
                ids: &declared_ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .unwrap_err();
        assert!(matches!(
            err[0],
            DiffError::IdentityChangeUnsupported { .. }
        ));
    }

    /// The expressible half of the same comparison survives. `diff` accumulates
    /// every change it can phrase and only then checks for errors, so returning
    /// A generated column is added after, and dropped before, the ordinary
    /// columns of the same plan: the engine refuses an expression over a
    /// column not added yet, and refuses to drop a column a generated column
    /// still reads (DEC-1168.1). Each half names its columns so that name
    /// order alone would get it wrong.
    #[test]
    fn a_generated_column_is_added_after_and_dropped_before_its_inputs() {
        let generated_over = |input: &str| {
            let mut c = Column::new(ty("int"));
            c.generated = Some(pbps_model::Generated {
                expression: format!("{input} * 2"),
                stored: true,
            });
            c
        };
        let bare = schema_of("app.t", table(&[("id", Column::new(ty("int")))]));
        let with = |generated: &str, input: &str| {
            schema_of(
                "app.t",
                table(&[
                    ("id", Column::new(ty("int"))),
                    (input, Column::new(ty("int"))),
                    (generated, generated_over(input)),
                ]),
            )
        };
        let order = |from: &Schema, to: &Schema, intents: &[pbps_model::Intent]| {
            let from_ids = crate::resolve(from, &IdsFile::default(), &[], &ctx())
                .unwrap()
                .ids;
            let to_ids = crate::resolve(to, &from_ids, intents, &ctx()).unwrap().ids;
            diff_partial(
                Side {
                    schema: from,
                    ids: &from_ids,
                },
                Side {
                    schema: to,
                    ids: &to_ids,
                },
                &MinimalDialect,
                &Hints::default(),
            )
            .changes
            .changes
            .iter()
            .filter_map(|p| {
                if let Change::AddColumn { name, .. } = &p.change {
                    Some(format!("+{name}"))
                } else if let Change::DropColumn { column, .. } = &p.change {
                    Some(format!("-{}", column.name))
                } else if let Change::AlterColumnType { column, .. } = &p.change {
                    Some(format!("~{}", column.name))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
        };
        // `a0` sorts before `b` by name, and is added after it.
        assert_eq!(order(&bare, &with("a0", "b"), &[]), ["+b", "+a0"]);
        // And after a retype of an input it reads, which the engine refuses
        // once the generated column stands.
        let narrow = schema_of(
            "app.t",
            table(&[
                ("id", Column::new(ty("int"))),
                ("b", Column::new(ty("smallint"))),
            ]),
        );
        assert_eq!(order(&narrow, &with("a0", "b"), &[]), ["~b", "+a0"]);
        // `zz` sorts after `a` by name, and is dropped before it.
        let t: TableName = "app.t".parse().unwrap();
        let drops = ["a", "zz"].map(|c| pbps_model::Intent::DropColumn {
            column: t.column(c),
            reason: "gone".into(),
        });
        assert_eq!(order(&with("zz", "a"), &bare, &drops), ["-zz", "-a"]);
        // And when the same plan renames the table, so each `DropColumn`
        // names it under the declared name: found by uid, not by name.
        let renamed = schema_of("app.u", table(&[("id", Column::new(ty("int")))]));
        let mut intents = vec![pbps_model::Intent::RenameTable {
            from: t.clone(),
            to: "app.u".parse().unwrap(),
        }];
        let u: TableName = "app.u".parse().unwrap();
        intents.extend(["a", "zz"].map(|c| pbps_model::Intent::DropColumn {
            column: u.column(c),
            reason: "gone".into(),
        }));
        assert_eq!(order(&with("zz", "a"), &renamed, &intents), ["-zz", "-a"]);
        // And when the same plan renames another column of the table, which
        // brings every drop of it into the rename's class.
        let mut both = with("zz", "a");
        both.tables
            .get_mut(&t)
            .unwrap()
            .columns
            .insert("x".into(), Column::new(ty("int")));
        let kept = schema_of(
            "app.t",
            table(&[
                ("id", Column::new(ty("int"))),
                ("y", Column::new(ty("int"))),
            ]),
        );
        let mut intents = vec![pbps_model::Intent::RenameColumn {
            table: t.clone(),
            from: "x".into(),
            to: "y".into(),
        }];
        intents.extend(drops.clone());
        // Uids are drawn fresh on every resolve, and they are what breaks a
        // tie in the class, so one draw can land right by chance.
        for _ in 0..32 {
            assert_eq!(order(&both, &kept, &intents), ["-zz", "-a"]);
        }
    }

    /// A recomputed generated column is checked against its nullability as it
    /// stands: a relaxation goes before the new expression and a tightening
    /// after it, whatever the names say (DEC-1168.1).
    #[test]
    fn a_generated_columns_nullability_relaxes_before_and_tightens_after_its_expression() {
        let generated_as = |type_: &str, expression: &str, nullable: bool| {
            let mut c = Column::new(ty(type_));
            c.nullable = nullable;
            c.generated = Some(pbps_model::Generated {
                expression: expression.into(),
                stored: true,
            });
            c
        };
        let generated =
            |expression: &str, nullable: bool| generated_as("int", expression, nullable);
        let order = |from: Column, to: Column| {
            let base = schema_of(
                "app.t",
                table(&[("a", Column::new(ty("int"))), ("g", from)]),
            );
            let want = schema_of("app.t", table(&[("a", Column::new(ty("int"))), ("g", to)]));
            let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
                .unwrap()
                .ids;
            let ids = crate::resolve(&want, &base_ids, &[], &ctx()).unwrap().ids;
            diff_partial(
                Side {
                    schema: &base,
                    ids: &base_ids,
                },
                Side {
                    schema: &want,
                    ids: &ids,
                },
                &MinimalDialect,
                &Hints::default(),
            )
            .changes
            .changes
            .iter()
            .map(|p| match &p.change {
                Change::AlterColumnExpression { .. } => "expression",
                Change::AlterColumnNullability {
                    to_nullable: true, ..
                } => "relax",
                Change::AlterColumnNullability {
                    to_nullable: false, ..
                } => "tighten",
                Change::AlterColumnType {
                    to_nullable: true, ..
                } => "retype",
                other => panic!("unexpected {other:?}"),
            })
            .collect::<Vec<_>>()
        };
        assert_eq!(
            order(generated("coalesce(a, 0)", false), generated("a", true)),
            ["relax", "expression"]
        );
        assert_eq!(
            order(generated("a", true), generated("coalesce(a, 0)", false)),
            ["expression", "tighten"]
        );
        // Retyped as well: the type change would carry the tightening, and
        // leaves it to a step after the new expression instead.
        assert_eq!(
            order(
                generated("a", true),
                generated_as("bigint", "coalesce(a, 0)", false)
            ),
            ["retype", "expression", "tighten"]
        );
    }

    /// A generated column's expression changes in place, as one typed change
    /// that carries both texts; a column that becomes or stops being
    /// generated, or changes kind, has no in-place form and is refused by name
    /// (DEC-1168.1). An ordinary default beside it is a default change.
    #[test]
    fn a_generated_expression_changes_in_place_and_nothing_else_about_generation_does() {
        let generated = |expression: &str, stored: bool| {
            let mut c = Column::new(ty("int"));
            c.generated = Some(pbps_model::Generated {
                expression: expression.into(),
                stored,
            });
            c
        };
        let diff_of = |from: Column, to: Column| {
            let base = schema_of("app.t", table(&[("b", from)]));
            let want = schema_of("app.t", table(&[("b", to)]));
            let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
                .unwrap()
                .ids;
            let ids = crate::resolve(&want, &base_ids, &[], &ctx()).unwrap().ids;
            diff_partial(
                Side {
                    schema: &base,
                    ids: &base_ids,
                },
                Side {
                    schema: &want,
                    ids: &ids,
                },
                &MinimalDialect,
                &Hints::default(),
            )
        };
        let changed = diff_of(generated("a * 2", true), generated("a * 3", true));
        assert!(changed.errors.is_empty(), "{:?}", changed.errors);
        assert!(
            matches!(
                changed.changes.changes.as_slice(),
                [p] if matches!(&p.change, Change::AlterColumnExpression { from, to, .. } if from == "a * 2" && to == "a * 3")
            ),
            "{:?}",
            changed.changes
        );
        assert!(
            diff_of(generated("a * 2", true), generated("a * 2", true))
                .changes
                .changes
                .is_empty()
        );
        for (from, to) in [
            (Column::new(ty("int")), generated("a * 2", true)),
            (generated("a * 2", true), Column::new(ty("int"))),
            (generated("a * 2", true), generated("a * 2", false)),
        ] {
            let refused = diff_of(from, to);
            assert!(
                matches!(
                    refused.errors.as_slice(),
                    [DiffError::GenerationChangeUnsupported { .. }]
                ),
                "{:?}",
                refused.errors
            );
        }
        // Negative: a default is not a generation, and changes as a default.
        let mut with_default = Column::new(ty("int"));
        with_default.default = Some("7".into());
        let defaulted = diff_of(Column::new(ty("int")), with_default);
        assert_eq!(kinds(&defaulted.changes), ["AlterColumnDefault"]);
    }

    /// `Err(errs)` threw away work it had already done — and `verify`, whose
    /// job is to *report* differences rather than approve them, showed only the
    /// identity error. The nullability drift beside it went missing from the
    /// count, the human report and the `on_drift` hook's payload alike.
    #[test]
    fn an_unexpressible_difference_does_not_hide_the_expressible_ones() {
        let base = schema_of(
            "dbo.t",
            table(&[("a", Column::new(ty("int"))), ("b", Column::new(ty("int")))]),
        );
        let mut identity_changed = Column::new(ty("int"));
        identity_changed.identity = Some(pbps_model::Identity {
            seed: 1,
            increment: 1,
        });
        let mut not_null = Column::new(ty("int"));
        not_null.nullable = false;
        let want = schema_of("dbo.t", table(&[("a", identity_changed), ("b", not_null)]));

        let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let declared_ids = crate::resolve(&want, &base_ids, &[], &ctx()).unwrap().ids;
        let sides = || {
            (
                Side {
                    schema: &base,
                    ids: &base_ids,
                },
                Side {
                    schema: &want,
                    ids: &declared_ids,
                },
            )
        };

        let (b, d) = sides();
        let partial = diff_partial(b, d, &MinimalDialect, &Hints::default());
        assert!(matches!(
            partial.errors.as_slice(),
            [DiffError::IdentityChangeUnsupported { .. }]
        ));
        assert_eq!(
            kinds(&partial.changes),
            ["AlterColumnNullability"],
            "the change the differ could phrase was dropped: {:?}",
            partial.changes
        );

        // And `diff` still refuses outright, because a plan that cannot express
        // every difference must not be applied at all. The two callers ask
        // different questions and this is the one place that says so.
        let (b, d) = sides();
        assert!(diff(b, d, &MinimalDialect, &Hints::default()).is_err());
    }

    /// A jump-version deploy: the entire reason uid matching exists.
    ///
    /// Prod sits at v1 while the declarations have moved on to v3, with a rename
    /// somewhere in between. That intent left the working tree long ago — but both
    /// identity files still record the same uid, so the rename is still detected,
    /// in one step, with no need to walk the v1→v2→v3 chain of names.
    #[test]
    fn a_rename_is_detected_across_many_versions() {
        let v1 = schema_of(
            "dbo.t",
            table(&[("customer_name", Column::new(ty("nvarchar(50)")))]),
        );
        let v1_ids = crate::resolve(&v1, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;

        // v2: the rename. The intent exists only in this version.
        let v2 = schema_of(
            "dbo.t",
            table(&[("full_name", Column::new(ty("nvarchar(50)")))]),
        );
        let v2_ids = crate::resolve(
            &v2,
            &v1_ids,
            &[Intent::RenameColumn {
                table: "dbo.t".parse().unwrap(),
                from: "customer_name".into(),
                to: "full_name".into(),
            }],
            &ctx(),
        )
        .unwrap()
        .ids;

        // v3: just a longer column. No intent at all.
        let v3 = schema_of(
            "dbo.t",
            table(&[("full_name", Column::new(ty("nvarchar(200)")))]),
        );
        let v3_ids = crate::resolve(&v3, &v2_ids, &[], &ctx()).unwrap().ids;

        // Apply v3 to an environment stuck at v1, supplying no intent.
        let cs = diff(
            Side {
                schema: &v1,
                ids: &v1_ids,
            },
            Side {
                schema: &v3,
                ids: &v3_ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .unwrap();

        assert_eq!(
            kinds(&cs),
            ["RenameColumn", "AlterColumnType"],
            "across versions a rename must still read as a rename, not a drop plus an add"
        );
        assert!(
            !cs.risks().contains(&RiskClass::Destructive),
            "this must never turn into a data-losing plan"
        );
    }

    /// Identical input must produce a plan in the same order, or the plan's
    /// checksum would be unstable.
    #[test]
    fn output_is_deterministic() {
        let base = schema_of(
            "dbo.t",
            table(&[("a", Column::new(ty("int"))), ("b", Column::new(ty("int")))]),
        );
        let want = schema_of(
            "dbo.t",
            table(&[
                ("a", Column::new(ty("bigint"))),
                ("b", Column::new(ty("bigint"))),
                ("c", Column::new(ty("int"))),
            ]),
        );
        let first = run(&base, &want, &[]);
        for _ in 0..10 {
            let again = run(&base, &want, &[]);
            assert_eq!(
                kinds(&first),
                kinds(&again),
                "identical input should produce the plan in the same order"
            );
        }
        let _ = Uid::generate(pbps_model::UidKind::Column);
    }

    // ---- modules (ADR-0002) ----

    fn a_module(kind: pbps_model::ModuleKind, definition: &str) -> pbps_model::Module {
        pbps_model::Module {
            kind,
            description: None,
            definition: definition.to_owned(),
        }
    }

    fn with_functions(mut schema: Schema, specs: &[(&str, &str)]) -> Schema {
        for (name, definition) in specs {
            schema.modules.insert(
                name.parse().unwrap(),
                a_module(pbps_model::ModuleKind::Function, definition),
            );
        }
        schema
    }

    fn with_modules(mut schema: Schema, specs: &[(&str, &str)]) -> Schema {
        for (name, definition) in specs {
            schema.modules.insert(
                name.parse().unwrap(),
                a_module(pbps_model::ModuleKind::View, definition),
            );
        }
        schema
    }

    /// Modules are matched by name and carry no identity, so `run`'s uid
    /// machinery is bypassed: this calls the differ directly with the same ids
    /// on both sides.
    fn module_diff(base: &Schema, declared: &Schema) -> ChangeSet {
        let ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        diff(
            Side {
                schema: base,
                ids: &ids,
            },
            Side {
                schema: declared,
                ids: &ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .unwrap()
    }

    #[test]
    fn a_new_module_is_created_and_a_removed_one_dropped() {
        let base = Schema::default();
        let declared = with_modules(Schema::default(), &[("dbo.v", "SELECT 1")]);
        assert_eq!(kinds(&module_diff(&base, &declared)), ["CreateModule"]);

        let dropped = module_diff(&declared, &base);
        assert_eq!(kinds(&dropped), ["DropModule"]);
        // What a dropped module destroys is the validity of its dependents, so
        // it faces the gate — with no tombstone and no reason, because git
        // holds the definition it had.
        assert!(dropped.risks().contains(&RiskClass::Destructive));
    }

    /// Layout is not a change. Re-stating every view on every deploy would
    /// train reviewers to skim the plan, which is the one thing a plan must not
    /// invite.
    #[test]
    fn a_reindented_definition_is_not_a_change() {
        let base = with_modules(Schema::default(), &[("dbo.v", "SELECT a, b FROM t")]);
        let declared = with_modules(
            Schema::default(),
            &[("dbo.v", "SELECT a,\n       b\nFROM t")],
        );
        assert!(module_diff(&base, &declared).is_empty());
    }

    #[test]
    fn a_changed_definition_is_restated_in_full() {
        let base = with_modules(Schema::default(), &[("dbo.v", "SELECT a FROM t")]);
        let declared = with_modules(Schema::default(), &[("dbo.v", "SELECT a, b FROM t")]);
        let cs = module_diff(&base, &declared);
        assert_eq!(kinds(&cs), ["AlterModule"]);
        // CREATE OR ALTER preserves the permissions granted on the object, so
        // an alteration must never be planned as drop + create.
        assert!(cs.risks().is_empty());
    }

    /// `CREATE OR ALTER` cannot turn a view into a procedure. Planning that as
    /// an alteration would fail at the statement, halfway through an apply.
    #[test]
    fn a_changed_kind_becomes_drop_plus_create() {
        let base = with_modules(Schema::default(), &[("dbo.thing", "SELECT 1")]);
        let mut declared = Schema::default();
        declared.modules.insert(
            "dbo.thing".parse().unwrap(),
            a_module(pbps_model::ModuleKind::Procedure, "AS SELECT 1"),
        );
        assert_eq!(
            kinds(&module_diff(&base, &declared)),
            ["DropModule", "CreateModule"]
        );
    }

    /// A trigger moved to another table needs no case of its own: its table is
    /// part of its identity (ADR-0009 §1), so the move is one key gone and one
    /// key new — which is the drop and the create, in that order, from the two
    /// ordinary loops.
    #[test]
    fn a_trigger_moved_to_another_table_is_a_drop_and_a_create() {
        let trigger = |table: &str| {
            let mut schema = Schema::default();
            schema.modules.insert(
                pbps_model::ModuleId::Trigger {
                    on: table.parse().unwrap(),
                    name: "audit".to_owned(),
                },
                a_module(pbps_model::ModuleKind::Trigger, "AFTER INSERT AS SELECT 1"),
            );
            schema
        };
        let cs = module_diff(&trigger("dbo.orders"), &trigger("dbo.customers"));
        assert_eq!(kinds(&cs), ["DropModule", "CreateModule"]);
        let subjects: Vec<String> = cs.changes.iter().map(|p| p.change.subject()).collect();
        assert_eq!(subjects, ["dbo.orders.audit", "dbo.customers.audit"]);

        // The same trigger on the same table is still an alteration, not a
        // replacement: nothing about its identity moved.
        let mut same = trigger("dbo.orders");
        for m in same.modules.values_mut() {
            m.definition = "AFTER UPDATE AS SELECT 1".to_owned();
        }
        assert_eq!(
            kinds(&module_diff(&trigger("dbo.orders"), &same)),
            ["AlterModule"]
        );
    }

    /// Two overloads of one function are two objects: a plan that adds the
    /// second must not read as an alteration of the first.
    #[test]
    fn a_second_overload_is_a_create_and_not_an_alteration() {
        let routine = |spec: &str, body: &str| {
            let mut schema = Schema::default();
            schema.modules.insert(
                spec.parse().unwrap(),
                a_module(pbps_model::ModuleKind::Function, body),
            );
            schema
        };
        let mut both = routine("app.f(integer)", "AS 1");
        both.modules.extend(routine("app.f(text)", "AS 2").modules);
        let cs = module_diff(&routine("app.f(integer)", "AS 1"), &both);
        assert_eq!(kinds(&cs), ["CreateModule"]);
        assert_eq!(cs.changes[0].change.subject(), "app.f(text)");
    }

    /// A view over a view has to be created second and dropped first, or the
    /// statement fails. The scan of the definition text is what produces the
    /// order; §8.2's "never parse" is about comparison, not about this.
    #[test]
    fn modules_are_ordered_by_what_they_reference() {
        let declared = with_modules(
            Schema::default(),
            &[
                ("dbo.top", "SELECT * FROM dbo.middle"),
                ("dbo.middle", "SELECT * FROM dbo.base"),
                ("dbo.base", "SELECT 1"),
            ],
        );
        let created: Vec<String> = module_diff(&Schema::default(), &declared)
            .changes
            .iter()
            .map(|p| p.change.subject())
            .collect();
        assert_eq!(created, ["dbo.base", "dbo.middle", "dbo.top"]);

        let dropped: Vec<String> = module_diff(&declared, &Schema::default())
            .changes
            .iter()
            .map(|p| p.change.subject())
            .collect();
        assert_eq!(dropped, ["dbo.top", "dbo.middle", "dbo.base"]);
    }

    /// A module that is going must go before the table changes: a SCHEMABINDING
    /// view blocks a rename of the column it binds. And one that is arriving
    /// must come after them, or it selects a column that does not exist yet.
    #[test]
    fn modules_bracket_the_table_changes() {
        let base = with_modules(
            schema_of("dbo.t", table(&[("a", Column::new(ty("int")))])),
            &[("dbo.going", "SELECT a FROM dbo.t")],
        );
        let declared = with_modules(
            schema_of(
                "dbo.t",
                table(&[("a", Column::new(ty("int"))), ("b", Column::new(ty("int")))]),
            ),
            &[("dbo.arriving", "SELECT a, b FROM dbo.t")],
        );
        let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let declared_ids = crate::resolve(&declared, &base_ids, &[], &ctx())
            .unwrap()
            .ids;
        let cs = diff(
            Side {
                schema: &base,
                ids: &base_ids,
            },
            Side {
                schema: &declared,
                ids: &declared_ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .unwrap();
        assert_eq!(kinds(&cs), ["DropModule", "AddColumn", "CreateModule"]);
    }

    // ---- roles (ADR-0005) ----

    mod roles {
        use super::*;
        use pbps_model::{GrantTarget, Permission, Role};

        fn role(grants: &[(&str, &[Permission])]) -> Role {
            let mut r = Role::default();
            for (target, perms) in grants {
                r.grants.insert(
                    target.parse::<GrantTarget>().unwrap(),
                    perms.iter().copied().collect(),
                );
            }
            r
        }

        /// One table `dbo.customer` (uid t_aaaaaa) on both sides, plus the
        /// roles given, with `r_` uids minted from the name list.
        fn side(roles: &[(&str, &str, Role)]) -> (Schema, IdsFile) {
            let mut s = Schema::default();
            let mut t = Table::default();
            t.columns
                .insert("id".to_owned(), Column::new("int".parse().unwrap()));
            s.tables.insert("dbo.customer".parse().unwrap(), t);
            let mut ids = IdsFile::default();
            ids.tables
                .insert("t_aaaaaa".parse().unwrap(), "dbo.customer".parse().unwrap());
            ids.columns.insert(
                "c_aaaaaa".parse().unwrap(),
                "dbo.customer.id".parse().unwrap(),
            );
            for (uid, name, role) in roles {
                ids.roles.insert(uid.parse().unwrap(), (*name).to_owned());
                s.roles.insert((*name).to_owned(), role.clone());
            }
            (s, ids)
        }

        fn kinds(base: &(Schema, IdsFile), declared: &(Schema, IdsFile)) -> Vec<String> {
            diff(
                Side {
                    schema: &base.0,
                    ids: &base.1,
                },
                Side {
                    schema: &declared.0,
                    ids: &declared.1,
                },
                &MinimalDialect,
                &Hints::default(),
            )
            .unwrap()
            .changes
            .iter()
            .map(|p| crate::schema_diff::tests::roles::describe(&p.change))
            .collect()
        }

        /// A dialect whose roles are the *cluster's*, not the database's —
        /// PostgreSQL's answer (ADR-0010 §3, DECISIONS 211). Everything else
        /// is `MinimalDialect`'s, so what a test using it shows is exactly the
        /// difference this one capability makes.
        #[derive(Debug, Clone, Copy, Default)]
        struct ClusterRoles;

        impl pbps_dialect::Dialect for ClusterRoles {
            fn name(&self) -> &'static str {
                "cluster-roles"
            }
            fn manages_roles(&self) -> bool {
                false
            }
            fn quote_ident(&self, ident: &str) -> Result<String, pbps_dialect::DialectError> {
                MinimalDialect.quote_ident(ident)
            }
            fn emit(
                &self,
                change: &Change,
                strategy: pbps_model::Strategy,
            ) -> Result<Vec<pbps_dialect::Statement>, pbps_dialect::DialectError> {
                MinimalDialect.emit(change, strategy)
            }
            fn normalize_type(
                &self,
                ty: &pbps_model::ColumnType,
            ) -> Result<pbps_model::ColumnType, pbps_dialect::DialectError> {
                MinimalDialect.normalize_type(ty)
            }
            fn type_change_risk(
                &self,
                from: &pbps_model::ColumnType,
                to: &pbps_model::ColumnType,
            ) -> pbps_dialect::TypeChangeRisk {
                MinimalDialect.type_change_risk(from, to)
            }
            fn fold_ident<'a>(&self, ident: &'a str) -> std::borrow::Cow<'a, str> {
                MinimalDialect.fold_ident(ident)
            }
            fn lexicon(&self) -> pbps_dialect::Lexicon {
                MinimalDialect.lexicon()
            }
            fn validate_table(
                &self,
                name: &pbps_model::TableName,
                table: &Table,
            ) -> Vec<pbps_dialect::DialectError> {
                MinimalDialect.validate_table(name, table)
            }
            fn transaction_framing(&self) -> pbps_dialect::TransactionFraming {
                MinimalDialect.transaction_framing()
            }
            fn probe_framing(&self) -> Option<pbps_dialect::TransactionFraming> {
                MinimalDialect.probe_framing()
            }
        }

        fn kinds_on(
            dialect: &dyn pbps_dialect::Dialect,
            base: &(Schema, IdsFile),
            declared: &(Schema, IdsFile),
        ) -> Vec<String> {
            diff(
                Side {
                    schema: &base.0,
                    ids: &base.1,
                },
                Side {
                    schema: &declared.0,
                    ids: &declared.1,
                },
                dialect,
                &Hints::default(),
            )
            .unwrap()
            .changes
            .iter()
            .map(|p| describe(&p.change))
            .collect()
        }

        /// The grants are managed either way; the *principal* is not. A plan
        /// that carried a `CreateRole` here would refuse the only way a role
        /// ever comes under management on such an engine — a DBA creates it in
        /// the cluster and the project then declares it.
        #[test]
        fn a_declared_role_is_granted_without_being_created_where_the_cluster_owns_it() {
            let base = side(&[]);
            let declared = side(&[(
                "r_aaaaaa",
                "app_reader",
                role(&[("dbo.customer", &[Permission::Select])]),
            )]);
            assert_eq!(
                kinds_on(&ClusterRoles, &base, &declared),
                ["grant app_reader dbo.customer select"]
            );
            // The same declaration on an engine that owns its roles creates
            // it, which is what says this test is about the capability and not
            // about the fixture.
            assert_eq!(
                kinds_on(&MinimalDialect, &base, &declared),
                ["create app_reader", "grant app_reader dbo.customer select"]
            );
        }

        /// `drop-role` revokes and unmanages. A plan that said "drop role" and
        /// ran nothing would leave a principal still holding every permission
        /// pbps was managing; one that really dropped it would reach every
        /// other database in the cluster.
        #[test]
        fn a_dropped_role_has_its_grants_revoked_and_the_principal_left_standing() {
            let base = side(&[(
                "r_aaaaaa",
                "app_reader",
                role(&[
                    ("dbo.customer", &[Permission::Select, Permission::Insert]),
                    ("schema::dbo", &[Permission::Usage]),
                ]),
            )]);
            let declared = side(&[]);
            assert_eq!(
                kinds_on(&ClusterRoles, &base, &declared),
                [
                    "revoke app_reader dbo.customer select+insert",
                    "revoke app_reader schema::dbo usage",
                ]
            );
            assert_eq!(
                kinds_on(&MinimalDialect, &base, &declared),
                ["drop app_reader"]
            );
        }

        /// A revoke on an object the same plan drops would fail on an object
        /// that is gone — the rule the per-target comparison already applies,
        /// on the path that does not go through it.
        #[test]
        fn a_dropped_roles_grant_on_a_dropped_object_is_not_revoked() {
            let base = side(&[(
                "r_aaaaaa",
                "app_reader",
                role(&[("dbo.customer", &[Permission::Select])]),
            )]);
            let mut declared = side(&[]);
            declared.0.tables.clear();
            declared.1.tables.clear();
            declared.1.columns.clear();
            let k = kinds_on(&ClusterRoles, &base, &declared);
            assert!(
                !k.iter().any(|c| c.starts_with("revoke")),
                "the DROP TABLE takes the permission with it: {k:?}"
            );
        }

        /// A rename is a rename a human performed in the cluster, and an ACL
        /// entry holds the role's oid rather than its name — so every grant
        /// followed it, and the plan has nothing to re-grant.
        #[test]
        fn a_renamed_role_is_not_renamed_again_where_the_cluster_owns_the_principal() {
            let base = side(&[(
                "r_aaaaaa",
                "old_name",
                role(&[("dbo.customer", &[Permission::Select])]),
            )]);
            let declared = side(&[(
                "r_aaaaaa",
                "new_name",
                role(&[("dbo.customer", &[Permission::Select])]),
            )]);
            assert!(kinds_on(&ClusterRoles, &base, &declared).is_empty());
            assert_eq!(
                kinds_on(&MinimalDialect, &base, &declared),
                ["rename old_name->new_name"]
            );
        }

        fn describe(c: &Change) -> String {
            match c {
                Change::CreateRole { name, .. } => format!("create {name}"),
                Change::DropRole { name, .. } => format!("drop {name}"),
                Change::RenameRole { from, to, .. } => format!("rename {from}->{to}"),
                Change::Grant {
                    role,
                    target,
                    permissions,
                } => format!(
                    "grant {role} {target} {}",
                    permissions
                        .iter()
                        .map(|p| p.as_str())
                        .collect::<Vec<_>>()
                        .join("+")
                ),
                Change::Revoke {
                    role,
                    target,
                    permissions,
                } => format!(
                    "revoke {role} {target} {}",
                    permissions
                        .iter()
                        .map(|p| p.as_str())
                        .collect::<Vec<_>>()
                        .join("+")
                ),
                other => format!("{:?}", std::mem::discriminant(other)),
            }
        }

        #[test]
        fn a_new_role_is_created_and_then_granted_and_nothing_is_gated() {
            let base = side(&[]);
            let declared = side(&[(
                "r_aaaaaa",
                "app_reader",
                role(&[("dbo.customer", &[Permission::Select, Permission::Insert])]),
            )]);
            let k = kinds(&base, &declared);
            assert_eq!(
                k,
                [
                    "create app_reader",
                    "grant app_reader dbo.customer select+insert"
                ]
            );
            let cs = diff(
                Side {
                    schema: &base.0,
                    ids: &base.1,
                },
                Side {
                    schema: &declared.0,
                    ids: &declared.1,
                },
                &MinimalDialect,
                &Hints::default(),
            )
            .unwrap();
            assert!(cs.risks().contains(&RiskClass::GrantWiden));
            assert!(cs.unapproved_risks(&Default::default()).is_empty());
        }

        /// Only the difference: restating what the role already holds would
        /// claim a widening that is not one, and dropping a permission is a
        /// revoke behind the gate.
        #[test]
        fn grants_are_compared_per_target_and_only_the_difference_is_emitted() {
            let base = side(&[(
                "r_aaaaaa",
                "app_reader",
                role(&[
                    ("dbo.customer", &[Permission::Select, Permission::Insert]),
                    ("schema::app", &[Permission::Execute]),
                ]),
            )]);
            let declared = side(&[(
                "r_aaaaaa",
                "app_reader",
                role(&[
                    ("dbo.customer", &[Permission::Select, Permission::Update]),
                    ("schema::app", &[Permission::Execute]),
                ]),
            )]);
            let k = kinds(&base, &declared);
            assert_eq!(
                k,
                [
                    "revoke app_reader dbo.customer insert",
                    "grant app_reader dbo.customer update"
                ]
            );
            let cs = diff(
                Side {
                    schema: &base.0,
                    ids: &base.1,
                },
                Side {
                    schema: &declared.0,
                    ids: &declared.1,
                },
                &MinimalDialect,
                &Hints::default(),
            )
            .unwrap();
            assert_eq!(
                cs.unapproved_risks(&Default::default()),
                [RiskClass::Revoke].into_iter().collect()
            );
            // The negative case: identical grants are no change at all.
            assert!(kinds(&base, &base).is_empty());
        }

        #[test]
        fn a_role_matched_by_uid_under_a_new_name_is_renamed_in_place() {
            let base = side(&[(
                "r_aaaaaa",
                "reader",
                role(&[("dbo.customer", &[Permission::Select])]),
            )]);
            let declared = side(&[(
                "r_aaaaaa",
                "app_reader",
                role(&[("dbo.customer", &[Permission::Select])]),
            )]);
            assert_eq!(kinds(&base, &declared), ["rename reader->app_reader"]);
        }

        #[test]
        fn a_role_gone_from_the_ids_is_dropped_behind_the_revoke_gate() {
            let base = side(&[(
                "r_aaaaaa",
                "legacy",
                role(&[("dbo.customer", &[Permission::Select])]),
            )]);
            let declared = side(&[]);
            assert_eq!(kinds(&base, &declared), ["drop legacy"]);
            let cs = diff(
                Side {
                    schema: &base.0,
                    ids: &base.1,
                },
                Side {
                    schema: &declared.0,
                    ids: &declared.1,
                },
                &MinimalDialect,
                &Hints::default(),
            )
            .unwrap();
            assert!(cs.risks().contains(&RiskClass::Revoke));
        }

        /// A grant follows its object through `sp_rename`, so a renamed table
        /// must not come out as a revoke on the old name and a grant on the
        /// new one.
        #[test]
        fn a_grant_on_a_renamed_table_is_not_revoked_and_regranted() {
            let base = side(&[(
                "r_aaaaaa",
                "r",
                role(&[("dbo.customer", &[Permission::Select])]),
            )]);
            let mut declared = side(&[(
                "r_aaaaaa",
                "r",
                role(&[("dbo.client", &[Permission::Select])]),
            )]);
            let t = declared
                .0
                .tables
                .remove(&"dbo.customer".parse().unwrap())
                .unwrap();
            declared.0.tables.insert("dbo.client".parse().unwrap(), t);
            declared.1.rename_table(
                &"dbo.customer".parse().unwrap(),
                &"dbo.client".parse().unwrap(),
            );
            let k = kinds(&base, &declared);
            assert!(
                k.iter()
                    .all(|c| !c.starts_with("grant") && !c.starts_with("revoke")),
                "{k:?}"
            );
        }

        /// The drop takes the permission with it; a `REVOKE` after it would
        /// fail on an object that is gone.
        #[test]
        fn a_grant_on_a_table_this_plan_drops_is_not_revoked() {
            let base = side(&[(
                "r_aaaaaa",
                "r",
                role(&[("dbo.customer", &[Permission::Select])]),
            )]);
            let mut declared = side(&[("r_aaaaaa", "r", role(&[]))]);
            declared.0.tables.clear();
            declared.1.tables.clear();
            declared.1.columns.clear();
            let k = kinds(&base, &declared);
            assert!(k.iter().all(|c| !c.starts_with("revoke")), "{k:?}");
        }

        /// Where routines overload, a drop takes the grants of the one routine
        /// it names and no other (ADR-0009 §1). Matching by name read the
        /// sibling's grant as gone with the drop, and left the role holding a
        /// permission the plan declared removed.
        #[test]
        fn a_dropped_overload_takes_only_its_own_grants() {
            let routine = |id: &str, body: &str| {
                (
                    id.parse::<ModuleId>().unwrap(),
                    pbps_model::Module {
                        kind: pbps_model::ModuleKind::Function,
                        description: None,
                        definition: body.to_owned(),
                    },
                )
            };
            let mut base = side(&[(
                "r_aaaaaa",
                "r",
                role(&[
                    ("dbo.f(integer)", &[Permission::Execute]),
                    ("dbo.f(text)", &[Permission::Execute]),
                ]),
            )]);
            base.0.modules.extend([
                routine("dbo.f(integer)", "RETURN 1"),
                routine("dbo.f(text)", "RETURN 'a'"),
            ]);
            // `dbo.f(integer)` is dropped; `dbo.f(text)` stays, and its grant
            // is declared gone.
            let mut declared = side(&[("r_aaaaaa", "r", role(&[]))]);
            declared
                .0
                .modules
                .extend([routine("dbo.f(text)", "RETURN 'a'")]);
            let cs = diff(
                Side {
                    schema: &base.0,
                    ids: &base.1,
                },
                Side {
                    schema: &declared.0,
                    ids: &declared.1,
                },
                &MinimalDialect,
                &Hints::default(),
            )
            .unwrap();
            let revoked: Vec<String> = cs
                .changes
                .iter()
                .filter_map(|p| match &p.change {
                    Change::Revoke { target, .. } => Some(target.to_string()),
                    _ => None,
                })
                .collect();
            assert_eq!(revoked, ["dbo.f(text)"], "{:?}", kinds(&base, &declared));
            assert!(
                cs.changes.iter().any(|p| matches!(
                    &p.change,
                    Change::DropModule { id, .. } if id.to_string() == "dbo.f(integer)"
                )),
                "{:?}",
                kinds(&base, &declared)
            );
        }

        /// The drop takes the permission with it, and the CREATE that follows
        /// makes a bare object: every declared permission on it is a GRANT
        /// again, even though the two grant sets read the same.
        #[test]
        fn a_grant_on_a_table_this_plan_drops_and_recreates_is_granted_again() {
            let grants = role(&[("dbo.customer", &[Permission::Select])]);
            let base = side(&[("r_aaaaaa", "r", grants.clone())]);
            // The same name under a new identity: a drop and a create.
            let mut declared = side(&[("r_aaaaaa", "r", grants)]);
            declared.1.tables.clear();
            declared.1.columns.clear();
            declared
                .1
                .tables
                .insert("t_bbbbbb".parse().unwrap(), "dbo.customer".parse().unwrap());
            declared.1.columns.insert(
                "c_bbbbbb".parse().unwrap(),
                "dbo.customer.id".parse().unwrap(),
            );
            let cs = diff(
                Side {
                    schema: &base.0,
                    ids: &base.1,
                },
                Side {
                    schema: &declared.0,
                    ids: &declared.1,
                },
                &MinimalDialect,
                &Hints::default(),
            )
            .unwrap();
            let at =
                |pred: &dyn Fn(&Change) -> bool| cs.changes.iter().position(|p| pred(&p.change));
            let drop = at(&|c| matches!(c, Change::DropTable { .. })).expect("a drop");
            let create = at(&|c| matches!(c, Change::CreateTable { .. })).expect("a create");
            let grant = at(&|c| matches!(c, Change::Grant { .. })).expect("the grant again");
            assert!(
                at(&|c| matches!(c, Change::Revoke { .. })).is_none(),
                "{:?}",
                kinds(&base, &declared)
            );
            // And the grant comes after the create, which comes after the drop.
            assert!(drop < create && create < grant, "{drop} {create} {grant}");
        }

        /// A dialect whose `AlterModule` rebuilds the object, PostgreSQL's
        /// answer (ADR-0009 §3, #248). Everything else is `MinimalDialect`'s,
        /// the same isolation `ClusterRoles` gives `manages_roles` above.
        #[derive(Debug, Clone, Copy, Default)]
        struct Rebuilds;

        impl pbps_dialect::Dialect for Rebuilds {
            fn name(&self) -> &'static str {
                "rebuilds-modules"
            }
            fn rebuilds_modules(&self) -> bool {
                true
            }
            fn quote_ident(&self, ident: &str) -> Result<String, pbps_dialect::DialectError> {
                MinimalDialect.quote_ident(ident)
            }
            fn emit(
                &self,
                change: &Change,
                strategy: pbps_model::Strategy,
            ) -> Result<Vec<pbps_dialect::Statement>, pbps_dialect::DialectError> {
                MinimalDialect.emit(change, strategy)
            }
            fn normalize_type(
                &self,
                ty: &pbps_model::ColumnType,
            ) -> Result<pbps_model::ColumnType, pbps_dialect::DialectError> {
                MinimalDialect.normalize_type(ty)
            }
            fn type_change_risk(
                &self,
                from: &pbps_model::ColumnType,
                to: &pbps_model::ColumnType,
            ) -> pbps_dialect::TypeChangeRisk {
                MinimalDialect.type_change_risk(from, to)
            }
            fn fold_ident<'a>(&self, ident: &'a str) -> std::borrow::Cow<'a, str> {
                MinimalDialect.fold_ident(ident)
            }
            fn lexicon(&self) -> pbps_dialect::Lexicon {
                MinimalDialect.lexicon()
            }
            fn validate_table(
                &self,
                name: &pbps_model::TableName,
                table: &Table,
            ) -> Vec<pbps_dialect::DialectError> {
                MinimalDialect.validate_table(name, table)
            }
            fn transaction_framing(&self) -> pbps_dialect::TransactionFraming {
                MinimalDialect.transaction_framing()
            }
            fn probe_framing(&self) -> Option<pbps_dialect::TransactionFraming> {
                MinimalDialect.probe_framing()
            }
        }

        /// #314: a module the declarations did not change, handed back to be
        /// rebuilt because a connected read found it depends on one that is
        /// rebuilt, is rebuilt with its grant restated — the same as one the
        /// declarations edited. An id the plan already changes, or one the
        /// declarations do not have, is not rebuilt a second time or at all.
        #[test]
        fn a_module_handed_back_to_be_rebuilt_is_rebuilt_with_its_grant() {
            let grants = role(&[("app.v", &[Permission::Select])]);
            let mut base = side(&[("r_aaaaaa", "app_reader", grants.clone())]);
            let mut declared = side(&[("r_aaaaaa", "app_reader", grants)]);
            for s in [&mut base.0, &mut declared.0] {
                s.modules.insert(
                    "app.v".parse().unwrap(),
                    a_module(pbps_model::ModuleKind::View, "SELECT a FROM app.t"),
                );
            }
            let run = |also: &[&str]| {
                diff_rebuilding(
                    Side {
                        schema: &base.0,
                        ids: &base.1,
                    },
                    Side {
                        schema: &declared.0,
                        ids: &declared.1,
                    },
                    &Rebuilds,
                    &Hints::default(),
                    &also.iter().map(|s| s.parse().unwrap()).collect(),
                )
                .unwrap()
            };
            let count = |cs: &ChangeSet, pred: &dyn Fn(&Change) -> bool| {
                cs.changes.iter().filter(|p| pred(&p.change)).count()
            };

            let rebuilt = run(&["app.v"]);
            assert_eq!(
                count(&rebuilt, &|c| matches!(c, Change::AlterModule { .. })),
                1
            );
            assert_eq!(count(&rebuilt, &|c| matches!(c, Change::Grant { .. })), 1);

            // Nothing handed back: nothing changes.
            assert!(run(&[]).changes.is_empty());
            // Not declared: nothing to rebuild it from.
            assert!(run(&["app.gone"]).changes.is_empty());
        }

        /// The whole point of #248: a module rebuild takes the object's ACL
        /// with it on this engine, so a role's grant that neither side's
        /// declaration changed is still gone afterwards unless the plan
        /// restates it. `diff_roles` treats the `AlterModule` as a drop only
        /// when the dialect says the engine rebuilds modules — on one that
        /// does not (SQL Server's `CREATE OR ALTER`), restating an unchanged
        /// grant would be noise nobody asked for, since nothing there took it
        /// away.
        #[test]
        fn a_rebuilding_dialect_restates_an_unchanged_grant_after_the_alter() {
            let grants = role(&[("app.v", &[Permission::Select])]);
            let mut base = side(&[("r_aaaaaa", "app_reader", grants.clone())]);
            base.0.modules.insert(
                "app.v".parse().unwrap(),
                a_module(pbps_model::ModuleKind::View, "SELECT a FROM app.t"),
            );
            let mut declared = side(&[("r_aaaaaa", "app_reader", grants)]);
            declared.0.modules.insert(
                "app.v".parse().unwrap(),
                a_module(pbps_model::ModuleKind::View, "SELECT a, b FROM app.t"),
            );

            let run = |dialect: &dyn pbps_dialect::Dialect| {
                diff(
                    Side {
                        schema: &base.0,
                        ids: &base.1,
                    },
                    Side {
                        schema: &declared.0,
                        ids: &declared.1,
                    },
                    dialect,
                    &Hints::default(),
                )
                .unwrap()
            };

            let rebuilt = run(&Rebuilds);
            let at = |cs: &ChangeSet, pred: &dyn Fn(&Change) -> bool| {
                cs.changes.iter().position(|p| pred(&p.change))
            };
            let alter =
                at(&rebuilt, &|c| matches!(c, Change::AlterModule { .. })).expect("the alter");
            let grant =
                at(&rebuilt, &|c| matches!(c, Change::Grant { .. })).expect("the grant again");
            assert!(
                alter < grant,
                "the grant must sort after the rebuild it restores"
            );
            match &rebuilt.changes[grant].change {
                Change::Grant {
                    role,
                    target,
                    permissions,
                } => {
                    assert_eq!(role, "app_reader");
                    assert_eq!(target.to_string(), "app.v");
                    assert_eq!(permissions, &[Permission::Select].into_iter().collect());
                }
                other => panic!("expected a grant, got {other:?}"),
            }

            // The engine whose `AlterModule` does not rebuild the object
            // restates nothing: the grant was never taken away, so writing it
            // again would be a change nobody declared.
            let altered = run(&MinimalDialect);
            assert!(
                at(&altered, &|c| matches!(c, Change::Grant { .. })).is_none(),
                "CREATE OR ALTER preserves the grant; restating it here is noise"
            );
        }

        /// A revoke still follows the column renames, though the constraint
        /// drops it used to share an ordering class with have moved ahead of
        /// them.
        ///
        /// The drops moved because the engine refuses a rename while a check
        /// or a filtered index names the column. A revoke has the opposite
        /// need — it names the object as the renames leave it — so it could
        /// not travel with them and took a class of its own. This is the
        /// negative case for that split: nothing about the move may drag the
        /// revoke forward with it.
        #[test]
        fn a_revoke_stays_behind_the_renames_the_drops_moved_ahead_of() {
            let base = side(&[(
                "r_aaaaaa",
                "app",
                role(&[("dbo.customer", &[Permission::Insert])]),
            )]);
            let mut declared = side(&[("r_aaaaaa", "app", role(&[]))]);
            // And a column of that table is renamed in the same plan.
            let t = declared
                .0
                .tables
                .get_mut(&"dbo.customer".parse::<TableName>().unwrap())
                .unwrap();
            t.columns.clear();
            t.columns
                .insert("row_id".to_owned(), Column::new("int".parse().unwrap()));
            declared.1.columns.insert(
                "c_aaaaaa".parse().unwrap(),
                "dbo.customer.row_id".parse().unwrap(),
            );

            // `describe` names the role changes; a column rename falls to its
            // discriminant, which is all this test needs from it.
            let k = kinds(&base, &declared);
            let at = |s: &str| {
                k.iter()
                    .position(|c| c.starts_with(s))
                    .unwrap_or_else(|| panic!("{s} in {k:?}"))
            };
            assert!(at("Discriminant") < at("revoke"), "{k:?}");
        }

        /// The ordering: a revoke after the renames it may depend on, a grant
        /// after every object exists and after the role does.
        #[test]
        fn role_changes_sort_where_their_statements_can_run() {
            let base = side(&[(
                "r_aaaaaa",
                "old",
                role(&[("dbo.customer", &[Permission::Insert])]),
            )]);
            let mut declared = side(&[
                (
                    "r_aaaaaa",
                    "renamed",
                    role(&[("dbo.customer", &[Permission::Select])]),
                ),
                (
                    "r_bbbbbb",
                    "fresh",
                    role(&[("dbo.customer", &[Permission::Select])]),
                ),
            ]);
            let mut t = Table::default();
            t.columns
                .insert("id".to_owned(), Column::new("int".parse().unwrap()));
            declared.0.tables.insert("dbo.extra".parse().unwrap(), t);
            declared
                .1
                .tables
                .insert("t_bbbbbb".parse().unwrap(), "dbo.extra".parse().unwrap());
            let k = kinds(&base, &declared);
            let at = |s: &str| {
                k.iter()
                    .position(|c| c.starts_with(s))
                    .unwrap_or_else(|| panic!("{s} in {k:?}"))
            };
            assert!(at("rename") < at("revoke"), "{k:?}");
            assert!(at("revoke") < at("create fresh"), "{k:?}");
            assert!(at("create fresh") < at("grant fresh"), "{k:?}");
            // CreateTable is a discriminant string here; it must precede grants.
            let create_table = k
                .iter()
                .position(|c| c.starts_with("Discriminant"))
                .unwrap();
            assert!(create_table < at("grant"), "{k:?}");
        }
    }

    /// `PUBLIC` execution on a routine this plan creates (issue #318,
    /// ADR-0010 §5).
    mod public_execution {
        use super::*;

        /// PostgreSQL's two answers, on a dialect that is otherwise
        /// `MinimalDialect` — the same isolation `Rebuilds` gives
        /// `rebuilds_modules` in `roles` above.
        #[derive(Debug, Clone, Copy, Default)]
        struct PublicExecutes;

        impl pbps_dialect::Dialect for PublicExecutes {
            fn name(&self) -> &'static str {
                "public-executes"
            }
            fn rebuilds_modules(&self) -> bool {
                true
            }
            fn creates_public_executable_routines(&self) -> bool {
                true
            }
            fn quote_ident(&self, ident: &str) -> Result<String, pbps_dialect::DialectError> {
                MinimalDialect.quote_ident(ident)
            }
            fn emit(
                &self,
                change: &Change,
                strategy: pbps_model::Strategy,
            ) -> Result<Vec<pbps_dialect::Statement>, pbps_dialect::DialectError> {
                MinimalDialect.emit(change, strategy)
            }
            fn normalize_type(
                &self,
                ty: &ColumnType,
            ) -> Result<ColumnType, pbps_dialect::DialectError> {
                MinimalDialect.normalize_type(ty)
            }
            fn type_change_risk(
                &self,
                from: &ColumnType,
                to: &ColumnType,
            ) -> pbps_dialect::TypeChangeRisk {
                MinimalDialect.type_change_risk(from, to)
            }
            fn fold_ident<'a>(&self, ident: &'a str) -> std::borrow::Cow<'a, str> {
                MinimalDialect.fold_ident(ident)
            }
            fn lexicon(&self) -> pbps_dialect::Lexicon {
                MinimalDialect.lexicon()
            }
            fn validate_table(
                &self,
                name: &TableName,
                table: &Table,
            ) -> Vec<pbps_dialect::DialectError> {
                MinimalDialect.validate_table(name, table)
            }
            fn transaction_framing(&self) -> pbps_dialect::TransactionFraming {
                MinimalDialect.transaction_framing()
            }
            fn probe_framing(&self) -> Option<pbps_dialect::TransactionFraming> {
                MinimalDialect.probe_framing()
            }
            fn overloads(&self, kind: pbps_model::ModuleKind) -> bool {
                matches!(
                    kind,
                    pbps_model::ModuleKind::Function | pbps_model::ModuleKind::Procedure
                )
            }
        }

        fn schema_with(specs: &[(&str, pbps_model::ModuleKind, &str)]) -> Schema {
            let mut s = Schema::default();
            for (id, kind, definition) in specs {
                s.modules
                    .insert(id.parse().unwrap(), a_module(*kind, definition));
            }
            s
        }

        fn run(
            dialect: &dyn Dialect,
            base: &Schema,
            declared: &Schema,
            hints: &Hints,
        ) -> ChangeSet {
            let ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
                .unwrap()
                .ids;
            diff(
                Side {
                    schema: base,
                    ids: &ids,
                },
                Side {
                    schema: declared,
                    ids: &ids,
                },
                dialect,
                hints,
            )
            .unwrap()
        }

        fn decided(cs: &ChangeSet) -> Vec<(String, PublicAccess, RoutineOrigin)> {
            cs.changes
                .iter()
                .filter_map(|p| {
                    if let Change::PublicExecution {
                        routine,
                        access,
                        origin,
                    } = &p.change
                    {
                        Some((routine.to_string(), *access, *origin))
                    } else {
                        None
                    }
                })
                .collect()
        }

        fn closed(cs: &ChangeSet) -> Vec<(String, RoutineOrigin)> {
            decided(cs)
                .into_iter()
                .filter(|(_, access, _)| *access == PublicAccess::Revoked)
                .map(|(routine, _, origin)| (routine, origin))
                .collect()
        }

        /// The whole of #318: a routine the plan creates must not be left
        /// holding the engine's default, and the revoke that takes it away is
        /// a change in the plan — not something the applier does afterwards,
        /// which would be outside the checksum a human approved.
        #[test]
        fn a_created_routine_is_closed_to_public_in_the_plan() {
            let declared = schema_with(&[(
                "app.f(integer)",
                pbps_model::ModuleKind::Function,
                "RETURNS integer AS $$ SELECT 1 $$ LANGUAGE sql",
            )]);
            let cs = run(
                &PublicExecutes,
                &Schema::default(),
                &declared,
                &Hints::default(),
            );
            assert_eq!(
                closed(&cs),
                [("app.f(integer)".to_owned(), RoutineOrigin::Created)],
                "{:?}",
                kinds(&cs)
            );
            // And after the create, or it names an object that is not there.
            let k = kinds(&cs);
            let at = |name: &str| k.iter().position(|c| c == name).unwrap();
            assert!(at("CreateModule") < at("PublicExecution"), "{k:?}");
            // A routine nobody could execute a statement earlier loses
            // nothing, so the gate is not asked: `--allow revoke` in front of
            // every plan that declares a function is friction without safety.
            assert!(cs.risks().is_empty(), "{:?}", cs.risks());
        }

        /// #687: each routine's `PUBLIC` decision is the statement right after
        /// its own `CREATE`, not one after every module. In the autocommit
        /// script `--sql` renders, anything between them is a window in which
        /// the whole cluster can execute the new routine. Three routines, so a
        /// placement after all modules, or one ordered by name alone, puts
        /// another routine's `CREATE` in between.
        #[test]
        fn each_routine_is_closed_to_public_in_the_statement_after_its_create() {
            let body = "RETURNS integer AS $$ SELECT 1 $$ LANGUAGE sql";
            // `b` calls `a`, so `b` has a later create rank than its peers: its
            // decision must follow its own rank, not sort at rank 0 ahead of
            // the `CREATE` it settles.
            let calls_a = "RETURNS integer AS $$ SELECT app.a(1) $$ LANGUAGE sql";
            let declared = schema_with(&[
                ("app.a(integer)", pbps_model::ModuleKind::Function, body),
                ("app.b(integer)", pbps_model::ModuleKind::Function, calls_a),
                ("app.c(integer)", pbps_model::ModuleKind::Function, body),
            ]);
            let cs = run(
                &PublicExecutes,
                &Schema::default(),
                &declared,
                &Hints::default(),
            );
            let mut closed = 0;
            for (at, planned) in cs.changes.iter().enumerate() {
                if let Change::PublicExecution { routine, .. } = &planned.change {
                    closed += 1;
                    let before = at.checked_sub(1).map(|i| &cs.changes[i].change);
                    assert!(
                        matches!(
                            before,
                            Some(Change::CreateModule { id: ModuleId::Routine(r), .. }) if r == routine
                        ),
                        "{routine} is not settled right after its own CREATE: {:?}",
                        kinds(&cs)
                    );
                }
            }
            assert_eq!(closed, 3, "{:?}", kinds(&cs));
        }

        /// The opt-out the user writes. One line in the declaration, so the
        /// decision goes through the merge request and into the checksum.
        #[test]
        fn a_declaration_that_asks_for_public_execution_keeps_it() {
            let declared = schema_with(&[(
                "app.f(integer)",
                pbps_model::ModuleKind::Function,
                "RETURNS integer AS $$ SELECT 1 $$ LANGUAGE sql",
            )]);
            let mut hints = Hints::default();
            hints
                .public_execute
                .insert("app.f(integer)".parse().unwrap());
            let cs = run(&PublicExecutes, &Schema::default(), &declared, &hints);
            assert!(closed(&cs).is_empty(), "{:?}", kinds(&cs));
            // Said out loud, not by silence. A plan that carried nothing here
            // could not be told from one with no opinion, and the connected
            // rebuild guard has to tell those apart — see the rebuild case
            // below. It widens, and says so.
            assert_eq!(
                decided(&cs),
                [(
                    "app.f(integer)".to_owned(),
                    PublicAccess::Kept,
                    RoutineOrigin::Created
                )],
                "{:?}",
                kinds(&cs)
            );
            assert!(
                cs.risks().contains(&RiskClass::GrantWiden),
                "{:?}",
                cs.risks()
            );
            // And it is the *signature* that opts in, not the name: the other
            // overload is still closed.
            let both = schema_with(&[
                (
                    "app.f(integer)",
                    pbps_model::ModuleKind::Function,
                    "RETURNS integer AS $$ SELECT 1 $$ LANGUAGE sql",
                ),
                (
                    "app.f(text)",
                    pbps_model::ModuleKind::Function,
                    "RETURNS integer AS $$ SELECT 2 $$ LANGUAGE sql",
                ),
            ]);
            let cs = run(&PublicExecutes, &Schema::default(), &both, &hints);
            assert_eq!(
                closed(&cs),
                [("app.f(text)".to_owned(), RoutineOrigin::Created)],
                "{:?}",
                kinds(&cs)
            );
        }

        /// A rebuild is the risky half and says so. The `CREATE` inside it
        /// restores the engine default, and whether anybody was relying on
        /// that default is not in the declarations — so the gate is asked,
        /// which is exactly what it is not asked for a fresh routine.
        #[test]
        fn a_rebuilt_routine_faces_the_gate_and_a_fresh_one_does_not() {
            let base = schema_with(&[(
                "app.f(integer)",
                pbps_model::ModuleKind::Function,
                "RETURNS integer AS $$ SELECT 1 $$ LANGUAGE sql",
            )]);
            let declared = schema_with(&[(
                "app.f(integer)",
                pbps_model::ModuleKind::Function,
                "RETURNS integer AS $$ SELECT 2 $$ LANGUAGE sql",
            )]);
            let cs = run(&PublicExecutes, &base, &declared, &Hints::default());
            assert_eq!(
                closed(&cs),
                [("app.f(integer)".to_owned(), RoutineOrigin::Rebuilt)],
                "{:?}",
                kinds(&cs)
            );
            assert!(cs.risks().contains(&RiskClass::Revoke), "{:?}", cs.risks());
        }

        /// The case a plan that only ever *revoked* got wrong: a routine
        /// somebody closed, whose declaration now asks for the default back,
        /// and whose definition changed in the same revision.
        ///
        /// On this engine the rebuild's `CREATE` restores the default by
        /// itself, so there is no statement to write — and a plan that
        /// therefore said nothing could not be told from one with no opinion.
        /// `crate::pbps_pg::modules::before_a_rebuild` refuses a missing
        /// default it finds no recorded intent for, so silence here refused a
        /// valid rebuild for the very state it had been asked to reach.
        #[test]
        fn an_opted_in_rebuild_records_the_decision_rather_than_staying_silent() {
            let base = schema_with(&[(
                "app.f(integer)",
                pbps_model::ModuleKind::Function,
                "RETURNS integer AS $$ SELECT 1 $$ LANGUAGE sql",
            )]);
            let declared = schema_with(&[(
                "app.f(integer)",
                pbps_model::ModuleKind::Function,
                "RETURNS integer AS $$ SELECT 2 $$ LANGUAGE sql",
            )]);
            let mut hints = Hints::default();
            hints
                .public_execute
                .insert("app.f(integer)".parse().unwrap());
            let cs = run(&PublicExecutes, &base, &declared, &hints);
            assert_eq!(
                decided(&cs),
                [(
                    "app.f(integer)".to_owned(),
                    PublicAccess::Kept,
                    RoutineOrigin::Rebuilt
                )],
                "{:?}",
                kinds(&cs)
            );
            // Nothing is taken away, so the revoke gate is not asked — but
            // the routine stays open to every principal, and that is
            // labelled.
            assert!(!cs.risks().contains(&RiskClass::Revoke), "{:?}", cs.risks());
            assert!(
                cs.risks().contains(&RiskClass::GrantWiden),
                "{:?}",
                cs.risks()
            );
        }

        /// A view has no `EXECUTE` and a trigger is invoked by nobody, so
        /// neither is ever named by one of these.
        #[test]
        fn only_a_routine_is_closed_to_public() {
            let declared = schema_with(&[
                ("app.v", pbps_model::ModuleKind::View, "SELECT 1"),
                (
                    "app.t.tr",
                    pbps_model::ModuleKind::Trigger,
                    "AFTER INSERT AS SELECT 1",
                ),
            ]);
            let cs = run(
                &PublicExecutes,
                &Schema::default(),
                &declared,
                &Hints::default(),
            );
            assert!(decided(&cs).is_empty(), "{:?}", kinds(&cs));
        }

        /// An engine whose `CREATE` grants nobody `EXECUTE` gets none of
        /// this. Emitting the revoke there would take access away from SQL
        /// Server's real `public` role that nothing in the project granted.
        #[test]
        fn an_engine_without_the_default_is_left_alone() {
            let declared = schema_with(&[(
                "app.f(integer)",
                pbps_model::ModuleKind::Function,
                "RETURNS integer AS $$ SELECT 1 $$ LANGUAGE sql",
            )]);
            let cs = run(
                &MinimalDialect,
                &Schema::default(),
                &declared,
                &Hints::default(),
            );
            assert!(decided(&cs).is_empty(), "{:?}", kinds(&cs));
        }
    }

    /// A declaration that leaves the primary key unnamed matches whatever
    /// name the engine invented; only a named declaration, or different
    /// columns, is a change.
    #[test]
    fn an_unnamed_declared_primary_key_matches_any_stored_name() {
        let mut base_t = Table::default();
        base_t
            .columns
            .insert("id".to_owned(), Column::new("int".parse().unwrap()));
        base_t.primary_key = Some(pbps_model::PrimaryKey {
            name: Some("PK__t__357D4CF8312E0151".to_owned()),
            columns: vec!["id".to_owned()],
            storage_parameters: Default::default(),
        });
        let mut declared_t = base_t.clone();
        declared_t.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["id".to_owned()],
            storage_parameters: Default::default(),
        });
        let mut changes = Vec::new();
        diff_constraints(
            &"dbo.t".parse().unwrap(),
            &base_t,
            &declared_t,
            &mut changes,
        );
        assert!(changes.is_empty(), "{changes:?}");

        // The negative cases: other columns, or a name of its own.
        declared_t.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["other".to_owned()],
            storage_parameters: Default::default(),
        });
        let mut changes = Vec::new();
        diff_constraints(
            &"dbo.t".parse().unwrap(),
            &base_t,
            &declared_t,
            &mut changes,
        );
        assert!(
            matches!(
                changes.as_slice(),
                [
                    Change::SetPrimaryKey { to: None, .. },
                    Change::SetPrimaryKey { from: None, .. }
                ]
            ),
            "a replaced key is its drop and its add: {changes:?}"
        );
        declared_t.primary_key = Some(pbps_model::PrimaryKey {
            name: Some("pk_t".to_owned()),
            columns: vec!["id".to_owned()],
            storage_parameters: Default::default(),
        });
        let mut changes = Vec::new();
        diff_constraints(
            &"dbo.t".parse().unwrap(),
            &base_t,
            &declared_t,
            &mut changes,
        );
        assert!(
            matches!(
                changes.as_slice(),
                [
                    Change::SetPrimaryKey { to: None, .. },
                    Change::SetPrimaryKey { from: None, .. }
                ]
            ),
            "a named key is compared in full: {changes:?}"
        );
    }

    /// A role that holds another dropped role has to be dropped first: its
    /// membership cleanup names a principal the other drop would have taken
    /// away, and the engine refuses that by name.
    #[test]
    fn a_role_holding_another_dropped_role_is_dropped_before_it() {
        use pbps_model::{Change, PlannedChange};
        let drop = |name: &str, members: &[&str]| {
            PlannedChange::new(Change::DropRole {
                uid: "r_aaaaaa".parse().unwrap(),
                name: name.to_owned(),
                members: members.iter().map(|m| (*m).to_owned()).collect(),
            })
        };
        // `a` is a member of `z`, and `z` of `top`: the name tiebreaker alone
        // would emit them a, top, z — every one of them wrong.
        let planned = vec![drop("a", &[]), drop("z", &["a"]), drop("top", &["z"])];
        let depth = super::member_depth(&planned);
        assert_eq!(depth["top"], 0);
        assert_eq!(depth["z"], 1);
        assert_eq!(depth["a"], 2);

        // A member that is not itself dropped is nobody's rank, and a plan
        // whose roles hold none of each other keeps the order it had.
        let flat = vec![drop("a", &["some_user"]), drop("z", &[])];
        let depth = super::member_depth(&flat);
        assert_eq!(depth["a"], 0);
        assert_eq!(depth["z"], 0);
        assert!(!depth.contains_key("some_user"));
    }

    /// The differ never sees a dropped role's members — `plan --db` writes
    /// them in afterwards — so the parent-first order has to be applied again
    /// once they are known, over the slots the drops already occupy, with
    /// everything else where it was (DECISIONS 139).
    #[test]
    fn role_drops_are_reordered_parent_first_once_their_members_are_known() {
        use pbps_model::{Change, ChangeSet, PlannedChange};
        let drop = |name: &str, members: &[&str]| {
            PlannedChange::new(Change::DropRole {
                uid: "r_aaaaaa".parse().unwrap(),
                name: name.to_owned(),
                members: members.iter().map(|m| (*m).to_owned()).collect(),
            })
        };
        let other = PlannedChange::new(Change::DropTable {
            uid: "t_aaaaaa".parse().unwrap(),
            name: TableName::new("dbo", "gone"),
            detach_from: None,
        });
        // As the differ left them: name order, with a table drop in between.
        let mut cs = ChangeSet {
            changes: vec![
                drop("a", &["some_user"]),
                other.clone(),
                drop("top", &["z"]),
                drop("z", &["a"]),
            ],
        };
        super::order_role_drops(&mut cs);
        let names: Vec<String> = cs
            .changes
            .iter()
            .map(|p| match &p.change {
                Change::DropRole { name, .. } => name.clone(),
                _ => "table".to_owned(),
            })
            .collect();
        assert_eq!(names, ["top", "table", "z", "a"]);

        // Nothing holding anything: untouched.
        let mut flat = ChangeSet {
            changes: vec![drop("a", &[]), other, drop("z", &["some_user"])],
        };
        let before = format!("{:?}", flat.changes);
        super::order_role_drops(&mut flat);
        assert_eq!(format!("{:?}", flat.changes), before);
    }
}
