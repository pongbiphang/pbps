//! SQLite's answer to the operations every engine must provide (#2798, #2950).
//!
//! Each method calls the function this crate already has for it, on the
//! [`Connection`] inside the [`Conn`]. They are plain blocking calls made in
//! place (DEC-2950.17): the future each method returns never waits on
//! anything but the library.
//!
//! Two methods are more than a call. `lock` returns a guard that has to live
//! as long as the lock does, and a command carries only its connection
//! between taking the lock and releasing it, so the guard is parked on the
//! connection and `unlock` takes it out again.
//!
//! The provided questions this file does not override keep the trait's stated
//! defaults (DEC-2950.18).

use std::collections::BTreeMap;

use pbps_db::catalog::{Pulled, RowsError};
use pbps_db::doctor::{RunwayAsk, RunwayReport};
use pbps_db::engine::{Engine, EngineFuture, Read};
use pbps_db::ledger::{StateRevision, StoredEntry};
use pbps_db::sqlite::Connection;
use pbps_db::{Conn, DbError, Driver, LedgerEntry, LedgerError, LockInfo, TimelineEntry};
use pbps_model::{IdsFile, ObservedRows, RowScope, Schema, StateSnapshot, TableName};

use crate::state::LockGuard;

/// The SQLite engine; carries no state, the connection is the state.
#[derive(Clone, Copy, Debug)]
pub struct SqliteEngine;

/// The SQLite connection inside `conn`, or a refusal: this engine was handed
/// another engine's connection, which is a routing bug and never a database
/// with nothing in it.
fn file(conn: &mut Conn) -> Result<&mut Connection, DbError> {
    conn.sqlite().ok_or_else(|| {
        DbError::Refused(
            "the SQLite engine was given a connection that is not to a SQLite file".into(),
        )
    })
}

/// Takes the lock and parks its guard on the connection.
fn lock(c: &mut Connection, holder: &str) -> Result<Option<LockInfo>, LedgerError> {
    let taken = crate::state::lock(c, holder)?;
    match c.park(Box::new(taken.guard)) {
        Ok(()) => Ok(taken.cleared),
        Err(back) => {
            // Something is already parked, so this connection took a lock and
            // never released it, and the row just written is a second one's.
            // Give that row back before saying so; the first guard stays.
            if let Ok(guard) = back.downcast::<LockGuard>() {
                crate::state::release(c, *guard)?;
            }
            Err(DbError::Refused(
                "this connection already holds a deployment lock; release it before taking another"
                    .into(),
            )
            .into())
        }
    }
}

/// Releases this run's lock when the connection holds its guard, and clears
/// whoever's row it is when it holds none (what `pbps unlock` asks for).
fn unlock(c: &mut Connection) -> Result<bool, DbError> {
    match c.unpark() {
        None => crate::state::unlock(c),
        Some(parked) => match parked.downcast::<LockGuard>() {
            Ok(guard) => crate::state::release(c, *guard),
            Err(other) => {
                // Not this engine's value. Put it back untouched: dropping it
                // here would end something this code knows nothing about.
                let _ = c.park(other);
                Err(DbError::Refused(
                    "the connection holds a value the SQLite engine did not park; the lock was not touched"
                        .into(),
                ))
            }
        },
    }
}

impl Engine for SqliteEngine {
    fn driver(&self) -> Driver {
        Driver::Sqlite
    }

    fn name(&self) -> &'static str {
        "SQLite"
    }

    fn is_initialized<'a>(&'a self, conn: &'a mut Conn) -> EngineFuture<'a, Result<bool, DbError>> {
        Box::pin(async move { crate::state::is_initialized(file(conn)?) })
    }

    fn latest<'a>(
        &'a self,
        conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<Option<LedgerEntry>, LedgerError>> {
        Box::pin(async move { crate::state::latest(file(conn)?) })
    }

    fn deployed_revision<'a>(
        &'a self,
        conn: &'a mut Conn,
        observed_id: i64,
    ) -> EngineFuture<'a, Result<Option<StateRevision>, LedgerError>> {
        Box::pin(async move { crate::state::deployed_revision(file(conn)?, observed_id) })
    }

    fn has_applied_plan<'a>(
        &'a self,
        conn: &'a mut Conn,
        checksum: &'a str,
    ) -> EngineFuture<'a, Result<bool, LedgerError>> {
        Box::pin(async move { crate::state::has_applied_plan(file(conn)?, checksum) })
    }

    fn entry<'a>(
        &'a self,
        conn: &'a mut Conn,
        id: Option<i64>,
    ) -> EngineFuture<'a, Result<Option<StoredEntry>, LedgerError>> {
        Box::pin(async move { crate::state::entry(file(conn)?, id) })
    }

    fn timeline<'a>(
        &'a self,
        conn: &'a mut Conn,
        limit: u32,
    ) -> EngineFuture<'a, Result<Vec<TimelineEntry>, LedgerError>> {
        Box::pin(async move { crate::state::timeline(file(conn)?, limit) })
    }

    fn record<'a>(
        &'a self,
        conn: &'a mut Conn,
        snapshot: &'a StateSnapshot,
    ) -> EngineFuture<'a, Result<i64, LedgerError>> {
        Box::pin(async move { crate::state::record(file(conn)?, snapshot) })
    }

    fn prune<'a>(
        &'a self,
        conn: &'a mut Conn,
        keep: u32,
    ) -> EngineFuture<'a, Result<u64, LedgerError>> {
        Box::pin(async move { crate::state::prune(file(conn)?, keep) })
    }

    fn session_application_name<'a>(
        &'a self,
        conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<String, DbError>> {
        Box::pin(async move { crate::state::session_application_name(file(conn)?) })
    }

    fn lock<'a>(
        &'a self,
        conn: &'a mut Conn,
        holder: &'a str,
    ) -> EngineFuture<'a, Result<Option<LockInfo>, LedgerError>> {
        Box::pin(async move { lock(file(conn)?, holder) })
    }

    fn unlock<'a>(&'a self, conn: &'a mut Conn) -> EngineFuture<'a, Result<bool, DbError>> {
        Box::pin(async move { unlock(file(conn)?) })
    }

    fn lock_holder<'a>(
        &'a self,
        conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<Option<LockInfo>, DbError>> {
        Box::pin(async move { crate::state::lock_holder(file(conn)?) })
    }

    fn introspect<'a>(
        &'a self,
        conn: &'a mut Conn,
        read: Read,
    ) -> EngineFuture<'a, Result<Pulled, DbError>> {
        Box::pin(async move {
            let c = file(conn)?;
            match read {
                // Takes its own read transaction, so the catalog is one moment.
                Read::Snapshot => crate::catalog::introspect(c),
                // Reads what the open transaction built, before it commits.
                Read::InsideOwnTransaction => crate::catalog::introspect_within_transaction(c),
            }
        })
    }

    fn read_rows<'a>(
        &'a self,
        _conn: &'a mut Conn,
        _schema: &'a Schema,
        scopes: &'a BTreeMap<TableName, RowScope>,
        _read: Read,
    ) -> EngineFuture<'a, Result<ObservedRows, RowsError>> {
        Box::pin(async move {
            // Reference data is not managed on SQLite yet. No scope asks
            // nothing and gets nothing; a scope is refused by its table, since
            // an empty answer would read as "that table has no such rows".
            match scopes.keys().next() {
                None => Ok(ObservedRows::default()),
                Some(table) => Err(RowsError::Unreadable {
                    table: table.clone(),
                    why: "managed rows are not read on SQLite yet".into(),
                }),
            }
        })
    }

    fn execute_staged_statement<'a>(
        &'a self,
        conn: &'a mut Conn,
        statement: &'a pbps_dialect::Statement,
    ) -> EngineFuture<'a, Result<(), DbError>> {
        // SQLite's emitted DDL has no build that outlives a failed statement.
        Box::pin(async move { conn.execute(&statement.sql).await })
    }

    fn server_version<'a>(
        &'a self,
        conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<String, DbError>> {
        // The library's version: there is no server, and the library linked
        // into this process is what runs every statement.
        Box::pin(async move { Ok(format!("SQLite {}", file(conn)?.version()?)) })
    }

    fn ledger_problems<'a>(
        &'a self,
        conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<Vec<String>, DbError>> {
        Box::pin(async move { crate::state::ledger_problems(file(conn)?) })
    }

    fn partition_runway<'a>(
        &'a self,
        conn: &'a mut Conn,
        _ask: &'a RunwayAsk<'a>,
        _project_ids: &'a IdsFile,
    ) -> EngineFuture<'a, Result<RunwayReport, DbError>> {
        // SQLite has no partitions, so no parent can run out of them: the
        // empty report is the answer, where the default would say the engine
        // had not declared whether it has any.
        Box::pin(async move { file(conn).map(|_| RunwayReport::default()) })
    }
}

#[cfg(test)]
mod tests;
