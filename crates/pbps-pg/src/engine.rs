//! PostgreSQL's answer to the operations every engine must provide (#2798).
//!
//! Each method calls the free `async fn` this crate already exposes for it, so
//! the trait adds a seam without moving any behaviour: the functions stay the
//! public API that engine-specific callers use directly.

use std::collections::BTreeMap;

use pbps_db::catalog::{Pulled, RowsError};
use pbps_db::doctor::{
    Ask, Capabilities, LockReadGap, PermissionGap, Permissions, RunwayAsk, RunwayReport,
};
use pbps_db::engine::{AssertionTarget, Engine, EngineFuture, Read, TargetAssertions};
use pbps_db::ledger::{StateRevision, StoredEntry};
use pbps_db::resolver::Discovery;
use pbps_db::{Conn, DbError, Driver, LedgerEntry, LedgerError, LockInfo, TimelineEntry};
use pbps_model::{IdsFile, ObservedRows, RowScope, Schema, StateSnapshot, TableName};

/// The PostgreSQL engine; carries no state, the connection is the state.
#[derive(Clone, Copy, Debug)]
pub struct PostgresEngine;

impl Engine for PostgresEngine {
    fn driver(&self) -> Driver {
        Driver::Postgres
    }

    fn name(&self) -> &'static str {
        "PostgreSQL"
    }

    fn dialect(&self) -> &'static str {
        crate::types::DIALECT
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
    ) -> EngineFuture<'a, Result<Option<LockInfo>, LedgerError>> {
        // A lock on this engine is either taken or refused: there is no stale
        // holder to clear on the way, so there is never one to report.
        Box::pin(async move { crate::state::lock(conn, holder).await.map(|()| None) })
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
        read: Read,
    ) -> EngineFuture<'a, Result<Pulled, DbError>> {
        match read {
            Read::Snapshot => Box::pin(crate::catalog::introspect(conn)),
            Read::InsideOwnTransaction => {
                Box::pin(crate::catalog::introspect_within_transaction(conn))
            }
        }
    }

    fn read_rows<'a>(
        &'a self,
        conn: &'a mut Conn,
        schema: &'a Schema,
        scopes: &'a BTreeMap<TableName, RowScope>,
        read: Read,
    ) -> EngineFuture<'a, Result<ObservedRows, RowsError>> {
        match read {
            Read::Snapshot => Box::pin(crate::catalog::read_rows(conn, schema, scopes)),
            Read::InsideOwnTransaction => Box::pin(crate::catalog::read_rows_within_transaction(
                conn, schema, scopes,
            )),
        }
    }

    fn execute_staged_statement<'a>(
        &'a self,
        conn: &'a mut Conn,
        statement: &'a pbps_dialect::Statement,
    ) -> EngineFuture<'a, Result<(), DbError>> {
        Box::pin(crate::staged::execute(conn, statement))
    }

    fn resolver_discovery<'a>(
        &'a self,
        conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<Discovery, DbError>> {
        Box::pin(crate::resolver::discover(conn))
    }

    fn server_version<'a>(
        &'a self,
        conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<String, DbError>> {
        Box::pin(async move { Ok(crate::doctor::server_version(conn).await?.text) })
    }

    fn capabilities<'a>(
        &'a self,
        _conn: &'a mut Conn,
        _version: Option<&'a str>,
    ) -> EngineFuture<'a, Result<Capabilities, DbError>> {
        // One edition, and every release this tool speaks to builds an index
        // `CONCURRENTLY` and replaces a routine with `CREATE OR REPLACE`; the
        // emitter writes both. Nothing is read because there is nothing that
        // could answer differently.
        Box::pin(async {
            Ok(Capabilities {
                edition: None,
                supports_online: true,
                supports_create_or_alter: Some(true),
            })
        })
    }

    /// PostgreSQL's answer is `ledger_problems` (issue #313): there, a role
    /// with `CREATE` on `public` can make the ledger tables first and attach
    /// code the deployment account would run.
    fn ledger_problems<'a>(
        &'a self,
        conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<Vec<String>, DbError>> {
        Box::pin(crate::state::ledger_problems(conn))
    }

    fn permissions<'a>(
        &'a self,
        conn: &'a mut Conn,
        project_ids: &'a IdsFile,
        ask: &'a Ask<'a>,
    ) -> EngineFuture<'a, Result<Permissions, DbError>> {
        Box::pin(async move {
            let held = crate::doctor::permissions(conn, ask, project_ids).await?;
            Ok(Permissions {
                gaps: crate::doctor::missing(&held)
                    .into_iter()
                    .map(|g| PermissionGap {
                        permission: g.permission,
                        securable: g.securable(),
                        why: g.why,
                    })
                    .collect(),
                // PostgreSQL's dependency reads need nothing the required
                // list does not ask.
                advised: Vec::new(),
                absent_schemas: held.absent_schemas,
            })
        })
    }

    fn partition_runway<'a>(
        &'a self,
        conn: &'a mut Conn,
        ask: &'a RunwayAsk<'a>,
        project_ids: &'a IdsFile,
    ) -> EngineFuture<'a, Result<RunwayReport, DbError>> {
        Box::pin(crate::doctor::partition_runway(conn, ask, project_ids))
    }

    fn lock_read_gap<'a>(
        &'a self,
        conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<LockReadGap, DbError>> {
        Box::pin(crate::doctor::lock_read_gap(conn))
    }

    fn read_assertion_target<'a>(
        &'a self,
        conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<TargetAssertions, DbError>> {
        Box::pin(async move {
            let target = crate::assertions::Target::read(conn).await?;
            Ok(TargetAssertions::Supported(
                Box::new(target) as Box<dyn AssertionTarget>
            ))
        })
    }
}
