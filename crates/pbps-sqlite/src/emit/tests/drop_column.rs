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

/// The one `DROP COLUMN` refusal that reaches the engine. SQLite will not drop
/// the only column of a table, and a plan that replaces every column of a table
/// (`t(a)` to `t(b)`, with a new column identity) is ordered with the drop
/// before the add, so the drop is the plan's first statement and fails when it
/// runs, inside the plan's transaction.
///
/// The dialect cannot prevent it. The effects a dialect states only verify an
/// order, they do not move a statement (a requirement that the added column
/// exist first fails the order audit instead of reordering); the emitter is
/// given the column and not the baseline's other columns, so it cannot know the
/// drop is the last one; and validation sees only the declared table. The
/// ordering rule that fixes it for every engine is issue #2963; until it lands,
/// this is the engine's refusal, atomic, and a rehearsal of the plan on an
/// in-memory database catches it before a plan is written.
#[test]
fn replacing_every_column_of_a_table_fails_atomically_at_the_engine_until_the_scheduler_orders_it()
{
    let (base, declared) = (
        schema(&[("t", table(&[("a", col("integer"))]))], &[]),
        schema(&[("t", table(&[("b", col("text"))]))], &[]),
    );
    let (bi, di) = (ids(&base, &[]), ids(&declared, &[]));
    let statements = plan((&base, &bi), (&declared, &di), &Sqlite::default()).expect("plans");
    let text = sql_text(&statements);
    assert!(
        text[0].contains("DROP COLUMN"),
        "the drop is first: {text:#?}"
    );
    let mut db = built("CREATE TABLE t(a integer); INSERT INTO t VALUES (1), (2)");
    let (catalog_before, rows_before) = (catalog_of(&mut db), rows(&mut db));
    db.connection().execute_batch("BEGIN").expect("begin");
    let failure = run(db.connection(), &statements).expect_err("the engine refuses");
    assert!(failure.contains("no other columns exist"), "{failure}");
    db.connection().execute_batch("ROLLBACK").expect("rollback");
    assert_same(
        &catalog_before,
        &catalog_of(&mut db),
        "the catalog is as it was",
    );
    assert_eq!(rows_before, rows(&mut db), "and so are the rows");
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
