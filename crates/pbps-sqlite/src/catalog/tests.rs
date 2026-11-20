//! Tests of the reader (#2950, item 4): `read_raw`, `assemble` and
//! `introspect` against a real SQLite. The tokenizer's own corpus is the input
//! (`text::tests::corpus`), the engine is the oracle, and what is read is
//! checked in both directions: what a file says reads as the model, and what
//! the model says, emitted into a fresh database, reads back as itself.

use pbps_db::catalog::{LimitationTarget, Pulled};
use pbps_model::{ColumnType, Schema, Table, TableName};

use super::introspect;
use crate::tests::Fresh;

mod environment;
mod reader;
mod refusals;
mod roundtrip;
mod unnamed;

/// What the reader makes of the database `fresh` holds.
pub(super) fn read(fresh: &mut Fresh) -> Pulled {
    introspect(fresh.connection()).expect("the catalog reads")
}

/// A fresh database made by `sql`, read.
pub(super) fn read_sql(sql: &str) -> (Fresh, Pulled) {
    let mut fresh = Fresh::memory();
    fresh
        .connection()
        .execute_batch(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let pulled = read(&mut fresh);
    (fresh, pulled)
}

/// The schema with every name the reader gave an unnamed constraint replaced
/// by one a declaration may carry, in order, so it can be emitted and read
/// again: the names of what it declares are then the names it reads back under.
pub(super) fn declarable(schema: &Schema) -> Schema {
    use crate::names::is_synthetic;
    let mut out = schema.clone();
    for table in out.tables.values_mut() {
        macro_rules! rename {
            ($map:expr, $kind:literal) => {{
                let old = std::mem::take(&mut $map);
                for (n, (key, value)) in old.into_iter().enumerate() {
                    let key = if is_synthetic(key.raw()) {
                        format!("named_{}_{n}", $kind).into()
                    } else {
                        key
                    };
                    $map.insert(key, value);
                }
            }};
        }
        rename!(table.checks, "check");
        rename!(table.unique, "unique");
        rename!(table.foreign_keys, "foreign");
    }
    out
}

/// The table `name` of `main`, which the read must hold.
pub(super) fn table<'a>(pulled: &'a Pulled, name: &str) -> &'a Table {
    pulled
        .schema
        .tables
        .get(&TableName::new("main", name))
        .unwrap_or_else(|| panic!("no table `{name}` was read: {:#?}", pulled.warnings))
}

/// Whether a warning says `needle`.
pub(super) fn warned(pulled: &Pulled, needle: &str) -> bool {
    pulled.warnings.iter().any(|w| w.contains(needle))
}

/// The limitations recorded on the relation `name` of `main`.
pub(super) fn limits<'a>(pulled: &'a Pulled, name: &str) -> Vec<&'a str> {
    let target = LimitationTarget::Relation(TableName::new("main", name));
    pulled
        .limitations
        .iter()
        .filter(|l| l.target == target)
        .map(|l| l.detail.as_str())
        .collect()
}

/// A declared type as the reader holds it.
pub(super) fn ty(declared: &str) -> ColumnType {
    crate::types::read_back(declared).expect("a type")
}
