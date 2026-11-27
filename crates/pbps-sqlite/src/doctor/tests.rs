//! Real-file measurements of `pbps_sqlite::doctor::permissions` (#2950).

use std::path::Path;
use std::time::Duration;

use pbps_db::DbError;
use pbps_db::sqlite::{Connection, OpenMode};

use super::{ask_the_system, permissions, permissions_within};
use crate::state::ensure_tables;
use crate::tests::Fresh;

fn dir_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

#[cfg(unix)]
fn protect_dir(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).expect("chmod 555");
    let probe = dir.join("probe");
    let created = std::fs::File::create(&probe).is_ok();
    if created {
        std::fs::remove_file(&probe).expect("remove probe");
    }
    !created
}

#[cfg(unix)]
fn unprotect_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).expect("chmod 755");
}

#[test]
fn a_healthy_file_with_a_ledger_has_no_gaps_and_is_left_untouched() {
    let mut fresh = Fresh::file();
    let path = fresh.path().to_path_buf();
    let dir = path.parent().expect("parent").to_path_buf();
    let c = fresh.connection();
    ensure_tables(c).expect("ledger");
    c.execute_batch("CREATE TABLE app (id INTEGER PRIMARY KEY); INSERT INTO app VALUES (1)")
        .expect("data");
    let bytes_before = std::fs::read(&path).expect("bytes");
    let listing_before = dir_entries(&dir);

    let held = permissions(c).expect("permissions");
    assert!(held.gaps.is_empty(), "{held:?}");
    assert!(held.advised.is_empty(), "{held:?}");
    assert!(held.absent_schemas.is_empty(), "{held:?}");
    assert!(!c.in_transaction());
    assert_eq!(std::fs::read(&path).expect("bytes"), bytes_before);
    assert_eq!(dir_entries(&dir), listing_before);
}

#[test]
fn a_file_with_no_ledger_yet_is_not_a_permission_gap() {
    // Non-empty file without ledger tables.
    let mut populated = Fresh::file();
    let pop_path = populated.path().to_path_buf();
    populated
        .connection()
        .execute_batch("CREATE TABLE app (id INTEGER PRIMARY KEY)")
        .expect("table");
    let before = std::fs::read(&pop_path).expect("bytes");
    let held = permissions(populated.connection()).expect("permissions");
    assert!(held.gaps.is_empty(), "{held:?}");
    assert_eq!(std::fs::read(&pop_path).expect("bytes"), before);

    // Brand-new 0-byte file with no ledger and no schema.
    let mut empty = Fresh::file();
    let empty_path = empty.path().to_path_buf();
    let empty_dir = empty_path.parent().expect("parent").to_path_buf();
    let empty_before = std::fs::read(&empty_path).expect("bytes");
    let listing_before = dir_entries(&empty_dir);
    let held = permissions(empty.connection()).expect("permissions");
    assert!(held.gaps.is_empty(), "{held:?}");
    assert_eq!(
        std::fs::read(&empty_path).expect("bytes"),
        empty_before,
        "a 0-byte file must stay untouched (0 bytes)"
    );
    assert_eq!(dir_entries(&empty_dir), listing_before);
}

#[cfg(unix)]
#[test]
fn a_read_only_directory_is_a_write_permission_gap_and_leaves_the_file_unchanged() {
    let mut fresh = Fresh::file();
    let path = fresh.path().to_path_buf();
    let dir = path.parent().expect("parent").to_path_buf();
    ensure_tables(fresh.connection()).expect("ledger");
    fresh
        .connection()
        .execute_batch("CREATE TABLE app (id INTEGER PRIMARY KEY)")
        .expect("table");
    let before = std::fs::read(&path).expect("bytes");

    if !protect_dir(&dir) {
        unprotect_dir(&dir);
        eprintln!("skipped: this process ignores directory permissions");
        return;
    }
    let result = permissions(fresh.connection());
    unprotect_dir(&dir);

    let held = result.expect("returns gaps, not unknown");
    assert_eq!(held.gaps.len(), 1, "{held:?}");
    assert_eq!(held.gaps[0].permission, "WRITE");
    assert!(
        held.gaps[0].securable.contains("database directory"),
        "{:?}",
        held.gaps[0]
    );
    assert!(held.gaps[0].why.contains("journal"), "{:?}", held.gaps[0]);
    assert_eq!(std::fs::read(&path).expect("bytes"), before);
}

#[cfg(unix)]
#[test]
fn an_empty_file_in_a_read_only_directory_is_also_a_write_permission_gap() {
    let mut fresh = Fresh::file();
    let path = fresh.path().to_path_buf();
    let dir = path.parent().expect("parent").to_path_buf();
    assert_eq!(std::fs::read(&path).expect("bytes").len(), 0);

    if !protect_dir(&dir) {
        unprotect_dir(&dir);
        eprintln!("skipped: this process ignores directory permissions");
        return;
    }
    let result = permissions(fresh.connection());
    unprotect_dir(&dir);

    let held = result.expect("returns gaps, not unknown");
    assert_eq!(
        held.gaps.len(),
        1,
        "a 0-byte file in a read-only directory must report a write gap: {held:?}"
    );
    assert_eq!(held.gaps[0].permission, "WRITE");
}

#[test]
fn a_query_only_connection_is_a_write_gap_on_the_connection() {
    let mut q = Fresh::file();
    q.connection()
        .execute_batch("PRAGMA query_only = ON")
        .expect("query_only");
    let held = permissions(q.connection()).expect("query_only is a permission gap");
    assert_eq!(held.gaps.len(), 1, "{held:?}");
    assert_eq!(held.gaps[0].permission, "WRITE");
    assert!(held.gaps[0].securable.contains("connection"), "{held:?}");
    // Negative: turned off again, the same connection has no gap.
    q.connection()
        .execute_batch("PRAGMA query_only = OFF")
        .expect("query_only off");
    assert!(
        permissions(q.connection())
            .expect("answered")
            .gaps
            .is_empty()
    );
}

// Unix only: the operating system is asked with `access`, and Windows keeps
// the answer in access control lists that only an attempt reads.
#[cfg(unix)]
#[test]
fn a_file_write_protected_after_the_open_is_a_write_gap_on_the_file() {
    // One write-protected before the open never gets here: the driver refuses
    // to open it. After the open, the next connection would be refused.
    use std::os::unix::fs::PermissionsExt as _;
    let mut fresh = Fresh::file();
    let path = fresh.path().to_path_buf();
    fresh
        .connection()
        .execute_batch("CREATE TABLE app (id INTEGER PRIMARY KEY)")
        .expect("table");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).expect("chmod ro");
    // A process that may write anything (root) is told so, and has no gap.
    // Learnt from a sibling: the database file itself is never opened again.
    let sibling = path.with_extension("sibling");
    std::fs::write(&sibling, b"").expect("sibling");
    std::fs::set_permissions(&sibling, std::fs::Permissions::from_mode(0o444)).expect("chmod");
    let refused = std::fs::OpenOptions::new()
        .append(true)
        .open(&sibling)
        .is_err();
    let result = permissions(fresh.connection());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod rw");
    let held = result.expect("answered");
    if refused {
        assert_eq!(held.gaps.len(), 1, "{held:?}");
        assert!(held.gaps[0].securable.contains("database file"), "{held:?}");
    } else {
        assert!(held.gaps.is_empty(), "{held:?}");
    }
}

#[test]
fn a_writer_holding_the_file_does_not_stop_the_question_and_journal_off_is_an_error() {
    // Nothing is tried and no lock is taken, so another connection's write
    // lock neither blocks the answer nor is disturbed by it.
    let mut busy = Fresh::file();
    busy.connection()
        .execute_batch("CREATE TABLE app (id INTEGER PRIMARY KEY)")
        .expect("table");
    let other = Connection::open(busy.path(), OpenMode::ExistingOnly).expect("second");
    other
        .execute_batch("BEGIN IMMEDIATE; INSERT INTO app VALUES (1)")
        .expect("hold lock");
    let held = permissions_within(busy.connection(), Duration::from_millis(50))
        .expect("answered beside a writer");
    assert!(held.gaps.is_empty(), "{held:?}");
    other.execute_batch("COMMIT").expect("the writer commits");

    // `journal_mode = OFF`: rollback is not reliable, so refused rather than risked.
    let mut off = Fresh::file();
    off.connection()
        .execute_batch("CREATE TABLE app (id INTEGER PRIMARY KEY); PRAGMA journal_mode = OFF")
        .expect("journal off");
    let err = permissions(off.connection()).expect_err("journal off must refuse");
    assert!(matches!(err, DbError::Refused(_)), "{err:?}");
}

// Unix only: Windows refuses to remove a file that is open, so a connection
// there cannot be left holding a file that is gone.
#[cfg(unix)]
#[test]
fn a_file_removed_under_the_connection_is_an_error_not_a_clean_answer() {
    let mut gone = Fresh::file();
    let gone_path = gone.path().to_path_buf();
    gone.connection()
        .execute_batch("CREATE TABLE app (id INTEGER PRIMARY KEY)")
        .expect("table");
    std::fs::remove_file(&gone_path).expect("unlink");
    let err = permissions(gone.connection()).expect_err("unlinked file must error");
    assert!(
        matches!(&err, DbError::Refused(why) if why.contains("moved or removed")),
        "{err:?}"
    );
}

#[test]
fn a_wal_mode_file_and_a_stranger_view_are_handled_truthfully() {
    // WAL mode: healthy file returns no gaps.
    let mut wal = Fresh::file();
    wal.connection()
        .execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE app (id INTEGER PRIMARY KEY)")
        .expect("wal");
    ensure_tables(wal.connection()).expect("ledger");
    let held = permissions(wal.connection()).expect("wal permissions");
    assert!(held.gaps.is_empty(), "{held:?}");

    // A view occupying `__pbps_state` is a ledger integrity error (`ledger.untrusted`),
    // not a `permission.unknown` failure.
    let mut view = Fresh::file();
    view.connection()
        .execute_batch("CREATE VIEW __pbps_state AS SELECT 1 AS id")
        .expect("stranger view");
    let held = permissions(view.connection()).expect("view left to ledger_problems");
    assert!(held.gaps.is_empty(), "{held:?}");
}

#[test]
fn a_platform_that_cannot_be_asked_is_said_and_never_read_as_writable() {
    let path = Path::new("dir").join("app.db");
    let (mut gaps, mut unasked) = (Vec::new(), Vec::new());
    ask_the_system(
        &path,
        &["-journal"],
        |_, _| Ok(None),
        &mut gaps,
        &mut unasked,
    )
    .expect("answered");
    assert!(gaps.is_empty(), "{gaps:?}");
    assert_eq!(unasked.len(), 1, "{unasked:?}");
    assert!(unasked[0].why.contains("tries nothing"), "{unasked:?}");

    // Negative: a platform that answers yes says nothing, and one that
    // answers no has a gap and no advice.
    let (mut gaps, mut unasked) = (Vec::new(), Vec::new());
    ask_the_system(
        &path,
        &["-journal"],
        |_, _| Ok(Some(true)),
        &mut gaps,
        &mut unasked,
    )
    .expect("answered");
    assert!(
        gaps.is_empty() && unasked.is_empty(),
        "{gaps:?} {unasked:?}"
    );
    let (mut gaps, mut unasked) = (Vec::new(), Vec::new());
    ask_the_system(
        &path,
        &["-journal"],
        |_, d| Ok(Some(!d)),
        &mut gaps,
        &mut unasked,
    )
    .expect("answered");
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    assert!(gaps[0].securable.contains("directory"), "{gaps:?}");
    assert!(unasked.is_empty(), "{unasked:?}");

    // A question that failed is an error, never an answer.
    let failed = ask_the_system(
        &path,
        &["-journal"],
        |_, _| Err(std::io::Error::other("no answer")),
        &mut Vec::new(),
        &mut Vec::new(),
    );
    assert!(matches!(failed, Err(DbError::Refused(_))), "{failed:?}");
}

#[test]
fn a_journal_file_the_journal_mode_writes_is_asked_when_it_exists() {
    let path = Path::new("dir").join("app.db");
    let absent = || Err(std::io::Error::from(std::io::ErrorKind::NotFound));
    let ends = |p: &Path, suffix: &str| p.to_string_lossy().ends_with(suffix);
    let journal = |p: &Path| ["-wal", "-shm", "-journal"].iter().any(|s| ends(p, s));

    // A write-ahead log this process may not write is a gap that names it.
    let (mut gaps, mut unasked) = (Vec::new(), Vec::new());
    ask_the_system(
        &path,
        &["-wal", "-shm"],
        |p, _| match p {
            p if ends(p, "-wal") => Ok(Some(false)),
            p if ends(p, "-shm") => absent(),
            _ => Ok(Some(true)),
        },
        &mut gaps,
        &mut unasked,
    )
    .expect("answered");
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    assert!(gaps[0].securable.contains("app.db-wal"), "{gaps:?}");

    // Negative: no journal file kept is no gap and no error, and a writable
    // one is no gap.
    for kept in [None, Some(true)] {
        let (mut gaps, mut unasked) = (Vec::new(), Vec::new());
        ask_the_system(
            &path,
            &["-wal", "-shm"],
            |p, _| match (journal(p), kept) {
                (false, _) => Ok(Some(true)),
                (true, None) => absent(),
                (true, answer) => Ok(answer),
            },
            &mut gaps,
            &mut unasked,
        )
        .expect("answered");
        assert!(
            gaps.is_empty() && unasked.is_empty(),
            "{gaps:?} {unasked:?}"
        );
    }

    // Negative: a read-only `-wal` file left beside a rollback-journal
    // database is not written, so it is not asked and is no gap.
    let (mut gaps, mut unasked) = (Vec::new(), Vec::new());
    ask_the_system(
        &path,
        &["-journal"],
        |p, _| match p {
            p if ends(p, "-wal") || ends(p, "-shm") => Ok(Some(false)),
            p if ends(p, "-journal") => absent(),
            _ => Ok(Some(true)),
        },
        &mut gaps,
        &mut unasked,
    )
    .expect("answered");
    assert!(gaps.is_empty(), "{gaps:?}");

    // A journal file whose question failed is an error, never an answer.
    let failed = ask_the_system(
        &path,
        &["-wal", "-shm"],
        |p, _| {
            if ends(p, "-shm") {
                Err(std::io::Error::other("no answer"))
            } else {
                Ok(Some(true))
            }
        },
        &mut Vec::new(),
        &mut Vec::new(),
    );
    assert!(matches!(failed, Err(DbError::Refused(_))), "{failed:?}");
}

#[cfg(unix)]
#[test]
fn a_read_only_wal_file_is_a_gap_in_wal_mode_and_none_in_rollback_mode() {
    use std::os::unix::fs::PermissionsExt as _;
    for (mode, gap) in [("wal", true), ("delete", false)] {
        let mut fresh = Fresh::file();
        let path = fresh.path().to_path_buf();
        fresh
            .connection()
            .query(&format!("PRAGMA journal_mode = {mode}"), &[])
            .expect("journal mode");
        fresh
            .connection()
            .execute_batch("CREATE TABLE app (id INTEGER PRIMARY KEY)")
            .expect("table");
        let mut name = path.as_os_str().to_owned();
        name.push("-wal");
        let wal = std::path::PathBuf::from(name);
        if !wal.exists() {
            std::fs::write(&wal, b"").expect("a leftover file");
        }
        std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o444)).expect("chmod ro");
        // A process that may write anything (root) has no "no" to be given.
        let refused = pbps_db::sqlite::may_write(&wal, false).expect("asked") == Some(false);
        let result = permissions(fresh.connection());
        std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o644)).expect("chmod rw");
        let held = result.expect("answered");
        assert_eq!(
            held.gaps.iter().any(|g| g.securable.contains("-wal")),
            gap && refused,
            "{mode}: {held:?}"
        );
    }
}
