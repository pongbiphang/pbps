//! SQLite as the third engine pbps supports (#2950).
//!
//! SQLite is the C library applications run, not a server: a database is a
//! file or a private in-memory database, there is no schema namespace, no role
//! and no grant, constraints carry no names in the catalog, and about a third of
//! ordinary schema changes need the table to be rebuilt. This crate holds what
//! is specific to it, the way `pbps-pg` and `pbps-mssql` hold theirs, and it is
//! built before the engine is wired into the CLI (DEC-2950.1): a `Driver` or
//! `Conn` variant today would break every exhaustive `match` that route steps 4
//! to 6 of #2798 are still rewriting.
//!
//! # What is here
//!
//! This is item 1 of #2950's plan: the skeleton and the test harness. The
//! connection is `pbps_db::sqlite`, because the driver crate is named in one
//! file of the transport crate and nowhere else (ARCHITECTURE constraint 9).
//! Each later item fills one module:
//!
//! | Module | Item | Holds |
//! |---|---|---|
//! | [`dialect`] | 2 | names, quoting, the type table, what is refused by name |
//! | [`emit`] | 3 | the SQL for the changes SQLite can make in place |
//! | [`catalog`] | 4 | the reader: pragmas, then a tokenizer for what only the stored text holds |
//! | [`state`] | 5 | the ledger, the plan record and the lock, as SQL |
//! | [`preflight`] | 6 | checks before a plan runs |
//!
//! The functions of those modules are synchronous and take a
//! `pbps_db::sqlite::Connection`; the wiring (item 7) runs them on a blocking
//! thread behind the async `Conn`.

use std::borrow::Cow;

use pbps_dialect::TransactionFraming;

pub mod catalog;
pub mod dialect;
pub mod emit;
pub mod preflight;
pub mod state;
#[cfg(test)]
mod tests;

pub use pbps_db::sqlite::Version;

/// The oldest SQLite a project targets unless it says otherwise: the first
/// release with `ALTER TABLE ... DROP COLUMN`. The library pbps links is at or
/// above it; the project field that raises it is item 2's.
pub const DEFAULT_VERSION_FLOOR: Version = Version::new(3, 35, 0);

/// How an apply frames its transaction.
///
/// `BEGIN IMMEDIATE` takes the write lock at once, so a concurrent writer is
/// refused instead of failing at its first write, and the catalog read-back
/// and the changes share one transaction. A plain `BEGIN` is deferred: in
/// write-ahead mode a read that later tries to write after another connection
/// committed fails with `database is locked` and not with the stale read it
/// would otherwise have continued from.
pub const TRANSACTION: TransactionFraming = TransactionFraming {
    begin: Cow::Borrowed("BEGIN IMMEDIATE"),
    commit: Cow::Borrowed("COMMIT"),
    rollback: Cow::Borrowed("ROLLBACK"),
};
