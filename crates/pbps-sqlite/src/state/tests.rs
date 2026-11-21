//! Tests of the ledger's write path against a real SQLite (#2950, item 5).
//!
//! The engine is the oracle: the stored text, the locks, the widths and the
//! atomicity are what it does, not what a mock says. Anything about waiting or
//! contention runs on a file with a second connection, in both journal modes.

use pbps_db::sqlite::{Connection, OpenMode};
use pbps_model::{IdsFile, Schema, StateKind, StateSnapshot};

use crate::tests::Fresh;

mod busy;
mod read;
mod recipe;
mod write;

/// A snapshot of an empty schema, as `operator` recorded it.
pub(super) fn snapshot(kind: StateKind, operator: &str) -> StateSnapshot {
    StateSnapshot::new(kind, Schema::default(), IdsFile::default(), operator)
}

pub(super) fn number(c: &Connection, sql: &str) -> i64 {
    c.query(sql, &[]).expect("query")[0]
        .integer(0)
        .expect("an integer")
}

pub(super) fn text(c: &Connection, sql: &str) -> String {
    c.query(sql, &[]).expect("query")[0]
        .text(0)
        .expect("text")
        .to_owned()
}

/// Runs `body` on a fresh file in the delete journal mode and again in WAL, with
/// a second connection to the same file.
pub(super) fn in_both_journal_modes(body: impl Fn(&mut Fresh, &Connection)) {
    for wal in [false, true] {
        let mut fresh = Fresh::file();
        if wal {
            assert_eq!(text(fresh.connection(), "PRAGMA journal_mode = WAL"), "wal");
        }
        let second = Connection::open(fresh.path(), OpenMode::ExistingOnly).expect("second");
        body(&mut fresh, &second);
    }
}
