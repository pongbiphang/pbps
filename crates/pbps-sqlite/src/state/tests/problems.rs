//! The integrity check: what is a problem, what is not, and what a write does
//! with the answer.

use std::time::Duration;

use pbps_db::DbError;
use pbps_db::ledger::LedgerError;
use pbps_db::sqlite::{Connection, OpenMode};
use pbps_model::StateKind;

use super::{number, snapshot, text};
use crate::state::busy::is_busy;
use crate::state::ddl::RECIPE;
use crate::state::problems::ledger_problems_within;
use crate::state::{
    LOCK_TABLE, STATE_TABLE, ensure_tables, environment_identity, latest, ledger_problems, lock,
    lock_holder, prune, record, release, timeline, unlock, untrusted_ledger_message,
};
use crate::tests::Fresh;

const NAMES: [&str; 3] = ["__pbps_state", "__pbps_lock", "__pbps_environment"];

/// A file with the ledger made, so a second connection and a lock sidecar work.
fn ledger() -> Fresh {
    let mut fresh = Fresh::file();
    ensure_tables(fresh.connection()).expect("created");
    fresh
}

/// The one problem found, which must name `part`.
fn the_problem(c: &Connection, part: &str) -> String {
    let mut found = ledger_problems(c).expect("asked");
    assert_eq!(found.len(), 1, "{found:#?}");
    let line = found.remove(0);
    assert!(line.contains(part), "{line}");
    line
}

/// Drops the ledger table `name` and runs `ddl` in its place.
fn replace(c: &Connection, name: &str, ddl: &str) {
    c.execute_batch(&format!("DROP TABLE main.{name}; {ddl}"))
        .expect("replaced");
}

/// Another table's idea of a ledger table: no column in common with the recipe.
fn stranger_table(name: &str) -> String {
    format!(
        "CREATE TABLE main.{name} (id INTEGER PRIMARY KEY, note TEXT); \
         INSERT INTO main.{name} VALUES (1, 'not the ledger')"
    )
}

#[test]
fn a_ledger_made_by_this_tool_has_no_problems_and_so_does_an_absent_one() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    assert_eq!(ledger_problems(c).expect("asked"), Vec::<String>::new());
    // A read creates nothing: the catalog is still empty.
    assert_eq!(number(c, "SELECT count(*) FROM sqlite_schema"), 0);
    ensure_tables(c).expect("created");
    assert_eq!(ledger_problems(c).expect("asked"), Vec::<String>::new());
    record(c, &snapshot(StateKind::Apply, "op")).expect("recorded");
    assert!(ledger_problems(c).expect("asked").is_empty());
}

#[test]
fn a_partial_ledger_is_not_a_problem_and_the_next_write_completes_it() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    c.execute_batch(&RECIPE[0].create()).expect("state only");
    assert!(ledger_problems(c).expect("asked").is_empty());
    assert_eq!(
        number(
            c,
            "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name LIKE '\\_\\_pbps%' ESCAPE '\\'"
        ),
        1
    );
    ensure_tables(c).expect("completed");
    assert_eq!(
        number(
            c,
            "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name LIKE '\\_\\_pbps%' ESCAPE '\\'"
        ),
        3
    );
    // The other way round is as little a problem: a lock and no state table.
    let mut other = Fresh::memory();
    other
        .connection()
        .execute_batch(&RECIPE[1].create())
        .expect("lock only");
    assert!(
        ledger_problems(other.connection())
            .expect("asked")
            .is_empty()
    );
}

#[test]
fn every_deviation_from_the_recipe_is_one_line_naming_the_object() {
    let table =
        |index: usize, tail: &str| format!("CREATE TABLE main.{}{tail}", RECIPE[index].body);
    // (what, the table dropped for the statement, the statement, the line expected)
    let cases: Vec<(&str, Option<&str>, String, &str)> = vec![
        (
            "an index on the state table",
            None,
            "CREATE INDEX extra_index ON __pbps_state (kind)".into(),
            "`extra_index` is an index on `__pbps_state`",
        ),
        (
            "a trigger on the lock table",
            None,
            "CREATE TRIGGER t BEFORE DELETE ON __pbps_lock BEGIN SELECT RAISE(IGNORE); END".into(),
            "`t` is a trigger on `__pbps_lock`",
        ),
        (
            "a column added",
            None,
            "ALTER TABLE main.__pbps_state ADD COLUMN extra INTEGER".into(),
            "`__pbps_state` is not defined as this tool creates it",
        ),
        (
            "a column renamed",
            None,
            "ALTER TABLE main.__pbps_environment RENAME COLUMN environment_id TO e".into(),
            "`__pbps_environment` is not defined",
        ),
        (
            "a column dropped",
            None,
            "ALTER TABLE main.__pbps_state DROP COLUMN reason".into(),
            "`__pbps_state` is not defined",
        ),
        (
            "a foreign key onto the state table",
            None,
            "CREATE TABLE child (p REFERENCES __pbps_state (id) ON DELETE CASCADE)".into(),
            "`child` has a foreign key onto `__pbps_state`",
        ),
        (
            "rebuilt WITHOUT ROWID",
            Some("__pbps_lock"),
            table(1, " WITHOUT ROWID"),
            "`__pbps_lock` is not defined",
        ),
        (
            "rebuilt STRICT",
            Some("__pbps_lock"),
            table(1, " STRICT"),
            "`__pbps_lock` is not defined",
        ),
        (
            "a type written another way",
            Some("__pbps_environment"),
            table(2, "").replacen("INTEGER PRIMARY KEY", "integer PRIMARY KEY", 1),
            "`__pbps_environment` is not defined",
        ),
        (
            "a check left out",
            Some("__pbps_lock"),
            table(1, "").replacen(" CHECK (id = 1)", "", 1),
            "`__pbps_lock` is not defined",
        ),
        (
            "the name in another case",
            Some("__pbps_lock"),
            table(1, "").replacen("__pbps_lock", "__PBPS_LOCK", 1),
            "`__PBPS_LOCK` is not defined",
        ),
        (
            "a virtual table",
            Some("__pbps_environment"),
            "CREATE VIRTUAL TABLE main.__pbps_environment USING fts5(a)".into(),
            "`__pbps_environment` is not defined",
        ),
        (
            "a view",
            Some("__pbps_environment"),
            "CREATE VIEW main.__pbps_environment AS SELECT 1 AS id".into(),
            "`__pbps_environment` is a view, not an ordinary table",
        ),
    ];
    for (label, dropped, ddl, expected) in cases {
        let mut fresh = ledger();
        let c = fresh.connection();
        match dropped {
            Some(name) => replace(c, name, &ddl),
            None => c.execute_batch(&ddl).expect(label),
        }
        let line = the_problem(c, expected);
        assert!(
            line.len() < 400,
            "{label}: not a dump of two statements: {line}"
        );
        // The refusal a write gives carries the line and the remedy.
        let message = untrusted_ledger_message(std::slice::from_ref(&line));
        assert!(
            message.starts_with(&line) && message.contains("restore them"),
            "{label}"
        );
    }
}

#[test]
fn a_view_in_a_ledger_name_is_reported_in_the_words_ensure_tables_refuses_it_in() {
    let mut fresh = ledger();
    let c = fresh.connection();
    replace(
        c,
        "__pbps_lock",
        "CREATE VIEW main.__pbps_lock AS SELECT 1 AS id",
    );
    let line = the_problem(c, "is a view, not an ordinary table");
    match ensure_tables(c) {
        Err(DbError::Refused(message)) => assert!(message.contains(&line), "{message}"),
        other => panic!("expected a refusal: {other:?}"),
    }
}

#[test]
fn the_line_says_where_the_definition_departs_and_quotes_only_a_little() {
    let mut fresh = ledger();
    let c = fresh.connection();
    c.execute_batch("ALTER TABLE main.__pbps_state ADD COLUMN extra INTEGER")
        .expect("added");
    let line = the_problem(c, "from character");
    assert!(line.contains(", extra INTEGER)"), "{line}");
    // Not the whole of either statement: the recipe's is far longer than this.
    assert!(!line.contains("staged_completed"), "{line}");
}

#[test]
fn the_text_is_compared_whole_and_not_by_the_columns_it_declares() {
    let mut fresh = ledger();
    let c = fresh.connection();
    // The same columns and types, one `CHECK` less: what `pragma table_info`
    // shows is identical and the text is not.
    let before = text(
        c,
        "SELECT group_concat(name || type) FROM pragma_table_info('__pbps_lock')",
    );
    replace(
        c,
        "__pbps_lock",
        &format!(
            "CREATE TABLE main.{}",
            RECIPE[1].body.replacen(" CHECK (id = 1)", "", 1)
        ),
    );
    assert_eq!(
        text(
            c,
            "SELECT group_concat(name || type) FROM pragma_table_info('__pbps_lock')"
        ),
        before
    );
    the_problem(c, "`__pbps_lock` is not defined");
}

#[test]
fn a_user_table_in_a_ledger_name_is_a_problem_whatever_the_rest_of_the_ledger_is() {
    for name in NAMES {
        // Beside a genuine ledger: the other two tables are the recipe's.
        let mut fresh = ledger();
        let c = fresh.connection();
        replace(c, name, &stranger_table(name));
        the_problem(
            c,
            &format!("`{name}` is not defined as this tool creates it"),
        );

        // And with nothing else there at all.
        let mut alone = Fresh::memory();
        let c = alone.connection();
        c.execute_batch(&stranger_table(name))
            .expect("a user table");
        the_problem(c, &format!("`{name}` is not defined"));
    }
}

#[test]
fn every_write_is_refused_over_a_user_table_in_a_ledger_name_and_leaves_it_alone() {
    for name in NAMES {
        let mut fresh = ledger();
        let path = fresh.path().to_path_buf();
        replace(fresh.connection(), name, &stranger_table(name));
        let c = fresh.connection();
        let schema = text(c, "SELECT group_concat(sql, ';') FROM sqlite_schema");
        let writes: Vec<(&str, bool)> = vec![
            ("ensure_tables", ensure_tables(c).is_err()),
            (
                "record",
                record(c, &snapshot(StateKind::Apply, "op")).is_err(),
            ),
            ("lock", lock(c, "holder").is_err()),
            ("prune", prune(c, 1).is_err()),
            ("unlock", unlock(c).is_err()),
        ];
        for (write, refused) in writes {
            assert!(refused, "{name}: {write} must be refused");
        }
        // Nothing was written into the stranger and nothing was made beside it.
        assert_eq!(
            number(c, &format!("SELECT count(*) FROM main.{name}")),
            1,
            "{name}"
        );
        assert_eq!(
            text(c, &format!("SELECT note FROM main.{name} WHERE id = 1")),
            "not the ledger",
            "{name}"
        );
        assert_eq!(
            text(c, "SELECT group_concat(sql, ';') FROM sqlite_schema"),
            schema
        );
        // Another connection sees the same file.
        let other = Connection::open(&path, OpenMode::ExistingOnly).expect("opens");
        assert_eq!(
            number(&other, &format!("SELECT count(*) FROM main.{name}")),
            1
        );
    }
}

#[test]
fn a_user_table_in_a_ledger_name_is_refused_before_a_missing_table_is_made_beside_it() {
    let mut fresh = Fresh::file();
    let c = fresh.connection();
    // The state table is the genuine one, and the stranger holds the lock's name.
    c.execute_batch(&RECIPE[0].create()).expect("state table");
    c.execute_batch(&stranger_table("__pbps_lock"))
        .expect("a user table");
    assert!(matches!(ensure_tables(c), Err(DbError::Refused(_))));
    assert_eq!(
        number(
            c,
            "SELECT count(*) FROM sqlite_schema WHERE name = '__pbps_environment'"
        ),
        0,
        "the environment table was not made beside an untrusted ledger"
    );
}

#[test]
fn every_write_is_refused_on_an_untrusted_ledger_and_reads_are_not() {
    let mut fresh = ledger();
    let first = lock(fresh.connection(), "holder").expect("locked");
    record(fresh.connection(), &snapshot(StateKind::Apply, "op")).expect("recorded");
    let c = fresh.connection();
    c.execute_batch(
        "CREATE TRIGGER mute BEFORE INSERT ON __pbps_state BEGIN SELECT RAISE(IGNORE); END",
    )
    .expect("a trigger");
    for (write, refused) in [
        ("ensure_tables", ensure_tables(c).is_err()),
        (
            "record",
            matches!(
                record(c, &snapshot(StateKind::Apply, "op")),
                Err(LedgerError::Db(DbError::Refused(_)))
            ),
        ),
        (
            "prune",
            matches!(prune(c, 1), Err(LedgerError::Db(DbError::Refused(_)))),
        ),
        ("unlock", matches!(unlock(c), Err(DbError::Refused(_)))),
        (
            "release",
            matches!(release(c, first.guard), Err(DbError::Refused(_))),
        ),
    ] {
        assert!(refused, "{write} must be refused");
    }
    // Refused means untouched: the row of the lock and the entry are still there.
    assert_eq!(number(c, &format!("SELECT count(*) FROM {LOCK_TABLE}")), 1);
    assert_eq!(number(c, &format!("SELECT count(*) FROM {STATE_TABLE}")), 1);
    // A read answers, as on the other engines: it writes nothing.
    assert!(latest(c).expect("read").is_some());
    assert_eq!(timeline(c, 10).expect("read").len(), 1);
    assert!(lock_holder(c).expect("read").is_some());
    assert!(environment_identity(c).expect("read").is_some());
}

#[test]
fn a_ledger_that_is_restored_writes_again() {
    let mut fresh = ledger();
    let c = fresh.connection();
    c.execute_batch(
        "CREATE TRIGGER mute BEFORE INSERT ON __pbps_state BEGIN SELECT RAISE(IGNORE); END",
    )
    .expect("a trigger");
    assert!(record(c, &snapshot(StateKind::Apply, "op")).is_err());
    c.execute_batch("DROP TRIGGER mute").expect("dropped");
    record(c, &snapshot(StateKind::Apply, "op")).expect("recorded again");
    assert!(ledger_problems(c).expect("asked").is_empty());
}

#[test]
fn what_the_check_does_not_see_is_data_and_other_schemas() {
    let mut fresh = ledger();
    let c = fresh.connection();
    // A table of the ledger's name in `temp` shadows nothing: the statements are
    // qualified with `main`, so the write lands in the ledger.
    c.execute_batch("CREATE TEMP TABLE __pbps_state (id INTEGER PRIMARY KEY)")
        .expect("a temporary table");
    assert!(ledger_problems(c).expect("asked").is_empty());
    record(c, &snapshot(StateKind::Apply, "op")).expect("recorded");
    assert_eq!(number(c, &format!("SELECT count(*) FROM {STATE_TABLE}")), 1);
    assert_eq!(number(c, "SELECT count(*) FROM temp.__pbps_state"), 0);
    // Rows are data, not shape.
    c.execute_batch("UPDATE main.sqlite_sequence SET seq = 0")
        .expect("reset");
    assert!(ledger_problems(c).expect("asked").is_empty());
}

#[test]
fn an_edit_of_the_stored_text_is_seen_and_a_file_is_not_checked_by_what_it_cannot_load() {
    let mut fresh = ledger();
    let c = fresh.connection();
    c.execute_batch("PRAGMA writable_schema = ON").expect("on");
    c.execute_batch("UPDATE sqlite_schema SET sql = replace(sql, 'reason', 'reasons') WHERE name = '__pbps_state'")
        .expect("edited");
    the_problem(c, "`__pbps_state` is not defined");
    c.execute_batch("UPDATE sqlite_schema SET sql = replace(sql, 'reasons', 'reason') WHERE name = '__pbps_state'")
        .expect("restored");
    assert!(ledger_problems(c).expect("asked").is_empty());
    // A virtual table whose module this library lacks is the application's own,
    // and the question about foreign keys must not fail on it.
    c.execute_batch(
        "INSERT INTO sqlite_schema (type, name, tbl_name, rootpage, sql) \
         VALUES ('table', 'theirs', 'theirs', 0, 'CREATE VIRTUAL TABLE theirs USING nomodule(a)')",
    )
    .expect("written");
    c.execute_batch("PRAGMA writable_schema = OFF")
        .expect("off");
    // The row is in the file; a connection that reads the schema afresh has it.
    fresh.reopen();
    assert_eq!(
        number(
            fresh.connection(),
            "SELECT count(*) FROM sqlite_schema WHERE name = 'theirs'"
        ),
        1
    );
    assert!(
        ledger_problems(fresh.connection())
            .expect("asked")
            .is_empty()
    );
}

#[test]
fn an_unreadable_catalog_is_an_error_and_never_an_empty_list() {
    let mut fresh = ledger();
    let second = Connection::open(fresh.path(), OpenMode::ExistingOnly).expect("opens");
    // In the delete journal mode an exclusive lock keeps every reader out.
    second.execute_batch("BEGIN EXCLUSIVE").expect("held");
    let error = ledger_problems_within(fresh.connection(), Duration::from_millis(100))
        .expect_err("not an empty list");
    assert!(is_busy(&error), "{error}");
    second.execute_batch("ROLLBACK").expect("released");
    assert!(
        ledger_problems(fresh.connection())
            .expect("asked")
            .is_empty()
    );
}

#[test]
fn the_wait_the_caller_set_is_put_back_by_the_check() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    c.set_busy_timeout(Duration::from_millis(1234))
        .expect("set");
    ledger_problems(c).expect("asked");
    assert_eq!(number(c, "PRAGMA busy_timeout"), 1234);
}

#[test]
fn a_user_table_in_a_ledger_name_makes_a_read_fail_when_its_columns_differ() {
    // The reads do not ask the check, so what keeps one from answering over
    // another table's rows is the columns it names: a table of another shape is
    // an error and never "no entry" or "no holder".
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    c.execute_batch(&format!(
        "{}; {}",
        stranger_table("__pbps_environment"),
        stranger_table("__pbps_lock")
    ))
    .expect("user tables");
    assert!(environment_identity(c).is_err());
    assert!(lock_holder(c).is_err());
}
