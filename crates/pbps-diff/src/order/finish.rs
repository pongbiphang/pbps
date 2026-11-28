//! The final stage for fresh deployment plans.

use crate::Side;
use pbps_dialect::{Dialect, slots::CatalogFacts};
use pbps_model::operation::{
    MembershipCatalog, ModuleEdits, OperationId, Operations, OwnerError, PlanOwners, Snapshot,
};
use pbps_model::plan_names::{PlanNames, PlanNamesError};
use pbps_model::resolver::OrderingProof;
use pbps_model::{Change, ChangeSet, Finding, ModuleDeps, PlanMode, Text};

pub use super::plan::{Standalone, StandaloneScheduled};
use super::rebuild;
use super::shadow::{self, Stage};
use super::slots;
use super::{
    Baseline, Because, CanonicalPlan, Checks, DiffPlan, OrderedPlan, PlanningPlan, Source,
};

/// Select the standalone route before running the shared scheduler.
pub fn standalone(plan: DiffPlan) -> PlanningPlan<Standalone> {
    super::plan::state(plan)
}

/// The declaration facts for the final sequence. Capture travels with the plan.
pub struct Inputs<'a> {
    pub before: Side<'a>,
    pub after: Side<'a>,
    pub modules: &'a ModuleDeps,
}

/// Optional observations from the connected catalog pass.
pub struct Catalog<'a> {
    pub slots: Option<&'a CatalogFacts>,
    pub membership: Option<&'a MembershipCatalog>,
}

#[derive(Debug, thiserror::Error)]
pub enum FinishError {
    #[error("final order cannot read the name history: {0}")]
    NameHistory(PlanNamesError),
    #[error("final order cannot read effects for {step}: {cause}")]
    EffectOwner { step: String, cause: OwnerError },
    #[error("final order cannot read name slots for {step}: {cause}")]
    SlotOwner { step: String, cause: OwnerError },
    #[error("final order cannot project annotations: {0}")]
    Annotation(String),
    #[error("final order requires {before} before {after}: {reason}")]
    Backward {
        before: String,
        after: String,
        reason: String,
    },
    #[error("final order cannot satisfy {step}: {reason}")]
    Symbolic { step: String, reason: String },
    #[error("final order cannot replay: {0}")]
    Replay(String),
    #[error("{0}")]
    PendingExpansion(String),
    #[error("{0}")]
    Rebuild(String),
}

/// An engine-branded plan that passed its route's final check.
///
/// ```compile_fail
/// use pbps_diff::order::{CanonicalPlan, finish::FinishedPlan};
/// use pbps_dialect::MinimalDialect;
/// fn forge(plan: CanonicalPlan<MinimalDialect>) -> FinishedPlan<MinimalDialect> {
///     FinishedPlan::new(plan)
/// }
/// ```
///
/// ```compile_fail
/// use pbps_diff::order::{CanonicalPlan, finish::FinishedPlan};
/// use pbps_dialect::MinimalDialect;
/// fn forge(plan: CanonicalPlan<MinimalDialect>) -> FinishedPlan<MinimalDialect> {
///     FinishedPlan { plan }
/// }
/// ```
///
/// ```compile_fail
/// use pbps_diff::order::{Schedule, finish::FinishedPlan};
/// use pbps_dialect::MinimalDialect;
/// fn reorder(plan: FinishedPlan<MinimalDialect>) -> FinishedPlan<MinimalDialect> {
///     Schedule::new(plan.changes().changes.len()).settle(plan).unwrap().into_planning()
/// }
/// ```
///
/// ```
/// use pbps_diff::order::{CanonicalPlan, OrderedPlan, finish};
/// use pbps_dialect::MinimalDialect;
/// fn for_target(plan: CanonicalPlan<MinimalDialect>, inputs: finish::Inputs<'_>,
///               catalog: finish::Catalog<'_>) -> OrderedPlan<MinimalDialect> {
///     finish::connected(plan, inputs, &MinimalDialect, catalog).unwrap()
///         .ordered(pbps_model::PlanMode::Transactional, &MinimalDialect)
/// }
/// ```
pub struct FinishedPlan<D: ?Sized> {
    plan: CanonicalPlan<D>,
}

impl<D: ?Sized> FinishedPlan<D> {
    fn new(plan: CanonicalPlan<D>) -> Self {
        Self { plan }
    }

    pub fn changes(&self) -> &ChangeSet {
        self.plan.changes()
    }

    pub fn proof(&self) -> &OrderingProof {
        self.plan.proof()
    }

    pub(super) fn into_canonical(self) -> CanonicalPlan<D> {
        self.plan
    }
}

impl<D: ?Sized> Clone for FinishedPlan<D> {
    fn clone(&self) -> Self {
        Self::new(self.plan.clone())
    }
}

impl<D: ?Sized> std::ops::Deref for FinishedPlan<D> {
    type Target = ChangeSet;

    fn deref(&self) -> &ChangeSet {
        self.changes()
    }
}

impl<D: Dialect + ?Sized> FinishedPlan<D> {
    pub fn ordered(self, mode: PlanMode, dialect: &D) -> OrderedPlan<D> {
        OrderedPlan::from_finished(self, mode, dialect)
    }
}

/// Finish a declaration build after the shared ordering passes.
///
/// ```compile_fail
/// use pbps_diff::order::{CanonicalPlan, finish};
/// use pbps_dialect::MinimalDialect;
/// fn misroute(plan: CanonicalPlan<MinimalDialect>, inputs: finish::Inputs<'_>) {
///     let _ = finish::offline(plan, inputs, &MinimalDialect);
/// }
/// ```
pub fn offline<D: Dialect + ?Sized>(
    plan: PlanningPlan<StandaloneScheduled<D>>,
    inputs: Inputs<'_>,
    dialect: &D,
) -> Result<FinishedPlan<D>, FinishError> {
    check(
        &plan,
        inputs,
        dialect,
        Stage::Offline,
        Catalog {
            slots: None,
            membership: None,
        },
    )?;
    Ok(FinishedPlan::new(super::plan::state(plan)))
}

/// Finish a connected plan after the last catalog ordering pass.
pub fn connected<D: Dialect + ?Sized>(
    plan: CanonicalPlan<D>,
    inputs: Inputs<'_>,
    dialect: &D,
    catalog: Catalog<'_>,
) -> Result<FinishedPlan<D>, FinishError> {
    check(&plan, inputs, dialect, Stage::Final, catalog)?;
    Ok(FinishedPlan::new(plan))
}

/// Verify the rebuild against its preparation and finish that sequence.
///
/// ```
/// use pbps_diff::order::{CanonicalPlan, finish, rebuild::{Input, Prepared}};
/// use pbps_dialect::MinimalDialect;
/// fn finish_rebuild(plan: CanonicalPlan<MinimalDialect>, input: &Input<'_>,
///                   prepared: &Prepared) -> Result<finish::FinishedPlan<MinimalDialect>, finish::FinishError> {
///     finish::rebuild(plan, input, prepared, &MinimalDialect)
/// }
/// ```
pub fn rebuild<D: ?Sized>(
    plan: CanonicalPlan<D>,
    input: &rebuild::Input<'_>,
    prepared: &rebuild::Prepared,
    dialect: &dyn Dialect,
) -> Result<FinishedPlan<D>, FinishError> {
    rebuild::verify_plan(input, prepared, plan.changes(), dialect).map_err(FinishError::Rebuild)?;
    Ok(FinishedPlan::new(plan))
}

fn subject(change: &Change) -> String {
    Text::from(change.subject()).shown().to_string()
}

fn stated(source: Source) -> bool {
    match source {
        Source::Effect
        | Source::Binding
        | Source::Declared
        | Source::Annotation
        | Source::Catalog
        | Source::Execution
        | Source::Search => true,
        Source::Guess | Source::Inferred | Source::Lexical => false,
    }
}

fn check<S>(
    plan: &PlanningPlan<S>,
    inputs: Inputs<'_>,
    dialect: &(impl Dialect + ?Sized),
    stage: Stage,
    catalog: Catalog<'_>,
) -> Result<(), FinishError> {
    super::plan::reject_pending_rebuild(plan.changes(), false)
        .map_err(FinishError::PendingExpansion)?;
    let owners = PlanOwners::new(
        PlanNames::of(&plan.boundaries.names(plan).changes, inputs.after.ids)
            .map_err(FinishError::NameHistory)?,
        inputs.before.ids,
        inputs.after.ids,
    );
    let modes = if dialect.rebuilds_modules() {
        ModuleEdits::Rebuilt
    } else {
        ModuleEdits::InPlace
    };
    let operations = if plan.boundaries.is_empty() {
        // The physical history is indexed by position. Write/write edges also
        // follow this view when no logical boundary contract owns their order.
        Operations::of_plan(&plan.changes, modes)
    } else {
        plan.identities
            .as_ref()
            .expect("registered boundaries retain identities")
            .with_steps(&plan.changes)
    };
    let ops = operations.as_slice();
    let label = |at: usize| {
        let id = plan
            .identities
            .as_ref()
            .map_or(ops[at].id(), |ids| ids.as_slice()[at].id());
        format!("step {id} ({})", subject(ops[at].change()))
    };
    let at = |id: OperationId| {
        ops.iter()
            .position(|op| op.id() == id)
            .expect("this finding belongs to the analysed sequence")
    };
    let backward = |before, after, reason| FinishError::Backward {
        before: label(before),
        after: label(after),
        reason,
    };
    let (annotations, lines) = plan
        .annotations
        .as_ref()
        .map_or_else(|| Ok(Default::default()), |contract| contract.stated(plan))
        .map_err(FinishError::Annotation)?;
    let before = Snapshot {
        schema: inputs.before.schema,
        ids: inputs.before.ids,
    };
    let after = Snapshot {
        schema: inputs.after.schema,
        ids: inputs.after.ids,
    };
    let baseline_error = |cause: OwnerError| FinishError::EffectOwner {
        step: "the opening snapshot".into(),
        cause,
    };
    let baseline = Baseline::of(before, &owners).map_err(baseline_error)?;
    let analysis = shadow::analyse_planning(
        &operations,
        &owners,
        before,
        after,
        dialect,
        catalog.slots,
        catalog.membership,
        plan,
        &annotations,
    )
    .map_err(baseline_error)?;
    let effects = analysis
        .effects()
        .map_err(|unread| FinishError::EffectOwner {
            step: label(at(unread.id)),
            cause: unread.error.clone(),
        })?;
    let baseline = baseline.with_symbolic(analysis.symbolic.clone());
    let graph = super::graph(ops, effects, &baseline, inputs.modules, Checks::ALL);
    if let Some(problem) = graph.symbolic_unmet.first() {
        let obligation = super::schedule::symbolic_obligation(problem, ops);
        return Err(FinishError::Symbolic {
            step: label(obligation.operation),
            reason: obligation.refusal,
        });
    }
    for edge in &graph.edges {
        if edge.before >= edge.after
            && stated(edge.source)
            && !shadow::catalog_release_waits(ops, edge, stage)
        {
            let reason = match &edge.because {
                Because::Declared => {
                    format!("depends_on names {}", subject(ops[edge.before].change()))
                }
                // "The first" is the step the refusal names first. A key has
                // no plain rendering, so the two labels name the objects.
                Because::RowSecurity => "the second's row security setup follows the first".into(),
                Because::Boundary(_) => "a membership boundary separates them".into(),
                Because::Restores(_) => {
                    "the second restores a value after the first writes it".into()
                }
                Because::Creates(_) => "the first creates what the second requires".into(),
                Because::Rewrites(_) => "the first rewrites what the second requires".into(),
                Because::BeforeRewrite(_) => {
                    "the first releases what would block the second's rewrite".into()
                }
                Because::Removes(_) => "the second removes what the first requires".into(),
                Because::Frees(_) => "the first removes what the second needs gone".into(),
                Because::Precedes(_) => "the first needs gone what the second brings back".into(),
                Because::Writes(_) => "the first writes the values the second reads".into(),
                Because::WriteWrite(_) => {
                    "both write the same thing and keep their base order".into()
                }
                Because::Replaces(_) => "the first ends what the second replaces".into(),
            };
            return Err(backward(edge.before, edge.after, reason));
        }
    }
    for edge in annotations {
        if edge.before >= edge.after && stated(edge.source) {
            let reason = lines.get(&(edge.before, edge.after)).map_or_else(
                || {
                    format!(
                        "declared dependency on {}",
                        subject(ops[edge.before].change())
                    )
                },
                |lines| lines.iter().cloned().collect::<Vec<_>>().join(", "),
            );
            return Err(backward(edge.before, edge.after, reason));
        }
    }

    let name = |id| label(at(id));
    let replay = super::verify::replay(ops, effects, &baseline, Checks::ALL);
    if let Some(reason) = shadow::judge_effects(ops, &graph, &replay, stage, name).first_failure() {
        return Err(FinishError::Replay(reason));
    }
    let slots = slots::analyse(ops, &owners, before, dialect);
    if let Some((id, cause)) = slots.effects_with_unread().1.unowned.first() {
        return Err(FinishError::SlotOwner {
            step: name(*id),
            cause: cause.clone(),
        });
    }
    let names = shadow::judge_slot_rows(
        ops,
        &slots,
        &owners,
        before,
        dialect,
        catalog.slots,
        stage,
        name,
    )
    .map_err(|cause| FinishError::SlotOwner {
        step: "the name replay's opening snapshot or catalog".into(),
        cause,
    })?;
    if let Some(reason) = names.first_failure() {
        return Err(FinishError::Replay(reason));
    }
    Ok(())
}

/// Append findings to the reviewed payload while keeping every step at its position.
pub fn record_findings<D: ?Sized>(
    mut plan: FinishedPlan<D>,
    findings: &[(usize, Finding)],
) -> FinishedPlan<D> {
    super::writebacks::findings(&mut plan.plan, findings);
    plan
}

/// Remap resolver evidence and operation IDs while keeping every step at its position.
pub fn remap_resolver<D: ?Sized>(
    before: &CanonicalPlan<D>,
    plan: FinishedPlan<D>,
    describe: impl Fn(&Change) -> String,
) -> Result<FinishedPlan<D>, String> {
    super::resolver_remap::finish(before, plan.plan, describe).map(FinishedPlan::new)
}

/// Carry a fixture's intermediate order to the conversion without another observation.
#[cfg(any(test, feature = "test-fixtures"))]
pub fn finish_fixture<D: Dialect + ?Sized>(plan: CanonicalPlan<D>) -> FinishedPlan<D> {
    FinishedPlan::new(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::{Schedule, Source, propose};
    use pbps_dialect::MinimalDialect;
    use pbps_model::finding::Severity;
    use pbps_model::resolver::OrderReason;
    use pbps_model::{PlannedChange, RoleChange};

    fn changes() -> ChangeSet {
        ChangeSet {
            changes: [("r_aaaaaa", "reader"), ("r_aaaaab", "writer")]
                .into_iter()
                .map(|(uid, name)| {
                    PlannedChange::new(Change::Role(RoleChange::CreateRole {
                        uid: uid.parse().unwrap(),
                        name: name.into(),
                    }))
                })
                .collect(),
        }
    }

    #[test]
    fn final_stage_preserves_the_complete_change_sequence() {
        let captured = pbps_model::operation::Captured {
            before: [("app.retained".parse().unwrap(), Default::default())].into(),
            ..Default::default()
        };
        let operations = Operations::of_plan(&changes().changes, ModuleEdits::InPlace);
        let mut intermediate =
            super::super::plan::state(propose(changes()).with_captured(&captured));
        intermediate.identities = Some(operations.clone());
        let before = intermediate.changes().clone();
        let proof = intermediate.proof().clone();
        let origins = intermediate.origins().to_vec();
        let side = Side {
            schema: &Default::default(),
            ids: &Default::default(),
        };
        let mut ids = pbps_model::IdsFile::default();
        ids.roles
            .insert("r_aaaaaa".parse().unwrap(), "reader".into());
        ids.roles
            .insert("r_aaaaab".parse().unwrap(), "writer".into());
        let after = Side { ids: &ids, ..side };
        let finished = offline(
            intermediate,
            Inputs {
                before: side,
                after,
                modules: &Default::default(),
            },
            &MinimalDialect,
        )
        .unwrap();
        assert_eq!(finished.changes(), &before);
        assert_eq!(finished.plan.captured(), &captured);
        assert_eq!(finished.plan.identities.as_ref(), Some(&operations));
        let ordered = finished.ordered(PlanMode::Transactional, &MinimalDialect);
        assert_eq!(ordered.changes(), &before);
        assert_eq!(ordered.proof(), &proof);
        assert_eq!(ordered.origins(), origins);
        assert_eq!(
            serde_json::to_vec(ordered.changes()).unwrap(),
            serde_json::to_vec(&before).unwrap()
        );

        let mut reversed = before.clone();
        reversed.changes.reverse();
        assert_ne!(reversed, before);
        assert_eq!(
            ordered.proof().validate(&reversed),
            Err(pbps_model::resolver::OrderError::Changed)
        );
    }

    #[test]
    fn final_stage_refuses_a_role_history_without_its_final_identity() {
        let side = Side {
            schema: &Default::default(),
            ids: &Default::default(),
        };
        let error = offline(
            super::super::plan::state(propose(changes())),
            Inputs {
                before: side,
                after: side,
                modules: &Default::default(),
            },
            &MinimalDialect,
        )
        .err()
        .expect("the missing final role identities refuse");
        assert!(matches!(error, FinishError::NameHistory(_)));
        assert!(error.to_string().contains("role"));
    }

    #[test]
    fn every_catalog_evidence_combination_refuses_a_backward_stated_edge() {
        use pbps_dialect::slots::{Namespace, SlotKey};
        use pbps_model::{Module, ModuleChange, ModuleId, ModuleKind};
        let provider: ModuleId = "app.provider".parse().unwrap();
        let consumer: ModuleId = "app.consumer".parse().unwrap();
        let modules =
            ModuleDeps::from([(consumer.clone(), [provider.clone()].into_iter().collect())]);
        let side = Side {
            schema: &Default::default(),
            ids: &Default::default(),
        };
        let plan = ChangeSet {
            changes: [consumer, provider]
                .into_iter()
                .map(|id| {
                    PlannedChange::new(Change::Module(ModuleChange::CreateModule {
                        id,
                        module: Box::new(Module {
                            kind: ModuleKind::View,
                            description: None,
                            definition: "SELECT 'body sentinel'".into(),
                            options: Default::default(),
                        }),
                    }))
                })
                .collect(),
        };
        let plan = super::super::plan::canonical(propose(plan), &MinimalDialect).unwrap();
        let empty_slots = CatalogFacts::default();
        let filled_slots = CatalogFacts {
            compared: [SlotKey {
                namespace: Namespace::Relations("app".into()),
                name: "provider".into(),
            }]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let empty_membership = MembershipCatalog::default();
        let filled_membership = MembershipCatalog {
            columns: [(
                "app.t"
                    .parse::<pbps_model::TableName>()
                    .unwrap()
                    .column("n"),
                Default::default(),
            )]
            .into_iter()
            .collect(),
        };
        for slots in [None, Some(&empty_slots), Some(&filled_slots)] {
            for membership in [None, Some(&empty_membership), Some(&filled_membership)] {
                let error = connected(
                    plan.clone(),
                    Inputs {
                        before: side,
                        after: side,
                        modules: &modules,
                    },
                    &MinimalDialect,
                    Catalog { slots, membership },
                )
                .err()
                .expect("the catalog evidence cannot bypass the final check");
                assert!(matches!(error, FinishError::Backward { .. }));
                let message = error.to_string();
                for detail in [
                    "step 1.0 (app.provider)",
                    "step 0.0 (app.consumer)",
                    "depends_on names app.provider",
                ] {
                    assert!(message.contains(detail), "{message}");
                }
                assert!(!message.contains("body sentinel"), "{message}");
            }
        }
    }

    struct UnreadMembership;

    impl Dialect for UnreadMembership {
        fn name(&self) -> &'static str {
            "unread membership fixture"
        }
        fn lexicon(&self) -> pbps_dialect::Lexicon {
            MinimalDialect.lexicon()
        }
        fn quote_ident(&self, ident: &str) -> Result<String, pbps_dialect::DialectError> {
            MinimalDialect.quote_ident(ident)
        }
        fn normalize_type(
            &self,
            ty: &pbps_model::ColumnType,
        ) -> Result<pbps_model::ColumnType, pbps_dialect::DialectError> {
            MinimalDialect.normalize_type(ty)
        }
        fn type_change_risk(
            &self,
            from: &pbps_model::ColumnType,
            to: &pbps_model::ColumnType,
        ) -> pbps_dialect::TypeChangeRisk {
            MinimalDialect.type_change_risk(from, to)
        }
        fn validate_table(
            &self,
            name: &pbps_model::TableName,
            table: &pbps_model::Table,
        ) -> Vec<pbps_dialect::DialectError> {
            MinimalDialect.validate_table(name, table)
        }
        fn transaction_framing(
            &self,
            zone: &pbps_model::SessionZone,
        ) -> pbps_dialect::TransactionFraming {
            MinimalDialect.transaction_framing(zone)
        }
        fn probe_framing(&self) -> Option<pbps_dialect::TransactionFraming> {
            MinimalDialect.probe_framing()
        }
        fn fold_ident<'a>(&self, ident: &'a str) -> std::borrow::Cow<'a, str> {
            MinimalDialect.fold_ident(ident)
        }
        fn intermediate_tables(
            &self,
            change: &Change,
            strategy: pbps_model::Strategy,
        ) -> Result<Vec<pbps_model::TableName>, pbps_dialect::DialectError> {
            MinimalDialect.intermediate_tables(change, strategy)
        }
        fn symbolic_effects(
            &self,
            _operation: &pbps_model::operation::Operation,
            _owners: &dyn pbps_model::operation::Owners,
            _before: Snapshot<'_>,
            _after: Snapshot<'_>,
            _facts: &pbps_model::operation::symbolic::Facts,
            _lineage: &pbps_model::operation::Lineage,
        ) -> Result<pbps_model::operation::Effects, OwnerError> {
            use pbps_model::operation::symbolic::{Clause, Fact};
            Ok(pbps_model::operation::Effects {
                symbolic: vec![Clause::Require(Fact::Unread)],
                ..Default::default()
            })
        }
    }

    #[test]
    fn a_single_step_with_an_unmet_symbolic_requirement_refuses() {
        use pbps_model::{BoundDatum, PartitionBound, PartitionChange, Schema, TableName, Uid};
        let parent: Uid = "t_aaaaaa".parse().unwrap();
        let child: Uid = "t_bbbbbb".parse().unwrap();
        let parent_name: TableName = "app.p".parse().unwrap();
        let child_name: TableName = "app.a".parse().unwrap();
        let mut schema = Schema::default();
        let mut ids = pbps_model::IdsFile::default();
        for (uid, name) in [(&parent, &parent_name), (&child, &child_name)] {
            ids.tables.insert(uid.clone(), name.clone());
            schema.tables.insert(name.clone(), Default::default());
        }
        let mut plan = propose(ChangeSet {
            changes: vec![PlannedChange::new(Change::Partition(
                PartitionChange::AttachPartition {
                    uid: child.clone(),
                    table: child_name,
                    parent: parent_name,
                    bound: PartitionBound::Range {
                        from: vec![BoundDatum::MinValue],
                        to: vec![BoundDatum::MaxValue],
                    },
                    key: Default::default(),
                    shape: Box::default(),
                    rehearsal: None,
                },
            ))],
        });
        plan.pending_boundaries(
            [crate::order::boundaries::Boundary { parent, child }],
            ModuleEdits::InPlace,
        );
        let side = Side {
            schema: &schema,
            ids: &ids,
        };
        let error = offline(
            super::super::plan::state(plan),
            Inputs {
                before: side,
                after: side,
                modules: &Default::default(),
            },
            &UnreadMembership,
        )
        .err()
        .expect("a symbolic obligation refuses before the forward edge check");
        assert!(matches!(error, FinishError::Symbolic { .. }));
        let message = error.to_string();
        assert!(
            message.contains("step 0.0 (app.a)") && message.contains("was not read"),
            "{message}"
        );
    }

    #[test]
    fn text_guesses_are_excluded_from_the_forward_check() {
        for source in [Source::Guess, Source::Inferred, Source::Lexical] {
            assert!(!stated(source));
        }
        assert!(stated(Source::Effect));
    }

    #[test]
    fn recording_findings_keeps_every_change_at_its_position() {
        let before = changes();
        let plan = super::super::plan::canonical(propose(before.clone()), &MinimalDialect).unwrap();
        let note = Finding::new("test.note", Severity::Warning, "example note", None);
        let finished = record_findings(
            finish_fixture(plan),
            &[(1, note.clone()), (before.changes.len(), note.clone())],
        );
        let mut expected = before.clone();
        expected.changes[1].findings.push(note);
        assert_eq!(finished.changes(), &expected);
        assert!(finished.proof().validate(&expected).is_ok());
        assert_eq!(
            finished.proof().validate(&before),
            Err(pbps_model::resolver::OrderError::Changed)
        );
    }

    #[test]
    fn remapping_resolver_evidence_keeps_every_change_at_its_position() {
        let before = changes();
        let mut schedule = Schedule::new(before.changes.len());
        schedule.edge(0, 1, Source::Effect, OrderReason::Structural);
        let resolver = super::super::plan::canonical(
            schedule
                .settle(propose(before.clone()))
                .unwrap()
                .into_planning(),
            &MinimalDialect,
        )
        .unwrap();
        let current =
            super::super::plan::canonical(propose(before.clone()), &MinimalDialect).unwrap();
        let remapped = remap_resolver(&resolver, finish_fixture(current), Change::subject).unwrap();
        assert_eq!(remapped.changes(), &before);
        assert_eq!(remapped.proof(), resolver.proof());

        let mut reversed = before;
        reversed.changes.reverse();
        let wrong = super::super::plan::canonical(propose(reversed), &MinimalDialect).unwrap();
        let error = remap_resolver(&resolver, finish_fixture(wrong), Change::subject)
            .err()
            .expect("an existing resolver edge is backward");
        assert!(error.contains("which the resolver orders after it"));
    }
}
