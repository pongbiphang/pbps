//! Nullability and the constraint forms (#2950): `SET` and `DROP NOT NULL`,
//! `ADD` and `DROP CONSTRAINT` of a check, and `DROP CONSTRAINT` of a foreign
//! key. Each is run on a real SQLite and compared with the table built directly;
//! the refusals the engine gives at apply are pinned as atomic, inside the
//! transaction a plan runs in.

use pbps_model::{
    CheckConstraint, ColumnChange, ConstraintChange, ForeignKey, ObjectLocalName,
    ReferentialAction, RiskClass,
};

use super::golden::uid;
use super::*;

fn n(name: &str) -> ObjectLocalName {
    ObjectLocalName::new(name)
}

pub(super) fn add_check(table: &str, name: &str, expression: &str) -> Change {
    Change::Constraint(ConstraintChange::AddCheck {
        table: tn(table),
        name: n(name),
        constraint: CheckConstraint {
            expression: expression.into(),
        },
    })
}

pub(super) fn drop_check(table: &str, name: &str) -> Change {
    Change::Constraint(ConstraintChange::DropCheck {
        table: tn(table),
        name: n(name),
    })
}

pub(super) fn drop_fk(table: &str, name: &str) -> Change {
    Change::Constraint(ConstraintChange::DropForeignKey {
        table: tn(table),
        name: n(name),
    })
}

pub(super) fn nullability(to_nullable: bool) -> Change {
    Change::Column(ColumnChange::AlterColumnNullability {
        uid: uid(UidKind::Column, "t.b"),
        column: pbps_model::ColumnRef {
            table: tn("t"),
            name: "b".into(),
        },
        ty: ty("integer"),
        to_nullable,
        collation: None,
        through_external_partitions: false,
    })
}

fn sql_of(change: Change) -> Vec<String> {
    statements(&Sqlite::default(), change)
        .expect("emits")
        .into_iter()
        .map(|s| s.sql)
        .collect()
}

/// Runs `change` inside a transaction on a database built from `setup`, and
/// returns the engine's answer; on a refusal the transaction is rolled back, as
/// a plan's is.
fn in_transaction(setup: &str, change: Change) -> (Result<(), String>, Catalog, Catalog) {
    let mut db = built(setup);
    let before = catalog_of(&mut db);
    db.connection().execute_batch("BEGIN").expect("begin");
    let result = run(
        db.connection(),
        &statements(&Sqlite::default(), change).expect("emits"),
    );
    db.connection()
        .execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })
        .expect("end");
    (result, before, catalog_of(&mut db))
}

#[test]
fn set_and_drop_not_null_write_the_alter_column_forms_and_reach_the_declared_table() {
    assert_eq!(
        sql_of(nullability(false)),
        vec!["ALTER TABLE main.\"t\" ALTER COLUMN \"b\" SET NOT NULL;"]
    );
    assert_eq!(
        sql_of(nullability(true)),
        vec!["ALTER TABLE main.\"t\" ALTER COLUMN \"b\" DROP NOT NULL;"]
    );
    let (set, ..) = in_transaction("CREATE TABLE t(a integer, b integer)", nullability(false));
    set.expect("sets");
    let mut tightened = built("CREATE TABLE t(a integer, b integer)");
    run(
        tightened.connection(),
        &statements(&Sqlite::default(), nullability(false)).expect("emits"),
    )
    .expect("runs");
    assert_same(
        &catalog_of(&mut tightened),
        &catalog_of(&mut built("CREATE TABLE t(a integer, b integer NOT NULL)")),
        "SET NOT NULL",
    );
    run(
        tightened.connection(),
        &statements(&Sqlite::default(), nullability(true)).expect("emits"),
    )
    .expect("runs");
    assert_same(
        &catalog_of(&mut tightened),
        &catalog_of(&mut built("CREATE TABLE t(a integer, b integer)")),
        "and back",
    );
}

#[test]
fn set_not_null_on_a_table_holding_a_null_is_the_engines_refusal_inside_the_transaction() {
    let (result, before, after) = in_transaction(
        "CREATE TABLE t(a integer, b integer); INSERT INTO t(a) VALUES (1)",
        nullability(false),
    );
    assert!(result.expect_err("refused").contains("constraint failed"));
    assert_same(
        &before,
        &after,
        "a refused SET NOT NULL leaves the table as it was",
    );
    // The shared code already tells the reviewer: the change carries the risk
    // class that `--allow` has to name.
    let risks = nullability(false).intrinsic_risks_with(crate::ident::identifier_continues);
    assert!(risks.contains(&RiskClass::NotNull), "{risks:?}");
    // The relaxation adds no risk.
    assert!(
        nullability(true)
            .intrinsic_risks_with(crate::ident::identifier_continues)
            .is_empty()
    );
}

#[test]
fn no_dependent_blocks_a_nullability_change() {
    for (what, setup) in [
        (
            "a partial index filter",
            "CREATE TABLE t(a integer, b integer); CREATE INDEX i ON t(a) WHERE b > 0",
        ),
        (
            "a view",
            "CREATE TABLE t(a integer, b integer); CREATE VIEW v AS SELECT b FROM t",
        ),
        (
            "a check",
            "CREATE TABLE t(a integer, b integer, CHECK (b > 0))",
        ),
        (
            "a unique key",
            "CREATE TABLE t(a integer, b integer UNIQUE); CREATE TABLE c(x integer REFERENCES t(b))",
        ),
        (
            "an index key",
            "CREATE TABLE t(a integer, b integer); CREATE INDEX i ON t(b)",
        ),
    ] {
        let (result, ..) = in_transaction(setup, nullability(false));
        assert!(result.is_ok(), "{what}: {result:?}");
    }
}

#[test]
fn a_check_is_added_and_dropped_by_name() {
    assert_eq!(
        sql_of(add_check("t", "k", "a > 0")),
        vec!["ALTER TABLE main.\"t\" ADD CONSTRAINT \"k\" CHECK (a > 0);"]
    );
    assert_eq!(
        sql_of(drop_check("t", "k")),
        vec!["ALTER TABLE main.\"t\" DROP CONSTRAINT \"k\";"]
    );
    let mut db = built("CREATE TABLE t(a integer, b integer)");
    run(
        db.connection(),
        &statements(&Sqlite::default(), add_check("t", "k", "a > b")).expect("emits"),
    )
    .expect("adds");
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut built(
            "CREATE TABLE t(a integer, b integer, CONSTRAINT k CHECK (a > b))",
        )),
        "add check",
    );
    run(
        db.connection(),
        &statements(&Sqlite::default(), drop_check("t", "k")).expect("emits"),
    )
    .expect("drops");
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut built("CREATE TABLE t(a integer, b integer)")),
        "drop check",
    );
}

#[test]
fn a_foreign_key_is_dropped_by_name_and_the_parent_stays() {
    let setup = "CREATE TABLE p(id integer primary key); CREATE TABLE t(a integer, CONSTRAINT f FOREIGN KEY (a) REFERENCES p (id) ON DELETE NO ACTION ON UPDATE NO ACTION)";
    let mut db = built(setup);
    run(
        db.connection(),
        &statements(&Sqlite::default(), drop_fk("t", "f")).expect("emits"),
    )
    .expect("drops");
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut built(
            "CREATE TABLE p(id integer primary key); CREATE TABLE t(a integer)",
        )),
        "drop foreign key",
    );
    let _ = ForeignKey {
        columns: vec![],
        references_table: tn("p"),
        references_columns: vec![],
        on_delete: ReferentialAction::NoAction,
        on_update: ReferentialAction::NoAction,
    };
}

#[test]
fn a_constraint_is_found_by_its_name_however_the_name_is_spelled() {
    // A name that needs quoting, with a quote inside, and a name in another
    // ASCII case: the quoting function writes each so SQLite reads the same
    // name, and `DROP CONSTRAINT` compares ASCII case folded.
    for (name, spelled_in_file) in [
        ("a b", "\"a b\""),
        ("q\"q", "\"q\"\"q\""),
        ("K", "k"),
        ("k", "K"),
    ] {
        let setup = [
            "CREATE TABLE t(a integer, CONSTRAINT ",
            spelled_in_file,
            " CHECK (a > 0))",
        ]
        .concat();
        let (result, ..) = in_transaction(&setup, drop_check("t", name));
        result.unwrap_or_else(|e| panic!("{name}: {e}"));
        let mut db = built(&setup);
        run(
            db.connection(),
            &statements(&Sqlite::default(), drop_check("t", name)).expect("emits"),
        )
        .expect("drops");
        assert_same(
            &catalog_of(&mut db),
            &catalog_of(&mut built("CREATE TABLE t(a integer)")),
            name,
        );
    }
    // And added under such a name, read back as that name.
    let mut db = built("CREATE TABLE t(a integer)");
    run(
        db.connection(),
        &statements(&Sqlite::default(), add_check("t", "q\"q", "a > 0")).expect("emits"),
    )
    .expect("adds");
    run(
        db.connection(),
        &statements(&Sqlite::default(), drop_check("t", "q\"q")).expect("emits"),
    )
    .expect("drops it by the same name");
}

#[test]
fn a_constraint_the_file_does_not_hold_is_refused_atomically_by_the_engine() {
    // Absent, or another kind's name: `no such constraint`, nothing
    // changed. The statement cannot see the stored text; a declaration names
    // every constraint it makes, so this is a baseline that is not the file's.
    for (what, setup, change) in [
        (
            "a name not there",
            "CREATE TABLE t(a integer, CONSTRAINT k CHECK (a > 0))",
            drop_check("t", "j"),
        ),
        (
            "a column's name",
            "CREATE TABLE t(a integer, CONSTRAINT k CHECK (a > 0))",
            drop_check("t", "a"),
        ),
    ] {
        let (result, before, after) = in_transaction(setup, change);
        let message = result.expect_err(what);
        assert!(message.contains("no such constraint"), "{what}: {message}");
        assert_same(&before, &after, what);
    }
    // An unnamed constraint has no name to write, and the empty name is refused
    // by the quoting function, so no statement is written for it at all.
    assert!(statements(&Sqlite::default(), drop_check("t", "")).is_err());
}

#[test]
fn adding_a_check_is_refused_atomically_for_a_violating_row_a_taken_name_and_a_missing_column() {
    let setup =
        "CREATE TABLE t(a integer, CONSTRAINT k CHECK (a > -100)); INSERT INTO t VALUES (5)";
    for (what, change, text) in [
        (
            "a row that violates it",
            add_check("t", "j", "a < 0"),
            "constraint failed",
        ),
        (
            "a name another constraint holds",
            add_check("t", "k", "a > 0"),
            "already exists",
        ),
        (
            "the same name in another case",
            add_check("t", "K", "a > 0"),
            "already exists",
        ),
        (
            "a column that is not there",
            add_check("t", "j", "zz > 0"),
            "no such column",
        ),
    ] {
        let (result, before, after) = in_transaction(setup, change);
        let message = result.expect_err(what);
        assert!(message.contains(text), "{what}: {message}");
        assert_same(&before, &after, what);
    }
}

#[test]
fn the_constraint_forms_carry_the_risk_classes_the_shared_code_gives_them() {
    let risks = |c: Change| c.intrinsic_risks_with(crate::ident::identifier_continues);
    assert!(risks(add_check("t", "k", "a > 0")).contains(&RiskClass::Constraint));
    assert!(risks(drop_check("t", "k")).is_empty());
    assert!(risks(drop_fk("t", "f")).is_empty());
}

#[test]
fn a_check_the_declaration_could_not_make_is_refused_before_a_statement() {
    for expression in ["", "a > 0; DROP TABLE t", "(a > 0", "a IN (SELECT 1)"] {
        assert!(
            statements(&Sqlite::default(), add_check("t", "k", expression)).is_err(),
            "{expression:?}"
        );
    }
}

#[test]
fn two_constraints_of_a_table_never_share_a_name_so_a_drop_by_name_finds_only_its_own() {
    // SQLite drops the first constraint of a name, whatever its kind, and
    // refuses to add a second of the same name in any ASCII case. A declaration
    // that gives two constraints of one table the same name is refused, and the
    // spelling that differs only in case is probed here.
    use pbps_dialect::Dialect as _;
    for (first, second) in [("k", "k"), ("k", "K")] {
        let mut t = table(&[("a", col("integer")), ("b", col("integer"))]);
        t.checks.insert(
            n(first),
            CheckConstraint {
                expression: "a > 0".into(),
            },
        );
        t.foreign_keys.insert(
            n(second),
            ForeignKey {
                columns: vec!["b".into()],
                references_table: tn("p"),
                references_columns: vec!["id".into()],
                on_delete: ReferentialAction::NoAction,
                on_update: ReferentialAction::NoAction,
            },
        );
        let errors = Sqlite::default().validate_table(&tn("t"), &t);
        assert_eq!(errors.len(), 1, "{first}/{second}: {errors:?}");
        // The engine takes the table (so this is a rule, not a restriction of
        // SQLite's grammar), and then cannot tell the two apart.
        let sql = "CREATE TABLE t(a integer, b integer, CONSTRAINT k CHECK (a > 0), CONSTRAINT K FOREIGN KEY (b) REFERENCES t(a))";
        let mut db = built(sql);
        let drop = db
            .connection()
            .execute_batch("ALTER TABLE t DROP CONSTRAINT k");
        assert!(drop.is_ok(), "SQLite drops the first of the two");
        let left = db
            .connection()
            .query("SELECT sql FROM sqlite_schema WHERE name = 't'", &[])
            .expect("reads");
        assert!(
            !left[0].text(0).expect("sql").contains("CHECK"),
            "the check went, the key stayed"
        );
    }
    // Names that differ are fine, and an unnamed primary key has no name to compare.
    let mut t = table(&[("a", col("integer"))]);
    t.checks.insert(
        n("k"),
        CheckConstraint {
            expression: "a > 0".into(),
        },
    );
    t.checks.insert(
        n("j"),
        CheckConstraint {
            expression: "a < 9".into(),
        },
    );
    assert!(Sqlite::default().validate_table(&tn("t"), &t).is_empty());
}

#[test]
fn a_nullability_change_through_partitions_is_refused_and_not_written() {
    let change = Change::Column(ColumnChange::AlterColumnNullability {
        uid: uid(UidKind::Column, "t.b"),
        column: pbps_model::ColumnRef {
            table: tn("t"),
            name: "b".into(),
        },
        ty: ty("integer"),
        to_nullable: false,
        collation: None,
        through_external_partitions: true,
    });
    assert!(matches!(
        statements(&Sqlite::default(), change),
        Err(DialectError::Unsupported { .. })
    ));
}
