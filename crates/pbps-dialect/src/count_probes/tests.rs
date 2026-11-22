use super::*;
use pbps_model::TableName;

/// A key under which two NULLs collide, spelled in upper case.
const EQUAL: CountSql = CountSql {
    rows: "COUNT(*)",
    grouped_rows: "ISNULL(SUM(c), 0)",
    count: "COUNT(*)",
    is_null: "IS NULL",
    unique_nulls: UniqueNulls::Equal,
};

/// A key under which they do not, with a clamped answer.
const DISTINCT: CountSql = CountSql {
    rows: "LEAST(count(*), 9)::int",
    grouped_rows: "LEAST(COALESCE(sum(c), 0), 9)::int",
    count: "count(*)",
    is_null: "IS NOT DISTINCT FROM NULL",
    unique_nulls: UniqueNulls::Distinct,
};

#[test]
fn a_null_count_is_spelled_by_the_engine() {
    let column = TableName::new("app", "t").column("v");
    assert_eq!(
        nulls(&EQUAL, &column, "[app].[t]", "[v]").sql,
        "SELECT COUNT(*) AS n FROM [app].[t] WHERE [v] IS NULL;"
    );
    assert_eq!(
        nulls(&DISTINCT, &column, &relation("SELECT 1 AS k0"), "r.k0").sql,
        "SELECT LEAST(count(*), 9)::int AS n FROM (\nSELECT 1 AS k0\n) AS r WHERE r.k0 IS NOT DISTINCT FROM NULL;"
    );
}

#[test]
fn a_description_shows_a_name_escaped_and_the_statement_is_not_touched() {
    let column = TableName::new("app", "t").column("v\n");
    let probe = nulls(&EQUAL, &column, "[app].[t]", "[v\n]");
    assert!(!probe.description.contains('\n'), "{}", probe.description);
    assert!(probe.sql.contains("[v\n] IS NULL"), "{}", probe.sql);
    let probe = check_violations(&EQUAL, &"c\u{1b}".into(), "t", "v > 0", None);
    assert!(
        !probe.description.contains('\u{1b}'),
        "{}",
        probe.description
    );
    assert!(!probe.refuses_when_it_fails);
}

#[test]
fn equal_nulls_count_every_row_of_a_group() {
    let probe = duplicates(&EQUAL, "t", &keys(2), "rows that would collide");
    assert_eq!(probe.description, "rows that would collide");
    assert_eq!(
        probe.sql,
        "SELECT ISNULL(SUM(c), 0) AS n FROM (\n    SELECT COUNT(*) AS c FROM t\n     GROUP BY r.k0, r.k1 HAVING COUNT(*) > 1) AS dup;"
    );
    assert!(!probe.sql.contains("IS NOT NULL"), "{}", probe.sql);
}

#[test]
fn distinct_nulls_leave_a_row_with_a_null_in_any_key_column_out() {
    let probe = duplicates(&DISTINCT, "t", &keys(2), "rows that would collide");
    assert_eq!(
        probe.sql,
        "SELECT LEAST(COALESCE(sum(c), 0), 9)::int AS n FROM (\n    SELECT count(*) AS c FROM t\n     WHERE r.k0 IS NOT NULL AND r.k1 IS NOT NULL\n     GROUP BY r.k0, r.k1 HAVING count(*) > 1) AS dup;"
    );
}

#[test]
fn the_key_expressions_are_the_callers_and_are_grouped_as_given() {
    let collated = vec!["r.k0 COLLATE Latin1_General_CS_AS".to_owned()];
    let probe = duplicates(&EQUAL, "t", &collated, "d");
    assert!(
        probe
            .sql
            .contains("GROUP BY r.k0 COLLATE Latin1_General_CS_AS HAVING"),
        "{}",
        probe.sql
    );
}

#[test]
fn a_check_counts_the_rows_its_predicate_is_false_for() {
    let probe = check_violations(
        &DISTINCT,
        &"positive".into(),
        "\"app\".\"t\"",
        "v > 0",
        None,
    );
    assert_eq!(
        probe.description,
        "rows that violate the new check positive"
    );
    assert_eq!(
        probe.sql,
        "SELECT LEAST(count(*), 9)::int AS n FROM \"app\".\"t\" WHERE NOT (v > 0);"
    );
}

#[test]
fn deleted_rows_are_not_counted_and_are_listed_in_one_order() {
    let literals: BTreeSet<String> = ["N'b'", "N'a'", "N'b'"].map(str::to_owned).into();
    let gone = Gone {
        key: "[code]",
        literals: &literals,
    };
    assert_eq!(
        check_violations(&EQUAL, &"c".into(), "t", "v > 0", Some(gone)).sql,
        "SELECT COUNT(*) AS n FROM t WHERE NOT (v > 0) AND [code] NOT IN (N'a', N'b');"
    );
}

#[test]
fn no_deleted_key_adds_no_clause() {
    let none = check_violations(&EQUAL, &"c".into(), "t", "v > 0", None);
    let empty = BTreeSet::new();
    let gone = Gone {
        key: "[code]",
        literals: &empty,
    };
    let nothing = check_violations(&EQUAL, &"c".into(), "t", "v > 0", Some(gone));
    assert_eq!(none, nothing);
    assert!(!none.sql.contains("NOT IN"), "{}", none.sql);
}

#[test]
fn an_orphan_is_a_child_row_with_no_null_in_its_key_and_no_parent() {
    let join = ["q.k0 = r.k0".to_owned(), "q.k1 = r.k1".to_owned()];
    assert_eq!(
        orphaned("CHILD", "PARENT", 2, &join),
        "FROM (\nCHILD\n) AS r\n WHERE r.k0 IS NOT NULL\n   AND r.k1 IS NOT NULL\n   AND NOT EXISTS (SELECT 1 FROM (\nPARENT\n) AS q WHERE q.k0 = r.k0 AND q.k1 = r.k1)"
    );
}

#[test]
fn keys_name_the_relations_columns_and_none_for_no_column() {
    assert_eq!(keys(2), ["r.k0", "r.k1"]);
    assert!(keys(0).is_empty());
}
