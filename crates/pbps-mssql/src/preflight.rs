//! Probes derived from the plan itself (SPEC §7.5).
//!
//! Each risky change knows how it can fail, so each one can be turned into a
//! query that counts the rows that would break it. On a non-zero count `apply`
//! aborts **before the first statement**, reporting the real number rather than
//! letting the engine discover it halfway through: "4,213 rows violate
//! ck_customer_amount" is actionable, and "the ALTER failed" is not.
//!
//! # What this is not
//!
//! It is not a second risk classifier. Classification is static and happens
//! offline (§7.2); these run at apply time, where a connection is guaranteed and
//! reading the data is the job.
//!
//! # Where a probe is deliberately absent
//!
//! An empty result means "nothing was checked", never "nothing is wrong", and
//! the cases are named rather than left to be discovered:
//!
//! - **Renames** carry a dependency risk, not a data one. They are answered by
//!   [`crate::impact`], which asks the catalog instead.
//! - **Drops** cannot be probed: every row is "affected", and counting them
//!   would produce a number that always looks alarming and never decides
//!   anything.
//! - **Reducing a `decimal`'s scale** rounds rather than failing, so no count
//!   exists to take. The `narrowing` class still flags it for approval.

use std::collections::{BTreeMap, BTreeSet};

use pbps_dialect::{DialectError, Probe};
use pbps_model::{
    Cell, Change, ChangeSet, ColumnRef, ColumnType, RowKey, TableName, TypeArg, Value,
};

use crate::emit::qualified;
use crate::ident::{literal, quote};
use crate::types;

/// Every probe a plan implies.
///
/// Errors in quoting are swallowed on purpose: an identifier this dialect
/// cannot write has already stopped the plan in [`crate::emit`], and a probe
/// list is not the place to report it a second time.
pub fn probes(changes: &ChangeSet) -> Vec<Probe> {
    let names = AsStored::of(changes);
    changes
        .changes
        .iter()
        .filter_map(|p| build(&p.change, &names).ok())
        .flatten()
        .collect()
}

/// Translates the names a plan uses into the names the database still has.
///
/// Probes run before the first statement, so every object they mention has to
/// be named as the catalog knows it *now*. A plan that renames `email` to
/// `contact_email` and then tightens it to NOT NULL describes that column by
/// its new name, and a probe built from the change alone would ask about a
/// column that does not exist yet — silently skipping the very check that
/// mattered.
#[derive(Debug, Default)]
struct AsStored {
    /// New table name → the name it currently has.
    tables: BTreeMap<TableName, TableName>,
    /// New column ref → the column name it currently has.
    columns: BTreeMap<ColumnRef, String>,
    /// Tables this plan creates. They are empty, so nothing in them can violate
    /// anything, and probing them would only produce "invalid object name".
    created: BTreeSet<TableName>,
    /// The declared rows this plan updates or deletes, by table. A row the
    /// plan itself moves off a parent that is going away must not be counted
    /// as still pointing at it (see [`delete_probe`]) — and a row the plan
    /// merely touches elsewhere must.
    moved: BTreeMap<TableName, Moved>,
    /// The foreign keys this plan takes away before the deletes run: a
    /// `DropForeignKey`, and every key into the table a `DropTable` removes.
    /// Both sort before `DeleteRow` (`order_key`), so counting a child
    /// through a constraint that will be gone refuses a delete the engine
    /// would have accepted (DECISIONS 128).
    removed: BTreeSet<Removed>,
}

/// The rows of one table a plan writes, as the pre-delete probe has to see
/// them: which are deleted outright, and what each update or insert leaves
/// in each column it touches.
///
/// Per row, not per column: a foreign key is a tuple, and the probe compares
/// a row's whole tuple against the parent row's (DECISIONS 121). A per-column
/// record counted a child on the surviving parent `(1, 2)` against deleting
/// `(1, 1)`, and could not tell an update that sets two columns of one key
/// from two updates.
#[derive(Debug, Default)]
struct Moved {
    key_column: String,
    /// Deleted: gone whatever they referenced.
    deleted: BTreeSet<RowKey>,
    /// Updated row -> column set by its update -> the value it is set to,
    /// as the SQL the engine compares (a literal, or the expression the
    /// catalog spells a literal default in: `(1)`, `('old')`). `None` for
    /// NULL or a default that is not a literal, which no probe can compare.
    ///
    /// An update that sets a *referencing* column to a value that is not the
    /// deleted key moves the row off the parent; one that sets any other
    /// column, or sets it to the same key under another spelling (`01` for
    /// `1`, `OLD` for `old` under a case-insensitive collation), leaves it
    /// pointing where it was, and with `ON DELETE CASCADE` the engine would
    /// then delete it silently. Whether two spellings are one key is the
    /// engine's call, so the probe asks it. A row that moves *onto* the
    /// parent is an arrival, counted the same way (see [`Moved::inserted`]).
    updated: BTreeMap<RowKey, BTreeMap<String, Option<String>>>,
    /// Inserted row -> column -> the value it arrives with, as the SQL the
    /// engine compares: spelled by the row, or the literal default of a
    /// column it omits. A column set to NULL, or left to a default that is
    /// not a literal, is absent.
    ///
    /// `COUNT(*)` over the child sees only what is stored, and a row
    /// arriving on the parent is either not in the table yet (an insert) or
    /// stored somewhere else (an update). Both run before the deletes
    /// (`order_key`), so the probe passes, the insert or update commits, and
    /// then `ON DELETE CASCADE` takes the row straight back out — or `SET
    /// NULL` unpicks its reference — with the apply succeeding and recording
    /// the result, so the next plan proposes the same row again. Counted
    /// here instead, with the engine deciding whether the value and the
    /// deleted key are one key, exactly as it decides for `updated`.
    ///
    /// A column an insert omits, or an update sets to `DEFAULT`, arrives at
    /// the column's default — which the plan carries (`InsertRow::defaults`,
    /// `Cell::Default`) and which, when it is a literal, is rendered for the
    /// engine to compare like any other value. A default that is not a
    /// literal (`NEXT VALUE FOR`, `NEWID()`) has no value before it runs
    /// and is the one arrival no probe can ask about (DECISIONS 117).
    inserted: BTreeMap<RowKey, BTreeMap<String, String>>,
    /// Column -> the rows this plan writes to a default the probe cannot
    /// evaluate: one that is not a literal (`CONVERT(int, 1)`, `NEXT VALUE
    /// FOR`), left by an insert that omits the column or set by an update to
    /// `DEFAULT`. No arrival can be counted for such a write, and treating it
    /// as absent let a default that names the deleted row arrive unseen —
    /// so where the catalog says a foreign key to the deleted row spans the
    /// column, the write is refused instead (DECISIONS 124).
    unprobeable: BTreeMap<String, BTreeSet<RowKey>>,
}

/// A foreign key this plan removes before its deletes run: the constraint,
/// under the table that holds it. Both are named as the database has them —
/// a dropped constraint is one the base side read from the catalog.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Removed {
    table: TableName,
    constraint: Option<String>,
}

/// The SQL the engine compares a written cell by: the literal, or `None` for
/// what it cannot compare before the write runs.
fn written(value: &Value) -> Option<String> {
    match value {
        Value::Text(t) => Some(literal(t)),
        Value::Int(i) => Some(literal(&i.to_string())),
        Value::Bool(b) => Some(literal(&b.to_string())),
        // A NULL foreign key references nothing, so no delete can cascade
        // into the row through it.
        Value::Null => None,
    }
}

/// A default the engine can compare without running anything: a literal
/// (the line 80 drew), and not `NULL`, which references no row.
fn constant_default(default: &str) -> Option<&str> {
    let d = unwrapped(default);
    (crate::rows::is_constant(d) && !d.eq_ignore_ascii_case("null")).then_some(d)
}

/// `NULL` under any number of parentheses: a default that references no
/// row, and so is neither an arrival nor a write the probe has to refuse.
fn is_null_default(default: &str) -> bool {
    unwrapped(default).eq_ignore_ascii_case("null")
}

fn unwrapped(default: &str) -> &str {
    let mut d = default.trim();
    while d.len() >= 2 && d.starts_with('(') && d.ends_with(')') {
        d = d[1..d.len() - 1].trim();
    }
    d
}

impl AsStored {
    fn of(changes: &ChangeSet) -> Self {
        let mut this = Self::default();
        for p in &changes.changes {
            match &p.change {
                Change::RenameTable { from, to, .. } => {
                    this.tables.insert(to.clone(), from.clone());
                }
                Change::RenameColumn {
                    table, from, to, ..
                } => {
                    this.columns.insert(table.column(to), from.clone());
                }
                Change::CreateTable { name, .. } => {
                    this.created.insert(name.clone());
                }
                // Both run before the deletes, so a child counted through
                // either would refuse a delete that will be valid by then.
                Change::DropForeignKey { table, name } => {
                    this.removed.insert(Removed {
                        table: table.clone(),
                        constraint: Some(name.clone()),
                    });
                }
                Change::DropTable { name, .. } => {
                    this.removed.insert(Removed {
                        table: name.clone(),
                        // Every key the table holds goes with it.
                        constraint: None,
                    });
                }
                Change::UpdateRow {
                    table,
                    key_column,
                    key,
                    columns,
                    ..
                } => {
                    let moved = this.moved.entry(table.clone()).or_default();
                    moved.key_column = key_column.clone();
                    let updated = moved.updated.entry(key.clone()).or_default();
                    for (column, (_, after)) in columns {
                        let sql = match after {
                            Cell::Value(v) => written(v),
                            Cell::Default(d) => {
                                let constant = constant_default(d);
                                if constant.is_none() && !is_null_default(d) {
                                    moved
                                        .unprobeable
                                        .entry(column.clone())
                                        .or_default()
                                        .insert(key.clone());
                                }
                                constant.map(|e| format!("({e})"))
                            }
                        };
                        updated.insert(column.clone(), sql);
                    }
                }
                Change::InsertRow {
                    table,
                    key_column,
                    key,
                    row,
                    defaults,
                    ..
                } => {
                    let moved = this.moved.entry(table.clone()).or_default();
                    moved.key_column = key_column.clone();
                    let inserted = moved.inserted.entry(key.clone()).or_default();
                    // The key is written like any other column, and may be
                    // one column of a composite foreign key.
                    inserted.insert(key_column.clone(), literal(key.as_str()));
                    for (column, default) in defaults {
                        if let Some(expr) = constant_default(default) {
                            inserted.insert(column.clone(), format!("({expr})"));
                        } else if !is_null_default(default) {
                            moved
                                .unprobeable
                                .entry(column.clone())
                                .or_default()
                                .insert(key.clone());
                        }
                    }
                    for (column, value) in &row.0 {
                        if let Some(sql) = written(value) {
                            inserted.insert(column.clone(), sql);
                        }
                    }
                }
                Change::DeleteRow {
                    table,
                    key_column,
                    key,
                    ..
                } => {
                    let moved = this.moved.entry(table.clone()).or_default();
                    moved.key_column = key_column.clone();
                    moved.deleted.insert(key.clone());
                }
                // Exhaustive rather than `_`: a change added later that moves a
                // name has to be reflected here, or every probe downstream of
                // it would quietly query the wrong object.
                Change::AddColumn { .. }
                | Change::DropColumn { .. }
                | Change::AlterColumnType { .. }
                | Change::AlterColumnNullability { .. }
                | Change::AlterColumnDefault { .. }
                | Change::SetColumnDeprecated { .. }
                | Change::SetPrimaryKey { .. }
                | Change::AddUnique { .. }
                | Change::DropUnique { .. }
                | Change::AddForeignKey { .. }
                | Change::AddCheck { .. }
                | Change::DropCheck { .. }
                | Change::AddIndex { .. }
                | Change::DropIndex { .. }
                // A module carries no data and no identity, so nothing here
                // applies to one.
                | Change::CreateModule { .. }
                | Change::AlterModule { .. }
                | Change::DropModule { .. }
                // Setting the mode emits nothing and names nothing.
                | Change::SetDataMode { .. }
                // A role is a principal, not an object: nothing here names a
                // table or a column, and nothing here moves a name a probe
                // could ask about.
                | Change::CreateRole { .. }
                | Change::DropRole { .. }
                | Change::RenameRole { .. }
                | Change::Grant { .. }
                | Change::Revoke { .. } => {}
            }
        }
        this
    }

    /// `None` when the object does not exist yet, and so cannot be probed.
    fn table(&self, name: &TableName) -> Option<TableName> {
        if self.created.contains(name) {
            return None;
        }
        Some(
            self.tables
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.clone()),
        )
    }

    fn column(&self, column: &ColumnRef) -> Option<ColumnRef> {
        let table = self.table(&column.table)?;
        let name = self
            .columns
            .get(column)
            .cloned()
            .unwrap_or_else(|| column.name.clone());
        Some(table.column(&name))
    }
}

fn build(change: &Change, names: &AsStored) -> Result<Vec<Probe>, DialectError> {
    match change {
        Change::AlterColumnNullability {
            column,
            to_nullable: false,
            ..
        } => match names.column(column) {
            Some(stored) => Ok(vec![null_probe(column, &stored)?]),
            None => Ok(Vec::new()),
        },

        Change::AlterColumnType {
            column,
            from,
            to,
            from_nullable,
            to_nullable,
            ..
        } => {
            let Some(stored) = names.column(column) else {
                return Ok(Vec::new());
            };
            let mut out = Vec::new();
            // A type change folds a nullability change into itself (§12), so it
            // has to carry that change's probe too.
            if *from_nullable && !*to_nullable {
                out.push(null_probe(column, &stored)?);
            }
            if types::change_risk(from, to).risk_class().is_some() {
                out.extend(conversion_probe(column, &stored, to)?);
            }
            Ok(out)
        }

        Change::AddCheck {
            table,
            name,
            constraint,
        } => {
            let Some(stored) = names.table(table) else {
                return Ok(Vec::new());
            };
            Ok(vec![Probe::new(
                format!("rows that violate the new check {name}"),
                // A CHECK rejects a row only when its predicate is FALSE;
                // UNKNOWN passes. `WHERE NOT (expr)` has exactly that
                // behaviour, so the count matches what the engine will refuse.
                //
                // The expression is not rewritten for renames: rewriting SQL
                // text by substitution is how a tool that promised never to
                // parse SQL starts parsing it badly. Such a probe fails to run
                // and is reported as unchecked, which is honest.
                format!(
                    "SELECT COUNT(*) AS n FROM {} WHERE NOT ({});",
                    qualified(&stored)?,
                    constraint.expression
                ),
            )])
        }

        Change::AddUnique {
            table,
            name,
            constraint,
        } => match stored_columns(names, table, &constraint.columns) {
            Some((stored, columns)) => Ok(vec![duplicate_probe(
                &stored,
                &columns,
                &format!("rows that would collide under the new unique constraint {name}"),
            )?]),
            None => Ok(Vec::new()),
        },

        Change::SetPrimaryKey {
            table,
            to: Some(pk),
            ..
        } => {
            let Some((stored, columns)) = stored_columns(names, table, &pk.columns) else {
                return Ok(Vec::new());
            };
            let mut out = Vec::new();
            for (declared, current) in pk.columns.iter().zip(&columns) {
                out.push(null_probe(
                    &table.column(declared),
                    &stored.column(current),
                )?);
            }
            out.push(duplicate_probe(
                &stored,
                &columns,
                "rows that would collide under the new primary key",
            )?);
            Ok(out)
        }

        Change::AddForeignKey {
            table,
            name,
            constraint,
        } => {
            let Some((child_table, child_columns)) =
                stored_columns(names, table, &constraint.columns)
            else {
                return Ok(Vec::new());
            };
            let Some((parent_table, parent_columns)) = stored_columns(
                names,
                &constraint.references_table,
                &constraint.references_columns,
            ) else {
                return Ok(Vec::new());
            };

            // A row with any NULL in the key is exempt from the constraint
            // (MATCH SIMPLE, which is what SQL Server implements), so excluding
            // them is not leniency — it is the rule.
            let not_null: Vec<String> = child_columns
                .iter()
                .map(|c| Ok(format!("c.{} IS NOT NULL", quote(c)?)))
                .collect::<Result<_, DialectError>>()?;
            let join: Vec<String> = child_columns
                .iter()
                .zip(&parent_columns)
                .map(|(child, parent)| Ok(format!("p.{} = c.{}", quote(parent)?, quote(child)?)))
                .collect::<Result<_, DialectError>>()?;

            Ok(vec![Probe::new(
                format!("rows with no matching parent for the new foreign key {name}"),
                format!(
                    "SELECT COUNT(*) AS n FROM {} AS c\n WHERE {}\n   AND NOT EXISTS (SELECT 1 FROM {} AS p WHERE {});",
                    qualified(&child_table)?,
                    not_null.join("\n   AND "),
                    qualified(&parent_table)?,
                    join.join(" AND ")
                ),
            )])
        }

        // Everything else either cannot fail on data or cannot be counted; see
        // the module docs.
        Change::CreateTable { .. }
        | Change::DropTable { .. }
        | Change::RenameTable { .. }
        | Change::AddColumn { .. }
        | Change::DropColumn { .. }
        | Change::RenameColumn { .. }
        | Change::AlterColumnNullability {
            to_nullable: true, ..
        }
        | Change::AlterColumnDefault { .. }
        // A module holds no rows. A definition the engine will not compile
        // fails inside the plan's transaction, where the rollback is total.
        | Change::CreateModule { .. }
        | Change::AlterModule { .. }
        | Change::DropModule { .. }
        | Change::SetColumnDeprecated { .. }
        | Change::SetPrimaryKey { to: None, .. }
        | Change::DropUnique { .. }
        | Change::DropForeignKey { .. }
        | Change::DropCheck { .. }
        | Change::AddIndex { .. }
        | Change::DropIndex { .. }
        // Inserting and updating a declared row need no probe: the row's whole
        // content is in the plan, and anything the engine refuses about it
        // rolls the plan back.
        | Change::InsertRow { .. }
        | Change::UpdateRow { .. }
        | Change::SetDataMode { .. }
        // A permission change fails on nothing in the data; the engine refuses
        // an impossible grant inside the transaction.
        | Change::CreateRole { .. }
        | Change::DropRole { .. }
        | Change::RenameRole { .. } => Ok(Vec::new()),

        // Except for the one thing a permission change can fail on that is
        // not the permission: the securable. `validate` accepts a schema
        // target it cannot see inside — an external schema has no declared
        // objects — and this tool never creates a schema, so a grant on one
        // the database does not have is a statement the engine refuses.
        // Under `apply --staged` every change before it has committed by
        // then (DECISIONS 134). An *object* target needs no probe: it is
        // declared, so it exists or this plan creates it.
        Change::Grant { role, target, .. } | Change::Revoke { role, target, .. } => {
            match target {
                pbps_model::GrantTarget::Schema(schema) => Ok(vec![schema_probe(role, schema)]),
                pbps_model::GrantTarget::Object(_) => Ok(Vec::new()),
            }
        }

        Change::DeleteRow {
            table,
            key_column,
            key,
            ..
        } => match names.column(&table.column(key_column)) {
            Some(stored) => {
                let mut probes = vec![delete_probe(table, key, &stored, names)?];
                // And, per table this plan writes to a default the probe
                // cannot evaluate, the refusal where a foreign key to this
                // table spans such a column (124). After the count, so a
                // reader meets the rows first.
                for (child, moved) in &names.moved {
                    if let Some(p) = unprobeable_probe(table, key, &stored, child, moved, names)? {
                        probes.push(p);
                    }
                }
                Ok(probes)
            }
            None => Ok(Vec::new()),
        },
    }
}

/// What a plan takes away from one child table before its deletes run.
enum Removal {
    /// The table itself: every key it holds goes with it.
    Table,
    /// The named constraints, and nothing else.
    Keys(Vec<String>),
}

impl AsStored {
    /// The keys this plan removes from `stored_child` before the deletes,
    /// under the names the catalog has now. Read by both probes over foreign
    /// keys: a constraint that will be gone must count nothing (DECISIONS
    /// 128).
    fn removed_from(&self, stored_child: &TableName) -> Removal {
        let mut keys = Vec::new();
        for removed in &self.removed {
            let table = self
                .table(&removed.table)
                .unwrap_or_else(|| removed.table.clone());
            if &table != stored_child {
                continue;
            }
            match &removed.constraint {
                Some(constraint) => keys.push(constraint.clone()),
                None => return Removal::Table,
            }
        }
        Removal::Keys(keys)
    }
}

/// The rows in other tables that still point at a row about to be deleted
/// (ADR-0004).
///
/// # Why the referencing tables are found at run time
///
/// A probe is built from the plan and nothing else, and the plan does not know
/// which tables reference this one — nor should it trust the declarations to
/// say: a foreign key someone added by hand is exactly the one that will refuse
/// the delete. So the statement asks `sys.foreign_keys` for every column that
/// references the key column, builds one `COUNT(*)` per referencing table, and
/// runs the result through `sp_executesql`. Nothing user-written reaches the
/// dynamic text: the table and column names come from the catalog and go
/// through `QUOTENAME`, and the key is bound as a parameter.
///
/// # Why `ON DELETE CASCADE` counts too
///
/// The engine would not refuse such a delete — it would take the child rows
/// with it, silently. A reference row's delete cascading into an application
/// table is the disaster the `data-delete` gate exists for, so those rows are
/// counted and the apply stops the same way.
///
/// # Rows the plan itself moves
///
/// A child row that this plan updates or deletes is left out of the count. The
/// ordinary shape is a child moved to a new parent in the same revision, and
/// the plan runs that update *before* the delete precisely so the engine
/// accepts it — a probe that ran before the first statement would otherwise
/// refuse every such plan. The exclusion is by key, not by which column the
/// update touches, so it can over-exclude a row whose update leaves it still
/// pointing at the doomed parent; that row is then refused by the engine inside
/// the transaction, where the rollback is total. Under-counting fails loudly,
/// which is the direction to be wrong in.
fn delete_probe(
    table: &TableName,
    key: &RowKey,
    stored: &ColumnRef,
    names: &AsStored,
) -> Result<Probe, DialectError> {
    // One count per foreign key, and a foreign key is a tuple: the child's
    // columns are compared against the parent row's together, through a
    // conjunction the engine assembles from `sys.foreign_key_columns` at run
    // time (`tuple`). One count per *column* — the first cut — matched a child
    // on the surviving parent `(1, 2)` against deleting `(1, 1)` by its first
    // column, and refused every such delete (DECISIONS 121).
    //
    // Per referencing table, the rows this plan moves in it, under the name
    // the database has for the table now. Doubled quotes throughout, because
    // each fragment is itself inside a T-SQL string literal. Which columns a
    // foreign key spans only the catalog knows, so the fragments that depend
    // on it are guarded at run time (`touches`, `reaches_beyond`): a deleted
    // row is gone for every key, an updated row is left out only of the keys
    // its update moves it off, and a row updated elsewhere still points at
    // the parent, and is counted.
    let parent = qualified(&stored.table)?;
    // The row being deleted, named by the column the *plan* keys it on. Every
    // fragment below that means "this parent row" says so this way, because a
    // foreign key may reference some other unique key of the same row, and
    // its referenced columns are then not the key column at all (DECISIONS
    // 116). `p` is the parent and `ch` the child throughout the generated
    // statement.
    let parent_row = format!(
        "SELECT 1 FROM {parent} AS p WHERE p.{} = @key",
        quote(&stored.name)?
    );
    let mut exclusions = Vec::new();
    let mut arrivals = Vec::new();
    for (child, moved) in &names.moved {
        let Some(stored_child) = names.table(child) else {
            continue;
        };
        let Some(stored_key) = names.column(&child.column(&moved.key_column)) else {
            continue;
        };
        let key_sql = format!("ch.{}", quote(&stored_key.name)?);
        let child_sql = qualified(&stored_child)?;
        // A column the database does not have yet cannot be in a foreign
        // key it has; one it names differently is asked about by that name.
        let stored_name = |column: &str| names.column(&child.column(column)).map(|r| r.name);
        // A quoted identifier may itself hold a `'` — `quote` escapes `]`,
        // not quotes — so every fragment carrying one into the generated
        // statement goes through `literal`, never straight into the outer
        // `N'...'`.
        let mut pieces = Vec::new();
        let mut terms = Vec::new();
        // Deleted rows are gone whatever they pointed at.
        if !moved.deleted.is_empty() {
            let list: Vec<String> = moved.deleted.iter().map(|k| literal(k.as_str())).collect();
            pieces.push(literal(&format!(
                " AND {key_sql} NOT IN ({})",
                list.join(", ")
            )));
        }
        for (row_key, columns) in &moved.updated {
            let mut comparable = BTreeMap::new();
            let mut uncomparable = BTreeSet::new();
            for (column, after) in columns {
                let Some(name) = stored_name(column) else {
                    continue;
                };
                match after {
                    Some(sql) => {
                        comparable.insert(name, sql.clone());
                    }
                    None => {
                        uncomparable.insert(name);
                    }
                }
            }
            if comparable.is_empty() {
                continue;
            }
            // The row's tuple after the update: the value the update sets
            // where it sets one, the stored cell elsewhere. It is left out
            // of a key's count only when the engine says that tuple is not
            // the deleted row's — so `01` and `1` are one key — and counted
            // for a key the update sets a cell of to something the probe
            // cannot compare, which is the direction to be wrong in.
            let after = side(&comparable);
            let row = literal(row_key.as_str());
            let excluded = format!(
                "{} + {} + {}",
                literal(&format!(
                    " AND NOT ({key_sql} = {row} AND NOT EXISTS ({parent_row}"
                )),
                tuple(&after),
                literal("))")
            );
            pieces.push(guarded(&uncomparable, &comparable, &excluded));
            // And the same row arriving on the parent, which the count
            // cannot see (see [`Moved::inserted`]): the row exists, so it
            // may already be inside the count, and only one that is *not*
            // on the parent now is arriving. A term added to the count
            // rather than a clause narrowing it, so a whole parenthesised
            // expression.
            let arrived = format!(
                "{} + {} + {} + {} + {}",
                literal(&format!(
                    " + (SELECT COUNT(*) FROM {child_sql} AS ch WHERE {key_sql} = {row} AND \
                     NOT EXISTS ({parent_row}"
                )),
                tuple(STORED),
                literal(&format!(") AND EXISTS ({parent_row}")),
                tuple(&after),
                literal("))")
            );
            terms.push(guarded(&uncomparable, &comparable, &arrived));
        }
        for columns in moved.inserted.values() {
            let known: BTreeMap<String, String> = columns
                .iter()
                .filter_map(|(column, sql)| stored_name(column).map(|name| (name, sql.clone())))
                .collect();
            if known.is_empty() {
                continue;
            }
            // Nothing stored to double-count: the row is either arriving on
            // the deleted row or it is not. A key spanning a column the
            // insert leaves to NULL, or to a default that is not a literal,
            // is not one the probe can ask about (117).
            let arrived = format!(
                "{} + {} + {}",
                literal(&format!(" + (CASE WHEN EXISTS ({parent_row}")),
                tuple(&side(&known)),
                literal(") THEN 1 ELSE 0 END)")
            );
            terms.push(format!(
                "CASE WHEN {} THEN N'' ELSE {arrived} END",
                reaches_beyond(known.keys())
            ));
        }
        let exclusion = if pieces.is_empty() {
            "N''".to_owned()
        } else {
            pieces.join(" + ")
        };
        exclusions.push(format!(
            "WHEN s.name = {} AND t.name = {} THEN {exclusion}",
            literal(&stored_child.schema),
            literal(&stored_child.name),
        ));
        if !terms.is_empty() {
            arrivals.push(format!(
                "WHEN s.name = {} AND t.name = {} THEN {}",
                literal(&stored_child.schema),
                literal(&stored_child.name),
                terms.join(" + ")
            ));
        }
    }
    let exclusion = if exclusions.is_empty() {
        "N''".to_owned()
    } else {
        format!("CASE {} ELSE N'' END", exclusions.join(" "))
    };
    let arrival = if arrivals.is_empty() {
        "N''".to_owned()
    } else {
        format!("CASE {} ELSE N'' END", arrivals.join(" "))
    };
    // The keys this plan takes away first are left out of the catalog read
    // altogether: by the time the delete runs the constraint is gone, and a
    // child counted through it refuses a delete the engine would accept.
    // Named with its table, because two schemas may each hold a constraint
    // of the same name (DECISIONS 128).
    let mut gone = Vec::new();
    for removed in &names.removed {
        let table = names.table(&removed.table).unwrap_or(removed.table.clone());
        let of_table = format!(
            "s.name = {} AND t.name = {}",
            literal(&table.schema),
            literal(&table.name)
        );
        gone.push(match &removed.constraint {
            Some(constraint) => {
                format!("AND NOT ({of_table} AND fk.name = {})", literal(constraint))
            }
            None => format!("AND NOT ({of_table})"),
        });
    }
    let gone = gone.join("\n                            ");

    Ok(Probe::new(
        format!(
            "rows in other tables that still reference {table} row `{key}`, which its delete \
             would orphan or cascade into"
        ),
        format!(
            "{}\nSELECT @n AS n;",
            counting_statement(&Referencing {
                parent: &parent,
                parent_row: &parent_row,
                key: &literal(key.as_str()),
                // The probe only reads; the delete's own guard is the
                // one that has to keep what it counted (see
                // [`still_referenced`]).
                hint: "",
                gone: &gone,
                exclusion: &exclusion,
                arrival: &arrival,
            })
        ),
    ))
}

/// The parts of one "rows referencing this parent row" statement that differ
/// between the probe and the delete's own guard.
struct Referencing<'a> {
    /// The parent table, quoted and qualified.
    parent: &'a str,
    /// `SELECT 1 FROM <parent> AS p WHERE p.<key column> = @key`.
    parent_row: &'a str,
    /// The key of the row being deleted, as a T-SQL literal.
    key: &'a str,
    /// A table hint for the child scan, or `""`.
    hint: &'a str,
    /// Catalog filters that leave keys out of the read entirely.
    gone: &'a str,
    /// Per-child-table SQL excluding rows the plan itself moves, or `N''`.
    exclusion: &'a str,
    /// Per-child-table SQL adding rows the plan puts onto the parent, or `N''`.
    arrival: &'a str,
}

/// Counts, into `@n`, the rows of every table with a foreign key into the
/// parent whose key tuple is the deleted row's.
///
/// The statements are built in a derived table and aggregated outside it: the
/// fragments carry subqueries over the key's columns, and an aggregate's
/// argument may not (Msg 130). Nothing user-written reaches the dynamic text —
/// names come from the catalog through `QUOTENAME`, and the key is a bound
/// parameter.
fn counting_statement(r: &Referencing<'_>) -> String {
    let Referencing {
        parent,
        parent_row,
        key,
        hint,
        gone,
        exclusion,
        arrival,
    } = r;
    format!(
        "DECLARE @n int = 0, @sql nvarchar(max);\n\
         SELECT @sql = STRING_AGG(CONVERT(nvarchar(max), x.stmt), N' ')\n\
           FROM (SELECT N'SELECT @n += (SELECT COUNT(*) FROM ' + QUOTENAME(s.name) + N'.' + QUOTENAME(t.name)\n\
                 + {} + {} + N')' + {exclusion} + N')' + {arrival} + N';' AS stmt\n\
                   FROM sys.foreign_keys fk\n\
                   JOIN sys.tables t ON t.object_id = fk.parent_object_id\n\
                   JOIN sys.schemas s ON s.schema_id = t.schema_id\n\
                  WHERE fk.referenced_object_id = OBJECT_ID({})\n\
                        {gone}) AS x;\n\
         IF @sql IS NOT NULL EXEC sp_executesql @sql, N'@key nvarchar(max), @n int OUTPUT', @key = {key}, @n = @n OUTPUT;",
        literal(&format!(" AS ch{hint} WHERE EXISTS ({parent_row}")),
        tuple(STORED),
        literal(parent),
    )
}

/// The guard a row delete carries: the same count, taken inside the delete's
/// own transaction under range locks it keeps until that transaction ends, so
/// that a child row committed between the preflight probe and the delete
/// cannot be cascaded away unseen (DECISIONS 129).
///
/// It is not the probe: by the time this runs, every insert, update and child
/// delete of the plan has run (`order_key`), so *any* row still referencing
/// the parent is one the probe did not account for. The probe stays where it
/// is — it reports the number before anything runs, which is what a human
/// approves; this refuses what changed underneath it.
pub(crate) fn still_referenced(
    table: &TableName,
    key_column: &str,
    key: &RowKey,
) -> Result<String, DialectError> {
    let parent = qualified(table)?;
    let parent_row = format!(
        "SELECT 1 FROM {parent} AS p WHERE p.{} = @key",
        quote(key_column)?
    );
    Ok(format!(
        "{}\nIF @n > 0 THROW 50000, {}, 1;",
        counting_statement(&Referencing {
            parent: &parent,
            parent_row: &parent_row,
            key: &literal(key.as_str()),
            // Serializable range locks over exactly the child rows this
            // asks about, held to the end of the transaction: an insert or
            // an update moving a row into the range waits for the delete
            // instead of racing it.
            hint: " WITH (HOLDLOCK)",
            // Both belong to a plan the probe reads whole; here the plan has
            // already run, and a row referencing the parent now is a row
            // nobody accounted for.
            gone: "",
            exclusion: "N\'\'",
            arrival: "N\'\'",
        }),
        literal(&format!(
            "{table} row `{key}` is referenced by row(s) that arrived after this plan was \
             checked; the delete would orphan or cascade into them. Nothing was applied. \
             Plan again."
        ))
    ))
}

/// The columns of the foreign key the generated statement is being built
/// for (`fk`), with their names on the child side (`c`) and the parent side
/// (`rc`).
const KEY_COLUMNS: &str = "FROM sys.foreign_key_columns fkc \
     JOIN sys.columns c ON c.object_id = fkc.parent_object_id AND c.column_id = fkc.parent_column_id \
     JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
     WHERE fkc.constraint_object_id = fk.object_id";

/// The child's stored cell, as the child side of a column of the key.
const STORED: &str = "N'ch.' + QUOTENAME(c.name)";

/// The key as a conjunction the engine assembles at run time: ` AND p.<rc>
/// = <child side>` for each of its columns, where `child_side` is SQL over
/// `c.name` spelling what the child holds in that column.
fn tuple(child_side: &str) -> String {
    format!(
        "(SELECT STRING_AGG(CONVERT(nvarchar(max), N' AND p.' + QUOTENAME(rc.name) + N' = ' + \
         {child_side}), N'') {KEY_COLUMNS})"
    )
}

/// A row's tuple as the plan writes it: the value written in each column
/// the plan spells, the stored cell in any other. Spelled for the engine as
/// a `CASE` over the key's columns.
fn side(values: &BTreeMap<String, String>) -> String {
    let whens: Vec<String> = values
        .iter()
        .map(|(name, sql)| format!("WHEN {} THEN {}", literal(name), literal(sql)))
        .collect();
    format!("CASE c.name {} ELSE {STORED} END", whens.join(" "))
}

/// Whether the key spans one of `columns`.
fn touches<'a>(columns: impl Iterator<Item = &'a String>) -> String {
    let list: Vec<String> = columns.map(|c| literal(c)).collect();
    if list.is_empty() {
        return "1 = 0".to_owned();
    }
    format!(
        "EXISTS (SELECT 1 {KEY_COLUMNS} AND c.name IN ({}))",
        list.join(", ")
    )
}

/// Whether the key spans a column outside `columns`.
fn reaches_beyond<'a>(columns: impl Iterator<Item = &'a String>) -> String {
    let list: Vec<String> = columns.map(|c| literal(c)).collect();
    if list.is_empty() {
        return "1 = 1".to_owned();
    }
    format!(
        "EXISTS (SELECT 1 {KEY_COLUMNS} AND c.name NOT IN ({}))",
        list.join(", ")
    )
}

/// `fragment` for a key the update moves a row along, nothing for one it
/// does not touch — and nothing, so the row stays counted, for a key it sets
/// a cell of to something the probe cannot compare.
fn guarded(
    uncomparable: &BTreeSet<String>,
    comparable: &BTreeMap<String, String>,
    fragment: &str,
) -> String {
    let mut arms = String::new();
    if !uncomparable.is_empty() {
        arms.push_str(&format!("WHEN {} THEN N'' ", touches(uncomparable.iter())));
    }
    format!(
        "CASE {arms}WHEN {} THEN {fragment} ELSE N'' END",
        touches(comparable.keys())
    )
}

/// A write this plan leaves to a default the probe cannot evaluate, on a
/// column that a foreign key to the deleted row's table spans. No arrival
/// can be counted for it, and treating it as absent let a default naming
/// the deleted row arrive unseen, for `ON DELETE CASCADE` to take the
/// declared child (DECISIONS 124). Refused by count — a probe that errors
/// is "unchecked" to `apply`, which then proceeds — with the columns and
/// rows named, and the remedy: spell the value.
///
/// Every foreign key from the child to the parent table, not only the ones
/// to the deleted key: which key of the parent the default names is exactly
/// what cannot be evaluated here.
fn unprobeable_probe(
    table: &TableName,
    key: &RowKey,
    stored: &ColumnRef,
    child: &TableName,
    moved: &Moved,
    names: &AsStored,
) -> Result<Option<Probe>, DialectError> {
    let Some(stored_child) = names.table(child) else {
        return Ok(None);
    };
    // The same filtering `delete_probe` does: a key this plan removes before
    // the deletes cannot carry a default onto the deleted row, and refusing
    // the write for it refuses a plan the engine would accept (DECISIONS 128).
    let removed = match names.removed_from(&stored_child) {
        Removal::Table => return Ok(None),
        Removal::Keys(keys) => keys,
    };
    let mut columns = Vec::new();
    let mut described = Vec::new();
    for (column, rows) in &moved.unprobeable {
        let Some(stored_column) = names.column(&child.column(column)) else {
            continue;
        };
        columns.push(literal(&stored_column.name));
        let rows: Vec<String> = rows.iter().map(|r| format!("`{r}`")).collect();
        described.push(format!(
            "{column} (row{} {})",
            if rows.len() == 1 { "" } else { "s" },
            rows.join(", ")
        ));
    }
    if columns.is_empty() {
        return Ok(None);
    }
    Ok(Some(Probe::new(
        format!(
            "foreign-key column(s) of {child} that reference {table} and that this plan writes to \
             a default the probe cannot evaluate, which may be row `{key}` being deleted: {}; \
             spell the value",
            described.join(", ")
        ),
        format!(
            "SELECT COUNT(*) AS n\n  \
               FROM sys.foreign_keys fk\n  \
               JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id\n  \
               JOIN sys.columns c ON c.object_id = fkc.parent_object_id AND c.column_id = fkc.parent_column_id\n \
              WHERE fk.referenced_object_id = OBJECT_ID({})\n   \
                AND fk.parent_object_id = OBJECT_ID({})\n   \
                AND c.name IN ({}){};",
            literal(&qualified(&stored.table)?),
            literal(&qualified(&stored_child)?),
            columns.join(", "),
            if removed.is_empty() {
                String::new()
            } else {
                format!(
                    "\n     AND fk.name NOT IN ({})",
                    removed
                        .iter()
                        .map(|k| literal(k))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        ),
    )))
}

/// The table and columns as the database currently names them, or `None` when
/// the table does not exist yet.
fn stored_columns(
    names: &AsStored,
    table: &TableName,
    columns: &[String],
) -> Option<(TableName, Vec<String>)> {
    let stored = names.table(table)?;
    let columns = columns
        .iter()
        .map(|c| names.column(&table.column(c)).map(|r| r.name))
        .collect::<Option<Vec<_>>>()?;
    Some((stored, columns))
}

/// A schema a permission names, which has to be there before the statement
/// runs: one, if it is not.
fn schema_probe(role: &str, schema: &str) -> Probe {
    Probe::new(
        format!(
            "`{schema}`, the schema this plan grants `{role}` permissions on, missing from the \
             database — create it, or write the target the way the database spells it"
        ),
        format!(
            "SELECT CASE WHEN SCHEMA_ID({}) IS NULL THEN 1 ELSE 0 END AS n;",
            literal(schema)
        ),
    )
}

/// `column` names it as the plan does — for the message a human reads — and
/// `stored` as the database does, for the query.
fn null_probe(column: &ColumnRef, stored: &ColumnRef) -> Result<Probe, DialectError> {
    Ok(Probe::new(
        format!("existing NULLs in {column}, which NOT NULL would reject"),
        format!(
            "SELECT COUNT(*) AS n FROM {} WHERE {} IS NULL;",
            qualified(&stored.table)?,
            quote(&stored.name)?
        ),
    ))
}

fn duplicate_probe(
    table: &TableName,
    columns: &[String],
    description: &str,
) -> Result<Probe, DialectError> {
    let list = columns
        .iter()
        .map(|c| quote(c))
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");
    Ok(Probe::new(
        description,
        // The rows, not the groups: "3 duplicate groups" makes an operator do
        // arithmetic before they know how much data is involved. `GROUP BY`
        // treats NULLs as equal, which is also how SQL Server's UNIQUE treats
        // them, so the count matches what the engine will refuse.
        format!(
            "SELECT ISNULL(SUM(c), 0) AS n FROM (\n    SELECT COUNT(*) AS c FROM {}\n     GROUP BY {} HAVING COUNT(*) > 1) AS dup;",
            qualified(table)?,
            list
        ),
    ))
}

/// The probe for narrowing a column's type, or `None` when no count exists.
fn conversion_probe(
    column: &ColumnRef,
    stored: &ColumnRef,
    to: &ColumnType,
) -> Result<Vec<Probe>, DialectError> {
    let table = qualified(&stored.table)?;
    let col = quote(&stored.name)?;

    // A bounded string or binary target truncates silently under CONVERT, so
    // `TRY_CONVERT` would return a value and report nothing. Length is the only
    // question that has an answer here.
    let bound = match to.args.first() {
        Some(TypeArg::Int(n)) => Some(*n),
        Some(TypeArg::Max) | Some(TypeArg::Ident(_)) | None => None,
    };
    if let Some(n) = bound {
        match to.base.as_str() {
            // LEN ignores trailing blanks, and so does the engine when it
            // shortens a character column — counting them would report rows
            // that convert perfectly well.
            "char" | "nchar" | "varchar" | "nvarchar" => {
                return Ok(vec![Probe::new(
                    format!("values in {column} too long for {to}"),
                    format!(
                        "SELECT COUNT(*) AS n FROM {table} WHERE {col} IS NOT NULL AND LEN({col}) > {n};"
                    ),
                )]);
            }
            "binary" | "varbinary" => {
                return Ok(vec![Probe::new(
                    format!("values in {column} too long for {to}"),
                    format!(
                        "SELECT COUNT(*) AS n FROM {table} WHERE {col} IS NOT NULL AND DATALENGTH({col}) > {n};"
                    ),
                )]);
            }
            _ => {}
        }
    }

    // Anything else: let the engine answer. `TRY_CONVERT` returns NULL exactly
    // where `CONVERT` would raise, which is the definition of the rows that
    // would break the ALTER.
    //
    // The type is interpolated, not bound: `TRY_CONVERT`'s first argument is a
    // type, and a type is not a value a parameter can carry. It comes from the
    // dialect's own catalogue, never from user text — `normalize` has already
    // rejected anything that is not a type this dialect knows.
    if types::normalize(to).is_err() {
        return Ok(Vec::new());
    }
    Ok(vec![Probe::new(
        format!("values in {column} that cannot become {to}"),
        format!(
            "SELECT COUNT(*) AS n FROM {table}\n WHERE {col} IS NOT NULL AND TRY_CONVERT({to}, {col}) IS NULL;"
        ),
    )])
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{
        CheckConstraint, ForeignKey, PlannedChange, PrimaryKey, ReferentialAction, UniqueConstraint,
    };

    fn cref(s: &str) -> ColumnRef {
        s.parse().unwrap()
    }
    fn tname(s: &str) -> TableName {
        s.parse().unwrap()
    }
    fn ty(s: &str) -> ColumnType {
        types::normalize(&s.parse::<ColumnType>().unwrap()).unwrap()
    }
    fn uid(s: &str) -> pbps_model::Uid {
        s.parse().unwrap()
    }
    fn plan(changes: Vec<Change>) -> ChangeSet {
        ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        }
    }
    fn sql_of(change: &Change) -> Vec<String> {
        probes(&plan(vec![change.clone()]))
            .into_iter()
            .map(|p| p.sql)
            .collect()
    }

    #[test]
    fn tightening_nullability_counts_the_nulls() {
        let sql = sql_of(&Change::AlterColumnNullability {
            uid: uid("c_aaaaaa"),
            column: cref("dbo.customer.email"),
            ty: ty("nvarchar(255)"),
            to_nullable: false,
        });
        assert_eq!(
            sql,
            ["SELECT COUNT(*) AS n FROM [dbo].[customer] WHERE [email] IS NULL;"]
        );
    }

    /// Relaxing to nullable cannot fail, and a probe that always returns zero
    /// is noise an operator learns to skip.
    #[test]
    fn relaxing_nullability_needs_no_probe() {
        assert!(
            sql_of(&Change::AlterColumnNullability {
                uid: uid("c_aaaaaa"),
                column: cref("dbo.customer.email"),
                ty: ty("nvarchar(255)"),
                to_nullable: true,
            })
            .is_empty()
        );
    }

    /// The differ folds a type change and a nullability change into one change
    /// (§12); folding must not lose the probe the separate change would have
    /// carried.
    #[test]
    fn a_type_change_that_also_tightens_nullability_probes_both() {
        let sql = sql_of(&Change::AlterColumnType {
            uid: uid("c_aaaaaa"),
            column: cref("dbo.customer.email"),
            from: ty("nvarchar(255)"),
            to: ty("nvarchar(50)"),
            from_nullable: true,
            to_nullable: false,
        });
        assert_eq!(sql.len(), 2, "{sql:?}");
        assert!(sql[0].contains("IS NULL"), "{sql:?}");
        assert!(sql[1].contains("LEN([email]) > 50"), "{sql:?}");
    }

    /// A shorter string truncates silently under CONVERT, so TRY_CONVERT would
    /// report nothing at all — length is the only question with an answer.
    #[test]
    fn shortening_a_string_is_probed_by_length_not_conversion() {
        let sql = sql_of(&Change::AlterColumnType {
            uid: uid("c_aaaaaa"),
            column: cref("dbo.customer.email"),
            from: ty("nvarchar(255)"),
            to: ty("nvarchar(50)"),
            from_nullable: true,
            to_nullable: true,
        });
        assert_eq!(sql.len(), 1);
        assert!(sql[0].contains("LEN([email]) > 50"), "{sql:?}");
        assert!(!sql[0].contains("TRY_CONVERT"), "{sql:?}");
    }

    #[test]
    fn narrowing_a_number_is_probed_by_conversion() {
        let sql = sql_of(&Change::AlterColumnType {
            uid: uid("c_aaaaaa"),
            column: cref("dbo.customer.balance"),
            from: ty("bigint"),
            to: ty("int"),
            from_nullable: true,
            to_nullable: true,
        });
        assert_eq!(sql.len(), 1);
        assert!(
            sql[0].contains("TRY_CONVERT(int, [balance]) IS NULL"),
            "{sql:?}"
        );
    }

    /// Widening cannot fail, so it is not probed — the same principle as
    /// relaxing nullability.
    #[test]
    fn widening_a_type_needs_no_probe() {
        assert!(
            sql_of(&Change::AlterColumnType {
                uid: uid("c_aaaaaa"),
                column: cref("dbo.customer.balance"),
                from: ty("int"),
                to: ty("bigint"),
                from_nullable: true,
                to_nullable: true,
            })
            .is_empty()
        );
    }

    #[test]
    fn a_check_counts_the_rows_it_would_reject() {
        let sql = sql_of(&Change::AddCheck {
            table: tname("dbo.customer"),
            name: "ck_customer_amount".into(),
            constraint: CheckConstraint {
                expression: "[amount] >= 0".into(),
            },
        });
        assert_eq!(
            sql,
            ["SELECT COUNT(*) AS n FROM [dbo].[customer] WHERE NOT ([amount] >= 0);"]
        );
    }

    #[test]
    fn a_unique_constraint_counts_the_colliding_rows() {
        let sql = sql_of(&Change::AddUnique {
            table: tname("dbo.customer"),
            name: "uq_customer_email".into(),
            constraint: UniqueConstraint {
                columns: vec!["email".into(), "tenant".into()],
            },
        });
        assert_eq!(sql.len(), 1);
        assert!(sql[0].contains("GROUP BY [email], [tenant]"), "{sql:?}");
        assert!(sql[0].contains("HAVING COUNT(*) > 1"), "{sql:?}");
        // The rows, not the groups.
        assert!(sql[0].contains("SUM(c)"), "{sql:?}");
    }

    #[test]
    fn a_foreign_key_counts_the_orphans() {
        let sql = sql_of(&Change::AddForeignKey {
            table: tname("dbo.customer"),
            name: "fk_customer_region".into(),
            constraint: Box::new(ForeignKey {
                columns: vec!["region_id".into()],
                references_table: tname("dbo.region"),
                references_columns: vec!["region_id".into()],
                on_delete: ReferentialAction::NoAction,
                on_update: ReferentialAction::NoAction,
            }),
        });
        assert_eq!(sql.len(), 1);
        assert!(sql[0].contains("NOT EXISTS"), "{sql:?}");
        assert!(sql[0].contains("[dbo].[region] AS p"), "{sql:?}");
        // A NULL key is exempt from the constraint, so it must be exempt from
        // the probe too, or every optional relationship reports false orphans.
        assert!(sql[0].contains("c.[region_id] IS NOT NULL"), "{sql:?}");
    }

    #[test]
    fn a_new_primary_key_probes_nulls_and_duplicates() {
        let sql = sql_of(&Change::SetPrimaryKey {
            table: tname("dbo.customer"),
            from: None,
            to: Some(PrimaryKey {
                name: Some("pk_customer".into()),
                columns: vec!["id".into()],
            }),
        });
        assert_eq!(sql.len(), 2, "{sql:?}");
        assert!(sql[0].contains("[id] IS NULL"), "{sql:?}");
        assert!(sql[1].contains("GROUP BY [id]"), "{sql:?}");
    }

    /// Named rather than left to be discovered: these carry no data question a
    /// count can answer (see the module docs).
    #[test]
    fn drops_and_renames_carry_no_probe() {
        for change in [
            Change::DropTable {
                uid: uid("t_aaaaaa"),
                name: tname("dbo.customer"),
            },
            Change::DropColumn {
                uid: uid("c_aaaaaa"),
                column: cref("dbo.customer.email"),
            },
            Change::RenameColumn {
                uid: uid("c_aaaaaa"),
                table: tname("dbo.customer"),
                from: "email".into(),
                to: "contact_email".into(),
            },
            Change::SetPrimaryKey {
                table: tname("dbo.customer"),
                from: Some(PrimaryKey {
                    name: None,
                    columns: vec!["id".into()],
                }),
                to: None,
            },
        ] {
            assert!(sql_of(&change).is_empty(), "{change:?}");
        }
    }

    /// The connected half of ADR-0004: a row delete counts what still points
    /// at the row, finds the referencing tables in the catalog at run time, and
    /// leaves out the rows this plan itself moves — under the names the
    /// database has for them now.
    #[test]
    fn deleting_a_row_counts_the_rows_that_still_reference_it() {
        let cs = plan(vec![Change::DeleteRow {
            table: tname("dbo.status"),
            key_column: "code".into(),
            key: RowKey::from("old"),
            cause: pbps_model::change::DeleteCause::Undeclared,
        }]);
        let p = probes(&cs);
        assert_eq!(p.len(), 1, "{p:?}");
        let sql = &p[0].sql;
        assert!(sql.contains("OBJECT_ID(N'[dbo].[status]')"), "{sql}");
        // Every foreign key to the table, whatever key of it they target:
        // the filter that was here dropped a constraint referencing some
        // other unique key out of the query altogether (116).
        assert!(!sql.contains("rc.name = N'code'"), "{sql}");
        // One count per foreign key, and the child is compared against the
        // row being deleted column by column, as one tuple the engine
        // assembles from the key's columns (121).
        assert!(
            sql.contains("FROM sys.foreign_keys fk\nJOIN sys.tables t"),
            "{sql}"
        );
        assert!(
            sql.contains(
                "N' AS ch WHERE EXISTS (SELECT 1 FROM [dbo].[status] AS p WHERE p.[code] = @key' + \
                 (SELECT STRING_AGG(CONVERT(nvarchar(max), N' AND p.' + QUOTENAME(rc.name) + \
                 N' = ' + N'ch.' + QUOTENAME(c.name)), N'') FROM sys.foreign_key_columns fkc"
            ),
            "{sql}"
        );
        assert!(
            sql.contains("WHERE fkc.constraint_object_id = fk.object_id) + N')'"),
            "{sql}"
        );
        assert!(sql.contains("@key = N'old'"), "{sql}");
        // The deleted row itself is excluded from a self-referencing count: a
        // row pointing at itself is gone with the delete, not orphaned by it.
        assert!(
            sql.contains(
                "WHEN s.name = N'dbo' AND t.name = N'status' THEN N' AND ch.[code] NOT IN (N''old'')'"
            ),
            "{sql}"
        );
        assert!(p[0].description.contains("dbo.status"), "{:?}", p[0]);
        assert!(p[0].description.contains("`old`"), "{:?}", p[0]);
    }

    /// A write the plan does not spell arrives at the column's default: an
    /// insert that omits the column, an update to `DEFAULT`. A literal one
    /// is rendered for the engine to compare; one that is not a literal, or
    /// `NULL`, is no arrival (DECISIONS 117).
    #[test]
    fn a_defaulted_write_onto_the_deleted_parent_is_an_arrival() {
        use pbps_model::Cell;
        let delete = Change::DeleteRow {
            table: tname("dbo.status"),
            key_column: "code".into(),
            key: RowKey::from("old"),
            cause: pbps_model::change::DeleteCause::Undeclared,
        };
        let insert = |default: &str| Change::InsertRow {
            table: tname("dbo.kind"),
            key_column: "id".into(),
            identity_key: false,
            key: RowKey::from("9"),
            row: pbps_model::Row::default(),
            defaults: [("status_code".to_owned(), default.to_owned())]
                .into_iter()
                .collect(),
            types: Default::default(),
        };
        let sql_of = |cs: &ChangeSet| probes(cs)[0].sql.clone();

        let sql = sql_of(&plan(vec![insert("('old')"), delete.clone()]));
        assert!(
            sql.contains("WHEN N'status_code' THEN N'(''old'')'")
                && sql.contains(") THEN 1 ELSE 0 END)'"),
            "the literal default is what the engine compares: {sql}"
        );
        // Not a literal: nothing to compare before it runs. NULL: no row.
        // The key is still written, so a key spanning it is asked about,
        // and one spanning `status_code` is not.
        for default in ["(NEXT VALUE FOR [dbo].[s])", "(CONVERT(int, 1))", "(NULL)"] {
            let sql = sql_of(&plan(vec![insert(default), delete.clone()]));
            assert!(!sql.contains("N'status_code'"), "{default}: {sql}");
            assert!(
                sql.contains("AND c.name NOT IN (N'id')) THEN N'' ELSE"),
                "{default}: {sql}"
            );
        }
        // A default the probe cannot evaluate may name the deleted row, so
        // where a foreign key to the table spans the column the write is
        // refused, by a second probe that counts such columns (124). `NULL`
        // names no row and needs none.
        for default in ["(NEXT VALUE FOR [dbo].[s])", "(CONVERT(int, 1))"] {
            let p = probes(&plan(vec![insert(default), delete.clone()]));
            assert_eq!(p.len(), 2, "{default}: {p:?}");
            assert!(
                p[1].description.contains(
                    "foreign-key column(s) of dbo.kind that reference dbo.status and that this \
                     plan writes to a default the probe cannot evaluate, which may be row `old` \
                     being deleted: status_code (row `9`); spell the value"
                ),
                "{}",
                p[1].description
            );
            assert!(
                p[1].sql
                    .contains("fk.referenced_object_id = OBJECT_ID(N'[dbo].[status]')")
                    && p[1]
                        .sql
                        .contains("fk.parent_object_id = OBJECT_ID(N'[dbo].[kind]')")
                    && p[1].sql.contains("c.name IN (N'status_code')"),
                "{}",
                p[1].sql
            );
        }
        assert_eq!(
            probes(&plan(vec![insert("(NULL)"), delete.clone()])).len(),
            1
        );

        // An update to DEFAULT, the same way.
        let update = Change::UpdateRow {
            unchanged: Default::default(),
            types: Default::default(),
            after_types: Default::default(),
            table: tname("dbo.kind"),
            key_column: "id".into(),
            key: RowKey::from("7"),
            columns: [(
                "status_code".to_owned(),
                (
                    Cell::Value(pbps_model::Value::Text("new".into())),
                    Cell::Default("('old')".into()),
                ),
            )]
            .into_iter()
            .collect(),
        };
        let sql = sql_of(&plan(vec![update, delete]));
        assert!(
            sql.contains("WHEN N'status_code' THEN N'(''old'')'"),
            "{sql}"
        );
        assert!(
            sql.contains(
                "ch.[id] = N''7'' AND NOT EXISTS (SELECT 1 FROM [dbo].[status] AS p WHERE p.[code] = @key'"
            ),
            "an update may already be counted: {sql}"
        );
    }

    /// A foreign key is a tuple: an update that sets two of its columns is
    /// compared as one row after the update, a key spanning a column the
    /// update sets to NULL keeps the row counted, and an insert that leaves
    /// a column of the key to NULL is not asked about (121).
    #[test]
    fn a_composite_foreign_key_is_matched_as_one_tuple() {
        use pbps_model::{Cell, Value};
        let update = |key: &str, cells: &[(&str, Value)]| Change::UpdateRow {
            unchanged: Default::default(),
            types: Default::default(),
            after_types: Default::default(),
            table: tname("dbo.pair_child"),
            key_column: "id".into(),
            key: RowKey::from(key),
            columns: cells
                .iter()
                .map(|(c, v)| {
                    (
                        (*c).to_owned(),
                        (Cell::Value(Value::Null), Cell::Value(v.clone())),
                    )
                })
                .collect(),
        };
        let cs = plan(vec![
            update("1", &[("grp", Value::Int(2)), ("sub", Value::Int(1))]),
            update("2", &[("grp", Value::Int(1)), ("sub", Value::Null)]),
            Change::InsertRow {
                table: tname("dbo.pair_child"),
                key_column: "id".into(),
                identity_key: false,
                key: RowKey::from("5"),
                row: [("grp".to_owned(), Value::Int(1))].into_iter().collect(),
                defaults: Default::default(),
                types: Default::default(),
            },
            Change::DeleteRow {
                table: tname("dbo.status"),
                key_column: "code".into(),
                key: RowKey::from("old"),
                cause: pbps_model::change::DeleteCause::Undeclared,
            },
        ]);
        let p = probes(&cs);
        assert_eq!(p.len(), 1, "{p:?}");
        let sql = &p[0].sql;
        // Both cells in one tuple, for a key spanning either column.
        assert!(
            sql.contains(
                "CASE c.name WHEN N'grp' THEN N'N''2''' WHEN N'sub' THEN N'N''1''' \
                 ELSE N'ch.' + QUOTENAME(c.name) END"
            ),
            "{sql}"
        );
        assert!(
            sql.contains("AND c.name IN (N'grp', N'sub')) THEN N' AND NOT (ch.[id] = N''1''"),
            "{sql}"
        );
        // The NULL is not compared: a key spanning `sub` counts row 2 as it
        // is, one spanning `grp` alone compares it.
        assert!(
            sql.contains(
                "AND c.name IN (N'sub')) THEN N'' WHEN EXISTS (SELECT 1 FROM sys.foreign_key_columns"
            ),
            "{sql}"
        );
        assert!(
            sql.contains("AND c.name IN (N'grp')) THEN N' AND NOT (ch.[id] = N''2''"),
            "{sql}"
        );
        // The insert spells `id` and `grp`; a key reaching `sub` is not asked.
        assert!(
            sql.contains("AND c.name NOT IN (N'grp', N'id')) THEN N'' ELSE"),
            "{sql}"
        );
        assert!(
            sql.contains("CASE c.name WHEN N'grp' THEN N'N''1''' WHEN N'id' THEN N'N''5'''"),
            "{sql}"
        );
    }

    /// The live test's own shape: the child moves to a new parent in the same
    /// plan, before the delete. Counting it would refuse the one plan the
    /// ordering was designed to make acceptable.
    #[test]
    fn a_child_row_the_plan_moves_is_not_counted_against_the_delete() {
        let cs = plan(vec![
            Change::UpdateRow {
                unchanged: Default::default(),
                types: Default::default(),
                after_types: Default::default(),
                table: tname("dbo.kind"),
                key_column: "id".into(),
                key: RowKey::from("7"),
                columns: [(
                    "status_code".to_owned(),
                    (
                        pbps_model::Cell::Value(pbps_model::Value::Text("old".into())),
                        pbps_model::Cell::Value(pbps_model::Value::Text("new".into())),
                    ),
                )]
                .into_iter()
                .collect(),
            },
            Change::DeleteRow {
                table: tname("dbo.status"),
                key_column: "code".into(),
                key: RowKey::from("old"),
                cause: pbps_model::change::DeleteCause::Undeclared,
            },
        ]);
        let p = probes(&cs);
        assert_eq!(p.len(), 1, "{p:?}");
        let sql = &p[0].sql;
        // Inside the dynamic text, so the quotes are doubled — and only for
        // a foreign key spanning the column the update sets; any other
        // foreign key from that table still counts the row.
        assert!(
            sql.contains(
                "WHEN s.name = N'dbo' AND t.name = N'kind' THEN CASE WHEN EXISTS (SELECT 1 FROM \
                 sys.foreign_key_columns fkc"
            ),
            "{sql}"
        );
        // The row is left out only if the engine says its tuple after the
        // update — `new` where the update sets it, the stored cell elsewhere
        // — is not the deleted row's, looked up through the parent table.
        assert!(
            sql.contains(
                "AND c.name IN (N'status_code')) THEN N' AND NOT (ch.[id] = N''7'' AND NOT EXISTS \
                 (SELECT 1 FROM [dbo].[status] AS p WHERE p.[code] = @key' + (SELECT STRING_AGG("
            ),
            "{sql}"
        );
        assert!(
            sql.contains(
                "CASE c.name WHEN N'status_code' THEN N'N''new''' ELSE N'ch.' + QUOTENAME(c.name) END"
            ),
            "{sql}"
        );
        assert!(sql.contains("ELSE N'' END"), "{sql}");
    }

    /// An update that sets some other column leaves the row pointing where
    /// it was. Excluding it anyway let `ON DELETE CASCADE` remove the row
    /// silently: the probe said zero, the engine cascaded.
    #[test]
    fn a_child_row_updated_elsewhere_is_still_counted_against_the_delete() {
        let cs = plan(vec![
            Change::UpdateRow {
                unchanged: Default::default(),
                types: Default::default(),
                after_types: Default::default(),
                table: tname("dbo.kind"),
                key_column: "id".into(),
                key: RowKey::from("7"),
                columns: [(
                    "note".to_owned(),
                    (
                        pbps_model::Cell::Value(pbps_model::Value::Null),
                        pbps_model::Cell::Value(pbps_model::Value::Text("x".into())),
                    ),
                )]
                .into_iter()
                .collect(),
            },
            Change::DeleteRow {
                table: tname("dbo.kind"),
                key_column: "id".into(),
                key: RowKey::from("8"),
                cause: pbps_model::change::DeleteCause::Undeclared,
            },
            Change::DeleteRow {
                table: tname("dbo.status"),
                key_column: "code".into(),
                key: RowKey::from("old"),
                cause: pbps_model::change::DeleteCause::Undeclared,
            },
        ]);
        let p = probes(&cs);
        let sql = &p
            .iter()
            .find(|p| p.description.contains("row `old`"))
            .expect("the status delete carries a probe")
            .sql;
        // Row 8 is deleted: excluded for every key. Row 7 is excluded only
        // from a key spanning `note`, which none does.
        assert!(
            sql.contains(
                "THEN N' AND ch.[id] NOT IN (N''8'')' + CASE WHEN EXISTS (SELECT 1 FROM \
                 sys.foreign_key_columns fkc"
            ),
            "{sql}"
        );
        assert!(
            sql.contains("AND c.name IN (N'note')) THEN N' AND NOT (ch.[id] = N''7''"),
            "{sql}"
        );
        assert!(sql.contains("ELSE N'' END"), "{sql}");
    }

    /// The delete names the row's table as the plan does, after any rename in
    /// the same plan; the catalog still has the old name when the probe runs.
    #[test]
    fn a_row_delete_is_probed_under_the_table_name_the_database_still_has() {
        let cs = plan(vec![
            Change::RenameTable {
                uid: uid("t_aaaaaa"),
                from: tname("dbo.state"),
                to: tname("dbo.status"),
            },
            Change::DeleteRow {
                table: tname("dbo.status"),
                key_column: "code".into(),
                key: RowKey::from("old"),
                cause: pbps_model::change::DeleteCause::Undeclared,
            },
        ]);
        let p = probes(&cs);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(
            p[0].sql.contains("OBJECT_ID(N'[dbo].[state]')"),
            "{}",
            p[0].sql
        );
        assert!(p[0].description.contains("dbo.status"), "{:?}", p[0]);
    }

    /// A table this plan creates has no rows for anything to reference; the
    /// same rule as every other probe.
    #[test]
    fn a_row_delete_in_a_table_created_by_this_plan_is_not_probed() {
        let mut table = pbps_model::Table::default();
        table
            .columns
            .insert("code".into(), pbps_model::Column::new(ty("varchar(20)")));
        let cs = plan(vec![
            Change::CreateTable {
                uid: uid("t_aaaaaa"),
                name: tname("dbo.status"),
                table: Box::new(table),
            },
            Change::DeleteRow {
                table: tname("dbo.status"),
                key_column: "code".into(),
                key: RowKey::from("old"),
                cause: pbps_model::change::DeleteCause::Undeclared,
            },
        ]);
        assert!(probes(&cs).is_empty());
    }

    /// Every probe must be a counting query returning one column called `n`;
    /// the executor reads it positionally and a probe that returned rows
    /// instead of a count would be read as garbage.
    #[test]
    fn every_probe_counts() {
        let changes = plan(vec![
            Change::AlterColumnNullability {
                uid: uid("c_aaaaaa"),
                column: cref("dbo.customer.email"),
                ty: ty("nvarchar(255)"),
                to_nullable: false,
            },
            Change::AddCheck {
                table: tname("dbo.customer"),
                name: "ck".into(),
                constraint: CheckConstraint {
                    expression: "[amount] >= 0".into(),
                },
            },
            Change::AddUnique {
                table: tname("dbo.customer"),
                name: "uq".into(),
                constraint: UniqueConstraint {
                    columns: vec!["email".into()],
                },
            },
        ]);
        let all = probes(&changes);
        assert!(!all.is_empty());
        for probe in all {
            assert!(probe.sql.contains(" AS n"), "{probe:?}");
            assert!(probe.sql.trim_end().ends_with(';'), "{probe:?}");
            assert!(!probe.description.is_empty(), "{probe:?}");
        }
    }

    /// The bug this whole `AsStored` layer exists for: a plan that renames a
    /// column and then tightens it must probe the name the database still has.
    /// Built change-by-change, the probe asked about a column that did not
    /// exist yet, and the one check that mattered was silently skipped.
    #[test]
    fn a_probe_uses_the_name_the_database_still_has() {
        let cs = plan(vec![
            Change::RenameColumn {
                uid: uid("c_aaaaaa"),
                table: tname("dbo.customer"),
                from: "email".into(),
                to: "contact_email".into(),
            },
            Change::AlterColumnNullability {
                uid: uid("c_aaaaaa"),
                column: cref("dbo.customer.contact_email"),
                ty: ty("nvarchar(255)"),
                to_nullable: false,
            },
        ]);
        let p = probes(&cs);
        assert_eq!(p.len(), 1);
        assert!(p[0].sql.contains("[email] IS NULL"), "{:?}", p[0]);
        assert!(!p[0].sql.contains("contact_email"), "{:?}", p[0]);
        // The message names it as the plan does; the reader is looking at the
        // plan, not at the catalog.
        assert!(p[0].description.contains("contact_email"), "{:?}", p[0]);
    }

    #[test]
    fn a_renamed_table_is_probed_under_its_old_name() {
        let cs = plan(vec![
            Change::RenameTable {
                uid: uid("t_aaaaaa"),
                from: tname("dbo.client"),
                to: tname("dbo.customer"),
            },
            Change::AddCheck {
                table: tname("dbo.customer"),
                name: "ck".into(),
                constraint: CheckConstraint {
                    expression: "[amount] >= 0".into(),
                },
            },
        ]);
        let p = probes(&cs);
        assert_eq!(p.len(), 1);
        assert!(p[0].sql.contains("[dbo].[client]"), "{:?}", p[0]);
    }

    /// A table this plan creates is empty, so nothing in it can violate
    /// anything — and probing it would only produce "invalid object name",
    /// which reads like a failure rather than the non-question it is.
    #[test]
    fn a_table_created_by_this_plan_is_not_probed() {
        let mut table = pbps_model::Table::default();
        table
            .columns
            .insert("id".into(), pbps_model::Column::new(ty("int")));
        let cs = plan(vec![
            Change::CreateTable {
                uid: uid("t_aaaaaa"),
                name: tname("dbo.brand_new"),
                table: Box::new(table),
            },
            Change::AddUnique {
                table: tname("dbo.brand_new"),
                name: "uq".into(),
                constraint: UniqueConstraint {
                    columns: vec!["id".into()],
                },
            },
        ]);
        assert!(probes(&cs).is_empty());
    }
}
