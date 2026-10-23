//! Checked saved artifacts keep their reviewed order; admission does not sort
//! and never pretends to have produced an ordering proof (DEC-1959.5).

use super::{OrderedPlan, PlanView};
use pbps_dialect::{Dialect, PlanPayload};
use pbps_model::{ChangeSet, SavedPlan};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct AdmissionError(Box<dyn std::error::Error + Send + Sync>);

impl AdmissionError {
    pub(super) fn from_error(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self(Box::new(error))
    }
    pub(super) fn message(message: String) -> Self {
        #[derive(Debug, thiserror::Error)]
        #[error("{0}")]
        struct Message(String);
        Self::from_error(Message(message))
    }
}

macro_rules! refuse {
    ($($arg:tt)*) => { return Err(AdmissionError::message(format!($($arg)*))) };
}

/// Only a newly sealed or checked artifact may be written or used for an
/// executable checksum. The model DTO remains readable untrusted wire data.
/// ```compile_fail
/// fn raw(plan: &pbps_model::SavedPlan) {
///     let _: &dyn pbps_diff::order::ArtifactView = plan;
/// }
/// ```
pub trait ArtifactView: super::plan::sealed::Sealed {
    fn wire(&self) -> &SavedPlan;
    fn checksum(&self) -> String {
        self.wire().wire_fingerprint()
    }
}

/// A reviewed wire artifact accepted by the existing artifact checks. Its
/// checksum-pinned sequence is trusted under SPEC's saved-plan contract;
/// admission neither reorders it nor enables the deferred release verifier.
///
/// ```compile_fail
/// fn unchecked(wire: pbps_model::SavedPlan)
///     -> pbps_diff::order::AdmittedPlan<pbps_dialect::MinimalDialect> {
///     pbps_diff::order::AdmittedPlan { wire }
/// }
/// ```
#[derive(Debug)]
pub struct AdmittedPlan<D: ?Sized> {
    wire: SavedPlan,
    brand: std::marker::PhantomData<fn(&D) -> &D>,
}

impl<D: ?Sized> Clone for AdmittedPlan<D> {
    fn clone(&self) -> Self {
        Self {
            wire: self.wire.clone(),
            brand: std::marker::PhantomData,
        }
    }
}

impl<D: ?Sized> AdmittedPlan<D> {
    pub fn wire(&self) -> &SavedPlan {
        &self.wire
    }
}
impl<D: ?Sized> std::ops::Deref for AdmittedPlan<D> {
    type Target = SavedPlan;
    fn deref(&self) -> &SavedPlan {
        &self.wire
    }
}
impl<D: ?Sized> ArtifactView for AdmittedPlan<D> {
    fn wire(&self) -> &SavedPlan {
        &self.wire
    }
}
impl<D: ?Sized> PlanPayload for AdmittedPlan<D> {
    fn operations(&self) -> pbps_model::operation::Operations {
        // Saved bytes omit planning origins (DEC-1958.3). Each reviewed wire
        // step is its own logical input here; never guess a lost source from
        // the optional, unsealed in-memory provenance (DEC-1959.5).
        pbps_model::operation::Operations::of_plan(
            &self.wire.changes.changes,
            pbps_model::operation::ModuleEdits::InPlace,
        )
    }
    fn changes(&self) -> &ChangeSet {
        &self.wire.changes
    }
    fn mode(&self) -> pbps_model::PlanMode {
        self.wire.mode
    }
}
impl<D: ?Sized> super::plan::sealed::Sealed for AdmittedPlan<D> {}
impl<D: ?Sized> PlanView<D> for AdmittedPlan<D> {}

/// Accepts only checked wire bytes, never granting `OrderedPlan` authority.
/// The checksum is computed here so the caller cannot substitute an unchecked
/// fingerprint. Refusals retain apply's version/checksum/origin/dialect order.
pub fn admit<D: Dialect + ?Sized>(
    plan: SavedPlan,
    dialect: &D,
    plan_path: &Path,
    approved_checksum: &str,
) -> Result<AdmittedPlan<D>, AdmissionError> {
    if plan.version != pbps_model::plan::CURRENT_VERSION {
        refuse!(
            "`{}` is a version {} plan and this tool understands version {}.\n\
             Recompute it with `pbps plan --db ... --out ...` and review the new artifact for approval.",
            plan_path.display(),
            plan.version,
            pbps_model::plan::CURRENT_VERSION
        );
    }
    let plan_checksum = plan.wire_fingerprint();
    if plan_checksum != approved_checksum {
        refuse!(
            "`{}` no longer matches the artifact approved at the deployment gate.\n\
             approved checksum: {approved_checksum}\n\
             plan checksum now: {plan_checksum}\n\
             Do not approve the new value implicitly: retrieve the reviewed artifact or run \
             `pbps explain --plan ...` and send it through approval again.",
            plan_path.display()
        );
    }
    if !plan.origin.is_applyable() {
        refuse!(
            "`{}` is an offline preview, not an applyable plan.\n\
             Recompute it against the target with `pbps plan --db ... --out {}`.",
            plan_path.display(),
            plan_path.display()
        );
    }
    if plan.dialect != dialect.name() {
        refuse!(
            "`{}` was computed for {} and this project is {}",
            plan_path.display(),
            pbps_model::escape::Escaped(&plan.dialect),
            dialect.name()
        );
    }
    validate_saved_plan(&plan, dialect)?;
    Ok(AdmittedPlan {
        wire: plan,
        brand: std::marker::PhantomData,
    })
}

/// A newly produced artifact, bound to the exact plan that ordering produced.
/// Metadata remains the existing wire DTO, with no capability/proof fields.
#[derive(Debug)]
pub struct SealedPlan<D: ?Sized> {
    wire: SavedPlan,
    ordered: OrderedPlan<D>,
}
impl<D: ?Sized> Clone for SealedPlan<D> {
    fn clone(&self) -> Self {
        Self {
            wire: self.wire.clone(),
            ordered: self.ordered.clone(),
        }
    }
}
impl<D: ?Sized> ArtifactView for SealedPlan<D> {
    fn wire(&self) -> &SavedPlan {
        &self.wire
    }
}
impl<D: ?Sized> SealedPlan<D> {
    pub fn wire(&self) -> &SavedPlan {
        &self.wire
    }
    pub fn ordered(&self) -> &OrderedPlan<D> {
        &self.ordered
    }
}
impl<D: ?Sized> std::ops::Deref for SealedPlan<D> {
    type Target = SavedPlan;
    fn deref(&self) -> &SavedPlan {
        &self.wire
    }
}
impl<D: ?Sized> PlanPayload for SealedPlan<D> {
    fn operations(&self) -> pbps_model::operation::Operations {
        self.ordered.operations()
    }
    fn changes(&self) -> &ChangeSet {
        self.ordered.changes()
    }
    fn mode(&self) -> pbps_model::PlanMode {
        self.ordered.mode()
    }
}
impl<D: ?Sized> super::plan::sealed::Sealed for SealedPlan<D> {}
impl<D: ?Sized> PlanView<D> for SealedPlan<D> {}

/// Metadata cannot replace a single operation, strategy or finding after
/// ordering: the original proof must still describe the wire payload exactly.
/// ```compile_fail
/// fn raw(changes: pbps_model::ChangeSet, wire: pbps_model::SavedPlan) {
///     let _ = pbps_diff::order::seal(changes, wire);
/// }
/// ```
pub fn seal<D: ?Sized>(
    ordered: OrderedPlan<D>,
    wire: SavedPlan,
) -> Result<SealedPlan<D>, AdmissionError> {
    ordered
        .proof()
        .validate(&wire.changes)
        .map_err(AdmissionError::from_error)?;
    if ordered.mode() != wire.mode {
        refuse!(
            "the saved plan mode {} does not match the ordered plan mode {}",
            wire.mode,
            ordered.mode()
        );
    }
    if ordered.dialect() != wire.dialect {
        refuse!(
            "the saved plan dialect {} does not match the ordered plan dialect {}",
            pbps_model::escape::Escaped(&wire.dialect),
            ordered.dialect()
        );
    }
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
    if !plan.ids.rebuilds.is_empty() {
        refuse!("recorded blocking rewrite intent cannot be executed yet");
    }
    for planned in &plan.changes.changes {
        if let Some((operation, table)) = planned.change.pending_rebuild() {
            refuse!(
                "{operation} for `{}` cannot be admitted before blocking rewrites are enabled",
                table.shown()
            );
        }
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
    if plan.mode.is_staged() && super::row_security::requires_transaction(&plan.changes) {
        refuse!(
            "row-security changes require a transactional plan; --staged is unsupported (DEC-1173.3)"
        );
    }
    plan.ids.validate().map_err(|source| {
        #[derive(Debug, thiserror::Error)]
        #[error("the plan's post-apply identity mapping is inconsistent")]
        struct Identity(#[source] Box<dyn std::error::Error + Send + Sync>);
        AdmissionError::from_error(Identity(Box::new(source)))
    })?;
    public_execution_decisions_match_their_modules(&plan.changes, dialect)?;
    super::row_security::validate(&plan.changes, &plan.ids, dialect)?;

    let risks = super::generated::risks(&plan.changes, &plan.ids, dialect);
    for (index, planned) in plan.changes.changes.iter().enumerate() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Change, PlannedChange};
    fn role(name: &str) -> PlannedChange {
        PlannedChange::new(Change::CreateRole {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Role, name, 0),
            name: name.into(),
        })
    }
    fn wire(changes: ChangeSet) -> SavedPlan {
        SavedPlan::new(
            pbps_model::PlanOrigin::Database,
            "minimal",
            "2026-10-22T00:00:00Z",
            pbps_model::PlanBaseline {
                description: "fixed artifact oracle".into(),
                checksum: "baseline".into(),
                database_collation: None,
                time_zone: None,
            },
            changes,
            Default::default(),
        )
    }
    #[test]
    fn admission_keeps_the_reviewed_wire_order_and_fingerprint() {
        let wire = wire(ChangeSet {
            changes: vec![role("z"), role("a")],
        });
        let before = wire.wire_fingerprint();
        let admitted = admit(
            wire.clone(),
            &pbps_dialect::MinimalDialect,
            Path::new("plan.json"),
            &before,
        )
        .unwrap();
        assert_eq!(admitted.wire(), &wire);
        assert_eq!(admitted.checksum(), before);
        let again = admit(
            admitted.wire().clone(),
            &pbps_dialect::MinimalDialect,
            Path::new("plan.json"),
            &before,
        )
        .unwrap();
        assert_eq!(again.wire(), admitted.wire());
        let mut wrong = wire;
        wrong.version += 1;
        assert!(
            admit(
                wrong,
                &pbps_dialect::MinimalDialect,
                Path::new("plan.json"),
                &before
            )
            .is_err()
        );
    }
    #[test]
    fn sealing_rejects_replaced_operations_and_post_order_payload_edits() {
        let ordered = crate::order::singleton(
            role("a"),
            pbps_model::PlanMode::Transactional,
            &pbps_dialect::MinimalDialect,
        );
        let exact = wire(ordered.changes().clone());
        assert!(seal(ordered.clone(), exact.clone()).is_ok());
        let mut replaced = exact.clone();
        replaced.changes.changes[0] = role("b");
        assert!(seal(ordered.clone(), replaced).is_err());
        let mut strategy = exact;
        strategy.changes.changes[0].strategy.online = true;
        assert!(seal(ordered, strategy).is_err());
    }
}
