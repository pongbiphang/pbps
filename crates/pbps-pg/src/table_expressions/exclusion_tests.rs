//! An exclusion constraint's keys and predicate are table expressions, judged as an
//! index's are (#2448 PR D review): the add evaluates them over the stored rows as the
//! deployment role, so a routine they call must pass the same owner-trust, fixed-search-path
//! and pin checks.

use pbps_model::{
    Change, ConstraintChange, ExclusionConstraint, ExclusionKey, ExclusionTiming, IndexColumn,
    IndexMethod, OperatorRef, TableName,
};

use super::{Keyed, carries, probed};
use crate::Postgres;

fn exclusion(key: IndexColumn, filter: Option<&str>) -> ExclusionConstraint {
    ExclusionConstraint {
        method: IndexMethod::Btree,
        keys: vec![ExclusionKey {
            key,
            operator: OperatorRef::symbol("=").unwrap(),
        }],
        include: Vec::new(),
        filter: filter.map(str::to_owned),
        storage_parameters: Default::default(),
        timing: ExclusionTiming::Immediate,
    }
}

fn expression(text: &str) -> IndexColumn {
    IndexColumn {
        key: pbps_model::IndexKey::Expression(text.into()),
        descending: false,
        opclass: None,
        collation: None,
        opclass_parameters: Default::default(),
    }
}

fn set(from: Option<ExclusionConstraint>, to: Option<ExclusionConstraint>) -> Change {
    Change::Constraint(ConstraintChange::SetExclusion {
        table: TableName::new("app", "t"),
        name: "x".into(),
        from: from.map(Box::new),
        to: to.map(Box::new),
    })
}

/// Only an exclusion that holds an expression key or a predicate carries something to
/// judge, and only its add: a key of plain columns with no predicate, and either half's
/// drop, carry nothing.
#[test]
fn an_exclusion_carries_expressions_only_when_its_add_holds_a_key_expression_or_a_predicate() {
    let columns = exclusion(IndexColumn::column("a"), None);
    assert!(!carries(&set(None, Some(columns.clone()))));
    assert!(carries(&set(
        None,
        Some(exclusion(expression("app.ok(email)"), None))
    )));
    assert!(carries(&set(
        None,
        Some(exclusion(IndexColumn::column("a"), Some("app.ok(email)")))
    )));
    // Negative: the drop half holds no expression the engine would evaluate.
    assert!(!carries(&set(
        Some(exclusion(expression("app.ok(email)"), Some("a > 0"))),
        None
    )));
    // No probe evaluates an exclusion, so leaving one unjudged leaves the probes running.
    assert!(!probed(&set(
        None,
        Some(exclusion(expression("app.ok(email)"), None))
    )));
}

/// The first statement of an exclusion's add carries the change, so the executor judges
/// it before it runs; an add of plain columns carries none.
#[test]
fn the_statement_that_adds_an_exclusion_with_an_expression_introduces_it() {
    let emit = |x: ExclusionConstraint| {
        Postgres::new()
            .fixture_emit(&set(None, Some(x)), Default::default())
            .expect("emit")
    };
    let held = emit(exclusion(
        expression("app.ok(email)"),
        Some("app.ok(email)"),
    ));
    assert!(held.iter().any(|s| s.introduces.is_some()), "{held:?}");
    let plain = emit(exclusion(IndexColumn::column("a"), None));
    assert!(plain.iter().all(|s| s.introduces.is_none()), "{plain:?}");
}

/// The bound expressions are labelled as the index's are, naming the constraint.
#[test]
fn an_exclusions_key_and_predicate_are_bound_and_named_like_an_indexs() {
    let x = exclusion(expression("app.ok(email)"), Some("app.ok(email)"));
    let change = set(None, Some(x.clone()));
    let spec = super::on_table(
        &change,
        &TableName::new("app", "t"),
        Some(Keyed::of_exclusion(&"x".into(), &x)),
        None,
    );
    let labels: Vec<String> = spec.constraints().into_iter().map(|c| c.label).collect();
    assert_eq!(
        labels,
        [
            "key 1 of exclusion constraint `x` on app.t",
            "the filter of exclusion constraint `x` on app.t"
        ]
    );
    // Negative: a key of plain columns binds nothing.
    let plain = exclusion(IndexColumn::column("a"), None);
    let spec = super::on_table(
        &set(None, Some(plain.clone())),
        &TableName::new("app", "t"),
        Some(Keyed::of_exclusion(&"x".into(), &plain)),
        None,
    );
    assert!(spec.constraints().is_empty());
}
