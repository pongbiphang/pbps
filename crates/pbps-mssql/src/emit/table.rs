//! Emits the table family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(table: &TableChange) -> Sql {
    match table {
        // The first statement is the one that brings the table into being;
        // it says so, and a staged checkpoint adopts the table from there.
        TableChange::CreateTable { name, table, .. } => {
            let mut out = create_table(name, table)?;
            if let Some(first) = out.first_mut() {
                first.creates.push(Created::Table(name.clone()));
            }
            Ok(out)
        }
        TableChange::DropTable {
            name,
            detach_from: None,
            ..
        } => one(format!("DROP TABLE {};", qualified(name)?)),
        // A partition is PostgreSQL's in this model, refused on this engine
        // by validation; a plan that carries one anyway is not run (#1171,
        // #1544).
        TableChange::DropTable {
            name: table,
            detach_from: Some(parent),
            ..
        } => Err(DialectError::Invalid {
            dialect: crate::types::DIALECT,
            message: format!(
                "{table} is a partition of {parent}, which SQL Server's model does not hold",
                table = table.shown(),
                parent = parent.shown()
            ),
        }),
        TableChange::RenameTable {
            from, to, defaults, ..
        } => rename_table(from, to, defaults),
    }
}
