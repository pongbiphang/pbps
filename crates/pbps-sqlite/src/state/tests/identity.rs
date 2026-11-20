//! The environment identifier.

use std::thread;

use pbps_db::DbError;
use pbps_db::sqlite::{Connection, OpenMode};
use pbps_model::StateKind;

use super::{number, snapshot, text};
use crate::state::ddl::RECIPE;
use crate::state::{
    ENVIRONMENT_ID_CHARS, ENVIRONMENT_TABLE, ensure_tables, environment_identity, prune, record,
};
use crate::tests::Fresh;

fn is_identifier(id: &str) -> bool {
    id.len() == ENVIRONMENT_ID_CHARS
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[test]
fn the_identifier_is_written_when_the_ledger_is_made_and_never_changes() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    assert_eq!(
        environment_identity(c).expect("asked"),
        None,
        "no ledger, no identifier"
    );
    ensure_tables(c).expect("created");
    let id = environment_identity(c)
        .expect("asked")
        .expect("written with the ledger");
    assert!(is_identifier(&id), "{id}");
    // Nothing a later write does changes it.
    ensure_tables(c).expect("again");
    for _ in 0..3 {
        record(c, &snapshot(StateKind::Apply, "op")).expect("recorded");
    }
    prune(c, 1).expect("pruned");
    assert_eq!(environment_identity(c).expect("asked"), Some(id));
    assert_eq!(
        number(c, &format!("SELECT count(*) FROM {ENVIRONMENT_TABLE}")),
        1
    );
}

#[test]
fn two_databases_have_two_identifiers() {
    let mut a = Fresh::memory();
    let mut b = Fresh::memory();
    ensure_tables(a.connection()).expect("created");
    ensure_tables(b.connection()).expect("created");
    let (a, b) = (
        environment_identity(a.connection()).expect("asked"),
        environment_identity(b.connection()).expect("asked"),
    );
    assert!(a.is_some() && b.is_some());
    assert_ne!(a, b, "a 128-bit random value does not repeat");
}

#[test]
fn the_identifier_follows_a_copy_a_vacuum_and_a_reopen_and_names_a_lineage_not_a_file() {
    let mut fresh = Fresh::file();
    ensure_tables(fresh.connection()).expect("created");
    let id = environment_identity(fresh.connection()).expect("asked");
    fresh.reopen();
    assert_eq!(
        environment_identity(fresh.connection()).expect("asked"),
        id,
        "a reopen"
    );

    let dir = fresh.path().parent().expect("a directory").to_path_buf();
    let copy = dir.join("copy.db");
    std::fs::copy(fresh.path(), &copy).expect("a byte copy");
    let copied = Connection::open(&copy, OpenMode::ExistingOnly).expect("opens");
    assert_eq!(
        environment_identity(&copied).expect("asked"),
        id,
        "a byte copy"
    );

    let vacuumed = dir.join("vacuumed.db");
    fresh
        .connection()
        .execute_batch(&format!("VACUUM INTO '{}'", vacuumed.display()))
        .expect("vacuum");
    let vacuumed = Connection::open(&vacuumed, OpenMode::ExistingOnly).expect("opens");
    assert_eq!(
        environment_identity(&vacuumed).expect("asked"),
        id,
        "VACUUM INTO"
    );
    // Negative: another file made by the tool is another environment.
    let mut other = Fresh::file();
    ensure_tables(other.connection()).expect("created");
    assert_ne!(environment_identity(other.connection()).expect("asked"), id);
}

#[test]
fn a_ledger_made_before_the_identifier_has_none_until_a_write_and_a_read_never_invents_it() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    c.execute_batch(&RECIPE[0].create())
        .expect("a state table only");
    for _ in 0..3 {
        assert_eq!(environment_identity(c).expect("asked"), None);
    }
    assert_eq!(
        number(
            c,
            "SELECT count(*) FROM sqlite_schema WHERE name = '__pbps_environment'"
        ),
        0,
        "asking made nothing"
    );
    ensure_tables(c).expect("completed");
    let id = environment_identity(c)
        .expect("asked")
        .expect("now written");
    ensure_tables(c).expect("again");
    assert_eq!(environment_identity(c).expect("asked"), Some(id));
}

#[test]
fn a_table_that_is_damaged_is_an_error_and_never_absence() {
    // No row.
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    ensure_tables(c).expect("created");
    c.execute_batch(&format!("DELETE FROM {ENVIRONMENT_TABLE}"))
        .expect("emptied");
    match environment_identity(c) {
        Err(DbError::BadRow(message)) => assert!(message.contains("no row"), "{message}"),
        other => panic!("{other:?}"),
    }
    // A value that is not an identifier (the right length, wrong characters).
    c.execute_batch(&format!(
        "INSERT INTO {ENVIRONMENT_TABLE} VALUES (1, '{}')",
        "z".repeat(ENVIRONMENT_ID_CHARS)
    ))
    .expect("written by hand");
    assert!(matches!(environment_identity(c), Err(DbError::BadRow(_))));
    // Uppercase is not what is written either.
    c.execute_batch(&format!("DELETE FROM {ENVIRONMENT_TABLE}"))
        .expect("emptied");
    c.execute_batch(&format!(
        "INSERT INTO {ENVIRONMENT_TABLE} VALUES (1, '{}')",
        "A".repeat(ENVIRONMENT_ID_CHARS)
    ))
    .expect("written by hand");
    assert!(matches!(environment_identity(c), Err(DbError::BadRow(_))));
    // A view in its name is refused.
    let mut other = Fresh::memory();
    other
        .connection()
        .execute_batch("CREATE VIEW __pbps_environment AS SELECT 1 AS id")
        .expect("ddl");
    assert!(matches!(
        environment_identity(other.connection()),
        Err(DbError::Refused(_))
    ));
    // Negative: the row `ensure_tables` writes is fine.
    let mut fine = Fresh::memory();
    ensure_tables(fine.connection()).expect("created");
    assert!(
        environment_identity(fine.connection())
            .expect("asked")
            .is_some()
    );
}

#[test]
fn the_table_holds_one_row_and_a_second_cannot_be_written() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    ensure_tables(c).expect("created");
    let second = c.execute_batch(&format!(
        "INSERT INTO {ENVIRONMENT_TABLE} VALUES (2, '{}')",
        "a".repeat(ENVIRONMENT_ID_CHARS)
    ));
    assert!(second.is_err(), "the CHECK refuses a second row");
    let short = c.execute_batch(&format!(
        "INSERT OR REPLACE INTO {ENVIRONMENT_TABLE} VALUES (1, 'abc')"
    ));
    assert!(short.is_err(), "and a value of the wrong length");
    assert!(
        text(
            c,
            &format!("SELECT environment_id FROM {ENVIRONMENT_TABLE}")
        )
        .len()
            == ENVIRONMENT_ID_CHARS
    );
}

#[test]
fn creators_racing_on_one_file_leave_one_identifier_and_all_read_it() {
    for _ in 0..5 {
        let fresh = Fresh::file();
        let path = fresh.path().to_path_buf();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(6));
        let handles: Vec<_> = (0..6)
            .map(|_| {
                let (path, barrier) = (path.clone(), barrier.clone());
                thread::spawn(move || {
                    let c = Connection::open(&path, OpenMode::ExistingOnly).expect("opens");
                    barrier.wait();
                    ensure_tables(&c).expect("created");
                    environment_identity(&c).expect("asked")
                })
            })
            .collect();
        let ids: Vec<_> = handles
            .into_iter()
            .map(|h| h.join().expect("joined"))
            .collect();
        assert!(ids[0].is_some());
        assert!(ids.iter().all(|id| *id == ids[0]), "{ids:?}");
        let c = Connection::open(&path, OpenMode::ExistingOnly).expect("opens");
        assert_eq!(
            number(&c, &format!("SELECT count(*) FROM {ENVIRONMENT_TABLE}")),
            1
        );
    }
}
