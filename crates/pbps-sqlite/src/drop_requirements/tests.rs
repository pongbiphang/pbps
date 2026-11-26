//! A guarded drop's row requirement, against a real SQLite.

use std::collections::BTreeMap;

use pbps_db::sqlite::Connection;
use pbps_model::{DropGuard, DropRequirement, UidKind};

use super::{check, vet};
use crate::emit::tests::golden::uid;
use crate::emit::tests::tn;
use crate::tests::Fresh;
use pbps_db::sqlite::OpenMode;
use pbps_model::{ColumnName, TableName};

fn guard(predicates: &[&str]) -> DropGuard {
    DropGuard {
        table_uid: uid(UidKind::Table, "t"),
        table: tn("t"),
        detach_from: None,
        requirements: predicates
            .iter()
            .enumerate()
            .map(|(n, predicate)| {
                (
                    uid(UidKind::Column, &format!("t.{n}")),
                    DropRequirement {
                        table_uid: uid(UidKind::Table, "t"),
                        predicate: (*predicate).to_owned(),
                        required_at: "2026-11-20".into(),
                        operator: "reviewer".into(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>(),
    }
}

fn with_rows(rows: &str) -> Fresh {
    let mut fresh = Fresh::memory();
    fresh
        .connection()
        .execute_batch(&format!("CREATE TABLE t(a integer, b text); {rows}"))
        .expect("rows");
    fresh
}

fn tables(c: &Connection) -> i64 {
    c.query("SELECT count(*) FROM sqlite_schema WHERE name = 't'", &[])
        .expect("query")[0]
        .integer(0)
        .expect("count")
}

#[test]
fn a_requirement_every_row_meets_permits_and_an_empty_table_permits() {
    let mut fresh = with_rows("INSERT INTO t VALUES (1, 'x'), (2, 'y')");
    assert_eq!(
        check(fresh.connection(), &guard(&["a > 0", "b <> ''"])),
        Ok(())
    );
    let mut empty = with_rows("");
    assert_eq!(check(empty.connection(), &guard(&["a > 0"])), Ok(()));
    // No requirement is nothing to check.
    assert_eq!(check(empty.connection(), &guard(&[])), Ok(()));
}

#[test]
fn a_violated_requirement_names_the_count_and_the_predicate_and_removes_nothing() {
    let mut fresh = with_rows("INSERT INTO t VALUES (-1, 'x'), (-2, 'y'), (3, 'z')");
    let refused = check(fresh.connection(), &guard(&["a > 0"])).expect_err("violated");
    assert!(
        refused.contains("2 row(s)") && refused.contains("`a > 0`"),
        "{refused}"
    );
    assert!(refused.contains("nothing was removed"), "{refused}");
    assert_eq!(tables(fresh.connection()), 1);
    // The second of two requirements is the one that fails.
    let second = check(fresh.connection(), &guard(&["a > -5", "a > 0"])).expect_err("second");
    assert!(second.contains("`a > 0`"), "{second}");
}

#[test]
fn a_column_named_like_a_boolean_word_cannot_stand_in_for_the_test() {
    // `TRUE` is a name before it is a constant: with a column of that name
    // holding 0, `(a > 0) IS NOT TRUE` compared the answer with the column and
    // counted nothing.
    for word in ["true", "TRUE", "false"] {
        let mut fresh = Fresh::memory();
        fresh
            .connection()
            .execute_batch(&format!(
                "CREATE TABLE t(a integer, \"{word}\"); INSERT INTO t VALUES (0, 0), (NULL, NULL)"
            ))
            .expect("rows");
        let refused = check(fresh.connection(), &guard(&["a > 0"])).expect_err("neither is true");
        assert!(refused.contains("2 row(s)"), "{word}: {refused}");
        // Negative: with every row meeting it, the same table permits.
        fresh
            .connection()
            .execute_batch("UPDATE t SET a = 1")
            .expect("rows");
        assert!(
            check(fresh.connection(), &guard(&["a > 0"])).is_ok(),
            "{word}"
        );
    }
}

#[test]
fn a_null_answer_is_a_violation_as_is_not_true_makes_it_on_the_other_engines() {
    let mut fresh = with_rows("INSERT INTO t VALUES (NULL, 'x'), (1, 'y')");
    let refused = check(fresh.connection(), &guard(&["a > 0"])).expect_err("a null is not true");
    assert!(refused.contains("1 row(s)"), "{refused}");
    // Negative: a row whose answer is true is not counted.
    let mut fine = with_rows("INSERT INTO t VALUES (1, 'x')");
    assert!(check(fine.connection(), &guard(&["a > 0"])).is_ok());
}

#[test]
fn a_predicate_the_engine_cannot_evaluate_never_permits_the_removal() {
    let mut fresh = with_rows("INSERT INTO t VALUES (1, 'x')");
    for predicate in ["missing > 0", "a >", "nosuchfn(a) > 0"] {
        let refused = check(fresh.connection(), &guard(&[predicate])).expect_err(predicate);
        assert!(
            refused.contains("could not be evaluated"),
            "{predicate}: {refused}"
        );
        assert!(
            refused.contains("never permits removal"),
            "{predicate}: {refused}"
        );
    }
    // A table that is not there is the same.
    let mut gone = Fresh::memory();
    assert!(check(gone.connection(), &guard(&["1 = 1"])).is_err());
}

#[test]
fn a_predicate_cannot_carry_a_second_statement_and_a_trailing_comment_does_not_swallow_the_rest() {
    let mut fresh = with_rows("INSERT INTO t VALUES (1, 'x')");
    let refused = check(
        fresh.connection(),
        &guard(&["a > 0); DROP TABLE t; SELECT (1"]),
    )
    .expect_err("one statement only");
    assert!(refused.contains("not one expression"), "{refused}");
    assert_eq!(tables(fresh.connection()), 1, "the table is still there");
    // Negative: a comment at the end is closed, and the requirement still counts.
    assert!(check(fresh.connection(), &guard(&["a > 0 -- positive"])).is_ok());
    assert!(check(fresh.connection(), &guard(&["a < 0 -- negative"])).is_err());
}

#[test]
fn the_check_runs_inside_the_applys_transaction_and_leaves_it_open() {
    let mut fresh = with_rows("INSERT INTO t VALUES (1, 'x')");
    let c = fresh.connection();
    c.execute_batch("BEGIN IMMEDIATE; INSERT INTO t VALUES (-1, 'z')")
        .expect("the apply's own work");
    // It sees the transaction's own rows, which is what the drop will remove.
    assert!(check(c, &guard(&["a > 0"])).is_err());
    assert!(c.in_transaction());
    c.execute_batch("ROLLBACK").expect("ended");
    assert!(check(c, &guard(&["a > 0"])).is_ok());
}

/// The count is only as good as the wrapper: a predicate that ends it early or
/// leaves a comment open would be counted as something else.
#[test]
fn a_predicate_that_escapes_the_wrapper_is_refused_and_never_counted() {
    let mut fresh = with_rows("INSERT INTO t VALUES (1, 'x')");
    for predicate in ["0) /*", "0) OR (1", "a > 0 /* open", "'open"] {
        let refused = check(fresh.connection(), &guard(&[predicate])).expect_err(predicate);
        assert!(
            refused.contains("not one expression"),
            "{predicate}: {refused}"
        );
    }
    // Negative: the same meanings written whole are counted.
    assert!(check(fresh.connection(), &guard(&["(0) OR (1)"])).is_ok());
    assert!(check(fresh.connection(), &guard(&["a > 0 /* closed */"])).is_ok());
    assert!(check(fresh.connection(), &guard(&["(0)"])).is_err());
}

fn cols(names: &[&str]) -> std::collections::BTreeSet<ColumnName> {
    names.iter().map(|n| ColumnName::new(*n)).collect()
}

#[test]
fn vet_binds_allowed_operators_and_returns_canonical_catalog_column_names() {
    let mut fresh = Fresh::file();
    fresh
        .connection()
        .execute_batch(
            "CREATE TABLE MixedTbl (
                Id INTEGER PRIMARY KEY,
                Amount INTEGER,
                Status TEXT,
                \"a \"\"b\"\"\" INTEGER
             );
             INSERT INTO MixedTbl VALUES (1, 10, 'open', 2);",
        )
        .expect("setup");

    let t = tn("MixedTbl");
    assert_eq!(
        vet(fresh.connection(), &t, "amount > 0").unwrap(),
        cols(&["Amount"])
    );
    assert_eq!(
        vet(
            fresh.connection(),
            &t,
            "\"AMOUNT\" > 0 AND `status` <> '' AND [a \"b\"] = 2"
        )
        .unwrap(),
        cols(&["Amount", "Status", "a \"b\""])
    );
    assert_eq!(
        vet(
            fresh.connection(),
            &t,
            "status LIKE 'o%' ESCAPE '\\' AND status GLOB 'o*' AND (status || '!') = 'open!'"
        )
        .unwrap(),
        cols(&["Status"])
    );
    assert_eq!(
        vet(
            fresh.connection(),
            &t,
            "CASE WHEN amount BETWEEN 1 AND 100 AND amount IN (10, 20) THEN 1 ELSE 0 END = 1 \
             AND status IS NOT NULL AND CAST(amount AS TEXT) COLLATE NOCASE = '10' -- ok"
        )
        .unwrap(),
        cols(&["Amount", "Status"])
    );
    // Constant predicate reads no column.
    assert_eq!(vet(fresh.connection(), &t, "1 = 1").unwrap(), cols(&[]));
    // Table lookup folds ASCII case and still returns stored column spellings.
    assert_eq!(
        vet(fresh.connection(), &tn("mixedtbl"), "amount > 0").unwrap(),
        cols(&["Amount"])
    );
    // Negative: a double-quoted typo fails with SQLite's code rather than reading as a string.
    let typo = vet(fresh.connection(), &t, "\"nope\" = 'x'").expect_err("dqs typo");
    assert!(typo.contains("SQLite code 1"), "{typo}");
}

#[test]
fn vet_refuses_subqueries_other_tables_functions_parameters_and_bad_expressions() {
    let mut fresh = Fresh::file();
    fresh
        .connection()
        .execute_batch(
            "CREATE TABLE t (a INTEGER, b TEXT);
             CREATE TABLE other (x INTEGER);
             ATTACH ':memory:' AS aux;
             CREATE TABLE aux.u (z INTEGER);",
        )
        .expect("setup");
    let t = tn("t");

    for (pred, expected) in [
        ("a IN (SELECT x FROM other)", "another table"),
        (
            "EXISTS (SELECT 1 FROM other WHERE other.x = a)",
            "another table",
        ),
        ("a IN (SELECT z FROM aux.u)", "another database"),
        ("(SELECT 1) = 1", "subquery"),
        ("a IN (SELECT a FROM t)", "subquery"),
        ("random() > 0", "calls function `random`"),
        ("date('now') > b", "calls function `date`"),
        ("strftime('%s', b) > '0'", "calls function `strftime`"),
        ("count(a) > 0", "calls function `count`"),
        ("max(a) > 0", "calls function `max`"),
        ("max(a, 1) > 0", "calls function `max`"),
        ("json(b) IS NOT NULL", "calls function `json`"),
        ("hex(b) = ''", "calls function `hex`"),
        (
            "load_extension('x') IS NULL",
            "calls function `load_extension`",
        ),
        (
            "b = CURRENT_TIMESTAMP",
            "calls function `current_timestamp`",
        ),
        ("b -> '$.x' = 1", "calls function `->`"),
        ("a = ?", "bound parameter"),
        ("a = :x", "bound parameter"),
        ("a = ?1", "bound parameter"),
        ("nope > 0", "SQLite code 1"),
        ("\"nope\" = 'x'", "SQLite code 1"),
        ("b COLLATE no_such_coll = 'x'", "SQLite code 257"),
        ("", "is blank"),
        ("   -- comment", "is blank"),
        ("0) OR (1", "not one expression"),
        ("a > 0; DROP TABLE t", "not one expression"),
        ("a > 0 /* open", "not one expression"),
    ] {
        let refused = vet(fresh.connection(), &t, pred).expect_err(pred);
        assert!(refused.contains(expected), "{pred}: {refused}");
    }
    // Negative control: authorizer and DQS are restored, so ordinary statements still work.
    assert!(vet(fresh.connection(), &t, "a > 0 AND b LIKE 'x%'").is_ok());
    fresh
        .connection()
        .execute_batch("INSERT INTO t VALUES (1, 'x')")
        .expect("connection usable after refusals");
}

#[test]
fn vet_refuses_generated_columns_and_implicit_rowid_but_allows_declared_columns() {
    let mut fresh = Fresh::file();
    fresh
        .connection()
        .execute_batch(
            "CREATE TABLE t (
                a INTEGER,
                g_virt INTEGER GENERATED ALWAYS AS (a + 1) VIRTUAL,
                g_stored INTEGER GENERATED ALWAYS AS (a * 2) STORED
             );
             CREATE TABLE pk_alias (id INTEGER PRIMARY KEY, val TEXT);
             CREATE TABLE has_rowid (rowid INTEGER, val TEXT);
             CREATE TABLE no_rowid (k INTEGER PRIMARY KEY, val TEXT) WITHOUT ROWID;",
        )
        .expect("setup");

    // Unreferenced generated columns do not block ordinary predicates.
    assert_eq!(
        vet(fresh.connection(), &tn("t"), "a > 0").unwrap(),
        cols(&["a"])
    );

    // Generated columns and implicit rowid aliases are refused.
    for (pred, expected) in [
        ("g_virt > 0", "virtual generated column `g_virt`"),
        ("g_stored > 0", "stored generated column `g_stored`"),
        ("rowid > 0", "implicit column `ROWID`"),
        ("_rowid_ > 0", "implicit column `ROWID`"),
        ("oid > 0", "implicit column `ROWID`"),
    ] {
        let refused = vet(fresh.connection(), &tn("t"), pred).expect_err(pred);
        assert!(refused.contains(expected), "{pred}: {refused}");
    }
    // On an INTEGER PRIMARY KEY table, SQLite aliases rowid to the declared PK column.
    assert_eq!(
        vet(fresh.connection(), &tn("pk_alias"), "rowid > 0").unwrap(),
        cols(&["id"])
    );
    // An explicitly declared column named `rowid` is an ordinary column.
    assert_eq!(
        vet(fresh.connection(), &tn("has_rowid"), "rowid > 0").unwrap(),
        cols(&["rowid"])
    );
    // On WITHOUT ROWID, SQLite itself refuses `rowid` as unknown column.
    let no_rowid = vet(fresh.connection(), &tn("no_rowid"), "rowid > 0").expect_err("no rowid");
    assert!(no_rowid.contains("SQLite code 1"), "{no_rowid}");
}

#[test]
fn vet_separates_absent_view_virtual_table_other_schema_and_unreadable_catalog() {
    let mut fresh = Fresh::file();
    fresh
        .connection()
        .execute_batch(
            "CREATE TABLE t (a INTEGER PRIMARY KEY AUTOINCREMENT);
             CREATE VIEW v AS SELECT a FROM t;
             CREATE VIRTUAL TABLE docs USING fts5(body);
             PRAGMA writable_schema = ON;
             INSERT INTO sqlite_schema (type, name, tbl_name, rootpage, sql)
             VALUES ('table', 'ghost_vtab', 'ghost_vtab', 0, 'CREATE VIRTUAL TABLE ghost_vtab USING no_such_module(a)');
             PRAGMA writable_schema = OFF;",
        )
        .expect("setup");
    fresh.reopen();

    // Absent table.
    let absent = vet(fresh.connection(), &tn("missing"), "1 = 1").expect_err("absent");
    assert!(absent.contains("does not exist"), "{absent}");
    // View.
    let view = vet(fresh.connection(), &tn("v"), "a > 0").expect_err("view");
    assert!(view.contains("view"), "{view}");
    // Virtual table with an unavailable module: refused by table_list without loading the module.
    let vtab = vet(fresh.connection(), &tn("ghost_vtab"), "a > 0").expect_err("vtab");
    assert!(
        vtab.contains("virtual table") && !vtab.contains("no such module"),
        "{vtab}"
    );
    // Shadow table of a virtual table.
    let shadow = vet(fresh.connection(), &tn("docs_content"), "1 = 1").expect_err("shadow");
    assert!(shadow.contains("shadow table"), "{shadow}");
    // Internal sqlite_* tables: `sqlite_sequence` is listed with kind `table` and refused by prefix;
    // `sqlite_schema` is not listed by single-name `PRAGMA table_list` and is refused too.
    let internal = vet(fresh.connection(), &tn("sqlite_sequence"), "1 = 1").expect_err("internal");
    assert!(internal.contains("internal SQLite table"), "{internal}");
    assert!(vet(fresh.connection(), &tn("sqlite_schema"), "1 = 1").is_err());
    // Non-main schema.
    let other_schema =
        vet(fresh.connection(), &TableName::new("temp", "t"), "a > 0").expect_err("other schema");
    assert!(other_schema.contains("main"), "{other_schema}");

    // Unreadable/busy catalog fails with SQLite busy code 5, never an empty column set.
    let locker = Connection::open(fresh.path(), OpenMode::ExistingOnly).expect("second connection");
    locker
        .execute_batch("BEGIN EXCLUSIVE; INSERT INTO t VALUES (1)")
        .expect("hold exclusive lock");
    let busy = vet(fresh.connection(), &tn("t"), "a > 0").expect_err("busy");
    assert!(busy.contains("SQLite code 5"), "{busy}");
    locker.execute_batch("ROLLBACK").expect("release");

    // Negative control: once unlocked, the same call succeeds.
    assert_eq!(
        vet(fresh.connection(), &tn("t"), "a > 0").unwrap(),
        cols(&["a"])
    );
}

#[test]
fn vet_does_not_read_rows_or_see_temp_table_shadowing_main() {
    let mut fresh = Fresh::file();
    fresh
        .connection()
        .execute_batch(
            "CREATE TABLE t (a INTEGER, b TEXT);
             INSERT INTO t VALUES (1, 'bad');
             CREATE TEMP TABLE t (z INTEGER);",
        )
        .expect("setup");
    // Resolves main.t, never temp.t.
    assert_eq!(
        vet(fresh.connection(), &tn("t"), "a > 0").unwrap(),
        cols(&["a"])
    );
    let temp_col = vet(fresh.connection(), &tn("t"), "z > 0").expect_err("temp col");
    assert!(temp_col.contains("SQLite code 1"), "{temp_col}");
}

#[test]
fn vet_takes_the_argument_only_scalar_functions_and_reports_the_columns_they_read() {
    let mut fresh = Fresh::file();
    fresh
        .connection()
        .execute_batch("CREATE TABLE t (a INTEGER, b TEXT, c REAL)")
        .expect("setup");
    let t = tn("t");
    for (pred, read) in [
        ("coalesce(a, 0) = 0", &["a"][..]),
        ("ifnull(a, 0) = 0", &["a"]),
        ("nullif(a, 0) IS NULL", &["a"]),
        ("iif(a > 0, b, 'x') = 'x'", &["a", "b"]),
        ("length(b) > 0", &["b"]),
        ("LENGTH(b) > 0", &["b"]),
        ("lower(b) = 'x' AND upper(b) = 'X'", &["b"]),
        (
            "trim(b) = '' OR ltrim(b, 'x') = '' OR rtrim(b) = ''",
            &["b"],
        ),
        ("substr(b, 1, 2) = 'ab' AND substring(b, 1) = 'ab'", &["b"]),
        ("instr(b, 'x') = 0", &["b"]),
        ("replace(b, 'x', 'y') = b", &["b"]),
        ("concat(b, a) = 'x1'", &["a", "b"]),
        (
            "abs(a) < 10 AND round(c, 1) = 1.5 AND round(c) = 2",
            &["a", "c"],
        ),
        ("typeof(a) = 'integer'", &["a"]),
        // Nested, and under an operator the authorizer reports as a function.
        ("lower(trim(b)) LIKE 'x%'", &["b"]),
    ] {
        assert_eq!(
            vet(fresh.connection(), &t, pred).unwrap_or_else(|e| panic!("{pred}: {e}")),
            cols(read),
            "{pred}"
        );
    }
    // Negative: a permitted function does not let in what it is given.
    for (pred, expected) in [
        ("length((SELECT 'x')) > 0", "subquery"),
        ("coalesce(a, random()) = 0", "calls function `random`"),
        ("length(nope) > 0", "SQLite code 1"),
        ("length(a, b) > 0", "SQLite code 1"),
        // The bundled library has no math functions, so these are not listed.
        ("floor(c) = 1", "SQLite code 1"),
        ("ceil(c) = 2", "SQLite code 1"),
        ("abs(?) = 1", "bound parameter"),
    ] {
        let refused = vet(fresh.connection(), &t, pred).expect_err(pred);
        assert!(refused.contains(expected), "{pred}: {refused}");
    }
}
