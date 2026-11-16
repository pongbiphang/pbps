//! Emits the index family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(change: &Change, index: &IndexChange, strategy: Strategy) -> Sql {
    match index {
        IndexChange::BuildPartitionedIndex(_) => {
            let (operation, table) = change.pending_rebuild().expect("explicit rebuild variant");
            Err(DialectError::RebuildUnavailable {
                operation,
                table: table.clone(),
            })
        }
        IndexChange::SetIndexStorageParameters { table, .. } => Err(DialectError::Unsupported {
            dialect: DIALECT,
            feature: format!(
                "index storage parameters on `{table}`, which are PostgreSQL's",
                table = table.shown()
            ),
        }),
        IndexChange::AddIndex {
            table,
            name,
            index,
            clustered,
        } => one(create_index(
            table,
            name.raw(),
            index,
            *clustered,
            strategy,
        )?),
        // No ONLINE clause, deliberately. SQL Server accepts `WITH (ONLINE =
        // ON)` on a drop only for a **clustered** index, where the drop rebuilds
        // the table as a heap and there is something to do online, and rejects
        // it on a nonclustered one (3745, measured). Since #1178 an index this
        // emitter drops can be either, and the change does not say which — so,
        // as for a UNIQUE constraint above, the drop is always offline. That
        // costs nothing on a nonclustered index, whose drop is metadata; on a
        // clustered one it is the heap rebuild under the table lock that the
        // layout change's operational note names.
        IndexChange::DropIndex { table, name } => one(format!(
            "DROP INDEX {} ON {};",
            quote(name.raw())?,
            qualified(table)?
        )),
    }
}
