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
//! The operations any engine needs in order to plan and apply: the ledger and
//! the lock, the catalog read, and executing one staged statement. Those are
//! required: there is no default body, because a default would let a new
//! engine compile while silently lacking part of the ledger.
//!
//! Capabilities only some engines have arrive as methods with a stated default
//! (#2798): the questions `doctor` asks, and plan target assertions. A default
//! never reads as success. Advisory data an engine may lack answers "not
//! available" in words that name the engine ([`not_available`]); knowledge
//! `doctor` requires stays unanswered ([`not_answered`]), which `doctor` reports
//! as unknown and which still withholds its readiness claim. An engine that
//! knows a feature does not apply says so itself (SQL Server's empty partition
//! report): a default is for an engine that gave no answer, and an unreadable
//! query is never read as an absent feature.
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

use std::time::Duration;

use pbps_dialect::Statement;
use pbps_model::{IdsFile, ObservedRows, RowScope, Schema, StateSnapshot, TableName};

use crate::catalog::{Pulled, RowsError};
use crate::doctor::{Ask, Capabilities, LockReadGap, Permissions, RunwayAsk, RunwayReport};
use crate::ledger::{StateRevision, StoredEntry};
use crate::query::AssertionResult;
use crate::resolver::{Candidate, Discovery};
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

/// The words of an advisory question an engine does not answer: not a failure
/// and not a positive result (#2798).
pub fn not_available(operation: &str, engine: &str, reason: &str) -> String {
    format!("{operation} is not available for {engine}: {reason}")
}

/// The refusal of a question `doctor` requires and an engine did not answer.
/// An error, never an empty or true answer, so the caller reports unknown.
pub fn not_answered(operation: &str, engine: &str) -> DbError {
    DbError::Refused(format!(
        "{operation} is not answered for {engine}: this engine has not implemented the question"
    ))
}

/// What an engine with no resolver discovery reports: a candidate that says
/// why it is unavailable and nothing else, so a missing advisory is visible
/// and cannot be read as a clean environment.
pub fn discovery_not_available(engine: &str) -> Discovery {
    Discovery::unverified(
        Default::default(),
        None,
        Candidate::Unavailable {
            reason: not_available(
                "resolver discovery",
                engine,
                "this engine has no discovery queries",
            ),
        },
    )
}

/// What an engine with no partition-runway question refuses with; `doctor`
/// shows it as a warning that the runway is undetermined (#1789).
pub fn partition_runway_not_available(engine: &str) -> DbError {
    DbError::Refused(not_available(
        "partition runway",
        engine,
        "this engine has not declared whether it has RANGE partitions to run out",
    ))
}

/// The words a plan reports when `--assert` reaches an engine without target
/// assertions (#2401). It names the engine; no assertion login can fix it.
pub fn assertions_unsupported(engine: &str) -> String {
    format!("{engine} does not support plan target assertions")
}

/// What the planning target is held to by `--assert`: the identity read from
/// the planning connection, kept private to the engine that read it.
///
/// Boxed rather than an associated type because it returns an
/// [`AssertionResult`], not certified evidence, so no command needs to name the
/// engine's type; a neutral associated target can replace it later without a
/// caller edit.
pub trait AssertionTarget: Send + Sync {
    /// Runs one assertion on a fresh connection from `connection`, after the
    /// engine has checked that it is the planning database, a separate login
    /// and effectively read-only. Every check the engine makes precedes the SQL.
    fn run<'a>(
        &'a self,
        connection: &'a str,
        sql: &'a str,
        timeout: Duration,
    ) -> EngineFuture<'a, Result<AssertionResult, DbError>>;
}

/// An engine's answer to "what do assertions run against?". Two outcomes, so an
/// engine without assertions is a named answer and not an error to be mistaken
/// for an unreadable target.
pub enum TargetAssertions {
    Unsupported,
    Supported(Box<dyn AssertionTarget>),
}

/// The connected operations every engine provides.
///
/// The ledger, catalog and execution methods are required; the `doctor` and
/// assertion questions below them have stated defaults (see the module
/// header). Methods take `&self` so an
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
    ///
    /// Returns the row of a dead holder this call cleared on the way, when it
    /// cleared one, so the command can say whose it was (DEC-2950.18). `None`
    /// is "cleared nobody's", which is every lock on an engine whose lock
    /// cannot outlive its holder's proof of life.
    fn lock<'a>(
        &'a self,
        conn: &'a mut Conn,
        holder: &'a str,
    ) -> EngineFuture<'a, Result<Option<LockInfo>, LedgerError>>;

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

    // -- What `doctor` asks (provided; see the module header) --------------

    /// Advisory target observations; never acquires or contacts a resolver.
    /// Default: a discovery that says it is not available.
    fn resolver_discovery<'a>(
        &'a self,
        _conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<Discovery, DbError>> {
        Box::pin(async move { Ok(discovery_not_available(self.name())) })
    }

    /// The server's version, as text an operator recognises. Required
    /// knowledge; default: not answered.
    fn server_version<'a>(
        &'a self,
        _conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<String, DbError>> {
        Box::pin(async move { Err(not_answered("the server version", self.name())) })
    }

    /// What the connected server can do. `version` is what
    /// [`Engine::server_version`] answered, where it answered. Required
    /// knowledge; default: not answered, never another engine's true values.
    fn capabilities<'a>(
        &'a self,
        _conn: &'a mut Conn,
        _version: Option<&'a str>,
    ) -> EngineFuture<'a, Result<Capabilities, DbError>> {
        Box::pin(async move { Err(not_answered("the server capabilities", self.name())) })
    }

    /// How the ledger's tables differ from the recipe they are created from,
    /// one line each; empty when they match or are not there yet. Required
    /// knowledge; default: not answered, never an empty list that reads clean.
    fn ledger_problems<'a>(
        &'a self,
        _conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<Vec<String>, DbError>> {
        Box::pin(async move { Err(not_answered("the ledger diagnostics", self.name())) })
    }

    /// What the connected account holds, with each gap's securable spelled the
    /// way this engine's `GRANT` names it. Required knowledge; default: not
    /// answered, never `Permissions::default()`.
    fn permissions<'a>(
        &'a self,
        _conn: &'a mut Conn,
        _project_ids: &'a IdsFile,
        _ask: &'a Ask<'a>,
    ) -> EngineFuture<'a, Result<Permissions, DbError>> {
        Box::pin(async move { Err(not_answered("the account's permissions", self.name())) })
    }

    /// Whether each partitioned parent's partitions reach far enough past its
    /// rows (#1789). Advisory; default: not available.
    fn partition_runway<'a>(
        &'a self,
        _conn: &'a mut Conn,
        _ask: &'a RunwayAsk<'a>,
        _project_ids: &'a IdsFile,
    ) -> EngineFuture<'a, Result<RunwayReport, DbError>> {
        Box::pin(async move { Err(partition_runway_not_available(self.name())) })
    }

    /// Why a fresh session of the same identity could not read the lock, to
    /// choose the remedy (#822). Default: unexplained, the observation that
    /// never invents a grant.
    fn lock_read_gap<'a>(
        &'a self,
        _conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<LockReadGap, DbError>> {
        Box::pin(async { Ok(LockReadGap::Unexplained) })
    }

    // -- Plan target assertions (provided) ---------------------------------

    /// Reads what `plan --assert` holds the assertion login to, from the
    /// planning connection. Default: [`TargetAssertions::Unsupported`], so
    /// an engine that has not implemented assertions is refused by name.
    fn read_assertion_target<'a>(
        &'a self,
        _conn: &'a mut Conn,
    ) -> EngineFuture<'a, Result<TargetAssertions, DbError>> {
        Box::pin(async { Ok(TargetAssertions::Unsupported) })
    }
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
        ) -> EngineFuture<'a, Result<Option<LockInfo>, LedgerError>> {
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

    /// The `Stub` above implements none of the provided methods and still
    /// compiles, so the doctor and assertion questions are optional. What their
    /// defaults say is pinned below at the function level, because the methods
    /// need a connection and no offline `Conn` exists.
    #[test]
    fn an_advisory_question_an_engine_lacks_is_worded_as_not_available_for_it() {
        assert_eq!(
            not_available("partition runway", "Stub", "no RANGE partitions declared"),
            "partition runway is not available for Stub: no RANGE partitions declared"
        );
        let DbError::Refused(text) = partition_runway_not_available("Stub") else {
            panic!("a missing partition runway must be a refusal, not an empty report");
        };
        assert!(
            text.starts_with("partition runway is not available for Stub: "),
            "{text}"
        );
    }

    #[test]
    fn a_missing_resolver_discovery_is_an_unavailable_candidate_not_a_clean_environment() {
        let discovery = discovery_not_available("Stub");
        assert!(discovery.observations.is_empty());
        assert!(discovery.extensions.is_none());
        let Candidate::Unavailable { reason } = &discovery.candidate else {
            panic!("an engine without discovery must not suggest a candidate");
        };
        assert!(
            reason.starts_with("resolver discovery is not available for Stub: "),
            "{reason}"
        );
    }

    #[test]
    fn an_unanswered_required_question_is_a_refusal_naming_the_engine_never_a_value() {
        // Each of these is an `Err` in the trait defaults: an `Ok` with an
        // empty or true value would let `doctor` claim readiness it never
        // established.
        for operation in [
            "the server version",
            "the server capabilities",
            "the ledger diagnostics",
            "the account's permissions",
        ] {
            let DbError::Refused(text) = not_answered(operation, "Stub") else {
                panic!("{operation} must be refused when unanswered");
            };
            assert!(
                text.starts_with(&format!("{operation} is not answered for Stub")),
                "{text}"
            );
            assert!(!text.contains("not available"), "{text}");
        }
    }

    #[test]
    fn the_assertion_refusal_names_the_engine() {
        assert_eq!(
            assertions_unsupported("Stub"),
            "Stub does not support plan target assertions"
        );
    }

    #[test]
    fn the_trait_object_can_be_shared_between_threads() {
        fn needs_send_sync<T: Send + Sync + ?Sized>(_: &T) {}
        needs_send_sync::<dyn Engine>(&Stub);
    }
}
