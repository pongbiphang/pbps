//! How the emitter names what it acts on, and what the stored text keeps
//! (#2950).

use super::golden::{WithDefault, add_column, create_table_change, index, uid};
use super::*;
use pbps_model::{ColumnChange, IndexChange, ObjectLocalName, TableChange};

/// The stored `CREATE` text of one object, in the comparison form.
fn stored(c: &Connection, name: &str) -> String {
    let rows = c
        .query(
            "SELECT sql FROM sqlite_schema WHERE name = ?1",
            &[Value::Text(name.to_owned())],
        )
        .expect("reads");
    canonical(rows[0].text(0).expect("text"))
}

#[test]
fn a_temp_table_of_the_same_name_is_never_the_one_acted_on() {
    // Measured: a bare `ALTER TABLE t` and `DROP TABLE t` act on a TEMP table
    // named `t` when one exists, so the emitter writes `main.` and these reach
    // the table in the file.
    let mut db = built("CREATE TABLE main.t(a integer); CREATE TEMP TABLE t(a integer)");
    let alter = statements(&Sqlite::default(), add_column("t", "x", col("text"))).expect("emits");
    run(db.connection(), &alter).expect("runs");
    let in_main = db
        .connection()
        .query("SELECT count(*) FROM pragma_table_info('t', 'main')", &[])
        .expect("reads");
    let in_temp = db
        .connection()
        .query("SELECT count(*) FROM pragma_table_info('t', 'temp')", &[])
        .expect("reads");
    assert_eq!(
        in_main[0].integer(0).expect("n"),
        2,
        "the column went to the table in `main`"
    );
    assert_eq!(
        in_temp[0].integer(0).expect("n"),
        1,
        "the temp table is untouched"
    );
    let drop = Change::Table(TableChange::DropTable {
        uid: uid(UidKind::Table, "t"),
        name: tn("t"),
        detach_from: None,
    });
    run(
        db.connection(),
        &statements(&Sqlite::default(), drop).expect("emits"),
    )
    .expect("runs");
    let left = db
        .connection()
        .query("SELECT schema FROM pragma_table_list WHERE name = 't'", &[])
        .expect("reads");
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].text(0).expect("schema"), "temp");
}

#[test]
fn the_bare_statement_is_the_one_that_would_reach_the_temp_table() {
    // The reason for the rule, kept as a measurement.
    let mut db = built("CREATE TABLE main.t(a integer); CREATE TEMP TABLE t(a integer)");
    db.connection()
        .execute_batch("ALTER TABLE t ADD COLUMN x text")
        .expect("runs");
    let in_temp = db
        .connection()
        .query("SELECT count(*) FROM pragma_table_info('t', 'temp')", &[])
        .expect("reads");
    assert_eq!(in_temp[0].integer(0).expect("n"), 2);
}

#[test]
fn every_name_position_is_written_the_way_sqlite_reads_it() {
    // Qualified where SQLite takes `main.`: what is created, altered or dropped.
    // Bare where it does not: a rename's new name, `ON <table>` and `REFERENCES`.
    let t = {
        let mut t = table(&[("a", col("integer"))]);
        t.foreign_keys.insert(
            ObjectLocalName::new("f"),
            pbps_model::ForeignKey {
                columns: vec!["a".into()],
                references_table: tn("p"),
                references_columns: vec!["id".into()],
                on_delete: pbps_model::ReferentialAction::NoAction,
                on_update: pbps_model::ReferentialAction::NoAction,
            },
        );
        t.indexes
            .insert(ObjectLocalName::new("i"), index(&["a"], false, None));
        t
    };
    let out = statements(&Sqlite::default(), create_table_change("t", &t)).expect("emits");
    assert!(
        out[0].sql.starts_with("CREATE TABLE main.\"t\" ("),
        "{}",
        out[0].sql
    );
    assert!(
        out[0].sql.contains("REFERENCES \"p\" (\"id\")"),
        "{}",
        out[0].sql
    );
    assert_eq!(out[1].sql, "CREATE INDEX main.\"i\" ON \"t\" (\"a\");");
    // And the engine takes each of them, and refuses the qualified forms.
    let mut db = built("CREATE TABLE p(id integer primary key)");
    run(db.connection(), &out).expect("runs");
    for bad in [
        "CREATE TABLE q(a integer REFERENCES main.p(id))",
        "CREATE INDEX j ON main.t(a)",
        "ALTER TABLE t RENAME TO main.u",
    ] {
        assert!(db.connection().execute_batch(bad).is_err(), "{bad}");
    }
}

/// For each in-place form: the text SQLite stores after the `ALTER`, against
/// what a rebuild by hand stores. No token newer than the floor's syntax appears,
/// which is why the forms are not gated by the floor (the quotes, case and white
/// space are SQLite's own and are not facts).
#[test]
fn each_in_place_form_leaves_the_text_a_rebuild_by_hand_would_store() {
    // ADD COLUMN.
    let mut db = built("CREATE TABLE t(a integer)");
    let add = statements(
        &Sqlite::default(),
        add_column("t", "x", col("integer").not_null().with_default("0")),
    )
    .expect("emits");
    run(db.connection(), &add).expect("runs");
    let hand = built("CREATE TABLE t(a integer, x integer NOT NULL DEFAULT 0)");
    let mut hand = hand;
    assert_eq!(
        stored(db.connection(), "t"),
        stored(hand.connection(), "t"),
        "add column"
    );
    // RENAME COLUMN: the table, its index and its view.
    let setup = |col: &str| {
        [
            "CREATE TABLE t(a integer, ",
            col,
            " text, CHECK (",
            col,
            " <> 'z')); CREATE INDEX i ON t(",
            col,
            ") WHERE ",
            col,
            " > 'a'; CREATE VIEW v AS SELECT ",
            col,
            " FROM t",
        ]
        .concat()
    };
    let mut db = built(&setup("b"));
    let rename = Change::Column(ColumnChange::RenameColumn {
        uid: uid(UidKind::Column, "t.b"),
        table: tn("t"),
        from: "b".into(),
        to: "c".into(),
        table_was: None,
    });
    run(
        db.connection(),
        &statements(&Sqlite::default(), rename).expect("emits"),
    )
    .expect("runs");
    let mut hand = built(&setup("c"));
    for name in ["t", "i", "v"] {
        assert_eq!(
            stored(db.connection(), name),
            stored(hand.connection(), name),
            "rename column: {name}"
        );
    }
    // RENAME TO: the table, its index, the foreign key of another table.
    let setup = |t: &str| {
        [
            "CREATE TABLE ",
            t,
            "(a integer primary key); CREATE TABLE c(a integer REFERENCES ",
            t,
            "(a)); CREATE INDEX i ON ",
            t,
            "(a)",
        ]
        .concat()
    };
    let mut db = built(&setup("t"));
    let rename = Change::Table(TableChange::RenameTable {
        uid: uid(UidKind::Table, "t"),
        from: tn("t"),
        to: tn("u"),
        defaults: Vec::new(),
    });
    run(
        db.connection(),
        &statements(&Sqlite::default(), rename).expect("emits"),
    )
    .expect("runs");
    let mut hand = built(&setup("u"));
    for name in ["u", "c", "i"] {
        assert_eq!(
            stored(db.connection(), name),
            stored(hand.connection(), name),
            "rename table: {name}"
        );
    }
}

#[test]
fn an_index_drop_names_the_index_and_a_drop_of_a_missing_one_fails_atomically() {
    let drop = Change::Index(IndexChange::DropIndex {
        table: tn("t"),
        name: ObjectLocalName::new("gone"),
    });
    let mut db = built("CREATE TABLE t(a integer)");
    assert!(
        run(
            db.connection(),
            &statements(&Sqlite::default(), drop).expect("emits")
        )
        .is_err()
    );
}
