//! SQL Server's answer to the operations every engine must provide (#2798).
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
        Box::pin(crate::doctor::server_version(conn))
    }

    fn capabilities<'a>(
        &'a self,
        conn: &'a mut Conn,
        version: Option<&'a str>,
    ) -> EngineFuture<'a, Result<Capabilities, DbError>> {
        Box::pin(async move {
            use crate::edition::Edition;
            let edition = crate::edition::edition(conn).await?;
            Ok(Capabilities {
                edition: Some(match &edition {
                    // Named as unrecognised rather than passed through: the
                    // tool is about to treat it as limited, and an operator
                    // reading their own edition string back without comment
                    // would not know that.
                    Edition::Unknown(raw) => format!("{raw} (unrecognised; treated as limited)"),
                    known @ (Edition::Full(_) | Edition::Limited(_)) => known.name().to_owned(),
                }),
                supports_online: edition.supports_online(),
                supports_create_or_alter: version
                    .map(|v| crate::doctor::supports_create_or_alter(v, edition.name())),
            })
        })
    }

    fn ledger_problems<'a>(
        &'a self,
        _conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<Vec<String>, DbError>> {
        // SQL Server's ledger lives in `dbo`, and whether it needs the check
        // PostgreSQL makes (#313) is not this question's to settle, so it
        // reports none: an answer of this engine, not a default.
        Box::pin(async { Ok(Vec::new()) })
    }

    fn permissions<'a>(
        &'a self,
        conn: &'a mut Conn,
        project_ids: &'a IdsFile,
        ask: &'a Ask<'a>,
    ) -> EngineFuture<'a, Result<Permissions, DbError>> {
        Box::pin(async move {
            let held = crate::doctor::permissions(
                conn,
                ask.managed_tables,
                ask.managed_schemas,
                ask.referenced_columns,
                ask.granted,
                ask.data,
                ask.declared_keys,
                project_ids,
            )
            .await?;
            let render = |gaps: Vec<crate::doctor::Gap>| -> Vec<PermissionGap> {
                gaps.into_iter()
                    .map(|g| PermissionGap {
                        permission: g.permission,
                        securable: g.securable(),
                        why: g.why.into(),
                    })
                    .collect()
            };
            Ok(Permissions {
                gaps: render(crate::doctor::missing(&held)),
                advised: render(crate::doctor::advised(&held)),
                absent_schemas: held.absent_schemas,
                unasked: Vec::new(),
            })
        })
    }

    fn partition_runway<'a>(
        &'a self,
        _conn: &'a mut Conn,
        _ask: &'a RunwayAsk<'a>,
        _project_ids: &'a IdsFile,
    ) -> EngineFuture<'a, Result<RunwayReport, DbError>> {
        // SQL Server has no RANGE partitions to run out of, so the question
        // does not apply and the report is empty: an answer of this engine,
        // not a default.
        Box::pin(async { Ok(RunwayReport::default()) })
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
