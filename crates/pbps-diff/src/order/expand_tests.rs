//! The rule driver over a toy rule (#1958): every role the plan creates needs
//! a drop of the same name ahead of it.

use pbps_model::resolver::OrderReason;
use pbps_model::{AddedRole, Change, ChangeSet, PlannedChange};

use super::edges::Source;
use super::expand::{
    Added, Addition, Anchor, Expansion, FACTOR, Order, Rewrite, Step, expand as current_expand,
};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Adds the drop the obligation names.
    Meets,
    /// Adds a drop of another name.
    Misses,
    /// Answers with nothing.
    Idles,
}

struct Toy(Mode);

/// The toy rule, whose final order puts each create ahead of every drop.
struct Undoes;

fn create(name: &str) -> PlannedChange {
    PlannedChange::new(Change::CreateRole {
        uid: "r_aaaaaa".parse().unwrap(),
        name: name.into(),
    })
}

fn drop(name: &str) -> PlannedChange {
    PlannedChange::new(Change::DropRole {
        uid: "r_aaaaaa".parse().unwrap(),
        name: name.into(),
        members: Vec::new(),
    })
}

fn name(p: &PlannedChange) -> Option<(&str, bool)> {
    if let Change::CreateRole { name, .. } = &p.change {
        Some((name, true))
    } else if let Change::DropRole { name, .. } = &p.change {
        Some((name, false))
    } else {
        None
    }
}

impl Expansion for Toy {
    type Obligation = String;

    fn rule(&self) -> &'static str {
        "toy"
    }

    fn unmet(&self, plan: &[PlannedChange]) -> Result<Vec<String>, String> {
        Ok(plan
            .iter()
            .enumerate()
            .filter_map(|(i, p)| match name(p) {
                Some((n, true)) if !plan[..i].iter().any(|q| name(q) == Some((n, false))) => {
                    Some(n.to_owned())
                }
                _ => None,
            })
            .collect())
    }

    fn expand(&self, plan: &[PlannedChange], obligation: &String) -> Result<Rewrite, String> {
        let at = plan
            .iter()
            .position(|p| name(p) == Some((obligation, true)))
            .unwrap();
        let dropped = match self.0 {
            Mode::Meets => drop(obligation),
            Mode::Misses => drop("other"),
            Mode::Idles => {
                return Ok(Rewrite::Add {
                    added: Vec::new(),
                    respelled: Vec::new(),
                    edges: Vec::new(),
                });
            }
        };
        Ok(Rewrite::Add {
            added: vec![Added::Teardown(Addition {
                of: at,
                change: dropped,
                // After, so only the edge can put it ahead.
                anchor: Anchor::After(at),
                reason: "a role is created over its old self".into(),
            })],
            respelled: Vec::new(),
            edges: vec![Order {
                before: Step::Added(0),
                after: Step::Plan(at),
                source: Source::Catalog,
                reason: OrderReason::Structural,
            }],
        })
    }

    fn order(&self, _: &[PlannedChange]) -> Result<Vec<Order>, String> {
        Ok(Vec::new())
    }
}

fn plan(names: &[&str]) -> ChangeSet {
    ChangeSet {
        changes: names.iter().map(|n| create(n)).collect(),
    }
}

#[test]
fn an_added_step_goes_where_its_edges_put_it_and_reports_its_origin() {
    let done = expand(plan(&["a", "b"]), &Toy(Mode::Meets), FACTOR).unwrap();
    let order: Vec<_> = done.changes.changes.iter().filter_map(name).collect();
    assert_eq!(
        order,
        [("a", false), ("a", true), ("b", false), ("b", true)],
        "settle, not the anchor, places the drop"
    );
    assert_eq!(done.rewrites, 2);
    let origin = done.changes.changes[0].provenance.as_ref().unwrap();
    assert_eq!(origin.rule, "toy");
    assert_eq!(origin.role, AddedRole::Teardown);
    assert_eq!(origin.source, "role a");
    assert!(
        done.changes.changes[1].provenance.is_none(),
        "the plan's own step has no added origin"
    );
}

#[test]
fn a_plan_with_nothing_unmet_is_returned_as_it_was() {
    let given = ChangeSet {
        changes: vec![drop("a"), create("a")],
    };
    let done = expand(given.clone(), &Toy(Mode::Meets), FACTOR).unwrap();
    assert_eq!(done.changes, given);
    assert_eq!(done.rewrites, 0);
}

#[test]
fn a_rewrite_that_leaves_its_obligation_unmet_is_refused() {
    let err = expand(plan(&["a"]), &Toy(Mode::Misses), FACTOR).unwrap_err();
    assert!(err.contains("toy: a rewrite for a left it unmet"), "{err}");
}

#[test]
fn a_rewrite_that_adds_nothing_ends_the_loop_naming_what_remains() {
    let err = expand(plan(&["a", "b"]), &Toy(Mode::Idles), FACTOR).unwrap_err();
    assert_eq!(err, "toy: nothing meets a; b");
}

#[test]
fn a_budget_below_what_the_plan_needs_refuses_it_naming_the_budget_and_the_size() {
    // Two creates and two obligations: four operations asked for, so a
    // factor of zero leaves no rewrite and the honest rule cannot finish.
    let err = expand(plan(&["a", "b"]), &Toy(Mode::Meets), 0).unwrap_err();
    assert!(
        err.contains("the rewrite budget of 0 (0 per operation, 4 asked for)"),
        "{err}"
    );
    assert!(err.contains("2 remain unmet: a; b"), "{err}");
    // And the same plan within the budget is not refused.
    assert!(expand(plan(&["a", "b"]), &Toy(Mode::Meets), 1).is_ok());
}

/// A rule that splits the whole step named `split`.
struct Splitter;

impl Expansion for Splitter {
    type Obligation = usize;

    fn rule(&self) -> &'static str {
        "splitter"
    }

    fn unmet(&self, plan: &[PlannedChange]) -> Result<Vec<usize>, String> {
        Ok(plan
            .iter()
            .position(|p| name(p) == Some(("split", true)))
            .into_iter()
            .collect())
    }

    fn expand(&self, _: &[PlannedChange], at: &usize) -> Result<Rewrite, String> {
        Ok(Rewrite::Split {
            at: *at,
            parts: Box::new([drop("x"), create("y")]),
            tail: Vec::new(),
            reason: "edited in one step".into(),
        })
    }

    fn order(&self, _: &[PlannedChange]) -> Result<Vec<Order>, String> {
        Ok(Vec::new())
    }
}

#[test]
fn a_split_keeps_its_logical_change_as_two_parts_in_its_place() {
    let done = expand(plan(&["a", "split", "b"]), &Splitter, FACTOR).unwrap();
    let order: Vec<_> = done.changes.changes.iter().filter_map(name).collect();
    assert_eq!(order, [("a", true), ("x", false), ("y", true), ("b", true)]);
    for part in &done.changes.changes[1..3] {
        let origin = part.provenance.as_ref().unwrap();
        assert_eq!(origin.role, AddedRole::SplitPart);
        assert_eq!(origin.source, "role split");
    }
}

/// A rule that splits a step it added, which is not a whole step.
struct SplitsItsOwn;

impl Expansion for SplitsItsOwn {
    type Obligation = &'static str;

    fn rule(&self) -> &'static str {
        "own"
    }

    fn unmet(&self, plan: &[PlannedChange]) -> Result<Vec<&'static str>, String> {
        Ok(match plan.len() {
            1 => vec!["teardown"],
            2 => vec!["split"],
            _ => Vec::new(),
        })
    }

    fn expand(&self, _: &[PlannedChange], step: &&'static str) -> Result<Rewrite, String> {
        Ok(if *step == "teardown" {
            Toy(Mode::Meets).expand(&[create("a")], &"a".to_owned())?
        } else {
            Rewrite::Split {
                at: 0,
                parts: Box::new([drop("x"), drop("y")]),
                tail: Vec::new(),
                reason: String::new(),
            }
        })
    }

    fn order(&self, _: &[PlannedChange]) -> Result<Vec<Order>, String> {
        Ok(Vec::new())
    }
}

#[test]
fn only_a_whole_step_can_be_split() {
    let err = expand(plan(&["a"]), &SplitsItsOwn, FACTOR).unwrap_err();
    assert_eq!(err, "own: only a whole step splits, not step 0");
}

impl Expansion for Undoes {
    type Obligation = String;

    fn rule(&self) -> &'static str {
        "undoes"
    }

    fn unmet(&self, plan: &[PlannedChange]) -> Result<Vec<String>, String> {
        Toy(Mode::Meets).unmet(plan)
    }

    fn expand(&self, plan: &[PlannedChange], obligation: &String) -> Result<Rewrite, String> {
        Toy(Mode::Meets).expand(plan, obligation)
    }

    fn order(&self, plan: &[PlannedChange]) -> Result<Vec<Order>, String> {
        let creates = plan
            .iter()
            .enumerate()
            .filter(|(_, p)| name(p).is_some_and(|n| n.1));
        let drops: Vec<usize> = plan
            .iter()
            .enumerate()
            .filter(|(_, p)| name(p).is_some_and(|n| !n.1))
            .map(|(i, _)| i)
            .collect();
        Ok(creates
            .flat_map(|(c, _)| {
                drops.iter().map(move |&d| Order {
                    before: Step::Plan(c),
                    after: Step::Plan(d),
                    source: Source::Catalog,
                    reason: OrderReason::Structural,
                })
            })
            .collect())
    }
}

#[test]
fn a_final_order_that_unmeets_an_obligation_is_refused() {
    let err = expand(plan(&["a"]), &Undoes, FACTOR).unwrap_err();
    assert_eq!(err, "undoes: the final order leaves 1 unmet: a");
}

// Every existing driver pin, including refusals, exercises the frozen driver.
use crate::order;
#[path = "../../tests/support/expansion.rs"]
mod legacy;

fn expand<E: Expansion>(
    plan: ChangeSet,
    rule: &E,
    factor: usize,
) -> Result<super::expand::Expanded, String> {
    let old = legacy::expand(plan.clone(), rule, factor);
    let now = current_expand(plan, rule, factor);
    match (&now, &old) {
        (Ok(a), Ok(b)) => {
            assert_eq!(a.rewrites, b.rewrites);
            assert_eq!(a.changes, b.changes);
            for (a, b) in a.changes.changes.iter().zip(&b.changes.changes) {
                assert_eq!(a.provenance, b.provenance);
            }
        }
        (Err(a), Err(b)) => assert_eq!(a, b),
        _ => panic!("driver disagreement: {now:?} / {old:?}"),
    }
    now
}

struct TailSplitter;

impl Expansion for TailSplitter {
    type Obligation = usize;

    fn rule(&self) -> &'static str {
        "tail-splitter"
    }

    fn unmet(&self, plan: &[PlannedChange]) -> Result<Vec<usize>, String> {
        Splitter.unmet(plan)
    }

    fn expand(&self, _: &[PlannedChange], at: &usize) -> Result<Rewrite, String> {
        Ok(Rewrite::Split {
            at: *at,
            parts: Box::new([drop("x"), create("y")]),
            tail: vec![create("z"), drop("w")],
            reason: "all parts serve the same logical change".into(),
        })
    }

    fn order(&self, _: &[PlannedChange]) -> Result<Vec<Order>, String> {
        Ok(Vec::new())
    }
}

#[test]
fn every_tail_part_keeps_the_source_and_other_whole_steps_keep_their_order() {
    let done = current_expand(plan(&["a", "split", "b"]), &TailSplitter, FACTOR).unwrap();
    assert_eq!(done.rewrites, 1);
    assert_eq!(
        done.changes
            .changes
            .iter()
            .filter_map(name)
            .collect::<Vec<_>>(),
        [
            ("a", true),
            ("x", false),
            ("y", true),
            ("z", true),
            ("w", false),
            ("b", true)
        ]
    );
    for part in &done.changes.changes[1..5] {
        let origin = part.provenance.as_ref().unwrap();
        assert_eq!(origin.rule, "tail-splitter");
        assert_eq!(origin.role, AddedRole::SplitPart);
        assert_eq!(origin.source, "role split");
        assert_eq!(origin.reason, "all parts serve the same logical change");
    }
    assert!(done.changes.changes[0].provenance.is_none());
    assert!(done.changes.changes[5].provenance.is_none());
    let again = current_expand(done.changes.clone(), &TailSplitter, FACTOR).unwrap();
    assert_eq!(again.rewrites, 0);
    assert_eq!(again.changes, done.changes);
}

#[test]
fn a_tail_split_does_not_escape_the_existing_budget() {
    let err = current_expand(plan(&["split"]), &TailSplitter, 0).unwrap_err();
    assert_eq!(
        err,
        "tail-splitter: the rewrite budget of 0 (0 per operation, 2 asked for) is spent and 1 remain unmet: 0"
    );
}

struct NonProgressingTail;

impl Expansion for NonProgressingTail {
    type Obligation = usize;
    fn rule(&self) -> &'static str {
        "stuck-tail"
    }
    fn unmet(&self, plan: &[PlannedChange]) -> Result<Vec<usize>, String> {
        Splitter.unmet(plan)
    }
    fn expand(&self, plan: &[PlannedChange], at: &usize) -> Result<Rewrite, String> {
        let mut rewrite = TailSplitter.expand(plan, at)?;
        if let Rewrite::Split { parts, .. } = &mut rewrite {
            parts[0] = create("split");
        }
        Ok(rewrite)
    }
    fn order(&self, _: &[PlannedChange]) -> Result<Vec<Order>, String> {
        Ok(Vec::new())
    }
}

#[test]
fn a_tail_cannot_hide_an_unmet_split_obligation() {
    let err = current_expand(plan(&["split"]), &NonProgressingTail, FACTOR).unwrap_err();
    assert_eq!(err, "stuck-tail: a rewrite for 0 left it unmet");
}
