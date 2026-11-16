//! Probes the index family of changes before apply (DEC-2790.4).

// The moved arms use the parent preflight's private helpers, which stay there.
use super::*;

pub(super) fn preflight<'v, 'p: 'v>(
    change: &Change,
    index: &IndexChange,
    names: &impl Deref<Target = AsStored<'v, 'p>>,
    unchecked: &mut Vec<Unchecked>,
) -> Result<Vec<Probe>, DialectError> {
    match index {
        IndexChange::BuildPartitionedIndex(_) => { let (operation, table) = change.pending_rebuild().expect("rebuild variant"); Err(DialectError::RebuildUnavailable { operation, table: table.clone() }) },
        // The same question `AddUnique` asks, because in SQL Server it is the
        // same object: a UNIQUE constraint is enforced by a unique index, and
        // which YAML key the uniqueness was written under cannot decide
        // whether the data is counted. A plain index constrains nothing and
        // falls to the arm below.
        IndexChange::AddIndex { table, name, index, .. } if index.unique => {
            // The key columns only. `INCLUDE` is payload the engine carries in
            // the leaf and never compares, so counting it would group rows the
            // index will still collide.
            // Refused by validation on this engine; unchecked rather than
            // counted over text that is not a column (DEC-1169.2).
            let Some(columns) = index.column_keys() else {
                return Ok(skip(change, "an index over an expression cannot be counted", unchecked));
            };
            let applies = match &index.filter {
                Some(predicate) => Applies::Where(predicate),
                None => Applies::AllRows,
            };
            let rows = rows_after(
                names,
                table,
                &columns,
                names.moved.get(table),
                "c",
                &applies,
            )?;
            let Some(rows) = rows else {
                return Ok(skip(change, "the planned index values or predicate cannot be evaluated before apply", unchecked));
            };
            Ok(vec![duplicate_probe(
                &format!("(\n{rows}\n) AS r"),
                &collated_keys(names, table, &columns)?,
                &format!("rows that would collide under the new unique index {name}",
name = name.shown(),
),
            )?])
        }
        // The unique ones are counted above; a plain index refuses nothing.
        IndexChange::AddIndex { .. } | IndexChange::DropIndex { .. } | // This dialect never produces one and `emit` refuses it; a probe
// would be a second opinion about a statement that will not be
// written.
IndexChange::SetIndexStorageParameters { .. } => Ok(Vec::new()),
    }
}
