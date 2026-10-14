//! The three ways a connected plan meets resolver selection (DEC-1515.1).
//!
//! 1. No resolver selected: ADR-0013's candidate rebuild and the
//!    unmanaged-dependent gate decide, as they did before any resolver
//!    existed. Nothing here runs, not even the assessment.
//! 2. A resolver selected, and the assessment needs none: the ordinary plan,
//!    and nothing resolver-only is opened — no key, no Docker socket, no
//!    scratch connection.
//! 3. A resolver selected, and some question needs it: the resolver answers,
//!    or deployable output is refused. Its answer is published after the
//!    ordinary connected checks and a fresh recheck of the target
//!    (DEC-1516.1).

use pbps_config::resolver::ResolverSelection;
use pbps_diff::resolver::{Answer, Assessment};

/// Which case a connected plan is in.
pub enum Case<'a> {
    Fallback,
    NotNeeded {
        assessment: Assessment,
    },
    Required {
        selection: &'a ResolverSelection,
        assessment: Assessment,
    },
}

/// Decide the case for the ordinary plan of `base -> desired`. Pure: it
/// reads the typed plan and nothing else, so deciding opens nothing.
pub fn case<'a>(
    selection: Option<&'a ResolverSelection>,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    ordinary: &pbps_model::ChangeSet,
    dialect: &dyn pbps_dialect::Dialect,
) -> Case<'a> {
    let Some(selection) = selection else {
        return Case::Fallback;
    };
    let assessment = pbps_diff::resolver::assess(base, desired, ordinary, dialect);
    if assessment.requires_resolution() {
        Case::Required {
            selection,
            assessment,
        }
    } else {
        Case::NotNeeded { assessment }
    }
}

/// What the selected resolver was to this plan: not needed (case 2), or
/// needed and answering, its plan published (case 3, #1516).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct ResolverAssessment {
    pub need: ResolutionNeed,
    /// Kept surfaces nothing in the plan can rebind.
    pub unaffected: usize,
    /// Kept surfaces the plan recreates from their declarations anyway.
    pub rebuilt: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionNeed {
    /// The lightweight assessment answered every binding question.
    NotNeeded,
    /// The resolver answered the questions the assessment could not, and
    /// the plan carries its sealed evidence (#1516).
    Resolved,
}

impl ResolverAssessment {
    pub fn not_needed(assessment: &Assessment) -> Self {
        Self {
            need: ResolutionNeed::NotNeeded,
            unaffected: assessment.count(Answer::Unaffected),
            rebuilt: assessment.count(Answer::Rebuild),
        }
    }

    pub fn resolved(assessment: &Assessment) -> Self {
        Self {
            need: ResolutionNeed::Resolved,
            ..Self::not_needed(assessment)
        }
    }
}

/// How case 3 failed to produce a deployable answer. The split is SPEC 9.8's
/// and DECISIONS 485's: a question the run answered — unresolved, or the
/// environment measured incompatible — is a finding; a run that could not
/// answer is not.
pub enum Refused {
    Finding(crate::output::Finding),
    Unanswerable(anyhow::Error),
}

/// The surfaces only the engine can answer, as a report names them.
pub fn named(assessment: &Assessment) -> String {
    let names: Vec<String> = assessment.unresolved().map(describe).collect();
    names.join(", ")
}

fn describe(surface: &pbps_model::resolver::Surface) -> String {
    use pbps_model::resolver::Surface;
    match surface {
        Surface::Module(id) => id.to_string(),
        Surface::Default(column) => format!("the default of {column}"),
        Surface::Check { table, name } => format!("check {name} on {table}"),
        Surface::Index { table, name } => format!("index {name} on {table}"),
        Surface::Namespace(schema) => format!("schema {schema}"),
        Surface::Table(table) => table.to_string(),
        Surface::Column(column) => column.to_string(),
    }
}

/// Everything case 3 needs from the plan being made.
pub struct Request<'a> {
    pub selection: &'a ResolverSelection,
    pub assessment: &'a Assessment,
    pub target: &'a crate::db::Target,
    pub project: &'a pbps_config::Project,
    pub base: pbps_diff::Side<'a>,
    pub desired: pbps_diff::Side<'a>,
    pub hints: &'a pbps_model::Hints,
    /// The `system_identifier` of the cluster the planning read reached, for
    /// a PostgreSQL target. The resolver's own target connection must reach
    /// the same one: a target name can reach several (#1685).
    pub target_identity: Option<&'a str>,
}

/// The `system_identifier` of the cluster `conn` reached, for a PostgreSQL
/// target: the planning read's, which the resolver's own target connection
/// must match, since a target name can reach several clusters (#1685).
/// Only PostgreSQL is resolved, and resolution refuses the other engines by
/// name, so they record none. Both kinds of resolver are bound by it: a
/// Docker scratch, like a supplied server, answers for the cluster the plan
/// was read from (#1674), so neither plans for a role refused the read.
pub async fn planning_identity(
    conn: &mut pbps_db::Conn,
    _selection: &ResolverSelection,
) -> Result<Option<String>, Refused> {
    if conn.driver() != pbps_db::Driver::Postgres {
        return Ok(None);
    }
    pbps_pg::resolver::scratch::cluster_identity(conn)
        .await
        .map(Some)
        .map_err(identity_unread)
}

/// A refused read of the cluster's identity is the deployment role's to
/// fix, and names the grant; any other failure is unanswerable.
fn identity_unread(error: pbps_db::DbError) -> Refused {
    if error.server_error_code().as_deref() == Some("42501") {
        return Refused::Finding(
            crate::output::Finding::error(
                "resolver.identity",
                format!(
                    "the resolver binds its answer to the target's database cluster, and the \
                     deployment role may not read the cluster's identity ({error})"
                ),
            )
            .remedy(
                "GRANT EXECUTE ON FUNCTION pg_catalog.pg_control_system() TO <the deployment role>",
            ),
        );
    }
    Refused::Unanswerable(anyhow::anyhow!(
        "the target cluster's identity could not be read: {error}"
    ))
}

fn unresolved(request: &Request<'_>, why: impl std::fmt::Display) -> Refused {
    Refused::Finding(
        crate::output::Finding::error(
            "resolver.unresolved",
            format!(
                "the selected resolver `{}` could not answer the binding questions of {}: {why}.\n\
                 Deployable output needs every one answered (ADR-0016 §1).",
                request.selection.name,
                named(request.assessment)
            ),
        )
        .remedy(
            "Repair the declaration so its bindings are unambiguous, add what the target \
             retains to the resolver's baseline, or select no resolver and keep the existing \
             protection.",
        ),
    )
}

/// A case the resolver does not answer, refused by name before it runs
/// (#1517): the gate and what to do instead, as `resolver.unresolved`.
fn gated(
    selection: &ResolverSelection,
    assessment: &Assessment,
    gate: &str,
    remedy: &str,
) -> crate::output::Finding {
    crate::output::Finding::error(
        "resolver.unresolved",
        format!(
            "the selected resolver `{}` cannot answer the binding questions of {}: {gate}.",
            selection.name,
            named(assessment)
        ),
    )
    .remedy(remedy)
}

/// `--staged` with a plan that needs the resolver: its evidence describes
/// one transaction (#1516), so the gate refuses before the resolver runs,
/// and before any target read a gate does not need, which the deployment
/// role might not be allowed (#1945 review).
pub fn staged(selection: &ResolverSelection, assessment: &Assessment) -> crate::output::Finding {
    gated(
        selection,
        assessment,
        "--staged cannot carry its answer, because a resolver-backed plan applies in one \
         transaction",
        "Plan this change without --staged.",
    )
}

/// Case 3: ask the selected resolver for the plan it seals. Only this path
/// reads the key or opens a resolver resource, and it reads the key first,
/// so a missing key costs no image and no scratch server.
///
/// Everything here holds for any profile: the engine with a binding adapter,
/// the key every persisted fingerprint needs (DEC-952.1), the bootstrap and
/// the refusal classes. Only [`producer::produce`] is profile-specific
/// (#1528), so a profile that binds its target differently plugs in there.
///
/// The answer comes back unsealed: the caller runs its ordinary passes on
/// it, rechecks the target, then seals it (#1516).
pub async fn resolve(
    request: &Request<'_>,
) -> Result<pbps_cli::resolver::server::Unsealed, Refused> {
    let driver = request.target.driver();
    if driver != pbps_db::Driver::Postgres {
        return Err(Refused::Finding(gated(
            request.selection,
            request.assessment,
            "the resolver answers for PostgreSQL only; SQL Server resolution is not built yet \
             (#619, #620)",
            "Select no resolver for this environment: without one, the existing protection \
             keeps deciding (DEC-1515.1).",
        )));
    }
    if let Err(missing) =
        pbps_cli::resolver::sealing::environment_key(request.project, request.target.environment())
    {
        return Err(Refused::Finding(
            crate::output::Finding::error("resolver.key", missing.to_string())
                .remedy("pbps key generate"),
        ));
    }
    let dialect = crate::dialect_for(request.project.config.dialect);
    let empty = pbps_model::Schema::default();
    let bootstrap: Vec<pbps_model::Change> = pbps_diff::diff(
        pbps_diff::Side {
            schema: &empty,
            ids: &pbps_model::IdsFile::default(),
        },
        request.desired,
        dialect.as_ref(),
        request.hints,
    )
    .map_err(|errors| {
        Refused::Unanswerable(
            anyhow::Error::new(crate::Unexpressible(errors))
                .context("the declarations' bootstrap cannot be formed"),
        )
    })?
    .changes
    .into_iter()
    .map(|planned| planned.change)
    .collect();
    producer::produce(request, driver, &bootstrap).await
}

/// A resolver's baseline file, read only from under the project root.
///
/// The baseline is reviewed history like the declarations (SPEC §9.3.2), so
/// it is never a file elsewhere on the planning machine: an absolute path, a
/// `..` or a symlink that leads outside the root is refused. Judged on the
/// resolved path, not lexically, because a symlink inside the root can point
/// anywhere. The file read is the one judged.
fn read_baseline(root: &std::path::Path, path: &std::path::Path) -> Result<String, String> {
    let unreadable = |error: std::io::Error| format!("could not be read: {error}");
    // Relative, as the configuration says: an absolute path would work from
    // one checkout only, even one inside the root (`join` replaces the root
    // with it).
    if path.has_root() {
        return Err("is an absolute path; name the baseline relative to the project root".into());
    }
    let root = root.canonicalize().map_err(unreadable)?;
    let file = root.join(path).canonicalize().map_err(unreadable)?;
    if !file.starts_with(&root) {
        return Err(format!(
            "lies outside the project root {}; a baseline is a file in the project",
            root.display()
        ));
    }
    std::fs::read_to_string(&file).map_err(unreadable)
}

/// The resolver's producer: a scratch run on a supplied server or a Docker
/// container, bound to the target by the cluster identity the planning read
/// recorded (DEC-1685.1). It reaches its scratch with SQL alone, so it runs
/// on every platform pbps supports (#1636).
mod producer {
    use super::{Refused, Request, unresolved};
    use pbps_cli::resolver::server::{Error, ProduceError};

    pub(super) async fn produce(
        request: &Request<'_>,
        driver: pbps_db::Driver,
        bootstrap: &[pbps_model::Change],
    ) -> Result<pbps_cli::resolver::server::Unsealed, Refused> {
        let binding = pbps_cli::resolver::server::BindingRequest {
            bootstrap,
            desired: request.desired.schema,
            base: request.base.schema,
        };
        // Both kinds are the one resolver (DEC-1528.1): a
        // supplied server, or a container started for the run (#1674).
        use pbps_cli::resolver::server::scratch::{self, Provenance};
        use pbps_config::resolver::ResolverProfile;
        let Some(identity) = request.target_identity else {
            return Err(Refused::Unanswerable(anyhow::anyhow!(
                "the planning read recorded no cluster identity for the resolver to bind"
            )));
        };
        let (baseline, outside_writes) = match &request.selection.profile {
            ResolverProfile::Server {
                baseline,
                outside_writes,
                ..
            }
            | ResolverProfile::Docker {
                baseline,
                outside_writes,
                ..
            } => (baseline, *outside_writes),
        };
        let baseline = baseline
            .as_ref()
            .map(|path| {
                super::read_baseline(request.project.root(), path).map_err(|why| {
                    Refused::Unanswerable(anyhow::anyhow!(
                        "the resolver `{}` names the baseline {}, which {why}",
                        request.selection.name,
                        path.display()
                    ))
                })
            })
            .transpose()?;
        let outside_writes = match outside_writes {
            pbps_config::resolver::OutsideWrites::Refuse => {
                pbps_model::resolver::OutsideWrites::Refused
            }
            pbps_config::resolver::OutsideWrites::Allow => {
                pbps_model::resolver::OutsideWrites::Allowed
            }
        };
        let run = async |scratch: &str,
                         standard: &pbps_pg::resolver::standard::Standard,
                         provenance: Provenance| {
            scratch::produce_unsealed(
                driver,
                scratch,
                request.target.connection(),
                identity,
                &binding,
                request.base,
                request.desired,
                request.hints,
                // The CLI's PostgreSQL dialect configures no write-path
                // extras (`dialect_for`), so the plan it diffed has none.
                &[],
                request.project,
                request.target.environment(),
                standard,
                baseline.as_deref(),
                &provenance,
            )
            .await
        };
        let produced = match &request.selection.profile {
            ResolverProfile::Server {
                url_env, standard, ..
            } => {
                let scratch = scratch_connection(request, url_env)?;
                let provenance = Provenance {
                    entry: request.selection.name.to_owned(),
                    scratch: pbps_model::resolver::ScratchSource::Server {},
                    outside_writes,
                };
                run(
                    &scratch,
                    &scratch::declared_standard(standard.as_ref()),
                    provenance,
                )
                .await
            }
            ResolverProfile::Docker { image, pull, .. } => {
                let provenance = Provenance {
                    entry: request.selection.name.to_owned(),
                    scratch: pbps_model::resolver::ScratchSource::Docker {
                        image: image.clone(),
                    },
                    outside_writes,
                };
                scratch::container::with_container(
                    driver,
                    request.target.connection(),
                    identity,
                    image,
                    *pull,
                    async |scratch: &str| {
                        run(scratch, &scratch::declared_standard(None), provenance).await
                    },
                )
                .await
            }
        };
        produced.map_err(|failure| refused(request, failure))
    }

    fn refused(request: &Request<'_>, failure: ProduceError) -> Refused {
        match failure {
            ProduceError::Run(error) if answered(&error) => unresolved(request, error),
            ProduceError::Run(error) => Refused::Unanswerable(error.into()),
            ProduceError::Acquire(_) | ProduceError::Cleanup(_) => {
                Refused::Unanswerable(failure.into())
            }
        }
    }

    /// The scratch server's connection string, from the variable the
    /// resolver names. Never the target's own variable: one string would
    /// then be both, and nothing after this could tell them apart by name.
    fn scratch_connection(request: &Request<'_>, url_env: &str) -> Result<String, Refused> {
        let target_env = request.target.environment().and_then(|name| {
            request
                .project
                .config
                .environments
                .get(name)
                .map(|environment| environment.url_env.as_str())
        });
        if target_env == Some(url_env) {
            return Err(Refused::Unanswerable(anyhow::anyhow!(
                "the resolver `{}` names the target's own connection variable {url_env}; \
                 a scratch server needs its own",
                request.selection.name
            )));
        }
        std::env::var(url_env).map_err(|_| {
            Refused::Unanswerable(anyhow::anyhow!(
                "the resolver `{}` reads its scratch server from {url_env}, which is not set",
                request.selection.name
            ))
        })
    }

    /// Whether a run's failure is an answer: `resolver.unresolved`, exit 2,
    /// rather than `unanswerable`, exit 1 (SPEC 9.8).
    pub(super) fn answered(error: &Error) -> bool {
        match error {
            // A declared surface had no sound verdict, or the environment was
            // read and differs.
            Error::Binding(_) | Error::Incompatible(_) => true,
            // The recheck read the target in full and found it other than
            // the run read it (#1516). Never a run's failure: the recheck
            // runs after the run, and `deploy` names it `resolver.recheck`.
            Error::Changed(_) => true,
            // Not answered: the run could not establish what it needed. A
            // catalog read that failed is one of these, however far into the
            // binding it came (#1575).
            Error::Scratch
            | Error::Scope(_)
            | Error::Read(_)
            // A scratch that cannot be used as configured answered nothing,
            // nor did one whose baseline is not the target's.
            | Error::Unusable(_)
            | Error::Baseline(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Column, Hints, IdsFile, Module, ModuleKind, Schema, Table};

    // Unix only: the fixture makes Unix symlinks.
    #[test]
    #[cfg(unix)]
    fn a_baseline_is_read_only_from_under_the_project_root() {
        use std::path::{Path, PathBuf};
        let dir = std::env::temp_dir().join(format!(
            "pbps-baseline-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = dir.join("project");
        std::fs::create_dir_all(root.join("db")).unwrap();
        std::fs::write(root.join("db/baseline.sql"), "CREATE SCHEMA ext;").unwrap();
        std::fs::write(dir.join("outside.sql"), "CREATE SCHEMA other;").unwrap();
        std::os::unix::fs::symlink(dir.join("outside.sql"), root.join("db/link.sql")).unwrap();
        std::os::unix::fs::symlink(root.join("db/baseline.sql"), root.join("inside.sql")).unwrap();

        assert_eq!(
            read_baseline(&root, Path::new("db/baseline.sql")).as_deref(),
            Ok("CREATE SCHEMA ext;")
        );
        // A symlink that stays inside the project is the file it names.
        assert_eq!(
            read_baseline(&root, Path::new("inside.sql")).as_deref(),
            Ok("CREATE SCHEMA ext;")
        );
        // A parent step that comes back inside the root is still relative.
        assert_eq!(
            read_baseline(&root, Path::new("db/../db/baseline.sql")).as_deref(),
            Ok("CREATE SCHEMA ext;")
        );
        // Negative: an absolute path is refused even inside the root.
        for path in [root.join("db/baseline.sql"), dir.join("outside.sql")] {
            let refused = read_baseline(&root, &path).unwrap_err();
            assert!(refused.starts_with("is an absolute path"), "{refused}");
        }
        // Negative: a parent step and a symlink each lead outside the
        // project root, to a file that exists.
        for path in [
            PathBuf::from("../outside.sql"),
            PathBuf::from("db/link.sql"),
        ] {
            let refused = read_baseline(&root, &path).unwrap_err();
            assert!(
                refused.starts_with("lies outside the project root"),
                "{}: {refused}",
                path.display()
            );
        }
        // Negative: a missing file is unreadable, not empty.
        assert!(
            read_baseline(&root, Path::new("db/missing.sql"))
                .unwrap_err()
                .starts_with("could not be read")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Every resolver kind is bound by the planning identity since #1674, so
    /// a role refused `pg_control_system()` is refused for a Docker entry as
    /// for a server entry, and the finding names the grant. Function
    /// privileges belong to each database: the revoke stays in the fixture's
    /// own.
    #[test]
    #[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
    fn every_resolver_kind_demands_the_cluster_identity_grant() {
        use pbps_config::resolver::{ResolverProfile, SelectionSource, SelectionStatus};
        let selection = |profile| ResolverSelection {
            name: "r".into(),
            source: SelectionSource::Cli,
            profile,
            status: SelectionStatus::NotAcquired,
        };
        let docker = selection(ResolverProfile::Docker {
            image: "postgres:18".into(),
            pull: Default::default(),
            baseline: None,
            outside_writes: Default::default(),
        });
        let server = selection(ResolverProfile::Server {
            url_env: "PBPS_UNUSED".into(),
            standard: None,
            baseline: None,
            outside_writes: Default::default(),
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        rt.block_on(async {
            let database = crate::test_pg::TestDb::create("identity").await;
            let mut conn = database.connect().await;
            let role = format!("pbps_bin_identity_{}", std::process::id());
            conn.execute(&format!(
                "DROP ROLE IF EXISTS {role};
                 CREATE ROLE {role} NOLOGIN;
                 REVOKE EXECUTE ON FUNCTION pg_catalog.pg_control_system() FROM PUBLIC;"
            ))
            .await
            .expect("the fixture");
            let granted = planning_identity(&mut conn, &server).await;
            conn.execute(&format!("SET ROLE {role}"))
                .await
                .expect("set role");
            let docker_refused = planning_identity(&mut conn, &docker).await;
            let refused = planning_identity(&mut conn, &server).await;
            conn.execute("RESET ROLE").await.expect("reset role");
            drop(conn);
            database.drop().await;
            crate::test_pg::shared()
                .await
                .execute(&format!("DROP ROLE {role}"))
                .await
                .expect("drop role");
            assert!(
                matches!(granted, Ok(Some(ref identity)) if !identity.is_empty()),
                "a superuser reads it"
            );
            for refused in [docker_refused, refused] {
                match refused {
                    Err(Refused::Finding(finding)) => assert_eq!(finding.id, "resolver.identity"),
                    Err(Refused::Unanswerable(error)) => panic!("answered, not: {error}"),
                    Ok(identity) => panic!("a refused role read {identity:?}"),
                }
            }
        });
    }

    /// #1685: a role refused `pg_control_system()` is told the grant, as an
    /// answered finding; any other failure to read the identity is
    /// unanswerable, never a finding that blames the grant.
    #[test]
    fn an_unreadable_cluster_identity_names_the_grant_only_when_it_was_refused() {
        let denied = pbps_db::DbError::Driver {
            message: "permission denied for function pg_control_system".into(),
            code: Some("42501".into()),
        };
        match identity_unread(denied) {
            Refused::Finding(finding) => {
                assert_eq!(finding.id, "resolver.identity");
                assert!(
                    finding
                        .remedy
                        .as_deref()
                        .is_some_and(|remedy| remedy.contains("pg_control_system()")),
                    "{finding:?}"
                );
            }
            Refused::Unanswerable(error) => panic!("a refused read is answered: {error}"),
        }
        // Negative: a lost connection is not a missing grant.
        let lost = pbps_db::DbError::Driver {
            message: "connection reset".into(),
            code: Some("08006".into()),
        };
        assert!(matches!(identity_unread(lost), Refused::Unanswerable(_)));
    }

    /// #1575: a catalog read that failed part-way through the binding is
    /// `unanswerable`, exit 1; the binding's own verdict stays
    /// `resolver.unresolved`, exit 2 (SPEC 9.8).
    #[test]
    fn a_failed_catalog_read_is_unanswerable_while_a_binding_verdict_is_answered() {
        use pbps_cli::resolver::server::Error;
        assert!(!producer::answered(&Error::Read(
            "closing the capture".into()
        )));
        assert!(producer::answered(&Error::Binding(
            "no sound verdict".into()
        )));
        assert!(!producer::answered(&Error::Scope("not established".into())));
        assert!(producer::answered(&Error::Incompatible(vec![
            "differs".into()
        ])));
    }

    fn selection() -> ResolverSelection {
        ResolverSelection {
            name: "local".into(),
            source: pbps_config::resolver::SelectionSource::Cli,
            profile: pbps_config::resolver::ResolverProfile::Server {
                url_env: "PBPS_RESOLVER_UNSET_1515".into(),
                standard: None,
                baseline: None,
                outside_writes: Default::default(),
            },
            status: pbps_config::resolver::SelectionStatus::NotAcquired,
        }
    }

    fn ids(schema: &Schema, old: &IdsFile) -> IdsFile {
        pbps_diff::resolve(
            schema,
            old,
            &[],
            &pbps_diff::Context {
                operator: "1515".into(),
                today: "2026-10-05".into(),
            },
        )
        .unwrap()
        .ids
    }

    fn decide<'a>(
        selection: Option<&'a ResolverSelection>,
        base: &Schema,
        desired: &Schema,
    ) -> Case<'a> {
        let base_ids = ids(base, &IdsFile::default());
        let desired_ids = ids(desired, &base_ids);
        let base = pbps_diff::Side {
            schema: base,
            ids: &base_ids,
        };
        let desired = pbps_diff::Side {
            schema: desired,
            ids: &desired_ids,
        };
        let dialect = pbps_pg::Postgres::new();
        let ordinary = pbps_diff::diff(base, desired, &dialect, &Hints::default()).unwrap();
        case(selection, base, desired, &ordinary, &dialect)
    }

    /// A view over a table, and the same with a routine arriving beside it:
    /// ADR-0013's motivating shape.
    fn arrival() -> (Schema, Schema) {
        let mut base = Schema::default();
        let mut table = Table::default();
        table
            .columns
            .insert("id".into(), Column::new("int".parse().unwrap()));
        base.tables.insert("app.t".parse().unwrap(), table);
        base.modules.insert(
            "app.v".parse().unwrap(),
            Module {
                kind: ModuleKind::View,
                description: None,
                definition: "SELECT id FROM t".into(),
            },
        );
        let mut desired = base.clone();
        desired.modules.insert(
            "app.t()".parse().unwrap(),
            Module {
                kind: ModuleKind::Function,
                description: None,
                definition: "RETURNS int LANGUAGE sql RETURN 1".into(),
            },
        );
        (base, desired)
    }

    /// #1636: the producer is not gated to one platform. A `kind: server`
    /// resolver reaches its dispatch everywhere — CI's Windows job runs this
    /// too — and is refused there for its own reason, the unset scratch
    /// variable, never for the host it runs on.
    #[test]
    fn a_server_resolver_reaches_its_producer_on_every_platform() {
        let selection = selection();
        let (base, desired) = arrival();
        let Case::Required { assessment, .. } = decide(Some(&selection), &base, &desired) else {
            panic!("the arrival needs the resolver");
        };
        let base_ids = ids(&base, &IdsFile::default());
        let desired_ids = ids(&desired, &base_ids);
        let target = crate::db::target_from_connection(
            "postgres://pbps@127.0.0.1:1/pbps",
            pbps_db::Driver::Postgres,
        );
        let project = pbps_config::Project {
            root: std::env::temp_dir(),
            config: pbps_config::Config::parse(
                "dialect: postgres\n",
                std::path::Path::new("pbps.yml"),
            )
            .unwrap(),
        };
        let hints = Hints::default();
        let request = |target_identity| Request {
            selection: &selection,
            assessment: &assessment,
            target: &target,
            project: &project,
            base: pbps_diff::Side {
                schema: &base,
                ids: &base_ids,
            },
            desired: pbps_diff::Side {
                schema: &desired,
                ids: &desired_ids,
            },
            hints: &hints,
            target_identity,
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let refusal = |target_identity| match rt.block_on(producer::produce(
            &request(target_identity),
            pbps_db::Driver::Postgres,
            &[],
        )) {
            Err(Refused::Unanswerable(error)) => error.to_string(),
            Err(Refused::Finding(finding)) => panic!("not answered: {}", finding.id),
            Ok(_) => panic!("a plan without a scratch server"),
        };
        let reached = refusal(Some("7"));
        assert!(
            reached.contains("PBPS_RESOLVER_UNSET_1515, which is not set"),
            "{reached}"
        );
        assert!(!reached.contains("Linux"), "{reached}");
        // Negative: without the planning read's cluster identity the
        // producer stops first, before it reads any scratch variable.
        let unbound = refusal(None);
        assert!(unbound.contains("no cluster identity"), "{unbound}");
    }

    /// SQL Server resolution is a gate that refuses by name before the
    /// resolver runs (#1517). Negative: a PostgreSQL target passes it, and
    /// stops at the next step, the missing fingerprint key.
    #[test]
    fn sql_server_resolution_is_refused_by_name() {
        let selection = selection();
        let (base, desired) = arrival();
        let Case::Required { assessment, .. } = decide(Some(&selection), &base, &desired) else {
            panic!("the arrival needs the resolver");
        };
        let base_ids = ids(&base, &IdsFile::default());
        let desired_ids = ids(&desired, &base_ids);
        let project = pbps_config::Project {
            root: std::env::temp_dir(),
            config: pbps_config::Config::parse(
                "dialect: postgres\n",
                std::path::Path::new("pbps.yml"),
            )
            .unwrap(),
        };
        let hints = Hints::default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let refusal = |connection: &str, driver| {
            let target = crate::db::target_from_connection(connection, driver);
            let request = Request {
                selection: &selection,
                assessment: &assessment,
                target: &target,
                project: &project,
                base: pbps_diff::Side {
                    schema: &base,
                    ids: &base_ids,
                },
                desired: pbps_diff::Side {
                    schema: &desired,
                    ids: &desired_ids,
                },
                hints: &hints,
                target_identity: None,
            };
            match rt.block_on(resolve(&request)) {
                Err(Refused::Finding(finding)) => finding,
                Err(Refused::Unanswerable(error)) => panic!("unanswerable: {error}"),
                Ok(_) => panic!("answered"),
            }
        };
        let mssql = refusal(
            "sqlserver://127.0.0.1:1;database=pbps",
            pbps_db::Driver::Mssql,
        );
        assert_eq!(mssql.id, "resolver.unresolved");
        assert!(
            mssql.message.contains("PostgreSQL only")
                && mssql
                    .message
                    .contains("SQL Server resolution is not built yet"),
            "{}",
            mssql.message
        );
        assert!(
            mssql
                .remedy
                .as_deref()
                .is_some_and(|r| r.contains("Select no resolver")),
            "{:?}",
            mssql.remedy
        );
        let postgres = refusal(
            "postgres://pbps@127.0.0.1:1/pbps",
            pbps_db::Driver::Postgres,
        );
        assert_eq!(postgres.id, "resolver.key", "{}", postgres.message);
    }

    #[test]
    fn without_a_selection_the_plan_is_never_assessed() {
        let (base, desired) = arrival();
        assert!(matches!(decide(None, &base, &desired), Case::Fallback));
    }

    #[test]
    fn a_selected_resolver_a_plan_does_not_need_is_not_required() {
        let (base, _) = arrival();
        let selected = selection();
        let Case::NotNeeded { assessment, .. } = decide(Some(&selected), &base, &base) else {
            panic!("an empty plan raises no question that needs an engine");
        };
        assert_eq!(
            ResolverAssessment::not_needed(&assessment),
            ResolverAssessment {
                need: ResolutionNeed::NotNeeded,
                unaffected: 1,
                rebuilt: 0,
            }
        );
    }

    /// An index named per table moves no lookup on SQL Server, so a plan that
    /// drops one beside a kept view is not refused for want of a binding
    /// adapter; on PostgreSQL the same name is a relation's (DECISIONS 453).
    #[test]
    fn a_dropped_index_is_a_binding_question_only_where_indexes_are_relations() {
        let (base, _) = arrival();
        let selected = selection();
        let base_ids = ids(&base, &IdsFile::default());
        let side = pbps_diff::Side {
            schema: &base,
            ids: &base_ids,
        };
        let ordinary = pbps_model::ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(
                pbps_model::Change::DropIndex {
                    table: "app.t".parse().unwrap(),
                    name: "ix_gone".into(),
                },
            )],
        };
        assert!(matches!(
            case(Some(&selected), side, side, &ordinary, &pbps_mssql::Mssql),
            Case::NotNeeded { .. }
        ));
        assert!(matches!(
            case(
                Some(&selected),
                side,
                side,
                &ordinary,
                &pbps_pg::Postgres::new()
            ),
            Case::Required { .. }
        ));
    }

    #[test]
    fn an_arrival_beside_a_kept_view_requires_the_selected_resolver() {
        let (base, desired) = arrival();
        let selected = selection();
        let Case::Required { assessment, .. } = decide(Some(&selected), &base, &desired) else {
            panic!("an arriving routine is a binding question");
        };
        assert_eq!(named(&assessment), "app.v");
    }
}
