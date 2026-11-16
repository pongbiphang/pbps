//! Emits the row family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(pg: &Postgres, row: &RowChange, row_defaults: &RowDefaults<'_>) -> Sql {
    match row {
        // The mode is a property of the declaration, not of the database: it
        // decides what future plans do about undeclared rows. The row changes
        // it implies are separate entries in this same plan.
        RowChange::SetDataMode { .. } | RowChange::SetEnsureKeys { .. } => Ok(Vec::new()),
        // Reference data (ADR-0004). Each row change is one statement — a `DO`
        // block carrying the write and the checks that hold it to what the
        // plan reviewed — under the table's own write path, because the
        // defaults those checks compare against are the user's verbatim
        // expressions.
        RowChange::InsertRow {
            table,
            key_column,
            identity_key,
            key,
            row,
            defaults,
            types,
        } => row_statement(
            pg,
            table,
            pbps_dialect::RowOperation::Insert {
                // The key and each cell the row sets, as `insert_row` lists them.
                columns: std::iter::once(key_column.clone())
                    .chain(row.columns().map(|(column, _)| column.clone()))
                    .collect(),
            },
            atomically(&insert_row(
                table,
                key_column.raw(),
                *identity_key,
                key,
                row,
                defaults,
                types,
                row_defaults,
            )?),
        ),
        RowChange::UpdateRow {
            table,
            key_column,
            key,
            columns,
            unchanged,
            types,
            after_types,
            key_type,
        } => row_statement(
            pg,
            table,
            pbps_dialect::RowOperation::Update {
                columns: columns.keys().cloned().collect(),
            },
            atomically(&update_row(
                table,
                key_column.raw(),
                key,
                columns,
                unchanged,
                types,
                after_types,
                key_type.as_ref(),
                row_defaults,
            )?),
        ),
        RowChange::DeleteRow {
            table,
            key_column,
            key,
            row,
            types,
            after_types,
            key_type,
            ..
        } => row_statement(
            pg,
            table,
            pbps_dialect::RowOperation::Delete,
            atomically(&delete_row(
                table,
                key_column.raw(),
                key,
                row,
                types,
                after_types,
                key_type.as_ref(),
                row_defaults,
            )?),
        ),
    }
}
