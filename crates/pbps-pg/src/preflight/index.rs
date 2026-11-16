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
        IndexChange::BuildPartitionedIndex(_) => Ok(skip(
            change,
            "the complete rebuild is recertified under its source-tree lock",
            unchecked,
        )),
        // A unique index asks the same question, over the rows its predicate
        // keeps. A plain index constrains nothing and falls to the arm below.
        IndexChange::AddIndex {
            table, name, index, ..
        } if index.unique => {
            // The key columns only. `INCLUDE` is payload the engine carries in
            // the leaf and never compares, so counting it would group rows the
            // index will still collide.
            // An expression over the planned rows is the engine's to evaluate
            // and no probe here can: unchecked, not counted as something it is
            // not (DEC-1169.2).
            let Some(columns) = index.column_keys() else {
                return Ok(skip(
                    change,
                    "a unique index over an expression cannot be counted before apply",
                    unchecked,
                ));
            };
            let applies = match &index.filter {
                Some(predicate) => Applies::Where(predicate),
                None => Applies::AllRows,
            };
            let Some(rows) = rows_after(
                names,
                table,
                &columns,
                "c",
                &applies,
                StoredConversions::Unchecked,
            )?
            else {
                return Ok(skip(
                    change,
                    "the planned index values or predicate cannot be evaluated before apply",
                    unchecked,
                ));
            };
            Ok(vec![duplicate_probe(
                &format!("(\n{rows}\n) AS r"),
                &keys(columns.len()),
                &format!(
                    "rows that would collide under the new unique index {name}",
                    name = name.shown(),
                ),
            )])
        }
        // The unique ones are counted above; a plain index refuses nothing.
        IndexChange::AddIndex { .. }
        | IndexChange::DropIndex { .. }
        | IndexChange::SetIndexStorageParameters { .. } => Ok(Vec::new()),
    }
}
