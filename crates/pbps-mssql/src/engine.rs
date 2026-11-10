//! SQL Server's answer to the operations every engine must provide (#2798).
//!
//! Each method calls the free `async fn` this crate already exposes for it, so
//! the trait adds a seam without moving any behaviour: the functions stay the
//! public API that engine-specific callers use directly.

use std::collections::BTreeMap;

use pbps_db::catalog::{Pulled, RowsError};
use pbps_db::engine::{Engine, EngineFuture, Read};
use pbps_db::ledger::{StateRevision, StoredEntry};
use pbps_db::{Conn, DbError, Driver, LedgerEntry, LedgerError, LockInfo, TimelineEntry};
use pbps_model::{ObservedRows, RowScope, Schema, StateSnapshot, TableName};

/// The SQL Server engine; carries no state, the connection is the state.
#[derive(Clone, Copy, Debug)]
pub struct MssqlEngine;

impl Engine for MssqlEngine {
    fn driver(&self) -> Driver {
        Driver::Mssql
    }

    fn name(&self) -> &'static str {
        "SQL Server"
    }

    fn is_initialized<'a>(&'a self, conn: &'a mut Conn) -> EngineFuture<'a, Result<bool, DbError>> {
        Box::pin(crate::state::is_initialized(conn))
    }

    fn latest<'a>(
        &'a self,
        conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<Option<LedgerEntry>, LedgerError>> {
        Box::pin(crate::state::latest(conn))
    }

    fn deployed_revision<'a>(
        &'a self,
        conn: &'a mut Conn,
        observed_id: i64,
    ) -> EngineFuture<'a, Result<Option<StateRevision>, LedgerError>> {
        Box::pin(crate::state::deployed_revision(conn, observed_id))
    }

    fn has_applied_plan<'a>(
        &'a self,
        conn: &'a mut Conn,
        checksum: &'a str,
    ) -> EngineFuture<'a, Result<bool, LedgerError>> {
        Box::pin(crate::state::has_applied_plan(conn, checksum))
    }

    fn entry<'a>(
        &'a self,
        conn: &'a mut Conn,
        id: Option<i64>,
    ) -> EngineFuture<'a, Result<Option<StoredEntry>, LedgerError>> {
        Box::pin(crate::state::entry(conn, id))
    }

    fn timeline<'a>(
        &'a self,
        conn: &'a mut Conn,
        limit: u32,
    ) -> EngineFuture<'a, Result<Vec<TimelineEntry>, LedgerError>> {
        Box::pin(crate::state::timeline(conn, limit))
    }

    fn record<'a>(
        &'a self,
        conn: &'a mut Conn,
        snapshot: &'a StateSnapshot,
    ) -> EngineFuture<'a, Result<i64, LedgerError>> {
        Box::pin(crate::state::record(conn, snapshot))
    }

    fn prune<'a>(
        &'a self,
        conn: &'a mut Conn,
        keep: u32,
    ) -> EngineFuture<'a, Result<u64, LedgerError>> {
        Box::pin(crate::state::prune(conn, keep))
    }

    fn session_application_name<'a>(
        &'a self,
        conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<String, DbError>> {
        Box::pin(crate::state::session_application_name(conn))
    }

    fn lock<'a>(
        &'a self,
        conn: &'a mut Conn,
        holder: &'a str,
    ) -> EngineFuture<'a, Result<(), LedgerError>> {
        Box::pin(crate::state::lock(conn, holder))
    }

    fn unlock<'a>(&'a self, conn: &'a mut Conn) -> EngineFuture<'a, Result<bool, DbError>> {
        Box::pin(crate::state::unlock(conn))
    }

    fn lock_holder<'a>(
        &'a self,
        conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<Option<LockInfo>, DbError>> {
        Box::pin(crate::state::lock_holder(conn))
    }

    fn introspect<'a>(
        &'a self,
        conn: &'a mut Conn,
        _read: Read,
    ) -> EngineFuture<'a, Result<Pulled, DbError>> {
        // SQL Server reads its catalog the same way inside and outside a
        // transaction, so the mode makes no difference here.
        Box::pin(crate::catalog::introspect(conn))
    }

    fn read_rows<'a>(
        &'a self,
        conn: &'a mut Conn,
        schema: &'a Schema,
        scopes: &'a BTreeMap<TableName, RowScope>,
        _read: Read,
    ) -> EngineFuture<'a, Result<ObservedRows, RowsError>> {
        Box::pin(crate::catalog::read_rows(conn, schema, scopes))
    }

    fn execute_staged_statement<'a>(
        &'a self,
        conn: &'a mut Conn,
        statement: &'a pbps_dialect::Statement,
    ) -> EngineFuture<'a, Result<(), DbError>> {
        // SQL Server's emitted DDL has no non-transactional build artifact.
        Box::pin(async move { conn.execute(&statement.sql).await })
    }
}
