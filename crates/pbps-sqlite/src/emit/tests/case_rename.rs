//! A table renamed to its own name in another ASCII case (#2950). SQLite reads
//! the two spellings as one name and refuses the direct rename ("there is
//! already another table or index with this name"), so the rename is two
//! statements through `__pbps_rename_<uid>`.

use pbps_dialect::Dialect as _;
use pbps_model::{Strategy, TableChange};

use super::golden::uid;
use super::rename_order::{ab, scenario};
use super::*;

fn rename(from: &str, to: &str) -> Change {
    Change::Table(TableChange::RenameTable {
        uid: uid(UidKind::Table, "t"),
        from: tn(from),
        to: tn(to),
        defaults: Vec::new(),
    })
}

fn inter() -> String {
    ["__pbps_rename_", uid(UidKind::Table, "t").as_str()].concat()
}

#[test]
fn a_case_only_rename_is_two_statements_through_a_name_of_the_tables_own() {
    let out = statements(&Sqlite::default(), rename("t", "T")).expect("emits");
    assert_eq!(
        sql_text(&out),
        vec![
            ["ALTER TABLE main.\"t\" RENAME TO \"", &inter(), "\";"].concat(),
            ["ALTER TABLE main.\"", &inter(), "\" RENAME TO \"T\";"].concat(),
        ]
    );
    // The statements record both steps, so the plan sees the name that is in
    // neither the baseline nor the declaration.
    assert_eq!(out[0].renames, vec![(tn("t"), tn(&inter()))]);
    assert_eq!(out[1].renames, vec![(tn(&inter()), tn("T"))]);
    let d = Sqlite::default();
    assert_eq!(
        d.intermediate_tables(&rename("t", "T"), Strategy::default()),
        Ok(vec![tn(&inter())])
    );
    assert_eq!(
        d.statement_count(
            &rename("t", "T"),
            Strategy::default(),
            PlanMode::Transactional
        ),
        Ok(2)
    );
    // A rename that changes more than case is one statement and no intermediate.
    assert_eq!(statements(&d, rename("t", "u")).expect("emits").len(), 1);
    assert_eq!(
        d.intermediate_tables(&rename("t", "u"), Strategy::default()),
        Ok(Vec::new())
    );
}

#[test]
fn the_intermediate_name_is_kept_from_declarations_and_taken_by_the_engine() {
    let name = inter();
    assert!(crate::names::is_intermediate(&name));
    assert!(
        crate::validate::table(&TableName::new("main", name.as_str()), &Default::default())
            .iter()
            .any(|error| error.to_string().contains(crate::names::RENAME_PREFIX)),
        "a declared table cannot take {name}"
    );
    assert!(
        !crate::names::is_ledger(&name),
        "{name} is not one of the ledger's tables"
    );
    assert!(!crate::ident::has_reserved_prefix(&name));
    assert!(crate::ident::quote(&name).is_ok());
    // And SQLite takes it: the first statement runs on a free name.
    let mut db = built("CREATE TABLE t(a integer)");
    db.connection()
        .execute_batch(&["ALTER TABLE main.t RENAME TO ", &name].concat())
        .expect("renames");
}

#[test]
fn the_rename_reaches_the_table_built_directly_with_what_names_it_rewritten() {
    let setup = |t: &str| {
        [
            "CREATE TABLE ",
            t,
            "(a integer primary key, b text); CREATE TABLE c(x integer REFERENCES ",
            t,
            "(a)); CREATE INDEX i ON ",
            t,
            "(b); CREATE VIEW v AS SELECT b FROM ",
            t,
        ]
        .concat()
    };
    let mut db = built(&setup("t"));
    run(
        db.connection(),
        &statements(&Sqlite::default(), rename("t", "T")).expect("emits"),
    )
    .expect("runs");
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut built(&setup("T"))),
        "case-only rename",
    );
    // The stored names are the new spelling, and nothing is left behind.
    let names = db
        .connection()
        .query(
            "SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name",
            &[],
        )
        .expect("reads");
    let names: Vec<_> = names
        .iter()
        .map(|r| r.text(0).expect("name").to_owned())
        .collect();
    assert_eq!(names, ["T", "c"]);
    // And back.
    run(
        db.connection(),
        &statements(&Sqlite::default(), rename("T", "t")).expect("emits"),
    )
    .expect("runs");
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut built(&setup("t"))),
        "and back",
    );
}

#[test]
fn a_name_already_held_refuses_the_first_statement_atomically() {
    for (what, extra) in [
        (
            "a table",
            ["CREATE TABLE ", &inter(), "(a integer)"].concat(),
        ),
        (
            "a view",
            ["CREATE VIEW ", &inter(), " AS SELECT 1"].concat(),
        ),
        ("an index", ["CREATE INDEX ", &inter(), " ON t(a)"].concat()),
        (
            "a table in another case",
            [
                "CREATE TABLE ",
                &inter().to_ascii_uppercase(),
                "(a integer)",
            ]
            .concat(),
        ),
    ] {
        let setup = ["CREATE TABLE t(a integer); ", &extra].concat();
        let mut db = built(&setup);
        let before = catalog_of(&mut db);
        let out = statements(&Sqlite::default(), rename("t", "T")).expect("emits");
        db.connection().execute_batch("BEGIN").expect("begin");
        let first = db.connection().execute_batch(&out[0].sql);
        let message = first.expect_err(what).to_string();
        assert!(
            message.contains("already another table or index"),
            "{what}: {message}"
        );
        db.connection().execute_batch("ROLLBACK").expect("rollback");
        assert_same(&before, &catalog_of(&mut db), what);
    }
}

#[test]
fn a_failure_between_the_two_statements_leaves_no_intermediate_name_behind() {
    let mut db = built("CREATE TABLE t(a integer); CREATE TABLE other(x integer)");
    let before = catalog_of(&mut db);
    let out = statements(&Sqlite::default(), rename("t", "T")).expect("emits");
    db.connection().execute_batch("BEGIN").expect("begin");
    db.connection()
        .execute_batch(&out[0].sql)
        .expect("the first statement");
    let held = db
        .connection()
        .query(
            "SELECT count(*) FROM sqlite_schema WHERE name = ?1",
            &[pbps_db::sqlite::Value::Text(inter())],
        )
        .expect("reads");
    assert_eq!(
        held[0].integer(0).expect("n"),
        1,
        "the name is held between the statements"
    );
    // Whatever fails next, the plan's transaction is rolled back whole.
    assert!(
        db.connection()
            .execute_batch("ALTER TABLE main.other DROP COLUMN nosuch")
            .is_err()
    );
    db.connection().execute_batch("ROLLBACK").expect("rollback");
    assert_same(
        &before,
        &catalog_of(&mut db),
        "nothing of the rename is left",
    );
    let held = db
        .connection()
        .query(
            "SELECT count(*) FROM sqlite_schema WHERE name = ?1",
            &[pbps_db::sqlite::Value::Text(inter())],
        )
        .expect("reads");
    assert_eq!(held[0].integer(0).expect("n"), 0);
}

#[test]
fn the_differ_routes_a_case_only_rename_through_the_intermediate() {
    let (base, declared) = (schema(&[("t", ab())], &[]), schema(&[("T", ab())], &[]));
    let (bi, di) = (
        ids(&base, &[]),
        ids(&declared, &[("T", "t"), ("T.a", "t.a"), ("T.b", "t.b")]),
    );
    let out = scenario(
        (&base, &bi, "CREATE TABLE t(a integer, b text)"),
        (&declared, &di, "CREATE TABLE \"T\"(a integer, b text)"),
    );
    assert_eq!(out.len(), 2, "{out:#?}");
    assert!(out[0].contains("__pbps_rename_"), "{out:#?}");
}
