//! Emits the partition family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(pg: &Postgres, change: &Change, partition: &PartitionChange) -> Sql {
    match partition {
        PartitionChange::CopyTableRows(copy) => rebuild::copy_table_rows(pg, copy),
        PartitionChange::AnalyzeTableTree(tree) => rebuild::analyze_table_tree(pg, tree),
        PartitionChange::RebuildPartitionedTable(_) => {
            let (operation, table) = change.pending_rebuild().expect("explicit rebuild variant");
            Err(DialectError::RebuildUnavailable {
                operation,
                table: table.clone(),
            })
        }
        PartitionChange::DetachPartition {
            table,
            parent,
            names,
            ..
        } => unscoped(detach_partition(table, parent, names)?),
        // pbps's alone, every name qualified and no declared expression in
        // it, so on the session's empty path (DEC-1564.1). The engine takes
        // the parent, then the table; it adopts the table's indexes and keys
        // that match the parent's and builds the rest (#1545, measured on 16
        // and 18).
        PartitionChange::AttachPartition {
            table,
            parent,
            bound,
            ..
        } => unscoped(format!(
            "ALTER TABLE {} ATTACH PARTITION {} {};",
            qualified(parent)?,
            qualified(table)?,
            bound_clause(bound)
        )),
        // A partition's own default, or, without one, the copy of its
        // parent's that the engine gives every partition: `DROP DEFAULT`
        // would leave the partition without the default its parent's rows
        // get, which is not what the declaration says (#1581, DEC-1581.1).
        //
        // Each text under the path it was written for: its own under the
        // partition's schema, as its `CREATE` sets it, and its parent's
        // under the parent's, which another schema's partition does not
        // share (#1607 review).
        PartitionChange::SetPartitionDefault {
            table,
            parent,
            column,
            to,
            fallback,
            ..
        } => {
            let set = |expr: &str| -> Result<String, DialectError> {
                Ok(format!(
                    "ALTER TABLE {} ALTER COLUMN {} SET DEFAULT {};",
                    qualified(table)?,
                    quote(column.raw())?,
                    verbatim(expr)
                ))
            };
            match (to, fallback) {
                (Some(own), _) => one(pg, table, set(own)?),
                (None, Some(parents)) => Ok(vec![scoped(pg, &parent.schema, &set(parents)?)?]),
                (None, None) => one(
                    pg,
                    table,
                    format!(
                        "ALTER TABLE {} ALTER COLUMN {} DROP DEFAULT;",
                        qualified(table)?,
                        quote(column.raw())?
                    ),
                ),
            }
        }
        PartitionChange::SetPartitionNotNull {
            table,
            column,
            not_null,
            ..
        } => one(
            pg,
            table,
            format!(
                "ALTER TABLE {} ALTER COLUMN {} {};",
                qualified(table)?,
                quote(column.raw())?,
                if *not_null {
                    "SET NOT NULL"
                } else {
                    "DROP NOT NULL"
                }
            ),
        ),
    }
}
