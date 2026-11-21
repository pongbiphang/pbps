//! A guarded drop's row requirement, against a real SQLite.

use std::collections::BTreeMap;

use pbps_db::sqlite::Connection;
use pbps_model::{DropGuard, DropRequirement, UidKind};

use super::check;
use crate::emit::tests::golden::uid;
use crate::emit::tests::tn;
use crate::tests::Fresh;

fn guard(predicates: &[&str]) -> DropGuard {
    DropGuard {
        table_uid: uid(UidKind::Table, "t"),
        table: tn("t"),
        detach_from: None,
        requirements: predicates
            .iter()
            .enumerate()
            .map(|(n, predicate)| {
                (
                    uid(UidKind::Column, &format!("t.{n}")),
                    DropRequirement {
                        table_uid: uid(UidKind::Table, "t"),
                        predicate: (*predicate).to_owned(),
                        required_at: "2026-11-20".into(),
                        operator: "reviewer".into(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>(),
    }
}

fn with_rows(rows: &str) -> Fresh {
    let mut fresh = Fresh::memory();
    fresh
        .connection()
        .execute_batch(&format!("CREATE TABLE t(a integer, b text); {rows}"))
        .expect("rows");
    fresh
}

fn tables(c: &Connection) -> i64 {
    c.query("SELECT count(*) FROM sqlite_schema WHERE name = 't'", &[])
        .expect("query")[0]
        .integer(0)
        .expect("count")
}

#[test]
fn a_requirement_every_row_meets_permits_and_an_empty_table_permits() {
    let mut fresh = with_rows("INSERT INTO t VALUES (1, 'x'), (2, 'y')");
    assert_eq!(
        check(fresh.connection(), &guard(&["a > 0", "b <> ''"])),
        Ok(())
    );
    let mut empty = with_rows("");
    assert_eq!(check(empty.connection(), &guard(&["a > 0"])), Ok(()));
    // No requirement is nothing to check.
    assert_eq!(check(empty.connection(), &guard(&[])), Ok(()));
}

#[test]
fn a_violated_requirement_names_the_count_and_the_predicate_and_removes_nothing() {
    let mut fresh = with_rows("INSERT INTO t VALUES (-1, 'x'), (-2, 'y'), (3, 'z')");
    let refused = check(fresh.connection(), &guard(&["a > 0"])).expect_err("violated");
    assert!(
        refused.contains("2 row(s)") && refused.contains("`a > 0`"),
        "{refused}"
    );
    assert!(refused.contains("nothing was removed"), "{refused}");
    assert_eq!(tables(fresh.connection()), 1);
    // The second of two requirements is the one that fails.
    let second = check(fresh.connection(), &guard(&["a > -5", "a > 0"])).expect_err("second");
    assert!(second.contains("`a > 0`"), "{second}");
}

#[test]
fn a_null_answer_is_a_violation_as_is_not_true_makes_it_on_the_other_engines() {
    let mut fresh = with_rows("INSERT INTO t VALUES (NULL, 'x'), (1, 'y')");
    let refused = check(fresh.connection(), &guard(&["a > 0"])).expect_err("a null is not true");
    assert!(refused.contains("1 row(s)"), "{refused}");
    // Negative: a row whose answer is true is not counted.
    let mut fine = with_rows("INSERT INTO t VALUES (1, 'x')");
    assert!(check(fine.connection(), &guard(&["a > 0"])).is_ok());
}

#[test]
fn a_predicate_the_engine_cannot_evaluate_never_permits_the_removal() {
    let mut fresh = with_rows("INSERT INTO t VALUES (1, 'x')");
    for predicate in ["missing > 0", "a >", "nosuchfn(a) > 0"] {
        let refused = check(fresh.connection(), &guard(&[predicate])).expect_err(predicate);
        assert!(
            refused.contains("could not be evaluated"),
            "{predicate}: {refused}"
        );
        assert!(
            refused.contains("never permits removal"),
            "{predicate}: {refused}"
        );
    }
    // A table that is not there is the same.
    let mut gone = Fresh::memory();
    assert!(check(gone.connection(), &guard(&["1 = 1"])).is_err());
}

#[test]
fn a_predicate_cannot_carry_a_second_statement_and_a_trailing_comment_does_not_swallow_the_rest() {
    let mut fresh = with_rows("INSERT INTO t VALUES (1, 'x')");
    let refused = check(
        fresh.connection(),
        &guard(&["a > 0); DROP TABLE t; SELECT (1"]),
    )
    .expect_err("one statement only");
    assert!(refused.contains("not one expression"), "{refused}");
    assert_eq!(tables(fresh.connection()), 1, "the table is still there");
    // Negative: a comment at the end is closed, and the requirement still counts.
    assert!(check(fresh.connection(), &guard(&["a > 0 -- positive"])).is_ok());
    assert!(check(fresh.connection(), &guard(&["a < 0 -- negative"])).is_err());
}

#[test]
fn the_check_runs_inside_the_applys_transaction_and_leaves_it_open() {
    let mut fresh = with_rows("INSERT INTO t VALUES (1, 'x')");
    let c = fresh.connection();
    c.execute_batch("BEGIN IMMEDIATE; INSERT INTO t VALUES (-1, 'z')")
        .expect("the apply's own work");
    // It sees the transaction's own rows, which is what the drop will remove.
    assert!(check(c, &guard(&["a > 0"])).is_err());
    assert!(c.in_transaction());
    c.execute_batch("ROLLBACK").expect("ended");
    assert!(check(c, &guard(&["a > 0"])).is_ok());
}

/// The count is only as good as the wrapper: a predicate that ends it early or
/// leaves a comment open would be counted as something else.
#[test]
fn a_predicate_that_escapes_the_wrapper_is_refused_and_never_counted() {
    let mut fresh = with_rows("INSERT INTO t VALUES (1, 'x')");
    for predicate in ["0) /*", "0) OR (1", "a > 0 /* open", "'open"] {
        let refused = check(fresh.connection(), &guard(&[predicate])).expect_err(predicate);
        assert!(
            refused.contains("not one expression"),
            "{predicate}: {refused}"
        );
    }
    // Negative: the same meanings written whole are counted.
    assert!(check(fresh.connection(), &guard(&["(0) OR (1)"])).is_ok());
    assert!(check(fresh.connection(), &guard(&["a > 0 /* closed */"])).is_ok());
    assert!(check(fresh.connection(), &guard(&["(0)"])).is_err());
}
