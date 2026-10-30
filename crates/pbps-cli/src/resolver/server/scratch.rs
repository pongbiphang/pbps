//! The resolver's scratch run, on a server the operator supplies or a
//! container started for the run (DEC-1528.1; #1667, #1672, #1674).
//!
//! The operator vouches for the scratch server: its isolation, the
//! confidentiality of what is compiled on it, and the channels to it. pbps
//! observes no process, proves no containment and reads no executable, so
//! the target may be anywhere its connection string reaches. What the run
//! still establishes, before it writes anything on scratch, is that it can
//! touch nothing that is not its own:
//!
//! - the scratch database is never the target's, read from the engines and
//!   not from the two strings, which can spell one server two ways;
//! - PostgreSQL keeps a confined account on the target's cluster; SQL Server
//!   permits only supplied layout there and enters a loginless sandbox;
//! - the database holds only the engine's own initial catalog inventory.
//!
//! The account decides how the run provisions scratch. On PostgreSQL a superuser on another
//! cluster gets a run-owned login and database, the deployer's authorization reproduced, the
//! declarations compiled as the reproduced deployer, all dropped afterwards.
//! Any other account compiles as itself in the database its connection names,
//! which it must own, and leaves it empty again with `DROP OWNED`. The scope
//! is qualified under the compatibility rule that compares what each engine
//! reports about itself.
//! SQL Server records its run-owned resources before creation too, executes
//! setup and externals in NO REVERT sandboxes, compiles as the reproduced deployer, and
//! refuses where planning from evidence begins (#2143).

use super::producer::{Identity, Opening, Sealing, Unsealed, request};
use super::{BindingRequest, Error, ProduceError, ResolvedPlan, engine, resolution};
use crate::resolver::scope;
use pbps_db::resolver::environment::Verdict;
use pbps_db::{Conn, Driver};
use pbps_model::Hints;
use pbps_model::resolver::{OutsideWrites, ResolverRuntime, ScratchLayout, ScratchSource};

pub use super::engine::{Standard, declared_standard};

pub mod domain;
pub mod preview;
pub mod recovery;
pub mod rehearse;

/// One run of the resolver for a connected plan. `scratch`
/// is the connection string the selected resolver's `url_env` holds. The run
/// releases everything it created before any result leaves, and a cleanup it
/// could not confirm names exactly what may remain.
#[allow(clippy::too_many_arguments)]
pub async fn produce(
    driver: Driver,
    scratch: &str,
    target_connection: &str,
    planned_identity: &str,
    binding: &BindingRequest<'_>,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    hints: &Hints,
    write_path_extras: &[String],
    project: &pbps_config::Project,
    environment: Option<&str>,
    standard: impl Into<Standard>,
    externals: Option<&str>,
    provenance: &Provenance,
) -> Result<ResolvedPlan, ProduceError> {
    let unsealed = produce_unsealed(
        driver,
        scratch,
        target_connection,
        planned_identity,
        binding,
        base,
        desired,
        hints,
        write_path_extras,
        project,
        environment,
        standard,
        externals,
        provenance,
    )
    .await?;
    // Rechecked as `plan` rechecks it, so every answer this returns has
    // passed the recheck too.
    let mut session = Conn::connect(driver, target_connection)
        .await
        .map_err(|error| {
            ProduceError::Run(Error::Read(format!("the recheck's session: {error}")))
        })?;
    let rechecked = unsealed
        .recheck(&mut session)
        .await
        .map_err(|refused| ProduceError::Run(refused.error))?;
    let changes = rechecked.changes().clone();
    rechecked
        .seal(changes, base, desired)
        .map_err(ProduceError::Run)
}

/// [`produce`]'s run, its plan returned before the evidence is sealed: the
/// publishing caller runs its ordinary passes and the recheck first (#1516).
#[allow(clippy::too_many_arguments)]
pub async fn produce_unsealed(
    driver: Driver,
    scratch: &str,
    target_connection: &str,
    planned_identity: &str,
    binding: &BindingRequest<'_>,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    hints: &Hints,
    write_path_extras: &[String],
    project: &pbps_config::Project,
    environment: Option<&str>,
    standard: impl Into<Standard>,
    externals: Option<&str>,
    provenance: &Provenance,
) -> Result<Unsealed, ProduceError> {
    if binding.base != base.schema || binding.desired != desired.schema {
        return Err(ProduceError::Run(Error::Binding(
            "the binding request and recorded planning sides disagree".into(),
        )));
    }
    let key = crate::resolver::sealing::environment_key(project, environment)
        .map_err(|error| ProduceError::Run(Error::Binding(error.to_string())))?;
    let request =
        request(driver, base, desired, hints, write_path_extras).map_err(ProduceError::Run)?;
    let standard = standard.into();
    // Before either connection opens: a LocalDB instance that cannot be found
    // is known without touching the target, and every connection of the run
    // then opens the one endpoint found here (DEC-2271.2).
    let scratch = &engine::endpoint(driver, scratch).await?;
    let mut target = Conn::connect(driver, target_connection)
        .await
        .map_err(|error| unusable(format!("the target could not be connected: {error}")))?;
    // Before the scratch session connects, which carries it from its first
    // packet (#2410).
    let tokens = Tokens::generate();
    let mut admin = engine::connect_scratch(driver, scratch, &tokens).await?;
    let (provisioning, state) = engine::scratch(
        driver,
        &mut target,
        &mut admin,
        &tokens,
        planned_identity,
        scratch,
        &request,
        &standard,
        externals,
        provenance,
    )
    .await?;
    let mut run = Run {
        driver,
        planned_identity,
        key: &key,
        request: &request,
        tokens: &tokens,
        externals,
        provenance,
        state,
    };
    let result = run
        .resolve(
            provisioning,
            &mut target,
            &mut admin,
            binding,
            base,
            desired,
            hints,
        )
        .await;
    let cleanup = run.state.release(&mut admin).await;
    drop(run);
    match (result, cleanup) {
        (_, Err(names)) => Err(ProduceError::Cleanup(names)),
        (Err(refused), Ok(())) => Err(ProduceError::Run(refused)),
        (Ok((ordered, sealing, opening)), Ok(())) => Ok(Unsealed {
            key,
            ordered,
            sealing,
            opening,
        }),
    }
}

/// What the evidence records of the scratch a run used (#1674): the entry
/// that configured it too (#1517). Its layout is the run's own finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub entry: String,
    pub scratch: ScratchSource,
    pub outside_writes: OutsideWrites,
    pub same_instance: pbps_model::resolver::SameInstance,
}

impl Provenance {
    /// A server entry named `scratch`, its externals file's outside writes refused.
    #[cfg(test)]
    pub fn server() -> Self {
        Self {
            entry: "scratch".into(),
            scratch: ScratchSource::Server {},
            outside_writes: OutsideWrites::Refused,
            same_instance: pbps_model::resolver::SameInstance::NotApplicable,
        }
    }
}

fn unusable(reason: impl Into<String>) -> ProduceError {
    ProduceError::Run(Error::Unusable(reason.into()))
}

/// The run's two session tokens: run-generated, so a token found in
/// `pg_stat_activity` is this run's session and no other.
///
/// `scratch` is also the run's token (#2410): its keeper session and every
/// resource it creates carry it.
pub(super) struct Tokens {
    pub(super) target: String,
    pub(super) scratch: String,
}

impl Tokens {
    pub(super) fn generate() -> Self {
        let token = || format!("{:032x}", rand::random::<u128>());
        Self {
            target: token(),
            scratch: token(),
        }
    }
}

/// How the run provisions its scratch database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Provisioning {
    /// A run-owned login and database, created and dropped by the run.
    RunOwned,
    /// The database the connection names, owned by the scratch account.
    Supplied,
}

struct Run<'a> {
    driver: Driver,
    planned_identity: &'a str,
    key: &'a pbps_db::fingerprint::EnvironmentFingerprintKey,
    request: &'a super::ScopeRequest,
    tokens: &'a Tokens,
    externals: Option<&'a str>,
    provenance: &'a Provenance,
    state: engine::Scratch<'a>,
}

impl Run<'_> {
    #[allow(clippy::too_many_arguments)]
    async fn resolve(
        &mut self,
        provisioning: Provisioning,
        target: &mut Conn,
        admin: &mut Conn,
        binding: &BindingRequest<'_>,
        base: pbps_diff::Side<'_>,
        desired: pbps_diff::Side<'_>,
        hints: &Hints,
    ) -> Result<
        (
            pbps_diff::resolver::Ordered<pbps_diff::order::Canonical<pbps_pg::Postgres>>,
            Sealing,
            Opening,
        ),
        Error,
    > {
        let driver = self.driver;
        let request = self.request;
        let db = |what: &'static str| {
            move |error: pbps_db::DbError| Error::Read(format!("{what}: {error}"))
        };
        let scope_schemas =
            scope::scope_schemas(driver, &request.schemas, &request.write_path_extras)
                .map_err(Error::Scope)?;
        // The target as its own deployer sees it, in one snapshot.
        let (mut target_catalog, authorization, history) = scope::read_target_with_history(
            target,
            driver,
            &request.schemas,
            &request.write_path_extras,
            &scope_schemas,
            &request.planned,
        )
        .await
        .map_err(db("the target's analysis scope"))?;
        let ambiguous = authorization.unpredictable(&request.planned);
        if !ambiguous.is_empty() {
            return Err(Error::Scope(format!(
                "the grantor of a planned grant cannot be predicted on the target: {}",
                ambiguous.join("; ")
            )));
        }
        let opening_catalog = target_catalog.clone();
        // What the externals file lacks is filled from the target (#1778,
        // DEC-1778.2); which of it the file holds is known once it has run.
        let mut prepared = self
            .state
            .prepare(
                provisioning,
                admin,
                target,
                &target_catalog,
                &authorization,
                &scope_schemas,
                binding,
                &request.write_path_extras,
                Some(self.key),
            )
            .await?;
        // The expected visibility is the deployer's after the plan's own
        // grants.
        authorization.project_visibility(
            &request.planned,
            &request.schemas,
            &request.write_path_extras,
            &mut target_catalog,
        );
        #[cfg(test)]
        if let Ok(Some(build)) = SCRATCH_BUILD.try_with(Clone::clone) {
            prepared.catalog_mut().observations.insert(
                "server_version".into(),
                pbps_db::resolver::Observation::Observed { value: build },
            );
        }
        let mut report = scope::compare_reported(driver, &target_catalog, prepared.catalog())
            .map_err(Error::Scope)?;
        for key in prepared.differences() {
            report.facts.insert(
                format!("authorization:{key}"),
                pbps_db::resolver::environment::FactStatus::Mismatch {
                    target: "the target deployer's authorization".into(),
                    resolver: "not reproduced on scratch".into(),
                },
            );
        }
        let mut limitations = report.limitations.clone();
        if let Some((name, reason)) = history.limitation() {
            limitations.insert(name.into(), reason.into());
        }
        match report.verdict() {
            Verdict::Verified => {}
            Verdict::Mismatch(facts) => return Err(Error::Incompatible(facts)),
            Verdict::Unknown(facts) => {
                return Err(Error::Scope(format!(
                    "the analysis scope could not be established: {}",
                    facts.join("; ")
                )));
            }
        }
        let extras = &request.write_path_extras;
        let mut reconstruction = self
            .state
            .reconstruction(
                target,
                binding,
                desired.ids,
                &hints.module_deps,
                &mut prepared,
            )
            .await?;
        let chains = reconstruction.chains(&prepared);
        let mut staged = if self.externals.is_some() || prepared.has_filled() {
            self.state
                .compare_externals(provisioning, &mut prepared, admin, target, &reconstruction)
                .await?
        } else {
            engine::staged(driver, &prepared)?
        };
        #[cfg(test)]
        if let Ok(Some(sql)) = AFTER_STAGING.try_with(Clone::clone) {
            target
                .execute(&sql)
                .await
                .map_err(db("the test's target write"))?;
        }
        // The target's zone, read before the scratch is compiled in it.
        let zone = target.session_zone().cloned();
        let outside_writes = self.provenance.outside_writes;
        let unfillable = prepared.unfillable();
        let failed = |error: Error, staged: &engine::Staged| {
            let error = staged.through_shape_view(error);
            if let Error::Creation(mut failure) = error {
                if !(chains.is_empty() && unfillable.is_empty()) {
                    let context = std::sync::Arc::make_mut(&mut failure);
                    context.externals = true;
                    context
                        .context
                        .extend(chains.iter().chain(&unfillable).cloned());
                }
                return Error::Creation(failure);
            }
            if let Error::Binding(reason) = &error
                && !(chains.is_empty() && unfillable.is_empty())
            {
                return Error::Externals(
                    chains
                        .iter()
                        .chain(&unfillable)
                        .cloned()
                        .chain([reason.clone()])
                        .collect(),
                );
            }
            error
        };
        reconstruction
            .compile(
                &mut prepared,
                admin,
                target,
                extras,
                zone.as_ref(),
                outside_writes,
                &mut staged,
            )
            .await
            .map_err(|error| failed(error, &staged))?;
        let compiled = reconstruction
            .capture_desired(
                &mut prepared,
                admin,
                target,
                extras,
                &target_catalog.visibility,
                &request.schemas,
                self.key,
                desired,
                &staged,
            )
            .await?;
        let (current, opening) = reconstruction
            .capture_opening(target, &compiled, self.key, base)
            .await?;
        // The shapes the externals file was compared with must be the ones this
        // capture seals: a target written between the two reads refuses.
        let changed = staged.changed_since(&current);
        if !changed.is_empty() {
            return Err(Error::Read(format!(
                "the target changed while the run read it, after the externals file was compared \
                 with it: {}; run again",
                changed.join("; ")
            )));
        }
        let assessment = reconstruction.assess(&compiled, &current, &staged)?;
        let records = compiled
            .planning_records()
            .map_err(|error| Error::Binding(error.to_string()))?;
        let surfaces = engine::surfaces(
            driver,
            base,
            desired,
            &resolution::records(&opening),
            &records,
            &assessment,
        )?;
        let ordered = reconstruction.plan(base, desired, hints, extras, &surfaces)?;
        let identity = Identity {
            rule: engine::reported_rule(driver)?,
            resolver_build: engine::reported_build(driver, self.key, prepared.catalog())?,
            channels: format!("scratch|{}|{}", self.tokens.target, self.tokens.scratch),
            runtime: ResolverRuntime::Scratch {
                entry: self.provenance.entry.as_str().into(),
                scratch: self.provenance.scratch.clone(),
                layout: match provisioning {
                    Provisioning::RunOwned => ScratchLayout::RunOwned,
                    Provisioning::Supplied => ScratchLayout::Supplied,
                },
                outside_writes: self.provenance.outside_writes,
                same_instance: self.provenance.same_instance,
            },
            target_cluster: self.planned_identity.to_owned(),
            limitations,
        };
        let producer = super::ProducerOutcome {
            compiled,
            opening_build: engine::reported_build(driver, self.key, &opening_catalog)?,
            surfaces,
        };
        let sealing = Sealing {
            planned: request.planned.clone(),
            opening_catalog,
            target_catalog,
            scratch_catalog: prepared.into_catalog(),
            authorization,
            schemas: request.schemas.clone(),
            write_path_extras: request.write_path_extras.clone(),
            identity,
            producer,
            opening,
        };
        let recheck = Opening {
            history,
            driver,
            request: request.clone(),
            scope_schemas,
            current,
            planned_identity: self.planned_identity.to_owned(),
        };
        Ok((ordered, sealing, recheck))
    }
}

#[cfg(test)]
tokio::task_local! {
    /// SQL a test runs on the target between the externals file's comparison and
    /// the opening capture: a writer the run cannot otherwise be timed
    /// against (#1755).
    static AFTER_STAGING: Option<String>;
    pub(super) static FILL_FAILURE2134: Option<(String, String)>;
    /// The build string a test's scratch reports instead of its own: the
    /// pinned servers run one image, and `server_version` is read-only, so
    /// a differently packaged scratch is otherwise out of reach (#1700).
    pub(super) static SCRATCH_BUILD: Option<String>;
}

pub mod container;

#[cfg(test)]
pub(super) mod fixtures;

#[cfg(test)]
mod live_tests;

#[cfg(test)]
mod mssql_live_tests;

// Windows LocalDB is the only place these can run (#2271).
#[cfg(all(test, windows))]
mod localdb_live_tests;

#[cfg(test)]
use super::engine::release_supplied;

#[cfg(test)]
mod tests {
    use super::{BindingRequest, Driver, Error, ProduceError, Provenance, produce_unsealed};
    use pbps_model::{Hints, IdsFile, Schema};

    /// Both engines validate the binding sides and key before connecting.
    #[tokio::test]
    async fn every_engine_validates_binding_sides_and_key_before_connections() {
        let schema = Schema::default();
        let mut different = Schema::default();
        different
            .tables
            .insert("app.other".parse().unwrap(), Default::default());
        let ids = IdsFile::default();
        let side = pbps_diff::Side {
            schema: &schema,
            ids: &ids,
        };
        let project = pbps_config::Project {
            root: std::env::temp_dir(),
            config: pbps_config::Config::parse(
                "dialect: postgres\n",
                std::path::Path::new("pbps.yml"),
            )
            .unwrap(),
        };
        let hints = Hints::default();
        let standard = pbps_pg::resolver::standard::Standard::default();
        let provenance = Provenance::server();
        for disagrees in [false, true] {
            let binding = BindingRequest {
                dependencies: &Default::default(),
                bootstrap: &pbps_diff::diff(
                    pbps_diff::Side {
                        schema: &pbps_model::Schema::default(),
                        ids: &pbps_model::IdsFile::default(),
                    },
                    pbps_diff::Side {
                        schema: &pbps_model::Schema::default(),
                        ids: &pbps_model::IdsFile::default(),
                    },
                    &pbps_dialect::MinimalDialect,
                    &pbps_model::Hints::default(),
                )
                .unwrap(),
                base: if disagrees { &different } else { &schema },
                desired: &schema,
            };
            let run = async |driver| match produce_unsealed(
                driver,
                "invalid scratch connection",
                "invalid target connection",
                "",
                &binding,
                side,
                side,
                &hints,
                &[],
                &project,
                None,
                &standard,
                None,
                &provenance,
            )
            .await
            {
                Err(ProduceError::Run(Error::Binding(reason))) => reason,
                Err(other) => panic!("not the first binding refusal: {other}"),
                Ok(_) => panic!("a run without a key or connections"),
            };
            let postgres = run(Driver::Postgres).await;
            assert_eq!(run(Driver::Mssql).await, postgres);
            if disagrees {
                assert_eq!(
                    postgres,
                    "the binding request and recorded planning sides disagree"
                );
            } else {
                assert_eq!(
                    postgres,
                    "resolver evidence requires the target environment's fingerprint key: \
                    a bare --db target names no configured environment. Generate one with \
                    `pbps key generate`, configure `fingerprint_key_env` or `fingerprint_key_file`, \
                    and plan with `--env` again"
                );
            }
        }
    }

    /// A LocalDB selector is resolved before the target or the scratch server
    /// is touched: the target here would refuse a connection if it were tried.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn a_localdb_selector_is_resolved_before_any_connection_opens() {
        use super::fixtures::{ENVIRONMENT, ProjectKey};
        let mut key = ProjectKey::new(true);
        let config = "dialect: mssql\nenvironments:\n  fixture:\n    url_env: PBPS_2271_UNUSED\n    \
            fingerprint_key_file: key\n";
        let path = key.project.root.join("pbps.yml");
        std::fs::write(&path, config).unwrap();
        key.project.config = pbps_config::Config::parse(config, &path).unwrap();
        let schema = Schema::default();
        let ids = IdsFile::default();
        let side = pbps_diff::Side {
            schema: &schema,
            ids: &ids,
        };
        let bootstrap =
            pbps_diff::resolver::ordinary(side, side, &pbps_mssql::Mssql, &Hints::default())
                .unwrap();
        let outcome = produce_unsealed(
            Driver::Mssql,
            r"Server=(localdb)\x;Integrated Security=SSPI",
            "Server=127.0.0.1,1;User Id=sa;Password=never-tried",
            "",
            &BindingRequest {
                dependencies: &Default::default(),
                bootstrap: &bootstrap,
                base: &schema,
                desired: &schema,
            },
            side,
            side,
            &Hints::default(),
            &[],
            &key.project,
            Some(ENVIRONMENT),
            super::Standard::Mssql,
            None,
            &Provenance::server(),
        )
        .await;
        match outcome {
            Err(ProduceError::Acquire(reason)) => {
                assert!(reason.contains("LocalDB instance"), "{reason}");
            }
            Err(other) => panic!("not the LocalDB acquire failure: {other}"),
            Ok(_) => panic!("a selector with no Windows to resolve it was accepted"),
        }
    }
}
