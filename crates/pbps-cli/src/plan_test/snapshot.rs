//! What a re-pin compares: every managed table's rows and every allocator's
//! next value, read from one comparison database and held in memory
//! (ADR-0019 §5). Equality is decided on everything; only the report is bounded.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context as _;
use pbps_model::Schema;

use super::report::{AllocatorComparison, Cell, RowDifference, TableComparison};
use super::session::{Session, quoted, table};

/// Differences shown per table; the comparison itself never stops early.
const SHOWN: usize = 20;
/// Characters of one value shown in a difference.
const VALUE: usize = 200;

/// One table's rows as a multiset. Columns are sorted by name: a column added
/// by a later hop sits last in a replayed table and in declared order in a
/// bootstrapped one, and neither order is a difference in the data.
pub(super) struct Rows {
    columns: Vec<String>,
    rows: BTreeMap<Vec<Option<String>>, u64>,
}

/// An allocator is compared under the column that owns it, else under its name:
/// a renamed table keeps its sequence's old generated name, while a bootstrap
/// of the new revision generates a new one, and neither is a difference.
pub(super) struct Allocator {
    name: String,
    used_by: Option<String>,
    next: String,
}

pub(super) struct Snapshot {
    tables: BTreeMap<String, Rows>,
    allocators: BTreeMap<String, Allocator>,
}

/// Reads rows first, then probes the allocators: a probe consumes a value, and
/// this database is discarded afterwards. `nextval` is the engine's own answer
/// for increments, caches and exhaustion; `last_value + 1` is a guess.
pub(super) async fn read(session: &Session<'_>, schema: &Schema) -> anyhow::Result<Snapshot> {
    let mut conn = session.admin().await?;
    let mut tables = BTreeMap::new();
    for name in schema.tables.keys() {
        let relation = table(name)?;
        let literal = relation.replace('\'', "''");
        let sql = format!(
            "SELECT a.attname::text AS name FROM pg_catalog.pg_attribute a \
             WHERE a.attrelid = '{literal}'::pg_catalog.regclass AND a.attnum > 0 \
             AND NOT a.attisdropped ORDER BY a.attname COLLATE \"C\""
        );
        let mut columns = Vec::new();
        for row in conn.query(&sql).await? {
            columns.push(
                row.try_get::<&str>("name")?
                    .context("a column has no name")?
                    .to_owned(),
            );
        }
        let cells = columns
            .iter()
            .map(|c| quoted(c).map(|c| format!("{c}::pg_catalog.text")))
            .collect::<anyhow::Result<Vec<_>>>()?
            .join(", ");
        // jsonb text keeps NULL apart from '' and every value as its own text.
        let sql = format!(
            "SELECT pg_catalog.to_jsonb(ARRAY[{cells}]::pg_catalog.text[])::pg_catalog.text AS row, \
             pg_catalog.count(*)::pg_catalog.text AS n FROM {relation} GROUP BY 1"
        );
        let mut rows = BTreeMap::new();
        for row in conn.query(&sql).await? {
            let cells: Vec<Option<String>> =
                serde_json::from_str(row.try_get::<&str>("row")?.context("a row has no text")?)?;
            let count = row.try_get::<&str>("n")?.context("a row has no count")?;
            rows.insert(cells, count.parse::<u64>()?);
        }
        tables.insert(name.shown().to_string(), Rows { columns, rows });
    }
    // The ledger's own tables are not managed: their allocator differs between
    // a replay and a bootstrap by construction. Every other user sequence is
    // compared, so an allocator reached only through a function default is
    // never omitted.
    let sql = "SELECT n.nspname::text AS schema, c.relname::text AS name, \
        tn.nspname::text AS owner_schema, t.relname::text AS owner_table, \
        a.attname::text AS owner_column \
        FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
        LEFT JOIN pg_catalog.pg_depend d ON d.objid = c.oid \
          AND d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass \
          AND d.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass \
          AND d.deptype IN ('a', 'i') \
        LEFT JOIN pg_catalog.pg_class t ON t.oid = d.refobjid \
        LEFT JOIN pg_catalog.pg_namespace tn ON tn.oid = t.relnamespace \
        LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid = t.oid AND a.attnum = d.refobjsubid \
        WHERE c.relkind = 'S' AND n.nspname !~ '^pg_' AND n.nspname <> 'information_schema' \
        ORDER BY 1, 2";
    let mut allocators = BTreeMap::new();
    for row in conn.query(sql).await? {
        let text = |column: &str| -> anyhow::Result<Option<String>> {
            Ok(row.try_get::<&str>(column)?.map(str::to_owned))
        };
        let (owner_schema, owner_table) = (text("owner_schema")?, text("owner_table")?);
        if owner_schema.as_deref() == Some(pbps_pg::state::LEDGER_SCHEMA)
            && [
                pbps_db::ledger::STATE_TABLE_NAME,
                pbps_db::ledger::LOCK_TABLE_NAME,
            ]
            .contains(&owner_table.as_deref().unwrap_or_default())
        {
            continue;
        }
        let name = pbps_model::TableName::new(
            text("schema")?.context("a sequence has no schema")?,
            text("name")?.context("a sequence has no name")?,
        );
        let shown = name.shown().to_string();
        let used_by = match (owner_schema, owner_table, text("owner_column")?) {
            (Some(schema), Some(table), Some(column)) => Some(format!("{schema}.{table}.{column}")),
            _ => None,
        };
        let literal = table(&name)?.replace('\'', "''");
        let next = conn
            .query(&format!(
                "SELECT pg_catalog.nextval('{literal}'::pg_catalog.regclass)::pg_catalog.text AS next"
            ))
            .await
            .with_context(|| format!("the next value of sequence {shown} cannot be established"))?;
        let next = next
            .first()
            .and_then(|r| r.try_get::<&str>("next").transpose())
            .transpose()?
            .with_context(|| format!("sequence {shown} gave no next value"))?
            .to_owned();
        // Two sequences owned by one column are told apart by name.
        let key = used_by
            .clone()
            .filter(|owner| !allocators.contains_key(owner))
            .unwrap_or_else(|| shown.clone());
        allocators.insert(
            key,
            Allocator {
                name: shown,
                used_by,
                next,
            },
        );
    }
    Ok(Snapshot { tables, allocators })
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
pub(super) fn compare(
    original: &Snapshot,
    proposed: &Snapshot,
) -> (Vec<TableComparison>, Vec<AllocatorComparison>) {
    let names: BTreeSet<&String> = original
        .tables
        .keys()
        .chain(proposed.tables.keys())
        .collect();
    let tables = names
        .into_iter()
        .map(|name| {
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
            TableComparison {
                table: name.clone(),
                equal: same_columns && ar == br,
                original_columns: columns(a),
                proposed_columns: columns(b),
                original_rows: a.map_or(0, total),
                proposed_rows: b.map_or(0, total),
                differences,
                truncated,
            }
        })
        .collect();
    let names: BTreeSet<&String> = original
        .allocators
        .keys()
        .chain(proposed.allocators.keys())
        .collect();
    let allocators = names
        .into_iter()
        .map(|name| {
            let (a, b) = (original.allocators.get(name), proposed.allocators.get(name));
            let named = |x: &Allocator| x.name.clone();
            AllocatorComparison {
                sequence: match (a, b) {
                    (Some(a), Some(b)) if a.name != b.name => format!("{} -> {}", a.name, b.name),
                    (a, b) => a.or(b).map(named).unwrap_or_default(),
                },
                used_by: a.or(b).and_then(|x| x.used_by.clone()),
                original_next: a.map(|x| x.next.clone()),
                proposed_next: b.map(|x| x.next.clone()),
                equal: a.map(|x| &x.next) == b.map(|x| &x.next),
            }
        })
        .collect();
    (tables, allocators)
}

#[cfg(test)]
mod tests;
