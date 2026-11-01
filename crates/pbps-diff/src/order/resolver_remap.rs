//! Existing resolver publication compatibility, inside the ordering owner.
use pbps_model::operation::Origin;
use pbps_model::resolver::{OrderEdge, OrderReason};
use std::collections::BTreeSet;

/// Where a step the resolver ordered stands in the final order.
///
/// A step is either still there as itself, or there through its expansion:
/// `weave` splits a default edited in place into a release half and a
/// restoration half, so the one step the resolver ordered is two in the final
/// plan. Matching it by what it does ([`same_change`]) would find neither
/// half and read the split as a removal (DEC-1896.2).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Placement {
    Whole(usize),
    /// In order, at least two; the first releases, the last restores.
    Split(Vec<usize>),
}

impl Placement {
    fn first(&self) -> usize {
        match self {
            Placement::Whole(at) => *at,
            Placement::Split(parts) => parts[0],
        }
    }

    fn last(&self) -> usize {
        match self {
            Placement::Whole(at) => *at,
            Placement::Split(parts) => parts[parts.len() - 1],
        }
    }

    /// The step an edge leaves from. A binding edge runs from the release of
    /// the old binding, which is the first half. Any other edge leaves a step
    /// when it is whole, so from its last half.
    fn leaving(&self, reason: OrderReason) -> usize {
        match reason {
            OrderReason::Binding => self.first(),
            OrderReason::Structural
            | OrderReason::Identity
            | OrderReason::Data
            | OrderReason::Authorization
            | OrderReason::Restoration => self.last(),
        }
    }

    /// The step an edge arrives at. A binding edge arrives at the creation of
    /// the new binding, which is the last half. Any other edge must have the
    /// whole step wait, so it arrives at the first half.
    fn entering(&self, reason: OrderReason) -> usize {
        match reason {
            OrderReason::Binding => self.last(),
            OrderReason::Structural
            | OrderReason::Identity
            | OrderReason::Data
            | OrderReason::Authorization
            | OrderReason::Restoration => self.first(),
        }
    }
}

/// Each step the resolver ordered, in the final order the caller's passes
/// left. A step is matched through its expansion when `weave` split it
/// (below), and otherwise by what it does, in order among equal ones
/// ([`same_change`]). A pass may add changes and their findings, but not
/// remove one the resolver ordered.
///
/// The origins are what tell a split from a removal: the resolver's step is
/// `Origin::Whole(l)` for the logical change `l`, and `weave` leaves its
/// halves as `Origin::SplitPart(l)`. They count only when no step of the final
/// plan is still `Whole(l)`, and when the resolver's own plan holds no
/// `SplitPart(l)` of its own (its halves are matched as themselves). Every
/// half must still act on what the step acted on, and there must be two of
/// them: a lone leftover is not the step. Pass empty origins for a plan that
/// has none, and no step is then found through an expansion.
fn place(
    before: &pbps_model::ChangeSet,
    before_origins: &[Origin],
    after: &pbps_model::ChangeSet,
    after_origins: &[Origin],
    describe: &impl Fn(&pbps_model::Change) -> String,
) -> Result<Vec<Placement>, String> {
    let mut taken = vec![false; after.changes.len()];
    let mut placed: Vec<Option<Placement>> = vec![None; before.changes.len()];
    if before_origins.len() == before.changes.len() && after_origins.len() == after.changes.len() {
        // The expansions first: a half can equal a step of its own, which a
        // greedy match by what changes do would take for that step.
        for (position, origin) in before_origins.iter().enumerate() {
            let Origin::Whole(logical) = origin else {
                continue;
            };
            if after_origins.contains(origin)
                || before_origins.contains(&Origin::SplitPart(*logical))
            {
                continue;
            }
            let parts: Vec<usize> = (0..after.changes.len())
                .filter(|&i| !taken[i] && after_origins[i] == Origin::SplitPart(*logical))
                .collect();
            let subject = before.changes[position].change.subject();
            if parts.len() >= 2
                && parts
                    .iter()
                    .all(|&i| after.changes[i].change.subject() == subject)
            {
                for &i in &parts {
                    taken[i] = true;
                }
                placed[position] = Some(Placement::Split(parts));
            }
        }
    }
    for (position, planned) in before.changes.iter().enumerate() {
        if placed[position].is_some() {
            continue;
        }
        let Some(found) = after
            .changes
            .iter()
            .enumerate()
            .position(|(i, p)| !taken[i] && same_change(&planned.change, &p.change))
        else {
            return Err(format!(
                "a planning pass removed a change the resolver ordered: {}",
                describe(&planned.change)
            ));
        };
        taken[found] = true;
        placed[position] = Some(Placement::Whole(found));
    }
    Ok(placed.into_iter().flatten().collect())
}

/// The resolver's edges at the places [`place`] found, refusing one a pass
/// turned around.
fn carry(
    placed: &[Placement],
    after: &pbps_model::ChangeSet,
    edges: &BTreeSet<OrderEdge>,
    describe: &impl Fn(&pbps_model::Change) -> String,
) -> Result<BTreeSet<OrderEdge>, String> {
    edges
        .iter()
        .map(|edge| {
            let before_at = placed[edge.before].leaving(edge.reason);
            let after_at = placed[edge.after].entering(edge.reason);
            if before_at < after_at {
                Ok(OrderEdge {
                    before: before_at,
                    after: after_at,
                    reason: edge.reason,
                })
            } else {
                Err(format!(
                    "a planning pass put `{}` before `{}`, which the resolver orders after it",
                    describe(&after.changes[after_at].change),
                    describe(&after.changes[before_at].change)
                ))
            }
        })
        .collect()
}

/// The resolver's edges, from its own order to `after`, the order the
/// caller's passes left, for plans that carry no origins: a change is matched
/// by what it does, in order among equal ones ([`same_change`]); a pass may
/// add changes and their findings, but not remove one the resolver ordered or
/// put one before another the resolver ordered after it. [`finish`] also finds
/// a step through its expansion.
pub fn remap(
    before: &pbps_model::ChangeSet,
    after: &pbps_model::ChangeSet,
    edges: &BTreeSet<OrderEdge>,
    describe: impl Fn(&pbps_model::Change) -> String,
) -> Result<BTreeSet<OrderEdge>, String> {
    let placed = place(before, &[], after, &[], &describe)?;
    carry(&placed, after, edges, &describe)
}

/// Whether `then`, a change the resolver ordered, is `now` after the
/// connected passes. What a change does is all of it, except what the
/// connected plan reads from the target and writes into it: a dropped
/// role's members, which the resolver leaves empty (#1516 review). Every
/// other field is the change's own, and a pass that rewrote one made
/// another change.
fn same_change(then: &pbps_model::Change, now: &pbps_model::Change) -> bool {
    use pbps_model::Change::DropRole;
    if let (
        DropRole { uid, name, members },
        DropRole {
            uid: now_uid,
            name: now_name,
            ..
        },
    ) = (then, now)
    {
        return members.is_empty() && uid == now_uid && name == now_name;
    }
    then == now
}

/// Replace the final intermediate's proof with the resolver's existing
/// ordinal remap. This is the old publication check, not a constraint union
/// or a new ordering stage; the caller retains its run-bound recheck.
pub fn finish<S, T>(
    before: &super::PlanningPlan<T>,
    mut after: super::PlanningPlan<S>,
    describe: impl Fn(&pbps_model::Change) -> String,
) -> Result<super::PlanningPlan<S>, String> {
    let placed = place(
        &before.payload,
        &before.origins,
        &after.payload,
        &after.origins,
        &describe,
    )?;
    let edges = carry(&placed, &after.payload, before.proof().edges(), &describe)?;
    if let Some(identities) = &before.identities {
        // The same placement as the proof remap. A step found through its
        // expansion keeps no identity: its halves are new steps, as `weave`
        // allocated them. Scope tails and woven additions keep their actual
        // planning sources.
        let mut matched = vec![None; after.payload.changes.len()];
        for (position, placement) in placed.iter().enumerate() {
            if let Placement::Whole(found) = placement {
                matched[*found] = Some(position);
            }
        }
        after.allocator = before.allocator.clone();
        after.allocator.reserve(identities);
        let extras = after.allocator.allocate(
            matched
                .iter()
                .enumerate()
                .filter(|(_, from)| from.is_none())
                .map(|(i, _)| (after.origins[i].clone(), after.payload.changes[i].clone())),
            pbps_model::operation::ModuleEdits::InPlace,
        );
        let mut extra = extras.iter();
        let operations =
            pbps_model::operation::Operations::from_operations(matched.into_iter().map(|from| {
                from.map_or_else(
                    || {
                        extra
                            .next()
                            .expect("every unmatched operation has a fresh identity")
                            .clone()
                    },
                    |from| identities.as_slice()[from].clone(),
                )
            }))
            .with_steps(&after.payload.changes);
        after.origins = operations
            .iter()
            .map(|operation| operation.origin().clone())
            .collect();
        after.identities = Some(operations);
    }
    after.proof = pbps_model::resolver::OrderingProof::new(&after, edges)
        .map_err(|error| format!("the final order: {error}"))?;
    Ok(after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::operation::{ModuleEdits, Operations, Origin, Source};
    use pbps_model::{Change, ChangeSet, PlannedChange};
    #[test]
    fn scope_tails_and_woven_additions_keep_resolver_ids_without_collisions() {
        let column = pbps_model::TableName::new("app", "t").column("n");
        let default = |value: &str| {
            PlannedChange::new(Change::AlterColumnDefault {
                uid: "c_aaaaaa".parse().unwrap(),
                column: column.clone(),
                from: None,
                to: Some(value.into()),
            })
        };
        let origin = Origin::Restoration(Source::Rebinding(
            pbps_model::resolver::Surface::Default(column.clone()),
        ));
        let mut before = crate::order::propose(ChangeSet {
            changes: vec![default("1")],
        });
        before.logical = 0;
        before.origins = vec![origin.clone()];
        before.identities = Some(Operations::expanded(
            0,
            [(origin.clone(), default("1"))],
            ModuleEdits::Rebuilt,
        ));
        let before = crate::order::plan::canonical(before, &pbps_dialect::MinimalDialect).unwrap();
        let scope = PlannedChange::new(Change::SetDataMode {
            table: column.table.clone(),
            from: None,
            to: Some(pbps_model::data::DataMode::Exact),
        });
        let mut after = crate::order::writebacks::ScopeInput::new(vec![scope])
            .unwrap()
            .append_to(before.clone(), &pbps_dialect::MinimalDialect);
        after.payload.changes.insert(0, default("2"));
        after.origins.insert(0, origin.clone());
        after.invalidate();
        let after = finish(&before, after, |change| change.subject()).unwrap();
        let operations = after.identities.as_ref().unwrap();
        let ids: Vec<_> = operations
            .iter()
            .map(|operation| operation.id().to_string())
            .collect();
        assert_eq!(ids, ["0.1", "0.0", "1.0"]);
        assert_eq!(
            operations
                .iter()
                .map(|op| op.id())
                .collect::<BTreeSet<_>>()
                .len(),
            3
        );
        assert_eq!(
            operations.as_slice()[1].id(),
            before.identities.as_ref().unwrap().as_slice()[0].id()
        );
        assert_eq!(operations.as_slice()[2].origin(), &Origin::Whole(0));
        assert_eq!(
            operations.iter().map(|op| op.planned()).collect::<Vec<_>>(),
            after.payload.changes.iter().collect::<Vec<_>>()
        );
    }

    /// A partition's default, which `weave` splits into a release and a
    /// restoration when the function it calls is dropped (#1896).
    fn edit(from: Option<&str>, to: Option<&str>, fallback: Option<&str>) -> PlannedChange {
        PlannedChange::new(Change::SetPartitionDefault {
            uid: "t_aaaaaa".parse().unwrap(),
            table: pbps_model::TableName::new("app", "p"),
            parent: pbps_model::TableName::new("app", "tree"),
            column: pbps_model::ColumnName::from("d"),
            from: from.map(Into::into),
            to: to.map(Into::into),
            fallback: fallback.map(Into::into),
        })
    }

    fn role(name: &str) -> PlannedChange {
        PlannedChange::new(Change::CreateRole {
            uid: pbps_model::Uid::generate(pbps_model::UidKind::Role),
            name: name.into(),
        })
    }

    fn edge(before: usize, after: usize, reason: OrderReason) -> OrderEdge {
        OrderEdge {
            before,
            after,
            reason,
        }
    }

    /// The edges `edges` become over a plan whose steps carry `origins`.
    fn carried(
        before: (&[PlannedChange], &[Origin]),
        after: (&[PlannedChange], &[Origin]),
        edges: &[OrderEdge],
    ) -> Result<BTreeSet<OrderEdge>, String> {
        let set = |changes: &[PlannedChange]| ChangeSet {
            changes: changes.to_vec(),
        };
        let (before_set, after_set) = (set(before.0), set(after.0));
        let describe = |change: &Change| change.subject();
        let placed = place(&before_set, before.1, &after_set, after.1, &describe)?;
        carry(
            &placed,
            &after_set,
            &edges.iter().cloned().collect(),
            &describe,
        )
    }

    /// The resolver's one edit of the default, `Whole(1)`, and what `weave`
    /// leaves of it: its release, then its restoration, `SplitPart(1)`.
    fn split() -> (PlannedChange, PlannedChange, PlannedChange) {
        (
            edit(Some("app.f(1)"), Some("3"), Some("4")),
            edit(Some("app.f(1)"), None, None),
            edit(None, Some("3"), Some("4")),
        )
    }

    #[test]
    fn a_split_step_takes_old_binding_edges_at_its_release_and_new_binding_edges_at_its_restoration()
     {
        let (whole, release, restoration) = split();
        let (input, drop) = (role("input"), role("drop"));
        // The binding edges the resolver states: the new binding's input is
        // made before the edit creates it, and the edit releases the old
        // binding before the drop invalidates it.
        let before = [input.clone(), whole, drop.clone()];
        let before_origins = [Origin::Whole(0), Origin::Whole(1), Origin::Whole(2)];
        // The drop sits between the halves: the release is ahead of it and
        // the restoration, which no longer reads the function, after.
        let after = [input, release, drop, restoration];
        let after_origins = [
            Origin::Whole(0),
            Origin::SplitPart(1),
            Origin::Whole(2),
            Origin::SplitPart(1),
        ];
        let edges = carried(
            (&before, &before_origins),
            (&after, &after_origins),
            &[
                edge(0, 1, OrderReason::Binding),
                edge(1, 2, OrderReason::Binding),
            ],
        )
        .unwrap();
        assert_eq!(
            edges,
            BTreeSet::from([
                edge(0, 3, OrderReason::Binding),
                edge(1, 2, OrderReason::Binding),
            ])
        );
    }

    #[test]
    fn a_split_step_takes_whole_step_edges_into_its_first_half_and_out_of_its_last() {
        let (whole, release, restoration) = split();
        let (before_x, after_x, between) = (role("before"), role("after"), role("between"));
        let before = [before_x.clone(), whole, after_x.clone()];
        let before_origins = [Origin::Whole(0), Origin::Whole(1), Origin::Whole(2)];
        let after = [before_x, release, between, restoration, after_x];
        let after_origins = [
            Origin::Whole(0),
            Origin::SplitPart(1),
            Origin::Whole(3),
            Origin::SplitPart(1),
            Origin::Whole(2),
        ];
        for reason in [
            OrderReason::Structural,
            OrderReason::Identity,
            OrderReason::Data,
            OrderReason::Authorization,
            OrderReason::Restoration,
        ] {
            let edges = carried(
                (&before, &before_origins),
                (&after, &after_origins),
                &[edge(0, 1, reason), edge(1, 2, reason)],
            )
            .unwrap();
            assert_eq!(
                edges,
                BTreeSet::from([edge(0, 1, reason), edge(3, 4, reason)]),
                "{reason:?}"
            );
        }
    }

    #[test]
    fn a_split_step_does_not_take_the_place_of_an_equal_step_of_its_own() {
        // The resolver's teardown of the same default equals the release half;
        // each is found at its own place, not the first equal one.
        let (whole, release, restoration) = split();
        let teardown_origin = Origin::Teardown(pbps_model::operation::Source::Change(5));
        let before = [whole, release.clone()];
        let before_origins = [Origin::Whole(1), teardown_origin.clone()];
        let after = [release.clone(), restoration, release];
        let after_origins = [Origin::SplitPart(1), Origin::SplitPart(1), teardown_origin];
        let edges = carried(
            (&before, &before_origins),
            (&after, &after_origins),
            &[edge(0, 1, OrderReason::Identity)],
        )
        .unwrap();
        assert_eq!(edges, BTreeSet::from([edge(1, 2, OrderReason::Identity)]));
    }

    #[test]
    fn a_change_that_is_gone_is_refused_whether_or_not_it_could_have_split() {
        let (whole, release, restoration) = split();
        let other = role("other");
        let before = [whole.clone(), other.clone()];
        let before_origins = [Origin::Whole(1), Origin::Whole(0)];
        // Negative: nothing is left of the step.
        let gone = carried(
            (&before, &before_origins),
            (std::slice::from_ref(&other), &[Origin::Whole(0)]),
            &[],
        );
        assert!(gone.unwrap_err().contains("removed"));
        // Negative: one half is not the step; a lone release is a removal.
        let lone = carried(
            (&before, &before_origins),
            (
                &[release.clone(), other.clone()],
                &[Origin::SplitPart(1), Origin::Whole(0)],
            ),
            &[],
        );
        assert!(lone.unwrap_err().contains("removed"));
        // Negative: halves of another object are not this step's.
        let elsewhere = PlannedChange::new(Change::SetPartitionDefault {
            uid: "t_bbbbbb".parse().unwrap(),
            table: pbps_model::TableName::new("app", "q"),
            parent: pbps_model::TableName::new("app", "tree"),
            column: pbps_model::ColumnName::from("d"),
            from: Some("app.f(1)".into()),
            to: None,
            fallback: None,
        });
        let stranger = carried(
            (&before, &before_origins),
            (
                &[elsewhere.clone(), elsewhere, other.clone()],
                &[Origin::SplitPart(1), Origin::SplitPart(1), Origin::Whole(0)],
            ),
            &[],
        );
        assert!(stranger.unwrap_err().contains("removed"));
        // Negative: a step still present whole is not also found in halves
        // left by something else.
        let whole_and_halves = carried(
            (&before, &before_origins),
            (
                &[whole, release, restoration, other],
                &[
                    Origin::Whole(1),
                    Origin::SplitPart(1),
                    Origin::SplitPart(1),
                    Origin::Whole(0),
                ],
            ),
            &[edge(0, 1, OrderReason::Identity)],
        )
        .unwrap();
        assert_eq!(
            whole_and_halves,
            BTreeSet::from([edge(0, 3, OrderReason::Identity)])
        );
    }

    #[test]
    fn a_pass_that_turns_a_split_steps_edge_around_is_refused() {
        let (whole, release, restoration) = split();
        let later = role("later");
        let before = [whole, later.clone()];
        let before_origins = [Origin::Whole(1), Origin::Whole(0)];
        // The restoration lands behind the step the whole edit was ordered
        // ahead of: the edit leaves its last half, which is now too late.
        let after = [release, later, restoration];
        let after_origins = [Origin::SplitPart(1), Origin::Whole(0), Origin::SplitPart(1)];
        let refused = carried(
            (&before, &before_origins),
            (&after, &after_origins),
            &[edge(0, 1, OrderReason::Identity)],
        );
        assert!(
            refused.as_ref().is_err_and(|why| why.contains("before")),
            "{refused:?}"
        );
        // Negative: the same plan is fine for the edge that concerns only the
        // old binding, which the release half keeps.
        assert_eq!(
            carried(
                (&before, &before_origins),
                (&after, &after_origins),
                &[edge(0, 1, OrderReason::Binding)],
            )
            .unwrap(),
            BTreeSet::from([edge(0, 1, OrderReason::Binding)])
        );
    }
}
