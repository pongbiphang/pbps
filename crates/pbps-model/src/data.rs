//! Declarative reference data — the `data:` block (ADR-0004).
//!
//! # Why this belongs in the model, when `strategy:` does not
//!
//! [`Strategy`](crate::strategy::Strategy) says *how* to get somewhere; it is
//! invisible in the database, so putting it inside [`Schema`](crate::Schema)
//! would break inviolable constraint 1. Declared rows are the opposite: they
//! are desired state, they are visible in the database, and drift has to see
//! them. So they live here, participate in `Schema` equality, and travel in the
//! state snapshot like every other declaration.
//!
//! # The line this block draws
//!
//! SPEC §1.3 excludes data *transformation*, and that stance does not move:
//! **the tool touches no table's rows unless the table has a `data:` block.**
//! Reference rows are part of the program — the application names them the way
//! it names a column. Business rows are user history, and are never a
//! declaration.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::name::TableName;
use crate::schema::Table;

/// The declared contents of one table.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableData {
    pub mode: DataMode,

    /// Rows by primary-key value.
    ///
    /// Inviolable constraint 2 at row granularity: the map key **is** the
    /// identity, so a duplicate key is unwritable and the key column's value is
    /// never repeated inside the row body where the two could disagree.
    pub rows: BTreeMap<RowKey, Row>,
}

/// How completely the declaration describes the table (ADR-0004).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum DataMode {
    /// The declared rows are the whole table. An undeclared row is drift, and
    /// removing it is a `DELETE` behind the gate.
    Exact,
    /// The declared rows must exist with the declared values; anything else in
    /// the table is ignored. A seeded core in a table the application also
    /// writes to — SPEC §8.2's "drift compares the managed set only", at row
    /// granularity.
    Ensure,
}

impl DataMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            DataMode::Exact => "exact",
            DataMode::Ensure => "ensure",
        }
    }
}

impl fmt::Display for DataMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One row's primary-key value, in its written form.
///
/// It is **text, not a [`Value`]**, for two reasons that both come from it
/// being a map key. JSON has no non-string keys, and the ids file, the saved
/// plan and the state snapshot are all JSON; and a key that carried its own
/// type could disagree with the type the primary-key column already declares.
/// The emitter renders the key according to that column — the one place that
/// knows how a `varchar` and an `int` differ — so there is exactly one answer.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct RowKey(pub String);

impl RowKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for RowKey {
    fn from(s: &str) -> Self {
        RowKey(s.to_owned())
    }
}

impl From<String> for RowKey {
    fn from(s: String) -> Self {
        RowKey(s)
    }
}

impl fmt::Display for RowKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One row's non-key columns.
///
/// **An omitted column is a declared column.** It means the column's declared
/// default, or `NULL` where there is none — not "this row says nothing about
/// it". The alternative reading would leave part of an `exact` table
/// undeclared, and `exact` mode's whole claim is that the declaration is the
/// table; it would also break ADR-0004's argument that a row can be recreated
/// losslessly, which is why rows need no identity machinery. `validate`
/// therefore refuses a row that omits a `NOT NULL` column with no default.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Row(pub BTreeMap<String, Value>);

impl Row {
    pub fn get(&self, column: &str) -> Option<&Value> {
        self.0.get(column)
    }

    pub fn columns(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<(String, Value)> for Row {
    fn from_iter<T: IntoIterator<Item = (String, Value)>>(it: T) -> Self {
        Row(it.into_iter().collect())
    }
}

/// One cell.
///
/// Deliberately **not** `serde_json::Value`, and there is no floating-point
/// arm at all. Two reasons, and both are load-bearing:
///
/// - `f64` is not `Eq`, and `Schema` equality is what constraint 1 and the
///   whole drift comparison are built on.
/// - The exact decimal form is what reaches the column. Reading `1.10` as a
///   binary float and writing it back out does not promise to return `1.10`,
///   and a declaration that disagrees with its own database on every plan is
///   worse than no declaration.
///
/// So the loader **refuses** a bare non-integer number and asks for it quoted.
/// Quoted, it is [`Value::Text`] holding exactly what was written, and the
/// emitter renders it as a numeric literal or a quoted string according to the
/// column's declared type — the same rule [`RowKey`] follows, and the same
/// treatment defaults and check expressions already get: keep it verbatim,
/// never parse it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    /// Anything else, exactly as written: a label, a code, and a decimal the
    /// user quoted.
    Text(String),
}

impl Value {
    /// What this value is called in a diagnostic.
    pub const fn kind(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Int(_) => "integer",
            Value::Text(_) => "text",
        }
    }

    pub const fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

impl fmt::Display for Value {
    /// For diagnostics and plan summaries only. It is **not** a SQL literal:
    /// nothing here quotes or escapes, because that is the emitter's job and
    /// SQL appears exactly once (constraint 3).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Text(t) => write!(f, "{t:?}"),
        }
    }
}

/// What a row says about one column, once the omission rule has been applied.
///
/// A [`Row`] cannot answer this on its own: an omitted column means the
/// column's declared default, and the column's declaration lives in the
/// table, which the row does not reach (constraint 2). So the differ resolves
/// each side's row against **that side's** table, and compares these.
///
/// `Default` is kept apart from any [`Value`] on purpose. The default is an
/// expression (`SYSUTCDATETIME()`, `0`, `''`) that only the engine can
/// evaluate, so "the default" and "NULL" are different declarations even when
/// they happen to evaluate the same — and an `UPDATE` has to say `DEFAULT`,
/// not `NULL`, or a `NOT NULL` column with a default fails on a row that
/// `validate` had passed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cell {
    Value(Value),
    /// The column's declared default — the expression as declared, because
    /// *which* default matters: when it changes, a row that omits the column
    /// should hold the new one, and `ALTER` does not backfill. Compared as
    /// text, like the defaults themselves are.
    Default(String),
}

impl fmt::Display for Cell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cell::Value(v) => v.fmt(f),
            Cell::Default(expr) => write!(f, "default ({expr})"),
        }
    }
}

/// Resolves one column of a row against the column's declaration.
///
/// `spec` is `None` when this side's table does not have the column at all —
/// the base side of a row whose table gains the column in the same plan. The
/// engine will have put NULL there (or the default, for a NOT NULL add), and
/// NULL is the answer that makes the differ restate whatever the declaration
/// wants: an `UPDATE ... = DEFAULT` after the `ADD` is idempotent, and a
/// declared NULL matches without one.
pub fn cell(row: &Row, column: &str, spec: Option<&crate::schema::Column>) -> Cell {
    match row.get(column) {
        Some(v) => Cell::Value(v.clone()),
        None => match spec.and_then(|c| c.default.as_ref()) {
            Some(default) => Cell::Default(default.clone()),
            None => Cell::Value(Value::Null),
        },
    }
}

/// Which of a table's rows a read-back is asked for (the connected half of
/// ADR-0004).
///
/// The catalog cannot decide this on its own: a database has rows, not a
/// notion of which of them are declared. The scope comes from a declaration or
/// from a recorded state, and it is what turns "the table holds these rows"
/// into a [`TableData`] the differ can compare.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataScope {
    pub mode: DataMode,
    /// The declared keys. For an `exact` table they are informational — every
    /// row is read, because an undeclared one is exactly what has to be seen.
    /// For an `ensure` table they are the whole scope: nothing else in the
    /// table is looked at, which is the promise the mode makes to a table the
    /// application also writes to.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub keys: BTreeSet<RowKey>,
}

/// The scopes of every table that declares rows, by table.
pub type DataScopes = BTreeMap<TableName, DataScope>;

/// What was read back: every row the read scope asked for, by table.
///
/// A table that was in scope but absent from the database has no entry — it
/// is reported as missing by the managed-set check, not read as empty.
/// One row as the catalog read it (the connected half of ADR-0004).
///
/// Every cell the read fetched is in `cells`, explicit. `at_default` names
/// the cells that hold the column's default — or whose default the engine was
/// not asked to evaluate, so the stored value cannot be told from it. The
/// catalog does not know which spelling a row was *written* with: a cell that
/// holds `'Unlabelled'` may have been declared as `label: Unlabelled` or by
/// leaving `label` out. So the observation carries both readings, and the
/// side that looks at it chooses ([`ObservedRow::as_seen_by`]) — which is
/// what lets an explicit value that equals the default, and an omitted one,
/// both compare equal to what they wrote, rather than one of them being
/// restated on every connected plan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservedRow {
    pub cells: Row,
    /// The engine confirmed these cells hold the column's default.
    pub at_default: BTreeSet<String>,
    /// The engine was never asked about these: the column's default is an
    /// expression it would have had to run. Nobody can tell the value from
    /// the default, so the side's own spelling decides — and a side with no
    /// spelling keeps the value, because a `NEWID()` key is a value.
    pub unknown: BTreeSet<String>,
}

impl ObservedRow {
    /// The row as one side spells it. A cell that side writes explicitly is
    /// read explicit, whatever it holds. A cell the engine confirmed at its
    /// default is omitted where the side omits it, and where there is no
    /// side at all (`pull`): it *is* the default. A cell whose default could
    /// not be asked about is omitted only where the side's own row omits it
    /// — taken at the declaration's word — and kept everywhere else.
    pub fn as_seen_by(&self, reference: Option<&Row>) -> Row {
        self.cells
            .columns()
            .filter(|(column, _)| {
                let spelled = reference.is_some_and(|r| r.get(column).is_some());
                if self.at_default.contains(*column) {
                    spelled
                } else if self.unknown.contains(*column) {
                    reference.is_none_or(|r| r.get(column).is_some())
                } else {
                    true
                }
            })
            .map(|(column, value)| (column.clone(), value.clone()))
            .collect()
    }
}

/// One table's rows as the catalog read them, keyed by the **engine's**
/// spelling of each key, with the spellings the read was asked for beside
/// them.
///
/// A declaration may write a key the engine spells differently — `"01"` for
/// an `int` key that comes back as `1`, `"a "` for a `char` that comes back
/// trimmed — and the engine, not this crate, decides that they are the same
/// row (§8.2). So every key a side spells is sent along with the read, the
/// engine says which row it names, and `aliases` records the answer; a side
/// then reads its rows back under its own spelling ([`ObservedTable::row`],
/// [`ObservedTable::rows_as`]). Without that, an `exact` declaration of `01`
/// planned an insert of `01` and a delete of `1` on every connected plan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservedTable {
    pub rows: BTreeMap<RowKey, ObservedRow>,
    /// Requested spelling -> the engine's, for every requested key that
    /// named a row.
    pub aliases: BTreeMap<RowKey, RowKey>,
}

impl ObservedTable {
    /// The row a side names by its own spelling, with the engine's.
    pub fn row(&self, key: &RowKey) -> Option<(&RowKey, &ObservedRow)> {
        let canonical = self.aliases.get(key).unwrap_or(key);
        self.rows.get_key_value(canonical)
    }

    /// Two of `spelling`'s keys that name one row, with the engine's key.
    pub fn conflict(&self, spelling: &BTreeSet<RowKey>) -> Option<(RowKey, RowKey, RowKey)> {
        let mut seen: BTreeMap<&RowKey, &RowKey> = BTreeMap::new();
        for key in spelling {
            if let Some(canonical) = self.aliases.get(key)
                && let Some(first) = seen.insert(canonical, key)
            {
                return Some((first.clone(), key.clone(), canonical.clone()));
            }
        }
        None
    }

    /// Every row, keyed as `spelling` spells it where it does and as the
    /// engine does elsewhere.
    pub fn rows_as(&self, spelling: &BTreeSet<RowKey>) -> BTreeMap<RowKey, &ObservedRow> {
        let mut requested: BTreeMap<&RowKey, &RowKey> = BTreeMap::new();
        for key in spelling {
            if let Some(canonical) = self.aliases.get(key) {
                requested.entry(canonical).or_insert(key);
            }
        }
        self.rows
            .iter()
            .map(|(canonical, row)| {
                let key = requested
                    .get(canonical)
                    .map_or_else(|| canonical.clone(), |k| (*k).clone());
                (key, row)
            })
            .collect()
    }
}

pub type ObservedRows = BTreeMap<TableName, ObservedTable>;

/// Two keys one side spells that the engine calls the same row — `1` and
/// `01` for an `int` key, `a` and `A` under a case-insensitive collation.
///
/// Refused rather than reconciled: keeping one spelling would plan an insert
/// of the other, which fails on the primary key at apply, and `validate`
/// cannot see it (which spellings are equal is the engine's call). Surfaced by
/// every connected command that projects rows.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{table}: rows `{first}` and `{second}` are the same row to the database (its key is \
     `{canonical}`); declare it once"
)]
pub struct RowConflict {
    pub table: TableName,
    pub first: RowKey,
    pub second: RowKey,
    pub canonical: RowKey,
}

/// Which rows a read-back query fetches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowScope {
    /// Every row of the table. `known` are the keys the reader spells, sent
    /// along so the engine can say which row each names
    /// ([`ObservedTable::aliases`]).
    Every { known: BTreeSet<RowKey> },
    /// Only the rows with these keys; the rest of the table is not looked at.
    Keys(BTreeSet<RowKey>),
}

impl RowScope {
    /// The narrower of two reads that still answers both.
    pub fn union(self, other: RowScope) -> RowScope {
        match (self, other) {
            (RowScope::Every { known: mut a }, RowScope::Every { known: b })
            | (RowScope::Every { known: mut a }, RowScope::Keys(b))
            | (RowScope::Keys(b), RowScope::Every { known: mut a }) => {
                a.extend(b);
                RowScope::Every { known: a }
            }
            (RowScope::Keys(mut a), RowScope::Keys(b)) => {
                a.extend(b);
                RowScope::Keys(a)
            }
        }
    }

    /// The keys this read spells, whichever shape it has.
    pub fn known(&self) -> &BTreeSet<RowKey> {
        match self {
            RowScope::Every { known } => known,
            RowScope::Keys(keys) => keys,
        }
    }
}

impl DataScope {
    pub fn of(data: &TableData) -> DataScope {
        DataScope {
            mode: data.mode,
            keys: data.rows.keys().cloned().collect(),
        }
    }

    /// The rows the catalog has to fetch to answer this scope.
    /// The scope that answers both: `exact` if either is, and every key
    /// either spells. What a plan's baseline is pinned under when the
    /// recorded scope and the plan's both cover a table — a key the plan adds
    /// to an `ensure` block, or an `ensure` -> `exact` switch, has rows the
    /// differ measured and the recorded scope alone would not check again
    /// before apply (DECISIONS 98).
    pub fn union(mut self, other: DataScope) -> DataScope {
        if other.mode == DataMode::Exact {
            self.mode = DataMode::Exact;
        }
        self.keys.extend(other.keys);
        self
    }

    pub fn rows_to_read(&self) -> RowScope {
        match self.mode {
            DataMode::Exact => RowScope::Every {
                known: self.keys.clone(),
            },
            DataMode::Ensure => RowScope::Keys(self.keys.clone()),
        }
    }

    /// The block this scope describes, from rows read under it or under a
    /// wider read.
    ///
    /// The filter is what makes `ensure` mean what it says: a read that
    /// happened to fetch every row (because another scope on the same table
    /// needed them) must not turn the undeclared ones into recorded state, or
    /// the next `verify` would call the application's own inserts drift.
    ///
    /// `reference` is this side's own block, whose spelling of each cell
    /// decides how a cell at its default is read (see [`ObservedRow`]).
    pub fn project(&self, observed: &ObservedTable, reference: Option<&TableData>) -> TableData {
        let seen = |k: RowKey, r: &ObservedRow| {
            let row = r.as_seen_by(reference.and_then(|d| d.rows.get(&k)));
            (k, row)
        };
        let rows = match self.mode {
            DataMode::Exact => observed
                .rows_as(&self.keys)
                .into_iter()
                .map(|(k, r)| seen(k, r))
                .collect(),
            DataMode::Ensure => self
                .keys
                .iter()
                .filter_map(|k| observed.row(k).map(|(_, r)| seen(k.clone(), r)))
                .collect(),
        };
        TableData {
            mode: self.mode,
            rows,
        }
    }
}

/// The read a connected plan needs: every table under either scope, fetched
/// widely enough to answer both.
///
/// Two scopes meet at plan time — what the environment's recorded state
/// covers, and what the declarations now cover — and they can disagree on a
/// table: a block added, removed, or switched between `exact` and `ensure`.
/// The drift check needs the recorded view, and the differ needs to see every
/// row the declaration will compare against, so the read is the union and each
/// caller projects its own view out of it.
pub fn read_scopes(recorded: &DataScopes, declared: &DataScopes) -> BTreeMap<TableName, RowScope> {
    let mut out: BTreeMap<TableName, RowScope> = BTreeMap::new();
    for (name, scope) in recorded.iter().chain(declared) {
        let read = scope.rows_to_read();
        let merged = match out.remove(name) {
            Some(existing) => existing.union(read),
            None => read,
        };
        out.insert(name.clone(), merged);
    }
    out
}

impl crate::schema::Schema {
    /// The scope of every table that declares rows.
    pub fn data_scopes(&self) -> DataScopes {
        self.tables
            .iter()
            .filter_map(|(n, t)| t.data.as_ref().map(|d| (n.clone(), DataScope::of(d))))
            .collect()
    }

    /// This schema — one read from a catalog, which declares no rows of its
    /// own — with the rows read back placed under `scopes`. Every table
    /// outside the scopes is left with `data: None`.
    ///
    /// A table in scope with no observed rows is left with `data: None`: the
    /// read did not reach it (it is missing from the database, and reported as
    /// such elsewhere), and "absent" must not be recorded as "empty".
    ///
    /// `reference` is the schema the scopes came from — the recorded snapshot
    /// for a drift check, the declarations for a rehearsal — because its rows
    /// say which cells that side spells explicitly ([`ObservedRow`]).
    ///
    /// Two keys `reference` spells that the engine calls one row is an error
    /// ([`RowConflict`]), never a choice between them — but `scope` is not
    /// always `reference`'s own: a connected plan pins projection to the union
    /// of the recorded and declared scopes, wider than either side alone
    /// (DECISIONS 98), and asking whether that union collides tests one side's
    /// spelling against the other's. `01` recorded and `1` declared alias to
    /// one row in the union and are refused, even though each side names it
    /// once — precisely what [`ObservedTable::conflict`]'s own doc says is not
    /// a conflict (issue #106).
    ///
    /// So the conflict test narrows to `reference`'s own keys for this table,
    /// falling back to `scope` only when `reference` has no `data` block for
    /// it at all. That fallback is safe, not a loophole, because of how a
    /// union scope is built ([`DataScope::union`], `pinned_scopes` in
    /// `pbps-cli`): a table's scope is ever a union of two sides' keys only
    /// where *both* sides declare rows for it. If `reference` — one of the two
    /// schemas the union was built from — declares nothing for this table,
    /// the union contributed nothing from `reference`'s side, so `scope` here
    /// already holds only the *other* side's keys. There is no second
    /// spelling to smuggle in, and testing `scope` directly is exactly the
    /// same question as testing `reference`'s own (empty) keys plus that other
    /// side's, i.e. asking `reference`'s own side about a table it never
    /// mentions is asking about a keyset that does not exist.
    ///
    /// This narrows only the *conflict test*; `scope.project` below still
    /// hands the unnarrowed `scope` to [`ObservedTable::rows_as`], which picks
    /// a canonical spelling among several aliasing to one row by sorting them
    /// (issue #107) rather than by which side asked. #107 stays open and
    /// stays exactly as broken after this change — fixing it is a different
    /// PR, against `rows_as` itself, not this narrowing.
    pub fn with_observed_rows(
        mut self,
        rows: &ObservedRows,
        scopes: &DataScopes,
        reference: &crate::schema::Schema,
    ) -> Result<Self, RowConflict> {
        for (name, table) in &mut self.tables {
            let own = reference.tables.get(name).and_then(|t| t.data.as_ref());
            table.data = match (scopes.get(name), rows.get(name)) {
                (Some(scope), Some(observed)) => {
                    let own_scope = own.map(DataScope::of);
                    let own_keys = own_scope.as_ref().map_or(&scope.keys, |s| &s.keys);
                    if let Some((first, second, canonical)) = observed.conflict(own_keys) {
                        return Err(RowConflict {
                            table: name.clone(),
                            first,
                            second,
                            canonical,
                        });
                    }
                    Some(scope.project(observed, own))
                }
                _ => None,
            };
        }
        Ok(self)
    }
}

/// The base a connected plan is computed against: the live schema, with the
/// rows read back for every table either side covers.
///
/// The **mode** is the recorded one where there is one, so that a switch
/// between `exact` and `ensure` shows at the gate as `SetDataMode`; a table the
/// declarations take over for the first time carries the declared mode, since
/// nothing was recorded to differ from. The **rows** are everything the union
/// read fetched, unfiltered: the differ must see every row the declaration
/// will be measured against — including, for an `exact` declaration, the rows
/// that are about to be deleted because nobody declared them.
///
/// Each cell is read as the **declarations** spell it ([`ObservedRow`]): the
/// base is what the declared rows are measured against, so a cell they write
/// explicitly is compared as a value and a cell they omit is compared as the
/// default. The recorded state's spelling is not consulted here; it belongs to
/// the drift check, which reads the same rows under its own reference.
pub fn plan_base(
    live: &crate::schema::Schema,
    rows: &ObservedRows,
    recorded: &DataScopes,
    declared: &crate::schema::Schema,
) -> Result<crate::schema::Schema, RowConflict> {
    let declared_scopes = declared.data_scopes();
    let mut base = live.clone();
    for (name, table) in &mut base.tables {
        let mode = recorded
            .get(name)
            .or_else(|| declared_scopes.get(name))
            .map(|s| s.mode);
        let own = declared.tables.get(name).and_then(|t| t.data.as_ref());
        let spelling = declared_scopes.get(name).map(|s| &s.keys);
        if let (Some(spelling), Some(observed)) = (spelling, rows.get(name))
            && let Some((first, second, canonical)) = observed.conflict(spelling)
        {
            return Err(RowConflict {
                table: name.clone(),
                first,
                second,
                canonical,
            });
        }
        table.data = match (mode, rows.get(name)) {
            (Some(mode), Some(observed)) => Some(TableData {
                mode,
                rows: observed
                    .rows_as(spelling.unwrap_or(&BTreeSet::new()))
                    .into_iter()
                    .map(|(k, r)| {
                        let row = r.as_seen_by(own.and_then(|d| d.rows.get(&k)));
                        (k, row)
                    })
                    .collect(),
            }),
            _ => None,
        };
    }
    Ok(base)
}

/// The order declared rows must be inserted in: a referenced table before the
/// table that references it.
///
/// Only tables that declare a `data:` block appear, but the edges are read from
/// **every** declared foreign key, including ones pointing at tables with no
/// block of their own — a chain `a -> b -> c` where only `a` and `c` hold rows
/// still means `c` first.
///
/// This exists because the same bug has already been paid for once at table
/// granularity: a foreign key between two new tables sorts after both creates,
/// and only a live test found it. Rows have exactly that shape, one level down,
/// and neither the unit suite nor a careful reading of a plan would notice a
/// `sub_status` inserted before the `status` it points at.
///
/// Deletes run in the reverse of this order, for the mirror-image reason.
pub fn insertion_order(schema: &crate::schema::Schema) -> Vec<TableName> {
    let names: Vec<TableName> = schema
        .tables
        .iter()
        .filter(|(_, t)| t.data.is_some())
        .map(|(n, _)| n.clone())
        .collect();

    // Transitive, so a table between two data tables does not break the chain.
    let mut needs: BTreeMap<&TableName, BTreeSet<TableName>> = BTreeMap::new();
    for name in &names {
        let mut reached = BTreeSet::new();
        let mut queue = vec![name.clone()];
        while let Some(current) = queue.pop() {
            let Some(table) = schema.tables.get(&current) else {
                continue;
            };
            for fk in table.foreign_keys.values() {
                // A self-reference is a row-order problem inside one table,
                // which this ordering cannot express and the engine reports.
                if fk.references_table == *name || !reached.insert(fk.references_table.clone()) {
                    continue;
                }
                queue.push(fk.references_table.clone());
            }
        }
        needs.insert(name, reached);
    }

    // Kahn's algorithm, taking the name-least ready table each round so two
    // runs over the same declarations produce the same plan.
    let mut done: BTreeSet<TableName> = BTreeSet::new();
    let mut out: Vec<TableName> = Vec::new();
    loop {
        let ready: Vec<&TableName> = names
            .iter()
            .filter(|n| !done.contains(*n))
            .filter(|n| {
                needs[*n]
                    .iter()
                    // Only the tables being ordered constrain the order; an
                    // edge to a table with no `data:` block is not a wait.
                    .filter(|d| names.contains(d))
                    .all(|d| done.contains(d))
            })
            .collect();
        if ready.is_empty() {
            break;
        }
        for n in ready {
            done.insert(n.clone());
            out.push(n.clone());
        }
    }
    // A cycle between two lookup tables: deterministic order, and the engine
    // decides — the same answer `creation_order` gives.
    for n in &names {
        if !done.contains(n) {
            out.push(n.clone());
        }
    }
    out
}

/// The default above which a `data:` block stops looking like reference data
/// (ADR-0004). Reference rows are part of the program; a thousand of them are
/// somebody's business table, and putting it under a declaration means every
/// plan compares it row by row.
pub const DEFAULT_MAX_ROWS: usize = 1000;

/// What a `data:` block must satisfy before anything is generated from it.
///
/// These are rules of the *model*, not of any engine — which is why they live
/// here rather than in a dialect's `validate`: the primary key is the row's
/// identity in PostgreSQL exactly as it is in SQL Server. `validate` reports
/// them, so the user sees the file rather than a constraint violation halfway
/// through an apply.
///
/// Returns one message per problem, all of them, in a stable order.
pub fn check(name: &TableName, table: &Table) -> Vec<String> {
    let Some(data) = &table.data else {
        return Vec::new();
    };
    let mut problems = Vec::new();

    // The key *is* the identity (constraint 2 at row granularity), so without a
    // primary key there is nothing for the map key to mean, and the differ
    // could not tell an update from a delete plus an insert.
    let key_column = match &table.primary_key {
        None => {
            problems.push(format!(
                "{name}: a `data:` block needs a primary key — the row keys are primary-key values"
            ));
            None
        }
        Some(pk) if pk.columns.len() == 1 => Some(pk.columns[0].clone()),
        Some(pk) => {
            // Deferred, not impossible: a composite key needs a written form
            // for a tuple, and inventing one that later has to change is worse
            // than saying so.
            problems.push(format!(
                "{name}: a `data:` block needs a single-column primary key; this one has {} \
                 ({}). Composite keys are not supported yet",
                pk.columns.len(),
                pk.columns.join(", ")
            ));
            None
        }
    };

    for (key, row) in &data.rows {
        for column in row.0.keys() {
            if !table.columns.contains_key(column) {
                problems.push(format!(
                    "{name}: row `{key}` sets `{column}`, which the table does not declare"
                ));
            }
            // The key is the one identity column a row may pin, and it pins it
            // through the map key with `SET IDENTITY_INSERT` around the
            // insert. Any other identity column is the engine's to assign;
            // a value for it would be refused at apply time, after `validate`
            // had said the declaration was fine.
            if table
                .columns
                .get(column)
                .is_some_and(|c| c.identity.is_some())
                && key_column.as_deref() != Some(column.as_str())
            {
                problems.push(format!(
                    "{name}: row `{key}` sets `{column}`, which is an IDENTITY column the engine \
                     assigns — leave it out"
                ));
            }
            if key_column.as_deref() == Some(column.as_str()) {
                // Not a style rule. If the body could restate the key, it could
                // disagree with it, and there would be two answers to "which
                // row is this" — the bug map keys exist to make unwritable.
                problems.push(format!(
                    "{name}: row `{key}` sets `{column}`, which is the primary key — the row's key \
                     is already its value"
                ));
            }
        }

        // The two spellings of "nothing here" are not the same statement, and
        // the engine treats them differently (see [`Row`]). An omitted column
        // is left out of the INSERT, so the default fills it; an explicit
        // `null` is *sent*, and a default never applies to a value that was
        // sent. So a `NOT NULL` column refuses an explicit null whatever its
        // default, and refuses omission only when nothing would fill it.
        for (column, spec) in &table.columns {
            if Some(column.as_str()) == key_column.as_deref() || spec.nullable {
                continue;
            }
            match row.0.get(column) {
                Some(Value::Null) => problems.push(format!(
                    "{name}: row `{key}` sets `{column}` to null, but it is NOT NULL{}",
                    if spec.default.is_some() {
                        " — leave it out to get the default"
                    } else {
                        ""
                    }
                )),
                None if spec.default.is_none() && spec.identity.is_none() => {
                    problems.push(format!(
                        "{name}: row `{key}` leaves `{column}` unset, but it is NOT NULL with no default"
                    ));
                }
                _ => {}
            }
        }
    }

    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::schema::{Column, PrimaryKey};
    use crate::types::ColumnType;
    use std::str::FromStr;

    fn table(pk: Option<Vec<&str>>, rows: Vec<(&str, Vec<(&str, Value)>)>) -> Table {
        let mut columns = indexmap::IndexMap::new();
        columns.insert(
            "code".to_owned(),
            Column::new(ColumnType::from_str("varchar(20)").unwrap()).not_null(),
        );
        columns.insert(
            "label".to_owned(),
            Column::new(ColumnType::from_str("nvarchar(50)").unwrap()).not_null(),
        );
        Table {
            columns,
            primary_key: pk.map(|c| PrimaryKey {
                name: None,
                columns: c.into_iter().map(str::to_owned).collect(),
            }),
            data: Some(TableData {
                mode: DataMode::Exact,
                rows: rows
                    .into_iter()
                    .map(|(k, cells)| {
                        (
                            RowKey::from(k),
                            cells
                                .into_iter()
                                .map(|(c, v)| (c.to_owned(), v))
                                .collect::<Row>(),
                        )
                    })
                    .collect(),
            }),
            ..Table::default()
        }
    }

    fn name() -> TableName {
        TableName::from_str("dbo.order_status").unwrap()
    }

    #[test]
    fn a_declared_row_that_satisfies_every_rule_reports_nothing() {
        let t = table(
            Some(vec!["code"]),
            vec![("new", vec![("label", Value::Text("New".to_owned()))])],
        );
        assert_eq!(check(&name(), &t), Vec::<String>::new());
    }

    #[test]
    fn a_table_with_no_data_block_is_never_checked() {
        let t = Table::default();
        assert!(check(&name(), &t).is_empty());
    }

    #[test]
    fn rows_without_a_primary_key_have_no_identity() {
        let t = table(
            None,
            vec![("new", vec![("label", Value::Text("New".into()))])],
        );
        let p = check(&name(), &t);
        assert!(p.iter().any(|m| m.contains("needs a primary key")), "{p:?}");
    }

    #[test]
    fn a_composite_key_is_refused_by_name_rather_than_silently_ignored() {
        let t = table(
            Some(vec!["code", "label"]),
            vec![("new", vec![("label", Value::Text("New".into()))])],
        );
        let p = check(&name(), &t);
        assert!(
            p.iter().any(|m| m.contains("single-column primary key")),
            "{p:?}"
        );
    }

    #[test]
    fn a_row_naming_a_column_the_table_does_not_have_is_refused() {
        let t = table(
            Some(vec!["code"]),
            vec![(
                "new",
                vec![
                    ("label", Value::Text("New".into())),
                    ("labl", Value::Text("New".into())),
                ],
            )],
        );
        let p = check(&name(), &t);
        assert!(p.iter().any(|m| m.contains("`labl`")), "{p:?}");
    }

    #[test]
    fn a_row_restating_its_own_key_is_refused() {
        let t = table(
            Some(vec!["code"]),
            vec![(
                "new",
                vec![
                    ("code", Value::Text("new".into())),
                    ("label", Value::Text("New".into())),
                ],
            )],
        );
        let p = check(&name(), &t);
        assert!(p.iter().any(|m| m.contains("is the primary key")), "{p:?}");
    }

    #[test]
    fn a_row_leaving_a_not_null_column_unset_is_refused_either_way_it_is_written() {
        // Omitted...
        let t = table(Some(vec!["code"]), vec![("new", vec![])]);
        let p = check(&name(), &t);
        assert!(p.iter().any(|m| m.contains("`label` unset")), "{p:?}");

        // ...and explicitly null, which is a *different* statement (it is
        // sent, so no default can fill it) and would otherwise slip past a
        // check that only looked for absence.
        let t = table(
            Some(vec!["code"]),
            vec![("new", vec![("label", Value::Null)])],
        );
        let p = check(&name(), &t);
        assert!(p.iter().any(|m| m.contains("`label` to null")), "{p:?}");
    }

    /// The second review's P2: an explicit `null` is *sent*, and a default
    /// never applies to a value that was sent — so this row fails at apply
    /// time on a `NOT NULL` column even though the column has a default.
    #[test]
    fn an_explicit_null_on_a_not_null_column_is_refused_even_with_a_default() {
        let mut t = table(
            Some(vec!["code"]),
            vec![("new", vec![("label", Value::Null)])],
        );
        t.columns.get_mut("label").unwrap().default = Some("''".to_owned());
        let p = check(&name(), &t);
        assert!(p.iter().any(|m| m.contains("to null")), "{p:?}");
        assert!(
            p.iter().any(|m| m.contains("leave it out")),
            "the remedy must be named: {p:?}"
        );
    }

    #[test]
    fn an_omitted_column_resolves_to_its_default_and_otherwise_to_null() {
        let with_default = {
            let mut c = Column::new(ColumnType::from_str("int").unwrap());
            c.default = Some("0".to_owned());
            c
        };
        let without = Column::new(ColumnType::from_str("int").unwrap());
        let row = Row::default();
        assert_eq!(
            cell(&row, "n", Some(&with_default)),
            Cell::Default("0".to_owned())
        );
        assert_eq!(cell(&row, "n", Some(&without)), Cell::Value(Value::Null));
        assert_eq!(cell(&row, "n", None), Cell::Value(Value::Null));
        // And a written value is that value, default or no default: the
        // default fills only what was left out.
        let written: Row = [("n".to_owned(), Value::Int(7))].into_iter().collect();
        assert_eq!(
            cell(&written, "n", Some(&with_default)),
            Cell::Value(Value::Int(7))
        );
    }

    /// The fourth review's shape, one step over: only the key may pin an
    /// identity value, and it does so through the map key. A row writing a
    /// non-key IDENTITY column is refused here rather than by the engine.
    #[test]
    fn a_row_writing_a_non_key_identity_column_is_refused() {
        let mut t = table(
            Some(vec!["code"]),
            vec![(
                "new",
                vec![("label", Value::Text("New".into())), ("seq", Value::Int(1))],
            )],
        );
        let mut seq = Column::new(ColumnType::from_str("int").unwrap()).not_null();
        seq.identity = Some(crate::schema::Identity {
            seed: 1,
            increment: 1,
        });
        t.columns.insert("seq".to_owned(), seq);
        let p = check(&name(), &t);
        assert!(p.iter().any(|m| m.contains("IDENTITY")), "{p:?}");
        // The negative case: omitting it is fine, identity or not.
        t.data.as_mut().unwrap().rows = [(
            RowKey::from("new"),
            [("label".to_owned(), Value::Text("New".into()))]
                .into_iter()
                .collect(),
        )]
        .into_iter()
        .collect();
        assert_eq!(check(&name(), &t), Vec::<String>::new());
    }

    #[test]
    fn a_not_null_column_with_a_default_may_be_omitted() {
        // The negative case for the rule above: the value exists, it just is
        // not written here.
        let mut t = table(Some(vec!["code"]), vec![("new", vec![])]);
        t.columns.get_mut("label").unwrap().default = Some("''".to_owned());
        assert_eq!(check(&name(), &t), Vec::<String>::new());
    }

    #[test]
    fn a_row_omitting_a_column_is_not_equal_to_one_declaring_it_null() {
        // The two mean the same thing *for a nullable column with no default*,
        // and the differ resolves them against the table before comparing. The
        // model does not, on purpose: a `Row` that guessed would need the table
        // to do it, and elements do not reach their container (constraint 2).
        let omitted = Row::default();
        let explicit: Row = [("label".to_owned(), Value::Null)].into_iter().collect();
        assert_ne!(omitted, explicit);
    }

    #[test]
    fn an_integer_and_the_same_digits_as_text_are_different_values() {
        // The negative case for "it is all text underneath". They render
        // differently — `1` against `'1'` — so a declaration that meant one
        // must never compare equal to one that meant the other.
        assert_ne!(Value::Int(1), Value::Text("1".to_owned()));
    }

    #[test]
    fn rows_serialize_in_key_order_whatever_order_they_arrived_in() {
        let a: TableData = TableData {
            mode: DataMode::Exact,
            rows: [
                (RowKey::from("shipped"), Row::default()),
                (RowKey::from("cancelled"), Row::default()),
            ]
            .into_iter()
            .collect(),
        };
        let b: TableData = TableData {
            mode: DataMode::Exact,
            rows: [
                (RowKey::from("cancelled"), Row::default()),
                (RowKey::from("shipped"), Row::default()),
            ]
            .into_iter()
            .collect(),
        };
        assert_eq!(a, b);
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }

    #[test]
    fn a_mode_change_alone_is_a_change() {
        // The negative case for "modes are just a comment": `ensure` -> `exact`
        // turns every undeclared row into a delete, so the two cannot compare
        // equal.
        let exact = TableData {
            mode: DataMode::Exact,
            rows: BTreeMap::new(),
        };
        let ensure = TableData {
            mode: DataMode::Ensure,
            rows: BTreeMap::new(),
        };
        assert_ne!(exact, ensure);
    }

    // ---- The connected half: scopes and read-back ----

    fn rows(keys: &[&str]) -> BTreeMap<RowKey, Row> {
        keys.iter()
            .map(|k| (RowKey::from(*k), Row::default()))
            .collect()
    }

    fn observed(keys: &[&str]) -> ObservedTable {
        ObservedTable {
            rows: keys
                .iter()
                .map(|k| (RowKey::from(*k), ObservedRow::default()))
                .collect(),
            aliases: BTreeMap::new(),
        }
    }

    /// The engine spells an `int` key `1`; the declaration wrote `01`. The
    /// engine said they are the same row, and each side reads it back under
    /// its own spelling — an `exact` block, an `ensure` block, and `pull`.
    #[test]
    fn a_declared_key_comes_back_in_the_declared_spelling() {
        let mut seen = observed(&["1", "2"]);
        seen.aliases.insert(RowKey::from("01"), RowKey::from("1"));
        let keys = |ks: &[&str]| ks.iter().map(|k| RowKey::from(*k)).collect::<BTreeSet<_>>();
        assert_eq!(seen.row(&RowKey::from("01")).unwrap().0, &RowKey::from("1"));
        assert_eq!(seen.row(&RowKey::from("2")).unwrap().0, &RowKey::from("2"));
        assert!(seen.row(&RowKey::from("3")).is_none());

        let exact = DataScope {
            mode: DataMode::Exact,
            keys: keys(&["01"]),
        };
        let names: Vec<String> = exact
            .project(&seen, None)
            .rows
            .keys()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            names,
            ["01", "2"],
            "declared spelling where declared, the engine's elsewhere"
        );
        let ensure = DataScope {
            mode: DataMode::Ensure,
            keys: keys(&["01"]),
        };
        let names: Vec<String> = ensure
            .project(&seen, None)
            .rows
            .keys()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            names,
            ["01"],
            "found through the alias, kept under the declared key"
        );
        let pull = DataScope {
            mode: DataMode::Exact,
            keys: BTreeSet::new(),
        };
        let names: Vec<String> = pull
            .project(&seen, None)
            .rows
            .keys()
            .map(ToString::to_string)
            .collect();
        assert_eq!(names, ["1", "2"], "nothing to spell: the engine's");
    }

    /// `1` and `01` declared side by side name one row to the engine; keeping
    /// either would plan an insert of the other, which the primary key then
    /// refuses at apply. Refused here instead, by both names.
    #[test]
    fn two_spellings_of_one_row_are_refused_not_reconciled() {
        let mut seen = observed(&["1"]);
        seen.aliases.insert(RowKey::from("01"), RowKey::from("1"));
        seen.aliases.insert(RowKey::from("1"), RowKey::from("1"));
        let both: BTreeSet<RowKey> = ["01", "1"].into_iter().map(RowKey::from).collect();
        assert_eq!(
            seen.conflict(&both),
            Some((RowKey::from("01"), RowKey::from("1"), RowKey::from("1")))
        );
        // One side spelling `01` and the other `1` is not a conflict: each
        // side is asked about its own keys.
        let one: BTreeSet<RowKey> = ["01"].into_iter().map(RowKey::from).collect();
        assert_eq!(seen.conflict(&one), None);

        let mut schema = crate::schema::Schema::default();
        schema.tables.insert(
            name(),
            table(Some(vec!["code"]), vec![("01", vec![]), ("1", vec![])]),
        );
        let scopes = schema.data_scopes();
        let mut live = crate::schema::Schema::default();
        live.tables
            .insert(name(), table(Some(vec!["code"]), vec![]));
        let observed: ObservedRows = [(name(), seen)].into_iter().collect();
        let err = live
            .clone()
            .with_observed_rows(&observed, &scopes, &schema)
            .unwrap_err();
        assert_eq!(err.first, RowKey::from("01"));
        assert_eq!(err.second, RowKey::from("1"));
        assert!(err.to_string().contains("declare it once"), "{err}");
        let err = plan_base(&live, &observed, &BTreeMap::new(), &schema).unwrap_err();
        assert_eq!(err.canonical, RowKey::from("1"));
    }

    /// A connected plan's baseline is pinned to the *union* of the recorded
    /// and declared scopes, wider than either side alone (DECISIONS 98). One
    /// side spelling `1` and the other `01` is still not a conflict there: the
    /// rule is "each side is asked about its own keys" (the test above), and
    /// pinning to a union must not smuggle the other side's spelling into that
    /// question (issue #106).
    #[test]
    fn one_spelling_per_side_in_a_pinned_union_is_not_a_conflict() {
        let mut seen = observed(&["1"]);
        seen.aliases.insert(RowKey::from("01"), RowKey::from("1"));
        seen.aliases.insert(RowKey::from("1"), RowKey::from("1"));

        let mut recorded = crate::schema::Schema::default();
        recorded
            .tables
            .insert(name(), table(Some(vec!["code"]), vec![("1", vec![])]));
        let mut declared = crate::schema::Schema::default();
        declared
            .tables
            .insert(name(), table(Some(vec!["code"]), vec![("01", vec![])]));

        let recorded_scope = recorded.data_scopes().remove(&name()).unwrap();
        let declared_scope = declared.data_scopes().remove(&name()).unwrap();
        // The union a connected plan is pinned under (`pinned_scopes` in
        // `pbps-cli`), rebuilt here with `DataScope::union` directly so the
        // test does not have to cross the crate boundary: both spellings are
        // in scope, exactly as they would be for the plan's baseline.
        let pinned = recorded_scope.union(declared_scope);
        assert_eq!(
            pinned.keys,
            ["01", "1"].into_iter().map(RowKey::from).collect()
        );
        // The union itself *does* alias both spellings to one row — the shape
        // `with_observed_rows` must not hand to `conflict` directly, or it
        // reproduces #106's false refusal from `plan --db`.
        assert!(seen.conflict(&pinned.keys).is_some());
        let scopes: DataScopes = [(name(), pinned)].into_iter().collect();

        let mut live = crate::schema::Schema::default();
        live.tables
            .insert(name(), table(Some(vec!["code"]), vec![]));
        let observed_rows: ObservedRows = [(name(), seen)].into_iter().collect();

        // Each side asked about its own keys alone passes, whichever side is
        // `reference` — reverting the narrowing in `with_observed_rows` to
        // test the pinned scope directly makes both of these fail with
        // `RowConflict` instead.
        live.clone()
            .with_observed_rows(&observed_rows, &scopes, &recorded)
            .expect("recorded's own spelling `1` alone is not a conflict");
        live.with_observed_rows(&observed_rows, &scopes, &declared)
            .expect("declared's own spelling `01` alone is not a conflict");
    }

    /// The catalog cannot tell `label: Unlabelled` from an omitted `label`
    /// when the default is `'Unlabelled'`; the side that reads the row can,
    /// and each side must see its own spelling back — or one of the two
    /// would be restated on every connected plan.
    #[test]
    fn a_cell_at_its_default_is_explicit_only_where_the_side_spells_it() {
        let text = |s: &str| Value::Text(s.to_owned());
        let seen = ObservedRow {
            cells: [
                ("label".to_owned(), text("Unlabelled")),
                ("rank".to_owned(), Value::Int(1)),
            ]
            .into_iter()
            .collect(),
            at_default: ["label".to_owned()].into_iter().collect(),
            unknown: BTreeSet::new(),
        };
        // The side omits `label`: omitted. The side writes it: explicit. No
        // side at all (`pull`): omitted. `rank` is not at its default and is
        // explicit every time.
        let omits: Row = [("rank".to_owned(), Value::Int(1))].into_iter().collect();
        assert_eq!(seen.as_seen_by(Some(&omits)), omits);
        let writes: Row = [
            ("label".to_owned(), text("Unlabelled")),
            ("rank".to_owned(), Value::Int(1)),
        ]
        .into_iter()
        .collect();
        assert_eq!(seen.as_seen_by(Some(&writes)), writes);
        assert_eq!(seen.as_seen_by(None), omits);
        // A side that writes a *different* value still sees what is stored:
        // the hand edit that set `label` back to its default is a drift.
        let other: Row = [("label".to_owned(), text("Other"))].into_iter().collect();
        assert_eq!(seen.as_seen_by(Some(&other)), writes);
    }

    /// A default the engine was never asked about (`NEWID()`, `GETDATE()`)
    /// leaves the cell's provenance unknown. The side's own row decides;
    /// with no row — `pull` — the value is kept, because a generated key
    /// is a value the block has to carry, not a default it can be rebuilt
    /// from. The first cut dropped every such cell from a pulled block.
    #[test]
    fn a_cell_of_unknown_provenance_is_kept_unless_the_side_omits_it() {
        let text = |s: &str| Value::Text(s.to_owned());
        let seen = ObservedRow {
            cells: [
                ("stamp".to_owned(), text("2026-09-03T10:00:00")),
                ("rank".to_owned(), Value::Int(1)),
            ]
            .into_iter()
            .collect(),
            at_default: BTreeSet::new(),
            unknown: ["stamp".to_owned()].into_iter().collect(),
        };
        let omits: Row = [("rank".to_owned(), Value::Int(1))].into_iter().collect();
        assert_eq!(
            seen.as_seen_by(Some(&omits)),
            omits,
            "the declaration's word"
        );
        let writes: Row = [
            ("stamp".to_owned(), text("2026-09-03T10:00:00")),
            ("rank".to_owned(), Value::Int(1)),
        ]
        .into_iter()
        .collect();
        assert_eq!(seen.as_seen_by(Some(&writes)), writes);
        assert_eq!(seen.as_seen_by(None), writes, "pull keeps the value");
    }

    #[test]
    fn an_exact_scope_reads_every_row_and_an_ensure_scope_only_its_keys() {
        let exact = DataScope {
            mode: DataMode::Exact,
            keys: ["a"].into_iter().map(RowKey::from).collect(),
        };
        assert_eq!(
            exact.rows_to_read(),
            RowScope::Every {
                known: ["a"].into_iter().map(RowKey::from).collect()
            }
        );
        let ensure = DataScope {
            mode: DataMode::Ensure,
            keys: ["a"].into_iter().map(RowKey::from).collect(),
        };
        assert_eq!(
            ensure.rows_to_read(),
            RowScope::Keys(["a"].into_iter().map(RowKey::from).collect())
        );
    }

    /// The promise `ensure` makes: a read that fetched more than the declared
    /// keys (because another scope on the table needed them) must not record
    /// the application's own rows, or the next `verify` calls them drift.
    #[test]
    fn projecting_an_ensure_scope_drops_the_rows_it_did_not_declare() {
        let ensure = DataScope {
            mode: DataMode::Ensure,
            keys: ["a"].into_iter().map(RowKey::from).collect(),
        };
        let observed = observed(&["a", "b"]);
        assert_eq!(ensure.project(&observed, None).rows, rows(&["a"]));
        // The negative case: `exact` keeps the undeclared row, which is
        // exactly the row the differ has to delete.
        let exact = DataScope {
            mode: DataMode::Exact,
            keys: ["a"].into_iter().map(RowKey::from).collect(),
        };
        assert_eq!(exact.project(&observed, None).rows, rows(&["a", "b"]));
    }

    #[test]
    fn the_union_read_is_wide_enough_for_both_scopes() {
        let t = name();
        let ensure = |keys: &[&str]| DataScope {
            mode: DataMode::Ensure,
            keys: keys.iter().map(|k| RowKey::from(*k)).collect(),
        };
        let exact = DataScope {
            mode: DataMode::Exact,
            keys: BTreeSet::new(),
        };
        // ensure + ensure: the keys of both.
        let read = read_scopes(
            &[(t.clone(), ensure(&["a"]))].into_iter().collect(),
            &[(t.clone(), ensure(&["b"]))].into_iter().collect(),
        );
        assert_eq!(
            read[&t],
            RowScope::Keys(["a", "b"].into_iter().map(RowKey::from).collect())
        );
        // ensure + exact, either way round: everything, spelling the keys
        // of both sides.
        let every = RowScope::Every {
            known: ["a"].into_iter().map(RowKey::from).collect(),
        };
        let read = read_scopes(
            &[(t.clone(), ensure(&["a"]))].into_iter().collect(),
            &[(t.clone(), exact.clone())].into_iter().collect(),
        );
        assert_eq!(read[&t], every);
        let read = read_scopes(
            &[(t.clone(), exact)].into_iter().collect(),
            &[(t.clone(), ensure(&["a"]))].into_iter().collect(),
        );
        assert_eq!(read[&t], every);
        // A table on one side only is still read.
        let read = read_scopes(
            &BTreeMap::new(),
            &[(t.clone(), ensure(&["a"]))].into_iter().collect(),
        );
        assert!(read.contains_key(&t));
    }

    /// Absent, empty and unread are three different things: a scoped table the
    /// read did not reach stays `None`, never becomes "declares no rows".
    #[test]
    fn a_scoped_table_with_no_observed_rows_is_not_recorded_as_empty() {
        let mut schema = crate::schema::Schema::default();
        schema
            .tables
            .insert(name(), table(Some(vec!["code"]), vec![]));
        let scopes: DataScopes = [(
            name(),
            DataScope {
                mode: DataMode::Exact,
                keys: BTreeSet::new(),
            },
        )]
        .into_iter()
        .collect();
        let none = crate::schema::Schema::default();
        let unread = schema
            .clone()
            .with_observed_rows(&BTreeMap::new(), &scopes, &none)
            .unwrap();
        assert_eq!(unread.tables[&name()].data, None);
        let read = schema
            .with_observed_rows(
                &[(name(), ObservedTable::default())].into_iter().collect(),
                &scopes,
                &none,
            )
            .unwrap();
        assert_eq!(
            read.tables[&name()].data,
            Some(TableData {
                mode: DataMode::Exact,
                rows: BTreeMap::new()
            })
        );
    }

    /// The base a connected plan sees: the recorded mode where there is one,
    /// so a mode switch reaches the gate, and every row the read fetched.
    #[test]
    fn the_plan_base_keeps_the_recorded_mode_and_every_observed_row() {
        let mut live = crate::schema::Schema::default();
        live.tables
            .insert(name(), table(Some(vec!["code"]), vec![]));
        let observed: ObservedRows = [(name(), observed(&["a", "b"]))].into_iter().collect();
        let recorded: DataScopes = [(
            name(),
            DataScope {
                mode: DataMode::Ensure,
                keys: ["a"].into_iter().map(RowKey::from).collect(),
            },
        )]
        .into_iter()
        .collect();
        let mut declared = crate::schema::Schema::default();
        declared
            .tables
            .insert(name(), table(Some(vec!["code"]), vec![("a", vec![])]));
        let base = plan_base(&live, &observed, &recorded, &declared).unwrap();
        let data = base.tables[&name()].data.as_ref().unwrap();
        assert_eq!(
            data.mode,
            DataMode::Ensure,
            "the recorded mode, so the switch shows"
        );
        assert_eq!(
            data.rows,
            rows(&["a", "b"]),
            "unfiltered: `b` is what exact will delete"
        );
        // Taken over for the first time: the declared mode, since nothing was
        // recorded to differ from.
        let base = plan_base(&live, &observed, &BTreeMap::new(), &declared).unwrap();
        assert_eq!(
            base.tables[&name()].data.as_ref().unwrap().mode,
            DataMode::Exact
        );
        // Neither side covers it: untouched.
        let base = plan_base(
            &live,
            &observed,
            &BTreeMap::new(),
            &crate::schema::Schema::default(),
        )
        .unwrap();
        assert_eq!(base.tables[&name()].data, None);
    }
}
