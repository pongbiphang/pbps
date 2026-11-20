//! The reserved word for a column with no declared type, through the emitter
//! (#2950). `definition` is the one place a type's text enters SQL, and for the
//! word it writes none: written, the word would be a NUMERIC column. Each test
//! runs the emitted statements on a real SQLite and compares the catalog with a
//! table written by hand, in both directions.

use pbps_model::{ColumnChange, ColumnRef};

use super::golden::{WithDefault, add_column, create_table_change, uid};
use super::*;
use crate::lexis;
use crate::types::{self, read_back};

fn statements_of(change: Change) -> Vec<String> {
    statements(&Sqlite::default(), change)
        .expect("emits")
        .into_iter()
        .map(|s| s.sql)
        .collect()
}

#[test]
fn create_table_writes_a_column_with_no_type_as_no_type() {
    let t = table(&[
        ("a", col("untyped")),
        ("b", col("text")),
        ("c", col("untyped").not_null().with_default("0")),
        ("d", col("UNTYPED")),
    ]);
    let sql = statements_of(create_table_change("t", &t));
    assert_eq!(
        sql[0],
        "CREATE TABLE main.\"t\" (\n    \"a\",\n    \"b\" text,\n    \"c\" NOT NULL DEFAULT 0,\n    \"d\"\n);"
    );
    let out = statements(&Sqlite::default(), create_table_change("t", &t)).expect("emits");
    let mut made = Fresh::memory();
    run(made.connection(), &out).expect("runs");
    let mut by_hand = built("CREATE TABLE t(a, b text, c NOT NULL DEFAULT 0, d)");
    assert_same(&catalog_of(&mut made), &catalog_of(&mut by_hand), "no type");
    // The negative: a column declared with the word is another table.
    let mut written =
        built("CREATE TABLE t(a untyped, b text, c untyped NOT NULL DEFAULT 0, d untyped)");
    assert_ne!(catalog_of(&mut made), catalog_of(&mut written));
}

#[test]
fn the_emitted_column_behaves_as_a_column_with_no_type() {
    let t = table(&[("a", col("untyped"))]);
    let out = statements(&Sqlite::default(), create_table_change("t", &t)).expect("emits");
    let mut fresh = Fresh::memory();
    run(fresh.connection(), &out).expect("runs");
    let c = fresh.connection();
    c.execute_batch("INSERT INTO t VALUES ('1'), (1), ('007')")
        .expect("inserts");
    let rows = c
        .query("SELECT typeof(a) FROM t ORDER BY rowid", &[])
        .expect("classes");
    let classes: Vec<&str> = rows.iter().map(|r| r.text(0).expect("class")).collect();
    assert_eq!(
        classes,
        ["text", "integer", "text"],
        "text stays text: no conversion"
    );
}

#[test]
fn add_column_writes_a_column_with_no_type_as_no_type() {
    for (what, column, golden, hand) in [
        (
            "nullable",
            col("untyped"),
            "ALTER TABLE main.\"t\" ADD COLUMN \"x\";",
            "x",
        ),
        (
            "not null with a default",
            col("untyped").not_null().with_default("0"),
            "ALTER TABLE main.\"t\" ADD COLUMN \"x\" NOT NULL DEFAULT 0;",
            "x NOT NULL DEFAULT 0",
        ),
        (
            "a default",
            col("untyped").with_default("'d'"),
            "ALTER TABLE main.\"t\" ADD COLUMN \"x\" DEFAULT 'd';",
            "x DEFAULT 'd'",
        ),
    ] {
        let out = statements(&Sqlite::default(), add_column("t", "x", column)).expect(what);
        assert_eq!(out[0].sql, golden, "{what}");
        let mut db = built("CREATE TABLE t(a integer); INSERT INTO t VALUES (1)");
        run(db.connection(), &out).unwrap_or_else(|e| panic!("{what}: {e}"));
        let mut direct = built(
            &[
                "CREATE TABLE t(a integer, ",
                hand,
                "); INSERT INTO t(a) VALUES (1)",
            ]
            .concat(),
        );
        assert_same(&catalog_of(&mut db), &catalog_of(&mut direct), what);
    }
    // The rule for `ADD COLUMN` is the same for a column with no type.
    let refused = statements(
        &Sqlite::default(),
        add_column("t", "x", col("untyped").not_null()),
    )
    .expect_err("a NOT NULL column without a default");
    assert!(
        refused.to_string().contains("Give the column a default"),
        "{refused}"
    );
}

#[test]
fn the_word_is_never_written_as_a_type_and_an_ordinary_type_that_holds_it_is() {
    let reserved = table(&[
        ("untyped", col("untyped")),
        ("d", col("text").with_default("'untyped'")),
        ("e", col("untyped")),
    ]);
    let mut out =
        statements(&Sqlite::default(), create_table_change("t", &reserved)).expect("emits");
    out.extend(
        statements(&Sqlite::default(), add_column("t", "f", col("untyped"))).expect("emits"),
    );
    for statement in &out {
        // A column named for it is quoted and a string is a string; neither
        // is the word as code.
        assert!(
            !lexis::has_bare_word(&statement.sql, "untyped"),
            "{}",
            statement.sql
        );
    }
    assert!(
        out[0].sql.contains("\"untyped\","),
        "a column may be named for it: {}",
        out[0].sql
    );
    // The ordinary types are written as they are.
    let ordinary = table(&[("a", col("untyped(5)")), ("b", col("untyped int"))]);
    let sql = statements_of(create_table_change("t", &ordinary));
    assert!(
        sql[0].contains("\"a\" untyped(5)") && sql[0].contains("\"b\" untyped int"),
        "{}",
        sql[0]
    );
}

#[test]
fn a_type_change_into_or_out_of_the_word_is_the_rebuilds_and_never_a_statement() {
    let change = |from: &str, to: &str| {
        Change::Column(ColumnChange::AlterColumnType {
            uid: uid(UidKind::Column, "t.a"),
            column: ColumnRef {
                table: tn("t"),
                name: "a".into(),
            },
            from: ty(from),
            to: ty(to),
            from_nullable: true,
            to_nullable: true,
            from_collation: None,
            to_collation: None,
            through_external_partitions: false,
            using: None,
        })
    };
    for (from, to) in [
        ("blob", "untyped"),
        ("untyped", "text"),
        ("untyped", "blob"),
    ] {
        let refused = statements(&Sqlite::default(), change(from, to)).expect_err("the rebuild's");
        assert!(
            refused.to_string().contains("table rebuild"),
            "{from} to {to}: {refused}"
        );
    }
}

#[test]
fn the_differ_adds_a_column_with_no_type_and_converges() {
    let (base, declared) = (
        schema(&[("t", table(&[("a", col("integer"))]))], &[]),
        schema(
            &[("t", table(&[("a", col("integer")), ("b", col("untyped"))]))],
            &[],
        ),
    );
    let (bi, di) = (ids(&base, &[]), ids(&declared, &[]));
    let out = plan((&base, &bi), (&declared, &di), &Sqlite::default()).expect("plans");
    assert_eq!(
        sql_text(&out),
        vec!["ALTER TABLE main.\"t\" ADD COLUMN \"b\";"]
    );
    let mut db = built("CREATE TABLE t(a integer)");
    run(db.connection(), &out).expect("runs");
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut built("CREATE TABLE t(a integer, b)")),
        "converges",
    );
}

#[test]
fn what_is_emitted_for_a_column_reads_back_as_the_declared_type() {
    // Declared, emitted, run, read from the catalog and read back: the type
    // comes out as it went in, for the word and for every ordinary type.
    for declared in [
        "untyped",
        "integer",
        "varchar(10)",
        "text",
        "untyped(5)",
        "untyped int",
        "blob",
    ] {
        let t = table(&[("a", col(declared))]);
        let out = statements(&Sqlite::default(), create_table_change("t", &t)).expect("emits");
        let mut fresh = Fresh::memory();
        run(fresh.connection(), &out).expect("runs");
        let rows = fresh
            .connection()
            .query("SELECT type FROM pragma_table_xinfo('t')", &[])
            .expect("type");
        let read = read_back(rows[0].text(0).expect("type")).expect(declared);
        assert_eq!(
            read,
            types::normalize(&ty(declared)).expect(declared),
            "{declared}"
        );
    }
}
