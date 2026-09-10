//! What a plan is asked about a live database before its first statement runs
//! — the reference-data half (ADR-0004).
//!
//! Only the pre-delete probe is here, with the refusal DECISIONS 124 pairs
//! with it. The rest of this dialect's probes — the not-null backfill, the
//! unique duplicate, the foreign-key orphan, the type change — arrive with
//! Phase 5 step 9, which is where `pbps-mssql`'s `preflight.rs` and
//! `impact.rs` have their counterparts. This one is here because a
//! `DeleteRow` cannot be emitted without it: the delete's own guard and the
//! probe are the same count, and the guard is part of the statement. Being the
//! same count is a claim to keep true: whatever makes one of them refuse has
//! to make the other refuse, or the refusal arrives after a staged apply has
//! committed everything ahead of the delete (DECISIONS 333).
//!
//! # Why the referencing tables are found at run time
//!
//! A probe is built from the plan and nothing else, and the plan does not know
//! which tables reference this one — nor should it trust the declarations to
//! say: a foreign key someone added by hand is exactly the one that will refuse
//! the delete. So the statement asks `pg_constraint` for every key that
//! references the parent, builds one `count(*)` per referencing table, and runs
//! the result through [`query_to_xml`](https://www.postgresql.org/docs/18/functions-xml.html).
//!
//! **That function is this engine's only way to run generated SQL from inside a
//! `SELECT`**, and a probe has to be one `SELECT`: [`pbps_dialect::Probe`]
//! promises "exactly one row with one integer column", and this driver's
//! `query` sends one statement through the extended protocol. SQL Server's
//! counterpart can say `DECLARE …; EXEC sp_executesql …; SELECT @n;` because
//! its driver hands back the last result set of a batch; here a `DO` block
//! returns nothing at all (DECISIONS 325).
//!
//! Nothing user-written reaches the generated text: the table and column names
//! come from the catalog through `quote_ident`, and the key is rendered by
//! [`crate::emit::value_literal`], which is setting-independent by
//! construction.
//!
//! # `NOT VALID` is not `NOCHECK` (ADR-0013 §1)
//!
//! The SQL Server probe one crate away skips a foreign key whose
//! `is_disabled` flag is set, because `NOCHECK CONSTRAINT` leaves the key in
//! the catalog and stops the engine enforcing it — counting those children
//! refused a delete the engine would have allowed (DECISIONS 144).
//!
//! `pg_constraint.convalidated` looks exactly like that flag and **measured, it
//! means the opposite**:
//!
//! ```text
//! ALTER TABLE r.child ADD CONSTRAINT fk_child
//!   FOREIGN KEY (pid) REFERENCES r.parent(id) NOT VALID;   -- convalidated = f
//!
//! INSERT INTO r.child VALUES (…, 998);
//!   ERROR:  insert or update on table "child" violates foreign key constraint "fk_child"
//! DELETE FROM r.parent WHERE id = 5;
//!   ERROR:  update or delete on table "parent" violates foreign key constraint "fk_child"
//! ```
//!
//! `NOT VALID` means "the rows that were already here were not checked". New
//! rows are checked and the delete action is enforced in full. **So this probe
//! counts every foreign key, validated or not, and `convalidated` is never
//! consulted.** The note is here rather than left to be rediscovered because
//! the code that looks like the pattern to copy is one crate away and reads its
//! own flag for the opposite reason (DECISIONS 320).
//!
//! # Why `ON DELETE CASCADE` counts too
//!
//! The engine would not refuse such a delete — it would take the child rows
//! with it, silently. A reference row's delete cascading into an application
//! table is the disaster the `data-delete` gate exists for, so those rows are
//! counted and the apply stops the same way.

use std::collections::{BTreeMap, BTreeSet};

use pbps_dialect::{DialectError, Probe};
use pbps_model::{Cell, Change, ChangeSet, ColumnRef, RowKey, TableName, Value};

use crate::emit::{qualified, value_literal};
use crate::quote;

/// The columns of the foreign key the generated statement is being built for,
/// with their names on the child side (`c`) and the parent side (`rc`).
///
/// `generate_subscripts` and array subscripts, not `unnest(conkey, confkey)`:
/// the two-array form of `unnest` is *grammar*, legal only in a `FROM` clause
/// and impossible to schema-qualify — **measured**, `pg_catalog.unnest(smallint[],
/// smallint[]) does not exist`. Everything this file names is qualified,
/// because the read scope pins an empty `search_path` (`catalog::CANONICAL_PATH`)
/// and the apply runs under whatever the operator has.
const KEY_COLUMNS: &str = "FROM pg_catalog.generate_subscripts(con.conkey, 1) AS k(ord) \
     JOIN pg_catalog.pg_attribute c ON c.attrelid = con.conrelid AND c.attnum = con.conkey[k.ord] \
     JOIN pg_catalog.pg_attribute rc \
       ON rc.attrelid = con.confrelid AND rc.attnum = con.confkey[k.ord] \
     LEFT JOIN pg_catalog.pg_collation co ON co.oid = rc.attcollation \
     LEFT JOIN pg_catalog.pg_namespace cn ON cn.oid = co.collnamespace \
     JOIN pg_catalog.pg_operator o ON o.oid = con.conpfeqop[k.ord] \
     JOIN pg_catalog.pg_namespace opn ON opn.oid = o.oprnamespace \
     WHERE true";

/// The parent side of a column of the key, compared as the engine's own
/// referential check compares it: under the referenced column's collation,
/// spelled out, and through the operator the constraint records.
///
/// **Measured**: a child column collated differently from the column it
/// references is a key the engine accepts and enforces — the delete is
/// refused — while `p.code = ch.ref` between the two is refused outright,
/// `could not determine which collation to use for string hashing`. The
/// engine's own check compares under the referenced column's collation, and
/// an explicit `COLLATE` on that side is what makes the comparison legal.
/// The operator is `conpfeqop`'s, spelled by schema, so that the comparison
/// is the constraint's and not whatever `=` resolves to (DECISIONS 352).
const PARENT_SIDE: &str = "'p.' || pg_catalog.quote_ident(rc.attname) \
     || CASE WHEN rc.attcollation <> 0 \
             THEN ' COLLATE ' || pg_catalog.quote_ident(cn.nspname) || '.' \
                  || pg_catalog.quote_ident(co.collname) \
             ELSE '' END \
     || ' OPERATOR(' || pg_catalog.quote_ident(opn.nspname) || '.' || o.oprname || ') '";

/// The child's stored cell, as the child side of a column of the key.
const STORED: &str = "'ch.' || pg_catalog.quote_ident(c.attname)";

/// Where a planned key compares a stored parent column against a stored child
/// column, the parent side is followed by one of these — `\u{1}<n>\u{1}` —
/// standing for the referenced column's own collation, which only the catalog
/// can spell. The count over stored rows is assembled by the engine from a
/// string, and there the mark becomes `COLLATE <schema>.<collation>` read from
/// `pg_attribute`; a mark that reached a statement as text would be a syntax
/// error, so every probe is checked for one before it is returned
/// (DECISIONS 353).
const COLLATION_MARK: char = '\u{1}';

/// The key as a conjunction the engine assembles at run time: ` AND p.<rc>
/// [COLLATE …] OPERATOR(…) <child side>` for each of its columns, where
/// `child_side` is SQL over `c.attname` spelling what the child holds in that
/// column ([`PARENT_SIDE`]).
fn tuple(child_side: &str) -> String {
    format!(
        "(SELECT pg_catalog.string_agg(' AND ' || {PARENT_SIDE} \
         || ({child_side}), '' ORDER BY k.ord) {KEY_COLUMNS})"
    )
}

/// A row's tuple as the plan writes it: the value written in each column the
/// plan spells, the stored cell in any other. Spelled for the engine as a
/// `CASE` over the key's columns.
fn side(values: &BTreeMap<String, String>) -> String {
    let whens: Vec<String> = values
        .iter()
        .map(|(name, sql)| format!("WHEN {} THEN {}", value_literal(name), value_literal(sql)))
        .collect();
    format!("CASE c.attname::text {} ELSE {STORED} END", whens.join(" "))
}

/// Whether the key spans one of `columns`.
fn touches<'a>(columns: impl Iterator<Item = &'a String>) -> String {
    let list: Vec<String> = columns.map(|c| value_literal(c)).collect();
    if list.is_empty() {
        return "false".to_owned();
    }
    format!(
        "EXISTS (SELECT 1 {KEY_COLUMNS} AND c.attname::text IN ({}))",
        list.join(", ")
    )
}

/// Whether the key spans a column outside `columns`.
fn reaches_beyond<'a>(columns: impl Iterator<Item = &'a String>) -> String {
    let list: Vec<String> = columns.map(|c| value_literal(c)).collect();
    if list.is_empty() {
        return "true".to_owned();
    }
    format!(
        "EXISTS (SELECT 1 {KEY_COLUMNS} AND c.attname::text NOT IN ({}))",
        list.join(", ")
    )
}

/// `fragment` for a key the update moves a row along, nothing for one it does
/// not touch — and nothing, so the row stays counted, for a key it sets a cell
/// of to something the probe cannot compare.
fn guarded(
    uncomparable: &BTreeSet<String>,
    comparable: &BTreeMap<String, String>,
    fragment: &str,
) -> String {
    let mut arms = String::new();
    // **A NULL anywhere in the key decides the whole tuple**, so it is asked
    // first. `MATCH SIMPLE` is this engine's default: a key with a NULL in any
    // column references nothing, whatever the other columns hold — so a row
    // that writes NULL to one column of a key and an unevaluable default to
    // another is a row the probe *can* place, and the arm below that gives up
    // on the unevaluable column must not win first. It did, and the ordinary
    // update-then-delete plan was refused for it (DECISIONS 336).
    let nulls: Vec<&String> = comparable
        .iter()
        .filter(|(_, sql)| crate::rows::unwrapped(sql).eq_ignore_ascii_case("null"))
        .map(|(c, _)| c)
        .collect();
    if !nulls.is_empty() {
        arms.push_str(&format!(
            "WHEN {} THEN {fragment} ",
            touches(nulls.into_iter())
        ));
    }
    if !uncomparable.is_empty() {
        arms.push_str(&format!("WHEN {} THEN '' ", touches(uncomparable.iter())));
    }
    format!(
        "CASE {arms}WHEN {} THEN {fragment} ELSE '' END",
        touches(comparable.keys())
    )
}

/// The rows of one table a plan writes, as the pre-delete probe has to see
/// them: which are deleted outright, and what each update or insert leaves in
/// each column it touches.
///
/// Per row, not per column: a foreign key is a tuple, and the probe compares a
/// row's whole tuple against the parent row's (DECISIONS 121).
#[derive(Debug, Default)]
struct Moved {
    key_column: String,
    /// Deleted: gone whatever they referenced.
    deleted: BTreeSet<RowKey>,
    /// Updated row -> column set by its update -> the value it is set to, as
    /// the SQL the engine compares. `None` for a default that is not a
    /// literal, which no probe can compare; a NULL is written and compared
    /// like any other value (DECISIONS 329).
    updated: BTreeMap<RowKey, BTreeMap<String, Option<String>>>,
    /// Inserted row -> column -> the value it arrives with. `count(*)` over the
    /// child sees only what is stored, and a row arriving on the parent is
    /// either not in the table yet (an insert) or stored somewhere else (an
    /// update); both run before the deletes, so without this the probe passes
    /// and `ON DELETE CASCADE` then takes the row straight back out.
    inserted: BTreeMap<RowKey, BTreeMap<String, String>>,
    /// Column -> the rows this plan writes to a default no probe can evaluate.
    /// Which key of the parent such a default names is decided when it runs,
    /// so the arrival cannot be counted and the write is refused instead
    /// (DECISIONS 124).
    unprobeable: BTreeMap<String, BTreeSet<RowKey>>,
    /// Updated row -> the columns its update leaves alone at NULL. The
    /// statement holds the row to its unchanged declared cells before and
    /// after it runs (DECISIONS 136), so a NULL among them is a NULL the
    /// row's tuple holds after the update, as surely as one the update
    /// writes (DECISIONS 349).
    held_null: BTreeMap<RowKey, BTreeSet<String>>,
}

impl Moved {
    /// The columns this plan writes to a NULL in `row`, under the names the
    /// catalog has now.
    ///
    /// A NULL is a value the probe compares (DECISIONS 329), and it is the one
    /// value that makes a whole tuple reference nothing: `MATCH SIMPLE` is
    /// this engine's default, so a key with a NULL in any of its columns is
    /// not checked at all. Read by the refusal over unprobeable defaults,
    /// which has nothing to refuse when the tuple cannot match any parent row
    /// however the default evaluates (DECISIONS 334).
    fn nulls_of(
        &self,
        row: &RowKey,
        stored_name: &impl Fn(&str) -> Option<String>,
    ) -> BTreeSet<String> {
        // `NULL`, `(NULL)` and `(NULL::text)` all reach here — a cell written
        // NULL, and a column left to a default that is NULL — and they are one
        // value to the engine.
        let written_null = |sql: &str| crate::rows::unwrapped(sql).eq_ignore_ascii_case("null");
        let mut out = BTreeSet::new();
        if let Some(columns) = self.updated.get(row) {
            for (column, sql) in columns {
                if sql.as_deref().is_some_and(written_null)
                    && let Some(name) = stored_name(column)
                {
                    out.insert(name);
                }
            }
        }
        if let Some(columns) = self.inserted.get(row) {
            for (column, sql) in columns {
                if written_null(sql)
                    && let Some(name) = stored_name(column)
                {
                    out.insert(name);
                }
            }
        }
        if let Some(columns) = self.held_null.get(row) {
            out.extend(columns.iter().filter_map(|c| stored_name(c)));
        }
        out
    }
}

/// A foreign key this plan removes before its deletes run: the constraint,
/// under the table that holds it. `None` for the constraint means every key
/// the table holds, because the table itself is going.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Removed {
    table: TableName,
    constraint: Option<String>,
}

/// Translates the names a plan uses into the names the database still has, and
/// records the rows the plan itself moves.
///
/// Probes run before the first statement, so every object they mention has to
/// be named as the catalog knows it *now*. A plan that renames a table and then
/// deletes a row from it describes the table by its new name, and a probe built
/// from the change alone would ask about a table that does not exist yet.
#[derive(Debug, Default)]
struct AsStored {
    tables: BTreeMap<TableName, TableName>,
    columns: BTreeMap<ColumnRef, String>,
    created: BTreeSet<TableName>,
    moved: BTreeMap<TableName, Moved>,
    removed: BTreeSet<Removed>,
    /// Foreign keys this plan adds, which the catalog does not hold yet and
    /// the engine will validate after the deletes have run (DECISIONS 335).
    added: Vec<Added>,
    /// Columns this plan adds, each as it is added: what every stored row of
    /// the table holds in it once `ADD COLUMN` has run (DECISIONS 336, 339).
    columns_added: BTreeMap<ColumnRef, AddedColumn>,
    /// Columns this plan retypes, with the type they will have: the
    /// `ALTER COLUMN … TYPE` runs at rank 9, before the deletes, so a key
    /// spanning one compares converted values (DECISIONS 340).
    retyped: BTreeMap<ColumnRef, pbps_model::ColumnType>,
    /// The type of every column a row change of this plan carries a type
    /// for, as the plan leaves it: a literal the probe compares against
    /// another literal is compared through it (DECISIONS 341).
    column_types: BTreeMap<ColumnRef, pbps_model::ColumnType>,
}

/// A column this plan adds, as the pre-delete probe needs it.
#[derive(Debug)]
struct AddedColumn {
    default: Option<String>,
    /// The declared type, which a backfilled literal is compared through:
    /// two spellings of one `date` are one value to the engine and two
    /// strings to a comparison of unknown literals — measured,
    /// `'2026-01-02' = '01/02/2026'` is false and the same through
    /// `CAST(… AS date)` is true (DECISIONS 339).
    ty: pbps_model::ColumnType,
    /// `GENERATED … AS IDENTITY`: the engine assigns every stored row a value
    /// from the sequence during the `ADD COLUMN` — measured, `1`, `2`, … —
    /// which no probe can evaluate and which may be a key of the parent.
    identity: bool,
}

/// A foreign key this plan adds, in the plan's own names.
#[derive(Debug)]
struct Added {
    child: TableName,
    columns: Vec<String>,
    parent: TableName,
    referenced: Vec<String>,
}

/// A foreign key whose delete action will run in this session — the only
/// kind the delete meets.
///
/// **A foreign key is triggers, and a trigger can be off.** `ALTER TABLE
/// parent DISABLE TRIGGER ALL` leaves the `pg_constraint` row saying
/// validated and enforced and stops the triggers that implement it —
/// **measured on 18.6**, the parent row then deletes with its child sitting
/// there, and the same under `session_replication_role = replica`, where
/// `O`-mode triggers do not fire. It is the parent-side trigger for the
/// *delete* (`tgrelid = confrelid`, `tgtype & 8`) that decides: with only
/// the update trigger off the delete is still refused, and with the child's
/// own triggers off it is too. Introspection already leaves such a key out
/// of the model as one whose checks are not running, and the SQL Server
/// probe skips `is_disabled` keys; a count through one refused a delete the
/// engine takes (DECISIONS 344). A key this plan adds never reaches a catalog
/// read: `planned_key_probes` asks about it from the plan (345).
const DELETE_ACTION_FIRES: &str = "NOT EXISTS (SELECT 1 FROM pg_catalog.pg_trigger t      WHERE t.tgconstraint = con.oid AND t.tgrelid = con.confrelid AND (t.tgtype & 8) <> 0        AND NOT (t.tgenabled = 'A'              OR (t.tgenabled = 'O' AND pg_catalog.current_setting('session_replication_role') <> 'replica')              OR (t.tgenabled = 'R' AND pg_catalog.current_setting('session_replication_role') = 'replica')))";

/// The SQL the engine compares a written cell by: the literal, or `None` for
/// what it cannot compare before the write runs.
fn written(value: &Value) -> Option<String> {
    match value {
        Value::Text(t) => Some(value_literal(t)),
        Value::Int(i) => Some(value_literal(&i.to_string())),
        Value::Bool(b) => Some(value_literal(&b.to_string())),
        // **A NULL is written, not dropped.** A NULL foreign key references
        // nothing — `p.code = NULL` is UNKNOWN, measured, so the tuple never
        // matches the parent row and the exclusion this produces takes the
        // row out of the count. Mapping it to `None` put it among the values
        // the probe *cannot* compare, which made the whole updated row
        // uncomparable, generated no exclusion at all, and refused the
        // ordinary plan that unpicks a reference before deleting its parent
        // (DECISIONS 329).
        Value::Null => Some("NULL".to_owned()),
    }
}

/// A default the engine can compare without running anything: a literal, read
/// through the cast this engine welds on (`rows::is_constant`).
///
/// **`NULL` is one of them.** It was excluded here — "it references no row",
/// which is true and is the wrong conclusion, exactly as it was for
/// `Value::Null` in DECISIONS 329. `None` from this function means *the probe
/// cannot compare this*, so an update setting a foreign-key column to a
/// `DEFAULT` whose default is NULL was uncomparable, its stored reference
/// stayed in the count, and the ordinary update-then-delete plan was refused —
/// while the same update spelled `null:` in the declaration was allowed.
/// `p.col = (NULL)` is UNKNOWN, so the tuple matches nothing and the row
/// leaves the count, which is the right answer (DECISIONS 335).
fn constant_default(default: &str) -> Option<&str> {
    let d = default.trim();
    crate::rows::is_constant(d).then_some(d)
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
                // Both run before the deletes (`order_key`), so a child
                // counted through either would refuse a delete that will be
                // valid by then (DECISIONS 128).
                Change::AddForeignKey {
                    table, constraint, ..
                } => {
                    this.added.push(Added {
                        child: table.clone(),
                        columns: constraint.columns.clone(),
                        parent: constraint.references_table.clone(),
                        referenced: constraint.references_columns.clone(),
                    });
                }
                Change::DropForeignKey { table, name } => {
                    this.removed.insert(Removed {
                        table: table.clone(),
                        constraint: Some(name.clone()),
                    });
                }
                Change::DropTable { name, .. } => {
                    this.removed.insert(Removed {
                        table: name.clone(),
                        constraint: None,
                    });
                }
                Change::UpdateRow {
                    table,
                    key_column,
                    key,
                    columns,
                    unchanged,
                    types,
                    after_types,
                } => {
                    this.remember_types(table, types);
                    this.remember_types(table, after_types);
                    let moved = this.moved.entry(table.clone()).or_default();
                    moved.key_column = key_column.clone();
                    // A cell the update leaves alone at NULL — declared NULL,
                    // or left to a default that is NULL — is a NULL of the
                    // row's tuple after the update (DECISIONS 349).
                    let is_null =
                        |sql: &str| crate::rows::unwrapped(sql).eq_ignore_ascii_case("null");
                    for (column, cell) in unchanged {
                        let null = match cell {
                            Cell::Value(v) => written(v).as_deref().is_some_and(is_null),
                            Cell::Default(d) => constant_default(d).is_some_and(is_null),
                        };
                        if null {
                            moved
                                .held_null
                                .entry(key.clone())
                                .or_default()
                                .insert(column.clone());
                        }
                    }
                    let updated = moved.updated.entry(key.clone()).or_default();
                    for (column, (_, after)) in columns {
                        let sql = match after {
                            Cell::Value(v) => written(v),
                            Cell::Default(d) => {
                                let constant = constant_default(d);
                                // A default that is not a literal and is not
                                // NULL may evaluate to the deleted row's key,
                                // and nothing can say so before it runs
                                // (DECISIONS 124).
                                if constant.is_none() {
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
                    types,
                    ..
                } => {
                    this.remember_types(table, types);
                    let moved = this.moved.entry(table.clone()).or_default();
                    moved.key_column = key_column.clone();
                    let inserted = moved.inserted.entry(key.clone()).or_default();
                    // The key is written like any other column, and may be one
                    // column of a composite foreign key.
                    inserted.insert(key_column.clone(), value_literal(key.as_str()));
                    for (column, default) in defaults {
                        if let Some(expr) = constant_default(default) {
                            inserted.insert(column.clone(), format!("({expr})"));
                        } else {
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
                    // A column the row omits and the table gives no default
                    // is inserted at NULL, and the plan carries every such
                    // column in `types` (DECISIONS 136). A NULL is a value
                    // the probe compares, and the one that makes a whole
                    // tuple reference nothing — left out, it was neither, and
                    // an insert leaving a sibling column of the key to an
                    // unevaluable default was refused for a tuple the engine
                    // never checks (DECISIONS 337).
                    for column in types.keys() {
                        if !inserted.contains_key(column) && !defaults.contains_key(column) {
                            inserted.insert(column.clone(), "NULL".to_owned());
                        }
                    }
                }
                Change::DeleteRow {
                    table,
                    key_column,
                    key,
                    types,
                    after_types,
                    ..
                } => {
                    this.remember_types(table, types);
                    this.remember_types(table, after_types);
                    let moved = this.moved.entry(table.clone()).or_default();
                    moved.key_column = key_column.clone();
                    moved.deleted.insert(key.clone());
                }
                Change::AddColumn {
                    table,
                    name,
                    column,
                    ..
                } => {
                    this.columns_added.insert(
                        table.column(name),
                        AddedColumn {
                            default: column.default.clone(),
                            ty: column.ty.clone(),
                            identity: column.identity.is_some(),
                        },
                    );
                }
                Change::AlterColumnType { column, to, .. } => {
                    this.retyped.insert(column.clone(), to.clone());
                }
                // Exhaustive rather than `_`: a change added later that moves a
                // name has to be reflected here, or every probe downstream of
                // it would quietly query the wrong object.
                Change::DropColumn { .. }
                | Change::AlterColumnNullability { .. }
                | Change::AlterColumnDefault { .. }
                | Change::SetColumnDeprecated { .. }
                | Change::SetPrimaryKey { .. }
                | Change::AddUnique { .. }
                | Change::DropUnique { .. }
                | Change::AddCheck { .. }
                | Change::DropCheck { .. }
                | Change::AddIndex { .. }
                | Change::DropIndex { .. }
                | Change::CreateModule { .. }
                | Change::AlterModule { .. }
                | Change::DropModule { .. }
                | Change::SetDataMode { .. }
                | Change::CreateRole { .. }
                | Change::DropRole { .. }
                | Change::RenameRole { .. }
                | Change::Grant { .. }
                | Change::Revoke { .. } => {}
            }
        }
        this
    }

    /// The name the catalog has for a table this plan names, or `None` where
    /// the plan creates it — a table that does not exist yet holds no rows and
    /// carries no foreign key, and probing it would only produce an error
    /// about a missing relation.
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

    fn column(&self, r: &ColumnRef) -> Option<ColumnRef> {
        let table = self.table(&r.table)?;
        Some(ColumnRef::new(
            table,
            self.columns
                .get(r)
                .cloned()
                .unwrap_or_else(|| r.name.clone()),
        ))
    }
}

/// The parts of one "rows referencing this parent row" statement that differ
/// between the probe and the delete's own guard.
struct Referencing<'a> {
    /// The parent table, quoted and qualified.
    parent: &'a str,
    /// `SELECT 1 FROM <parent> AS p WHERE p.<key column> = <key>`.
    parent_row: &'a str,
    /// Catalog filters that leave keys out of the read entirely.
    gone: &'a str,
    /// Per-child-table SQL excluding rows the plan itself moves, or `''`.
    exclusion: &'a str,
    /// Per-child-table SQL adding rows the plan puts onto the parent, or `''`.
    arrival: &'a str,
}

/// Counts the rows of every table with a foreign key into the parent whose key
/// tuple is the deleted row's — as one scalar expression.
///
/// **The per-child statement is `SELECT (SELECT count(*) …) <arrivals> AS n`,
/// and the parentheses are load-bearing.** The arrivals are terms *added to*
/// the count — a row this plan puts onto the parent is not in the child's
/// stored rows yet, so no `WHERE` clause can reach it — and appended to the
/// count's own `WHERE` they are a syntax error. The first version of this
/// wrote them there, and no test saw it: every live probe until then had a
/// plan that moved no child row, and the unit test that does inspects the SQL
/// without running it.
///
/// `convalidated` is deliberately not in the `WHERE`: see this module's own
/// documentation for why the flag that looks like SQL Server's means the
/// opposite of it.
fn counting_expression(r: &Referencing<'_>) -> String {
    let Referencing {
        parent,
        parent_row,
        gone,
        exclusion,
        arrival,
    } = r;
    format!(
        "LEAST(COALESCE((SELECT pg_catalog.sum(\n           \
           (pg_catalog.xpath('/row/n/text()',\n             \
             pg_catalog.query_to_xml(x.stmt, false, true, '')))[1]::text::bigint)\n    \
         FROM (SELECT 'SELECT (SELECT count(*) FROM '\n                 \
              || CASE WHEN cl.relkind = 'p' THEN '' ELSE 'ONLY ' END\n                 \
              || pg_catalog.quote_ident(ns.nspname)\n                 \
              || '.' || pg_catalog.quote_ident(cl.relname) || {}\n                 \
              || {} || ')' || {exclusion} || ')' || {arrival} || ' AS n' AS stmt\n            \
              FROM pg_catalog.pg_constraint con\n            \
              JOIN pg_catalog.pg_class cl ON cl.oid = con.conrelid\n            \
              JOIN pg_catalog.pg_namespace ns ON ns.oid = cl.relnamespace\n           \
             WHERE con.contype = 'f'\n                 \
               AND con.conparentid = 0\n                 \
               AND {DELETE_ACTION_FIRES}\n                 \
               AND con.confrelid = pg_catalog.to_regclass({})\n                 \
               {gone}) AS x), 0), 2147483647)::int",
        value_literal(&format!(" AS ch WHERE EXISTS ({parent_row}")),
        tuple(STORED),
        value_literal(parent),
    )
}

/// The keys this plan takes away before its deletes run, as catalog filters.
///
/// They are left out of the read altogether: by the time the delete runs the
/// constraint is gone, and a child counted through it refuses a delete the
/// engine would accept. Named with its table, because two schemas may each
/// hold a constraint of the same name (DECISIONS 128).
///
/// Written for a query that has `con`, `cl` and `ns` in scope, which both
/// probes over `pg_constraint` do.
fn gone_keys(names: &AsStored) -> String {
    let mut gone = Vec::new();
    for removed in &names.removed {
        let table = names
            .table(&removed.table)
            .unwrap_or_else(|| removed.table.clone());
        let of_table = format!(
            "ns.nspname = {} AND cl.relname = {}",
            value_literal(&table.schema),
            value_literal(&table.name)
        );
        gone.push(match &removed.constraint {
            Some(constraint) => format!(
                "AND NOT ({of_table} AND con.conname = {})",
                value_literal(constraint)
            ),
            None => format!("AND NOT ({of_table})"),
        });
    }
    gone.join("\n                 ")
}

/// The rows in other tables that still point at a row about to be deleted
/// (ADR-0004).
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
    let parent = qualified(&stored.table)?;
    // The row being deleted, named by the column the *plan* keys it on. Every
    // fragment below that means "this parent row" says so this way, because a
    // foreign key may reference some other unique key of the same row, and its
    // referenced columns are then not the key column at all (DECISIONS 116).
    let parent_row = format!(
        "SELECT 1 FROM {parent} AS p WHERE p.{} = {}",
        quote(&stored.name)?,
        value_literal(key.as_str())
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
        // A column the database does not have yet cannot be in a foreign key it
        // has; one it names differently is asked about by that name.
        let stored_name = |column: &str| names.column(&child.column(column)).map(|r| r.name);
        let mut pieces = Vec::new();
        let mut terms = Vec::new();
        // Deleted rows are gone whatever they pointed at.
        if !moved.deleted.is_empty() {
            let list: Vec<String> = moved
                .deleted
                .iter()
                .map(|k| value_literal(k.as_str()))
                .collect();
            pieces.push(value_literal(&format!(
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
            // The row's tuple after the update: the value the update sets where
            // it sets one, the stored cell elsewhere. It is left out of a key's
            // count only when the engine says that tuple is not the deleted
            // row's — so `01` and `1` are one key — and counted for a key the
            // update sets a cell of to something the probe cannot compare,
            // which is the direction to be wrong in.
            let after = side(&comparable);
            let row = value_literal(row_key.as_str());
            let excluded = format!(
                "{} || {} || {}",
                value_literal(&format!(
                    " AND NOT ({key_sql} = {row} AND NOT EXISTS ({parent_row}"
                )),
                tuple(&after),
                value_literal("))")
            );
            pieces.push(guarded(&uncomparable, &comparable, &excluded));
            // And the same row arriving on the parent, which the count cannot
            // see: the row exists, so it may already be inside the count, and
            // only one that is *not* on the parent now is arriving. A term
            // added to the count rather than a clause narrowing it, so a whole
            // parenthesised expression.
            let arrived = format!(
                "{} || {} || {} || {} || {}",
                value_literal(&format!(
                    " + (SELECT count(*) FROM {child_sql} AS ch WHERE {key_sql} = {row} AND \
                     NOT EXISTS ({parent_row}"
                )),
                tuple(STORED),
                value_literal(&format!(") AND EXISTS ({parent_row}")),
                tuple(&after),
                value_literal("))")
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
            // Nothing stored to double-count: the row is either arriving on the
            // deleted row or it is not. A key spanning a column the insert
            // leaves to NULL, or to a default that is not a literal, is not one
            // the probe can ask about (DECISIONS 117).
            let arrived = format!(
                "{} || {} || {}",
                value_literal(&format!(" + (CASE WHEN EXISTS ({parent_row}")),
                tuple(&side(&known)),
                value_literal(") THEN 1 ELSE 0 END)")
            );
            terms.push(format!(
                "CASE WHEN {} THEN '' ELSE {arrived} END",
                reaches_beyond(known.keys())
            ));
        }
        let exclusion = if pieces.is_empty() {
            "''".to_owned()
        } else {
            pieces.join(" || ")
        };
        exclusions.push(format!(
            "WHEN ns.nspname = {} AND cl.relname = {} THEN {exclusion}",
            value_literal(&stored_child.schema),
            value_literal(&stored_child.name),
        ));
        if !terms.is_empty() {
            arrivals.push(format!(
                "WHEN ns.nspname = {} AND cl.relname = {} THEN {}",
                value_literal(&stored_child.schema),
                value_literal(&stored_child.name),
                terms.join(" || ")
            ));
        }
    }
    let exclusion = if exclusions.is_empty() {
        "''".to_owned()
    } else {
        format!("CASE {} ELSE '' END", exclusions.join(" "))
    };
    let arrival = if arrivals.is_empty() {
        "''".to_owned()
    } else {
        format!("CASE {} ELSE '' END", arrivals.join(" "))
    };
    let gone = gone_keys(names);

    Ok(Probe::new(
        format!(
            "rows in other tables that still reference {table} row `{key}`, which its delete \
             would orphan or cascade into"
        ),
        format!(
            "SELECT {} AS n;",
            counting_expression(&Referencing {
                parent: &parent,
                parent_row: &parent_row,
                gone: &gone,
                exclusion: &exclusion,
                arrival: &arrival,
            })
        ),
    ))
}

/// The guard a row delete carries: the same count, taken inside the delete's
/// own block behind a lock on the parent row that it holds until the
/// transaction ends, so that a child row committed between the preflight probe
/// and the delete cannot be cascaded away unseen (DECISIONS 129).
///
/// **The lock is the parent row's, not a table's, and that is the engine's own
/// mechanism rather than a choice.** PostgreSQL enforces a foreign key by
/// locking the parent row it points at — **measured**, an `INSERT` into a child
/// waiting on our lock reports the statement it was blocked in:
///
/// ```text
/// while another session holds SELECT … FROM parent WHERE id = 5 FOR UPDATE:
/// INSERT INTO child VALUES (99, 5);
///   ERROR: canceling statement due to lock timeout
///   CONTEXT: while locking tuple (0,3) in relation "parent"
///   SQL statement "SELECT 1 FROM ONLY "m4"."parent" x WHERE "id" = $1 FOR KEY SHARE OF x"
/// ```
///
/// So a row arriving on the parent waits for the delete instead of racing it,
/// and the lock is one row wide rather than SQL Server's range over the child
/// (DECISIONS 326).
///
/// It is not the probe: by the time this runs, every insert, update and child
/// delete of the plan has run (`order_key`), so *any* row still referencing the
/// parent is one the probe did not account for. The probe stays where it is —
/// it reports the number before anything runs, which is what a human approves;
/// this refuses what changed underneath it.
pub(crate) fn still_referenced(
    table: &TableName,
    key_column: &str,
    key: &RowKey,
) -> Result<String, DialectError> {
    let parent = qualified(table)?;
    let parent_row = format!(
        "SELECT 1 FROM {parent} AS p WHERE p.{} = {}",
        quote(key_column)?,
        value_literal(key.as_str())
    );
    Ok(format!(
        "PERFORM 1 FROM {parent} AS p WHERE p.{} = {} FOR UPDATE;\n\
         IF {} THEN\n    {}\nEND IF;\n\
         pbps_referencing := {};\n\
         IF pbps_referencing > 0 THEN\n    {}\nEND IF;",
        quote(key_column)?,
        value_literal(key.as_str()),
        // Before the count, because the count is what it invalidates.
        a_hidden_child(&parent),
        crate::emit::refuse(&format!(
            "{table} row `{key}` cannot be deleted safely: a table with a foreign key into \
             {table} has row-level security in force for this session, so the count of rows \
             referencing this one is filtered by a policy — and this engine's referential \
             actions are not. Measured, an `ON DELETE CASCADE` child the policy hides is \
             deleted anyway, which is the silent loss this guard exists to prevent. Nothing \
             was applied. Delete the row as a role the policy does not filter, or remove the \
             row-level security from the referencing table."
        )),
        counting_expression(&Referencing {
            parent: &parent,
            parent_row: &parent_row,
            // The plan's own moves belong to the probe, which reads the plan
            // whole; here the plan has already run, and a row referencing the
            // parent now is a row nobody accounted for.
            gone: "",
            // The catalog's keys only, as every read of them is (345). A key
            // this plan adds is not there yet when this runs either — but the
            // probe has already refused the plan for it before the first
            // statement, which is the only place a staged apply can still be
            // stopped; here it would be one more reason to abort after the
            // plan is half applied.
            // The one exception is the row this statement is about to delete.
            // A self-referencing foreign key makes it its own child, and it
            // still points at itself when the guard runs — the delete has not
            // happened yet. **Measured on 18.6**, the engine takes that delete
            // without complaint, because the one statement removes both sides
            // of the reference:
            //
            // ```text
            // INSERT INTO t VALUES (5, 5);      -- t.parent REFERENCES t.id
            // the guard's count before the delete:  1
            // DELETE FROM t WHERE id = 5;           succeeded
            // ```
            //
            // So the count refused a plan the engine accepts. Only this row,
            // and only on the parent's own table: another row pointing at it
            // through the same self-reference is a real child and is counted
            // (DECISIONS 330).
            exclusion: &self_reference(key_column, key)?,
            arrival: "''",
        }),
        crate::emit::refuse(&format!(
            "{table} row `{key}` is referenced by row(s) that arrived after this plan was \
             checked; the delete would orphan or cascade into them. Nothing was applied. \
             Plan again."
        ))
    ))
}

/// The row about to be deleted, left out of the count on its own table.
///
/// `con.conrelid = con.confrelid` is the self-reference: the count already
/// filters on `confrelid`, so a constraint whose referencing table is also the
/// referenced one is a foreign key from the parent into itself. On every other
/// table the exclusion is empty and every referencing row is counted.
fn self_reference(key_column: &str, key: &RowKey) -> Result<String, DialectError> {
    Ok(format!(
        "CASE WHEN con.conrelid = con.confrelid THEN {} ELSE '' END",
        value_literal(&format!(
            " AND ch.{} NOT IN ({})",
            quote(key_column)?,
            value_literal(key.as_str())
        ))
    ))
}

/// Whether any table with a foreign key into the parent would hide rows from
/// the count — the one thing that makes the count *incomplete* rather than
/// merely small.
///
/// **Referential actions bypass row-level security and this count does not.**
/// **Measured on 18.6**, with an `ON DELETE CASCADE` child under a policy the
/// deploying role does not satisfy:
///
/// ```text
/// as the deploying role:  SELECT count(*) FROM r2.child  ->  0
///                         DELETE FROM r2.parent WHERE code = 'old'  ->  succeeded
/// as the owner afterwards: children left  ->  0
/// ```
///
/// The row the guard exists to protect was cascaded away, unseen, by a delete
/// both the probe and the guard called safe. So the guard refuses instead.
///
/// `row_security_active`, not `relrowsecurity`: the second says the switch is
/// on, and the question is whether it applies *to this session*. **Measured**,
/// the same table answers `relrowsecurity = t` to both roles while
/// `row_security_active` is false for the owner — who bypasses the policy, and
/// whose count is therefore complete — and true for the deploying role. Reading
/// the switch would refuse a plan whose count is exact.
///
/// `con.conparentid = 0`, as the count and the probe filter: a partitioned
/// child holds one constraint row per partition beside its own (334), and
/// the count scans the partitioned relation — where the *partitions'*
/// policies do not apply. **Measured**, a role the leaf's policy hides every
/// row from counts the row through the parent, while `row_security_active`
/// is true for the leaf; asked of every row, this refused a delete whose
/// count was complete (DECISIONS 346).
fn a_hidden_child(parent: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM pg_catalog.pg_constraint con
                         JOIN pg_catalog.pg_class cl ON cl.oid = con.conrelid
                       WHERE con.contype = 'f'
                            AND con.conparentid = 0
                            AND con.confrelid = pg_catalog.to_regclass({})
                            AND {DELETE_ACTION_FIRES}
                            AND pg_catalog.row_security_active(cl.oid))",
        value_literal(parent)
    )
}

/// The keys this plan removes from `stored_child` before the deletes run,
/// under the names the catalog holds now.
///
/// Read by both probes over foreign keys: a constraint that will be gone by
/// the time the delete runs must count nothing, or the probe refuses a delete
/// the engine would accept (DECISIONS 128).
enum Removal {
    /// The table itself: every key it holds goes with it.
    Table,
    /// The named constraints, and nothing else.
    Keys(Vec<String>),
}

impl AsStored {
    /// The stored children whose key into `parent` this plan adds, under the
    /// names the catalog holds now (DECISIONS 336, 345).
    fn planned_children(&self, parent: &TableName) -> Vec<TableName> {
        self.added
            .iter()
            .filter(|a| a.parent == *parent)
            .filter_map(|a| self.table(&a.child))
            .collect()
    }

    /// The stored columns of `child` the planned-key count reads: the key's
    /// stored columns, and the row key where the plan moves rows of it
    /// (DECISIONS 340).
    fn planned_reads(&self, parent: &TableName, stored_child: &TableName) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for a in self.added.iter().filter(|a| a.parent == *parent) {
            if self.table(&a.child).as_ref() != Some(stored_child) {
                continue;
            }
            for c in &a.columns {
                if self.columns_added.contains_key(&a.child.column(c)) {
                    continue;
                }
                if let Some(r) = self.column(&a.child.column(c)) {
                    out.insert(r.name);
                }
            }
            if let Some(key) = self.row_key_read(&a.child) {
                out.insert(key);
            }
        }
        out
    }

    /// The row key column of `child`, under the catalog's name, where the
    /// count reads it: the plan deletes or updates rows of the child, and the
    /// exclusions name them by key.
    fn row_key_read(&self, child: &TableName) -> Option<String> {
        let m = self.moved.get(child)?;
        if m.deleted.is_empty() && m.updated.is_empty() {
            return None;
        }
        self.column(&child.column(&m.key_column)).map(|r| r.name)
    }

    fn remember_types(
        &mut self,
        table: &TableName,
        types: &BTreeMap<String, pbps_model::ColumnType>,
    ) {
        for (column, ty) in types {
            self.column_types.insert(table.column(column), ty.clone());
        }
    }

    /// The type `column` has once this plan has run, where the plan says:
    /// the type it retypes it to, the type it adds it with, or the type a row
    /// change carries for it. `None` for a column no change of this plan
    /// describes — the row key among them, which no row change types.
    fn final_type(&self, column: &ColumnRef) -> Option<pbps_model::ColumnType> {
        self.retyped
            .get(column)
            .or_else(|| self.columns_added.get(column).map(|a| &a.ty))
            .or_else(|| self.column_types.get(column))
            .cloned()
    }

    /// A literal the plan writes to `column`, as the probe compares it to
    /// another literal: through the column's type where the plan knows it,
    /// because two unknown literals compare as text and `'2026-01-02'` and
    /// `'01/02/2026'` are one `date` (DECISIONS 339, 341). A NULL stays bare,
    /// so that it reads as one; against a column reference the engine
    /// coerces the literal itself, and the cast changes nothing.
    fn typed(&self, column: &ColumnRef, literal: String) -> String {
        if crate::rows::unwrapped(&literal).eq_ignore_ascii_case("null") {
            return literal;
        }
        match self.final_type(column) {
            Some(ty) => format!(
                "CAST({literal} AS {})",
                crate::types::normalize(&ty).unwrap_or_else(|_| ty.clone())
            ),
            None => literal,
        }
    }

    /// `expr`, converted to the type this plan gives `column` — or as it is,
    /// where the plan leaves the type alone.
    fn converted(&self, column: &ColumnRef, expr: String) -> String {
        match self.retyped.get(column) {
            Some(to) => format!(
                "CAST({expr} AS {})",
                crate::types::normalize(to).unwrap_or_else(|_| to.clone())
            ),
            None => expr,
        }
    }

    fn removed_from(&self, stored_child: &TableName) -> Removal {
        let mut keys = Vec::new();
        for removed in &self.removed {
            let table = self
                .table(&removed.table)
                .unwrap_or_else(|| removed.table.clone());
            if table != *stored_child {
                continue;
            }
            match &removed.constraint {
                None => return Removal::Table,
                Some(constraint) => keys.push(constraint.clone()),
            }
        }
        Removal::Keys(keys)
    }
}

/// The refusal that goes with the count: the tables whose referencing rows
/// this session cannot count.
///
/// The probe and the delete's own guard are the same count, and the guard
/// refuses when row-level security makes that count incomplete (DECISIONS
/// 329). Only the guard did. A probe that answers `0` because a policy filtered
/// it away reports "nothing references this row" for "I cannot see what
/// references this row" — the difference `CLAUDE.md` is about — and a human
/// approves the plan on it. In a staged apply every insert and update of that
/// plan then commits for good before the guard is reached, so the deployment
/// is half applied and the refusal arrives too late to be the refusal.
///
/// A second probe rather than a term in the count, for the reason DECISIONS
/// 124 gives: the count is a number a reader is meant to understand, and
/// inflating it to force a refusal would make it a number about something
/// else. This one counts referencing *tables*, not rows.
///
/// `row_security_active`, not `relrowsecurity`: the question is whether the
/// policy applies to this session, and measured, the owner — whose count is
/// complete — reads `relrowsecurity = t` and `row_security_active = f`
/// (DECISIONS 333).
///
/// **And a table the session cannot read at all.** Measured, a role with
/// `DELETE` on the parent and no `SELECT` on the child gets `permission
/// denied` from the count — which the probe runner reports as *unchecked*,
/// and `apply` proceeds — while the engine's own key still sees the child and
/// refuses the delete. Unreadable was read as "nothing there", in the one
/// place this project has a rule about it; `has_table_privilege` asks the
/// catalog first (DECISIONS 335).
fn hidden_children_probe(
    table: &TableName,
    key: &RowKey,
    stored: &ColumnRef,
    names: &AsStored,
) -> Result<Probe, DialectError> {
    let parent = qualified(&stored.table)?;
    // A child whose key this plan adds has no constraint row yet, and is
    // asked about by name (DECISIONS 336, 345).
    let mut planned = String::new();
    for child in names.planned_children(table) {
        let reads: Vec<String> = names
            .planned_reads(table, &child)
            .iter()
            .map(|c| {
                format!(
                    "pg_catalog.has_column_privilege(cl.oid, {}, 'SELECT')",
                    value_literal(c)
                )
            })
            .collect();
        planned.push_str(&format!(
            "\n   + (SELECT count(*) FROM pg_catalog.pg_class cl\n      \
               WHERE cl.oid = pg_catalog.to_regclass({})\n        \
                 AND (pg_catalog.row_security_active(cl.oid)\n              \
                      OR NOT (pg_catalog.has_schema_privilege(cl.relnamespace, 'USAGE')\n                              \
                              AND (pg_catalog.has_table_privilege(cl.oid, 'SELECT') OR ({})))))",
            value_literal(&qualified(&child)?),
            if reads.is_empty() {
                "true".to_owned()
            } else {
                reads.join(" AND ")
            }
        ));
    }
    // The columns the count reads, beyond the key's own: the row key of a
    // child whose rows this plan deletes or updates.
    let mut row_keys = Vec::new();
    for child in names.moved.keys() {
        let (Some(stored_child), Some(key)) = (names.table(child), names.row_key_read(child))
        else {
            continue;
        };
        row_keys.push(format!(
            "WHEN ns.nspname = {} AND cl.relname = {} \
             THEN pg_catalog.has_column_privilege(cl.oid, {}, 'SELECT')",
            value_literal(&stored_child.schema),
            value_literal(&stored_child.name),
            value_literal(&key)
        ));
    }
    let row_keys = if row_keys.is_empty() {
        "true".to_owned()
    } else {
        format!("CASE {} ELSE true END", row_keys.join(" "))
    };
    // The parent's own side: every count reads the deleted row by its key
    // column and compares the referenced columns of every key — the stored
    // keys' from the catalog, a planned key's by name — and `DELETE` grants
    // none of those reads. **Measured**: with `DELETE` and `SELECT` on the
    // key column alone, the count fails `permission denied for table`, the
    // delete runs, and `ON DELETE CASCADE` takes the child the count never
    // saw (DECISIONS 354).
    let mut parent_reads = vec![format!(
        "pg_catalog.has_column_privilege(pc.oid, {}, 'SELECT')",
        value_literal(&stored.name)
    )];
    for a in &names.added {
        if a.parent != *table {
            continue;
        }
        for r in &a.referenced {
            if let Some(stored_r) = names.column(&a.parent.column(r)) {
                parent_reads.push(format!(
                    "pg_catalog.has_column_privilege(pc.oid, {}, 'SELECT')",
                    value_literal(&stored_r.name)
                ));
            }
        }
    }
    let parent_side = format!(
        "\n   + (SELECT count(*) FROM pg_catalog.pg_class pc\n      \
           WHERE pc.oid = pg_catalog.to_regclass({})\n        \
             AND (pg_catalog.row_security_active(pc.oid)\n              \
                  OR NOT (pg_catalog.has_schema_privilege(pc.relnamespace, 'USAGE')\n                          \
                          AND (pg_catalog.has_table_privilege(pc.oid, 'SELECT')\n                               \
                               OR ({}\n                                   \
                                   AND NOT EXISTS (SELECT 1\n                                     \
                                     FROM pg_catalog.pg_constraint con\n                                     \
                                     JOIN pg_catalog.pg_class cl ON cl.oid = con.conrelid\n                                     \
                                     JOIN pg_catalog.pg_namespace ns ON ns.oid = cl.relnamespace,\n                                     \
                                     pg_catalog.generate_subscripts(con.confkey, 1) AS k(ord)\n                                     \
                                     WHERE con.contype = 'f' AND con.conparentid = 0\n                                       \
                                       AND {DELETE_ACTION_FIRES}\n                                       \
                                       AND con.confrelid = pc.oid\n                                       \
                                       {}\n                                       \
                                       AND NOT pg_catalog.has_column_privilege(pc.oid, con.confkey[k.ord], 'SELECT')))))))",
        value_literal(&parent),
        parent_reads.join("\n                                   AND "),
        gone_keys(names)
    );
    Ok(Probe::new(
        format!(
            "tables with a foreign key into {table} whose rows this session cannot count — a \
             policy filters them, or the session cannot read the table at all — or {table} \
             itself, whose key and referenced columns every count reads, so the count of what \
             references row `{key}` is not the count, while this engine's referential actions \
             see every row; delete as a role that can read every referencing table unfiltered \
             and {table}'s referenced columns, or take the policy off"
        ),
        format!(
            "SELECT LEAST(((SELECT count(*)\n  \
               FROM {} con\n  \
               JOIN pg_catalog.pg_class cl ON cl.oid = con.conrelid\n  \
               JOIN pg_catalog.pg_namespace ns ON ns.oid = cl.relnamespace\n \
              WHERE con.contype = 'f'\n   \
                AND con.conparentid = 0\n   \
                AND {DELETE_ACTION_FIRES}\n   \
                AND con.confrelid = pg_catalog.to_regclass({})\n   \
                AND (pg_catalog.row_security_active(cl.oid)\n   \
                     OR NOT (pg_catalog.has_schema_privilege(cl.relnamespace, 'USAGE')\n   \
                             AND (pg_catalog.has_table_privilege(cl.oid, 'SELECT')\n   \
                                  OR (NOT EXISTS (SELECT 1 {KEY_COLUMNS}\n   \
                                        AND NOT pg_catalog.has_column_privilege(con.conrelid, c.attnum, 'SELECT'))\n   \
                                      AND {row_keys}))))\n                 \
                {}){}{parent_side}), 2147483647)::int AS n;",
            "pg_catalog.pg_constraint",
            value_literal(&parent),
            gone_keys(names),
            planned
        ),
    ))
}

/// The refusal DECISIONS 124 asks for, on this engine: a write to a default no
/// probe can evaluate, on a column a live foreign key into the deleted row's
/// table spans.
///
/// A default that is not a literal is evaluated when it runs, so which key of
/// the parent it names cannot be known before the plan starts. Counting it as
/// absent lets the write pass preflight, commit — in a staged apply it commits
/// for good — and the delete's own guard then finds the reference and aborts,
/// leaving a deployment half applied. Refusing it before the first statement
/// costs the operator one spelled value.
///
/// **A count, not a `RAISE`.** A probe that errors reads as "unchecked" to
/// `apply`, which then proceeds; a probe that returns a number above zero is a
/// refusal `apply` acts on. The columns and the rows are in the description,
/// with the remedy, because that is all the operator gets to work from.
///
/// There is no `convalidated` filter, for the reason this module documents at
/// length: a `NOT VALID` key here still enforces the delete action in full,
/// unlike SQL Server's `NOCHECK`, so a key that looks unenforced is not one.
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
    let removed = match names.removed_from(&stored_child) {
        Removal::Table => return Ok(None),
        Removal::Keys(keys) => keys,
    };
    // Per row, because a foreign key is a tuple and this question is about a
    // tuple: an unprobeable value in one column of a key says nothing if
    // another column of the *same* key is written NULL by the *same* row
    // (DECISIONS 334).
    let stored_name = |column: &str| names.column(&child.column(column)).map(|r| r.name);
    let mut described = Vec::new();
    let mut arms = Vec::new();
    let mut rows: BTreeMap<&RowKey, BTreeSet<String>> = BTreeMap::new();
    for (column, keys) in &moved.unprobeable {
        let Some(name) = stored_name(column) else {
            continue;
        };
        for k in keys {
            rows.entry(k).or_default().insert(name.clone());
        }
        let listed: Vec<String> = keys.iter().map(|r| format!("`{r}`")).collect();
        described.push(format!(
            "{column} (row{} {})",
            if listed.len() == 1 { "" } else { "s" },
            listed.join(", ")
        ));
    }
    for (row, unprobeable) in &rows {
        let nulls = moved.nulls_of(row, &stored_name);
        arms.push(if nulls.is_empty() {
            format!("({})", touches(unprobeable.iter()))
        } else {
            // `MATCH SIMPLE`, this engine's default: measured, a tuple with a
            // NULL in any column references no row at all, and the parent
            // deletes with that child sitting there.
            format!(
                "({} AND NOT {})",
                touches(unprobeable.iter()),
                touches(nulls.iter())
            )
        });
    }
    // And per inserted row, a key reaching a column the insert neither
    // spells nor leaves to a default the plan carries: every other column
    // is in the insert's `types`, so that one is the engine's to assign — an
    // identity — and its value may be the deleted row's key. The catalog
    // says which key reaches it; the plan cannot (DECISIONS 343).
    for (row, set) in &moved.inserted {
        let mut recorded: BTreeSet<String> = set.keys().filter_map(|c| stored_name(c)).collect();
        for (column, keys) in &moved.unprobeable {
            if keys.contains(row)
                && let Some(name) = stored_name(column)
            {
                recorded.insert(name);
            }
        }
        let nulls = moved.nulls_of(row, &stored_name);
        let beyond = reaches_beyond(recorded.iter());
        arms.push(if nulls.is_empty() {
            format!("({beyond})")
        } else {
            format!("({beyond} AND NOT {})", touches(nulls.iter()))
        });
        described.push(format!(
            "a column row `{row}` leaves to the engine to assign, where the key reaches one"
        ));
    }
    if arms.is_empty() {
        return Ok(None);
    }
    let gone = if removed.is_empty() {
        String::new()
    } else {
        format!(
            "\n   AND con.conname NOT IN ({})",
            removed
                .iter()
                .map(|c| value_literal(c))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Ok(Some(Probe::new(
        format!(
            "foreign-key column(s) of {child} that reference {table} and that this plan writes to \
             a default the probe cannot evaluate, which may be row `{key}` being deleted: {}; \
             spell the value",
            described.join(", ")
        ),
        format!(
            "SELECT LEAST(count(*), 2147483647)::int AS n\n  \
               FROM {} con\n \
              WHERE con.contype = 'f'\n   \
                AND con.conparentid = 0\n   \
                AND {DELETE_ACTION_FIRES}\n   \
                AND con.confrelid = pg_catalog.to_regclass({})\n   \
                AND con.conrelid = pg_catalog.to_regclass({})\n   \
                AND ({}){gone};",
            "pg_catalog.pg_constraint",
            value_literal(&qualified(&stored.table)?),
            value_literal(&qualified(&stored_child)?),
            arms.join("\n                     OR "),
        ),
    )))
}

/// The count and the refusal for every key this plan adds, asked from the
/// plan alone (DECISIONS 345).
///
/// `DeleteRow` runs at rank 12 and `AddForeignKey` at 13 (`order_key`), so
/// the delete runs against a catalog that does not hold the key, and the
/// `ALTER` that follows validates every child row and fails on the one the
/// delete just orphaned — measured; in a staged apply the delete has
/// committed by then. The first answer (335) was a synthetic `pg_constraint`
/// row beside the stored ones, so that every rule for a catalog row would
/// reach it. Five rounds of review found what a catalog row cannot say — a
/// column this plan adds, one it retypes, a child it creates, a survivor the
/// plan leaves holding the deleted row's tuple — and each took this path
/// until every planned key did. What follows is the first of those.
///
/// **A column the database does not have holds a reference the moment it is
/// added.** `ADD COLUMN … DEFAULT 'old'` backfills every stored row with the
/// default — **measured on 18.6**, and in the plan's own order:
///
/// ```text
/// ALTER TABLE child ADD COLUMN parent text DEFAULT 'old';   -- rank 8
/// SELECT parent FROM child;                                  -- 'old', every row
/// DELETE FROM parent WHERE code = 'old';                     -- rank 12, succeeded
/// ALTER TABLE child ADD FOREIGN KEY (parent) REFERENCES parent(code);
///   ERROR 23503: Key (parent)=(old) is not present in table "parent"
/// ```
///
/// The synthetic row had no attnum to be built from, the probe counted
/// nothing, and the delete went through. So the question is asked from the
/// plan: every stored row of the
/// child holds the default in the new column and its stored cell in every
/// other column of the key, and the count is of the rows whose tuple, spelled
/// that way, is the deleted row's. `DEFAULT NULL`, or no default, is a NULL in
/// the tuple and references nothing (`MATCH SIMPLE`, DECISIONS 334) — the
/// engine says so, `p.code = (NULL)` being UNKNOWN, and no branch here repeats
/// it. The rows this plan itself moves are the same exclusions and arrivals
/// the catalog keys get, spelled statically because the key's columns are
/// known here (DECISIONS 336).
///
/// **And the referenced side is backfilled the same way.** A key into a column
/// the plan adds to the *parent* names, in the deleted row, the value that
/// column is added with — the same value every other parent row now holds.
/// So a child matching it references the deleted row and every survivor
/// alike, and the engine accepts the delete as long as one survivor holds the
/// tuple — **measured**: with one parent row left the key is added, with none
/// it fails with 23503. The count therefore subtracts the rows a parent row
/// this plan does not delete still satisfies (DECISIONS 337). The unique key
/// the planned foreign key needs on that column is the plan's problem, not
/// the probe's: an `ADD UNIQUE` that fails on two survivors fails loudly.
///
/// The refusal beside it is DECISIONS 124's: a default the probe cannot
/// evaluate, backfilled into every stored row on either side or written by
/// one of the plan's rows to a column of the key, may be the deleted row's
/// key, and nothing can say so before it runs. Counted as rows — the ones the
/// default reaches — for the reason `unprobeable_probe` gives.
///
/// `ONLY` by `relkind`, as the count over catalog keys does: the key is not
/// inherited, and a partitioned child is scanned whole (DECISIONS 334).
fn planned_key_probes(
    table: &TableName,
    key: &RowKey,
    stored: &ColumnRef,
    names: &AsStored,
) -> Result<Vec<Probe>, DialectError> {
    let parent = qualified(&stored.table)?;
    let parent_key = quote(&stored.name)?;
    let parent_row = format!(
        "SELECT 1 FROM {parent} AS p WHERE p.{parent_key} = {}",
        value_literal(key.as_str())
    );
    let is_null = |sql: &str| crate::rows::unwrapped(sql).eq_ignore_ascii_case("null");
    let mut out = Vec::new();
    for a in names.added.iter().filter(|a| a.parent == *table) {
        // A child this plan creates is named as the plan names it, and holds
        // no stored rows: only its arrivals are counted (DECISIONS 343).
        let created = names.created.contains(&a.child);
        let stored_child = if created {
            a.child.clone()
        } else {
            match names.table(&a.child) {
                Some(t) => t,
                None => continue,
            }
        };
        let child_sql = qualified(&stored_child)?;
        // What a column this plan adds holds in every stored row once the
        // `ADD COLUMN` has run: `None` for a backfill no probe can evaluate —
        // an expression, or the value an identity sequence hands out. A
        // literal is compared through the column's declared type, as every
        // other default comparison in this crate is (DECISIONS 329, 339),
        // and a NULL stays a bare NULL so that it reads as one.
        let backfill = |column: &ColumnRef| -> Option<Option<String>> {
            names.columns_added.get(column).map(|added| {
                if added.identity {
                    return None;
                }
                match &added.default {
                    Some(d) => constant_default(d).map(|literal| {
                        if is_null(literal) {
                            "NULL".to_owned()
                        } else {
                            format!(
                                "CAST(({literal}) AS {})",
                                crate::types::normalize(&added.ty)
                                    .unwrap_or_else(|_| added.ty.clone())
                            )
                        }
                    }),
                    None => Some("NULL".to_owned()),
                }
            })
        };
        // The key, column by column: the child's column in the plan's name,
        // the parent side as SQL over an alias of the parent row, and the
        // child side every stored row holds. `None` on either side is a
        // backfill no probe can evaluate.
        type Side = Box<dyn Fn(&str) -> Option<String>>;
        // (child column, referenced column, parent side, child side)
        let mut columns: Vec<(String, String, Side, Option<String>)> = Vec::new();
        let mut unprobeable_backfill = Vec::new();
        // The child columns of the key every stored row already holds, as
        // SQL over `ch`: a NULL there is a tuple that references nothing,
        // whatever backfill the other columns get (DECISIONS 348).
        let mut stored_sides: Vec<String> = Vec::new();
        // Per referenced column this plan compares as a stored column: the
        // SQL that spells its collation clause from the catalog, and the mark
        // that stands for it in a body the engine assembles (DECISIONS 353).
        let mut clauses: Vec<String> = Vec::new();
        let mut collated: BTreeMap<String, usize> = BTreeMap::new();
        for (c, r) in a.columns.iter().zip(&a.referenced) {
            let Some(stored_r) = names.column(&a.parent.column(r)) else {
                continue;
            };
            let stored_c = if created {
                a.child.column(c)
            } else {
                match names.column(&a.child.column(c)) {
                    Some(r) => r,
                    None => continue,
                }
            };
            let parent_side: Side = match backfill(&a.parent.column(r)) {
                Some(held) => {
                    if held.is_none() {
                        unprobeable_backfill.push(described_backfill(
                            r,
                            &a.parent,
                            names.columns_added.get(&a.parent.column(r)),
                        ));
                    }
                    Box::new(move |_alias: &str| held.clone())
                }
                None => {
                    // Spelled once per alias the tuple uses, so that the
                    // closure owns what it needs.
                    let column = a.parent.column(r);
                    let quoted = quote(&stored_r.name)?;
                    let p = names.converted(&column, format!("p.{quoted}"));
                    let q = names.converted(&column, format!("q.{quoted}"));
                    // A column this plan retypes carries its collation into
                    // the new type only where that type has one.
                    let still_collatable = match names.retyped.get(&column) {
                        Some(to) => format!(
                            " AND (SELECT t.typcollation <> 0 FROM pg_catalog.pg_type t \
                             WHERE t.oid = pg_catalog.to_regtype({}))",
                            value_literal(
                                &crate::types::normalize(to)
                                    .unwrap_or_else(|_| to.clone())
                                    .to_string()
                            )
                        ),
                        None => String::new(),
                    };
                    collated.insert(r.clone(), clauses.len());
                    clauses.push(format!(
                        "(SELECT CASE WHEN a.attcollation <> 0{still_collatable} \
                         THEN ' COLLATE ' || pg_catalog.quote_ident(n.nspname) || '.' \
                         || pg_catalog.quote_ident(c.collname) ELSE '' END \
                         FROM pg_catalog.pg_attribute a \
                         LEFT JOIN pg_catalog.pg_collation c ON c.oid = a.attcollation \
                         LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.collnamespace \
                         WHERE a.attrelid = pg_catalog.to_regclass({}) AND a.attname = {})",
                        value_literal(&parent),
                        value_literal(&stored_r.name)
                    ));
                    Box::new(move |alias: &str| {
                        Some(if alias == "q" { q.clone() } else { p.clone() })
                    })
                }
            };
            let held = match backfill(&a.child.column(c)) {
                Some(held) => {
                    if held.is_none() {
                        unprobeable_backfill.push(described_backfill(
                            c,
                            &a.child,
                            names.columns_added.get(&a.child.column(c)),
                        ));
                    }
                    held
                }
                None => {
                    let side = names
                        .converted(&a.child.column(c), format!("ch.{}", quote(&stored_c.name)?));
                    stored_sides.push(side.clone());
                    Some(side)
                }
            };
            columns.push((c.clone(), r.clone(), parent_side, held));
        }
        // A NULL on either side of any column: the tuple references nothing,
        // whatever the other columns hold. Through `unwrapped`, because a
        // declared `NULL::text` is a NULL too (DECISIONS 337).
        let backfill_null = columns.iter().any(|(_, _, parent_side, held)| {
            parent_side("p").as_deref().is_some_and(is_null) || held.as_deref().is_some_and(is_null)
        });
        // Per row this plan writes, whether its tuple *as written* holds a
        // NULL: the cell it spells, or the backfill where it spells none.
        // Not the table-wide answer above — a row may spell a value into a
        // column whose backfill is NULL, and that row's tuple then holds
        // none, whatever every stored row holds (DECISIONS 347).
        let parent_null = columns
            .iter()
            .any(|(_, _, parent_side, _)| parent_side("p").as_deref().is_some_and(is_null));
        let parent_unprobeable = columns
            .iter()
            .any(|(_, _, parent_side, _)| parent_side("p").is_none());
        let tuple_null = |sides: &dyn Fn(&str) -> Option<Option<String>>| -> bool {
            parent_null
                || columns.iter().any(|(c, _, _, held)| match sides(c) {
                    Some(Some(sql)) => is_null(&sql),
                    Some(None) => false,
                    None => held.as_deref().is_some_and(is_null),
                })
        };
        // The parent rows this plan itself writes before the delete, as the
        // survivor check has to see them (DECISIONS 338): per referenced
        // column, the rows an update sets it in and what it is set to, and
        // per inserted row what it holds in every referenced column.
        let parent_moved = names.moved.get(table);
        let parent_updates: BTreeMap<&String, Vec<(String, Option<String>)>> = parent_moved
            .map(|m| {
                let mut out: BTreeMap<&String, Vec<(String, Option<String>)>> = BTreeMap::new();
                for (row_key, set) in &m.updated {
                    for (column, after) in set {
                        if a.referenced.contains(column) {
                            out.entry(column)
                                .or_default()
                                .push((value_literal(row_key.as_str()), after.clone()));
                        }
                    }
                }
                out
            })
            .unwrap_or_default();
        // ` AND <parent side> = <child side>` for each column of the key, a
        // NULL where a side is not one the probe can compare — which decides
        // the tuple only when no other column already does. Under the alias
        // of a surviving parent row, the value an update of this plan leaves
        // in it; the deleted row's own updates do not outlive it.
        let tuple = |alias: &str, sides: &dyn Fn(&str) -> Option<String>| -> String {
            columns
                .iter()
                .map(|(c, r, parent_side, held)| {
                    let stored = parent_side(alias).unwrap_or_else(|| "NULL".to_owned());
                    let parent_value = match parent_updates.get(r) {
                        Some(rows) if alias == "q" => {
                            let whens: Vec<String> = rows
                                .iter()
                                .map(|(row, after)| {
                                    format!(
                                        "WHEN {row} THEN {}",
                                        after.clone().map_or_else(
                                            || "NULL".to_owned(),
                                            |sql| names.typed(&a.parent.column(r), sql)
                                        )
                                    )
                                })
                                .collect();
                            format!(
                                "CASE {alias}.{parent_key} {} ELSE {stored} END",
                                whens.join(" ")
                            )
                        }
                        _ => stored,
                    };
                    let spelled = sides(c).map(|sql| names.typed(&a.child.column(c), sql));
                    // Two stored columns, and the engine's own check compares
                    // them under the referenced one's collation, which the
                    // mark stands for (DECISIONS 353). A literal takes the
                    // column's collation on its own.
                    let mark = match (&spelled, held, collated.get(r)) {
                        (None, Some(h), Some(i)) if stored_sides.iter().any(|s| s == h) => {
                            format!("{COLLATION_MARK}{i}{COLLATION_MARK}")
                        }
                        _ => String::new(),
                    };
                    format!(
                        " AND {parent_value}{mark} = {}",
                        spelled
                            .or_else(|| held.clone())
                            .unwrap_or_else(|| "NULL".to_owned())
                    )
                })
                .collect()
        };
        // Where the referenced side is backfilled, every parent row holds the
        // same value there; where it is retyped, two parent rows may hold
        // one converted value. Either way a child that matches the deleted
        // row may match a parent row this plan leaves in place: it is
        // referencing that one, and the engine takes the delete. The
        // survivors are the stored rows the plan does not delete, as its
        // updates leave them, and the rows it inserts on the parent
        // (DECISIONS 337, 338, 341).
        let survivors = {
            let deleted: Vec<String> = parent_moved
                .map(|m| {
                    m.deleted
                        .iter()
                        .map(|k| value_literal(k.as_str()))
                        .collect()
                })
                .unwrap_or_default();
            let gone = if deleted.is_empty() {
                String::new()
            } else {
                format!(" AND q.{parent_key} NOT IN ({})", deleted.join(", "))
            };
            // An inserted parent row is a constant tuple: what the insert
            // spells or leaves to a known default, the backfill in a
            // planned column, and nothing the probe can ask about
            // anywhere else — such a row is not a survivor it can see.
            let mut inserted = Vec::new();
            for set in parent_moved
                .map(|m| m.inserted.values())
                .into_iter()
                .flatten()
            {
                let mut all = Vec::new();
                for (_, r, parent_side, _) in &columns {
                    let value = match set.get(r) {
                        Some(sql) => Some(names.typed(&a.parent.column(r), sql.clone())),
                        None if names.columns_added.contains_key(&a.parent.column(r)) => {
                            parent_side("q")
                        }
                        None => None,
                    };
                    all.push(value);
                }
                if all.iter().all(Option::is_some) {
                    inserted.push(all.into_iter().flatten().collect::<Vec<_>>());
                }
            }
            (gone, inserted)
        };
        let references = |sides: &dyn Fn(&str) -> Option<String>| {
            let survivor = {
                let (gone, inserted) = &survivors;
                {
                    let mut terms = vec![format!(
                        "EXISTS (SELECT 1 FROM {parent} AS q WHERE true{gone}{})",
                        tuple("q", sides)
                    )];
                    for row in inserted {
                        let matched: Vec<String> = columns
                            .iter()
                            .zip(row)
                            .map(|((c, r, _, held), value)| {
                                // The inserted value is a literal, and so may
                                // be the child's side: two literals compare
                                // under the database's collation, not the
                                // referenced column's, which under a
                                // case-insensitive one calls `a` and `A`
                                // two values the engine's own check calls
                                // one. The mark stands for the referenced
                                // column's collation here as between two
                                // stored columns (DECISIONS 353, 366).
                                let mark = collated
                                    .get(r)
                                    .map(|i| format!("{COLLATION_MARK}{i}{COLLATION_MARK}"))
                                    .unwrap_or_default();
                                format!(
                                    "{value}{mark} = {}",
                                    sides(c)
                                        .map(|sql| names.typed(&a.child.column(c), sql))
                                        .or_else(|| held.clone())
                                        .unwrap_or_else(|| "NULL".to_owned())
                                )
                            })
                            .collect();
                        terms.push(format!("({})", matched.join(" AND ")));
                    }
                    format!(" AND NOT ({})", terms.join(" OR "))
                }
            };
            format!("(EXISTS ({parent_row}{}){survivor})", tuple("p", sides))
        };
        let as_backfilled = references(&|_| None);
        // The count over the stored rows, `ONLY` unless partitioned, assembled
        // by the engine because `relkind` is the engine's to say — and, where
        // a body carries a collation mark, because the collation is too.
        // The text of `body` as the engine will concatenate it: its literal
        // pieces, and in place of each mark the clause that reads the
        // collation from the catalog.
        let spliced = |body: &str| -> Vec<String> {
            let mut pieces = Vec::new();
            for (i, part) in body.split(COLLATION_MARK).enumerate() {
                if i % 2 == 0 {
                    pieces.push(value_literal(part));
                } else {
                    let clause = &clauses[part.parse::<usize>().expect("a mark's index")];
                    pieces.push(format!("COALESCE({clause}, '')"));
                }
            }
            pieces
        };
        // One integer term of the count, assembled by the engine where it
        // carries a mark — a row this plan inserts, compared with a parent
        // row it inserts under the referenced column's collation (DECISIONS
        // 366) — and as it is where it does not, so that a probe with no
        // collation to ask about stays readable.
        let evaluated = |term: &str| -> String {
            if !term.contains(COLLATION_MARK) {
                return term.to_owned();
            }
            format!(
                "COALESCE((SELECT (pg_catalog.xpath('/row/n/text()',
                              pg_catalog.query_to_xml('SELECT (' || {} || ') AS n', false, true, ''))                 )[1]::text::int), 0)",
                spliced(term).join(" || ")
            )
        };
        let stored_rows = |body: &str| {
            let pieces = spliced(body);
            format!(
                "LEAST(COALESCE((SELECT (pg_catalog.xpath('/row/n/text()',\n           \
                   pg_catalog.query_to_xml('SELECT (SELECT count(*) FROM '\n             \
                     || CASE WHEN cl.relkind = 'p' THEN '' ELSE 'ONLY ' END\n             \
                     || {} || ') AS n', false, true, '')))[1]::text::bigint\n    \
                 FROM pg_catalog.pg_class cl\n   \
                WHERE cl.oid = pg_catalog.to_regclass({})), 0), 2147483647)::int",
                {
                    let mut body_pieces = vec![value_literal(&format!("{child_sql} AS ch WHERE "))];
                    body_pieces.extend(pieces);
                    body_pieces.join(" || ")
                },
                value_literal(&child_sql)
            )
        };

        let moved = names.moved.get(&a.child);
        let key_sql = match moved {
            Some(m) => Some(format!(
                "ch.{}",
                quote(
                    &names
                        .column(&a.child.column(&m.key_column))
                        .map(|r| r.name)
                        .unwrap_or_else(|| m.key_column.clone())
                )?
            )),
            None => None,
        };
        // Rows this plan deletes from the child are gone whatever they hold.
        let mut surviving = String::new();
        let mut narrowing = String::new();
        let mut arrivals = String::new();
        let mut refused: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut arriving: BTreeSet<String> = BTreeSet::new();
        // Stored rows this plan rewrites in a column of the key are counted
        // as written — refused, arriving, or narrowed — never as stored.
        let mut rewritten = String::new();
        if let (Some(m), Some(key_sql)) = (moved, &key_sql) {
            if !m.deleted.is_empty() {
                let list: Vec<String> = m
                    .deleted
                    .iter()
                    .map(|k| value_literal(k.as_str()))
                    .collect();
                surviving = format!(" AND {key_sql} NOT IN ({})", list.join(", "));
            }
            let in_key = |column: &str| a.columns.iter().any(|c| c == column);
            let rewriting: Vec<String> = m
                .updated
                .iter()
                .filter(|(_, set)| set.keys().any(|c| in_key(c)))
                .map(|(k, _)| value_literal(k.as_str()))
                .collect();
            if !rewriting.is_empty() {
                rewritten = format!(" AND {key_sql} NOT IN ({})", rewriting.join(", "));
            }
            for (row_key, set) in &m.updated {
                if !set.keys().any(|c| in_key(c)) {
                    continue;
                }
                let row = value_literal(row_key.as_str());
                let after = references(&|c| set.get(c).cloned().flatten());
                // The cell the update writes; else a NULL it leaves the row
                // holding (DECISIONS 349); else the column's backfill.
                let held_null = m.held_null.get(row_key);
                let written_null = tuple_null(&|c| {
                    set.get(c).cloned().or_else(|| {
                        held_null
                            .is_some_and(|h| h.contains(c))
                            .then(|| Some("NULL".to_owned()))
                    })
                });
                let uncomparable: Vec<&String> = set
                    .iter()
                    .filter(|(c, sql)| in_key(c) && sql.is_none())
                    .map(|(c, _)| c)
                    .collect();
                if !uncomparable.is_empty() && !written_null {
                    // Counted as it stands, and refused: the row's tuple after
                    // the update is one no probe can compare.
                    for c in uncomparable {
                        refused
                            .entry(c.clone())
                            .or_default()
                            .insert(row_key.to_string());
                    }
                    continue;
                }
                // Its tuple after the update, against a parent backfill the
                // probe cannot evaluate, may be the deleted row's.
                if parent_unprobeable && !written_null {
                    arriving.insert(row_key.to_string());
                    continue;
                }
                narrowing.push_str(&format!(" AND NOT ({key_sql} = {row} AND NOT {after})"));
                arrivals.push_str(&format!(
                    " + {}",
                    stored_rows(&format!(
                        "{key_sql} = {row} AND NOT {as_backfilled} AND {after}"
                    ))
                ));
            }
            for (row_key, set) in &m.inserted {
                // A stored column the insert leaves alone is not one the probe
                // can ask about (DECISIONS 117); a planned one holds its
                // default.
                let written_null = tuple_null(&|c| set.get(c).cloned().map(Some));
                // A column of the key the insert neither spells nor leaves
                // to a default the plan carries — every other column is in
                // the insert's `types` — is one the engine assigns, an
                // identity, and its value may be the deleted row's key
                // (DECISIONS 343).
                let assigned: Vec<&String> = columns
                    .iter()
                    .map(|(c, _, _, _)| c)
                    .filter(|c| {
                        !set.contains_key(*c)
                            && !names.columns_added.contains_key(&a.child.column(*c))
                            && !m
                                .unprobeable
                                .get(*c)
                                .is_some_and(|rows| rows.contains(row_key))
                    })
                    .collect();
                if !assigned.is_empty() && !written_null {
                    for c in assigned {
                        refused
                            .entry(format!("{c} (left to the engine to assign)"))
                            .or_default()
                            .insert(row_key.to_string());
                    }
                    continue;
                }
                let left_unprobeable: Vec<&String> = m
                    .unprobeable
                    .iter()
                    .filter(|(c, rows)| in_key(c) && rows.contains(row_key))
                    .map(|(c, _)| c)
                    .collect();
                if !left_unprobeable.is_empty() && !written_null {
                    for c in left_unprobeable {
                        refused
                            .entry(c.clone())
                            .or_default()
                            .insert(row_key.to_string());
                    }
                    continue;
                }
                // A row arriving against a parent backfill the probe cannot
                // evaluate may be arriving on the deleted row: refused with
                // the stored rows that backfill reaches, unless its own
                // tuple holds a NULL (DECISIONS 345, 347).
                if parent_unprobeable && !written_null {
                    arriving.insert(row_key.to_string());
                    continue;
                }
                arrivals.push_str(&format!(
                    " + {}",
                    evaluated(&format!(
                        "(CASE WHEN {} THEN 1 ELSE 0 END)",
                        references(&|c| set.get(c).cloned())
                    ))
                ));
            }
        }
        out.push(Probe::new(
            format!(
                "rows of {} that reference {table} row `{key}` through the foreign key this plan \
                 adds on a column it also adds, whose default is what every stored row will hold \
                 there; the key cannot be added once the row is gone",
                a.child
            ),
            format!(
                "SELECT LEAST({}{arrivals}, 2147483647)::int AS n;",
                if created {
                    "0".to_owned()
                } else {
                    stored_rows(&format!("{as_backfilled}{surviving}{narrowing}"))
                }
            ),
        ));
        // A backfill no probe can evaluate reaches every stored row the plan
        // does not delete or rewrite — unless another column of the key
        // backfills NULL, or the row itself holds a NULL in a stored column
        // of the key: that tuple references nothing however the backfill
        // evaluates (DECISIONS 348).
        let mut terms = Vec::new();
        if !unprobeable_backfill.is_empty() && !backfill_null {
            let holding: String = stored_sides
                .iter()
                .map(|side| format!(" AND ({side}) IS NOT NULL"))
                .collect();
            terms.push(if created {
                "0::int".to_owned()
            } else {
                stored_rows(&format!("true{surviving}{rewritten}{holding}"))
            });
        }
        if !arriving.is_empty() {
            terms.push(format!("{}::int", arriving.len()));
        }
        if !refused.is_empty() {
            let n = refused.values().flatten().collect::<BTreeSet<_>>().len();
            terms.push(format!("{n}::int"));
        }
        if terms.is_empty() {
            continue;
        }
        let mut described: Vec<String> = unprobeable_backfill
            .iter()
            .filter(|_| !backfill_null)
            .cloned()
            .collect();
        for (column, rows) in &refused {
            let listed: Vec<String> = rows.iter().map(|r| format!("`{r}`")).collect();
            described.push(format!(
                "{column} (row{} {})",
                if listed.len() == 1 { "" } else { "s" },
                listed.join(", ")
            ));
        }
        if !arriving.is_empty() {
            let listed: Vec<String> = arriving.iter().map(|r| format!("`{r}`")).collect();
            described.push(format!(
                "row{} {} of {} written against that backfill",
                if listed.len() == 1 { "" } else { "s" },
                listed.join(", "),
                a.child
            ));
        }
        out.push(Probe::new(
            format!(
                "column(s) of the foreign key this plan adds from {} into {table} that hold a \
                 default the probe cannot evaluate, which may be row `{key}` being deleted: {}; \
                 spell the value",
                a.child,
                described.join(", ")
            ),
            format!("SELECT LEAST({}, 2147483647)::int AS n;", terms.join(" + ")),
        ));
    }
    // A mark stands for a collation only inside a body the engine assembles;
    // as text in a statement it is a syntax error the runner would read as
    // unchecked (DECISIONS 353).
    if let Some(p) = out.iter().find(|p| p.sql.contains(COLLATION_MARK)) {
        return Err(DialectError::Invalid {
            dialect: crate::types::DIALECT,
            message: format!(
                "a collation mark reached a probe as text — a comparison of two stored columns \
                 outside the engine-assembled count: {}",
                p.description
            ),
        });
    }
    Ok(out)
}

/// How a backfill no probe can evaluate is named in the refusal: the value
/// the identity sequence hands every stored row, or the default expression.
fn described_backfill(column: &str, table: &TableName, added: Option<&AddedColumn>) -> String {
    if added.is_some_and(|a| a.identity) {
        format!("{column} of {table} (its identity, assigned to every stored row)")
    } else {
        format!("{column} of {table} (its default, backfilled into every stored row)")
    }
}

/// Every probe this dialect asks before a plan runs.
///
/// Per `DeleteRow`, the count of what still references the row, and then one
/// refusal per table this plan writes to a default the probe cannot evaluate
/// (DECISIONS 124). Nothing else yet: see this module's own documentation for
/// what Phase 5 step 9 adds.
///
/// The count comes first, so a reader meets the rows before the refusal.
pub(crate) fn probes(changes: &ChangeSet) -> Vec<Probe> {
    let names = AsStored::of(changes);
    let mut out = Vec::new();
    for p in &changes.changes {
        if let Change::DeleteRow {
            table,
            key_column,
            key,
            ..
        } = &p.change
            && let Some(stored) = names.column(&table.column(key_column))
            && let Ok(probe) = delete_probe(table, key, &stored, &names)
        {
            out.push(probe);
            // Then whether that count could be complete at all, before the
            // refusals about what the plan itself writes.
            if let Ok(hidden) = hidden_children_probe(table, key, &stored, &names) {
                out.push(hidden);
            }
            if let Ok(planned) = planned_key_probes(table, key, &stored, &names) {
                out.extend(planned);
            }
            for (child, moved) in &names.moved {
                if let Ok(Some(refusal)) =
                    unprobeable_probe(table, key, &stored, child, moved, &names)
                {
                    out.push(refusal);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Change, PlannedChange};

    fn planned(change: Change) -> PlannedChange {
        PlannedChange::new(change)
    }

    fn deleting(table: &str, key: &str) -> Change {
        Change::DeleteRow {
            table: table.parse().expect("a table name"),
            key_column: "code".to_owned(),
            key: RowKey::from(key),
            cause: pbps_model::change::DeleteCause::Undeclared,
            row: BTreeMap::new(),
            types: BTreeMap::new(),
            after_types: BTreeMap::new(),
        }
    }

    fn set(changes: Vec<Change>) -> ChangeSet {
        ChangeSet {
            changes: changes.into_iter().map(planned).collect(),
        }
    }

    /// ADR-0013 §1, and the reason it is a test and not only a comment: the
    /// SQL Server probe one crate away *does* read its own flag, so the shape
    /// that looks like the pattern to copy is one grep away. A `NOT VALID`
    /// key still enforces the delete action — measured — so counting only
    /// validated keys would let a delete through that the engine refuses.
    #[test]
    fn the_pre_delete_probe_never_asks_whether_a_foreign_key_is_validated() {
        let probes = probes(&set(vec![deleting("app.status", "old")]));
        // The count, and beside it the refusal that says whether the count
        // could be complete at all (DECISIONS 333).
        assert_eq!(probes.len(), 2, "{probes:#?}");
        let sql = &probes[0].sql;
        assert!(!sql.contains("convalidated"), "{sql}");
        assert!(sql.contains("con.contype = 'f'"), "{sql}");
        assert!(sql.contains("query_to_xml"), "{sql}");
        // The parent is named through `to_regclass`, which answers NULL for a
        // table that is not there — where a `::regclass` cast is an error, and
        // an erroring probe is reported as unchecked.
        assert!(sql.contains("to_regclass"), "{sql}");
        assert!(
            probes[0].description.contains("app.status row `old`"),
            "{probes:#?}"
        );
    }

    /// A key this plan adds is asked about from the plan alone — and the
    /// hidden-children refusal names the child outright (DECISIONS 336, 345).
    #[test]
    fn a_key_this_plan_adds_on_a_column_it_adds_gets_its_own_probes() {
        let child: TableName = "app.child".parse().expect("a table name");
        let parent: TableName = "app.status".parse().expect("a table name");
        let mut column = pbps_model::Column::new("text".parse().expect("a type"));
        column.default = Some("'old'".to_owned());
        let key = pbps_model::ForeignKey {
            columns: vec!["status".to_owned()],
            references_table: parent.clone(),
            references_columns: vec!["code".to_owned()],
            on_delete: pbps_model::ReferentialAction::NoAction,
            on_update: pbps_model::ReferentialAction::NoAction,
        };
        let plan = |column: pbps_model::Column| {
            set(vec![
                Change::AddColumn {
                    uid: pbps_model::Uid::generate(pbps_model::UidKind::Column),
                    table: child.clone(),
                    name: "status".to_owned(),
                    column: Box::new(column),
                },
                Change::AddForeignKey {
                    table: child.clone(),
                    name: "child_status_fkey".to_owned(),
                    constraint: Box::new(key.clone()),
                },
                deleting("app.status", "old"),
            ])
        };
        let asked = probes(&plan(column.clone()));
        assert_eq!(asked.len(), 3, "{asked:#?}");
        // Every probe answers in the runner's width, and a count past it
        // is the width's maximum rather than an out-of-range error the
        // runner would read as unchecked (DECISIONS 342, 343).
        for p in &asked {
            assert!(
                p.sql.contains("2147483647)::int AS n;") || p.sql.contains("2147483647)::int"),
                "{}",
                p.sql
            );
        }
        // And every read of the catalog's keys asks whether the delete
        // action will run in this session (DECISIONS 344).
        for p in asked.iter().take(2) {
            assert!(p.sql.contains("t.tgconstraint = con.oid"), "{}", p.sql);
        }
        assert!(
            asked[1].sql.contains("to_regclass(E'\"app\".\"child\"')"),
            "the hidden-children refusal names the child: {}",
            asked[1].sql
        );
        let planned = &asked[2];
        assert!(
            planned.description.contains("on a column it also adds"),
            "{planned:#?}"
        );
        assert!(
            planned
                .sql
                .contains("AND p.\"code\" = CAST((''old'') AS text))"),
            "the backfilled default is the child side: {}",
            planned.sql
        );
        assert!(
            planned
                .sql
                .contains("CASE WHEN cl.relkind = 'p' THEN '' ELSE 'ONLY ' END"),
            "{}",
            planned.sql
        );
        // A default no probe can evaluate is refused, counted as the rows it
        // reaches.
        column.default = Some("lower('OLD')".to_owned());
        let asked = probes(&plan(column.clone()));
        assert_eq!(asked.len(), 4, "{asked:#?}");
        assert!(
            asked[3].description.contains("cannot evaluate")
                && asked[3]
                    .description
                    .contains("status of app.child (its default, backfilled"),
            "{asked:#?}"
        );
        // And with no default every stored row holds NULL there, which the
        // engine is left to say references nothing: the count is still asked,
        // the refusal is not.
        column.default = None;
        let asked = probes(&plan(column));
        assert_eq!(asked.len(), 3, "{asked:#?}");
        assert!(
            asked[2].sql.contains("AND p.\"code\" = NULL"),
            "{}",
            asked[2].sql
        );
    }

    /// A key this plan removes before its deletes run is left out of the
    /// catalog read altogether: by the time the delete runs the constraint is
    /// gone, and a child counted through it refuses a delete the engine would
    /// accept (DECISIONS 128).
    #[test]
    fn a_key_this_plan_drops_first_is_left_out_of_the_probe() {
        let probes = probes(&set(vec![
            Change::DropForeignKey {
                table: "app.child".parse().expect("a table name"),
                name: "fk_child".to_owned(),
            },
            Change::DropTable {
                uid: pbps_model::Uid::generate(pbps_model::UidKind::Table),
                name: "app.gone".parse().expect("a table name"),
            },
            deleting("app.status", "old"),
        ]));
        let sql = &probes[0].sql;
        assert!(sql.contains("con.conname = E'fk_child'"), "{sql}");
        // The dropped table's keys go with it, so it is excluded by name and
        // without naming a constraint.
        assert!(
            sql.contains("AND NOT (ns.nspname = E'app' AND cl.relname = E'gone')"),
            "{sql}"
        );
    }

    /// The ordinary shape: a child moved to a new parent in the same revision.
    /// The plan runs that update before the delete precisely so the engine
    /// accepts it, and a probe that counted the row would refuse every such
    /// plan.
    #[test]
    fn a_child_row_this_plan_moves_is_left_out_of_the_count() {
        let probes = probes(&set(vec![
            Change::UpdateRow {
                table: "app.child".parse().expect("a table name"),
                key_column: "code".to_owned(),
                key: RowKey::from("c1"),
                columns: [(
                    "parent".to_owned(),
                    (
                        Cell::Value(Value::Text("old".into())),
                        Cell::Value(Value::Text("new".into())),
                    ),
                )]
                .into_iter()
                .collect(),
                unchanged: BTreeMap::new(),
                types: BTreeMap::new(),
                after_types: BTreeMap::new(),
            },
            deleting("app.status", "old"),
        ]));
        let sql = &probes[0].sql;
        assert!(sql.contains("E'app'"), "{sql}");
        // Compared as the constraint compares: under the referenced column's
        // collation, through the operator it records (DECISIONS 352).
        assert!(
            sql.contains("o.oid = con.conpfeqop[k.ord]")
                && sql.contains("CASE WHEN rc.attcollation <> 0")
                && sql.contains("' OPERATOR(' || pg_catalog.quote_ident(opn.nspname)"),
            "{sql}"
        );
        assert!(sql.contains(r#"ch."code" = E''c1''"#), "{sql}");
        // And the same row arriving on the parent is added back, because a
        // `count(*)` over the child cannot see a row that is about to be
        // moved *onto* the deleted parent.
        assert!(sql.contains("SELECT count(*) FROM"), "{sql}");
    }

    /// A delete registers the parent's own row as moved too, because a table
    /// may reference itself — and the exclusion then has to name the key that
    /// is going, or the probe counts the very row it is about to remove.
    /// The negative case that makes the second probe worth having: it asks
    /// about the *session*, not about the switch, so a table under a policy
    /// the deploying role bypasses refuses nothing (DECISIONS 333).
    #[test]
    fn the_count_is_followed_by_whether_this_session_could_complete_it() {
        let probes = probes(&set(vec![deleting("app.status", "old")]));
        let sql = &probes[1].sql;
        assert!(
            sql.contains("pg_catalog.row_security_active(cl.oid)"),
            "{sql}"
        );
        assert!(!sql.contains("relrowsecurity"), "{sql}");
        // Tables, not rows: the count next door is the number about rows, and
        // inflating it to force a refusal would make it a number about
        // something else.
        assert!(sql.contains("FROM pg_catalog.pg_constraint con"), "{sql}");
        assert!(!sql.contains("query_to_xml"), "{sql}");
        assert!(
            probes[1].description.contains("cannot count"),
            "{probes:#?}"
        );
    }

    #[test]
    fn the_deleted_key_is_excluded_from_the_count_on_the_parents_own_table() {
        let probes = probes(&set(vec![deleting("app.status", "old")]));
        let sql = &probes[0].sql;
        assert!(sql.contains(r#"AND ch."code" NOT IN (E''old'')"#), "{sql}");
        // Nothing arrives, so the arrival term is the empty string rather than
        // a `CASE` with no arms, which this engine would refuse to parse.
        assert!(sql.contains("END || ')' || '' || ' AS n' AS stmt"), "{sql}");
    }
}
