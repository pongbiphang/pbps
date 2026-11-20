//! Waiting for another writer, and what a busy refusal says.

use std::time::{Duration, Instant};

use pbps_db::DbError;
use pbps_db::sqlite::Connection;
use pbps_model::StateKind;

use super::{in_both_journal_modes, number, snapshot};
use crate::state::busy::is_busy;
use crate::state::ddl::ensure_tables_within;
use crate::state::write::record_within;
use crate::state::{STATE_TABLE, ensure_tables, is_initialized};

const SHORT: Duration = Duration::from_millis(200);

fn driver(code: Option<&str>) -> DbError {
    DbError::Driver {
        message: "x".into(),
        code: code.map(str::to_owned),
        diagnostic: None,
    }
}

#[test]
fn every_busy_code_is_busy_and_nothing_else_is() {
    // 5 busy, 261 recovery, 517 stale snapshot, 773 timeout.
    for code in ["5", "261", "517", "773"] {
        assert!(is_busy(&driver(Some(code))), "{code}");
        // Still busy once a sentence is put around it.
        assert!(
            is_busy(&driver(Some(code)).context("while recording")),
            "{code}"
        );
    }
    // 6 is "locked" in the same connection, 275 a CHECK, 1555 a key.
    for code in [Some("6"), Some("275"), Some("1555"), Some("x"), None] {
        assert!(!is_busy(&driver(code)), "{code:?}");
    }
    assert!(!is_busy(&DbError::Refused("5".into())));
}

#[test]
fn a_free_database_writes_at_once() {
    in_both_journal_modes(|fresh, _| {
        let started = Instant::now();
        record_within(fresh.connection(), &snapshot(StateKind::Apply, "op"), SHORT)
            .expect("recorded");
        assert!(started.elapsed() < SHORT, "{:?}", started.elapsed());
    });
}

#[test]
fn a_database_another_connection_is_writing_is_reported_busy_after_the_wait() {
    in_both_journal_modes(|fresh, second| {
        second.execute_batch("BEGIN IMMEDIATE").expect("held");
        let started = Instant::now();
        let refused = record_within(fresh.connection(), &snapshot(StateKind::Apply, "op"), SHORT)
            .expect_err("busy");
        assert!(
            started.elapsed() >= SHORT,
            "it waited: {:?}",
            started.elapsed()
        );
        let pbps_db::ledger::LedgerError::Db(error) = refused else {
            panic!("{refused:?}");
        };
        assert!(is_busy(&error), "{error:?}");
        let said = error.to_string();
        assert!(
            said.contains("not this tool's lock") && said.contains("pbps unlock"),
            "{said}"
        );
        assert!(!fresh.connection().in_transaction());
        // The wait ends the moment the other writer does.
        second.execute_batch("ROLLBACK").expect("released");
        ensure_tables_within(fresh.connection(), SHORT).expect("created");
    });
}

#[test]
fn an_existing_ledger_is_not_stopped_by_a_writer_elsewhere_but_a_missing_one_is() {
    in_both_journal_modes(|fresh, second| {
        ensure_tables(fresh.connection()).expect("created");
        second.execute_batch("BEGIN IMMEDIATE").expect("held");
        // Nothing is missing, so nothing is created and nothing waits. (Whether
        // the statement is sent cannot be told on this library, which does not
        // lock for it; see `ensure_tables`.)
        let started = Instant::now();
        ensure_tables_within(fresh.connection(), SHORT).expect("nothing to create");
        assert!(is_initialized(fresh.connection()).expect("asked"));
        assert!(started.elapsed() < SHORT, "{:?}", started.elapsed());
        second.execute_batch("ROLLBACK").expect("released");
    });
    // Negative: with the ledger absent the same hold does stop it, because a
    // creation does write.
    in_both_journal_modes(|fresh, second| {
        second.execute_batch("BEGIN IMMEDIATE").expect("held");
        let refused = ensure_tables_within(fresh.connection(), SHORT).expect_err("busy");
        assert!(is_busy(&refused), "{refused:?}");
        second.execute_batch("ROLLBACK").expect("released");
    });
}

#[test]
fn a_failed_commit_of_the_creation_leaves_no_transaction_open() {
    // In the delete journal mode a reader blocks the commit of a writer, so the
    // creation's last step fails after its statements ran.
    let mut fresh = crate::tests::Fresh::file();
    let reader =
        Connection::open(fresh.path(), pbps_db::sqlite::OpenMode::ExistingOnly).expect("second");
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM sqlite_schema")
        .expect("a read held open");
    let c = fresh.connection();
    let refused = ensure_tables_within(c, SHORT).expect_err("the commit is refused");
    assert!(is_busy(&refused), "{refused:?}");
    assert!(!c.in_transaction(), "the connection was left as it was");
    assert_eq!(number(c, "SELECT count(*) FROM sqlite_schema"), 0);
    reader.execute_batch("ROLLBACK").expect("released");
    ensure_tables_within(c, SHORT).expect("created once the reader left");
    assert_eq!(number(c, &format!("SELECT count(*) FROM {STATE_TABLE}")), 0);
}

#[test]
fn a_failure_that_is_not_busy_is_not_described_as_busy() {
    let mut fresh = crate::tests::Fresh::memory();
    let c = fresh.connection();
    let mut broken = snapshot(StateKind::Apply, "op");
    broken.reason = Some(pbps_model::Text::from("x".repeat(2000)));
    let error = match record_within(c, &broken, SHORT) {
        Err(pbps_db::ledger::LedgerError::Db(error)) => error,
        other => panic!("{other:?}"),
    };
    assert!(!is_busy(&error));
    assert!(!error.to_string().contains("busy"), "{error}");
}
