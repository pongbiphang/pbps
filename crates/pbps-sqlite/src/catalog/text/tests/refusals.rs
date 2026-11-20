//! What the tokenizer refuses, by name. A refusal says the object and what
//! was not understood, escaped; a text that is valid SQLite and outside the
//! subset is pinned as such, so the line between "refused" and "read" is a
//! decision a test holds and not an accident of the code.

use super::super::table;
use super::corpus::{self, Case};

/// (text, a fragment of the sentence, whether the engine takes the text).
const TABLES: &[(&str, &str, bool)] = &[
    ("CREATE TABLE t AS SELECT 1", "column list", true),
    ("CREATE TEMP TABLE t(a)", "temporary", true),
    ("CREATE TABLE main.t(a)", "schema-qualified", true),
    ("CREATE TABLE t(a) FOO", "table option", false),
    (
        "CREATE TABLE t(a) STRICT WITHOUT ROWID",
        "text after",
        false,
    ),
    ("CREATE TABLE t(a) WITHOUT", "expected `rowid`", false),
    ("CREATE TABLE t()", "a column's name", false),
    (
        "CREATE TABLE t(a int, PRIMARY KEY (lower(a)))",
        "key column",
        false,
    ),
    (
        "CREATE TABLE t(a int DEFAULT abc)",
        "default that is not one of the forms",
        true,
    ),
    (
        "CREATE TABLE t(a int DEFAULT \"dq\")",
        "default that is not one of the forms",
        true,
    ),
    (
        "CREATE TABLE t(a int DEFAULT -x'ab')",
        "sign that is not followed by a number",
        true,
    ),
    ("CREATE TABLE t(a int DEFAULT 1 + 1)", "constraint", false),
    (
        "CREATE TABLE t(a blob DEFAULT x 'ab')",
        "default that is not one of the forms",
        false,
    ),
    (
        "CREATE TABLE t(a int REFERENCES p ON DELETE 'x\ny')",
        "foreign key action",
        false,
    ),
    ("CREATE TABLE t(a int DEFAULT)", "default", false),
    (
        "CREATE TABLE t(a int CONSTRAINT)",
        "constraint's name",
        false,
    ),
    (
        "CREATE TABLE t(a int CONSTRAINT n)",
        "no constraint after it",
        true,
    ),
    (
        "CREATE TABLE t(a int CONSTRAINT n CONSTRAINT m NOT NULL)",
        "two constraint names",
        true,
    ),
    (
        "CREATE TABLE t(a int CONSTRAINT n COLLATE nocase)",
        "before `COLLATE`",
        true,
    ),
    ("CREATE TABLE t(a int", "never closed", false),
    ("CREATE TABLE t(a \"x", "never closes", false),
    ("CREATE TABLE t(a int /* x", "never closes", false),
    (
        "CREATE TABLE t(a int); DROP TABLE t",
        "another statement",
        true,
    ),
    ("CREATE TABLE t(a int CHECK ())", "nothing in it", false),
    ("CREATE TABLE t(a int CHECK (a > 0)", "never closed", false),
    ("CREATE TABLE t(a int(x))", "type argument", false),
    ("CREATE TABLE t(a select)", "where a type word", false),
    (
        "CREATE TABLE t(a int FOREIGN KEY)",
        "where a type word or a constraint was expected",
        false,
    ),
    (
        "CREATE TABLE t(a int REFERENCES p ON DELETE FOO)",
        "foreign key action",
        false,
    ),
    (
        "CREATE TABLE t(a int REFERENCES p ON FOO)",
        "`ON` that is not",
        false,
    ),
    (
        "CREATE TABLE t(a int REFERENCES p ON DELETE CASCADE ON DELETE RESTRICT)",
        "given twice",
        true,
    ),
    (
        "CREATE TABLE t(a int NOT DEFERRABLE)",
        "expected `null`",
        true,
    ),
    (
        "CREATE TABLE t(a int NOT NULL ON CONFLICT FOO)",
        "conflict action",
        false,
    ),
    (
        "CREATE TABLE t(a int, CONSTRAINT k)",
        "not followed by a constraint",
        true,
    ),
    (
        "CREATE TABLE t(a int, UNIQUE (a), b int)",
        "after a table constraint",
        false,
    ),
    (
        "CREATE TABLE t(UNIQUE (a), a int)",
        "before any column",
        false,
    ),
    (
        "CREATE TABLE t(a int, FOREIGN KEY (a) p)",
        "expected `references`",
        false,
    ),
    (
        "CREATE TABLE t(a int, FOREIGN KEY a REFERENCES p)",
        "expected `(`",
        false,
    ),
    (
        "CREATE TABLE t(a int REFERENCES main.p(id))",
        "schema-qualified parent",
        false,
    ),
    ("", "expected `create`", true),
    ("CREATE", "expected `table`", false),
    ("SELECT 1", "expected `create`", true),
    ("CREATE VIEW v AS SELECT 1", "expected `table`", true),
    ("CREATE TABLE t(a int AS 1)", "expected `(`", false),
    (
        "CREATE TABLE t(a int, b int) STRICT,",
        "table option",
        false,
    ),
    (
        "CREATE TABLE t(a int) WITHOUT ROWID STRICT",
        "text after",
        false,
    ),
    (
        "CREATE TABLE t(a int PRIMARY KEY AUTOINCREMENT DESC)",
        "constraint",
        false,
    ),
];

#[test]
fn text_outside_the_subset_is_refused_by_object_and_cause() {
    let mut wrong: Vec<String> = Vec::new();
    for (sql, fragment, takes) in TABLES {
        let case = Case {
            setup: corpus::PARENTS,
            sql: (*sql).to_owned(),
        };
        if corpus::takes(&case) != *takes {
            wrong.push(format!("{sql}: the engine takes it is not {takes}"));
        }
        let refused = table("t", sql).expect_err(sql);
        let said = refused.what();
        if !said.contains(fragment) {
            wrong.push(format!("{sql}: {said}"));
        }
        assert!(said.starts_with("`t`: "), "{sql}: {said}");
        assert_eq!(refused.to_string(), said);
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[test]
fn an_object_name_is_escaped_in_what_is_said_about_it() {
    let refused = table("a\nb`\u{7}", "CREATE TABLE t AS SELECT 1").expect_err("refused");
    assert!(!refused.what().chars().any(char::is_control), "{refused}");
    assert!(refused.what().contains("a\\nb"), "{refused}");
    // A text the lexer refuses is escaped the same way.
    let refused = table("a\nb", "CREATE TABLE t(a \"x").expect_err("refused");
    assert!(!refused.what().chars().any(char::is_control), "{refused}");
    assert!(refused.what().contains("a\\nb"), "{refused}");
    // The text near the cause is escaped too.
    let refused =
        table("t", "CREATE TABLE t(a int REFERENCES p ON DELETE 'x\ny')").expect_err("refused");
    assert!(!refused.what().chars().any(char::is_control), "{refused}");
    assert!(refused.what().contains("x\\ny"), "{refused}");
    let refused = table("t", "CREATE TABLE t(a int DEFAULT 'a\nb' + 1 `x`)").expect_err("refused");
    assert!(!refused.what().chars().any(char::is_control), "{refused}");
}

#[test]
fn a_refusal_is_never_an_empty_read_of_text_that_holds_something() {
    // Valid texts the subset does not hold are `Err`, not a table with fewer
    // columns: a read that shortened a table would be silent.
    for sql in [
        "CREATE TABLE t(a int DEFAULT abc, b int)",
        "CREATE TABLE t(a int CONSTRAINT n COLLATE nocase, b int)",
    ] {
        assert!(table("t", sql).is_err(), "{sql}");
    }
}
