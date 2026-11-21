//! The ledger as SQL: the table this tool keeps its recorded states in, and the
//! functions that create it and write to it (item 5 of #2950, DEC-2950.11).
//!
//! The shapes and the meaning of a ledger row live in `pbps_db::ledger`; the
//! statements are here, as `pbps_pg::state` and `pbps_mssql::state` hold theirs.
//! Every function is synchronous and takes the connection of `pbps_db::sqlite`,
//! and none of them names a driver type or a `Conn`: the wiring (item 7) runs
//! them on a blocking thread behind the trait of `pbps_db::engine`.
//!
//! This part holds the state table, the write path ([`ensure_tables`],
//! [`is_initialized`], [`record`], [`prune`]) and the reads ([`latest`], [`entry`],
//! [`deployed_revision`], [`has_applied_plan`], [`timeline`]). The lock and the
//! integrity check are later parts of the same item and add to the same list
//! of tables ([`ddl::RECIPE`]); nothing here is written for one table only.
//!
//! # Where the ledger lives, and how it is named
//!
//! In the file itself, in `main`. Every statement names it `main.`-qualified,
//! because an unqualified name finds a `TEMP` table of that name first, and the
//! reader (`catalog`) reads `main` only. The names are
//! [`pbps_db::ledger::STATE_TABLE_NAME`], which [`crate::names`] already
//! reserves in the one namespace tables, views and indexes share.
//!
//! # Why the ledger is plain
//!
//! It lives in the application's file, and the oldest library an application
//! may run is the project's version floor (3.35). So the stored text must open
//! there. It is therefore not `STRICT` (3.37: an older library cannot read the
//! schema of a file that has one), calls no JSON function (`state_json` is
//! `TEXT`; the functions are built in from 3.38), and uses only what is old:
//! `AUTOINCREMENT`, `CHECK`, `length` and `strftime`. `RETURNING` and `ON
//! CONFLICT` appear in statements this tool runs on its own library, never in
//! stored text.

mod busy;
mod ddl;
mod read;
mod rows;
mod write;

pub use busy::BUSY_WAIT;
pub use ddl::{ensure_tables, is_initialized};
pub use read::{deployed_revision, entry, has_applied_plan, latest, timeline};
pub use write::{prune, record};

/// The state table as the statements spell it. See the module header on `main.`.
pub const STATE_TABLE: &str = "main.__pbps_state";

/// The width of `reason`, in **characters**, which is what `length()` counts and
/// what PostgreSQL's `varchar(1000)` counts. A state recorded under any engine
/// therefore fits any ledger. SQL Server's own limit is in UTF-16 units and is
/// that engine's.
pub const REASON_CHARS: usize = 1000;

/// The width of `operator`, in characters.
pub const OPERATOR_CHARS: usize = 128;

/// A failure reason cut to what the ledger's `reason` holds.
///
/// For the best-effort audit paths, where a failed attempt that cannot be
/// recorded is the opposite of what the row exists for. A `--reason` the user
/// supplied is not cut and fails loudly instead (the column's `CHECK`): an audit
/// text silently shortened is a different kind of loss.
///
/// The cut is made here, in characters, and the `CHECK` is the backstop and not
/// the bound: `length()` stops counting at an embedded NUL, so a text with one
/// would pass it however long it is.
#[must_use]
pub fn truncate_reason(text: &str) -> String {
    text.chars().take(REASON_CHARS).collect()
}

#[cfg(test)]
mod tests;
