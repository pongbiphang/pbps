//! Emits the table_property family of changes (DEC-2790.4).

// The moved arms use the parent emitter's private helpers, which stay there.
use super::*;

pub(super) fn emit(pg: &Postgres, table_property: &TablePropertyChange) -> Sql {
    match table_property {
        TablePropertyChange::SetReplicaIdentity { table, to, .. } => {
            unscoped(set_replica_identity(table, to.as_ref())?)
        }
        TablePropertyChange::SetComment { target, text } => {
            unscoped(comment_on(target, text.as_ref())?)
        }
        // Rewrites the table and its indexes, in either direction (#1443).
        TablePropertyChange::SetTablePersistence {
            table, unlogged, ..
        } => one(
            pg,
            table,
            format!(
                "ALTER TABLE {} SET {};",
                qualified(table)?,
                if *unlogged { "UNLOGGED" } else { "LOGGED" }
            ),
        ),
        TablePropertyChange::SetStorageParameters {
            table, set, reset, ..
        } => {
            // One statement with both subcommands, so no read between them
            // finds half the change: a staged checkpoint holds the table to
            // its old parameters or its new ones (#1441).
            let actions = storage_actions(set, reset, &|name| {
                pbps_model::storage::table_kind(name).is_some()
            })?;
            one(
                pg,
                table,
                format!("ALTER TABLE {} {actions};", qualified(table)?),
            )
        }
    }
}
