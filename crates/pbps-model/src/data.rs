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
}
