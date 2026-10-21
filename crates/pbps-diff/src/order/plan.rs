//! Emission authority belongs to order, never to a mutable draft (DEC-1959.5).

use pbps_dialect::PlanPayload;
use pbps_model::ChangeSet;
use pbps_model::resolver::OrderingProof;

mod sealed {
    pub trait Sealed {}
}

/// An immutable plan accepted by real engine emitters. Implementations are
/// sealed here so a draft or a caller-defined view cannot become executable.
///
/// ```compile_fail
/// struct Unordered(pbps_model::ChangeSet);
/// impl pbps_dialect::PlanPayload for Unordered {
///     fn changes(&self) -> &pbps_model::ChangeSet { &self.0 }
/// }
/// impl pbps_diff::order::PlanView for Unordered {}
/// ```
pub trait PlanView: PlanPayload + sealed::Sealed {}

/// A plan whose exact sequence was produced by the ordering primitive.
/// Editing a draft obtained from it discards this authority.
///
/// ```compile_fail
/// use pbps_diff::order::OrderedPlan;
/// let plan = OrderedPlan::new(todo!(), todo!());
/// ```
///
/// ```compile_fail
/// fn edit(plan: &mut pbps_diff::order::OrderedPlan) {
///     plan.changes().changes.reverse();
/// }
/// ```
#[derive(Debug, Clone)]
pub struct OrderedPlan {
    changes: ChangeSet,
    proof: OrderingProof,
}

impl OrderedPlan {
    pub(super) fn new(changes: ChangeSet, proof: OrderingProof) -> Self {
        Self { changes, proof }
    }

    pub fn changes(&self) -> &ChangeSet {
        &self.changes
    }

    pub fn proof(&self) -> &OrderingProof {
        &self.proof
    }

    pub fn into_draft(self) -> ChangeSet {
        self.changes
    }
}

impl PlanPayload for OrderedPlan {
    fn changes(&self) -> &ChangeSet {
        &self.changes
    }
}
impl sealed::Sealed for OrderedPlan {}
impl PlanView for OrderedPlan {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::{Schedule, Source};
    use pbps_model::resolver::OrderReason;
    use pbps_model::{Change, PlannedChange};

    #[test]
    fn the_certificate_keeps_the_exact_settled_sequence_and_proof() {
        let changes = ChangeSet {
            changes: ["before", "after"]
                .into_iter()
                .map(|name| {
                    PlannedChange::new(Change::CreateRole {
                        uid: "r_aaaaaa".parse().unwrap(),
                        name: name.into(),
                    })
                })
                .collect(),
        };
        let mut schedule = Schedule::new(2);
        schedule.edge(1, 0, Source::Effect, OrderReason::Structural);
        let ordered = schedule.settle(changes).unwrap().into_ordered();
        ordered.proof().validate(ordered.changes()).unwrap();
        assert_eq!(ordered.changes().changes[0].change.subject(), "role after");
        let mut draft = ordered.clone().into_draft();
        draft.changes.reverse();
        assert_ne!(&draft, ordered.changes());
        assert_eq!(ordered.proof().edges().len(), 1);
    }
}
