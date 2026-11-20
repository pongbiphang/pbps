//! A column's type, and the other places the pragmas speak in a way that
//! needs the text to be read right (#2950, item 4).
//!
//! `pragma_table_xinfo` reports the type `''` for a column with no type and for
//! one declared with a quoted empty name (`a ""`) alike, and the two are not the
//! same column: the empty name has NUMERIC affinity and no type has BLOB. Only
//! the text tells them apart, so `agree` asks it.

use super::super::table;
use super::facts::{agree, facts, stored};
use crate::catalog::facts::read;
use crate::tests::Fresh;

/// The stored text, the pragma facts and the engine, for one `CREATE TABLE t`.
fn made(sql: &str) -> (Fresh, String) {
    let mut fresh = Fresh::memory();
    fresh
        .connection()
        .execute_batch(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let text = stored(fresh.connection(), "table")
        .into_iter()
        .find(|(n, _)| n == "t")
        .expect("t")
        .1;
    (fresh, text)
}

/// What the engine does with `'1'` put in column `a`: a NUMERIC column makes
/// it an integer and a BLOB one keeps the text.
fn kept_as(declaration: &str) -> String {
    let (mut fresh, _) = made(&format!("CREATE TABLE t({declaration})"));
    let c = fresh.connection();
    c.execute_batch("INSERT INTO t(a) VALUES ('1')")
        .expect("insert");
    let rows = c.query("SELECT typeof(a) FROM t", &[]).expect("typeof");
    rows[0].text(0).expect("text").to_owned()
}

const EMPTY_NAMES: [&str; 4] = ["a \"\"", "a ''", "a []", "a ``"];

#[test]
fn the_engine_reports_the_empty_type_for_no_type_and_for_a_quoted_empty_one() {
    for declaration in ["a", "a NOT NULL", "a AS (1)"].iter().chain(&EMPTY_NAMES) {
        let (mut fresh, _) = made(&format!("CREATE TABLE t(z int, {declaration})"));
        assert_eq!(
            facts(fresh.connection(), "t").columns[1].ty,
            "",
            "{declaration}"
        );
    }
    // They are different columns: this is why the text is asked.
    assert_eq!(kept_as("a"), "text", "no type is BLOB affinity");
    for declaration in EMPTY_NAMES {
        assert_eq!(kept_as(declaration), "integer", "{declaration} is NUMERIC");
    }
    // A blank quoted name is reported as it is, and is NUMERIC too.
    let (mut fresh, _) = made("CREATE TABLE t(a \" \")");
    assert_eq!(facts(fresh.connection(), "t").columns[0].ty, " ");
    assert_eq!(kept_as("a \" \""), "integer");
}

#[test]
fn a_column_has_type_words_in_the_text_exactly_when_it_has_a_type_word() {
    for (sql, typed) in [
        (
            "CREATE TABLE t(a, b int, c \"\" NOT NULL, d NOT NULL, e AS (1))",
            vec![false, true, true, false, false],
        ),
        (
            "CREATE TABLE t(a varchar(10), b double precision, c 'x y' DEFAULT 1)",
            vec![true, true, true],
        ),
        (
            "CREATE TABLE t(a PRIMARY KEY, b CHECK (b > 0), c REFERENCES p(id))",
            vec![false, false, false],
        ),
    ] {
        let (mut fresh, text) = made(&format!("CREATE TABLE p(id integer PRIMARY KEY); {sql}"));
        let read = table("t", &text).expect("reads");
        assert_eq!(read.typed, typed, "{sql}");
        assert_eq!(read.typed.len(), read.columns.len());
        let _ = fresh.connection();
    }
}

#[test]
fn a_quoted_empty_type_is_a_disagreement_and_no_type_and_a_real_type_are_not() {
    for declaration in EMPTY_NAMES {
        let (mut fresh, text) = made(&format!("CREATE TABLE t(b int, {declaration})"));
        let why = agree(
            &table("t", &text).expect("reads"),
            &facts(fresh.connection(), "t"),
        )
        .expect_err(declaration);
        assert!(
            why.contains("column `a`") && why.contains("type words"),
            "{why}"
        );
    }
    // Negative: no type, a type, and a blank quoted name, each in agreement
    // (the blank one is refused later, by the type reader, and not here).
    for declaration in ["a", "a NOT NULL", "a AS (1)", "a int", "a \" \""] {
        let (mut fresh, text) = made(&format!("CREATE TABLE t(b int, {declaration})"));
        agree(
            &table("t", &text).expect("reads"),
            &facts(fresh.connection(), "t"),
        )
        .unwrap_or_else(|e| panic!("{declaration}: {e}"));
    }
}

#[test]
fn a_type_disagreement_is_seen_in_each_direction() {
    let (mut fresh, text) = made("CREATE TABLE t(a int, b)");
    let found = facts(fresh.connection(), "t");
    let good = table("t", &text).expect("reads");
    agree(&good, &found).expect("agrees");
    // The text says a type the pragma does not report.
    let mut bad = good.clone();
    bad.typed[1] = true;
    assert!(agree(&bad, &found).unwrap_err().contains("column `b`"));
    // The pragma reports a type the text does not say.
    let mut bad = good;
    bad.typed[0] = false;
    assert!(agree(&bad, &found).unwrap_err().contains("column `a`"));
}

#[test]
fn a_null_is_read_as_absent_and_text_as_text() {
    let (mut fresh, _) = made(
        "CREATE TABLE p(id integer PRIMARY KEY, k integer UNIQUE); \
         CREATE TABLE t(a integer, b DEFAULT NULL, c DEFAULT '', d DEFAULT (1+1), \
         e REFERENCES p, f REFERENCES p(k)); \
         CREATE INDEX i ON t(a, (a + 1))",
    );
    let found = read(fresh.connection(), "t")
        .expect("reads")
        .expect("the table");
    let defaults: Vec<Option<&str>> = found.columns.iter().map(|c| c.default.as_deref()).collect();
    // No default is absent. `DEFAULT NULL` and `DEFAULT ''` are text, and the
    // empty string is not an absent default.
    assert_eq!(
        defaults,
        [None, Some("NULL"), Some("''"), Some("1+1"), None, None]
    );
    // A reference that names no column has no `to`; one that names it has.
    let to: Vec<Option<&str>> = found.foreign.iter().map(|f| f.to.as_deref()).collect();
    assert_eq!(to, [None, Some("k")]);
    // An expression key has no name; a column key has one.
    let keys: Vec<Option<&str>> = found.indexes[0]
        .keys
        .iter()
        .map(|k| k.0.as_deref())
        .collect();
    assert_eq!(keys, [Some("a"), None]);
    // A table that is not there is absent, which is neither an error nor a
    // table with nothing in it.
    assert_eq!(read(fresh.connection(), "missing").expect("reads"), None);
}

/// SQLite finds a table under any ASCII case of its name, and so must the
/// flags: a `STRICT` table asked for as `T` is still strict, or its key
/// column's implied `NOT NULL` would read as a disagreement.
#[test]
fn a_table_asked_for_in_another_case_keeps_its_flags_and_agrees() {
    let mut fresh = Fresh::memory();
    let sql = "CREATE TABLE t(a INT PRIMARY KEY) STRICT";
    fresh.connection().execute_batch(sql).expect("creates");
    for name in ["t", "T"] {
        let found = read(fresh.connection(), name)
            .expect("reads")
            .expect("the table");
        assert!(found.strict, "{name}");
        let text = crate::catalog::text::table("t", sql).expect("tokenizes");
        agree(&text, &found).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    // Negative: a rowid table is not strict under either spelling.
    let mut plain = Fresh::memory();
    plain
        .connection()
        .execute_batch("CREATE TABLE t(a INT PRIMARY KEY)")
        .expect("creates");
    assert!(
        !read(plain.connection(), "T")
            .expect("reads")
            .expect("the table")
            .strict
    );
}
