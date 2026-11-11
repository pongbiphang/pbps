//! The operations every engine must provide to plan and apply (#2798).
//!
//! # Why the trait lives here
//!
//! `pbps-dialect` is pure and is a dependency of this crate, so a trait whose
//! methods take a [`Conn`] cannot live there without a cycle. This crate owns
//! `Conn` and every result type the methods return (`LedgerEntry`,
//! `LedgerError`, `Pulled`, ...), and an engine crate already depends on it, so
//! the trait sits where an engine can implement it using only `pbps-db` and
//! `pbps-model` types. No engine crate type appears in a signature, which is
//! what lets the CLI hold a `&'static dyn Engine` and a third engine be added
//! without touching a caller.
//!
//! # What is, and is not, in the trait
//!
//! Only the operations any engine needs in order to plan and apply: the ledger
//! and the lock, the catalog read, and executing one staged statement. Every
//! method is required: there is no default body, because a default would let a
//! new engine compile while silently lacking part of the ledger. Capabilities
//! only some engines have (extensions, partitions, temporal tables, ...) arrive
//! in later steps as methods with refusing defaults (#2798).
//!
//! # How async is expressed
//!
//! Methods return an [`EngineFuture`], a boxed future, so the trait is object
//! safe without a dependency: the workspace has no direct `async-trait`, and
//! `futures-util` (already here) names the boxed type. The future is `Send`
//! because every engine future in this workspace is, and a bound the engines
//! already meet costs nothing now but is expensive to add after a third engine
//! has been written without it. One allocation per connected call is
//! immaterial next to the network round trip it precedes.

use std::collections::BTreeMap;

use pbps_dialect::Statement;
use pbps_model::{ObservedRows, RowScope, Schema, StateSnapshot, TableName};

use crate::catalog::{Pulled, RowsError};
use crate::ledger::{StateRevision, StoredEntry};
use crate::{Conn, DbError, Driver, LedgerEntry, LedgerError, LockInfo, TimelineEntry};

/// What an [`Engine`] method returns: a boxed, `Send` future borrowing the
/// engine, the connection and the arguments for `'a`.
pub type EngineFuture<'a, T> = futures_util::future::BoxFuture<'a, T>;

/// Where a catalog read runs, which the command knows and no engine can
/// guess from the connection alone.
///
/// The two engines differ here in a way the seam had to learn from a live
/// run (DECISIONS 418): SQL Server reads its catalog the same way inside and
/// outside a transaction, and PostgreSQL's pull takes a transaction of its
/// own and refuses to be asked inside somebody else's (253) — while the
/// apply's read-back *must* run inside the transaction that built what it
/// reads (147). So the command says which it is, and a mismatch is refused by
/// the engine that can tell.
///
/// An engine with no such distinction treats both variants alike; SQL Server
/// does today.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    /// Outside any transaction. PostgreSQL takes its own `REPEATABLE READ
    /// READ ONLY` snapshot for it and refuses if one is open.
    Snapshot,
    /// Inside the transaction this command opened for a plan's statements, to
    /// record what they built before it commits. PostgreSQL reads under a
    /// savepoint and refuses if no transaction is open.
    InsideOwnTransaction,
}

/// The connected operations every engine provides.
///
/// Every method is required and none has a default body; optional capabilities
/// arrive later with refusing defaults (#2798). Methods take `&self` so an
/// engine is a shareable, usually zero-sized, value, and `&mut Conn` because
/// the connection is the one thing they mutate. Each returns the same result
/// type the CLI's per-operation function returns, so callers match on
/// `LedgerError::NotInitialized` and the like unchanged.
pub trait Engine: Send + Sync {
    /// The driver this engine speaks, which is what a connection reports.
    fn driver(&self) -> Driver;

    /// The engine's name as error messages spell it ("PostgreSQL").
    fn name(&self) -> &'static str;

    // -- The ledger and the lock (SPEC section 8.1) ------------------------

    /// Whether the ledger tables exist.
    fn is_initialized<'a>(&'a self, conn: &'a mut Conn) -> EngineFuture<'a, Result<bool, DbError>>;

    /// The newest ledger entry, or `None` for an empty ledger.
    fn latest<'a>(
        &'a self,
        conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<Option<LedgerEntry>, LedgerError>>;

    /// The revision deployed under `observed_id`, if any.
    fn deployed_revision<'a>(
        &'a self,
        conn: &'a mut Conn,
        observed_id: i64,
    ) -> EngineFuture<'a, Result<Option<StateRevision>, LedgerError>>;

    /// Whether a plan with this checksum was already applied.
    fn has_applied_plan<'a>(
        &'a self,
        conn: &'a mut Conn,
        checksum: &'a str,
    ) -> EngineFuture<'a, Result<bool, LedgerError>>;

    /// A selected historical entry, or the newest when `id` is `None`.
    fn entry<'a>(
        &'a self,
        conn: &'a mut Conn,
        id: Option<i64>,
    ) -> EngineFuture<'a, Result<Option<StoredEntry>, LedgerError>>;

    /// The newest `limit` ledger entries, summarised.
    fn timeline<'a>(
        &'a self,
        conn: &'a mut Conn,
        limit: u32,
    ) -> EngineFuture<'a, Result<Vec<TimelineEntry>, LedgerError>>;

    /// Appends `snapshot` to the ledger and returns its id.
    fn record<'a>(
        &'a self,
        conn: &'a mut Conn,
        snapshot: &'a StateSnapshot,
    ) -> EngineFuture<'a, Result<i64, LedgerError>>;

    /// Deletes all but the newest `keep` entries and returns how many went.
    fn prune<'a>(
        &'a self,
        conn: &'a mut Conn,
        keep: u32,
    ) -> EngineFuture<'a, Result<u64, LedgerError>>;

    /// The application name this session carries, which the lock records.
    ///
    /// Separate from [`Engine::lock`] because the holder text is rendered by
    /// the caller from this name and from the host, pid and CI job, which no
    /// engine knows.
    fn session_application_name<'a>(
        &'a self,
        conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<String, DbError>>;

    /// Takes the lock, recording the already-rendered `holder` text.
    fn lock<'a>(
        &'a self,
        conn: &'a mut Conn,
        holder: &'a str,
    ) -> EngineFuture<'a, Result<(), LedgerError>>;

    /// Releases the lock; `false` when it was not held.
    fn unlock<'a>(&'a self, conn: &'a mut Conn) -> EngineFuture<'a, Result<bool, DbError>>;

    /// Who holds the lock, if anyone.
    fn lock_holder<'a>(
        &'a self,
        conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<Option<LockInfo>, DbError>>;

    // -- The catalog -------------------------------------------------------

    /// Reads the database's schema as it stands, where `read` says to.
    fn introspect<'a>(
        &'a self,
        conn: &'a mut Conn,
        read: Read,
    ) -> EngineFuture<'a, Result<Pulled, DbError>>;

    /// Reads the declared rows of the tables in `scopes`, where `read` says to.
    fn read_rows<'a>(
        &'a self,
        conn: &'a mut Conn,
        schema: &'a Schema,
        scopes: &'a BTreeMap<TableName, RowScope>,
        read: Read,
    ) -> EngineFuture<'a, Result<ObservedRows, RowsError>>;

    // -- Execution ---------------------------------------------------------

    /// Executes one staged DDL statement, including recovery owned by the
    /// engine.
    fn execute_staged_statement<'a>(
        &'a self,
        conn: &'a mut Conn,
        statement: &'a Statement,
    ) -> EngineFuture<'a, Result<(), DbError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An engine defined outside the two real ones, using only `pbps-db` and
    /// `pbps-model` types. That this compiles is the test: nothing in the
    /// trait names an engine crate. Its methods fail rather than answer,
    /// because none can be reached without a connection to a server.
    struct Stub;

    fn refused<'a, T: Send + 'a>() -> EngineFuture<'a, Result<T, DbError>> {
        Box::pin(async { Err(DbError::BadRow("the stub engine has no server".into())) })
    }

    fn refused_ledger<'a, T: Send + 'a>() -> EngineFuture<'a, Result<T, LedgerError>> {
        Box::pin(async { Err(LedgerError::NotInitialized) })
    }

    impl Engine for Stub {
        fn driver(&self) -> Driver {
            Driver::Postgres
        }
        fn name(&self) -> &'static str {
            "Stub"
        }
        fn is_initialized<'a>(
            &'a self,
            _: &'a mut Conn,
        ) -> EngineFuture<'a, Result<bool, DbError>> {
            refused()
        }
        fn latest<'a>(
            &'a self,
            _: &'a mut Conn,
        ) -> EngineFuture<'a, Result<Option<LedgerEntry>, LedgerError>> {
            refused_ledger()
        }
        fn deployed_revision<'a>(
            &'a self,
            _: &'a mut Conn,
            _: i64,
        ) -> EngineFuture<'a, Result<Option<StateRevision>, LedgerError>> {
            refused_ledger()
        }
        fn has_applied_plan<'a>(
            &'a self,
            _: &'a mut Conn,
            _: &'a str,
        ) -> EngineFuture<'a, Result<bool, LedgerError>> {
            refused_ledger()
        }
        fn entry<'a>(
            &'a self,
            _: &'a mut Conn,
            _: Option<i64>,
        ) -> EngineFuture<'a, Result<Option<StoredEntry>, LedgerError>> {
            refused_ledger()
        }
        fn timeline<'a>(
            &'a self,
            _: &'a mut Conn,
            _: u32,
        ) -> EngineFuture<'a, Result<Vec<TimelineEntry>, LedgerError>> {
            refused_ledger()
        }
        fn record<'a>(
            &'a self,
            _: &'a mut Conn,
            _: &'a StateSnapshot,
        ) -> EngineFuture<'a, Result<i64, LedgerError>> {
            refused_ledger()
        }
        fn prune<'a>(
            &'a self,
            _: &'a mut Conn,
            _: u32,
        ) -> EngineFuture<'a, Result<u64, LedgerError>> {
            refused_ledger()
        }
        fn session_application_name<'a>(
            &'a self,
            _: &'a mut Conn,
        ) -> EngineFuture<'a, Result<String, DbError>> {
            refused()
        }
        fn lock<'a>(
            &'a self,
            _: &'a mut Conn,
            _: &'a str,
        ) -> EngineFuture<'a, Result<(), LedgerError>> {
            refused_ledger()
        }
        fn unlock<'a>(&'a self, _: &'a mut Conn) -> EngineFuture<'a, Result<bool, DbError>> {
            refused()
        }
        fn lock_holder<'a>(
            &'a self,
            _: &'a mut Conn,
        ) -> EngineFuture<'a, Result<Option<LockInfo>, DbError>> {
            refused()
        }
        fn introspect<'a>(
            &'a self,
            _: &'a mut Conn,
            _: Read,
        ) -> EngineFuture<'a, Result<Pulled, DbError>> {
            refused()
        }
        fn read_rows<'a>(
            &'a self,
            _: &'a mut Conn,
            _: &'a Schema,
            _: &'a BTreeMap<TableName, RowScope>,
            _: Read,
        ) -> EngineFuture<'a, Result<ObservedRows, RowsError>> {
            Box::pin(async {
                Err(RowsError::Unreadable {
                    table: TableName::new("public", "t"),
                    why: "the stub engine has no server".into(),
                })
            })
        }
        fn execute_staged_statement<'a>(
            &'a self,
            _: &'a mut Conn,
            _: &'a Statement,
        ) -> EngineFuture<'a, Result<(), DbError>> {
            refused()
        }
    }

    #[test]
    fn a_third_engine_implements_the_trait_through_a_trait_object() {
        let engine: &'static dyn Engine = &Stub;
        assert_eq!(engine.driver(), Driver::Postgres);
        assert_eq!(engine.name(), "Stub");
    }

    #[test]
    fn the_trait_object_can_be_shared_between_threads() {
        fn needs_send_sync<T: Send + Sync + ?Sized>(_: &T) {}
        needs_send_sync::<dyn Engine>(&Stub);
    }
}
