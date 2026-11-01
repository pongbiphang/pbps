//! One hard projection from declared references to operation boundaries.
//! The index describes immutable groups; it supplies no ordering preference.
mod contract;
pub use contract::Contract;
pub(crate) use contract::Creation;

use std::collections::{BTreeMap, BTreeSet};

use pbps_model::object_ref::{ObjectDeps, ObjectRef, Root, Selector};
use pbps_model::operation::{Operation, Transition};
use pbps_model::{Change, ModuleDeps};

use super::{Constraint, Source};
use pbps_model::resolver::OrderReason;

/// Both ends use the same reference and boundary vocabulary. Keeping these
/// separate prevents a removed baseline annotation from gating a final install.
#[derive(Debug, Clone)]
pub struct Annotation<R> {
    pub owner: ObjectRef<R>,
    pub provider: ObjectRef<R>,
}

#[derive(Debug, Clone, Default)]
pub struct Boundary {
    pub installs: BTreeSet<usize>,
    pub removals: BTreeSet<usize>,
}

/// An operation may install many surfaces at one indivisible boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionError {
    Missing(usize),
    InternalCycle(usize),
}

#[derive(Debug, Clone)]
pub struct Index<R: Ord> {
    pub boundaries: BTreeMap<ObjectRef<R>, Boundary>,
    pub standing: BTreeSet<ObjectRef<R>>,
    pub containment: BTreeSet<(ObjectRef<R>, ObjectRef<R>)>,
    /// Availability proven by engine metadata, independently of boundaries.
    pub continuous: BTreeSet<(ObjectRef<R>, usize)>,
}
impl<R: Ord> Default for Index<R> {
    fn default() -> Self {
        Self {
            boundaries: BTreeMap::new(),
            standing: BTreeSet::new(),
            containment: BTreeSet::new(),
            continuous: BTreeSet::new(),
        }
    }
}
impl<R: Ord + Clone> Index<R> {
    pub fn record(&mut self, reference: ObjectRef<R>, at: usize, transition: Transition) {
        let boundary = self.boundaries.entry(reference).or_default();
        match transition {
            Transition::Produces => boundary.installs.insert(at),
            Transition::Ends => boundary.removals.insert(at),
        };
    }

    /// Projection is independent of object kind. Every emitted edge is hard;
    /// inference can yield only through the existing Guess SCC policy.
    pub fn project(
        &self,
        base: &[Annotation<R>],
        goal: &[Annotation<R>],
    ) -> Result<Vec<Constraint>, ProjectionError> {
        let mut edges = BTreeSet::new();
        for (reverse, annotations) in [(true, base), (false, goal)] {
            for (number, annotation) in annotations.iter().enumerate() {
                let owner = self.boundaries.get(&annotation.owner);
                let provider = self.boundaries.get(&annotation.provider);
                if !reverse && owner.is_none() && !self.standing.contains(&annotation.owner) {
                    return Err(ProjectionError::Missing(number));
                }
                let Some(owners) = owner.map(|b| if reverse { &b.removals } else { &b.installs })
                else {
                    continue; // An annotation edit alone changes no DDL.
                };
                let providers = provider.map(|b| if reverse { &b.removals } else { &b.installs });
                if !reverse
                    && !owners.is_empty()
                    && providers.is_none_or(BTreeSet::is_empty)
                    && !(self.standing.contains(&annotation.provider)
                        && provider.is_none_or(|b| b.removals.is_empty()))
                {
                    return Err(ProjectionError::Missing(number));
                }
                for &owner in owners {
                    for &provider in providers.into_iter().flatten() {
                        if reverse
                            && self
                                .continuous
                                .contains(&(annotation.provider.clone(), provider))
                        {
                            continue; // Availability survives an indivisible in-place replacement.
                        }
                        if !reverse
                            && owner == provider
                            && self
                                .containment
                                .contains(&(annotation.owner.clone(), annotation.provider.clone()))
                        {
                            return Err(ProjectionError::InternalCycle(number));
                        }
                        if owner != provider {
                            let (before, after) = if reverse {
                                (owner, provider)
                            } else {
                                (provider, owner)
                            };
                            edges.insert(Constraint {
                                before,
                                after,
                                source: Source::Annotation,
                                reason: OrderReason::Structural,
                            });
                        }
                    }
                }
            }
        }
        Ok(edges.into_iter().collect())
    }
}

fn module(id: &pbps_model::ModuleId) -> ObjectRef {
    ObjectRef {
        root: Root::Module(id.clone()),
        part: Selector::Whole,
    }
}
fn intents(dependencies: &ModuleDeps) -> Vec<Annotation<Root>> {
    ObjectDeps::from_legacy(dependencies)
        .0
        .into_iter()
        .flat_map(|(owner, providers)| {
            providers.into_iter().map(move |provider| Annotation {
                owner: owner.clone(),
                provider,
            })
        })
        .collect()
}

/// Compatibility adapter: only enumeration differs; projection has one
/// authority for legacy modules and the extended reference contract.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn legacy(operations: &[Operation], dependencies: &ModuleDeps) -> Vec<Constraint> {
    let mut index = Index::default();
    for (at, op) in operations.iter().enumerate() {
        match op.change() {
            Change::CreateModule { id, .. } => index.record(module(id), at, Transition::Produces),
            Change::AlterModule { id, .. } => index.record(module(id), at, Transition::Produces),
            Change::DropModule { id, .. } => index.record(module(id), at, Transition::Ends),
            _ => {}
        }
    }
    let annotations = intents(dependencies);
    // A module with no operation can be a standing baseline provider. Full
    // declaration validation belongs to the caller, as before (DEC-2469.1).
    for annotation in &annotations {
        index.standing.insert(annotation.provider.clone());
        index.standing.insert(annotation.owner.clone());
    }
    index
        .project(&annotations, &annotations)
        .expect("legacy providers are indexed or standing")
}

/// The generated expander's module list is a legacy input enumeration, not a
/// separate annotation ordering rule.
pub(crate) fn modules<'a>(
    modules: impl IntoIterator<Item = (usize, &'a pbps_model::ModuleId)>,
    dependencies: &ModuleDeps,
) -> Vec<Constraint> {
    let mut index = Index::default();
    for (at, id) in modules {
        index.record(module(id), at, Transition::Produces);
    }
    let annotations = intents(dependencies);
    index.standing.extend(
        annotations
            .iter()
            .flat_map(|a| [a.provider.clone(), a.owner.clone()]),
    );
    index
        .project(&[], &annotations)
        .expect("legacy validation supplies standing providers")
}

#[cfg(test)]
mod tests;
