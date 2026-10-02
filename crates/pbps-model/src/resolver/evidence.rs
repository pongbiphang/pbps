//! A required artifact contract; neither an optional metadata bag nor a
//! `verified` flag. Runtime qualification belongs to its connected producer.

use super::manifest::hex;
use super::{
    InputManifest, ManifestError, ObjectTransition, OrderError, OrderingProof, SurfaceResolution,
};
use crate::{Change, ChangeSet};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "kind",
    content = "evidence",
    rename_all = "kebab-case",
    deny_unknown_fields
)]
pub enum PlanAnalysis {
    Ordinary,
    Resolved(Box<ResolverEvidence>),
}

/// The keyed-fingerprint design declassifies fingerprints, not external
/// plaintext (DEC-952.1). There is no caller-selected public/private boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvidenceHandling {
    KeyedFingerprints,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ResolverRuntime {
    Container {
        image_digest: String,
        platform: String,
        profile: String,
    },
    Supplied {
        profile: String,
        identity: String,
    },
}

/// Environment-keyed fingerprints of the complete qualified observations.
/// Paths, source-bearing settings and executable metadata are not exported.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Qualification {
    pub rule: String,
    /// Raw opening target catalog facts, before any approved grant projection.
    pub target_environment: String,
    /// The same observation after projecting exactly the ordered approved grants.
    pub target_environment_after: String,
    pub resolver_environment: String,
    pub target_build: String,
    pub resolver_build: String,
    pub channels: String,
    pub runtime: ResolverRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationCondition {
    pub rule: String,
    pub before: String,
    pub after: String,
    /// Exactly the approved authorization-changing steps. The adapter derives
    /// `after` at planning time; apply checks it and never projects new grants.
    pub changes: BTreeSet<usize>,
}

/// This version covers creation-time bindings only; a runtime-bound body is
/// an explicit limitation, retained in each manifest, never a verified body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BindingCoverage {
    CreationTimeV1,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolverEvidence {
    version: u32,
    handling: EvidenceHandling,
    coverage: BindingCoverage,
    qualification: Qualification,
    authorization: AuthorizationCondition,
    before: InputManifest,
    after: InputManifest,
    surfaces: Vec<SurfaceResolution>,
    transitions: Vec<ObjectTransition>,
    ordering: OrderingProof,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvidenceError {
    #[error("unsupported resolver evidence version or qualification")]
    Version,
    #[error("resolver evidence omits or contradicts a required binding or authorization condition")]
    Incomplete,
    #[error("resolver evidence does not derive its closing manifest from the approved changes")]
    Projection,
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Ordering(#[from] OrderError),
}

impl ResolverEvidence {
    /// Sealing is a planning operation. The caller supplies engine-observed
    /// compiled facts; this constructor derives the closing manifest, rather
    /// than accepting a caller's replacement for untouched target inputs.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        changes: &ChangeSet,
        qualification: Qualification,
        authorization: AuthorizationCondition,
        before: InputManifest,
        compiled: &InputManifest,
        surfaces: Vec<SurfaceResolution>,
        transitions: Vec<ObjectTransition>,
        ordering: OrderingProof,
    ) -> Result<Self, EvidenceError> {
        let after = before.project(changes, compiled, &transitions)?;
        let evidence = Self {
            version: 2,
            handling: EvidenceHandling::KeyedFingerprints,
            coverage: BindingCoverage::CreationTimeV1,
            qualification,
            authorization,
            before,
            after,
            surfaces,
            transitions,
            ordering,
        };
        evidence.validate(changes)?;
        Ok(evidence)
    }

    pub fn before(&self) -> &InputManifest {
        &self.before
    }
    pub fn after(&self) -> &InputManifest {
        &self.after
    }
    pub fn surfaces(&self) -> &[SurfaceResolution] {
        &self.surfaces
    }
    pub fn qualification(&self) -> &Qualification {
        &self.qualification
    }
    pub fn authorization(&self) -> &AuthorizationCondition {
        &self.authorization
    }
    pub fn ordering(&self) -> &OrderingProof {
        &self.ordering
    }

    #[allow(clippy::wildcard_enum_match_arm)]
    pub fn validate(&self, changes: &ChangeSet) -> Result<(), EvidenceError> {
        if self.version != 2
            || self.qualification.rule.is_empty()
            || self.authorization.rule.is_empty()
        {
            return Err(EvidenceError::Version);
        }
        let q = &self.qualification;
        if [
            &q.target_environment,
            &q.target_environment_after,
            &q.resolver_environment,
            &q.target_build,
            &q.resolver_build,
            &q.channels,
            &self.authorization.before,
            &self.authorization.after,
        ]
        .iter()
        .any(|d| !hex(d, 64))
        {
            return Err(EvidenceError::Incomplete);
        }
        let qualified = match &q.runtime {
            ResolverRuntime::Container {
                image_digest,
                platform,
                profile,
            } => {
                image_digest
                    .strip_prefix("sha256:")
                    .is_some_and(|d| hex(d, 64))
                    && !platform.is_empty()
                    && !profile.is_empty()
            }
            ResolverRuntime::Supplied { profile, identity } => {
                !profile.is_empty() && hex(identity, 64)
            }
        };
        if !qualified {
            return Err(EvidenceError::Incomplete);
        }
        let authorization_changes = changes
            .changes
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                matches!(
                    &p.change,
                    Change::Grant { .. }
                        | Change::Revoke { .. }
                        | Change::PublicExecution { .. }
                        | Change::CreateRole { .. }
                        | Change::RenameRole { .. }
                        | Change::DropRole { .. }
                )
            })
            .map(|(i, _)| i)
            .collect::<BTreeSet<_>>();
        if authorization_changes != self.authorization.changes
            || (authorization_changes.is_empty()
                && self.authorization.before != self.authorization.after)
        {
            return Err(EvidenceError::Incomplete);
        }
        self.ordering.validate(changes)?;
        // The artifact must at least cover every explicitly changed binding
        // surface. Unchanged managed surfaces come from the producer's complete
        // schema inventory; their required membership is pinned by the seal.
        for step in &changes.changes {
            let required = match &step.change {
                Change::CreateTable { name, table, .. } => {
                    // Owner transitions cover catalog records, not resolution
                    // membership: every inline binding needs its own observation.
                    let mut surfaces = Vec::new();
                    for (column, spec) in &table.columns {
                        if spec.default.is_some() || spec.generated.is_some() {
                            surfaces.push(super::Surface::Default(name.column(column)));
                        }
                    }
                    for check in table.checks.keys() {
                        surfaces.push(super::Surface::Check {
                            table: name.clone(),
                            name: check.clone(),
                        });
                    }
                    for (index, spec) in &table.indexes {
                        if spec.holds_expression() {
                            surfaces.push(super::Surface::Index {
                                table: name.clone(),
                                name: index.clone(),
                            });
                        }
                    }
                    surfaces
                }
                Change::CreateModule { id, .. }
                | Change::AlterModule { id, .. }
                | Change::DropModule { id, .. } => vec![super::Surface::Module(id.clone())],
                // A generation expression is the column's `pg_attrdef` row,
                // as a default is (DEC-1168.1).
                Change::AlterColumnDefault { column, .. }
                | Change::AlterColumnExpression { column, .. } => {
                    vec![super::Surface::Default(column.clone())]
                }
                Change::AddColumn {
                    table,
                    name,
                    column,
                    ..
                } if column.default.is_some() || column.generated.is_some() => {
                    vec![super::Surface::Default(table.column(name))]
                }
                Change::AddCheck { table, name, .. } | Change::DropCheck { table, name } => {
                    vec![super::Surface::Check {
                        table: table.clone(),
                        name: name.clone(),
                    }]
                }
                Change::AddIndex {
                    table, name, index, ..
                } if index.holds_expression() => vec![super::Surface::Index {
                    table: table.clone(),
                    name: name.clone(),
                }],
                _ => Vec::new(),
            };
            for surface in required {
                if !self.surfaces.iter().any(|s| s.surface == surface) {
                    return Err(EvidenceError::Incomplete);
                }
            }
        }

        // The opening manifest is a complete observation; only the closing
        // manifest may hold placeholders for the plan's own records.
        if self
            .before
            .prerequisites()
            .iter()
            .any(|p| p.is_managed_closing())
        {
            return Err(EvidenceError::Incomplete);
        }
        if !self
            .before
            .closing_matches(changes, &self.after, &self.transitions)?
        {
            return Err(EvidenceError::Projection);
        }
        if !self
            .transitions
            .windows(2)
            .all(|t| t[0].surface < t[1].surface)
        {
            return Err(EvidenceError::Incomplete);
        }
        if !self
            .surfaces
            .windows(2)
            .all(|s| s[0].surface < s[1].surface)
        {
            return Err(EvidenceError::Incomplete);
        }
        let removed: BTreeSet<_> = self.transitions.iter().flat_map(|t| &t.before).collect();
        let installed: BTreeSet<_> = self.transitions.iter().flat_map(|t| &t.after).collect();
        for surface in &self.surfaces {
            // Ownership may aggregate internal expression records under a
            // table transition. Require the actual observed records, not just
            // a transition carrying the right surface name.
            let changed = surface.current != surface.desired
                || changes
                    .changes
                    .iter()
                    .any(|p| super::projection::touches(&p.change, &surface.surface));
            let covered = surface
                .current
                .as_ref()
                .is_none_or(|o| removed.contains(&o.object))
                && surface
                    .desired
                    .as_ref()
                    .is_none_or(|o| installed.contains(&o.object));
            if changed && !covered {
                return Err(EvidenceError::Incomplete);
            }
            // Absence of a binding surface does not imply absence of its
            // catalog record: removing an index predicate leaves a plain
            // index. The qualified adapter supplies that inventory.
            if surface.current.is_none() && surface.desired.is_none() {
                return Err(EvidenceError::Incomplete);
            }
            // A record the plan installs is not in the closing manifest: its
            // sealed desired bindings are the expectation itself.
            let desired = surface
                .desired
                .as_ref()
                .filter(|observed| !installed.contains(&observed.object));
            for (observed, manifest) in [
                (surface.current.as_ref(), &self.before),
                (desired, &self.after),
            ] {
                if let Some(observed) = observed {
                    let Some(p) = manifest
                        .prerequisites()
                        .iter()
                        .find(|p| p.object == observed.object)
                    else {
                        return Err(EvidenceError::Incomplete);
                    };
                    if p.bindings != observed.bindings {
                        return Err(EvidenceError::Incomplete);
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::{
        BoundSurface, CandidateSet, Membership, ObjectIdentity, Prerequisite, ReadScope, Surface,
    };
    use crate::{IdsFile, Module, ModuleKind, PlanBaseline, PlanOrigin, PlannedChange, SavedPlan};
    use serde_json::json;

    fn object(schema: &str, name: &str) -> ObjectIdentity {
        ObjectIdentity {
            class: "relation".into(),
            name: vec![schema.into(), name.into()],
            signature: vec![],
        }
    }

    fn manifest(new: bool) -> InputManifest {
        let query = CandidateSet {
            class: "relation".into(),
            namespace: Some("app".into()),
            name: None,
        };
        let ext = object("ext", "input");
        let view = object("app", "v");
        let mut records = vec![Prerequisite {
            object: ext.clone(),
            ownership: crate::resolver::ObjectOwnership::Unqualified,
            canonicalization: "fixture-v1".into(),
            properties: if new { "aa" } else { "bb" }.repeat(32),
            bindings: vec![],
        }];
        if new {
            records.push(Prerequisite {
                object: view.clone(),
                ownership: crate::resolver::ObjectOwnership::Surface(Surface::Module(
                    "app.v".parse().unwrap(),
                )),
                canonicalization: "fixture-v1".into(),
                properties: "cc".repeat(32),
                bindings: vec![],
            });
        }
        records.sort_by(|a, b| a.object.cmp(&b.object));
        InputManifest::new(
            "fixture-v1".into(),
            18,
            "01".repeat(8),
            ReadScope {
                retained: BTreeSet::from([ext]),
                candidates: BTreeSet::from([query.clone()]),
            },
            "02".repeat(32),
            "03".repeat(32),
            records,
            vec![Membership {
                predicate: query,
                members: if new {
                    BTreeSet::from([view])
                } else {
                    BTreeSet::new()
                },
            }],
            BTreeSet::new(),
            vec![],
        )
        .unwrap()
    }

    /// The engine-compiled desired manifest behind [`plan`]: what scratch
    /// would capture for the created view, including its fingerprint.
    pub(super) fn compiled() -> InputManifest {
        manifest(true)
    }

    /// The catalog a target shows once the plan has applied: the closing
    /// manifest with every installed record observed from `compiled`, with
    /// its real fingerprint, whether or not the closing manifest kept a
    /// placeholder for it. Fixtures that start a later plan from this state
    /// need it, because an opening manifest is a complete observation and
    /// may hold no placeholder.
    pub(super) fn observed(evidence: &ResolverEvidence, compiled: &InputManifest) -> InputManifest {
        let installed: BTreeSet<_> = evidence.transitions.iter().flat_map(|t| &t.after).collect();
        let mut records: Vec<_> = evidence
            .after
            .prerequisites()
            .iter()
            .filter(|p| !installed.contains(&p.object))
            .cloned()
            .collect();
        records.extend(
            compiled
                .prerequisites()
                .iter()
                .filter(|p| installed.contains(&p.object))
                .cloned(),
        );
        records.sort_by(|a, b| a.object.cmp(&b.object));
        let mut json = serde_json::to_value(&evidence.after).unwrap();
        json["prerequisites"] = serde_json::to_value(records).unwrap();
        serde_json::from_value(json).unwrap()
    }

    /// The closing manifest's shape (DEC-1274.1): every opening record the
    /// plan does not remove is kept unchanged, and no installed record is
    /// predicted. One that is present at all is only the placeholder of its
    /// compiled record, carrying identity, ownership and bindings.
    pub(super) fn assert_closing(evidence: &ResolverEvidence, compiled: &InputManifest) {
        let removed: BTreeSet<_> = evidence
            .transitions
            .iter()
            .flat_map(|t| &t.before)
            .collect();
        let installed: BTreeSet<_> = evidence.transitions.iter().flat_map(|t| &t.after).collect();
        for record in evidence.after.prerequisites() {
            let expected = if installed.contains(&record.object) {
                compiled
                    .prerequisites()
                    .iter()
                    .find(|p| p.object == record.object)
                    .unwrap()
                    .managed_closing()
            } else {
                evidence
                    .before
                    .prerequisites()
                    .iter()
                    .find(|p| p.object == record.object)
                    .unwrap()
                    .clone()
            };
            assert_eq!(record, &expected);
        }
        for record in evidence.before.prerequisites() {
            if !removed.contains(&record.object) {
                assert!(evidence.after.prerequisites().contains(record));
            }
        }
    }

    pub(super) fn plan() -> SavedPlan {
        let changes = ChangeSet {
            changes: vec![PlannedChange::new(Change::CreateModule {
                id: "app.v".parse().unwrap(),
                module: Box::new(Module {
                    kind: ModuleKind::View,
                    description: None,
                    definition: "SELECT 1 AS n".into(),
                }),
            })],
        };
        let surface = Surface::Module("app.v".parse().unwrap());
        let evidence = ResolverEvidence::new(
            &changes,
            Qualification {
                rule: "fixture-environment-v1".into(),
                target_environment: "01".repeat(32),
                target_environment_after: "01".repeat(32),
                resolver_environment: "02".repeat(32),
                target_build: "03".repeat(32),
                resolver_build: "04".repeat(32),
                channels: "05".repeat(32),
                runtime: ResolverRuntime::Container {
                    image_digest: format!("sha256:{}", "06".repeat(32)),
                    platform: "linux/amd64".into(),
                    profile: "fixture-v1".into(),
                },
            },
            AuthorizationCondition {
                rule: "fixture-authorization-v1".into(),
                before: "07".repeat(32),
                after: "07".repeat(32),
                changes: BTreeSet::new(),
            },
            manifest(false),
            &manifest(true),
            vec![SurfaceResolution {
                surface: surface.clone(),
                current: None,
                desired: Some(BoundSurface {
                    object: object("app", "v"),
                    bindings: vec![],
                    managed_inputs: BTreeSet::new(),
                }),
            }],
            vec![ObjectTransition {
                surface,
                before: BTreeSet::new(),
                after: BTreeSet::from([object("app", "v")]),
            }],
            OrderingProof::new(&changes, BTreeSet::new()).unwrap(),
        )
        .unwrap();
        SavedPlan::new(
            PlanOrigin::Database,
            "postgres",
            "fixture",
            PlanBaseline {
                description: "fixture".into(),
                checksum: "00".repeat(32),
                database_collation: None,
            },
            changes,
            IdsFile::default(),
        )
        .with_resolution(evidence)
        .unwrap()
    }

    #[test]
    fn planned_candidates_arrive_without_changing_untouched_external_properties() {
        let plan = plan();
        let PlanAnalysis::Resolved(evidence) = &plan.analysis else {
            panic!("resolved")
        };
        let after = evidence.after();
        assert_eq!(
            after.membership()[0].members,
            BTreeSet::from([object("app", "v")])
        );
        assert_eq!(
            after
                .prerequisites()
                .iter()
                .find(|p| p.object == object("ext", "input"))
                .unwrap()
                .properties,
            "bb".repeat(32)
        );
        plan.validate_analysis().unwrap();
        let restored: SavedPlan =
            serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
        restored.validate_analysis().unwrap();
        assert_eq!(restored.checksum(), plan.checksum());
    }

    #[test]
    fn missing_or_downgraded_analysis_never_becomes_an_ordinary_plan() {
        let plan = plan();
        let mut missing = serde_json::to_value(&plan).unwrap();
        missing.as_object_mut().unwrap().remove("analysis");
        assert!(serde_json::from_value::<SavedPlan>(missing).is_err());
        let mut downgraded = plan.clone();
        downgraded.analysis = PlanAnalysis::Ordinary;
        assert!(downgraded.validate_analysis().is_err());
        let mut origin = plan.clone();
        origin.origin = PlanOrigin::Database;
        assert!(origin.validate_analysis().is_err());
        let mut staged = plan.clone().staged();
        assert!(staged.validate_analysis().is_err());
        staged.analysis = PlanAnalysis::Ordinary;
        staged.origin = PlanOrigin::Database;
        staged.validate_analysis().unwrap();
    }

    #[test]
    fn every_required_evidence_component_and_its_version_is_enforced() {
        let plan = plan();
        let json = serde_json::to_value(&plan).unwrap();
        for key in json["analysis"]["evidence"].as_object().unwrap().keys() {
            let mut missing = json.clone();
            missing["analysis"]["evidence"]
                .as_object_mut()
                .unwrap()
                .remove(key);
            assert!(
                serde_json::from_value::<SavedPlan>(missing).is_err(),
                "{key}"
            );
        }
        for (pointer, value) in [
            ("/analysis/evidence/version", json!(3)),
            ("/analysis/evidence/coverage", json!("all-runtime-code")),
            ("/analysis/evidence/handling", json!("public")),
            ("/analysis/evidence/qualification/target_build", json!("")),
            ("/analysis/evidence/ordering/version", json!(9)),
            (
                "/analysis/evidence/authorization/after",
                json!("08".repeat(32)),
            ),
        ] {
            let mut edited = json.clone();
            *edited.pointer_mut(pointer).unwrap() = value;
            if let Ok(parsed) = serde_json::from_value::<SavedPlan>(edited) {
                assert!(parsed.validate_analysis().is_err(), "{pointer}");
                assert_ne!(parsed.checksum(), plan.checksum(), "{pointer}");
            }
        }
    }

    #[test]
    fn closing_external_drift_cannot_hide_behind_an_approved_managed_change() {
        let mut plan = plan();
        let PlanAnalysis::Resolved(evidence) = &mut plan.analysis else {
            panic!("resolved")
        };
        let mut json = serde_json::to_value(&evidence.after).unwrap();
        let row = json["prerequisites"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|p| p["object"]["name"] == json!(["ext", "input"]))
            .unwrap();
        row["properties"] = json!("dd".repeat(32));
        evidence.after = serde_json::from_value(json).unwrap();
        assert_eq!(plan.validate_analysis(), Err(EvidenceError::Projection));
    }
    #[test]
    fn an_approved_grant_changes_only_its_owned_catalog_properties() {
        let template = plan();
        let PlanAnalysis::Resolved(template) = template.analysis else {
            panic!("resolved")
        };
        let changes = ChangeSet {
            changes: vec![PlannedChange::new(Change::Grant {
                role: "reader".into(),
                target: "app.v".parse().unwrap(),
                permissions: BTreeSet::from([crate::Permission::Select]),
            })],
        };
        // The view exists now, observed with its real fingerprint.
        let before = observed(&template, &compiled());
        let mut json = serde_json::to_value(&before).unwrap();
        for record in json["prerequisites"].as_array_mut().unwrap() {
            record["properties"] = json!("ee".repeat(32));
        }
        let compiled: InputManifest = serde_json::from_value(json).unwrap();
        let transition = ObjectTransition {
            surface: Surface::Module("app.v".parse().unwrap()),
            before: BTreeSet::from([object("app", "v")]),
            after: BTreeSet::from([object("app", "v")]),
        };
        let mut authorization = template.authorization.clone();
        authorization.changes = BTreeSet::from([0]);
        let evidence = ResolverEvidence::new(
            &changes,
            template.qualification.clone(),
            authorization,
            before.clone(),
            &compiled,
            vec![],
            vec![transition.clone()],
            OrderingProof::new(&changes, BTreeSet::new()).unwrap(),
        )
        .unwrap();
        // The granted view is the plan's own record: a candidate member, so
        // kept as a placeholder, never its compiled "ee" fingerprint.
        assert!(
            evidence
                .after
                .prerequisites()
                .iter()
                .find(|p| p.object == object("app", "v"))
                .unwrap()
                .is_managed_closing()
        );
        assert_eq!(
            evidence
                .after
                .prerequisites()
                .iter()
                .find(|p| p.object == object("ext", "input"))
                .unwrap()
                .properties,
            "bb".repeat(32)
        );
        assert_closing(&evidence, &compiled);
        assert_eq!(evidence.after.membership(), before.membership());
        assert!(
            before
                .project(&ChangeSet::default(), &compiled, &[transition])
                .is_err()
        );
        let mut changed = changes.clone();
        if let Change::Grant { target, .. } = &mut changed.changes[0].change {
            *target = "app.other".parse().unwrap();
        }
        assert!(
            before
                .project(&changed, &compiled, &evidence.transitions)
                .is_err()
        );
    }

    #[test]
    fn removing_changed_surface_observations_is_not_an_empty_success() {
        let mut plan = plan();
        let PlanAnalysis::Resolved(evidence) = &mut plan.analysis else {
            panic!("resolved")
        };
        evidence.surfaces.clear();
        assert_eq!(plan.validate_analysis(), Err(EvidenceError::Incomplete));
    }

    fn with_records(manifest: &InputManifest, records: Vec<Prerequisite>) -> InputManifest {
        let mut records: Vec<_> = manifest
            .prerequisites()
            .iter()
            .filter(|p| !records.iter().any(|r| r.object == p.object))
            .cloned()
            .chain(records.iter().cloned())
            .collect();
        records.sort_by(|a, b| a.object.cmp(&b.object));
        let mut json = serde_json::to_value(manifest).unwrap();
        json["prerequisites"] = serde_json::to_value(records).unwrap();
        serde_json::from_value(json).unwrap()
    }

    fn resolved(plan: &SavedPlan) -> ResolverEvidence {
        let PlanAnalysis::Resolved(evidence) = &plan.analysis else {
            panic!("resolved")
        };
        (**evidence).clone()
    }

    #[test]
    fn an_installed_record_nothing_references_is_not_in_the_closing_manifest() {
        let plan = plan();
        let template = resolved(&plan);
        // An engine-named internal record of the view: no binding, member,
        // lookup or retained address names it, so only the managed
        // revalidation can check it.
        let internal = Prerequisite {
            object: ObjectIdentity {
                class: "adapter-internal-type".into(),
                name: vec!["v_rowtype".into()],
                signature: vec![],
            },
            ownership: crate::resolver::ObjectOwnership::Surface(Surface::Module(
                "app.v".parse().unwrap(),
            )),
            canonicalization: "fixture-v1".into(),
            properties: "dd".repeat(32),
            bindings: vec![],
        };
        let compiled = with_records(&compiled(), vec![internal.clone()]);
        let mut transitions = template.transitions.clone();
        transitions[0].after.insert(internal.object.clone());
        let evidence = ResolverEvidence::new(
            &plan.changes,
            template.qualification.clone(),
            template.authorization.clone(),
            template.before.clone(),
            &compiled,
            template.surfaces.clone(),
            transitions,
            template.ordering.clone(),
        )
        .unwrap();
        assert!(
            !evidence
                .after
                .prerequisites()
                .iter()
                .any(|p| p.object == internal.object)
        );
        assert_closing(&evidence, &compiled);
        // Its placeholder is not an expectation the plan derives either.
        let mut extra = evidence.clone();
        extra.after = with_records(&evidence.after, vec![internal.managed_closing()]);
        assert_eq!(
            extra.validate(&plan.changes),
            Err(EvidenceError::Projection)
        );
    }

    #[test]
    fn a_referenced_installed_record_is_only_a_placeholder() {
        let plan = plan();
        let evidence = resolved(&plan);
        let compiled = compiled();
        let view = compiled
            .prerequisites()
            .iter()
            .find(|p| p.object == object("app", "v"))
            .unwrap();
        // The candidate membership names the created view, so the closing
        // manifest keeps its identity, ownership and bindings, not its
        // compiled fingerprint.
        let closing = evidence
            .after
            .prerequisites()
            .iter()
            .find(|p| p.object == view.object)
            .unwrap();
        assert!(closing.is_managed_closing());
        assert_eq!(closing, &view.managed_closing());
        assert_ne!(closing.properties, view.properties);
        assert_closing(&evidence, &compiled);
        // A saved closing manifest that predicts the record is refused.
        let mut predicted = evidence.clone();
        predicted.after = with_records(&evidence.after, vec![view.clone()]);
        assert!(predicted.validate(&plan.changes).is_err());
    }

    #[test]
    fn the_opening_manifest_cannot_hold_a_placeholder() {
        let plan = plan();
        let template = resolved(&plan);
        let Change::CreateModule { id, module } = plan.changes.changes[0].change.clone() else {
            panic!("create")
        };
        let changes = ChangeSet {
            changes: vec![PlannedChange::new(Change::AlterModule { id, module })],
        };
        let mut evidence = template.clone();
        evidence.surfaces[0].current = evidence.surfaces[0].desired.clone();
        evidence.transitions[0].before = evidence.transitions[0].after.clone();
        evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
        evidence.before = observed(&template, &compiled());
        evidence.validate(&changes).unwrap();
        // The previous plan's closing manifest is no observation of the view:
        // its placeholder would stand in for the record this plan replaces.
        evidence.before = template.after.clone();
        assert_eq!(evidence.validate(&changes), Err(EvidenceError::Incomplete));
    }

    #[test]
    fn the_closing_manifest_cannot_add_a_placeholder_the_plan_does_not_install() {
        let plan = plan();
        let evidence = resolved(&plan);
        let external = evidence
            .before
            .prerequisites()
            .iter()
            .find(|p| p.object == object("ext", "input"))
            .unwrap();
        let stray = Prerequisite {
            object: object("app", "stray"),
            ..external.clone()
        };
        // An untouched target record can neither be swapped for a
        // placeholder nor joined by one for an object no transition installs.
        for placeholder in [external.managed_closing(), stray.managed_closing()] {
            let mut wrong = evidence.clone();
            wrong.after = with_records(&evidence.after, vec![placeholder]);
            assert_eq!(
                wrong.validate(&plan.changes),
                Err(EvidenceError::Projection)
            );
        }
    }

    #[test]
    fn creating_an_object_the_target_already_holds_untouched_is_refused() {
        let plan = plan();
        let evidence = resolved(&plan);
        // The opening already holds app.v and no transition removes it, so
        // the plan's CREATE would install over a kept input.
        let opening = compiled();
        assert_eq!(
            opening.project(&plan.changes, &compiled(), &evidence.transitions),
            Err(ManifestError::Invalid)
        );
        let mut wrong = evidence.clone();
        wrong.before = opening;
        assert!(wrong.validate(&plan.changes).is_err());
    }

    #[test]
    fn a_placeholder_needs_ownership_its_installing_transition_authorizes() {
        let plan = plan();
        let evidence = resolved(&plan);
        let placeholder = evidence
            .after
            .prerequisites()
            .iter()
            .find(|p| p.is_managed_closing())
            .unwrap()
            .clone();
        evidence.validate(&plan.changes).unwrap();
        for ownership in [
            crate::resolver::ObjectOwnership::Unqualified,
            crate::resolver::ObjectOwnership::Surface(Surface::Module(
                "app.other".parse().unwrap(),
            )),
        ] {
            let mut wrong = evidence.clone();
            wrong.after = with_records(
                &evidence.after,
                vec![Prerequisite {
                    ownership,
                    ..placeholder.clone()
                }],
            );
            assert_eq!(
                wrong.validate(&plan.changes),
                Err(EvidenceError::Projection)
            );
        }
    }
}

#[cfg(test)]
mod transition_tests;

#[cfg(test)]
mod transition_scope_tests;

#[cfg(test)]
mod owner_mutation_scope_tests;

#[cfg(test)]
mod chained_rename_tests;

#[cfg(test)]
mod qualification_phase_tests;
