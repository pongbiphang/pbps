//! `DROP COLUMN` through the differ's own route (#2950). SQLite refuses it for a
//! column in an index, a table check, a foreign key, a key or unique constraint,
//! a view, and for the last column of a table, and, while any view is invalid,
//! for every column of every table. The statement cannot see any of that. The
//! order does: each scenario here runs the plan the shared scheduler writes on a
//! real SQLite and compares the catalog with the declared schema built directly,
//! and then runs the bare `DROP COLUMN` on the same baseline to show the engine
//! refuses it there, so it is the order that made the plan work.

use pbps_model::{CheckConstraint, ForeignKey, ObjectLocalName, ReferentialAction};

use super::golden::index;
use super::rename_order::{ab, ac, scenario};
use super::*;
use pbps_dialect::Dialect as _;

fn view_ids(schema: &Schema, seeds: &[(&str, &str)]) -> IdsFile {
    ids(schema, seeds)
}

/// The engine's own answer to the bare statement on the baseline.
fn bare_refusal(base_sql: &str, drop: &str) -> String {
    let mut db = built(base_sql);
    db.connection()
        .execute_batch(drop)
        .expect_err("the engine refuses the bare drop")
        .to_string()
}

fn t_with_check() -> Table {
    let mut t = ab();
    t.checks.insert(
        ObjectLocalName::new("k"),
        CheckConstraint {
            expression: "b > a".into(),
        },
    );
    t
}

#[test]
fn a_column_in_an_index_is_dropped_after_the_index() {
    let mut base_t = ab();
    base_t
        .indexes
        .insert(ObjectLocalName::new("i"), index(&["b"], false, None));
    let a_only = table(&[("a", col("integer"))]);
    let (base, declared) = (schema(&[("t", base_t)], &[]), schema(&[("t", a_only)], &[]));
    let (bi, di) = (ids(&base, &[]), ids(&declared, &[]));
    let base_sql = "CREATE TABLE t(a integer, b text); CREATE INDEX i ON t(b)";
    let out = scenario(
        (&base, &bi, base_sql),
        (&declared, &di, "CREATE TABLE t(a integer)"),
    );
    let index = out
        .iter()
        .position(|s| s.starts_with("DROP INDEX"))
        .expect("drops the index");
    let column = out
        .iter()
        .position(|s| s.contains("DROP COLUMN"))
        .expect("drops the column");
    assert!(index < column, "{out:#?}");
    assert!(bare_refusal(base_sql, "ALTER TABLE t DROP COLUMN b").contains("no such column"));
}

#[test]
fn a_column_in_a_table_check_is_dropped_after_the_check() {
    let (base, declared) = (
        schema(&[("t", t_with_check())], &[]),
        schema(&[("t", table(&[("a", col("integer"))]))], &[]),
    );
    let (bi, di) = (ids(&base, &[]), ids(&declared, &[]));
    let base_sql = "CREATE TABLE t(a integer, b text, CONSTRAINT k CHECK (b > a))";
    let out = scenario(
        (&base, &bi, base_sql),
        (&declared, &di, "CREATE TABLE t(a integer)"),
    );
    let check = out
        .iter()
        .position(|s| s.contains("DROP CONSTRAINT"))
        .expect("drops the check");
    let column = out
        .iter()
        .position(|s| s.contains("DROP COLUMN"))
        .expect("drops the column");
    assert!(check < column, "{out:#?}");
    assert!(bare_refusal(base_sql, "ALTER TABLE t DROP COLUMN b").contains("no such column"));
}

#[test]
fn a_column_named_only_in_a_partial_index_filter_is_dropped_after_the_index() {
    let mut base_t = ab();
    base_t.indexes.insert(
        ObjectLocalName::new("i"),
        index(&["a"], false, Some("b > 0")),
    );
    let (base, declared) = (
        schema(&[("t", base_t)], &[]),
        schema(&[("t", table(&[("a", col("integer"))]))], &[]),
    );
    let (bi, di) = (ids(&base, &[]), ids(&declared, &[]));
    let base_sql = "CREATE TABLE t(a integer, b text); CREATE INDEX i ON t(a) WHERE b > 0";
    let out = scenario(
        (&base, &bi, base_sql),
        (&declared, &di, "CREATE TABLE t(a integer)"),
    );
    let index = out
        .iter()
        .position(|s| s.starts_with("DROP INDEX"))
        .expect("drops the index");
    let column = out
        .iter()
        .position(|s| s.contains("DROP COLUMN"))
        .expect("drops the column");
    assert!(index < column, "{out:#?}");
    assert!(bare_refusal(base_sql, "ALTER TABLE t DROP COLUMN b").contains("no such column"));
}

#[test]
fn a_foreign_key_over_the_column_is_dropped_first() {
    let mut child = table(&[("a", col("integer")), ("b", col("integer"))]);
    child.foreign_keys.insert(
        ObjectLocalName::new("f"),
        ForeignKey {
            columns: vec!["b".into()],
            references_table: tn("p"),
            references_columns: vec!["id".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    let parent = table(&[("id", col("integer"))]);
    let mut parent_pk = parent.clone();
    parent_pk.primary_key = Some(pbps_model::PrimaryKey {
        name: None,
        columns: vec!["id".into()],
        storage_parameters: Default::default(),
        filegroup: None,
    });
    let (base, declared) = (
        schema(&[("p", parent_pk.clone()), ("t", child)], &[]),
        schema(
            &[("p", parent_pk), ("t", table(&[("a", col("integer"))]))],
            &[],
        ),
    );
    let (bi, di) = (ids(&base, &[]), ids(&declared, &[]));
    let base_sql = "CREATE TABLE p(id integer, PRIMARY KEY (id)); CREATE TABLE t(a integer, b integer, CONSTRAINT f FOREIGN KEY (b) REFERENCES p (id) ON DELETE NO ACTION ON UPDATE NO ACTION)";
    let out = scenario(
        (&base, &bi, base_sql),
        (
            &declared,
            &di,
            "CREATE TABLE p(id integer, PRIMARY KEY (id)); CREATE TABLE t(a integer)",
        ),
    );
    let key = out
        .iter()
        .position(|s| s.contains("DROP CONSTRAINT"))
        .expect("drops the key");
    let column = out
        .iter()
        .position(|s| s.contains("DROP COLUMN"))
        .expect("drops the column");
    assert!(key < column, "{out:#?}");
    assert!(bare_refusal(base_sql, "ALTER TABLE t DROP COLUMN b").contains("unknown column"));
}

#[test]
fn a_view_that_stops_reading_the_column_is_torn_down_around_the_drop() {
    let (base, declared) = (
        schema(&[("t", ab())], &[("v", "SELECT b FROM t")]),
        schema(
            &[("t", table(&[("a", col("integer"))]))],
            &[("v", "SELECT a FROM t")],
        ),
    );
    let (bi, di) = (view_ids(&base, &[]), view_ids(&declared, &[]));
    let base_sql = "CREATE TABLE t(a integer, b text); CREATE VIEW v AS SELECT b FROM t";
    let out = scenario(
        (&base, &bi, base_sql),
        (
            &declared,
            &di,
            "CREATE TABLE t(a integer); CREATE VIEW v AS SELECT a FROM t",
        ),
    );
    let drop_view = out
        .iter()
        .position(|s| s.starts_with("DROP VIEW"))
        .expect("drops the view");
    let column = out
        .iter()
        .position(|s| s.contains("DROP COLUMN"))
        .expect("drops the column");
    let create_view = out
        .iter()
        .position(|s| s.starts_with("CREATE VIEW"))
        .expect("creates it again");
    assert!(drop_view < column && column < create_view, "{out:#?}");
    assert!(bare_refusal(base_sql, "ALTER TABLE t DROP COLUMN b").contains("error in view"));
}

#[test]
fn a_view_that_is_dropped_goes_before_the_column_it_read() {
    let (base, declared) = (
        schema(&[("t", ab())], &[("v", "SELECT b FROM t")]),
        schema(&[("t", table(&[("a", col("integer"))]))], &[]),
    );
    let (bi, di) = (view_ids(&base, &[]), view_ids(&declared, &[]));
    let out = scenario(
        (
            &base,
            &bi,
            "CREATE TABLE t(a integer, b text); CREATE VIEW v AS SELECT b FROM t",
        ),
        (&declared, &di, "CREATE TABLE t(a integer)"),
    );
    assert!(
        out.iter().position(|s| s.starts_with("DROP VIEW"))
            < out.iter().position(|s| s.contains("DROP COLUMN")),
        "{out:#?}"
    );
}

#[test]
fn a_view_over_a_view_that_reads_the_column_is_torn_down_with_it() {
    let (base, declared) = (
        schema(
            &[("t", ab())],
            &[("v1", "SELECT b FROM t"), ("v2", "SELECT * FROM v1")],
        ),
        schema(
            &[("t", table(&[("a", col("integer"))]))],
            &[("v1", "SELECT a AS b FROM t"), ("v2", "SELECT * FROM v1")],
        ),
    );
    let (bi, di) = (view_ids(&base, &[]), view_ids(&declared, &[]));
    let out = scenario(
        (
            &base,
            &bi,
            "CREATE TABLE t(a integer, b text); CREATE VIEW v1 AS SELECT b FROM t; CREATE VIEW v2 AS SELECT * FROM v1",
        ),
        (
            &declared,
            &di,
            "CREATE TABLE t(a integer); CREATE VIEW v1 AS SELECT a AS b FROM t; CREATE VIEW v2 AS SELECT * FROM v1",
        ),
    );
    let drops: Vec<_> = out.iter().filter(|s| s.starts_with("DROP VIEW")).collect();
    assert_eq!(drops.len(), 2, "{out:#?}");
}

/// A star view survives the drop of a column by itself, and is rebuilt all the
/// same: the scan cannot tell it from one whose column count matters.
#[test]
fn a_view_that_reads_every_column_by_star_is_rebuilt_around_the_drop() {
    let (base, declared) = (
        schema(&[("t", ab())], &[("v", "SELECT * FROM t")]),
        schema(
            &[("t", table(&[("a", col("integer"))]))],
            &[("v", "SELECT * FROM t")],
        ),
    );
    let (bi, di) = (view_ids(&base, &[]), view_ids(&declared, &[]));
    let out = scenario(
        (
            &base,
            &bi,
            "CREATE TABLE t(a integer, b text); CREATE VIEW v AS SELECT * FROM t",
        ),
        (
            &declared,
            &di,
            "CREATE TABLE t(a integer); CREATE VIEW v AS SELECT * FROM t",
        ),
    );
    assert!(
        out[0].starts_with("DROP VIEW") && out[2].starts_with("CREATE VIEW"),
        "{out:#?}"
    );
}

/// The number of columns is part of a view: a union of two stars is invalid
/// from the drop on one side to the drop on the other.
#[test]
fn a_union_of_stars_is_torn_down_around_the_drops_on_both_sides() {
    let two = || table(&[("a", col("integer")), ("b", col("text"))]);
    let one = || table(&[("a", col("integer"))]);
    let body = "SELECT * FROM t UNION SELECT * FROM u";
    let (base, declared) = (
        schema(&[("t", two()), ("u", two())], &[("v", body)]),
        schema(&[("t", one()), ("u", one())], &[("v", body)]),
    );
    let (bi, di) = (view_ids(&base, &[]), view_ids(&declared, &[]));
    let base_sql = "CREATE TABLE t(a integer, b text); CREATE TABLE u(a integer, b text); CREATE VIEW v AS SELECT * FROM t UNION SELECT * FROM u";
    let out = scenario(
        (&base, &bi, base_sql),
        (
            &declared,
            &di,
            "CREATE TABLE t(a integer); CREATE TABLE u(a integer); CREATE VIEW v AS SELECT * FROM t UNION SELECT * FROM u",
        ),
    );
    assert!(
        out.first().is_some_and(|s| s.starts_with("DROP VIEW"))
            && out.last().is_some_and(|s| s.starts_with("CREATE VIEW")),
        "{out:#?}"
    );
    assert!(bare_refusal(base_sql, "ALTER TABLE t DROP COLUMN b").contains("error in view"));
}

#[test]
fn a_view_that_reads_a_dropped_table_does_not_stand_invalid_across_a_column_drop() {
    // The view stops reading `old`; `old` is dropped; another table drops a
    // column in the same plan. SQLite refuses that drop while the view is
    // invalid, which it would be between the table's drop and the view's own
    // alteration, so the view is torn down first.
    let base = schema(&[("t", ab()), ("old", ab())], &[("v", "SELECT b FROM old")]);
    let declared = schema(
        &[("t", table(&[("a", col("integer"))]))],
        &[("v", "SELECT a FROM t")],
    );
    let (bi, di) = (view_ids(&base, &[]), view_ids(&declared, &[]));
    let base_sql = "CREATE TABLE t(a integer, b text); CREATE TABLE old(a integer, b text); CREATE VIEW v AS SELECT b FROM old";
    let out = scenario(
        (&base, &bi, base_sql),
        (
            &declared,
            &di,
            "CREATE TABLE t(a integer); CREATE VIEW v AS SELECT a FROM t",
        ),
    );
    let drop_view = out
        .iter()
        .position(|s| s.starts_with("DROP VIEW"))
        .expect("drops the view");
    let column = out
        .iter()
        .position(|s| s.contains("DROP COLUMN"))
        .expect("drops the column");
    assert!(drop_view < column, "{out:#?}");
    let mut db = built(base_sql);
    db.connection()
        .execute_batch("DROP TABLE old")
        .expect("drops");
    assert!(
        db.connection()
            .execute_batch("ALTER TABLE t DROP COLUMN b")
            .is_err()
    );
}

#[test]
fn a_column_that_is_renamed_is_not_a_dropped_one() {
    let (base, declared) = (schema(&[("t", ab())], &[]), schema(&[("t", ac())], &[]));
    let (bi, di) = (ids(&base, &[]), ids(&declared, &[("t.c", "t.b")]));
    let out = scenario(
        (&base, &bi, "CREATE TABLE t(a integer, b text)"),
        (&declared, &di, "CREATE TABLE t(a integer, c text)"),
    );
    assert!(!out.iter().any(|s| s.contains("DROP COLUMN")), "{out:#?}");
}

#[test]
fn a_key_or_unique_column_is_refused_by_name_and_never_reaches_the_engine() {
    let mut keyed = ab();
    keyed.primary_key = Some(pbps_model::PrimaryKey {
        name: None,
        columns: vec!["b".into()],
        storage_parameters: Default::default(),
        filegroup: None,
    });
    let (base, declared) = (
        schema(&[("t", keyed)], &[]),
        schema(&[("t", table(&[("a", col("integer"))]))], &[]),
    );
    let (bi, di) = (ids(&base, &[]), ids(&declared, &[]));
    let refusal = plan((&base, &bi), (&declared, &di), &Sqlite::default()).expect_err("refused");
    assert!(refusal.contains("table rebuild"), "{refusal}");
    assert!(
        bare_refusal(
            "CREATE TABLE t(a integer, b text, PRIMARY KEY (b))",
            "ALTER TABLE t DROP COLUMN b"
        )
        .contains("PRIMARY KEY")
    );
}

#[test]
fn a_table_the_declaration_leaves_without_a_column_is_refused_by_validation() {
    let errors = Sqlite::default().validate_table(&tn("t"), &Table::default());
    assert!(
        errors.iter().any(|e| e.to_string().contains("no column")),
        "{errors:?}"
    );
    assert!(
        Sqlite::default()
            .validate_table(&tn("t"), &table(&[("a", col("integer"))]))
            .is_empty()
    );
}

#[test]
fn a_column_named_after_a_comment_the_shared_scanner_would_nest_is_still_found() {
    // SQLite ends a block comment at its first `*/`, so `b` below is code and
    // the check names it; a scanner that nests comments would read all of it as
    // one comment and let the plan drop the column first.
    let mut base_t = ab();
    base_t.checks.insert(
        ObjectLocalName::new("k"),
        CheckConstraint {
            expression: "a > 0 /* /* */ AND b IS NOT NULL".into(),
        },
    );
    let (base, declared) = (
        schema(&[("t", base_t)], &[]),
        schema(&[("t", table(&[("a", col("integer"))]))], &[]),
    );
    let (bi, di) = (ids(&base, &[]), ids(&declared, &[]));
    let base_sql =
        "CREATE TABLE t(a integer, b text, CONSTRAINT k CHECK (a > 0 /* /* */ AND b IS NOT NULL))";
    let out = scenario(
        (&base, &bi, base_sql),
        (&declared, &di, "CREATE TABLE t(a integer)"),
    );
    let check = out
        .iter()
        .position(|s| s.contains("DROP CONSTRAINT"))
        .expect("drops the check");
    let column = out
        .iter()
        .position(|s| s.contains("DROP COLUMN"))
        .expect("drops the column");
    assert!(check < column, "{out:#?}");
    assert!(bare_refusal(base_sql, "ALTER TABLE t DROP COLUMN b").contains("no such column"));
}

/// Every row of `t`, as text, in rowid order.
fn rows(db: &mut Fresh) -> Vec<Vec<String>> {
    db.connection()
        .query("SELECT * FROM t ORDER BY rowid", &[])
        .expect("reads")
        .iter()
        .map(|row| {
            (0..row.len())
                .map(|i| {
                    row.text(i)
                        .map(str::to_owned)
                        .or_else(|_| row.integer(i).map(|n| n.to_string()))
                        .unwrap_or_else(|_| "NULL".into())
                })
                .collect()
        })
        .collect()
}

/// SQLite will not drop the only column of a table, so a plan that replaces
/// every column of a table (`t(a)` to `t(b)`, a new column identity) adds before
/// it drops: the order the differ writes for an engine that refuses the last
/// column (#2963, DEC-2963.1). The bare drop is the engine's refusal, and the
/// plan that reaches the declared schema is the one the order made.
#[test]
fn replacing_every_column_of_a_table_adds_before_it_drops_the_last_and_applies() {
    let (base, declared) = (
        schema(&[("t", table(&[("a", col("integer"))]))], &[]),
        schema(&[("t", table(&[("b", col("text"))]))], &[]),
    );
    let (bi, di) = (ids(&base, &[]), ids(&declared, &[]));
    let statements = plan((&base, &bi), (&declared, &di), &Sqlite::default()).expect("plans");
    let text = sql_text(&statements);
    assert!(
        text[0].contains("ADD COLUMN"),
        "the add is first: {text:#?}"
    );
    let base_sql = "CREATE TABLE t(a integer); INSERT INTO t VALUES (1), (2)";
    assert!(
        bare_refusal(base_sql, "ALTER TABLE t DROP COLUMN a").contains("no other columns exist")
    );
    let mut db = built(base_sql);
    db.connection().execute_batch("BEGIN").expect("begin");
    run(db.connection(), &statements).expect("applies");
    db.connection().execute_batch("COMMIT").expect("commit");
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut built("CREATE TABLE t(b text)")),
        "the plan reaches the declared schema",
    );
    assert_eq!(
        rows(&mut db),
        vec![vec!["NULL".to_owned()], vec!["NULL".to_owned()]],
        "and the rows stay"
    );
}

#[test]
fn replacing_two_columns_keeps_a_column_at_every_statement_and_converges() {
    let (base, declared) = (
        schema(
            &[("t", table(&[("a", col("integer")), ("b", col("integer"))]))],
            &[],
        ),
        schema(
            &[("t", table(&[("c", col("text")), ("d", col("integer"))]))],
            &[],
        ),
    );
    let (bi, di) = (ids(&base, &[]), ids(&declared, &[]));
    let statements = plan((&base, &bi), (&declared, &di), &Sqlite::default()).expect("plans");
    let mut db = built("CREATE TABLE t(a integer, b integer); INSERT INTO t VALUES (1, 2)");
    db.connection().execute_batch("BEGIN").expect("begin");
    run(db.connection(), &statements)
        .unwrap_or_else(|e| panic!("{e}\n{:#?}", sql_text(&statements)));
    db.connection().execute_batch("COMMIT").expect("commit");
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut built("CREATE TABLE t(c text, d integer)")),
        "the plan reaches the declared schema",
    );
}

#[test]
fn a_generated_column_does_not_count_as_the_column_that_stays() {
    // `g` is generated and stays out of the count: `a` is the last ordinary
    // column, so its drop waits for the add of `b` (measured: "must have at
    // least one non-generated column").
    let generated = |expression: &str| {
        let mut g = col("integer");
        g.value_source = pbps_model::ValueSource::Generated(pbps_model::Generated {
            expression: expression.into(),
            stored: false,
        });
        g
    };
    let (base, declared) = (
        schema(
            &[("t", table(&[("a", col("integer")), ("g", generated("1"))]))],
            &[],
        ),
        schema(&[("t", table(&[("b", col("integer"))]))], &[]),
    );
    let (bi, di) = (ids(&base, &[]), ids(&declared, &[]));
    let statements = plan((&base, &bi), (&declared, &di), &Sqlite::default()).expect("plans");
    let base_sql = "CREATE TABLE t(a integer, g integer GENERATED ALWAYS AS (1) VIRTUAL)";
    let mut db = built(base_sql);
    db.connection().execute_batch("BEGIN").expect("begin");
    run(db.connection(), &statements)
        .unwrap_or_else(|e| panic!("{e}\n{:#?}", sql_text(&statements)));
    db.connection().execute_batch("COMMIT").expect("commit");
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut built("CREATE TABLE t(b integer)")),
        "the plan reaches the declared schema",
    );
}

/// No order performs it: the old column cannot go first (the only one), and the
/// new one cannot come first (the name stands). Refused when the plan is made,
/// with the way out, and never reaching the engine.
#[test]
fn the_only_column_replaced_by_a_column_of_the_same_name_is_refused_when_the_plan_is_made() {
    let (base, declared) = (
        schema(&[("t", table(&[("a", col("integer"))]))], &[]),
        schema(&[("t", table(&[("a", col("text"))]))], &[]),
    );
    let (bi, di) = (ids(&base, &[]), ids(&declared, &[("t.a", "t.a_again")]));
    let refused =
        plan((&base, &bi), (&declared, &di), &Sqlite::default()).expect_err("no order performs it");
    assert!(
        refused.contains("replaces every one of its columns"),
        "{refused}"
    );
    assert!(refused.contains("`a`"), "{refused}");
    assert!(refused.contains("two revisions"), "{refused}");
    // Negative: the same column under its own identity is no replacement.
    assert!(plan((&base, &bi), (&base, &bi), &Sqlite::default()).is_ok());
    // And the engine's two refusals are what the plan avoids.
    assert!(
        bare_refusal("CREATE TABLE t(a integer)", "ALTER TABLE t DROP COLUMN a")
            .contains("no other columns exist")
    );
    assert!(
        bare_refusal(
            "CREATE TABLE t(a integer)",
            "ALTER TABLE t ADD COLUMN a text"
        )
        .contains("duplicate column name")
    );
}

#[test]
fn names_that_differ_only_by_case_are_one_name_to_sqlite_and_are_refused_alike() {
    let (base, declared) = (
        schema(&[("t", table(&[("a", col("integer"))]))], &[]),
        schema(&[("t", table(&[("A", col("integer"))]))], &[]),
    );
    let (bi, di) = (ids(&base, &[]), ids(&declared, &[]));
    let refused = plan((&base, &bi), (&declared, &di), &Sqlite::default())
        .expect_err("SQLite folds the names");
    assert!(
        refused.contains("replaces every one of its columns"),
        "{refused}"
    );
}

#[test]
fn a_table_that_keeps_one_baseline_column_applies_and_converges() {
    // `a` goes, `b` stays, `c` arrives: the table has a column at every step.
    let (base, declared) = (
        schema(
            &[("t", table(&[("a", col("integer")), ("b", col("integer"))]))],
            &[],
        ),
        schema(
            &[("t", table(&[("b", col("integer")), ("c", col("text"))]))],
            &[],
        ),
    );
    let (bi, di) = (ids(&base, &[]), ids(&declared, &[]));
    let d = Sqlite::default();
    let statements = plan((&base, &bi), (&declared, &di), &d).expect("plans");
    let mut db = built("CREATE TABLE t(a integer, b integer); INSERT INTO t VALUES (1, 10)");
    db.connection().execute_batch("BEGIN").expect("begin");
    run(db.connection(), &statements).expect("applies");
    db.connection().execute_batch("COMMIT").expect("commit");
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut built("CREATE TABLE t(b integer, c text)")),
        "converges",
    );
    assert_eq!(
        rows(&mut db),
        vec![vec!["10".to_owned(), "NULL".to_owned()]]
    );
}

#[test]
fn a_view_that_stops_reading_a_dropped_view_does_not_stand_invalid_across_a_column_drop() {
    // `v1` is dropped and `v2` stops reading it. Between the two, `v2` would be
    // invalid, and SQLite refuses a column drop while any view is.
    let base = schema(
        &[("t", ab())],
        &[("v1", "SELECT a FROM t"), ("v2", "SELECT * FROM v1")],
    );
    let declared = schema(
        &[("t", table(&[("a", col("integer"))]))],
        &[("v2", "SELECT a FROM t")],
    );
    let (bi, di) = (view_ids(&base, &[]), view_ids(&declared, &[]));
    let base_sql = "CREATE TABLE t(a integer, b text); CREATE VIEW v1 AS SELECT a FROM t; CREATE VIEW v2 AS SELECT * FROM v1";
    let out = scenario(
        (&base, &bi, base_sql),
        (
            &declared,
            &di,
            "CREATE TABLE t(a integer); CREATE VIEW v2 AS SELECT a FROM t",
        ),
    );
    let column = out
        .iter()
        .position(|s| s.contains("DROP COLUMN"))
        .expect("drops the column");
    assert!(
        out[..column]
            .iter()
            .filter(|s| s.starts_with("DROP VIEW"))
            .count()
            == 2,
        "{out:#?}"
    );
}

#[test]
fn a_view_naming_the_column_after_a_comment_the_shared_scanner_would_nest_is_torn_down() {
    // The comment ends at its first `*/`, so `b` is code and the view reads it.
    // A scanner that nests comments would read the rest as one comment, miss
    // the view, and the plan would drop the column with the view standing.
    let body = "SELECT a /* /* */ , b FROM t";
    let (base, declared) = (
        schema(&[("t", ab())], &[("v", body)]),
        schema(
            &[("t", table(&[("a", col("integer"))]))],
            &[("v", "SELECT a FROM t")],
        ),
    );
    let (bi, di) = (view_ids(&base, &[]), view_ids(&declared, &[]));
    let out = scenario(
        (
            &base,
            &bi,
            &["CREATE TABLE t(a integer, b text); CREATE VIEW v AS ", body].concat(),
        ),
        (
            &declared,
            &di,
            "CREATE TABLE t(a integer); CREATE VIEW v AS SELECT a FROM t",
        ),
    );
    assert!(
        out.iter().position(|s| s.starts_with("DROP VIEW"))
            < out.iter().position(|s| s.contains("DROP COLUMN")),
        "{out:#?}"
    );
}

/// Runs a plan that drops `t.b` (or `u.b` after a rename) and checks the view
/// named `torn` is dropped before the column and created after it.
fn torn_around_the_drop(
    base: (&Schema, &IdsFile, &str),
    declared: (&Schema, &IdsFile, &str),
    torn: &str,
) {
    let base_sql = base.2;
    let out = scenario(base, declared);
    let at = |start: &str| {
        out.iter()
            .position(|s| s.starts_with(start) && s.contains(&["\"", torn, "\""].concat()))
            .unwrap_or_else(|| panic!("{start} {torn}: {out:#?}"))
    };
    let column = out
        .iter()
        .position(|s| s.contains("DROP COLUMN"))
        .expect("drops the column");
    assert!(
        at("DROP VIEW") < column && column < at("CREATE VIEW"),
        "{out:#?}"
    );
    assert!(bare_refusal(base_sql, "ALTER TABLE t DROP COLUMN b").contains("error in view"));
}

#[test]
fn a_view_of_a_table_that_is_renamed_and_loses_the_column_is_torn_down() {
    let (base, declared) = (
        schema(&[("t", ab())], &[("v", "SELECT b FROM t")]),
        schema(
            &[("u", table(&[("a", col("integer"))]))],
            &[("v", "SELECT a FROM u")],
        ),
    );
    let (bi, di) = (view_ids(&base, &[]), view_ids(&declared, &[("u", "t")]));
    torn_around_the_drop(
        (
            &base,
            &bi,
            "CREATE TABLE t(a integer, b text); CREATE VIEW v AS SELECT b FROM t",
        ),
        (
            &declared,
            &di,
            "CREATE TABLE u(a integer); CREATE VIEW v AS SELECT a FROM u",
        ),
        "v",
    );
}

#[test]
fn a_view_that_reads_the_column_through_a_star_view_is_torn_down() {
    let (base, declared) = (
        schema(
            &[("t", ab())],
            &[("v1", "SELECT * FROM t"), ("v2", "SELECT b FROM v1")],
        ),
        schema(
            &[("t", table(&[("a", col("integer"))]))],
            &[("v1", "SELECT * FROM t"), ("v2", "SELECT a FROM v1")],
        ),
    );
    let (bi, di) = (view_ids(&base, &[]), view_ids(&declared, &[]));
    torn_around_the_drop(
        (
            &base,
            &bi,
            "CREATE TABLE t(a integer, b text); CREATE VIEW v1 AS SELECT * FROM t; CREATE VIEW v2 AS SELECT b FROM v1",
        ),
        (
            &declared,
            &di,
            "CREATE TABLE t(a integer); CREATE VIEW v1 AS SELECT * FROM t; CREATE VIEW v2 AS SELECT a FROM v1",
        ),
        "v2",
    );
}

#[test]
fn a_view_that_names_its_table_in_single_quotes_is_torn_down() {
    let (base, declared) = (
        schema(&[("t", ab())], &[("v", "SELECT b FROM 't'")]),
        schema(
            &[("t", table(&[("a", col("integer"))]))],
            &[("v", "SELECT a FROM 't'")],
        ),
    );
    let (bi, di) = (view_ids(&base, &[]), view_ids(&declared, &[]));
    torn_around_the_drop(
        (
            &base,
            &bi,
            "CREATE TABLE t(a integer, b text); CREATE VIEW v AS SELECT b FROM 't'",
        ),
        (
            &declared,
            &di,
            "CREATE TABLE t(a integer); CREATE VIEW v AS SELECT a FROM 't'",
        ),
        "v",
    );
}

/// SQLite drops a view whatever reads it, so a view's own drop waits for
/// nothing. The scan is lexical: two views that only take each other's names
/// as column aliases would otherwise each wait for the other.
#[test]
fn two_dropped_views_that_spell_each_others_names_wait_for_nothing() {
    let (base, declared) = (
        schema(
            &[("t", ab())],
            &[
                ("v1", "SELECT a AS v2 FROM t"),
                ("v2", "SELECT a AS v1 FROM t"),
            ],
        ),
        schema(&[("t", ab())], &[]),
    );
    let (bi, di) = (view_ids(&base, &[]), view_ids(&declared, &[]));
    let base_sql = "CREATE TABLE t(a integer, b text); CREATE VIEW v1 AS SELECT a AS v2 FROM t; CREATE VIEW v2 AS SELECT a AS v1 FROM t";
    let out = scenario(
        (&base, &bi, base_sql),
        (&declared, &di, "CREATE TABLE t(a integer, b text)"),
    );
    assert_eq!(out.len(), 2, "{out:#?}");
    // And one that does read the other: the engine drops the read one first
    // without complaint.
    let mut db = built(
        "CREATE TABLE t(a integer); CREATE VIEW v1 AS SELECT a FROM t; CREATE VIEW v2 AS SELECT a FROM v1",
    );
    db.connection()
        .execute_batch("DROP VIEW v1; DROP VIEW v2")
        .expect("a read view is dropped first");
}

/// The audit's half of the order: SQLite's own requirement that the add that
/// replaces the last column exists before its drop, checked by the verifier
/// over each order of the two changes, with a kept column as the negative.
#[test]
fn the_last_columns_drop_requires_its_replacement_to_exist_first() {
    use pbps_diff::order::{Baseline, analyse, reordered_operations, verify};
    use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};
    use pbps_model::plan_names::PlanNames;
    use pbps_model::{Change, ColumnChange, PlannedChange};

    let violations = |was: &[&str], now: &[&str], order: &[usize]| {
        let columns = |names: &[&str]| {
            table(
                &names
                    .iter()
                    .map(|n| (*n, col("integer")))
                    .collect::<Vec<_>>(),
            )
        };
        let (base, declared) = (
            schema(&[("t", columns(was))], &[]),
            schema(&[("t", columns(now))], &[]),
        );
        let (bi, di) = (ids(&base, &[]), ids(&declared, &[]));
        let name = base.tables.keys().next().unwrap().clone();
        let uid =
            |ids: &IdsFile, column: &str| ids.column_uid(&name.column(column)).cloned().unwrap();
        let logical: Vec<PlannedChange> = vec![
            Change::Column(ColumnChange::DropColumn {
                uid: uid(&bi, "a"),
                column: name.column("a"),
            }),
            Change::Column(ColumnChange::AddColumn {
                uid: uid(&di, "b"),
                table: name.clone(),
                name: "b".into(),
                column: Box::new(col("integer")),
            }),
        ]
        .into_iter()
        .map(PlannedChange::new)
        .collect();
        let ops = Operations::of_plan(&logical, ModuleEdits::InPlace);
        let owners = PlanOwners::new(PlanNames::of(&logical, &di).unwrap(), &bi, &di);
        let (before, after) = (
            Snapshot {
                schema: &base,
                ids: &bi,
            },
            Snapshot {
                schema: &declared,
                ids: &di,
            },
        );
        let baseline = Baseline::of(before, &owners).expect("read the test baseline");
        let shadowed = analyse(&ops, &owners, before, after, &Sqlite::default())
            .expect("read the analysis baseline");
        assert!(shadowed.unowned.is_empty(), "{:?}", shadowed.unowned);
        let ordered = reordered_operations(&ops, order);
        let effects: Vec<_> = order.iter().map(|&i| shadowed.effects[i].clone()).collect();
        verify(ordered.as_slice(), &effects, &baseline, &Default::default())
    };
    // `a` replaced by `b`: the add first is the order SQLite performs.
    assert_eq!(violations(&["a"], &["b"], &[1, 0]), []);
    assert!(!violations(&["a"], &["b"], &[0, 1]).is_empty());
    // Negative: `k` stays, so either order is one the engine performs.
    assert_eq!(violations(&["a", "k"], &["k", "b"], &[0, 1]), []);
    assert_eq!(violations(&["a", "k"], &["k", "b"], &[1, 0]), []);
}
