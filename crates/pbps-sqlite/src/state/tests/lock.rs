//! The lock: the row, the operating-system lock that proves its holder alive, and
//! what happens when either is missing.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use pbps_db::DbError;
use pbps_db::LockHolder;
use pbps_db::ledger::{LOCKED_BY_CHARS, LedgerError, LockInfo};
use pbps_db::sqlite::{Connection, OpenMode};

use super::{in_both_journal_modes, number, text};
use crate::state::busy::is_busy;
use crate::state::lock::lock_within;
use crate::state::{
    LOCK_FILE_SUFFIX, LOCK_TABLE, Proof, RowOnly, Taken, ensure_tables, lock, lock_holder, release,
    session_application_name, session_lookup, sidecar_path, unlock,
};
use crate::tests::Fresh;

const HOLDER: &str = "leon [host build-7, pid 4242, application_name \"pbps-sqlite\"]";

fn open(path: &Path) -> Connection {
    Connection::open(path, OpenMode::ExistingOnly).expect("opens")
}

fn rows(c: &Connection) -> i64 {
    number(c, &format!("SELECT count(*) FROM {LOCK_TABLE}"))
}

fn row_proof(c: &Connection) -> String {
    text(c, &format!("SELECT proof FROM {LOCK_TABLE}"))
}

/// Whether the operating-system lock on the sidecar of `database` is free now:
/// it can be taken, and is let go again.
fn sidecar_is_free(database: &Path) -> bool {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(sidecar_path(database))
        .expect("the sidecar opens");
    let free = file.try_lock().is_ok();
    if free {
        file.unlock().expect("unlocked");
    }
    free
}

fn locked_info(result: Result<Taken, LedgerError>) -> LockInfo {
    match result {
        Err(LedgerError::Locked(info)) => info,
        other => panic!("expected the lock to be held by another: {other:?}"),
    }
}

/// A stale row as a dead run leaves it: for a proof of `file`, with the sidecar
/// a run on this connection would lock.
fn stale_row(c: &Connection, holder: &str, proof: &str) {
    ensure_tables(c).expect("created");
    let sidecar = if proof == "file" {
        // A lock taken and dropped leaves exactly that row; keep its sidecar.
        let taken = lock(c, "probe").expect("a free lock");
        drop(taken);
        let key = text(c, &format!("SELECT sidecar FROM {LOCK_TABLE}"));
        c.execute_batch(&format!("DELETE FROM {LOCK_TABLE}"))
            .expect("emptied");
        format!("'{key}'")
    } else {
        "NULL".to_owned()
    };
    c.execute_batch(&format!(
        "INSERT INTO {LOCK_TABLE} (id, locked_by, proof, sidecar, token) \
         VALUES (1, '{holder}', '{proof}', {sidecar}, 'ghost')"
    ))
    .expect("a row left behind");
}

#[test]
fn a_lock_is_a_row_and_an_os_lock_on_a_sidecar_that_never_meets_sqlites_own_files() {
    let mut fresh = Fresh::file();
    let path = fresh.path().to_path_buf();
    let taken = lock(fresh.connection(), HOLDER).expect("locked");
    assert_eq!(taken.guard.proof(), &Proof::File);
    assert!(taken.cleared.is_none());
    assert_eq!(rows(fresh.connection()), 1);
    assert_eq!(row_proof(fresh.connection()), "file");
    let sidecar = sidecar_path(&path);
    assert!(sidecar.exists());
    assert_eq!(
        sidecar.file_name().expect("a name").to_string_lossy(),
        format!("fresh.db{LOCK_FILE_SUFFIX}")
    );
    // Not a name SQLite makes for itself.
    for own in ["-journal", "-wal", "-shm"] {
        assert_ne!(LOCK_FILE_SUFFIX, own);
    }
    assert!(!sidecar_is_free(&path), "held while the guard lives");
    release(fresh.connection(), taken.guard).expect("released");
    assert!(sidecar_is_free(&path));
}

#[test]
fn a_second_run_is_refused_naming_the_holder_while_the_first_is_alive() {
    let mut fresh = Fresh::file();
    let taken = lock(fresh.connection(), HOLDER).expect("locked");
    let other = open(fresh.path());
    let info = locked_info(lock(&other, "second [pid 2]"));
    assert_eq!(info.locked_by, HOLDER);
    assert!(info.locked_at.starts_with("20"), "{}", info.locked_at);
    let lookup = info.session_lookup.expect("a sentence for the person");
    assert!(lookup.contains("alive"), "{lookup}");
    // The refusal's own words reach the person, with the way out.
    let said = LedgerError::Locked(LockInfo {
        locked_by: HOLDER.into(),
        locked_at: "t".into(),
        session_lookup: Some(lookup),
    })
    .to_string();
    assert!(
        said.contains("pbps unlock") && said.contains("build-7"),
        "{said}"
    );
    // The row is the first run's still, and the refused run holds nothing.
    assert_eq!(
        lock_holder(&other).expect("read").expect("held").locked_by,
        HOLDER
    );
    release(fresh.connection(), taken.guard).expect("released");
    // Released: the next run takes it, and the sidecar was left where it was.
    assert!(sidecar_path(fresh.path()).exists());
    let again = lock(&other, "second [pid 2]").expect("free now");
    release(&other, again.guard).expect("released");
}

#[test]
fn release_deletes_its_own_row_and_nobody_elses() {
    let mut fresh = Fresh::file();
    let taken = lock(fresh.connection(), HOLDER).expect("locked");
    // A person cleared it by hand: there is nothing of ours to delete.
    assert!(unlock(fresh.connection()).expect("cleared"));
    assert!(!release(fresh.connection(), taken.guard).expect("released"));
    assert!(
        sidecar_is_free(fresh.path()),
        "the lock file is let go either way"
    );

    // Another run's row in its place is left alone.
    let taken = lock(fresh.connection(), HOLDER).expect("locked");
    fresh
        .connection()
        .execute_batch(&format!(
            "UPDATE {LOCK_TABLE} SET token = 'someone-else', locked_by = 'other'"
        ))
        .expect("replaced");
    assert!(!release(fresh.connection(), taken.guard).expect("released"));
    assert_eq!(rows(fresh.connection()), 1, "the other row stayed");
    assert_eq!(
        text(
            fresh.connection(),
            &format!("SELECT locked_by FROM {LOCK_TABLE}")
        ),
        "other"
    );
}

#[test]
fn a_dead_holders_row_is_cleared_by_the_next_run_which_says_whose_it_was() {
    let mut fresh = Fresh::file();
    stale_row(fresh.connection(), "ghost [pid 9]", "file");
    let taken = lock(fresh.connection(), HOLDER).expect("the stale row is cleared");
    assert_eq!(taken.guard.proof(), &Proof::File);
    let cleared = taken.cleared.expect("it says whose row it cleared");
    assert_eq!(cleared.locked_by, "ghost [pid 9]");
    assert_eq!(
        text(
            fresh.connection(),
            &format!("SELECT locked_by FROM {LOCK_TABLE}")
        ),
        HOLDER
    );
    // Negative: a lock taken on a free database clears nothing.
    release(fresh.connection(), taken.guard).expect("released");
    let clean = lock(fresh.connection(), HOLDER).expect("locked");
    assert!(clean.cleared.is_none());
}

#[test]
fn a_guard_dropped_without_a_release_leaves_a_row_the_next_run_clears() {
    let mut fresh = Fresh::file();
    let taken = lock(fresh.connection(), HOLDER).expect("locked");
    drop(taken.guard);
    assert_eq!(rows(fresh.connection()), 1, "the row stayed");
    assert!(
        sidecar_is_free(fresh.path()),
        "the OS lock went with the handle"
    );
    let next = lock(fresh.connection(), "next [pid 3]").expect("cleared and taken");
    assert_eq!(next.cleared.expect("cleared").locked_by, HOLDER);
}

#[test]
fn a_row_whose_holder_had_no_proof_is_never_cleared_by_a_run_that_has_one() {
    let mut fresh = Fresh::file();
    let path = fresh.path().to_path_buf();
    stale_row(fresh.connection(), "no-proof [pid 5]", "row");
    let info = locked_info(lock(fresh.connection(), HOLDER));
    assert_eq!(info.locked_by, "no-proof [pid 5]");
    let lookup = info.session_lookup.expect("said");
    assert!(lookup.contains("without a liveness check"), "{lookup}");
    assert_eq!(rows(fresh.connection()), 1, "the row is still there");
    assert_eq!(row_proof(fresh.connection()), "row");
    assert!(
        sidecar_is_free(&path),
        "the run that was refused let go of the OS lock it had taken"
    );
    // Only the person clears it.
    assert!(unlock(fresh.connection()).expect("cleared"));
    let again = lock(fresh.connection(), HOLDER).expect("free");
    release(fresh.connection(), again.guard).expect("released");
}

#[test]
fn where_the_sidecar_cannot_be_used_the_row_alone_holds_and_only_unlock_clears_it() {
    let mut fresh = Fresh::file();
    let path = fresh.path().to_path_buf();
    // A directory in the sidecar's place cannot be opened as the lock file.
    std::fs::create_dir(sidecar_path(&path)).expect("a directory");
    let taken = lock(fresh.connection(), HOLDER).expect("locked without a proof");
    let Proof::RowOnly(RowOnly::Unavailable(why)) = taken.guard.proof().clone() else {
        panic!("{:?}", taken.guard.proof());
    };
    assert!(why.contains("cannot be opened"), "{why}");
    assert_eq!(row_proof(fresh.connection()), "row");
    // A second run is refused, and so is one after the first is dropped: nothing
    // proves the holder gone.
    let other = open(&path);
    let info = locked_info(lock(&other, "second [pid 2]"));
    assert!(
        info.session_lookup
            .expect("said")
            .contains("without a liveness check")
    );
    drop(taken.guard);
    locked_info(lock(&other, "second [pid 2]"));
    assert!(unlock(&other).expect("cleared"));
    let again = lock(&other, "second [pid 2]").expect("free");
    assert!(again.cleared.is_none());
}

#[test]
fn a_run_without_a_proof_cannot_clear_a_row_that_had_one_and_says_why() {
    let mut fresh = Fresh::file();
    let path = fresh.path().to_path_buf();
    stale_row(fresh.connection(), "ghost [pid 9]", "file");
    // The lock file a holder left is put out of reach: a directory in its name.
    std::fs::remove_file(sidecar_path(&path)).expect("the file the holder left");
    std::fs::create_dir(sidecar_path(&path)).expect("a directory");
    let info = locked_info(lock(fresh.connection(), HOLDER));
    let lookup = info.session_lookup.expect("said");
    assert!(
        lookup.contains("cannot tell whether the holder is alive"),
        "{lookup}"
    );
    assert!(lookup.contains("cannot be opened"), "{lookup}");
    assert_eq!(rows(fresh.connection()), 1, "the row was left");
    // Negative: the same row is cleared once the lock file can be used.
    std::fs::remove_dir(sidecar_path(&path)).expect("removed");
    assert!(
        lock(fresh.connection(), HOLDER)
            .expect("cleared")
            .cleared
            .is_some()
    );
}

#[test]
fn a_database_with_no_file_needs_no_sidecar_and_holds_by_its_row() {
    let mut fresh = Fresh::memory();
    let taken = lock(fresh.connection(), HOLDER).expect("locked");
    assert_eq!(taken.guard.proof(), &Proof::RowOnly(RowOnly::NoFile));
    assert_eq!(row_proof(fresh.connection()), "row");
    // A second lock on the same private database is refused, and is not cleared.
    locked_info(lock(fresh.connection(), HOLDER));
    assert!(release(fresh.connection(), taken.guard).expect("released"));
    assert_eq!(rows(fresh.connection()), 0);
    lock(fresh.connection(), HOLDER).expect("free again");
}

#[test]
fn two_spellings_of_one_file_meet_on_one_sidecar() {
    #[cfg(unix)]
    {
        let fresh = Fresh::file();
        let real = fresh.path().to_path_buf();
        let dir = real.parent().expect("a directory").to_path_buf();
        let by_link = dir.join("link.db");
        std::os::unix::fs::symlink(&real, &by_link).expect("a symbolic link to the file");
        let aliased = dir.join("alias");
        std::os::unix::fs::symlink(&dir, &aliased).expect("a symbolic link to the directory");
        let spelled = aliased.join("fresh.db");

        let first = open(&by_link);
        let taken = lock(&first, HOLDER).expect("locked");
        // Opened by the real path and by a path through a linked directory: the
        // same lock file, so the same holder is found.
        for second in [open(&real), open(&spelled)] {
            assert_eq!(locked_info(lock(&second, "x")).locked_by, HOLDER);
        }
        assert!(sidecar_path(&real).exists());
        assert!(
            !dir.join(format!("link.db{LOCK_FILE_SUFFIX}")).exists(),
            "no sidecar was made under the link's own name"
        );
        release(&first, taken.guard).expect("released");
    }
}

#[test]
fn a_run_that_takes_the_os_lock_and_then_fails_to_write_the_row_releases_it() {
    in_both_journal_modes(|fresh, second| {
        ensure_tables(fresh.connection()).expect("created");
        second
            .execute_batch("BEGIN IMMEDIATE")
            .expect("another writer");
        let failed = lock_within(fresh.connection(), HOLDER, Duration::from_millis(100));
        match failed {
            Err(LedgerError::Db(error)) => {
                assert!(is_busy(&error), "{error:?}");
                assert!(
                    error.to_string().contains("not this tool's lock"),
                    "{error}"
                );
            }
            other => panic!("expected a busy database, got {other:?}"),
        }
        assert!(
            sidecar_is_free(fresh.path()),
            "the OS lock was released on the failure"
        );
        second.execute_batch("ROLLBACK").expect("released");
        assert_eq!(rows(second), 0, "and no row was left");
        lock(fresh.connection(), HOLDER).expect("then it can be taken");
    });
}

#[test]
fn a_holder_too_long_for_the_row_is_refused_before_anything_is_touched() {
    let mut fresh = Fresh::file();
    let long = "h".repeat(LOCKED_BY_CHARS + 1);
    match lock(fresh.connection(), &long) {
        Err(LedgerError::Db(DbError::Refused(message))) => {
            assert!(message.contains("lock holder"), "{message}")
        }
        other => panic!("{other:?}"),
    }
    assert!(!sidecar_path(fresh.path()).exists());
    assert_eq!(
        number(fresh.connection(), "SELECT count(*) FROM sqlite_schema"),
        0
    );
    // Negative: exactly the width is taken.
    let exact = "h".repeat(LOCKED_BY_CHARS);
    lock(fresh.connection(), &exact).expect("a full width fits");
}

#[test]
fn many_runs_at_once_leave_exactly_one_holder() {
    for _ in 0..10 {
        let fresh = Fresh::file();
        let path = fresh.path().to_path_buf();
        let n = 6;
        let barrier = Arc::new(Barrier::new(n));
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let (path, barrier) = (path.clone(), barrier.clone());
                thread::spawn(move || {
                    let c = open(&path);
                    barrier.wait();
                    let result = lock(&c, &format!("run {i}"));
                    // The connection comes back with the result, so a win is held
                    // until every run has tried.
                    (c, result)
                })
            })
            .collect();
        let outcomes: Vec<_> = handles
            .into_iter()
            .map(|h| h.join().expect("joined"))
            .collect();
        let won = outcomes.iter().filter(|(_, r)| r.is_ok()).count();
        assert_eq!(won, 1, "exactly one run holds the lock");
        for (_, result) in &outcomes {
            if let Err(error) = result {
                assert!(matches!(error, LedgerError::Locked(_)), "{error:?}");
            }
        }
    }
}

#[test]
fn many_runs_finding_one_stale_row_clear_it_once_and_one_wins() {
    for _ in 0..10 {
        let fresh = Fresh::file();
        let path = fresh.path().to_path_buf();
        stale_row(&open(&path), "ghost [pid 9]", "file");
        let n = 6;
        let barrier = Arc::new(Barrier::new(n));
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let (path, barrier) = (path.clone(), barrier.clone());
                thread::spawn(move || {
                    let c = open(&path);
                    barrier.wait();
                    let result = lock(&c, &format!("run {i}"));
                    (c, result)
                })
            })
            .collect();
        let outcomes: Vec<_> = handles
            .into_iter()
            .map(|h| h.join().expect("joined"))
            .collect();
        let winners: Vec<&Taken> = outcomes
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok())
            .collect();
        assert_eq!(winners.len(), 1, "one run wins");
        assert_eq!(
            winners[0].cleared.as_ref().map(|c| c.locked_by.as_str()),
            Some("ghost [pid 9]"),
            "and it is the one that cleared the stale row"
        );
        for (_, result) in &outcomes {
            if let Err(error) = result {
                assert!(matches!(error, LedgerError::Locked(_)), "{error:?}");
            }
        }
        assert_eq!(rows(&open(&path)), 1);
    }
}

#[test]
fn lock_holder_tells_none_a_holder_and_an_unreadable_table_apart() {
    let mut fresh = Fresh::file();
    // No lock table: nothing holds it.
    assert!(lock_holder(fresh.connection()).expect("asked").is_none());
    assert!(
        !unlock(fresh.connection()).expect("asked"),
        "and nothing to clear"
    );
    let taken = lock(fresh.connection(), HOLDER).expect("locked");
    let info = lock_holder(fresh.connection())
        .expect("asked")
        .expect("held");
    assert_eq!(info.locked_by, HOLDER);
    release(fresh.connection(), taken.guard).expect("released");
    assert!(
        lock_holder(fresh.connection()).expect("asked").is_none(),
        "an empty table"
    );
    // Not a table: refused, never read as "nobody".
    let mut view = Fresh::memory();
    view.connection()
        .execute_batch("CREATE VIEW __pbps_lock AS SELECT 1 AS id")
        .expect("ddl");
    assert!(matches!(
        lock_holder(view.connection()),
        Err(DbError::Refused(_))
    ));
    assert!(matches!(
        unlock(view.connection()),
        Err(DbError::Refused(_))
    ));
    // A reader is not blocked by a writer elsewhere, so the committed answer comes.
    in_both_journal_modes(|fresh, second| {
        ensure_tables(fresh.connection()).expect("created");
        second
            .execute_batch("BEGIN IMMEDIATE")
            .expect("another writer");
        assert!(lock_holder(fresh.connection()).expect("asked").is_none());
        second.execute_batch("ROLLBACK").expect("released");
    });
}

#[test]
fn unlock_clears_whoever_holds_and_leaves_the_sidecar_alone() {
    let mut fresh = Fresh::file();
    let held = lock(fresh.connection(), HOLDER).expect("locked");
    let sidecar = sidecar_path(fresh.path());
    let other = open(fresh.path());
    assert!(
        unlock(&other).expect("cleared by another connection"),
        "a live holder's row too"
    );
    assert!(
        sidecar.exists(),
        "the sidecar a live holder has locked is not deleted"
    );
    assert!(!sidecar_is_free(fresh.path()), "and its lock is intact");
    assert!(!unlock(&other).expect("again"), "nothing more to clear");
    drop(held);
}

#[test]
fn the_session_helpers_say_what_a_file_database_can_and_cannot_tell() {
    let mut fresh = Fresh::memory();
    assert_eq!(
        session_application_name(fresh.connection()).expect("name"),
        "pbps-sqlite"
    );
    let rendered = LockHolder {
        operator: "leon",
        host: Some("build-7"),
        pid: 4242,
        ci_job: None,
        application_name: "pbps-sqlite",
    }
    .render();
    let lookup = session_lookup(&rendered).expect("a holder with an application name");
    assert!(lookup.contains("pbps unlock"), "{lookup}");
    // It is given the text alone, so it must hold for a lock taken either way.
    assert!(
        lookup.contains("only when the lock was taken with the lock file"),
        "{lookup}"
    );
    assert!(
        lookup.contains("without it is cleared only by `pbps unlock`"),
        "{lookup}"
    );
    // Negative: a holder recorded without one has nothing to look up.
    assert!(session_lookup("leon [pid 1]").is_none());
}

/// The helper process: takes the lock, says so, and holds it until it is killed.
const CHILD: &str = "PBPS_SQLITE_TEST_LOCK_CHILD";

#[test]
fn a_killed_holders_lock_is_released_and_its_row_is_cleared() {
    if let Some(path) = std::env::var_os(CHILD) {
        let c = open(Path::new(&path));
        let _held = lock(&c, "child holder [pid 1]").expect("the child takes the lock");
        println!("PBPS-LOCK-READY");
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }

    let mut fresh = Fresh::file();
    let path = fresh.path().to_path_buf();
    ensure_tables(fresh.connection()).expect("created");
    let mut child = Command::new(std::env::current_exe().expect("this test binary"))
        .args([
            "--exact",
            "state::tests::lock::a_killed_holders_lock_is_released_and_its_row_is_cleared",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, &path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the helper starts");
    // Wait for the helper to say it holds the lock, however long a loaded
    // machine takes to start it.
    let (tx, rx) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().expect("piped");
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            // The test harness has written `test <name> ... ` without a newline,
            // so the marker ends a line and does not start one.
            if line.trim_end().ends_with("PBPS-LOCK-READY") {
                let _ = tx.send(());
                return;
            }
        }
    });
    if rx.recv_timeout(Duration::from_secs(300)).is_err() {
        let _ = child.kill();
        panic!("the helper never reported that it holds the lock");
    }

    // While it lives, it is alive: refused, and named.
    let info = locked_info(lock(fresh.connection(), "parent [pid 2]"));
    assert_eq!(info.locked_by, "child holder [pid 1]");
    assert!(info.session_lookup.expect("said").contains("alive"));

    // Killed, not asked to leave: nothing of the child ran after this.
    child.kill().expect("killed");
    child.wait().expect("reaped");
    assert!(
        sidecar_is_free(&path),
        "the operating system released the killed process's lock"
    );
    assert_eq!(rows(fresh.connection()), 1, "and its row was left behind");
    let taken =
        lock(fresh.connection(), "parent [pid 2]").expect("the dead holder's row is cleared");
    assert_eq!(
        taken.cleared.expect("it says whose row").locked_by,
        "child holder [pid 1]"
    );
    assert_eq!(taken.guard.proof(), &Proof::File);
}

/// A file name on Unix need not be UTF-8. The connection opens such a file, so
/// the lock must find its sidecar and not fail on the name.
#[cfg(unix)]
#[test]
fn a_database_whose_file_name_is_not_utf8_is_locked_with_its_sidecar() {
    use std::os::unix::ffi::OsStringExt;
    // Beside a harness file, so the directory is removed with it.
    let fresh = Fresh::file();
    let name = std::ffi::OsString::from_vec(vec![b'd', 0xff, 0xfe, b'.', b'd', b'b']);
    let path = fresh.path().with_file_name(name);
    let c = pbps_db::sqlite::Connection::open(&path, pbps_db::sqlite::OpenMode::CreateIfMissing)
        .expect("opened");
    let taken = lock(&c, "op [pid 1]").expect("locked");
    assert_eq!(taken.guard.proof(), &Proof::File);
    let sidecar = sidecar_path(&std::fs::canonicalize(&path).expect("the real path"));
    assert!(sidecar.exists(), "{}", sidecar.display());
    assert!(release(&c, taken.guard).expect("released"));
}

/// Two hard links to one database file are two paths to everything, so each has
/// a sidecar of its own. The lock a run takes on one says nothing about a holder
/// that locked the other: its row is never cleared, alive or gone.
#[test]
fn a_holder_that_locked_through_a_hard_link_is_never_cleared_by_the_other_path() {
    let fresh = Fresh::file();
    let real = fresh.path().to_path_buf();
    let linked = real.with_file_name("hard.db");
    std::fs::hard_link(&real, &linked).expect("a hard link to the file");
    let first = open(&real);
    let taken = lock(&first, HOLDER).expect("locked");
    let second = open(&linked);
    // Alive: the other path's sidecar is free, and the row still holds.
    let info = locked_info(lock(&second, "x"));
    assert_eq!(info.locked_by, HOLDER);
    let said = info.session_lookup.expect("said");
    assert!(said.contains("through another path"), "{said}");
    assert!(said.contains("pbps unlock"), "{said}");
    // Gone: still not cleared from the other path, which cannot know.
    drop(taken);
    assert_eq!(locked_info(lock(&second, "x")).locked_by, HOLDER);
    // Negative: the path the holder used clears it, as for any dead holder.
    let third = open(&real);
    let taken = lock(&third, "y").expect("the same path proves the holder gone");
    assert_eq!(taken.cleared.expect("cleared").locked_by, HOLDER);
    release(&third, taken.guard).expect("released");
}

/// A view put in the lock's name while a run holds the lock must not take the
/// release's delete: a trigger on it would run.
#[test]
fn release_refuses_a_view_in_the_locks_name_and_runs_no_trigger() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    let taken = lock(c, HOLDER).expect("locked");
    c.execute_batch(
        "DROP TABLE main.__pbps_lock;
         CREATE TABLE hits (n INTEGER);
         CREATE VIEW __pbps_lock AS SELECT 1 AS id, 'x' AS token;
         CREATE TRIGGER fired INSTEAD OF DELETE ON __pbps_lock
         BEGIN INSERT INTO hits VALUES (1); END;",
    )
    .expect("the lock table replaced by a view");
    let refused = release(c, taken.guard).expect_err("refused");
    assert!(
        refused
            .to_string()
            .contains("is a view, not an ordinary table"),
        "{refused}"
    );
    assert_eq!(
        number(c, "SELECT count(*) FROM hits"),
        0,
        "the trigger did not run"
    );
}
