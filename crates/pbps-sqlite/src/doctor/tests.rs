//! Real-file measurements of `pbps_sqlite::doctor::permissions` (#2950).

use std::path::Path;
use std::time::Duration;

use pbps_db::DbError;
use pbps_db::sqlite::{Connection, OpenMode};

use super::{permissions, permissions_within};
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
fn a_query_only_connection_is_a_write_gap_and_an_open_descriptor_is_asked_through_sqlite() {
    // A file write-protected before it is opened never gets here: the driver
    // refuses to open it. Measured: one write-protected after the open is
    // still writable through the open descriptor, and SQLite, which a
    // deployment on this connection would write through, reports no gap.
    let mut fresh = Fresh::file();
    let path = fresh.path().to_path_buf();
    fresh
        .connection()
        .execute_batch("CREATE TABLE app (id INTEGER PRIMARY KEY)")
        .expect("table");
    let mut perms = std::fs::metadata(&path).expect("meta").permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&path, perms).expect("chmod ro");
    let result = permissions(fresh.connection());
    let mut perms = std::fs::metadata(&path).expect("meta").permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&path, perms).expect("chmod rw");
    assert!(result.expect("answered").gaps.is_empty());

    // `PRAGMA query_only = ON` makes the connection itself read-only.
    let mut q = Fresh::file();
    q.connection()
        .execute_batch("PRAGMA query_only = ON")
        .expect("query_only");
    let held = permissions(q.connection()).expect("query_only is a permission gap");
    assert_eq!(held.gaps.len(), 1, "{held:?}");
    assert_eq!(held.gaps[0].permission, "WRITE");
    assert!(held.gaps[0].why.contains("read-only"), "{:?}", held.gaps[0]);
}

#[test]
fn unreadable_busy_unlinked_and_journal_off_databases_are_errors_not_clean_or_gaps() {
    // Busy (another writer holds `BEGIN IMMEDIATE`).
    let mut busy = Fresh::file();
    busy.connection()
        .execute_batch("CREATE TABLE app (id INTEGER PRIMARY KEY)")
        .expect("table");
    let other = Connection::open(busy.path(), OpenMode::ExistingOnly).expect("second");
    other.execute_batch("BEGIN IMMEDIATE").expect("hold lock");
    let err = permissions_within(busy.connection(), Duration::from_millis(50))
        .expect_err("busy must be an error");
    assert!(err.to_string().contains("busy"), "{err}");
    other.execute_batch("ROLLBACK").expect("release");

    // `journal_mode = OFF`: rollback is not reliable, so refused rather than risked.
    let mut off = Fresh::file();
    off.connection()
        .execute_batch("CREATE TABLE app (id INTEGER PRIMARY KEY); PRAGMA journal_mode = OFF")
        .expect("journal off");
    let err = permissions(off.connection()).expect_err("journal off must refuse");
    assert!(matches!(err, DbError::Refused(_)), "{err:?}");

    // File vanished under an open handle: absent/unreadable is an error, never clean.
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
