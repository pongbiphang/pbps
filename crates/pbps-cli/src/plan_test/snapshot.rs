//! What a re-pin compares: every managed table's rows and every allocator's
//! next value, read from one comparison database and held in memory
//! (ADR-0019 §5). Equality is decided on everything; only the report is bounded.

use std::collections::{BTreeMap, BTreeSet};

use pbps_model::Schema;

use super::report::{AllocatorComparison, Cell, RowDifference, TableComparison};
use super::session::Session;

/// Differences shown per table; this limit never decides equality.
const SHOWN: usize = 20;
/// Characters of one value shown in a difference.
const VALUE: usize = 200;

/// One table's rows as a multiset. Columns are sorted by name: a column added
/// by a later hop sits last in a replayed table and in declared order in a
/// bootstrapped one, and neither order is a difference in the data.
pub(crate) struct Rows {
    pub(super) columns: Vec<String>,
    pub(super) rows: BTreeMap<Vec<Option<String>>, u64>,
}

/// An allocator is compared under the column that owns it, else under its name:
/// a renamed table keeps its sequence's old generated name, while a bootstrap
/// of the new revision generates a new one, and neither is a difference.
pub(crate) struct Allocator {
    pub(super) name: String,
    pub(super) used_by: Option<String>,
    pub(super) next: String,
}

pub(crate) struct Snapshot {
    pub(super) tables: BTreeMap<String, Rows>,
    pub(super) allocators: BTreeMap<String, Allocator>,
}

/// The comparison database's rows and allocators, read by the run's engine
/// (see [`super::session::TestEngine::snapshot`]).
pub(super) async fn read(session: &Session<'_>, schema: &Schema) -> anyhow::Result<Snapshot> {
    session.snapshot(schema).await
}

/// Two allocators owned by one column are told apart by name.
pub(super) fn insert(allocators: &mut BTreeMap<String, Allocator>, allocator: Allocator) {
    let key = allocator
        .used_by
        .clone()
        .filter(|owner| !allocators.contains_key(owner))
        .unwrap_or_else(|| allocator.name.clone());
    allocators.insert(key, allocator);
}

fn cell(value: &Option<String>) -> Cell {
    match value {
        None => Cell::Null,
        Some(v) if v.chars().count() > VALUE => Cell::Text {
            value: v.chars().take(VALUE).collect(),
            truncated: true,
        },
        Some(v) => Cell::Text {
            value: v.clone(),
            truncated: false,
        },
    }
}

fn total(rows: &Rows) -> u64 {
    rows.rows.values().sum()
}

/// Rows and allocators of the two sides, with every difference found. A table
/// or sequence only one side has is a difference, never skipped.
pub(super) async fn compare(
    original: &Snapshot,
    proposed: &Snapshot,
) -> (Vec<TableComparison>, Vec<AllocatorComparison>) {
    let names: BTreeSet<&String> = original
        .tables
        .keys()
        .chain(proposed.tables.keys())
        .collect();
    let mut tables = Vec::new();
    for name in names {
        let (a, b) = (original.tables.get(name), proposed.tables.get(name));
        let empty = BTreeMap::new();
        let (ar, br) = (a.map_or(&empty, |t| &t.rows), b.map_or(&empty, |t| &t.rows));
        let mut differences = Vec::new();
        let mut truncated = false;
        for row in ar.keys().chain(br.keys()).collect::<BTreeSet<_>>() {
            let (x, y) = (
                ar.get(row).copied().unwrap_or(0),
                br.get(row).copied().unwrap_or(0),
            );
            if x == y {
                continue;
            }
            if differences.len() == SHOWN {
                truncated = true;
                break;
            }
            differences.push(RowDifference {
                original_count: x,
                proposed_count: y,
                row: row.iter().map(cell).collect(),
            });
        }
        let columns = |t: Option<&Rows>| t.map(|t| t.columns.clone()).unwrap_or_default();
        let same_columns = a.map(|t| &t.columns) == b.map(|t| &t.columns);
        tables.push(TableComparison {
            table: name.clone(),
            equal: same_columns && ar == br,
            original_columns: columns(a),
            proposed_columns: columns(b),
            original_rows: a.map_or(0, total),
            proposed_rows: b.map_or(0, total),
            differences,
            truncated,
        });
        // Give the run's Control a chance to observe its deadline and signal,
        // including after the last table, before a match can be published.
        tokio::task::yield_now().await;
    }
    let names: BTreeSet<&String> = original
        .allocators
        .keys()
        .chain(proposed.allocators.keys())
        .collect();
    let mut allocators = Vec::new();
    for name in names {
        let (a, b) = (original.allocators.get(name), proposed.allocators.get(name));
        let named = |x: &Allocator| x.name.clone();
        allocators.push(AllocatorComparison {
            sequence: match (a, b) {
                (Some(a), Some(b)) if a.name != b.name => format!("{} -> {}", a.name, b.name),
                (a, b) => a.or(b).map(named).unwrap_or_default(),
            },
            used_by: a.or(b).and_then(|x| x.used_by.clone()),
            original_next: a.map(|x| x.next.clone()),
            proposed_next: b.map(|x| x.next.clone()),
            equal: a.map(|x| &x.next) == b.map(|x| &x.next),
        });
        tokio::task::yield_now().await;
    }
    (tables, allocators)
}

#[cfg(test)]
mod tests;
