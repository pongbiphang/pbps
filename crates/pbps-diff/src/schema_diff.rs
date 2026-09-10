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
    Cell, Change, ChangeSet, ColumnRef, ColumnType, DataMode, GrantTarget, Hints, IdsFile,
    ModuleId, Permission, PlannedChange, Renames, Schema, Table, TableName, Uid, Value,
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

    /// A `data:` block on a table whose primary key cannot key its rows
    /// (ADR-0004). `validate` says the same thing against the file and the
    /// line; this is here so that a differ reached another way never quietly
    /// produces a plan with the rows left out of it.
    #[error(
        "{table} declares `data:` but has no single-column primary key, so its rows have no identity"
    )]
    DataWithoutKey { table: TableName },

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
    let d = diff_partial(base, declared, dialect, hints);
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
    for (uid, declared_name) in declared_tables {
        let Some(base_name) = base_tables.get(uid) else {
            continue;
        };
        if base_name != declared_name {
            changes.push(Change::RenameTable {
                uid: uid.clone(),
                from: base_name.clone(),
                to: declared_name.clone(),
            });
        }
        let (Some(base_table), Some(declared_table)) = (
            base.schema.tables.get(base_name),
            declared.schema.tables.get(declared_name),
        ) else {
            continue;
        };

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

    diff_modules(base.schema, declared.schema, dialect, &mut changes);
    diff_roles(base, declared, dialect, &mut changes);

    // The ordering and risk pass below runs whether or not there are errors:
    // it is pure computation over the changes already built, and a caller that
    // is going to *report* the partial set needs it sorted and classified
    // exactly as a plan would be.
    let mut planned: Vec<PlannedChange> = changes.into_iter().map(PlannedChange::new).collect();
    for p in &mut planned {
        if let Change::AlterColumnType { from, to, .. } = &p.change
            && let Some(r) = dialect.type_change_risk(from, to).risk_class()
        {
            p.risks.insert(r);
        }
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
    // belongs in separate `order_key` classes; this tiebreaker cannot express
    // it.
    // Rows follow the foreign keys among the tables that declare them. The
    // declared side is the right one to read: a row being inserted is going
    // into the schema as it will be, not as it was.
    let data_rank = rank_of_tables(&pbps_model::data::insertion_order(declared.schema));
    // Roles that are dropped together are ordered parent before member.
    let role_rank = member_depth(&planned);
    planned.sort_by_key(|p| {
        (
            order_key(&p.change),
            dependency_rank(&p.change, &create_rank, &drop_rank, &data_rank, &role_rank),
            p.change.subject(),
            format!("{:?}", p.change),
        )
    });
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

        let norm = |t: &ColumnType| dialect.normalize_type(t).unwrap_or_else(|_| t.clone());
        let (from_ty, to_ty) = (norm(&base_col.ty), norm(&col.ty));
        let retyped = from_ty != to_ty;
        // A type change subsumes a nullability change rather than sitting beside
        // one: `ALTER COLUMN` restates the whole definition, so two changes would
        // mean two statements where the second undoes half of the first.
        if from_ty != to_ty {
            changes.push(Change::AlterColumnType {
                uid: uid.clone(),
                column: declared_ref.clone(),
                from: from_ty,
                to: to_ty,
                from_nullable: base_col.nullable,
                to_nullable: col.nullable,
            });
        } else if base_col.nullable != col.nullable {
            changes.push(Change::AlterColumnNullability {
                uid: uid.clone(),
                column: declared_ref.clone(),
                ty: to_ty,
                to_nullable: col.nullable,
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

/// Constraints and indexes are always matched by name and never modified in
/// place — the database itself does drop + add, and pretending otherwise would
/// only give the emitter one more path that can fail.
fn diff_constraints(name: &TableName, base: &Table, declared: &Table, changes: &mut Vec<Change>) {
    // A declaration that leaves the key unnamed (`primary_key: [id]`) leaves
    // the name to the engine, and the engine invents one (`PK__t__357D...`)
    // that the recorded state then carries. Comparing names there would
    // restate the key on every connected plan until somebody copied the
    // invented name into the file. So an unnamed declaration matches any
    // stored name and only the columns are compared; a *named* declaration is
    // compared in full, because renaming a constraint is a real change.
    let pk_differs = match (&base.primary_key, &declared.primary_key) {
        (Some(b), Some(d)) if d.name.is_none() => b.columns != d.columns,
        (b, d) => b != d,
    };
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
        });
        if split {
            changes.push(Change::SetPrimaryKey {
                table: name.clone(),
                from: None,
                to: declared.primary_key.clone(),
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

    by_name!(unique, AddUnique, DropUnique, std::convert::identity);
    by_name!(foreign_keys, AddForeignKey, DropForeignKey, Box::new);
    by_name!(checks, AddCheck, DropCheck, std::convert::identity);

    for (n, ix) in &declared.indexes {
        if base.indexes.get(n) != Some(ix) {
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
        let base_key = base
            .primary_key
            .as_ref()
            .filter(|pk| pk.columns.len() == 1)
            .map(|pk| pk.columns[0].as_str());
        if base_name_of.get(&key_column).map(String::as_str) != base_key {
            errs.push(DiffError::DataKeyColumnChanged {
                table: name.clone(),
            });
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
                    // A non-key `IDENTITY` column is the engine's: never
                    // written by a row (`validate` refuses it) and never read
                    // back (DECISIONS 94), so both sides resolve it to NULL
                    // and it is neither a change nor a cell the row can be
                    // held to — the engine assigned it.
                    if spec.identity.is_some() {
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
                    if spec.identity.is_some() {
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
                    // Only for a column the declaration still has.
                    // `base_name_of` is keyed by the ids file, which still
                    // names a column this plan drops: that column is gone by
                    // the time the `DELETE` runs, so it gets no type and the
                    // emitter builds no predicate on it. The cell stays in
                    // `row`, where the reviewer can still read what the
                    // baseline held.
                    let Some(declared_ty) = declared.columns.get(declared_column).map(|d| &d.ty)
                    else {
                        continue;
                    };
                    types.insert(declared_column.clone(), spec.ty.clone());
                    if *declared_ty != spec.ty {
                        after_types.insert(declared_column.clone(), declared_ty.clone());
                    }
                }
                changes.push(Change::DeleteRow {
                    table: name.clone(),
                    key_column: key_column.clone(),
                    key: key.clone(),
                    cause: DeleteCause::Undeclared,
                    row,
                    types,
                    after_types,
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
        | Change::RenameTable { .. }
        | Change::AddColumn { .. }
        | Change::DropColumn { .. }
        | Change::RenameColumn { .. }
        | Change::AlterColumnNullability { .. }
        | Change::SetColumnDeprecated { .. }
        | Change::SetPrimaryKey { .. }
        | Change::AddUnique { .. }
        | Change::DropUnique { .. }
        | Change::AddCheck { .. }
        | Change::DropCheck { .. }
        | Change::AddIndex { .. }
        | Change::DropIndex { .. }
        | Change::SetDataMode { .. } => 0,
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
    let dropped: Vec<pbps_model::Dropped> = changes.iter().filter_map(Change::drops).collect();
    // Base table name -> the name it has after this plan, by uid.
    let renamed: BTreeMap<&TableName, &TableName> = base
        .ids
        .tables
        .iter()
        .filter_map(|(uid, name)| declared.ids.tables.get(uid).map(|to| (name, to)))
        .filter(|(from, to)| from != to)
        .collect();
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
            let target = forward(target);
            // A `REVOKE` on an object this same plan drops would fail on an
            // object that is gone — the same rule the per-target comparison
            // below applies, and for the same reason.
            if permissions.is_empty() || dropped.iter().any(|d| d.takes(&target)) {
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
        // oid rather than its name, so every grant followed it and nothing
        // has to be re-granted. The connected check confirms the cluster has
        // the declared name; the grant comparison below then runs against it,
        // which is right either way because the two names are one principal.
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
            let target_dropped = dropped.iter().any(|d| d.takes(target));
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

/// Modules are matched by **name**, never by uid: they carry no data, so they
/// carry no identity (ADR-0002).
///
/// Two of the three comparisons are ordinary. The third is the interesting one:
/// a module whose *kind* or trigger table changed is not an alteration at all —
/// `CREATE OR ALTER` cannot turn a view into a procedure, or move a trigger to
/// another table — so it is emitted as a drop followed by a create.
fn diff_modules(
    base: &Schema,
    declared: &Schema,
    dialect: &dyn Dialect,
    changes: &mut Vec<Change>,
) {
    for (id, module) in &declared.modules {
        match base.modules.get(id) {
            None => changes.push(Change::CreateModule {
                id: id.clone(),
                module: Box::new(module.clone()),
            }),
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
/// The table rename comes first, so every later step can use current names,
/// and the constraint and index drops come next: they must precede both the
/// column renames they would otherwise block and the column drops they
/// reference. Adding them back must follow adding columns.
/// Inserting a class shifts every class below it, and these ordinals are
/// quoted in prose that uses them to justify behaviour: DECISIONS 140, 146,
/// 151, 174 and 237, `docs/PITFALLS.md`, `preflight.rs` and `deploy.rs`. A new
/// class means renumbering those in the same commit — a stale ordinal there
/// reads as a statement about the code and is not checked against it.
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
        Change::DropTable { .. } => 6,
        Change::CreateTable { .. } => 7,
        Change::AddColumn { .. } => 8,
        Change::AlterColumnType { .. }
        | Change::AlterColumnNullability { .. }
        | Change::AlterColumnDefault { .. } => 9,
        Change::SetColumnDeprecated { .. } => 10,
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
        Change::DeleteRow { .. } => 12,
        Change::SetPrimaryKey { .. }
        | Change::AddUnique { .. }
        | Change::AddForeignKey { .. }
        | Change::AddCheck { .. }
        | Change::AddIndex { .. } => 13,
        Change::CreateModule { .. } | Change::AlterModule { .. } => 14,
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
        .unwrap()
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
                        name: "n".to_owned(),
                        descending: false,
                    }],
                    include: Vec::new(),
                    unique: false,
                    filter: Some("n > 0".to_owned()),
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
        let (row, _) = cs
            .changes
            .iter()
            .find_map(|p| match &p.change {
                Change::DeleteRow { row, types, .. } => Some((row, types)),
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
                }),
                std::mem::discriminant(&Change::AlterColumnNullability {
                    uid: pbps_model::Uid::generate(pbps_model::UidKind::Column),
                    column: pbps_model::ColumnRef {
                        table: "dbo.t".parse().unwrap(),
                        name: "id".to_owned(),
                    },
                    ty: ty("int"),
                    to_nullable: true,
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
                name: c.into(),
                descending: false,
            }],
            include: vec![],
            unique: false,
            filter: None,
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
                    name: "sku".into(),
                    descending: false,
                }],
                include: vec![],
                unique: true,
                filter: None,
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
        });
        let mut want_t = table(&[
            ("cust_id", Column::new(ty("int"))),
            ("sku", Column::new(ty("int"))),
        ]);
        want_t.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["cust_id".into()],
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
                    name: "old".into(),
                    descending: false,
                }],
                include: vec!["note".into()],
                unique: false,
                filter: None,
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
                    name: "new".into(),
                    descending: false,
                }],
                include: vec!["note".into()],
                unique: false,
                filter: None,
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
                name: column.into(),
                descending: false,
            }],
            include: vec![],
            unique: false,
            filter: Some(format!("[{column}] IS NOT NULL")),
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
                    name: "doomed".into(),
                    descending: false,
                }],
                include: vec![],
                unique: false,
                filter: None,
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
            ["DropIndex", "RenameColumn", "DropColumn"],
            "an index must be dropped before the column it references, and the \
             whole drop class now runs before the column renames — this index \
             blocks neither, which is the point: the ordering is uniform, not \
             conditional on what a constraint happens to name"
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
        });
        let mut declared_t = base_t.clone();
        declared_t.primary_key = Some(pbps_model::PrimaryKey {
            name: None,
            columns: vec!["id".to_owned()],
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
