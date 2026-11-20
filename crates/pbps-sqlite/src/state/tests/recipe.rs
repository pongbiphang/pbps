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
    // `AUTOINCREMENT` makes the engine keep its own counter table, and nothing
    // else is made.
    assert_eq!(
        number(
            c,
            "SELECT count(*) FROM sqlite_schema WHERE name NOT IN ('__pbps_state', 'sqlite_sequence')"
        ),
        0
    );
}

#[test]
fn a_ledger_name_in_another_case_is_the_ledger_and_is_not_created_twice() {
    let mut fresh = Fresh::memory();
    let c = fresh.connection();
    c.execute_batch("CREATE TABLE __PBPS_STATE (x)")
        .expect("ddl");
    assert!(is_initialized(c).expect("asked"));
    ensure_tables(c).expect("nothing to create");
    assert_eq!(
        number(
            c,
            "SELECT count(*) FROM sqlite_schema WHERE name = '__pbps_state' COLLATE NOCASE"
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
    // Negative: an ordinary table of the name is the ledger.
    let mut fresh = Fresh::memory();
    fresh
        .connection()
        .execute_batch("CREATE TABLE __pbps_state (x)")
        .expect("ddl");
    ensure_tables(fresh.connection()).expect("a table is accepted");
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
