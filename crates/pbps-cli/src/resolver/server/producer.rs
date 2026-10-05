//! Qualified, fixed-key evidence production on one owned ScratchRun.

use super::{
    BindingRequest, Error, NativeTarget, ResolvedPlan, RunControl, ScopeRequest, ScratchRun,
    Verdict, scope, transitions,
};
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_model::Hints;
use pbps_model::resolver::{Qualification, ResolverEvidence, ResolverRuntime};
use std::collections::BTreeSet;

fn request(
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

fn fingerprint(
    key: &EnvironmentFingerprintKey,
    rule: &str,
    component: &str,
    bytes: &[u8],
) -> String {
    crate::resolver::sealing::hex(key.fingerprint(rule, component, bytes))
}

fn resolver_build(
    key: &EnvironmentFingerprintKey,
    observed: &pbps_db::resolver::environment::ExecutableSet,
) -> Result<String, Error> {
    use pbps_db::resolver::environment::{ExecutableIdentity, ExecutableRole, Provenance};
    let readable = |identity: &ExecutableIdentity| {
        identity.digest.as_ref().is_some_and(|digest| {
            digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
        }) && matches!(
            (&identity.role, &identity.provenance),
            (
                ExecutableRole::Engine | ExecutableRole::Preloaded,
                Provenance::LoadedContent
            ) | (ExecutableRole::LateLoaded, Provenance::DiskCandidate)
        )
    };
    if observed.engine.role != ExecutableRole::Engine
        || !readable(&observed.engine)
        || !observed.libraries.iter().all(readable)
    {
        return Err(Error::Scope(
            "the resolver executable content is incomplete or unreadable".into(),
        ));
    }
    // Qualified paths are names inside the owned process's mount namespace.
    // They preserve loader-name/content association without recording a host
    // PID, connection ID or run-generated resource name.
    let mut measured = observed.clone();
    measured.libraries.sort_by_cached_key(|library| {
        serde_json::to_vec(library).expect("qualified executable serializes")
    });
    let bytes = serde_json::to_vec(&measured)
        .map_err(|_| Error::Scope("resolver build cannot be encoded".into()))?;
    Ok(fingerprint(
        key,
        "pbps/pg-resolver-build/v1",
        "qualified-executables",
        &bytes,
    ))
}

fn runtime(run: &RunControl, key: &EnvironmentFingerprintKey) -> Result<ResolverRuntime, Error> {
    match run {
        RunControl::Supplied(inner) => {
            let pinned = &inner.control.pinned;
            let bytes = serde_json::to_vec(&(
                &pinned.id,
                &pinned.image,
                &inner.control.identity.instance_key,
            ))
            .map_err(|_| Error::Scope("supplied runtime identity cannot be encoded".into()))?;
            // The admitted profile, not a family name: each one enforces its
            // own layout, and the PG16 profile also pins the server major.
            Ok(ResolverRuntime::Supplied {
                profile: inner.control.profile.name.into(),
                identity: fingerprint(key, "pbps/pg-runtime/v1", "supplied", &bytes),
            })
        }
        RunControl::Container(control) => Ok(ResolverRuntime::Container {
            image_digest: control
                .image_digest()
                .ok_or(Error::Unqualified("the owned image identity is missing"))?
                .into(),
            platform: "linux/amd64".into(),
            profile: "linux-amd64-v1".into(),
        }),
    }
}

impl ScratchRun {
    /// Bind the environment key before any fresh read. A fresh run qualifies
    /// itself; a previously qualified run is accepted only under the exact
    /// same ordered extras, schemas and preliminary grant sequence.
    #[allow(clippy::too_many_arguments)]
    pub async fn plan_resolved(
        &mut self,
        target: &mut NativeTarget,
        binding: &BindingRequest<'_>,
        base: pbps_diff::Side<'_>,
        desired: pbps_diff::Side<'_>,
        hints: &Hints,
        write_path_extras: &[String],
        project: &pbps_config::Project,
        environment: Option<&str>,
    ) -> Result<ResolvedPlan, Error> {
        if self.inner.driver() != pbps_db::Driver::Postgres {
            return Err(Error::Binding(
                "persistent resolver evidence has no SQL Server adapter".into(),
            ));
        }
        if binding.base != base.schema || binding.desired != desired.schema {
            return Err(Error::Binding(
                "the binding request and recorded planning sides disagree".into(),
            ));
        }
        let key = crate::resolver::sealing::environment_key(project, environment)
            .map_err(|error| Error::Binding(error.to_string()))?;
        let request = request(base, desired, hints, write_path_extras)?;
        if let Some(sealed) = &self.scope {
            if sealed.report.verdict() != Verdict::Verified
                || sealed.schemas != request.schemas
                || sealed.write_path_extras != request.write_path_extras
                || sealed.planned != request.planned
            {
                return Err(Error::Scope(
                    "the existing verified scope does not match the planning request".into(),
                ));
            }
        } else {
            // A mismatch was measured and is the finding; an unknown fact
            // was not, and stays operational (SPEC 9.8).
            let error = match self.qualify(target, &request).await? {
                Verdict::Verified => None,
                Verdict::Mismatch(facts) => Some(Error::Incompatible(facts)),
                Verdict::Unknown(facts) => Some(Error::Scope(format!(
                    "the analysis scope could not be established: {}",
                    facts.join("; ")
                ))),
            };
            if let Some(error) = error {
                self.refuse_and_retire(error.clone());
                return Err(error);
            }
        }
        let outcome = self
            .resolve_with_key(target, binding, Some(&key), Some((base, desired)))
            .await?;
        let result = (|| {
            let sealed = self.scope.as_ref().ok_or(Error::Cancelled)?;
            let producer = outcome.producer.ok_or(Error::Cancelled)?;
            let opening = outcome.opening.ok_or(Error::Cancelled)?;
            let dialect = pbps_pg::Postgres::with_write_path_extras(write_path_extras.to_vec());
            let ordered =
                pbps_diff::resolver::plan(base, desired, hints, &producer.surfaces, &dialect)
                    .map_err(|error| Error::Binding(error.to_string()))?;
            self.seal_ordered(&key, sealed, producer, opening, ordered, base, desired)
        })();
        if let Err(cause) = &result {
            self.refuse_and_retire(cause.clone());
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn seal_ordered(
        &self,
        key: &EnvironmentFingerprintKey,
        sealed: &super::QualifiedScope,
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
        sealed.authorization_context.project_visibility(
            &grants,
            &sealed.schemas,
            &sealed.write_path_extras,
            &mut projected,
        );
        if projected != sealed.target.catalog {
            return Err(Error::Scope(
                "the final grant projection disagrees with the qualified target facts".into(),
            ));
        }
        let authorization = sealed
            .authorization_context
            .persisted(key, &changes)
            .map_err(Error::Scope)?;
        if matches!(sealed.authorization_context, scope::Authorization::Mssql(_)) {
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
        let qualification = Qualification {
            rule: pbps_pg::resolver::compatibility::RULE.into(),
            target_environment: crate::resolver::sealing::target_catalog_fingerprint(
                key,
                &sealed.opening_catalog,
            )
            .map_err(|reason| Error::Scope(reason.into()))?,
            target_environment_after: crate::resolver::sealing::target_catalog_fingerprint(
                key, &projected,
            )
            .map_err(|reason| Error::Scope(reason.into()))?,
            resolver_environment: crate::resolver::sealing::target_catalog_fingerprint(
                key,
                &sealed.scratch_facts.catalog,
            )
            .map_err(|reason| Error::Scope(reason.into()))?,
            target_build: producer.opening_build,
            resolver_build: resolver_build(key, &sealed.scratch_facts.executables)?,
            channels: fingerprint(
                key,
                "pbps/pg-channels/v1",
                "qualified-connections",
                format!(
                    "{:?}|{:?}",
                    sealed.target_connection, sealed.scratch_connection
                )
                .as_bytes(),
            ),
            runtime: runtime(&self.inner, key)?,
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
}

#[cfg(test)]
mod released_tests {
    use super::{Error, ProduceError, released};

    /// A cleanup that succeeded leaves nothing to name, whatever the refusal
    /// listed before it ran; a failed one names only what it could not
    /// confirm (#1514 review).
    #[test]
    fn only_unconfirmed_cleanup_is_reported_after_a_refusal() {
        assert!(matches!(
            released(Error::Scratch, None),
            ProduceError::Run(Error::Scratch)
        ));
        assert!(matches!(
            released(Error::Scratch, Some(Vec::new())),
            ProduceError::Run(Error::Scratch)
        ));
        let ProduceError::Cleanup(names) = released(Error::Scratch, Some(vec!["db".into()])) else {
            panic!("an unconfirmed cleanup is named");
        };
        assert_eq!(names, ["db"]);
    }
}

/// The native Docker daemon's socket. Only a root-owned socket closed to
/// other users is admitted (`LocalApi::connect_native`); a rootless or
/// proxied daemon needs its own measured profile (RESOLVER-RUNTIME).
pub const NATIVE_DOCKER_SOCKET: &str = "/var/run/docker.sock";

/// What stopped a production run before it returned evidence (#1514).
#[derive(Debug, thiserror::Error)]
pub enum ProduceError {
    #[error(transparent)]
    Target(#[from] crate::resolver::native::TargetConnectError),
    #[error("the target's database recipe could not be read: {0}")]
    Recipe(crate::resolver::native::EnvironmentError),
    #[error("the resolver profile could not be acquired: {0}")]
    Acquire(String),
    /// The run or its admission refused; nothing it owned is left behind.
    #[error("{0}")]
    Run(Error),
    /// Cleanup was not confirmed: these exact names may remain.
    #[error("the resolver run's cleanup was not confirmed; remove exactly: {}", .0.join(", "))]
    Cleanup(Vec<String>),
}

impl From<super::ServerFailure> for ProduceError {
    fn from(failure: super::ServerFailure) -> Self {
        if failure.recovery_names.is_empty() {
            Self::Run(failure.cause)
        } else {
            Self::Cleanup(failure.recovery_names)
        }
    }
}

/// One production resolver run for a connected plan (DEC-1514.1), for the
/// target's driver as the caller chose it. It binds the
/// selected target to its native service, opens the profile's scratch run,
/// produces the sealed order and evidence, and closes the run before any
/// result leaves. Every refusal has already released what the run owned, or
/// names exactly what it could not confirm.
#[allow(clippy::too_many_arguments)]
pub async fn produce(
    driver: pbps_db::Driver,
    profile: &pbps_config::resolver::ResolverProfile,
    target_connection: &str,
    binding: &BindingRequest<'_>,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    hints: &Hints,
    write_path_extras: &[String],
    project: &pbps_config::Project,
    environment: Option<&str>,
) -> Result<ResolvedPlan, ProduceError> {
    produce_with(
        std::path::Path::new(NATIVE_DOCKER_SOCKET),
        driver,
        profile,
        target_connection,
        binding,
        base,
        desired,
        hints,
        write_path_extras,
        project,
        environment,
    )
    .await
}

/// [`produce`] with the Docker socket a fixture supplies.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn produce_with(
    docker_socket: &std::path::Path,
    driver: pbps_db::Driver,
    profile: &pbps_config::resolver::ResolverProfile,
    target_connection: &str,
    binding: &BindingRequest<'_>,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    hints: &Hints,
    write_path_extras: &[String],
    project: &pbps_config::Project,
    environment: Option<&str>,
) -> Result<ResolvedPlan, ProduceError> {
    // The caller's driver, chosen where the CLI chooses engines
    // (`db::driver_for`): this run names none, so an engine without evidence
    // reaches `plan_resolved`'s named refusal rather than a parse error.
    let mut target = NativeTarget::connect(driver, target_connection).await?;
    let recipe = target
        .database_recipe()
        .await
        .map_err(ProduceError::Recipe)?;
    let mut run = open_run(docker_socket, profile, &mut target, &recipe).await?;
    let result = run
        .plan_resolved(
            &mut target,
            binding,
            base,
            desired,
            hints,
            write_path_extras,
            project,
            environment,
        )
        .await;
    // The run is released before anything reads the result, so a refusal
    // after it cannot strand the scratch server or its container.
    let closed = run.close().await;
    match (result, closed) {
        (_, Err(failure)) if !failure.recovery_names.is_empty() => {
            Err(ProduceError::Cleanup(failure.recovery_names))
        }
        (Err(refused), _) => Err(ProduceError::Run(refused)),
        (Ok(_), Err(failure)) => Err(ProduceError::Run(failure.cause)),
        (Ok(plan), Ok(())) => Ok(plan),
    }
}

/// A refusal after its runtime's cleanup ran. The refusal's own recovery
/// names were the obligations before that cleanup; only what the cleanup
/// itself could not confirm may remain, so only those are reported.
fn released(cause: Error, cleanup: Option<Vec<String>>) -> ProduceError {
    match cleanup {
        Some(names) if !names.is_empty() => ProduceError::Cleanup(names),
        Some(_) | None => ProduceError::Run(cause),
    }
}

/// The profile's scratch run. A refusal after the runtime started releases
/// it before returning, as the run's own close would.
async fn open_run(
    docker_socket: &std::path::Path,
    profile: &pbps_config::resolver::ResolverProfile,
    target: &mut NativeTarget,
    recipe: &pbps_db::resolver::environment::DatabaseRecipe,
) -> Result<ScratchRun, ProduceError> {
    use pbps_config::resolver::ResolverProfile;
    match profile {
        ResolverProfile::Docker { .. } => {
            let mut api = crate::resolver::docker::LocalApi::connect_native(docker_socket)
                .await
                .map_err(|error| ProduceError::Acquire(error.to_string()))?;
            let image = api
                .acquire(profile)
                .await
                .map_err(|error| ProduceError::Acquire(error.to_string()))?;
            let mut candidate =
                crate::resolver::docker::CandidateSession::start(api, image, target)
                    .await
                    .map_err(|failure| {
                        if failure.recovery_names.is_empty() {
                            ProduceError::Acquire(failure.cause.to_string())
                        } else {
                            ProduceError::Cleanup(failure.recovery_names)
                        }
                    })?;
            match candidate.open_scratch(recipe).await {
                Ok(run) => Ok(run),
                Err(refused) => Err(released(
                    refused.cause,
                    candidate
                        .close()
                        .await
                        .err()
                        .map(|failure| failure.recovery_names),
                )),
            }
        }
        ResolverProfile::Server { .. } => {
            let endpoint =
                super::ScratchEndpoint::from_profile(profile).map_err(ProduceError::Run)?;
            let mut server = super::DedicatedServer::admit(endpoint, target).await?;
            match server.open_scratch(recipe).await {
                Ok(run) => Ok(run),
                Err(refused) => Err(released(
                    refused.cause,
                    server
                        .discard()
                        .await
                        .err()
                        .map(|failure| failure.recovery_names),
                )),
            }
        }
    }
}
