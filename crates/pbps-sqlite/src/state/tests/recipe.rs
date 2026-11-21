//! The recipe, and creating the ledger from it.

use pbps_db::DbError;
use pbps_db::ledger::STATE_TABLE_NAME;

use super::{number, text};
use crate::names;
use crate::state::ddl::{RECIPE, in_a_unit};
use crate::state::{STATE_TABLE, ensure_tables, is_initialized};
use crate::tests::Fresh;

const STATE_TEXT: &str = "CREATE TABLE __pbps_state (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    applied_at       TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now')),
    kind             TEXT    NOT NULL CHECK (length(kind) <= 16),
    git_sha          TEXT    CHECK (length(git_sha) <= 40),
    plan_checksum    TEXT    CHECK (length(plan_checksum) <= 64),
    state_json       TEXT    NOT NULL,
    operator         TEXT    NOT NULL CHECK (length(operator) <= 128),
    reason           TEXT    CHECK (length(reason) <= 1000),
    state_version    INTEGER,
    tables_count     INTEGER,
    modules_count    INTEGER,
    staged_completed INTEGER,
    staged_total     INTEGER
)";

const LOCK_TEXT: &str = "CREATE TABLE __pbps_lock (
    id        INTEGER PRIMARY KEY CHECK (id = 1),
    locked_by TEXT    NOT NULL CHECK (length(locked_by) <= 256),
    locked_at TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now')),
    proof     TEXT    NOT NULL CHECK (proof IN ('file', 'row')),
    sidecar   TEXT,
    token     TEXT    NOT NULL
)";

const ENVIRONMENT_TEXT: &str = "CREATE TABLE __pbps_environment (
    id             INTEGER PRIMARY KEY CHECK (id = 1),
    environment_id TEXT    NOT NULL CHECK (length(environment_id) = 32)
)";

#[test]
fn the_stored_text_of_the_lock_and_the_environment_tables_is_the_recipe() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    ensure_tables(c).expect("created");
    for (name, golden, index) in [
        ("__pbps_lock", LOCK_TEXT, 1),
        ("__pbps_environment", ENVIRONMENT_TEXT, 2),
    ] {
        let stored = text(
            c,
            &format!("SELECT sql FROM sqlite_schema WHERE name = '{name}'"),
        );
        assert_eq!(stored, RECIPE[index].stored(), "{name}");
        assert_eq!(stored, golden, "{name}");
    }
}

#[test]
fn the_recipe_names_are_exactly_the_reserved_ledger_names() {
    let mut recipe: Vec<&str> = RECIPE.iter().map(|r| r.name).collect();
    let mut reserved: Vec<&str> = names::LEDGER_TABLES.to_vec();
    recipe.sort_unstable();
    reserved.sort_unstable();
    assert_eq!(recipe, reserved);
    // Only the environment table is seeded.
    assert_eq!(
        RECIPE.iter().filter(|r| r.seed.is_some()).count(),
        1,
        "one table holds a row from the day it is made"
    );
}

#[test]
fn a_view_in_the_lock_or_environment_name_is_refused_beside_an_existing_state_table() {
    for name in ["__pbps_lock", "__pbps_environment"] {
        let mut fresh = Fresh::memory();
        let c = fresh.connection();
        c.execute_batch(&format!("CREATE VIEW {name} AS SELECT 1 AS id"))
            .expect("ddl");
        let refused = ensure_tables(c).expect_err(name);
        assert!(
            matches!(refused, DbError::Refused(_)),
            "{name}: {refused:?}"
        );
        // Nothing was made: not even the state table.
        assert_eq!(
            number(c, "SELECT count(*) FROM sqlite_schema WHERE type = 'table'"),
            0,
            "{name}"
        );
    }
}

#[test]
fn a_ledger_that_lacks_the_newer_tables_gets_them_at_its_next_write_and_not_before() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    // A ledger with only a state table, as an earlier build made it.
    c.execute_batch(&RECIPE[0].create()).expect("ddl");
    assert!(
        is_initialized(c).expect("asked"),
        "the state table is the ledger"
    );
    assert_eq!(
        number(c, "SELECT count(*) FROM sqlite_schema WHERE type = 'table'"),
        2
    );
    ensure_tables(c).expect("completed");
    assert_eq!(
        number(c, "SELECT count(*) FROM sqlite_schema WHERE type = 'table'"),
        4
    );
}

#[test]
fn every_recipe_table_is_a_reserved_ledger_name_and_spelled_main_qualified() {
    for recipe in &RECIPE {
        assert!(names::is_ledger(recipe.name), "{}", recipe.name);
        assert!(recipe.body.starts_with(recipe.name), "{}", recipe.name);
    }
    assert_eq!(STATE_TABLE, format!("main.{STATE_TABLE_NAME}"));
    // Negative: a name this tool does not keep is not one.
    assert!(!names::is_ledger("__pbps_states"));
}

#[test]
fn the_stored_text_of_the_state_table_is_the_recipe() {
    let mut fresh = Fresh::memory();
    ensure_tables(fresh.connection()).expect("created");
    let stored = text(
        fresh.connection(),
        "SELECT sql FROM sqlite_schema WHERE name = '__pbps_state'",
    );
    assert_eq!(stored, RECIPE[0].stored());
    // The text itself, written out: the recipe is what files that pbps has
    // touched hold, so changing it is a decision and not a refactor.
    assert_eq!(stored, STATE_TEXT);
    // Negative: what the engine stores is the text from the name on, so a table
    // with the same columns written another way is not the recipe's.
    let mut other = Fresh::memory();
    other
        .connection()
        .execute_batch("CREATE TABLE __pbps_state (id INTEGER PRIMARY KEY AUTOINCREMENT)")
        .expect("ddl");
    let stored = text(
        other.connection(),
        "SELECT sql FROM sqlite_schema WHERE name = '__pbps_state'",
    );
    assert_ne!(stored, RECIPE[0].stored());
}

#[test]
fn the_ledger_creates_once_and_is_initialized_afterwards() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    assert!(!is_initialized(c).expect("asked"), "absent is not an error");
    ensure_tables(c).expect("created");
    assert!(is_initialized(c).expect("asked"));
    let before = text(c, "SELECT group_concat(sql) FROM sqlite_schema");
    ensure_tables(c).expect("again");
    assert_eq!(
        before,
        text(c, "SELECT group_concat(sql) FROM sqlite_schema")
    );
    // The three tables, and `AUTOINCREMENT`'s own counter table, and nothing else.
    assert_eq!(
        number(
            c,
            "SELECT count(*) FROM sqlite_schema WHERE name NOT IN \
             ('__pbps_state', '__pbps_lock', '__pbps_environment', 'sqlite_sequence')"
        ),
        0
    );
    assert_eq!(
        number(c, "SELECT count(*) FROM sqlite_schema WHERE type = 'table'"),
        4
    );
}

#[test]
fn a_ledger_name_in_another_case_is_the_ledger_and_is_not_created_twice() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    c.execute_batch(&format!(
        "CREATE TABLE main.{}",
        RECIPE[0].body.replacen("__pbps_state", "__PBPS_STATE", 1)
    ))
    .expect("ddl");
    assert!(is_initialized(c).expect("asked"));
    // Its text is not the recipe's (the name is written in another case), so the
    // integrity check refuses it, and the missing tables are not made beside it.
    let refused = ensure_tables(c).expect_err("not the ledger this tool creates");
    assert!(matches!(refused, DbError::Refused(_)), "{refused:?}");
    assert!(
        refused
            .to_string()
            .contains("`__PBPS_STATE` is not defined"),
        "{refused}"
    );
    assert_eq!(
        number(
            c,
            "SELECT count(*) FROM sqlite_schema WHERE name = '__pbps_state' COLLATE NOCASE"
        ),
        1
    );
    assert_eq!(
        number(
            c,
            "SELECT count(*) FROM sqlite_schema WHERE name LIKE '\\_\\_pbps%' ESCAPE '\\'"
        ),
        1
    );
}

#[test]
fn an_object_that_is_not_a_table_in_the_ledgers_name_is_refused_not_skipped() {
    for (what, ddl) in [
        ("a view", "CREATE VIEW __pbps_state AS SELECT 1 AS id"),
        (
            "an index",
            "CREATE TABLE t (a); CREATE INDEX __pbps_state ON t (a)",
        ),
        (
            "a view in another case",
            "CREATE VIEW __PBPS_STATE AS SELECT 1 AS id",
        ),
    ] {
        let mut fresh = Fresh::memory();
        let c = fresh.connection();
        c.execute_batch(ddl).expect("ddl");
        let created = ensure_tables(c).expect_err(what);
        assert!(
            matches!(created, DbError::Refused(_)),
            "{what}: {created:?}"
        );
        assert!(
            created.to_string().contains("not an ordinary table"),
            "{what}: {created}"
        );
        // Asking is the same refusal, and not "absent".
        assert!(
            matches!(is_initialized(c), Err(DbError::Refused(_))),
            "{what}"
        );
        // Nothing was made beside it.
        assert_eq!(
            number(
                c,
                "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = '__pbps_state' COLLATE NOCASE"
            ),
            0,
            "{what}"
        );
    }
    // Negative: an ordinary table of the name is not refused as a stranger of
    // another kind; whether it is the ledger is the integrity check's question
    // (`problems`), and it is refused there only for its text.
    let mut fresh = Fresh::memory();
    ensure_tables(fresh.connection()).expect("the ledger this tool creates is accepted");
    ensure_tables(fresh.connection()).expect("and again");
}

#[test]
fn a_view_is_named_escaped_in_the_refusal() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    c.execute_batch("CREATE VIEW \"__PBPS_STATE\" AS SELECT 1")
        .expect("ddl");
    let message = ensure_tables(c).expect_err("refused").to_string();
    assert!(message.contains("`__PBPS_STATE` is a view"), "{message}");
}

#[test]
fn a_temp_table_of_the_ledgers_name_does_not_shadow_it() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    c.execute_batch("CREATE TEMP TABLE __pbps_state (x)")
        .expect("ddl");
    assert!(
        !is_initialized(c).expect("asked"),
        "the temp table is not the ledger"
    );
    ensure_tables(c).expect("created in main");
    assert_eq!(
        number(
            c,
            "SELECT count(*) FROM main.sqlite_schema WHERE name = '__pbps_state'"
        ),
        1
    );
    assert!(is_initialized(c).expect("asked"));
    // A record goes to `main` too: an unqualified name would find the temp one.
    crate::state::record(c, &super::snapshot(pbps_model::StateKind::Apply, "op"))
        .expect("recorded");
    assert_eq!(number(c, "SELECT count(*) FROM main.__pbps_state"), 1);
    assert_eq!(number(c, "SELECT count(*) FROM temp.__pbps_state"), 0);
}

#[test]
fn a_unit_that_fails_leaves_nothing_and_one_that_succeeds_commits() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    let failed = in_a_unit(c, || {
        c.execute_batch("CREATE TABLE a (x)")?;
        c.execute_batch("CREATE TABLE a (x)")
    });
    assert!(failed.is_err());
    assert!(!c.in_transaction(), "the unit left no transaction open");
    assert_eq!(
        number(c, "SELECT count(*) FROM sqlite_schema WHERE name = 'a'"),
        0,
        "the first statement of the unit was undone with it"
    );
    in_a_unit(c, || c.execute_batch("CREATE TABLE b (x)")).expect("committed");
    assert!(!c.in_transaction());
    assert_eq!(
        number(c, "SELECT count(*) FROM sqlite_schema WHERE name = 'b'"),
        1
    );
}

#[test]
fn a_unit_that_reads_before_it_writes_waits_for_another_writer() {
    // The shape that was refused at once: the unit holds a read lock when it asks
    // for the write lock, and another connection is writing.
    super::in_both_journal_modes(|fresh, _| {
        fresh
            .connection()
            .execute_batch("CREATE TABLE seen (x)")
            .expect("made");
        let path = fresh.path().to_path_buf();
        let (held, wait_for_it) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let other =
                pbps_db::sqlite::Connection::open(&path, pbps_db::sqlite::OpenMode::ExistingOnly)
                    .expect("opens");
            other
                .execute_batch("BEGIN IMMEDIATE; CREATE TABLE theirs (x)")
                .expect("holds the write lock");
            held.send(()).expect("told");
            std::thread::sleep(std::time::Duration::from_millis(300));
            other.execute_batch("COMMIT").expect("commits");
        });
        wait_for_it.recv().expect("the writer holds the lock");
        let c = fresh.connection();
        c.set_busy_timeout(std::time::Duration::from_secs(20))
            .expect("set");
        in_a_unit(c, || {
            c.query("SELECT count(*) FROM seen", &[])?;
            c.execute_batch("CREATE TABLE ours (x)")
        })
        .expect("the unit waited for the writer and then wrote");
        writer.join().expect("joined");
        assert!(!c.in_transaction());
        assert_eq!(
            number(
                c,
                "SELECT count(*) FROM sqlite_schema WHERE name IN ('ours', 'theirs')"
            ),
            2
        );
    });
}

#[test]
fn a_unit_does_not_wait_longer_than_the_connection_allows() {
    // Negative: the wait is the caller's, and a writer that stays is still an error.
    super::in_both_journal_modes(|fresh, second| {
        second
            .execute_batch("BEGIN IMMEDIATE; CREATE TABLE theirs (x)")
            .expect("holds the write lock");
        let c = fresh.connection();
        c.set_busy_timeout(std::time::Duration::from_millis(50))
            .expect("set");
        let refused = in_a_unit(c, || c.execute_batch("CREATE TABLE ours (x)"))
            .expect_err("the writer never left");
        assert!(crate::state::busy::is_busy(&refused), "{refused:?}");
        assert!(!c.in_transaction(), "no transaction is left open");
        second.execute_batch("ROLLBACK").expect("undone");
    });
}

#[test]
fn a_unit_inside_a_transaction_undoes_only_itself() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    c.execute_batch("BEGIN IMMEDIATE; CREATE TABLE kept (x)")
        .expect("open");
    let failed = in_a_unit(c, || {
        c.execute_batch("CREATE TABLE undone (x)")?;
        Err::<(), _>(DbError::Refused("stop".into()))
    });
    assert!(failed.is_err());
    assert!(c.in_transaction(), "the caller's transaction is still open");
    c.execute_batch("COMMIT").expect("commit");
    assert_eq!(
        number(
            c,
            "SELECT count(*) FROM sqlite_schema WHERE name IN ('kept', 'undone')"
        ),
        1
    );
    assert_eq!(
        number(c, "SELECT count(*) FROM sqlite_schema WHERE name = 'kept'"),
        1
    );
}

/// The probe takes no lock, so a view of the ledger's name can appear between
/// it and the statement, and `CREATE TABLE IF NOT EXISTS` over a view succeeds
/// and makes nothing. The creation reads the catalog again and refuses.
#[test]
fn a_view_that_appears_after_the_probe_is_refused_by_the_creation_itself() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    // What the race leaves: the probe saw nothing, and now there is a view.
    c.execute_batch("CREATE VIEW __pbps_state AS SELECT 1 AS id")
        .expect("the view");
    let missing: Vec<&crate::state::ddl::Recipe> = RECIPE.iter().collect();
    // The engine's side: the statement alone reports nothing.
    c.execute_batch(&RECIPE[0].create())
        .expect("a silent no-op");
    let refused = crate::state::ddl::create(c, &missing).expect_err("refused");
    assert!(
        refused
            .to_string()
            .contains("is a view, not an ordinary table"),
        "{refused}"
    );
    assert!(!c.in_transaction());
    // Negative: with the name free the same call creates the table.
    c.execute_batch("DROP VIEW __pbps_state").expect("dropped");
    crate::state::ddl::create(c, &missing).expect("created");
    assert!(is_initialized(c).expect("asked"));
}

/// A seed is sent only after the catalog confirms what holds the name: a view
/// that appeared in the environment's name is refused in this tool's words and
/// not answered by the engine's error for an insert into a view.
#[test]
fn a_view_in_a_seeded_tables_name_is_refused_before_the_seed_is_sent() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    c.execute_batch("CREATE VIEW __pbps_environment AS SELECT 1 AS id, 'x' AS environment_id")
        .expect("the view");
    let missing: Vec<&crate::state::ddl::Recipe> = RECIPE.iter().collect();
    let refused = crate::state::ddl::create(c, &missing).expect_err("refused");
    assert!(
        refused
            .to_string()
            .contains("is a view, not an ordinary table"),
        "{refused}"
    );
    assert!(!c.in_transaction());
    assert_eq!(
        number(
            c,
            "SELECT count(*) FROM main.sqlite_schema WHERE type = 'table'"
        ),
        0,
        "nothing of a ledger is left beside it"
    );
}
