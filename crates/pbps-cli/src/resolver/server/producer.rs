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

pub(super) fn fingerprint(
    key: &EnvironmentFingerprintKey,
    rule: &str,
    component: &str,
    bytes: &[u8],
) -> String {
    crate::resolver::sealing::hex(key.fingerprint(rule, component, bytes))
}

/// The qualified scope a resolved plan is sealed against, whichever run
/// qualified it.
pub(super) struct Sealing<'a> {
    pub planned: &'a [super::PlannedGrant],
    /// Unprojected opening target facts.
    pub opening_catalog: &'a pbps_db::resolver::environment::CatalogFacts,
    /// The same read, visibility expected after the plan's grants.
    pub target_catalog: &'a pbps_db::resolver::environment::CatalogFacts,
    pub scratch_catalog: &'a pbps_db::resolver::environment::CatalogFacts,
    pub authorization: &'a scope::Authorization,
    pub schemas: &'a [String],
    pub write_path_extras: &'a [String],
}

/// What the evidence says about the run that produced it: the rule that
/// qualified the scope, the scratch build, the channels and the runtime.
pub(super) struct Identity {
    pub rule: &'static str,
    pub resolver_build: String,
    /// Unkeyed; fingerprinted under the environment key here.
    pub channels: String,
    pub runtime: ResolverRuntime,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn seal(
    key: &EnvironmentFingerprintKey,
    sealed: &Sealing<'_>,
    identity: Identity,
    producer: super::ProducerOutcome,
    opening: pbps_model::resolver::InputManifest,
    ordered: pbps_diff::resolver::Ordered,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
) -> Result<ResolvedPlan, Error> {
    let changes = ordered.changes;
    let grants = scope::planned_schema_grants(&changes).map_err(Error::Scope)?;
    if grants != sealed.planned {
        return Err(Error::Scope(
            "the final ordered grants disagree with the qualified scope".into(),
        ));
    }
    let mut projected = sealed.opening_catalog.clone();
    sealed.authorization.project_visibility(
        &grants,
        sealed.schemas,
        sealed.write_path_extras,
        &mut projected,
    );
    if projected != *sealed.target_catalog {
        return Err(Error::Scope(
            "the final grant projection disagrees with the qualified target facts".into(),
        ));
    }
    let authorization = sealed
        .authorization
        .persisted(key, &changes)
        .map_err(Error::Scope)?;
    if matches!(sealed.authorization, scope::Authorization::Mssql(_)) {
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
        target_environment: catalog(sealed.opening_catalog)?,
        target_environment_after: catalog(&projected)?,
        resolver_environment: catalog(sealed.scratch_catalog)?,
        target_build: producer.opening_build,
        resolver_build: identity.resolver_build,
        channels: fingerprint(
            key,
            "pbps/pg-channels/v1",
            "qualified-connections",
            identity.channels.as_bytes(),
        ),
        runtime: identity.runtime,
    };
    let evidence = ResolverEvidence::new(
        &changes,
        qualification,
        authorization,
        opening,
        &compiled,
        producer.surfaces,
        transitions,
        ordered.proof,
    )
    .map_err(|error| Error::Binding(format!("closing evidence projection: {error}")))?;
    Ok(ResolvedPlan { changes, evidence })
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
