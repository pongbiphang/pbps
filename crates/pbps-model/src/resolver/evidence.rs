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
    pub target_environment: String,
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
            version: 1,
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
        if self.version != 1
            || self.qualification.rule.is_empty()
            || self.authorization.rule.is_empty()
        {
            return Err(EvidenceError::Version);
        }
        let q = &self.qualification;
        if [
            &q.target_environment,
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
            let surface = match &step.change {
                Change::CreateModule { id, .. }
                | Change::AlterModule { id, .. }
                | Change::DropModule { id, .. } => Some(super::Surface::Module(id.clone())),
                // A generation expression is the column's `pg_attrdef` row,
                // as a default is (DEC-1168.1).
                Change::AlterColumnDefault { column, .. }
                | Change::AlterColumnExpression { column, .. } => {
                    Some(super::Surface::Default(column.clone()))
                }
                Change::AddColumn {
                    table,
                    name,
                    column,
                    ..
                } if column.default.is_some() => Some(super::Surface::Default(table.column(name))),
                Change::AddCheck { table, name, .. } | Change::DropCheck { table, name } => {
                    Some(super::Surface::Check {
                        table: table.clone(),
                        name: name.clone(),
                    })
                }
                Change::AddIndex {
                    table, name, index, ..
                } if index.holds_expression() => Some(super::Surface::Index {
                    table: table.clone(),
                    name: name.clone(),
                }),
                _ => None,
            };
            if let Some(surface) = surface
                && !self.surfaces.iter().any(|s| s.surface == surface)
            {
                return Err(EvidenceError::Incomplete);
            }
        }

        if self
            .before
            .project(changes, &self.after, &self.transitions)?
            != self.after
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
            for (observed, manifest) in [
                (&surface.current, &self.before),
                (&surface.desired, &self.after),
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
            ("/analysis/evidence/version", json!(2)),
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
        let before = template.after.clone();
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
        assert_eq!(
            evidence
                .after
                .prerequisites()
                .iter()
                .find(|p| p.object == object("app", "v"))
                .unwrap()
                .properties,
            "ee".repeat(32)
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
}

#[cfg(test)]
mod transition_tests;

#[cfg(test)]
mod transition_scope_tests;

#[cfg(test)]
mod owner_mutation_scope_tests;

#[cfg(test)]
mod chained_rename_tests;
