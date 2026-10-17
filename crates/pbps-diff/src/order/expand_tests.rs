//! The rule driver over a toy rule (#1958): every role the plan creates needs
//! a drop of the same name ahead of it.

use pbps_model::resolver::OrderReason;
use pbps_model::{AddedRole, Change, ChangeSet, PlannedChange};

use super::edges::Source;
use super::expand::{Added, Addition, Anchor, Expansion, FACTOR, Order, Rewrite, Step, expand};

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
