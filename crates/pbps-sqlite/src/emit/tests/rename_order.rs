//! Renames through the differ's own route (#2950). SQLite refuses every rename
//! while any view is invalid, and rewrites the views, indexes and foreign keys
//! that name a renamed table or column, so the statements' order is what makes a
//! rename safe. These scenarios run the plan the shared scheduler orders on a
//! real SQLite and compare the catalog with the declared schema built directly.
//! They are why a plain rename needs no ordering input of its own: the
//! scheduler already puts a module after the tables it reads and before the
//! tables it stops reading.

use super::*;

/// Plans `base` to `declared`, runs the plan on a database built from
/// `base_sql`, and compares with `declared_sql` built directly. Returns the
/// statements, so a scenario can pin the order too.
pub(super) fn scenario(
    (base, base_ids, base_sql): (&Schema, &IdsFile, &str),
    (declared, declared_ids, declared_sql): (&Schema, &IdsFile, &str),
) -> Vec<String> {
    let d = Sqlite::default();
    let out = plan((base, base_ids), (declared, declared_ids), &d).expect("plans");
    let mut db = built(base_sql);
    run(db.connection(), &out).unwrap_or_else(|e| panic!("{e}\nplan: {:#?}", sql_text(&out)));
    assert_same(
        &catalog_of(&mut db),
        &catalog_of(&mut built(declared_sql)),
        "the plan reaches the declared schema",
    );
    sql_text(&out).into_iter().map(str::to_owned).collect()
}

pub(super) fn ab() -> Table {
    table(&[("a", col("integer")), ("b", col("text"))])
}

pub(super) fn ac() -> Table {
    table(&[("a", col("integer")), ("c", col("text"))])
}

#[test]
fn a_column_rename_with_a_view_that_follows_it() {
    let (base, declared) = (
        schema(&[("t", ab())], &[("v", "SELECT b FROM t")]),
        schema(&[("t", ac())], &[("v", "SELECT c FROM t")]),
    );
    let (bi, di) = (ids(&base, &[]), ids(&declared, &[("t.c", "t.b")]));
    let out = scenario(
        (
            &base,
            &bi,
            "CREATE TABLE t(a integer, b text); CREATE VIEW v AS SELECT b FROM t",
        ),
        (
            &declared,
            &di,
            "CREATE TABLE t(a integer, c text); CREATE VIEW v AS SELECT c FROM t",
        ),
    );
    assert_eq!(
        out[0],
        "ALTER TABLE main.\"t\" RENAME COLUMN \"b\" TO \"c\";"
    );
}

#[test]
fn a_table_rename_with_a_view_that_follows_it() {
    let (base, declared) = (
        schema(&[("t", ab())], &[("v", "SELECT b FROM t")]),
        schema(&[("u", ab())], &[("v", "SELECT b FROM u")]),
    );
    let (bi, di) = (
        ids(&base, &[]),
        ids(&declared, &[("u", "t"), ("u.a", "t.a"), ("u.b", "t.b")]),
    );
    scenario(
        (
            &base,
            &bi,
            "CREATE TABLE t(a integer, b text); CREATE VIEW v AS SELECT b FROM t",
        ),
        (
            &declared,
            &di,
            "CREATE TABLE u(a integer, b text); CREATE VIEW v AS SELECT b FROM u",
        ),
    );
}

#[test]
fn a_new_view_that_names_the_new_column_is_created_after_the_rename() {
    let (base, declared) = (
        schema(&[("t", ab())], &[]),
        schema(&[("t", ac())], &[("v", "SELECT c FROM t")]),
    );
    let (bi, di) = (ids(&base, &[]), ids(&declared, &[("t.c", "t.b")]));
    let out = scenario(
        (&base, &bi, "CREATE TABLE t(a integer, b text)"),
        (
            &declared,
            &di,
            "CREATE TABLE t(a integer, c text); CREATE VIEW v AS SELECT c FROM t",
        ),
    );
    let rename = out
        .iter()
        .position(|s| s.contains("RENAME COLUMN"))
        .expect("renames");
    let create = out
        .iter()
        .position(|s| s.contains("CREATE VIEW"))
        .expect("creates");
    assert!(rename < create, "{out:#?}");
}

#[test]
fn a_dropped_view_is_gone_before_the_rename_of_a_column_it_read() {
    let (base, declared) = (
        schema(&[("t", ab())], &[("v", "SELECT b FROM t")]),
        schema(&[("t", ac())], &[]),
    );
    let (bi, di) = (ids(&base, &[]), ids(&declared, &[("t.c", "t.b")]));
    scenario(
        (
            &base,
            &bi,
            "CREATE TABLE t(a integer, b text); CREATE VIEW v AS SELECT b FROM t",
        ),
        (&declared, &di, "CREATE TABLE t(a integer, c text)"),
    );
}

#[test]
fn a_view_moved_off_a_dropped_table_does_not_leave_an_invalid_view_in_front_of_a_rename() {
    let base = schema(
        &[("t", ab()), ("old", ab()), ("x", ab())],
        &[("v", "SELECT b FROM old")],
    );
    let declared = schema(&[("t", ab()), ("y", ab())], &[("v", "SELECT b FROM t")]);
    let (bi, di) = (
        ids(&base, &[]),
        ids(&declared, &[("y", "x"), ("y.a", "x.a"), ("y.b", "x.b")]),
    );
    scenario(
        (
            &base,
            &bi,
            "CREATE TABLE t(a integer, b text); CREATE TABLE old(a integer, b text); CREATE TABLE x(a integer, b text); CREATE VIEW v AS SELECT b FROM old",
        ),
        (
            &declared,
            &di,
            "CREATE TABLE t(a integer, b text); CREATE TABLE y(a integer, b text); CREATE VIEW v AS SELECT b FROM t",
        ),
    );
}

#[test]
fn a_view_over_a_dropped_table_that_is_also_dropped_goes_first() {
    let base = schema(&[("t", ab()), ("x", ab())], &[("v", "SELECT b FROM t")]);
    let declared = schema(&[("y", ab())], &[]);
    let (bi, di) = (
        ids(&base, &[]),
        ids(&declared, &[("y", "x"), ("y.a", "x.a"), ("y.b", "x.b")]),
    );
    scenario(
        (
            &base,
            &bi,
            "CREATE TABLE t(a integer, b text); CREATE TABLE x(a integer, b text); CREATE VIEW v AS SELECT b FROM t",
        ),
        (&declared, &di, "CREATE TABLE y(a integer, b text)"),
    );
}

/// The table names are chosen to sort before and after each other, so the
/// order cannot be the names' order: a rename comes before the table drop that
/// leaves a view invalid until its re-creation, whatever the names are.
#[test]
fn a_rename_never_lands_between_a_table_drop_and_the_view_that_stopped_reading_it() {
    for (from, to, dropped) in [
        ("a1", "a2", "m"),
        ("z1", "z2", "m"),
        ("a1", "z2", "m"),
        ("z1", "a2", "m"),
        ("zz", "aa", "mm"),
    ] {
        for column_rename in [false, true] {
            let mut base_tables = vec![("t", ab()), (dropped, ab()), (from, ab())];
            let mut declared_tables = vec![("t", ab()), (to, ab())];
            let (to_a, from_a, to_b, from_b) = (
                [to, ".a"].concat(),
                [from, ".a"].concat(),
                [to, ".b"].concat(),
                [from, ".b"].concat(),
            );
            let mut seeds = vec![
                (to, from),
                (to_a.as_str(), from_a.as_str()),
                (to_b.as_str(), from_b.as_str()),
            ];
            if column_rename {
                declared_tables[0] = ("t", ac());
                seeds.push(("t.c", "t.b"));
            }
            base_tables.sort_by_key(|(name, _)| *name);
            declared_tables.sort_by_key(|(name, _)| *name);
            let read = if column_rename { "c" } else { "b" };
            let base = schema(
                &base_tables,
                &[("v", &["SELECT b FROM ", dropped].concat())],
            );
            let declared = schema(
                &declared_tables,
                &[("v", &["SELECT ", read, " FROM t"].concat())],
            );
            let (bi, di) = (ids(&base, &[]), ids(&declared, &seeds));
            let mut base_sql = String::new();
            for (name, _) in &base_tables {
                base_sql.push_str(&["CREATE TABLE ", name, "(a integer, b text);"].concat());
            }
            base_sql.push_str(&["CREATE VIEW v AS SELECT b FROM ", dropped].concat());
            let mut declared_sql = String::new();
            for (name, t) in &declared_tables {
                let second = if t.columns.contains_key("c") {
                    "c"
                } else {
                    "b"
                };
                declared_sql
                    .push_str(&["CREATE TABLE ", name, "(a integer, ", second, " text);"].concat());
            }
            declared_sql.push_str(&["CREATE VIEW v AS SELECT ", read, " FROM t"].concat());
            let out = scenario((&base, &bi, &base_sql), (&declared, &di, &declared_sql));
            let last_rename = out
                .iter()
                .rposition(|s| s.contains(" RENAME "))
                .expect("renames");
            let drop = out
                .iter()
                .position(|s| s.starts_with("DROP TABLE"))
                .expect("drops");
            assert!(last_rename < drop, "{from}->{to}: {out:#?}");
        }
    }
}

#[test]
fn a_table_renamed_into_the_name_a_dropped_table_held_is_dropped_first() {
    let base = schema(&[("u", ab()), ("x", ab())], &[]);
    let declared = schema(&[("u", ab())], &[]);
    let (bi, di) = (
        ids(&base, &[]),
        ids(&declared, &[("u", "x"), ("u.a", "x.a"), ("u.b", "x.b")]),
    );
    let out = scenario(
        (
            &base,
            &bi,
            "CREATE TABLE u(a integer, b text); CREATE TABLE x(a integer, b text)",
        ),
        (&declared, &di, "CREATE TABLE u(a integer, b text)"),
    );
    let drop = out
        .iter()
        .position(|s| s.starts_with("DROP TABLE"))
        .expect("drops");
    let rename = out
        .iter()
        .position(|s| s.contains(" RENAME TO "))
        .expect("renames");
    assert!(drop < rename, "{out:#?}");
}

#[test]
fn a_swap_of_two_table_names_is_refused_when_planned_and_never_at_apply() {
    let base = schema(&[("p", ab()), ("q", ac())], &[]);
    let declared = schema(&[("p", ac()), ("q", ab())], &[]);
    let (bi, di) = (
        ids(&base, &[]),
        ids(
            &declared,
            &[
                ("p", "q"),
                ("p.a", "q.a"),
                ("p.c", "q.c"),
                ("q", "p"),
                ("q.a", "p.a"),
                ("q.b", "p.b"),
            ],
        ),
    );
    let refusal =
        plan((&base, &bi), (&declared, &di), &Sqlite::default()).expect_err("a cycle of renames");
    assert!(refusal.contains("cycle"), "{refusal}");
}
