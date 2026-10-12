//! Sealing a run's answer into the plan's evidence, and the run's errors.

use super::{Error, ResolvedPlan, ScopeRequest, transitions};
use crate::resolver::scope;
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_model::Hints;
use pbps_model::resolver::{Qualification, ResolverEvidence, ResolverRuntime};
use std::collections::BTreeSet;

pub(super) fn request(
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    hints: &Hints,
    extras: &[String],
) -> Result<ScopeRequest, Error> {
    let dialect = pbps_pg::Postgres::with_write_path_extras(extras.to_vec());
    // The same plan `pbps_diff::resolver::plan` starts from: without
    // ADR-0013's candidate rebuilds, or the scope qualified here and the
    // grants sealed at the end could describe two different plans.
    let ordinary =
        pbps_diff::resolver::ordinary(base, desired, &dialect, hints).map_err(|errors| {
            Error::Binding(format!("ordinary typed plan cannot be formed: {errors:?}"))
        })?;
    let planned = scope::planned_schema_grants(&ordinary).map_err(Error::Scope)?;
    let mut schemas = BTreeSet::new();
    for schema in [base.schema, desired.schema] {
        schemas.extend(schema.tables.keys().map(|name| name.schema.clone()));
        schemas.extend(schema.modules.keys().map(|id| id.schema().to_owned()));
    }
    schemas.extend(planned.iter().map(|grant| grant.schema.clone()));
    Ok(ScopeRequest {
        schemas: schemas.into_iter().collect(),
        write_path_extras: extras.to_vec(),
        planned,
    })
}

/// The domain the sealed cluster identity is keyed under. The publication
/// recheck and apply key the identifier their own session reports the same
/// way and compare digests (#1516, #616).
pub(crate) const CLUSTER_RULE: &str = "pbps/pg-cluster-identity/v1";

pub(super) fn fingerprint(
    key: &EnvironmentFingerprintKey,
    rule: &str,
    component: &str,
    bytes: &[u8],
) -> String {
    crate::resolver::sealing::hex(key.fingerprint(rule, component, bytes))
}

/// The qualified scope a resolved plan is sealed against, whichever run
/// qualified it, and what the run observed. Held by value: the seal waits
/// until the caller's ordinary passes have run on the plan, after the
/// scratch is released (#1516).
pub(super) struct Sealing {
    pub planned: Vec<super::PlannedGrant>,
    /// Unprojected opening target facts.
    pub opening_catalog: pbps_db::resolver::environment::CatalogFacts,
    /// The same read, visibility expected after the plan's grants.
    pub target_catalog: pbps_db::resolver::environment::CatalogFacts,
    pub scratch_catalog: pbps_db::resolver::environment::CatalogFacts,
    pub authorization: scope::Authorization,
    pub schemas: Vec<String>,
    pub write_path_extras: Vec<String>,
    pub identity: Identity,
    pub producer: super::ProducerOutcome,
    pub opening: pbps_model::resolver::InputManifest,
}

/// What the evidence says about the run that produced it: the rule that
/// qualified the scope, the scratch build, the channels and the runtime.
pub(super) struct Identity {
    pub rule: &'static str,
    pub resolver_build: String,
    /// Unkeyed; fingerprinted under the environment key here.
    pub channels: String,
    pub runtime: ResolverRuntime,
    /// The planning read's `system_identifier`, unkeyed; fingerprinted
    /// under the environment key here (#1516).
    pub target_cluster: String,
    /// The differences the rule accepted, by name (#1700).
    pub limitations: std::collections::BTreeMap<String, String>,
}

/// Seals `changes`, the plan in its final order, with `proof` over that
/// order.
pub(super) fn seal(
    key: &EnvironmentFingerprintKey,
    sealed: Sealing,
    changes: pbps_model::ChangeSet,
    proof: pbps_model::resolver::OrderingProof,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
) -> Result<ResolvedPlan, Error> {
    let Sealing {
        planned,
        opening_catalog,
        target_catalog,
        scratch_catalog,
        authorization,
        schemas,
        write_path_extras,
        identity,
        producer,
        opening,
    } = sealed;
    let grants = scope::planned_schema_grants(&changes).map_err(Error::Scope)?;
    if grants != planned {
        return Err(Error::Scope(
            "the final ordered grants disagree with the qualified scope".into(),
        ));
    }
    let mut projected = opening_catalog.clone();
    authorization.project_visibility(&grants, &schemas, &write_path_extras, &mut projected);
    if projected != target_catalog {
        return Err(Error::Scope(
            "the final grant projection disagrees with the qualified target facts".into(),
        ));
    }
    let persisted = authorization
        .persisted(key, &changes)
        .map_err(Error::Scope)?;
    if matches!(authorization, scope::Authorization::Mssql(_)) {
        return Err(Error::Scope("SQL Server evidence is unsupported".into()));
    }
    let compiled_records = producer
        .compiled
        .planning_records()
        .map_err(|error| Error::Binding(error.to_string()))?;
    // Logical ownership records are safe to use before the private raw
    // scratch capture is sealed exactly once under the selected key.
    let opening_records = super::resolution::records(&opening);
    let transitions =
        transitions::derive(&changes, base, desired, &opening_records, &compiled_records)?;
    let compiled = producer
        .compiled
        .seal_for_plan()
        .map_err(|error| Error::Binding(format!("final compiled catalog manifest: {error}")))?;
    let catalog = |facts| {
        crate::resolver::sealing::target_catalog_fingerprint(key, facts)
            .map_err(|reason| Error::Scope(reason.into()))
    };
    let qualification = Qualification {
        rule: identity.rule.into(),
        target_environment: catalog(&opening_catalog)?,
        target_environment_after: catalog(&projected)?,
        resolver_environment: catalog(&scratch_catalog)?,
        target_build: producer.opening_build,
        resolver_build: identity.resolver_build,
        channels: fingerprint(
            key,
            "pbps/pg-channels/v1",
            "qualified-connections",
            identity.channels.as_bytes(),
        ),
        runtime: identity.runtime,
        target_cluster: fingerprint(
            key,
            CLUSTER_RULE,
            "system-identifier",
            identity.target_cluster.as_bytes(),
        ),
        limitations: identity.limitations,
    };
    let evidence = ResolverEvidence::new(
        &changes,
        qualification,
        persisted,
        opening,
        &compiled,
        producer.surfaces,
        transitions,
        proof,
    )
    .map_err(|error| Error::Binding(format!("closing evidence projection: {error}")))?;
    Ok(ResolvedPlan { changes, evidence })
}

/// A run's answer before its evidence is sealed (#1516). The run is over and
/// its scratch released. The caller runs its ordinary passes on
/// [`Unsealed::changes`], [rechecks](Unsealed::recheck) the target, then
/// [seals](Unsealed::seal) the plan in its final order, so the sealed order
/// already holds whatever those passes added or moved (DEC-1516.1).
pub struct Unsealed {
    pub(super) key: EnvironmentFingerprintKey,
    pub(super) ordered: pbps_diff::resolver::Ordered,
    pub(super) sealing: Sealing,
    pub(super) opening: Opening,
}

/// What the run read on the target, for the publication recheck.
pub(super) struct Opening {
    pub driver: pbps_db::Driver,
    pub request: ScopeRequest,
    pub scope_schemas: Vec<String>,
    /// The opening capture: its scope, which holds the baseline's staged
    /// objects, and its dropped signatures are captured again.
    pub current: pbps_pg::resolver::capture::CapturedInputs,
    /// The `system_identifier` the planning read recorded (DEC-1685.1).
    pub planned_identity: String,
}

impl Unsealed {
    /// The plan in the resolver's own order.
    pub fn changes(&self) -> &pbps_model::ChangeSet {
        &self.ordered.changes
    }

    /// Seals `changes`, the resolver's plan after the caller's passes. Every
    /// edge the resolver ordered must still hold in the final order, and no
    /// change it ordered may be gone: a pass that undoes the resolver's
    /// order is a defect, refused here rather than sealed (#1516). Only a
    /// passed [recheck](Self::recheck) makes the [`Rechecked`] it takes.
    pub fn seal(
        self,
        _rechecked: Rechecked,
        changes: pbps_model::ChangeSet,
        base: pbps_diff::Side<'_>,
        desired: pbps_diff::Side<'_>,
    ) -> Result<ResolvedPlan, Error> {
        let edges = remap(&self.ordered.changes, &changes, self.ordered.proof.edges())?;
        let proof = pbps_model::resolver::OrderingProof::new(&changes, edges)
            .map_err(|error| Error::Binding(format!("the final order: {error}")))?;
        seal(&self.key, self.sealing, changes, proof, base, desired)
    }

    /// The fresh pre-publication recheck (#1516). The session must reach
    /// the cluster the planning read recorded (DEC-1685.1). The target's
    /// reported facts and its deployer's authorization are read again and
    /// must equal the run's opening read. The opening capture's scope,
    /// which holds the objects the baseline staged, is captured afresh and
    /// must equal it. Any difference refuses: the evidence would describe
    /// a target that is no longer there.
    pub async fn recheck(&self, target: &mut pbps_db::Conn) -> Result<Rechecked, Error> {
        let read = |what: &'static str| {
            move |error: pbps_db::DbError| Error::Read(format!("the recheck's {what}: {error}"))
        };
        let identity = pbps_pg::resolver::scratch::cluster_identity(target)
            .await
            .map_err(read("cluster identity"))?;
        if identity != self.opening.planned_identity {
            return Err(Error::Changed(vec![
                "the recheck reached another cluster than the planning read".into(),
            ]));
        }
        let request = &self.opening.request;
        let (catalog, authorization) = scope::read_target(
            target,
            self.opening.driver,
            &request.schemas,
            &request.write_path_extras,
            &self.opening.scope_schemas,
            &request.planned,
        )
        .await
        .map_err(read("analysis scope"))?;
        #[cfg(test)]
        let catalog = {
            let mut catalog = catalog;
            if let Ok(Some(build)) = RECHECK_BUILD.try_with(Clone::clone) {
                catalog.observations.insert(
                    "server_version".into(),
                    pbps_db::resolver::Observation::Observed { value: build },
                );
            }
            catalog
        };
        let mut changed = Vec::new();
        if catalog != self.sealing.opening_catalog {
            changed.push("the target's reported facts".to_owned());
        }
        let canonical = |authorization: &scope::Authorization| match authorization {
            scope::Authorization::Postgres(context) => Some(context.canonical()),
            scope::Authorization::Mssql(_) => None,
        };
        if canonical(&authorization).is_none()
            || canonical(&authorization) != canonical(&self.sealing.authorization)
        {
            changed.push("the target deployer's authorization".to_owned());
        }
        let (_, differences) = pbps_pg::resolver::capture::recapture(target, &self.opening.current)
            .await
            .map_err(|error| Error::Read(format!("the recheck's capture: {error}")))?;
        changed.extend(differences.iter().map(|difference| {
            let change = serde_json::to_value(difference.change)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            match &difference.object {
                Some(object) => format!("{} {} ({change})", object.class, object.name.join(".")),
                None => format!("the capture's {change}"),
            }
        }));
        if changed.is_empty() {
            Ok(Rechecked(()))
        } else {
            Err(Error::Changed(changed))
        }
    }
}

/// That the target passed the publication recheck. Made only by
/// [`Unsealed::recheck`] and taken by [`Unsealed::seal`]: a plan cannot be
/// sealed, and so cannot be published, past a recheck it skipped (#1516).
pub struct Rechecked(());

#[cfg(test)]
tokio::task_local! {
    /// The build string the target reports to a test's recheck: a minor
    /// upgrade between the run and the recheck, which the pinned servers
    /// cannot perform (#1700).
    pub(super) static RECHECK_BUILD: Option<String>;
}

/// The resolver's edges, from its own order to `after`, the order the
/// caller's passes left. A change is matched by what it does, in order
/// among equal ones; a pass may add changes and their findings, but not
/// remove one the resolver ordered or put one before another the resolver
/// ordered after it.
fn remap(
    before: &pbps_model::ChangeSet,
    after: &pbps_model::ChangeSet,
    edges: &BTreeSet<pbps_model::resolver::OrderEdge>,
) -> Result<BTreeSet<pbps_model::resolver::OrderEdge>, Error> {
    let mut taken = vec![false; after.changes.len()];
    let mut at = Vec::with_capacity(before.changes.len());
    for planned in &before.changes {
        let Some(found) = after
            .changes
            .iter()
            .enumerate()
            .position(|(i, p)| !taken[i] && p.change == planned.change)
        else {
            return Err(Error::Binding(format!(
                "a planning pass removed a change the resolver ordered: {}",
                describe(&planned.change)
            )));
        };
        taken[found] = true;
        at.push(found);
    }
    edges
        .iter()
        .map(|edge| {
            let (before_at, after_at) = (at[edge.before], at[edge.after]);
            if before_at < after_at {
                Ok(pbps_model::resolver::OrderEdge {
                    before: before_at,
                    after: after_at,
                    reason: edge.reason,
                })
            } else {
                Err(Error::Binding(format!(
                    "a planning pass put `{}` before `{}`, which the resolver orders after it",
                    describe(&after.changes[after_at].change),
                    describe(&after.changes[before_at].change)
                )))
            }
        })
        .collect()
}

/// A change as a refusal names it: its kind and what it acts on.
fn describe(change: &pbps_model::Change) -> String {
    let op = serde_json::to_value(change)
        .ok()
        .and_then(|v| v.get("op").and_then(|op| op.as_str()).map(str::to_owned))
        .unwrap_or_default();
    format!("{op} {}", change.subject())
}

/// What stopped a production run before it returned evidence (#1514).
#[derive(Debug, thiserror::Error)]
pub enum ProduceError {
    #[error("the resolver profile could not be acquired: {0}")]
    Acquire(String),
    /// The run refused; nothing it owned is left behind.
    #[error("{0}")]
    Run(Error),
    /// Cleanup was not confirmed: these exact names may remain.
    #[error("the resolver run's cleanup was not confirmed; remove exactly: {}", .0.join(", "))]
    Cleanup(Vec<String>),
}

#[cfg(test)]
mod remap_tests {
    use super::{Error, remap};
    use pbps_model::resolver::{OrderEdge, OrderReason};
    use pbps_model::{Change, ChangeSet, PlannedChange, Uid, UidKind};
    use std::collections::BTreeSet;

    fn role(name: &str) -> PlannedChange {
        PlannedChange::new(Change::CreateRole {
            uid: Uid::generate(UidKind::Role),
            name: name.into(),
        })
    }

    fn set(changes: &[&PlannedChange]) -> ChangeSet {
        ChangeSet {
            changes: changes.iter().map(|&c| c.clone()).collect(),
        }
    }

    fn edge(before: usize, after: usize) -> OrderEdge {
        OrderEdge {
            before,
            after,
            reason: OrderReason::Binding,
        }
    }

    #[test]
    fn a_pass_that_adds_and_moves_changes_keeps_each_edge_at_its_new_places() {
        let (a, b, added) = (role("a"), role("b"), role("added"));
        let edges = remap(
            &set(&[&a, &b]),
            &set(&[&added, &a, &b]),
            &BTreeSet::from([edge(0, 1)]),
        )
        .unwrap();
        assert_eq!(edges, BTreeSet::from([edge(1, 2)]));
    }

    #[test]
    fn a_pass_that_removes_a_change_the_resolver_ordered_is_refused() {
        let (a, b) = (role("a"), role("b"));
        let refused = remap(&set(&[&a, &b]), &set(&[&b]), &BTreeSet::from([edge(0, 1)]));
        assert!(
            matches!(&refused, Err(Error::Binding(why)) if why.contains("removed") && why.contains("role a")),
            "{refused:?}"
        );
    }

    #[test]
    fn a_pass_that_reverses_a_resolver_edge_is_refused() {
        let (a, b) = (role("a"), role("b"));
        let refused = remap(
            &set(&[&a, &b]),
            &set(&[&b, &a]),
            &BTreeSet::from([edge(0, 1)]),
        );
        assert!(
            matches!(&refused, Err(Error::Binding(why)) if why.contains("role b") && why.contains("role a")),
            "{refused:?}"
        );
        // Negative: the same swap with no edge between them is a free move.
        assert!(
            remap(&set(&[&a, &b]), &set(&[&b, &a]), &BTreeSet::new())
                .unwrap()
                .is_empty()
        );
    }
}
