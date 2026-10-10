//! The three ways a connected plan meets resolver selection (DEC-1515.1).
//!
//! 1. No resolver selected: ADR-0013's candidate rebuild and the
//!    unmanaged-dependent gate decide, as they did before any resolver
//!    existed. Nothing here runs, not even the assessment.
//! 2. A resolver selected, and the assessment needs none: the ordinary plan,
//!    and nothing resolver-only is opened — no key, no Docker socket, no
//!    scratch connection.
//! 3. A resolver selected, and some question needs it: the resolver answers,
//!    or deployable output is refused. Its answer is not yet published: that
//!    waits for the fresh pre-publication recheck (#1516).

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

/// Case 2's report: a selected resolver this plan did not need.
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
}

impl ResolverAssessment {
    pub fn not_needed(assessment: &Assessment) -> Self {
        Self {
            need: ResolutionNeed::NotNeeded,
            unaffected: assessment.count(Answer::Unaffected),
            rebuilt: assessment.count(Answer::Rebuild),
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
    /// Only a producer reads the target's side, and a host with none refuses
    /// before it would.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub base: pbps_diff::Side<'a>,
    pub desired: pbps_diff::Side<'a>,
    pub hints: &'a pbps_model::Hints,
    /// The `system_identifier` of the cluster the planning read reached, for
    /// a PostgreSQL target. The resolver's own target connection must reach
    /// the same one: a target name can reach several (#1685). Like `base`,
    /// only a producer reads it.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub target_identity: Option<&'a str>,
}

/// The `system_identifier` of the cluster `conn` reached, for a PostgreSQL
/// target: the planning read's, which the resolver's own target connection
/// must match, since a target name can reach several clusters (#1685).
/// Only PostgreSQL is resolved, and resolution refuses the other engines by
/// name, so they record none. Only a supplied server is bound by it: the
/// Docker profile does not read it, and must not demand the grant (#1718
/// review).
pub async fn planning_identity(
    conn: &mut pbps_db::Conn,
    selection: &ResolverSelection,
) -> Result<Option<String>, Refused> {
    let binds = matches!(
        selection.profile,
        pbps_config::resolver::ResolverProfile::Server { .. }
    );
    if !binds || conn.driver() != pbps_db::Driver::Postgres {
        return Ok(None);
    }
    pbps_pg::resolver::vouched::cluster_identity(conn)
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
        .remedy("Repair the declaration so its bindings are unambiguous, or select a resolver profile that can reproduce what it binds."),
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
/// The evidence is not returned: nothing publishes it before the recheck
/// that #1516 adds, and a value no caller may use is one a caller will.
pub async fn resolve(request: &Request<'_>) -> Result<pbps_model::ChangeSet, Refused> {
    let driver = request.target.driver();
    if driver != pbps_db::Driver::Postgres {
        return Err(unresolved(
            request,
            "binding resolution is implemented for PostgreSQL only",
        ));
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
        Refused::Unanswerable(anyhow::anyhow!(
            "the declarations' bootstrap cannot be formed: {} error(s)",
            errors.len()
        ))
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
/// anywhere. The file read is the one judged. Linux only, with the producer
/// that reads it.
#[cfg(target_os = "linux")]
fn read_baseline(root: &std::path::Path, path: &std::path::Path) -> Result<String, String> {
    let unreadable = |error: std::io::Error| format!("could not be read: {error}");
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

/// The producers of the profiles pbps implements. Today every one is a
/// measured profile (RESOLVER-RUNTIME): its run binds the target by observing
/// the engine service that holds the connection (DEC-1514.1). That premise
/// belongs to these profiles, not to resolution.
#[cfg(target_os = "linux")]
mod producer {
    use super::{Refused, Request, unresolved};
    use pbps_cli::resolver::server::{Error, ProduceError};

    pub(super) async fn produce(
        request: &Request<'_>,
        driver: pbps_db::Driver,
        bootstrap: &[pbps_model::Change],
    ) -> Result<pbps_model::ChangeSet, Refused> {
        let binding = pbps_cli::resolver::server::BindingRequest {
            bootstrap,
            desired: request.desired.schema,
            base: request.base.schema,
        };
        // A supplied server is the operator-vouched resolver (DEC-1528.1);
        // Docker stays the measured profile until #1674 gives it a vouched
        // runtime.
        if let pbps_config::resolver::ResolverProfile::Server {
            url_env,
            standard,
            baseline,
        } = &request.selection.profile
        {
            let scratch = scratch_connection(request, url_env)?;
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
            let Some(identity) = request.target_identity else {
                return Err(Refused::Unanswerable(anyhow::anyhow!(
                    "the planning read recorded no cluster identity for the resolver to bind"
                )));
            };
            return pbps_cli::resolver::server::vouched::produce(
                driver,
                &scratch,
                request.target.connection(),
                identity,
                &binding,
                request.base,
                request.desired,
                request.hints,
                &[],
                request.project,
                request.target.environment(),
                &pbps_cli::resolver::server::vouched::declared_standard(standard.as_ref()),
                baseline.as_deref(),
            )
            .await
            .map(|resolved| resolved.changes)
            .map_err(|failure| refused(request, failure));
        }
        pbps_cli::resolver::server::produce(
            driver,
            &request.selection.profile,
            request.target.connection(),
            &binding,
            request.base,
            request.desired,
            request.hints,
            // The CLI's PostgreSQL dialect configures no write-path extras
            // (`dialect_for`), so the plan it diffed has none either.
            &[],
            request.project,
            request.target.environment(),
        )
        .await
        .map(|resolved| resolved.changes)
        .map_err(|failure| refused(request, failure))
    }

    fn refused(request: &Request<'_>, failure: ProduceError) -> Refused {
        match failure {
            ProduceError::Run(error) if answered(&error) => unresolved(request, error),
            ProduceError::Run(error) => Refused::Unanswerable(error.into()),
            ProduceError::Target(_)
            | ProduceError::Recipe(_)
            | ProduceError::Acquire(_)
            | ProduceError::Cleanup(_) => Refused::Unanswerable(failure.into()),
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
            // measured and differs, or this run's profile cannot serve the
            // engine.
            Error::Binding(_) | Error::Incompatible(_) | Error::UnsupportedProfile { .. } => true,
            // The supplied server was measured and does not meet the profile
            // it claims: also an answer.
            Error::Container(_)
            | Error::Configuration(_)
            | Error::EngineExecutable
            | Error::Containment(_)
            | Error::Mount(_)
            | Error::TargetInstance => true,
            // Not answered: the run could not establish what it needed. A
            // catalog read that failed is one of these, however far into the
            // binding it came (#1575).
            Error::Endpoint
            | Error::Daemon(_)
            | Error::Channel(_)
            | Error::Exclusivity(_)
            | Error::Unqualified(_)
            | Error::Identity(_)
            | Error::Deadline
            | Error::Cancelled
            | Error::Scratch
            | Error::Cleanup
            | Error::Consumed
            | Error::Scope(_)
            | Error::Read(_)
            // A scratch that cannot be used as configured answered nothing,
            // nor did one whose baseline is not the target's.
            | Error::Vouched(_)
            | Error::Baseline(_) => false,
        }
    }
}

/// The measured profiles run on a native Linux host (RESOLVER-RUNTIME), and
/// no other profile exists yet; the question still needs an answer, so the
/// plan is refused rather than guessed.
#[cfg(not(target_os = "linux"))]
mod producer {
    use super::{Refused, Request, unresolved};

    pub(super) async fn produce(
        request: &Request<'_>,
        _driver: pbps_db::Driver,
        _bootstrap: &[pbps_model::Change],
    ) -> Result<pbps_model::ChangeSet, Refused> {
        Err(unresolved(
            request,
            "the selected profile's runtime needs a native Linux host (RESOLVER-RUNTIME)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Column, Hints, IdsFile, Module, ModuleKind, Schema, Table};

    // Linux only: the reader lives with the Linux producer, and the fixture
    // makes Unix symlinks.
    #[test]
    #[cfg(target_os = "linux")]
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
        // Negative: a parent step, an absolute path and a symlink each lead
        // outside the project root, to a file that exists.
        for path in [
            PathBuf::from("../outside.sql"),
            dir.join("outside.sql"),
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

    /// #1718 review: only a supplied server is bound by the planning
    /// identity, so a Docker profile plans for a role refused
    /// `pg_control_system()`, while a server profile refuses it and names the
    /// grant. Function privileges belong to each database: the revoke stays in
    /// the fixture's own.
    #[test]
    #[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
    fn only_a_supplied_server_demands_the_cluster_identity_grant() {
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
        });
        let server = selection(ResolverProfile::Server {
            url_env: "PBPS_UNUSED".into(),
            standard: None,
            baseline: None,
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
            let unbound = planning_identity(&mut conn, &docker).await;
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
            assert!(matches!(unbound, Ok(None)), "Docker reads no identity");
            match refused {
                Err(Refused::Finding(finding)) => assert_eq!(finding.id, "resolver.identity"),
                Err(Refused::Unanswerable(error)) => panic!("answered, not: {error}"),
                Ok(identity) => panic!("a refused role read {identity:?}"),
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
    #[cfg(target_os = "linux")]
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
