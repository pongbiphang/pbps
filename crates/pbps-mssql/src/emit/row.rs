//! Emits the row family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(row: &RowChange) -> Sql {
    match row {
        // Reference data (ADR-0004). The only DML this tool emits, and it
        // reaches here only for a table that declared a `data:` block.
        RowChange::InsertRow {
            table,
            key_column,
            identity_key,
            key,
            row,
            defaults,
            types,
        } => insert_row(
            table,
            key_column.raw(),
            *identity_key,
            key,
            row,
            defaults,
            types,
        ),
        RowChange::UpdateRow {
            table,
            key_column,
            key,
            columns,
            unchanged,
            types,
            after_types,
            // T-SQL has no user-defined operators: a key's `=` is the
            // engine's whatever the session (DEC-1564.2).
            key_type: _,
        } => update_row(
            table,
            key_column.raw(),
            key,
            columns,
            unchanged,
            types,
            after_types,
        ),
        // The row's content is not carried (the pinned baseline holds it),
        // so the delete holds the row to its existence: a row already gone
        // is a baseline this plan was not reviewed against (DECISIONS 122).
        RowChange::DeleteRow {
            table,
            key_column,
            key,
            row,
            types,
            after_types,
            ..
        } => {
            // Keyed *and* held to the row the plan recorded. The checksum
            // pins the state only up to the moment `apply` reads it, so a
            // key-only DELETE removes whatever an application session left
            // under that key in between, and `@@ROWCOUNT = 1` calls the loss
            // a success. Each recorded cell is compared the way the read-back
            // rendered it, exactly as an update's precondition does. The
            // comparison is of text, so a cell that used to be carried unheld
            // for want of a native `=` is held like any other; what is still
            // carried unheld is narrower — a column this plan *retypes* whose
            // old type has no way back from its own rendering: `image`, which
            // has no conversion from text at all, and the two spatial types,
            // whose text leaves out the SRID (DECISIONS 143 and 471).
            let mut predicates = vec![format!("{} = {}", quote(key_column.raw())?, row_key(key))];
            for (column, cell) in row {
                predicates.extend(recorded_cell(
                    column.raw(),
                    cell,
                    Held::of(types.get(column), after_types.get(column)),
                )?);
            }
            one(atomically(&format!(
                // The guard, the delete and the checks after it are one
                // transaction of their own (`atomically`): the range locks the
                // guard takes have to be held through the delete they protect,
                // and a staged apply runs each statement outside a transaction.
                "{}\n\
                 DELETE FROM {} WHERE {};\n\
                 {}\n\
                 {}",
                crate::preflight::still_referenced(table, key_column.raw(), key)?,
                qualified(table)?,
                predicates.join(" AND "),
                exactly_one_row(table, key),
                // And the row stayed gone: a trigger that put it back would
                // otherwise be read back and recorded as this plan's result.
                gone_row(table, key, key_column.raw())?
            )))
        }
        // The mode is a property of the declaration, not of the database: it
        // decides what future plans do about undeclared rows. The row changes
        // it implies are separate entries in this same plan.
        RowChange::SetDataMode { .. } | RowChange::SetEnsureKeys { .. } => Ok(Vec::new()),
    }
}
