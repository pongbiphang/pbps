//! Emits the table family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(pg: &Postgres, table: &TableChange) -> Sql {
    match table {
        // The first statement is the one that brings the table into being; it
        // says so, and a staged checkpoint adopts the table from there.
        TableChange::CreateTable {
            name,
            table,
            beside_default,
            moves_at_most,
            ..
        } => {
            let mut out = create_table(
                pg,
                name,
                table,
                beside_default.as_ref().map(|d| (d, *moves_at_most)),
            )?;
            if let Some(first) = out.first_mut() {
                first.creates.push(Created::Table(name.clone()));
            }
            Ok(out)
        }
        // A partition is detached first: measured on 16 and 18, one that a
        // foreign key to its parent reaches cannot be dropped while attached
        // (`other objects depend on it`), and the detach refuses while rows
        // still reference it. One statement, so that no apply, staged or
        // not, can stop with the partition detached and not dropped (#1171).
        TableChange::DropTable {
            name,
            detach_from: Some(parent),
            ..
        } => one(
            pg,
            name,
            format!(
                "ALTER TABLE {} DETACH PARTITION {};\nDROP TABLE {};",
                qualified(parent)?,
                qualified(name)?,
                qualified(name)?
            ),
        ),
        TableChange::DropTable {
            name,
            detach_from: None,
            ..
        } => one(pg, name, format!("DROP TABLE {};", qualified(name)?)),
        // Two statements when both halves move, and neither engine has one that
        // does both: `RENAME TO` cannot cross a schema and `SET SCHEMA` cannot
        // rename. The transfer goes first, so the name in between is the old
        // one in the new schema — and each statement says what it does to the
        // name (`Statement::renaming`), because between them the table is
        // findable under neither the baseline's name nor the plan's.
        TableChange::RenameTable { from, to, .. } => rename_table(pg, from, to),
    }
}
