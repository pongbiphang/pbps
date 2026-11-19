//! The `PlanDialect` surface (#2950): the guard a drop carries, the two plan
//! modes, and the counts the scheduler asks for.

use std::collections::BTreeMap;

use pbps_dialect::Dialect;
use pbps_model::{DropGuard, Strategy, TableChange};

use super::golden::{add_column, create_table_change, uid};
use super::*;

fn drop_table() -> Change {
    Change::Table(TableChange::DropTable {
        uid: uid(UidKind::Table, "t"),
        name: tn("t"),
        detach_from: None,
    })
}

fn guard() -> DropGuard {
    DropGuard {
        table_uid: uid(UidKind::Table, "t"),
        table: tn("t"),
        detach_from: None,
        requirements: BTreeMap::new(),
    }
}

fn lowered(
    change: Change,
    guard: Option<DropGuard>,
    mode: PlanMode,
) -> Result<Vec<Statement>, DialectError> {
    let d = Sqlite::default();
    let mut planned = PlannedChange::new(change);
    planned.drop_guard = guard;
    let ordered = pbps_diff::order::singleton(planned, mode, &d);
    lower(&ordered as &dyn PlanView<Sqlite>, 0, &d).map(|l| l.into_statements())
}

#[test]
fn a_guarded_drop_carries_its_guard_on_the_statement_that_drops() {
    let out = lowered(drop_table(), Some(guard()), PlanMode::Transactional).expect("emits");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].drop_guard, Some(guard()));
    assert_eq!(
        lowered(drop_table(), None, PlanMode::Transactional).expect("emits")[0].drop_guard,
        None
    );
}

#[test]
fn a_guarded_drop_is_refused_in_a_staged_plan() {
    let err = lowered(drop_table(), Some(guard()), PlanMode::Staged).expect_err("refused");
    assert!(err.to_string().contains("transactional"), "{err}");
    // The same drop without a guard is allowed staged: SQLite has no statement
    // that must stand alone, so the two modes write the same text.
    let staged = lowered(drop_table(), None, PlanMode::Staged).expect("emits");
    let whole = lowered(drop_table(), None, PlanMode::Transactional).expect("emits");
    assert_eq!(sql_text(&staged), sql_text(&whole));
}

#[test]
fn every_statement_runs_in_the_plan_transaction() {
    for change in [
        create_table_change("t", &table(&[("a", col("integer"))])),
        add_column("t", "x", col("text")),
        drop_table(),
    ] {
        for statement in lowered(change, None, PlanMode::Transactional).expect("emits") {
            assert!(statement.transactional, "{}", statement.sql);
            assert!(!statement.own_batch, "{}", statement.sql);
        }
    }
}

#[test]
fn the_counts_the_scheduler_asks_for_are_the_statements_written() {
    let d = Sqlite::default();
    let mut rich = table(&[("a", col("integer")), ("b", col("integer"))]);
    rich.indexes
        .insert("i".into(), super::golden::index(&["a"], false, None));
    rich.indexes
        .insert("j".into(), super::golden::index(&["b"], false, None));
    for (what, change, expected) in [
        (
            "a table with two indexes",
            create_table_change("t", &rich),
            3,
        ),
        ("a drop", drop_table(), 1),
        ("an added column", add_column("t", "x", col("text")), 1),
    ] {
        for mode in [PlanMode::Transactional, PlanMode::Staged] {
            assert_eq!(
                d.statement_count(&change, Strategy::default(), mode),
                Ok(expected),
                "{what}"
            );
        }
        assert_eq!(
            d.intermediate_tables(&change, Strategy::default()),
            Ok(Vec::new()),
            "{what}"
        );
    }
}
