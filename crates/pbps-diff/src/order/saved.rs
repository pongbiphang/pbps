//! Checked saved artifacts keep their reviewed order; admission does not sort
//! and never pretends to have produced an ordering proof (DEC-1959.5).

use super::{OrderedPlan, PlanView};
use pbps_dialect::{Dialect, PlanPayload};
use pbps_model::{ChangeSet, SavedPlan};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct AdmissionError(#[source] Box<dyn std::error::Error + Send + Sync>);

impl AdmissionError {
    fn from_error(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self(Box::new(error))
    }
    fn message(message: String) -> Self {
        #[derive(Debug, thiserror::Error)]
        #[error("{0}")]
        struct Message(String);
        Self::from_error(Message(message))
    }
}

macro_rules! refuse {
    ($($arg:tt)*) => { return Err(AdmissionError::message(format!($($arg)*))) };
}

/// A reviewed wire artifact accepted by the existing artifact checks. Its
/// checksum-pinned sequence is trusted under SPEC's saved-plan contract;
/// admission neither reorders it nor enables the deferred release verifier.
#[derive(Debug, Clone)]
pub struct AdmittedPlan {
    wire: SavedPlan,
}

impl AdmittedPlan {
    pub fn wire(&self) -> &SavedPlan {
        &self.wire
    }
}
impl std::ops::Deref for AdmittedPlan {
    type Target = SavedPlan;
    fn deref(&self) -> &SavedPlan {
        &self.wire
    }
}
impl PlanPayload for AdmittedPlan {
    fn changes(&self) -> &ChangeSet {
        &self.wire.changes
    }
}
impl super::plan::sealed::Sealed for AdmittedPlan {}
impl PlanView for AdmittedPlan {}

/// Accepts only checked wire bytes, never granting `OrderedPlan` authority.
pub fn admit(
    plan: SavedPlan,
    dialect: &(impl Dialect + ?Sized),
) -> Result<AdmittedPlan, AdmissionError> {
    validate_saved_plan(&plan, dialect)?;
    Ok(AdmittedPlan { wire: plan })
}

/// A newly produced artifact, bound to the exact plan that ordering produced.
/// Metadata remains the existing wire DTO, with no capability/proof fields.
#[derive(Debug, Clone)]
pub struct SealedPlan {
    wire: SavedPlan,
    ordered: OrderedPlan,
}
impl SealedPlan {
    pub fn wire(&self) -> &SavedPlan {
        &self.wire
    }
    pub fn ordered(&self) -> &OrderedPlan {
        &self.ordered
    }
}
impl std::ops::Deref for SealedPlan {
    type Target = SavedPlan;
    fn deref(&self) -> &SavedPlan {
        &self.wire
    }
}

/// Metadata cannot replace a single operation, strategy or finding after
/// ordering: the original proof must still describe the wire payload exactly.
pub fn seal(
    ordered: OrderedPlan,
    wire: SavedPlan,
) -> Result<SealedPlan, pbps_model::resolver::OrderError> {
    ordered.proof().validate(&wire.changes)?;
    Ok(SealedPlan { wire, ordered })
}

pub fn logical_changes(cs: &pbps_model::ChangeSet) -> usize {
    let mut built = std::collections::BTreeSet::new();
    for planned in &cs.changes {
        // `if let` rather than a match with a wildcard: no other change
        // brings a routine into being, so there is no list to enumerate and
        // none to keep up to date.
        let id = if let pbps_model::Change::CreateModule { id, .. } = &planned.change {
            id
        } else if let pbps_model::Change::AlterModule { id, .. } = &planned.change {
            id
        } else {
            continue;
        };
        if let pbps_model::ModuleId::Routine(routine) = id {
            built.insert(routine);
        }
    }
    cs.changes
        .iter()
        .filter(|planned| {
            !matches!(
                &planned.change,
                pbps_model::Change::PublicExecution { routine, .. } if built.contains(routine)
            )
        })
        .count()
}

fn public_execution_decisions_match_their_modules(
    cs: &pbps_model::ChangeSet,
    dialect: &(impl Dialect + ?Sized),
) -> Result<(), AdmissionError> {
    use pbps_model::{Change, ModuleId, RoutineOrigin};

    let routine_of = |change: &Change| -> Option<(u8, pbps_model::RoutineId)> {
        // `if let` rather than a match with a wildcard: these are the three
        // changes that decide whether an object is there afterwards, and a
        // list of all the rest would need maintaining for nothing.
        let (rank, id) = if let Change::CreateModule { id, .. } = change {
            (0u8, id)
        } else if let Change::AlterModule { id, .. } = change {
            (1, id)
        } else if let Change::DropModule { id, .. } = change {
            (2, id)
        } else {
            return None;
        };
        match id {
            ModuleId::Routine(routine) => Some((rank, routine.clone())),
            ModuleId::Named(_) | ModuleId::Trigger { .. } => None,
        }
    };

    let mut created = std::collections::BTreeSet::new();
    let mut altered = std::collections::BTreeSet::new();
    let mut dropped = std::collections::BTreeSet::new();
    for planned in &cs.changes {
        match routine_of(&planned.change) {
            Some((0, routine)) => created.insert(routine),
            Some((1, routine)) => altered.insert(routine),
            Some((_, routine)) => dropped.insert(routine),
            None => continue,
        };
    }
    // A create whose identity this plan also drops is a changed kind, which
    // is a rebuild however the two statements are spelled.
    let origin_of = |routine: &pbps_model::RoutineId| -> Option<RoutineOrigin> {
        if altered.contains(routine) || (created.contains(routine) && dropped.contains(routine)) {
            Some(RoutineOrigin::Rebuilt)
        } else if created.contains(routine) {
            Some(RoutineOrigin::Created)
        } else {
            None
        }
    };

    let mut decided: std::collections::BTreeMap<&pbps_model::RoutineId, usize> =
        std::collections::BTreeMap::new();
    for (index, planned) in cs.changes.iter().enumerate() {
        let Change::PublicExecution {
            routine, origin, ..
        } = &planned.change
        else {
            continue;
        };
        if let Some(first) = decided.insert(routine, index) {
            refuse!(
                "changes {} and {} both decide `PUBLIC` execution on `{}`; a plan settles \
                 that once.\n\
                 The artifact was edited or produced by a broken planner; regenerate it with \
                 `pbps plan --db`.",
                first + 1,
                index + 1,
                routine.shown()
            );
        }
        let derived = origin_of(routine);
        if derived != Some(*origin) {
            refuse!(
                "change {} decides `PUBLIC` execution on `{}` as `{}`, which the rest of \
                 the plan does not bear out ({}).\n\
                 The artifact was edited or produced by a broken planner; regenerate it with \
                 `pbps plan --db`.",
                index + 1,
                routine.shown(),
                match origin {
                    RoutineOrigin::Created => "created",
                    RoutineOrigin::Rebuilt => "rebuilt",
                },
                match derived {
                    Some(RoutineOrigin::Rebuilt) => "this plan rebuilds that routine",
                    Some(RoutineOrigin::Created) => "this plan creates that routine",
                    None => "no change in this plan creates or rebuilds it",
                }
            );
        }
    }

    if !dialect.creates_public_executable_routines() {
        return Ok(());
    }
    let mut owed: Vec<&pbps_model::RoutineId> = created.iter().collect();
    if dialect.rebuilds_modules() {
        owed.extend(altered.iter());
    }
    let silent: Vec<String> = owed
        .into_iter()
        .filter(|routine| !decided.contains_key(routine))
        .map(|routine| routine.shown().to_string())
        .collect();
    if !silent.is_empty() {
        refuse!(
            "this plan brings {} into being on an engine whose `CREATE` hands a routine to \
             `PUBLIC`, and settles nothing about that: {}.\n\
             The artifact was edited or produced by a broken planner; regenerate it with \
             `pbps plan --db`.",
            if silent.len() == 1 {
                "1 routine".to_owned()
            } else {
                format!("{} routines", silent.len())
            },
            silent.join(", ")
        );
    }
    Ok(())
}

pub fn validate_saved_plan(
    plan: &pbps_model::SavedPlan,
    dialect: &(impl Dialect + ?Sized),
) -> Result<(), AdmissionError> {
    if plan.version != pbps_model::plan::CURRENT_VERSION {
        refuse!("unsupported saved-plan version {}", plan.version);
    }
    dialect
        .validate_plan_analysis(plan)
        .map_err(AdmissionError::from_error)?;
    // A matching checksum approves these bytes, not a broader execution mode
    // than the planner accepts. Count logical changes, since one rename may
    // emit a schema transfer and a rename (ADR-0003, decision 2) — and the
    // same count as the planner used, or an artifact the planner wrote would
    // be refused by the command that reads it back.
    let logical = logical_changes(&plan.changes);
    if plan.mode.is_staged() && logical != 1 {
        refuse!(
            "a staged plan must contain exactly one logical change; this artifact has {logical}.\n\
             Regenerate it with `pbps plan --db --staged`, isolating the change in its own \
             revision; use a transactional plan for multiple changes."
        );
    }
    plan.ids.validate().map_err(|error| {
        AdmissionError::message(format!(
            "the plan's post-apply identity mapping is inconsistent: {error}"
        ))
    })?;
    public_execution_decisions_match_their_modules(&plan.changes, dialect)?;

    let risks = super::generated::risks(&plan.changes, &plan.ids, dialect);
    for (index, planned) in plan.changes.changes.iter().enumerate() {
        // A saved artifact need not have passed this build's differ. Refuse
        // every operation carrying a complete table shape (DEC-1172.1).
        // The other operations carry no Table payload.
        #[allow(clippy::wildcard_enum_match_arm)]
        let shape = match &planned.change {
            pbps_model::Change::CreateTable { name, table, .. } => Some((name, table)),
            pbps_model::Change::AttachPartition { table, shape, .. }
            | pbps_model::Change::DetachPartition { table, shape, .. } => Some((table, shape)),
            _ => None,
        };
        if let Some((name, table)) = shape
            && table.has_row_security()
        {
            refuse!(
                "{} has row-security flags or policies; RLS planning arrives with #1172's later PRs",
                name.shown()
            );
        }
        let expected = &risks[index];
        if &planned.risks != expected {
            let names = |risks: &std::collections::BTreeSet<pbps_model::RiskClass>| {
                risks
                    .iter()
                    .map(|risk| risk.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            refuse!(
                "change {} in the plan carries inconsistent risks (file: [{}], derived: [{}]).\n\
                 The artifact was edited or produced by a broken planner; regenerate it with \
                 `pbps plan --db`.",
                index + 1,
                names(&planned.risks),
                names(expected)
            );
        }
    }
    Ok(())
}
