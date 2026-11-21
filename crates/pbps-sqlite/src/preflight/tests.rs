//! The checks before a plan runs, against a real SQLite.

use std::path::Path;
use std::time::{Duration, Instant};

use pbps_db::DbError;
use pbps_db::sqlite::{Connection, OpenMode};
use pbps_model::{ChangeSet, PlannedChange};

use super::{
    Stage, Verdict, before_apply, connection_problems, floor_notes, library_lines, refusal,
    verdict_for, writable, writable_within,
};
use crate::Version;
use crate::emit::Form;
use crate::emit::tests::versions::{add_column_change, forms, rename_column};
use crate::state::ensure_tables;
use crate::tests::Fresh;

fn plan(changes: impl IntoIterator<Item = pbps_model::Change>) -> ChangeSet {
    ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    }
}

fn text(c: &Connection, sql: &str) -> String {
    c.query(sql, &[]).expect("query")[0]
        .text(0)
        .expect("text")
        .to_owned()
}

#[test]
fn a_connection_as_this_tool_opens_it_has_no_problems() {
    for mut fresh in [Fresh::memory(), Fresh::file()] {
        assert_eq!(
            connection_problems(fresh.connection()).expect("asked"),
            Vec::<String>::new()
        );
    }
}

#[test]
fn each_setting_on_the_wrong_value_is_one_line_naming_it_and_the_right_value_is_clean() {
    // (the pragma, the wrong value, the right value)
    for (pragma, wrong, right) in [
        ("foreign_keys", 0, 1),
        ("legacy_alter_table", 1, 0),
        ("ignore_check_constraints", 1, 0),
        ("defer_foreign_keys", 1, 0),
        ("query_only", 1, 0),
        ("writable_schema", 1, 0),
    ] {
        let mut fresh = Fresh::memory();
        let c = fresh.connection();
        c.execute_batch(&format!("PRAGMA {pragma} = {wrong}"))
            .expect("set");
        let found = connection_problems(c).expect("asked");
        assert_eq!(found.len(), 1, "{pragma}: {found:?}");
        assert!(
            found[0].contains(&format!("`PRAGMA {pragma}`")),
            "{found:?}"
        );
        assert!(found[0].contains(&format!("reads {wrong}")), "{found:?}");
        // Negative: put back, it is clean, so the line is about the setting.
        c.execute_batch(&format!("PRAGMA {pragma} = {right}"))
            .expect("restored");
        assert!(
            connection_problems(c).expect("asked").is_empty(),
            "{pragma}"
        );
    }
}

#[test]
fn legacy_alter_table_is_refused_because_a_rename_then_breaks_a_view_without_an_error() {
    let rename = |legacy: &str| {
        let mut fresh = Fresh::memory();
        let c = fresh.connection();
        c.execute_batch(&format!(
            "PRAGMA legacy_alter_table = {legacy}; CREATE TABLE t(a); \
             CREATE VIEW v AS SELECT a FROM t; ALTER TABLE t RENAME TO t2"
        ))
        .expect("every statement succeeds");
        c.query("SELECT * FROM v", &[])
    };
    assert!(rename("OFF").is_ok(), "the view follows the rename");
    let broken = rename("ON").expect_err("the view now names a table that is gone");
    assert!(broken.to_string().contains("no such table"), "{broken}");
}

#[test]
fn the_form_of_each_in_place_change_is_named_and_every_other_change_has_none() {
    let mut all = forms();
    all.push((Form::RenameColumn, rename_column()));
    for (form, change) in &all {
        assert_eq!(Form::of(change), Some(*form), "{form:?}");
    }
    // Negative: adding a column is written in place and needs no newer form.
    let add = add_column_change();
    assert_eq!(Form::of(&add), None);
}

#[test]
fn a_library_older_than_a_forms_arrival_is_one_line_naming_the_form_and_both_versions() {
    let mut all = forms();
    all.push((Form::RenameColumn, rename_column()));
    for (form, change) in all {
        let changes = plan([change]);
        let before = Version::new(form.arrival().major, form.arrival().minor - 1, 0);
        let found = library_lines(before, &changes);
        // The reader's own floor is below every form's, and below this one.
        let found: Vec<_> = found
            .into_iter()
            .filter(|line| !line.contains("pragma_table_list"))
            .collect();
        assert_eq!(found.len(), 1, "{form:?}: {found:?}");
        assert!(found[0].contains(form.words()), "{found:?}");
        assert!(found[0].contains(&form.arrival().to_string()), "{found:?}");
        assert!(found[0].contains(&before.to_string()), "{found:?}");
        // At the arrival it is clean, but for the reader's own floor.
        let at: Vec<_> = library_lines(form.arrival(), &changes)
            .into_iter()
            .filter(|line| !line.contains("pragma_table_list"))
            .collect();
        assert!(at.is_empty(), "{form:?}: {at:?}");
    }
}

#[test]
fn a_form_used_twice_is_one_line_and_a_plan_with_no_form_asks_nothing_of_the_library() {
    let (_, change) = forms().remove(1);
    let twice = plan([change.clone(), change]);
    assert_eq!(library_lines(Version::new(3, 40, 0), &twice).len(), 1);
    // Negative: no in-place form, so only the reader's floor can speak.
    let none = plan([add_column_change()]);
    assert!(library_lines(Version::new(3, 40, 0), &none).is_empty());
    assert!(library_lines(Version::new(3, 0, 0), &none)[0].contains("pragma_table_list"));
}

#[test]
fn the_linked_library_takes_every_form() {
    // Cannot fail with the bundled library (DEC-2950.4): that is the point.
    let mut fresh = Fresh::memory();
    let all: Vec<_> = forms().into_iter().map(|(_, change)| change).collect();
    let found = super::library_problems(fresh.connection(), &plan(all)).expect("asked");
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_file_that_can_be_written_is_left_exactly_as_it_was() {
    let mut fresh = Fresh::file();
    let path = fresh.path().to_path_buf();
    let c = fresh.connection();
    ensure_tables(c).expect("a ledger beside the application's own");
    c.execute_batch("CREATE TABLE app(a); INSERT INTO app VALUES (1)")
        .expect("application data");
    let before = std::fs::read(&path).expect("bytes");
    let listing = names(path.parent().expect("a directory"));
    writable(c).expect("writable");
    assert!(!c.in_transaction());
    assert_eq!(std::fs::read(&path).expect("bytes"), before);
    assert_eq!(names(path.parent().expect("a directory")), listing);
    // No table of the probe's name is left, in the file or the schema.
    assert_eq!(
        c.query(
            "SELECT count(*) FROM sqlite_schema WHERE name LIKE '__pbps_probe_%'",
            &[]
        )
        .expect("query")[0]
            .integer(0)
            .expect("count"),
        0
    );
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("a directory")
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

#[test]
fn inside_a_callers_transaction_only_the_probes_savepoint_is_undone() {
    let mut fresh = Fresh::file();
    let c = fresh.connection();
    c.execute_batch("CREATE TABLE app(a); BEGIN IMMEDIATE; INSERT INTO app VALUES (1)")
        .expect("a caller's open work");
    writable(c).expect("writable");
    assert!(c.in_transaction(), "the caller's transaction is still open");
    assert_eq!(
        c.query("SELECT count(*) FROM app", &[]).expect("query")[0]
            .integer(0)
            .expect("count"),
        1,
        "and so is its row"
    );
    assert_eq!(
        c.query(
            "SELECT count(*) FROM sqlite_schema WHERE name LIKE '__pbps_probe_%'",
            &[]
        )
        .expect("query")[0]
            .integer(0)
            .expect("count"),
        0,
        "the probe's table is not left in the caller's transaction"
    );
    c.execute_batch("ROLLBACK").expect("the caller's own end");
    assert!(!c.in_transaction());
}

/// A database at its page limit with no free page: two tables, capped where it
/// stands.
fn fill_to_the_limit(c: &Connection) {
    c.execute_batch("CREATE TABLE app(a); CREATE TABLE other(b); INSERT INTO other VALUES (1)")
        .expect("data");
    let integer = |sql: &str| {
        c.query(sql, &[]).expect("query")[0]
            .integer(0)
            .expect("integer")
    };
    let pages = integer("PRAGMA page_count");
    assert_eq!(integer("PRAGMA freelist_count"), 0);
    assert_eq!(
        integer(&format!("PRAGMA max_page_count = {pages}")),
        pages,
        "capped where it stands"
    );
    let full = c
        .execute_batch("CREATE TABLE one_more(x)")
        .expect_err("no page for another table");
    assert_eq!(full.server_error_code().as_deref(), Some("13"));
}

#[test]
fn a_database_at_its_page_limit_is_not_called_unwritable() {
    let mut fresh = Fresh::file();
    let c = fresh.connection();
    fill_to_the_limit(c);
    writable(c).expect("the probe's page is not the plan's");
    assert!(!c.in_transaction(), "nothing left open");
    assert_eq!(
        c.query("SELECT count(*) FROM sqlite_schema", &[])
            .expect("query")[0]
            .integer(0)
            .expect("integer"),
        2,
        "the probe left nothing"
    );
    let apply = before_apply(c, &plan([]), Version::new(3, 54, 1), Stage::Apply).expect("asked");
    assert!(apply.problems.is_empty(), "{apply:?}");
    // What a plan may do there without a new page still commits.
    c.execute_batch("ALTER TABLE app RENAME TO renamed; INSERT INTO other VALUES (2)")
        .expect("a rename and a row where there is room");
}

#[cfg(unix)]
#[test]
fn a_full_database_in_a_write_protected_directory_is_still_refused() {
    // Negative: letting the page limit pass does not let the directory pass.
    let mut fresh = Fresh::file();
    fill_to_the_limit(fresh.connection());
    let dir = fresh.path().parent().expect("a directory").to_path_buf();
    if !protect(&dir) {
        unprotect(&dir);
        eprintln!("skipped: this process ignores directory permissions, so nothing is protected");
        return;
    }
    let refused = writable(fresh.connection());
    unprotect(&dir);
    let refused = refused.expect_err("the journal cannot be made");
    let DbError::Refused(message) = &refused else {
        panic!("expected a refusal: {refused:?}");
    };
    assert!(message.contains("write-protected"), "{message}");
}

#[test]
fn a_database_another_connection_is_writing_is_busy_and_not_unwritable() {
    let mut fresh = Fresh::file();
    fresh
        .connection()
        .execute_batch("CREATE TABLE app(a)")
        .expect("table");
    let second = Connection::open(fresh.path(), OpenMode::ExistingOnly).expect("opens");
    second.execute_batch("BEGIN IMMEDIATE").expect("held");
    // A wait far shorter than the bound below, and the answer is the same
    // however loaded the machine is: it cannot be taken.
    let started = Instant::now();
    let error = writable_within(fresh.connection(), Duration::from_millis(100))
        .expect_err("another writer holds the lock");
    assert!(!matches!(error, DbError::Refused(_)), "{error}");
    assert!(error.to_string().contains("busy"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(30));
    assert!(!fresh.connection().in_transaction(), "nothing left open");
    second.execute_batch("ROLLBACK").expect("released");
    writable(fresh.connection()).expect("free again");
}

/// Makes `dir` unwritable, and says whether that took effect: a process that
/// ignores permissions (a privileged one) cannot test it, and the test says so.
#[cfg(unix)]
fn protect(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).expect("chmod");
    let probe = dir.join("probe");
    let created = std::fs::File::create(&probe).is_ok();
    if created {
        std::fs::remove_file(&probe).expect("removed");
    }
    !created
}

#[cfg(unix)]
fn unprotect(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

#[cfg(unix)]
#[test]
fn a_write_protected_directory_is_refused_in_its_own_words_and_the_first_real_write_fails_alike() {
    let mut fresh = Fresh::file();
    fresh
        .connection()
        .execute_batch("CREATE TABLE app(a); INSERT INTO app VALUES (1)")
        .expect("data");
    let dir = fresh.path().parent().expect("a directory").to_path_buf();
    let before = std::fs::read(fresh.path()).expect("bytes");
    if !protect(&dir) {
        unprotect(&dir);
        eprintln!("skipped: this process ignores directory permissions, so nothing is protected");
        return;
    }
    let run = |fresh: &mut Fresh| {
        let c = fresh.connection();
        let refused = writable(c).expect_err("the journal cannot be made");
        let DbError::Refused(message) = &refused else {
            panic!("expected a refusal: {refused:?}");
        };
        assert!(message.contains("write-protected"), "{message}");
        assert!(message.contains("1544"), "{message}");
        assert!(!c.in_transaction(), "nothing left open");
        // The first real write fails with the code the probe reported.
        let real = c
            .execute("INSERT INTO app VALUES (2)", &[])
            .expect_err("the apply's first write");
        assert_eq!(real.server_error_code().as_deref(), Some("1544"));
        // And the stage decides whether it is asked.
        let apply =
            before_apply(c, &plan([]), Version::new(3, 54, 1), Stage::Apply).expect("asked");
        assert_eq!(apply.problems.len(), 1, "{apply:?}");
        let planned =
            before_apply(c, &plan([]), Version::new(3, 54, 1), Stage::Plan).expect("asked");
        assert!(
            planned.problems.is_empty(),
            "plan writes nothing: {planned:?}"
        );
    };
    run(&mut fresh);
    unprotect(&dir);
    assert_eq!(std::fs::read(fresh.path()).expect("bytes"), before);
}

#[cfg(unix)]
#[test]
fn a_write_ahead_file_in_a_write_protected_directory_is_refused_when_it_is_opened() {
    // The connection is closed before the directory is protected, which removes the
    // write-ahead file's companions, so a file on its own is what is opened.
    let dir = std::env::temp_dir().join(format!("pbps-sqlite-{:032x}", rand::random::<u128>()));
    std::fs::create_dir(&dir).expect("a temporary directory");
    let path = dir.join("wal.db");
    {
        let c = Connection::open(&path, OpenMode::CreateIfMissing).expect("a new file");
        assert_eq!(text(&c, "PRAGMA journal_mode = WAL"), "wal");
    }
    let protected = protect(&dir);
    // The shared-memory file cannot be made, so even a read fails: `writable`
    // has nothing to add to what the open already says.
    let opened = Connection::open(&path, OpenMode::ExistingOnly);
    unprotect(&dir);
    std::fs::remove_dir_all(&dir).expect("removed");
    if !protected {
        eprintln!("skipped: this process ignores directory permissions, so nothing is protected");
        return;
    }
    assert!(opened.is_err(), "a write-ahead file cannot be read here");
}

#[test]
fn the_floor_is_a_note_below_the_library_and_nothing_at_or_above_it() {
    let library = Version::new(3, 54, 1);
    let below = floor_notes(library, Version::new(3, 35, 0));
    assert_eq!(below.len(), 1);
    assert!(
        below[0].contains("3.54.1") && below[0].contains("3.35.0"),
        "{below:?}"
    );
    assert!(floor_notes(library, library).is_empty());
    assert!(floor_notes(library, Version::new(3, 60, 0)).is_empty());
}

#[test]
fn before_apply_gathers_every_check_and_a_clean_database_has_only_the_floor_note() {
    let mut fresh = Fresh::file();
    let floor = Version::new(3, 35, 0);
    let c = fresh.connection();
    let clean = before_apply(c, &plan([]), floor, Stage::Apply).expect("asked");
    assert!(clean.problems.is_empty(), "{clean:?}");
    assert_eq!(clean.notes.len(), 1, "{clean:?}");
    // At the library's own version there is nothing to note.
    let current =
        before_apply(c, &plan([]), c.version().expect("version"), Stage::Apply).expect("asked");
    assert_eq!(current, Verdict::default());

    // A wrong setting and a trigger on a ledger table are both found, once each.
    ensure_tables(c).expect("a ledger");
    c.execute_batch(
        "PRAGMA legacy_alter_table = ON; \
         CREATE TRIGGER mute BEFORE INSERT ON __pbps_state BEGIN SELECT RAISE(IGNORE); END",
    )
    .expect("damage");
    let found = before_apply(c, &plan([]), floor, Stage::Apply).expect("asked");
    assert_eq!(found.problems.len(), 2, "{found:?}");
    assert!(
        found.problems[0].contains("legacy_alter_table"),
        "{found:?}"
    );
    assert!(
        found.problems[1].contains("`mute` is a trigger"),
        "{found:?}"
    );
}

#[test]
fn the_refusal_carries_each_line_and_says_nothing_was_changed() {
    let lines = vec!["first".to_owned(), "second".to_owned()];
    let text = refusal(&lines);
    assert!(text.starts_with("first;\nsecond."), "{text}");
    assert!(text.contains("Nothing was changed"), "{text}");
}

#[test]
fn a_database_the_connection_may_not_write_is_refused_in_the_generic_words() {
    let mut fresh = Fresh::file();
    let c = fresh.connection();
    c.execute_batch("PRAGMA query_only = ON").expect("set");
    let refused = writable(c).expect_err("read-only");
    let DbError::Refused(message) = &refused else {
        panic!("expected a refusal: {refused:?}");
    };
    // Not the directory's sentence: nothing says the directory is the cause.
    assert!(message.contains("read-only"), "{message}");
    assert!(!message.contains("write-protected"), "{message}");
    assert!(!c.in_transaction(), "nothing left open");
}

#[test]
fn the_callers_wait_is_put_back_after_the_probe() {
    let mut fresh = Fresh::file();
    let c = fresh.connection();
    c.set_busy_timeout(Duration::from_millis(1234))
        .expect("set");
    writable(c).expect("writable");
    let rows = c.query("PRAGMA busy_timeout", &[]).expect("query");
    assert_eq!(rows[0].integer(0).expect("ms"), 1234);
}

#[test]
fn an_older_library_is_told_what_the_plan_needs_and_the_note_follows_it() {
    let mut fresh = Fresh::file();
    let (_, check) = forms().remove(3);
    let floor = Version::new(3, 35, 0);
    let c = fresh.connection();
    let old = verdict_for(
        c,
        Version::new(3, 40, 0),
        &plan([check.clone()]),
        floor,
        Stage::Plan,
    )
    .expect("asked");
    assert_eq!(old.problems.len(), 1, "{old:?}");
    assert!(
        old.problems[0].contains("adding a check constraint"),
        "{old:?}"
    );
    assert!(old.notes[0].contains("3.40.0"), "{old:?}");
    // Negative: the same plan on a library that has the form.
    let new = verdict_for(
        c,
        Version::new(3, 54, 1),
        &plan([check]),
        floor,
        Stage::Plan,
    )
    .expect("asked");
    assert!(new.problems.is_empty(), "{new:?}");
}

#[test]
fn the_probe_waits_for_a_writer_that_leaves_instead_of_failing_at_once() {
    use std::sync::mpsc;
    let mut fresh = Fresh::file();
    fresh
        .connection()
        .execute_batch("CREATE TABLE app(a)")
        .expect("table");
    let path = fresh.path().to_path_buf();
    let (held, wait_for_it) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        let second = Connection::open(&path, OpenMode::ExistingOnly).expect("opens");
        second.execute_batch("BEGIN IMMEDIATE").expect("held");
        held.send(()).expect("told");
        std::thread::sleep(Duration::from_millis(200));
        second.execute_batch("ROLLBACK").expect("released");
    });
    wait_for_it.recv().expect("the writer holds the lock");
    // A wait far longer than the writer stays: the probe must outlast it, however
    // loaded the machine is, where a connection with no wait fails at once.
    writable_within(fresh.connection(), Duration::from_secs(60)).expect("waited and wrote");
    writer.join().expect("the writer ended");
}

/// Without a journal a rollback is not reliable, and both an apply and the
/// probe rest on one.
#[test]
fn a_connection_with_the_journal_off_is_refused_and_the_probe_writes_nothing() {
    let mut fresh = Fresh::file();
    let c = fresh.connection();
    c.execute_batch("CREATE TABLE app(a)").expect("a table");
    assert_eq!(text(c, "PRAGMA journal_mode = OFF"), "off");
    let found = connection_problems(c).expect("asked");
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("`PRAGMA journal_mode` reads off"),
        "{found:?}"
    );
    match writable(c) {
        Err(DbError::Refused(line)) => assert_eq!(line, found[0]),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(!c.in_transaction());
    assert_eq!(
        c.query(
            "SELECT count(*) FROM sqlite_schema WHERE name LIKE '__pbps_probe_%'",
            &[]
        )
        .expect("query")[0]
            .integer(0)
            .expect("count"),
        0
    );
    // Said once by the whole check, though both the connection and the probe say it.
    let verdict = before_apply(c, &plan([]), Version::new(3, 35, 0), Stage::Apply).expect("asked");
    assert_eq!(
        verdict
            .problems
            .iter()
            .filter(|line| line.contains("journal_mode"))
            .count(),
        1,
        "{verdict:?}"
    );
    // Negative: with the journal back, neither refuses.
    assert_eq!(text(c, "PRAGMA journal_mode = DELETE"), "delete");
    assert_eq!(connection_problems(c).expect("asked"), Vec::<String>::new());
    writable(c).expect("writable");
}
