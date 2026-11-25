//! The engine through its trait, on a real SQLite file (#2950, item 7).
//!
//! Every call goes through `&dyn Engine` and a `Conn`, as a command's does, and
//! every future is polled exactly once: an engine whose calls are made in place
//! must be ready without a runtime (DEC-2950.17).

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use futures_util::FutureExt;
use pbps_db::catalog::RowsError;
use pbps_db::doctor::RunwayAsk;
use pbps_db::engine::{Engine, Read};
use pbps_db::{Conn, DbError, Driver, LedgerError};
use pbps_model::{IdsFile, RowScope, Schema, StateKind, StateSnapshot, TableName};

use super::SqliteEngine;
use crate::TRANSACTION;
use crate::tests::Fresh;

const ENGINE: &dyn Engine = &SqliteEngine;
const FIRST: &str = "first [host a, pid 1, application_name \"pbps-sqlite\"]";
const SECOND: &str = "second [host b, pid 2, application_name \"pbps-sqlite\"]";

/// The value of a future that must not wait.
fn now<T>(future: impl Future<Output = T>) -> T {
    future
        .now_or_never()
        .expect("a SQLite call is made in place and is ready at the first poll")
}

/// Another connection to the database of `fresh`, as a second command has.
fn connect(fresh: &Fresh) -> Conn {
    let path = fresh.path().to_str().expect("a UTF-8 path");
    now(Conn::connect(Driver::Sqlite, path)).expect("the file opens")
}

fn locked_by(result: Result<Option<pbps_db::LockInfo>, LedgerError>) -> String {
    match result {
        Err(LedgerError::Locked(info)) => info.locked_by,
        other => panic!("expected a held lock: {other:?}"),
    }
}

#[test]
fn the_ledger_is_written_and_read_back_through_the_trait() {
    let fresh = Fresh::file();
    let mut conn = connect(&fresh);
    assert_eq!(ENGINE.driver(), Driver::Sqlite);
    assert_eq!(ENGINE.name(), "SQLite");
    assert!(!now(ENGINE.is_initialized(&mut conn)).expect("read"));
    // Not initialised is its own answer, never "no entry".
    assert!(matches!(
        now(ENGINE.latest(&mut conn)),
        Err(LedgerError::NotInitialized)
    ));

    assert!(now(ENGINE.lock(&mut conn, FIRST)).expect("free").is_none());
    assert!(now(ENGINE.is_initialized(&mut conn)).expect("read"));
    let snapshot = StateSnapshot::new(
        StateKind::Apply,
        Schema::default(),
        IdsFile::default(),
        "first",
    );
    let id = now(ENGINE.record(&mut conn, &snapshot)).expect("recorded");
    let latest = now(ENGINE.latest(&mut conn))
        .expect("read")
        .expect("an entry");
    assert_eq!(latest.id, id);
    assert_eq!(now(ENGINE.timeline(&mut conn, 10)).expect("read").len(), 1);
    assert!(
        now(ENGINE.entry(&mut conn, Some(id)))
            .expect("read")
            .is_some()
    );
    assert!(
        now(ENGINE.ledger_problems(&mut conn))
            .expect("read")
            .is_empty()
    );
    assert!(now(ENGINE.unlock(&mut conn)).expect("released"));

    let version = now(ENGINE.server_version(&mut conn)).expect("the library's version");
    assert!(version.starts_with("SQLite 3."), "{version}");
}

#[test]
fn the_lock_lives_on_the_connection_that_took_it_until_unlock() {
    let fresh = Fresh::file();
    let mut first = connect(&fresh);
    let mut second = connect(&fresh);
    assert!(now(ENGINE.lock(&mut first, FIRST)).expect("free").is_none());

    // The guard is parked, not dropped: the holder is proved alive, so the
    // second run is refused and clears nothing.
    assert_eq!(locked_by(now(ENGINE.lock(&mut second, SECOND))), FIRST);
    let holder = now(ENGINE.lock_holder(&mut second))
        .expect("read")
        .expect("held");
    assert_eq!(holder.locked_by, FIRST);

    assert!(now(ENGINE.unlock(&mut first)).expect("released"));
    assert!(
        now(ENGINE.lock_holder(&mut second))
            .expect("read")
            .is_none()
    );
    assert!(
        now(ENGINE.lock(&mut second, SECOND))
            .expect("free")
            .is_none()
    );
    assert!(now(ENGINE.unlock(&mut second)).expect("released"));
    // Nothing is held now, and saying so is not an error.
    assert!(!now(ENGINE.unlock(&mut second)).expect("nothing to release"));
}

#[test]
fn a_run_that_ended_without_unlocking_is_cleared_and_named_by_the_next() {
    let fresh = Fresh::file();
    let mut dead = connect(&fresh);
    assert!(now(ENGINE.lock(&mut dead, FIRST)).expect("free").is_none());
    // The command ends without releasing: its connection goes, and the parked
    // guard with it.
    drop(dead);

    let mut next = connect(&fresh);
    let cleared = now(ENGINE.lock(&mut next, SECOND))
        .expect("the stale row is cleared")
        .expect("and the run is told whose it was");
    assert_eq!(cleared.locked_by, FIRST);
    let notice = cleared.cleared_notice();
    assert!(
        notice.contains(FIRST) && notice.contains("stale"),
        "{notice}"
    );
    assert!(now(ENGINE.unlock(&mut next)).expect("released"));
}

#[test]
fn a_second_lock_on_one_connection_is_refused_and_the_first_stays_held() {
    let fresh = Fresh::file();
    let mut conn = connect(&fresh);
    let mut other = connect(&fresh);
    assert!(now(ENGINE.lock(&mut conn, FIRST)).expect("free").is_none());
    assert!(now(ENGINE.lock(&mut conn, SECOND)).is_err());
    // The refusal neither replaced the row nor let go of the first guard.
    assert_eq!(locked_by(now(ENGINE.lock(&mut other, SECOND))), FIRST);
    assert!(now(ENGINE.unlock(&mut conn)).expect("the first lock's release"));
}

#[test]
fn unlock_without_a_guard_clears_whoever_holds_the_row() {
    let fresh = Fresh::file();
    let mut dead = connect(&fresh);
    assert!(now(ENGINE.lock(&mut dead, FIRST)).expect("free").is_none());
    drop(dead);

    // `pbps unlock`: a connection that took no lock, asked to clear one.
    let mut operator = connect(&fresh);
    assert!(now(ENGINE.unlock(&mut operator)).expect("cleared"));
    assert!(
        now(ENGINE.lock_holder(&mut operator))
            .expect("read")
            .is_none()
    );
}

#[test]
fn unlock_leaves_a_parked_value_that_is_not_its_guard() {
    let fresh = Fresh::file();
    let mut conn = connect(&fresh);
    conn.sqlite()
        .expect("a SQLite connection")
        .park(Box::new(7_u32))
        .expect("an empty slot");
    assert!(matches!(
        now(ENGINE.unlock(&mut conn)),
        Err(DbError::Refused(_))
    ));
    let parked = conn
        .sqlite()
        .expect("a SQLite connection")
        .unpark()
        .expect("still parked");
    assert_eq!(parked.downcast_ref::<u32>(), Some(&7));
}

#[test]
fn the_catalog_read_follows_where_the_command_says_it_runs() {
    let fresh = Fresh::file();
    let mut conn = connect(&fresh);
    let before = now(ENGINE.introspect(&mut conn, Read::Snapshot)).expect("read");
    assert!(before.schema.tables.is_empty());

    now(conn.begin(&TRANSACTION)).expect("begun");
    now(conn.execute("CREATE TABLE built (id INTEGER PRIMARY KEY) STRICT")).expect("ddl");
    // What the open transaction built is read before it commits.
    let inside = now(ENGINE.introspect(&mut conn, Read::InsideOwnTransaction)).expect("read");
    assert_eq!(inside.schema.tables.len(), 1);
    // A snapshot read takes a transaction of its own, which cannot open here.
    assert!(now(ENGINE.introspect(&mut conn, Read::Snapshot)).is_err());
    now(conn.rollback(&TRANSACTION)).expect("rolled back");

    let after = now(ENGINE.introspect(&mut conn, Read::Snapshot)).expect("read");
    assert!(after.schema.tables.is_empty());
}

#[test]
fn managed_rows_are_refused_by_table_and_never_read_as_none() {
    let fresh = Fresh::file();
    let mut conn = connect(&fresh);
    let schema = Schema::default();
    let nothing = BTreeMap::new();
    let rows =
        now(ENGINE.read_rows(&mut conn, &schema, &nothing, Read::Snapshot)).expect("nothing asked");
    assert_eq!(rows, pbps_model::ObservedRows::default());

    let table = TableName::new("main", "currency");
    let asked = BTreeMap::from([(
        table.clone(),
        RowScope::Every {
            known: BTreeSet::new(),
        },
    )]);
    match now(ENGINE.read_rows(&mut conn, &schema, &asked, Read::Snapshot)) {
        Err(RowsError::Unreadable { table: named, .. }) => assert_eq!(named, table),
        other => panic!("expected a refusal naming the table: {other:?}"),
    }
}

#[test]
fn an_engine_without_partitions_reports_none_and_does_not_say_unknown() {
    let fresh = Fresh::file();
    let mut conn = connect(&fresh);
    let ask = RunwayAsk {
        parents: &[],
        horizon: None,
    };
    let report = now(ENGINE.partition_runway(&mut conn, &ask, &IdsFile::default())).expect("known");
    assert!(report.parents.is_empty());
    // The questions SQLite has no answer to keep their stated refusals.
    assert!(now(ENGINE.capabilities(&mut conn, None)).is_err());
}
