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

pub mod attach;
pub mod domain;
pub mod retained;

use pbps_cli::resolver::server::ProduceError;
use pbps_config::resolver::ResolverSelection;
use pbps_diff::resolver::{Answer, Assessment, UnaffectedReason};

// The dialect brand survives the ordinary continuation and the publication recheck.
pub(crate) trait Continuation:
    crate::emission::Engine + pbps_cli::resolver::server::PlanningDialect
{
    fn with_scope(
        &self,
        scope: &pbps_diff::order::writebacks::ScopeInput,
        resolved: &pbps_cli::resolver::server::Unsealed<Self>,
    ) -> pbps_diff::order::PlanningPlan<pbps_diff::order::Continuing<Self>> {
        scope.append_to(resolved.changes().clone(), self)
    }
}
impl Continuation for pbps_pg::Postgres {}
impl Continuation for pbps_mssql::Mssql {}
impl Continuation for pbps_sqlite::dialect::Sqlite {}

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
#[cfg(test)]
pub fn case<'a>(
    selection: Option<&'a ResolverSelection>,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    ordinary: &pbps_model::ChangeSet,
    dialect: &dyn pbps_dialect::Dialect,
) -> Case<'a> {
    case_with_facts(selection, base, desired, ordinary, dialect, None)
}

/// Decide the case for the ordinary plan of `base -> desired`, optionally
/// narrowed by connected catalog [`pbps_model::resolver::DependencyFacts`]
/// (DEC-2987.1). With `facts = None`, every answer is DEC-1515.1's.
pub fn case_with_facts<'a>(
    selection: Option<&'a ResolverSelection>,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    ordinary: &pbps_model::ChangeSet,
    dialect: &dyn pbps_dialect::Dialect,
    facts: Option<&pbps_model::resolver::DependencyFacts>,
) -> Case<'a> {
    let Some(selection) = selection else {
        return Case::Fallback;
    };
    let assessment =
        pbps_diff::resolver::assess_with_facts(base, desired, ordinary, dialect, facts);
    if assessment.requires_resolution() {
        Case::Required {
            selection,
            assessment,
        }
    } else {
        Case::NotNeeded { assessment }
    }
}

/// Stable finding identifier for the advisory note emitted when connected
/// dependency facts could not be read and assessment fell back to plan-level
/// answers.
pub const DEPENDENCY_FACTS_UNREAD_ID: &str = "resolver.dependency-facts-unread";

/// Consume the result of a connected dependency-facts read. A failed read is
/// never a plan error: it yields `None` (plan-level assessment) and one note.
pub fn unwrap_dependency_facts(
    result: Result<Option<pbps_model::resolver::DependencyFacts>, pbps_db::DbError>,
) -> (
    Option<pbps_model::resolver::DependencyFacts>,
    Option<crate::output::Finding>,
) {
    match result {
        Ok(facts) => (facts, None),
        Err(error) => {
            let message = format!(
                "Could not read catalog dependency facts for lightweight binding assessment; \
                 falling back to plan-level assessment: {error}"
            );
            (
                None,
                Some(crate::output::Finding::note(
                    DEPENDENCY_FACTS_UNREAD_ID,
                    message,
                )),
            )
        }
    }
}

/// The note of a connected plan whose dependency facts could not be read
/// when its candidate rebuilds were narrowed (DEC-2987.5). Never a plan
/// error: the plan keeps every rebuild its text asks for, which is the plan
/// it had before the facts existed.
pub fn rebuilds_kept_unread(error: &pbps_db::DbError) -> crate::output::Finding {
    crate::output::Finding::note(
        DEPENDENCY_FACTS_UNREAD_ID,
        format!(
            "Could not read catalog dependency facts; every unchanged module whose text \
             mentions an arriving name is rebuilt, as without them: {error}"
        ),
    )
}

/// What the selected resolver was to this plan: not needed (case 2), or
/// needed and answering, its plan published (case 3, #1516).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct ResolverAssessment {
    pub need: ResolutionNeed,
    /// Kept surfaces nothing in the plan can rebind, whatever the reason.
    pub unaffected: usize,
    /// Of `unaffected`, those the plan could have reached by name, and that
    /// the dependencies the engine recorded prove it does not (DEC-2987.1).
    /// Apart from the rest because it is a proof from the catalog, not from
    /// the plan alone.
    pub proven_by_dependency: usize,
    /// Kept routines whose body the engine did not bind when it created
    /// them, whatever the answer to their header: unaffected, or answered by
    /// the resolver. Neither checks the body (DEC-2987.1), which a reader must
    /// be able to tell from a proof. Known from the engine's dependency
    /// facts, so zero where none were read.
    pub body_not_checked: usize,
    /// Kept surfaces the plan recreates from their declarations anyway.
    pub rebuilt: usize,
    /// The identity actually used by the successful resolver run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization: Option<pbps_model::resolver::AuthorizationReproduction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_notice: Option<&'static str>,
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

/// Shared by plan and offline explain, from the sealed status rather than
/// the current profile: selecting a supplied server need not run it (#2570).
pub fn authorization_notice(
    status: pbps_model::resolver::AuthorizationReproduction,
) -> Option<&'static str> {
    match status {
        pbps_model::resolver::AuthorizationReproduction::Reproduced => None,
        pbps_model::resolver::AuthorizationReproduction::LoginStandin => Some(
            "Authorization was not reproduced: supplied PostgreSQL scratch compiled as its login (login_standin), which can have more privileges than the deployer. Prefer Docker or run-owned scratch on another cluster. Verified deployer-role support is planned in #2570. Account layout guidance: https://github.com/pongbiphang/pbps/issues/1163.",
        ),
    }
}

impl ResolverAssessment {
    pub fn record_authorization(
        &mut self,
        status: pbps_model::resolver::AuthorizationReproduction,
    ) {
        self.authorization = Some(status);
        self.authorization_notice = authorization_notice(status);
    }

    pub fn not_needed(assessment: &Assessment) -> Self {
        Self {
            need: ResolutionNeed::NotNeeded,
            unaffected: assessment.count(Answer::Unaffected),
            proven_by_dependency: assessment.count_unaffected(UnaffectedReason::ByDependency),
            body_not_checked: assessment.unbound_bodies(),
            rebuilt: assessment.count(Answer::Rebuild),
            authorization: None,
            authorization_notice: None,
        }
    }

    pub fn resolved(assessment: &Assessment) -> Self {
        Self {
            need: ResolutionNeed::Resolved,
            ..Self::not_needed(assessment)
        }
    }

    /// Why no resolver ran, for the plan's text report. A plan that moves a
    /// name and was answered from the catalog must not be described as one
    /// that moves none: the reader would take a proof for an absence.
    pub fn not_needed_reason(&self) -> String {
        let rebuilt = self.rebuilt;
        if self.proven_by_dependency == 0 {
            format!(
                "this plan moves no name a kept binding could reach ({} unaffected, {rebuilt} \
                 rebuilt from their declarations)",
                self.unaffected
            )
        } else {
            format!(
                "the dependencies the engine recorded show that no kept binding reads what \
                 this plan moves ({} unaffected, {} of them proven by dependency; {rebuilt} \
                 rebuilt from their declarations)",
                self.unaffected, self.proven_by_dependency
            )
        }
    }

    /// The line that says what the answer above does not cover, or `None`
    /// when it covers everything. Printed whether or not a resolver ran: a
    /// resolver compares what a surface bound when it was created, and these
    /// bodies bound nothing then (DEC-2987.1).
    pub fn body_notice(&self) -> Option<String> {
        let (count, noun) = match self.body_not_checked {
            0 => return None,
            1 => (1, "routine has a body"),
            n => (n, "routines have bodies"),
        };
        Some(format!(
            "{count} kept {noun} the engine does not bind at creation: the header was checked, \
             the body was not. What such a body finds when it runs is for the routine's own \
             tests."
        ))
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
        Surface::Module(id) => id.shown().to_string(),
        Surface::Default(column) => format!("the default of {column}", column = column.shown()),
        Surface::Check { table, name } => format!(
            "check {name} on {table}",
            table = table.shown(),
            name = name.shown(),
        ),
        Surface::Index { table, name } => format!(
            "index {name} on {table}",
            table = table.shown(),
            name = name.shown(),
        ),
        Surface::Namespace(schema) => format!("schema {schema}"),
        Surface::Table(table) => table.shown().to_string(),
        Surface::Column(column) => column.shown().to_string(),
        Surface::Type(name) => format!("type {name}", name = name.shown()),
    }
}

/// Everything case 3 needs from the plan being made.
pub struct Request<'a> {
    pub sources: Option<&'a pbps_load::DeclarationSourceIndex>,
    pub dependencies: &'a pbps_model::dependency::Dependencies,
    pub selection: &'a ResolverSelection,
    pub assessment: &'a Assessment,
    pub target: &'a crate::db::Target,
    pub project: &'a pbps_config::Project,
    pub base: pbps_diff::Side<'a>,
    pub desired: pbps_diff::Side<'a>,
    pub hints: &'a pbps_model::Hints,
    /// The planning read's engine lineage identity: PostgreSQL's cluster
    /// `system_identifier` or SQL Server master's `family_guid`. The resolver
    /// must reach the same one: a target name can reach several (#1685).
    pub target_identity: Option<&'a str>,
}

/// The engine lineage identity the planning read reached, which the
/// resolver's own target connection must match, since a target name can
/// reach several clusters or instances (#1685).
/// Both kinds of resolver are bound by the engine's lineage identity: a
/// Docker scratch, like a supplied server, answers for the cluster the plan
/// was read from (#1674), so neither plans for a role refused the read.
pub async fn planning_identity<D: pbps_cli::resolver::server::PlanningDialect>(
    dialect: &D,
    conn: &mut pbps_db::Conn,
    _selection: &ResolverSelection,
) -> Result<Option<String>, Refused> {
    let driver = conn.driver();
    pbps_cli::resolver::server::target_identity(dialect, conn)
        .await
        .map(Some)
        .map_err(|error| crate::engine::target_identity_unread(driver, error))
}

/// A refused read of the cluster's identity is the deployment role's to
/// fix, and names the grant; any other failure is unanswerable.
pub(crate) fn identity_unread(error: pbps_db::DbError) -> Refused {
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
             retains to the resolver's externals file, or select no resolver and keep the existing \
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

/// A plan that creates, drops or updates a package and also needs the resolver
/// (DEC-2447.6, .8, .10).
/// The resolver reconstructs the desired namespace on a scratch that does not
/// hold a package the target lacks, so it cannot seal evidence over the create;
/// measured, it failed late with an internal error. The two plans taken in
/// order work: the package alone needs no resolver answer, and once it is
/// installed and managed the scratch holds it through its externals file
/// (DEC-2447.2).
fn package_created(request: &Request<'_>) -> Option<crate::output::Finding> {
    let created: Vec<String> = request
        .desired
        .schema
        .extensions
        .keys()
        .filter(|name| {
            request.desired.ids.extension_uid(name).is_some()
                && !request.base.schema.extensions.contains_key(*name)
        })
        .map(|name| format!("`{}`", name.shown()))
        .collect();
    if created.is_empty() {
        let (base, want) = (&request.base, &request.desired.schema.extensions);
        let dropped: Vec<_> = (base.schema.extensions.keys())
            .filter(|n| base.ids.extension_uid(n).is_some() && !want.contains_key(*n))
            .map(|n| format!("`{}`", n.shown()))
            .collect();
        if dropped.is_empty() {
            // An update or a move: the scratch holds the version and schema the
            // target does, not the ones the plan leaves (DEC-2447.10,
            // DEC-2447.11).
            let changed = |f: &pbps_model::Extension, to: &pbps_model::Extension| {
                (f.version != to.version, f.schema != to.schema)
            };
            let updated: Vec<_> = (want.iter())
                .filter(|(n, to)| {
                    base.ids.extension_uid(n).is_some()
                        && base.schema.extensions.get(*n).is_some_and(|f| {
                            let (version, schema) = changed(f, to);
                            version || schema
                        })
                })
                .map(|(n, _)| format!("`{}`", n.shown()))
                .collect();
            let verb = if (want.iter()).any(|(n, to)| {
                base.schema
                    .extensions
                    .get(n)
                    .is_some_and(|f| changed(f, to).0)
            }) {
                "updates"
            } else {
                "moves"
            };
            if updated.is_empty() {
                return None;
            }
            return Some(gated(
                request.selection,
                request.assessment,
                &format!(
                    "this plan also {verb} extension {}, and the resolver cannot describe a \
                     package whose scratch holds another version or schema than the plan leaves",
                    updated.join(", ")
                ),
                "Plan the update or move alone first and apply it. Then raise the version, or \
                 the schema, in the scratch's externals file (DEC-2447.2) and plan the rest.",
            ));
        }
        return Some(gated(
            request.selection,
            request.assessment,
            &format!(
                "this plan also drops extension {}, and the resolver cannot describe a package \
                 that leaves while its scratch holds it",
                dropped.join(", ")
            ),
            "Plan the drop alone first and apply it. Then plan the rest.",
        ));
    }
    Some(gated(
        request.selection,
        request.assessment,
        &format!(
            "this plan also creates extension {}, and the resolver cannot describe a package the \
             target does not hold yet",
            created.join(", ")
        ),
        "Plan the package alone first and apply it. Then provision it on the scratch through \
         the entry's externals file (DEC-2447.2) and plan the rest.",
    ))
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
pub(crate) async fn resolve<D: pbps_cli::resolver::server::PlanningDialect>(
    request: &Request<'_>,
    entry: &mut Entry<'_>,
) -> Result<pbps_cli::resolver::server::Unsealed<D>, Refused> {
    // The only way into a resolver run, so this is the one place a package
    // create, drop or update is turned away: before the key is read or a scratch is touched.
    if let Some(finding) = package_created(request) {
        return Err(Refused::Finding(finding));
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
    let (prerequisites, prerequisite_ids) = pbps_diff::bootstrap_base(request.desired);
    let bootstrap = pbps_diff::bootstrap(
        pbps_diff::Side {
            schema: &prerequisites,
            ids: &prerequisite_ids,
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
    })?;
    producer::produce::<D>(request, &bootstrap, entry).await
}

/// A resolver's externals file, read only from under the project root.
///
/// The externals file is reviewed history like the declarations (SPEC §9.3.2), so
/// it is never a file elsewhere on the planning machine: an absolute path, a
/// `..` or a symlink that leads outside the root is refused. Judged on the
/// resolved path, not lexically, because a symlink inside the root can point
/// anywhere. The file read is the one judged.
pub(crate) fn read_externals(
    root: &std::path::Path,
    path: &std::path::Path,
) -> Result<String, String> {
    let unreadable = |error: std::io::Error| format!("could not be read: {error}");
    // Relative, as the configuration says: an absolute path would work from
    // one checkout only, even one inside the root (`join` replaces the root
    // with it).
    if path.has_root() {
        return Err(
            "is an absolute path; name the externals file relative to the project root".into(),
        );
    }
    let root = root.canonicalize().map_err(unreadable)?;
    let file = root.join(path).canonicalize().map_err(unreadable)?;
    if !file.starts_with(&root) {
        return Err(format!(
            "lies outside the project root {}; an externals file is a file in the project",
            root.display()
        ));
    }
    std::fs::read_to_string(&file).map_err(unreadable)
}

/// The scratch entry a connected plan selected, acquired at most once per run
/// (#2379). A `kind: docker` entry is one container the resolver and the
/// rehearsal share; a `kind: server` entry is the server they both name.
/// Each use creates and releases its own scratch database there, so one
/// finishes before the next begins.
pub(crate) struct Entry<'a> {
    selection: &'a ResolverSelection,
    project: &'a pbps_config::Project,
    target: &'a crate::db::Target,
    held: Option<pbps_cli::resolver::server::scratch::container::Held>,
}

/// What a use of the entry runs against.
pub(crate) struct Open<D: pbps_cli::resolver::server::PlanningDialect> {
    pub connection: String,
    pub standard: pbps_cli::resolver::server::scratch::Standard<D>,
    pub provenance: pbps_cli::resolver::server::scratch::Provenance,
    pub externals: Option<String>,
}

impl<'a> Entry<'a> {
    pub fn new(
        selection: &'a ResolverSelection,
        project: &'a pbps_config::Project,
        target: &'a crate::db::Target,
    ) -> Self {
        Self {
            selection,
            project,
            target,
            held: None,
        }
    }

    pub fn selection(&self) -> &ResolverSelection {
        self.selection
    }

    /// The entry's scratch, acquired on the first use. A container is bound
    /// to the target's cluster identity before it is started, so a target
    /// connection that reaches another cluster than the planning read
    /// refuses with nothing started.
    pub async fn open<D: pbps_cli::resolver::server::PlanningDialect>(
        &mut self,
        identity: &str,
    ) -> Result<Open<D>, Refused> {
        use pbps_cli::resolver::server::scratch::{self, Provenance};
        use pbps_config::resolver::ResolverProfile;
        let unanswerable = |error: ProduceError| Refused::Unanswerable(error.into());
        let (externals, outside_writes) = match &self.selection.profile {
            ResolverProfile::Server {
                externals,
                outside_writes,
                ..
            }
            | ResolverProfile::Docker {
                externals,
                outside_writes,
                ..
            } => (externals, *outside_writes),
        };
        let externals = externals
            .as_ref()
            .map(|path| {
                read_externals(self.project.root(), path).map_err(|why| {
                    Refused::Unanswerable(anyhow::anyhow!(
                        "the resolver `{}` names the externals file {}, which {why}",
                        self.selection.name,
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
        let same_instance = producer::same_instance(D::DRIVER, &self.selection.profile);
        let entry = self.selection.name.to_owned();
        match &self.selection.profile {
            ResolverProfile::Server {
                url_env, standard, ..
            } => Ok(Open {
                connection: producer::scratch_connection(
                    self.project,
                    self.target,
                    &self.selection.name,
                    url_env,
                )?,
                standard: scratch::declared_standard::<D>(standard.as_ref())
                    .map_err(|error| Refused::Unanswerable(error.into()))?,
                provenance: Provenance {
                    entry,
                    scratch: pbps_model::resolver::ScratchSource::Server {},
                    outside_writes,
                    same_instance,
                },
                externals,
            }),
            ResolverProfile::Docker { image, pull, .. } => {
                if self.held.is_none() {
                    scratch::container::bind_target::<D>(self.target.connection(), identity)
                        .await
                        .map_err(unanswerable)?;
                    self.held = Some(
                        scratch::container::Held::start(D::DRIVER, image, *pull)
                            .await
                            .map_err(unanswerable)?,
                    );
                }
                let held = self.held.as_ref().expect("the container was just held");
                Ok(Open {
                    connection: held.connection().to_owned(),
                    standard: scratch::declared_standard::<D>(None)
                        .map_err(|error| Refused::Unanswerable(error.into()))?,
                    provenance: Provenance {
                        entry,
                        scratch: pbps_model::resolver::ScratchSource::Docker {
                            image: image.as_str().into(),
                        },
                        outside_writes,
                        same_instance,
                    },
                    externals,
                })
            }
        }
    }

    /// Removes the container, if one was started, and confirms it gone. Any
    /// other way out of the plan leaves it to the container's drop guard.
    pub async fn finish(&mut self) -> Result<(), Refused> {
        match self.held.take() {
            Some(held) => held
                .remove()
                .await
                .map_err(|error| Refused::Unanswerable(error.into())),
            None => Ok(()),
        }
    }
}

/// The resolver's producer: a scratch run on a supplied server or a Docker
/// container, bound to the target by the cluster identity the planning read
/// recorded (DEC-1685.1). It reaches its scratch with SQL alone, so it runs
/// on every platform pbps supports (#1636).
mod producer {
    use super::{Entry, Refused, Request, unresolved};
    use pbps_cli::resolver::server::{Error, ProduceError};

    pub(super) async fn produce<D: pbps_cli::resolver::server::PlanningDialect>(
        request: &Request<'_>,
        bootstrap: &pbps_diff::order::DiffPlan,
        entry: &mut Entry<'_>,
    ) -> Result<pbps_cli::resolver::server::Unsealed<D>, Refused> {
        let binding = pbps_cli::resolver::server::BindingRequest {
            dependencies: request.dependencies,
            bootstrap,
            desired: request.desired.schema,
            base: request.base.schema,
        };
        // Both kinds are the one resolver (DEC-1528.1): a
        // supplied server, or a container started for the run (#1674).
        use pbps_cli::resolver::server::scratch;
        let Some(identity) = request.target_identity else {
            return Err(Refused::Unanswerable(anyhow::anyhow!(
                "the planning read recorded no cluster identity for the resolver to bind"
            )));
        };
        let open = entry.open::<D>(identity).await?;
        scratch::produce_unsealed::<D>(
            &open.connection,
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
            &open.standard,
            open.externals.as_deref(),
            &open.provenance,
        )
        .await
        .map_err(|failure| refused(request, failure))
    }

    /// Omission never grants consent; inapplicability stays distinct from refusal.
    pub(super) fn same_instance(
        driver: pbps_db::Driver,
        profile: &pbps_config::resolver::ResolverProfile,
    ) -> pbps_model::resolver::SameInstance {
        crate::engine::same_instance_consent(driver, profile)
    }

    fn refused(request: &Request<'_>, failure: ProduceError) -> Refused {
        match failure {
            ProduceError::Run(Error::Creation(mut failure)) => {
                std::sync::Arc::make_mut(&mut failure).sources =
                    request.sources.cloned().unwrap_or_default();
                if failure.externals {
                    Refused::Unanswerable(Error::Creation(failure).into())
                } else {
                    unresolved(request, failure)
                }
            }
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
    pub(super) fn scratch_connection(
        project: &pbps_config::Project,
        target: &crate::db::Target,
        name: &str,
        url_env: &str,
    ) -> Result<String, Refused> {
        let target_env = target.environment().and_then(|environment| {
            project
                .config
                .environments
                .get(environment)
                .map(|environment| environment.url_env.as_str())
        });
        if target_env == Some(url_env) {
            return Err(Refused::Unanswerable(anyhow::anyhow!(
                "the resolver `{name}` names the target's own connection variable {url_env}; \
                 a scratch server needs its own"
            )));
        }
        std::env::var(url_env).map_err(|_| {
            Refused::Unanswerable(anyhow::anyhow!(
                "the resolver `{name}` reads its scratch server from {url_env}, which is not set"
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
            Error::Creation(failure) => !failure.externals,
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
            // nor did one whose externals file is not the target's.
            | Error::Unusable(_)
            | Error::Externals(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Column, Hints, IdsFile, Module, ModuleKind, Schema, Table};

    #[test]
    fn runtime_policy_preserves_applicability_without_implicit_consent() {
        use pbps_db::Driver;
        use pbps_model::resolver::SameInstance;
        for (dialect, driver, entry, expected) in [
            (
                "mssql",
                Driver::Mssql,
                "{kind: server, url_env: S}",
                SameInstance::Refused,
            ),
            (
                "mssql",
                Driver::Mssql,
                "{kind: server, url_env: S, outside_writes: allow}",
                SameInstance::Refused,
            ),
            (
                "mssql",
                Driver::Mssql,
                "{kind: server, url_env: S, same_instance: refuse}",
                SameInstance::Refused,
            ),
            (
                "mssql",
                Driver::Mssql,
                "{kind: server, url_env: S, same_instance: allow}",
                SameInstance::Allowed,
            ),
            (
                "mssql",
                Driver::Mssql,
                "{kind: docker, image: mssql:2025}",
                SameInstance::Refused,
            ),
            (
                "postgres",
                Driver::Postgres,
                "{kind: server, url_env: S}",
                SameInstance::NotApplicable,
            ),
            (
                "postgres",
                Driver::Postgres,
                "{kind: docker, image: postgres:18}",
                SameInstance::NotApplicable,
            ),
        ] {
            let config = pbps_config::Config::parse(
                &format!("dialect: {dialect}\nresolvers:\n  scratch: {entry}\n"),
                std::path::Path::new("pbps.yml"),
            )
            .unwrap();
            assert_eq!(
                producer::same_instance(driver, &config.resolvers["scratch"]),
                expected,
                "{dialect} {entry}"
            );
        }
    }

    // Unix only: the fixture makes Unix symlinks.
    #[test]
    #[cfg(unix)]
    fn an_externals_file_is_read_only_from_under_the_project_root() {
        use std::path::{Path, PathBuf};
        let dir = std::env::temp_dir().join(format!(
            "pbps-externals-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = dir.join("project");
        std::fs::create_dir_all(root.join("db")).unwrap();
        std::fs::write(root.join("db/externals.sql"), "CREATE SCHEMA ext;").unwrap();
        std::fs::write(dir.join("outside.sql"), "CREATE SCHEMA other;").unwrap();
        std::os::unix::fs::symlink(dir.join("outside.sql"), root.join("db/link.sql")).unwrap();
        std::os::unix::fs::symlink(root.join("db/externals.sql"), root.join("inside.sql")).unwrap();

        assert_eq!(
            read_externals(&root, Path::new("db/externals.sql")).as_deref(),
            Ok("CREATE SCHEMA ext;")
        );
        // A symlink that stays inside the project is the file it names.
        assert_eq!(
            read_externals(&root, Path::new("inside.sql")).as_deref(),
            Ok("CREATE SCHEMA ext;")
        );
        // A parent step that comes back inside the root is still relative.
        assert_eq!(
            read_externals(&root, Path::new("db/../db/externals.sql")).as_deref(),
            Ok("CREATE SCHEMA ext;")
        );
        // Negative: an absolute path is refused even inside the root.
        for path in [root.join("db/externals.sql"), dir.join("outside.sql")] {
            let refused = read_externals(&root, &path).unwrap_err();
            assert!(refused.starts_with("is an absolute path"), "{refused}");
        }
        // Negative: a parent step and a symlink each lead outside the
        // project root, to a file that exists.
        for path in [
            PathBuf::from("../outside.sql"),
            PathBuf::from("db/link.sql"),
        ] {
            let refused = read_externals(&root, &path).unwrap_err();
            assert!(
                refused.starts_with("lies outside the project root"),
                "{}: {refused}",
                path.display()
            );
        }
        // Negative: a missing file is unreadable, not empty.
        assert!(
            read_externals(&root, Path::new("db/missing.sql"))
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
            externals: None,
            outside_writes: Default::default(),
        });
        let server = selection(ResolverProfile::Server {
            same_instance: None,
            url_env: "PBPS_UNUSED".into(),
            standard: None,
            externals: None,
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
            let granted = planning_identity(&pbps_pg::Postgres::new(), &mut conn, &server).await;
            conn.execute(&format!("SET ROLE {role}"))
                .await
                .expect("set role");
            let docker_refused =
                planning_identity(&pbps_pg::Postgres::new(), &mut conn, &docker).await;
            let refused = planning_identity(&pbps_pg::Postgres::new(), &mut conn, &server).await;
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
            diagnostic: None,
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
            diagnostic: None,
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
                same_instance: None,
                url_env: "PBPS_RESOLVER_UNSET_1515".into(),
                standard: None,
                externals: None,
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
                options: Default::default(),
            },
        );
        let mut desired = base.clone();
        desired.modules.insert(
            "app.t()".parse().unwrap(),
            Module {
                kind: ModuleKind::Function,
                description: None,
                definition: "RETURNS int LANGUAGE sql RETURN 1".into(),
                options: Default::default(),
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
        let dependencies = Default::default();
        let request = |target_identity| Request {
            sources: None,
            dependencies: &dependencies,
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
        let refusal = |target_identity| match rt.block_on(producer::produce::<pbps_pg::Postgres>(
            &request(target_identity),
            &pbps_diff::diff(
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
            &mut Entry::new(&selection, &project, &target),
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

    /// Both engines require the fingerprint key before acquiring scratch.
    #[test]
    fn both_engines_require_the_key_before_acquiring_scratch() {
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
                sources: None,
                dependencies: &Default::default(),
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
            match rt.block_on(resolve::<pbps_pg::Postgres>(
                &request,
                &mut Entry::new(&selection, &project, &target),
            )) {
                Err(Refused::Finding(finding)) => finding,
                Err(Refused::Unanswerable(error)) => panic!("unanswerable: {error}"),
                Ok(_) => panic!("answered"),
            }
        };
        let mssql = refusal(
            "sqlserver://127.0.0.1:1;database=pbps",
            pbps_db::Driver::Mssql,
        );
        assert_eq!(mssql.id, "resolver.key");
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
                proven_by_dependency: 0,
                body_not_checked: 0,
                rebuilt: 0,
                authorization: None,
                authorization_notice: None,
            }
        );
    }

    /// A kept view, a second one and a procedural function, beside a routine
    /// that arrives under a name none of them could reach.
    fn arrival_beside_three_kept_surfaces() -> (Schema, Schema) {
        let (mut base, _) = arrival();
        let module = |kind, definition: &str| Module {
            kind,
            description: None,
            definition: definition.into(),
            options: Default::default(),
        };
        base.modules.insert(
            "app.w".parse().unwrap(),
            module(ModuleKind::View, "SELECT 1 AS one"),
        );
        base.modules.insert(
            "app.p()".parse().unwrap(),
            module(
                ModuleKind::Function,
                "RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$",
            ),
        );
        let mut desired = base.clone();
        desired.modules.insert(
            "app.late()".parse().unwrap(),
            module(ModuleKind::Function, "RETURNS int LANGUAGE sql RETURN 1"),
        );
        (base, desired)
    }

    /// The report must let a reader tell three answers apart: nothing moved,
    /// the catalog proved it, and a body nobody checked (DEC-2987.1).
    #[test]
    fn the_report_counts_a_proof_by_dependency_and_an_unchecked_body_apart() {
        use pbps_model::resolver::{
            CatalogName, DependencyEdge, DependencyFacts, NameAvailability, ReadFact, Surface,
            SurfaceDependencies,
        };
        use std::collections::{BTreeMap, BTreeSet};
        let (base, desired) = arrival_beside_three_kept_surfaces();
        let selected = selection();
        let base_ids = ids(&base, &IdsFile::default());
        let desired_ids = ids(&desired, &base_ids);
        let base_side = pbps_diff::Side {
            schema: &base,
            ids: &base_ids,
        };
        let desired_side = pbps_diff::Side {
            schema: &desired,
            ids: &desired_ids,
        };
        let dialect = pbps_pg::Postgres::new();
        let ordinary =
            pbps_diff::diff(base_side, desired_side, &dialect, &Hints::default()).unwrap();
        let needed =
            pbps_diff::resolver::needed_facts(base_side, desired_side, &ordinary, &dialect)
                .expect("a routine arrives beside kept surfaces");
        let procedural = Surface::Module("app.p()".parse().unwrap());
        let reads_table = Surface::Module("app.v".parse().unwrap());
        let facts = DependencyFacts {
            surfaces: needed
                .surfaces
                .iter()
                .map(|surface| {
                    let edges = if *surface == reads_table {
                        BTreeSet::from([DependencyEdge::Relation("app.t".parse().unwrap())])
                    } else {
                        BTreeSet::new()
                    };
                    let found = if *surface == procedural {
                        SurfaceDependencies::Unparsed(edges)
                    } else {
                        SurfaceDependencies::Parsed(edges)
                    };
                    (surface.clone(), found)
                })
                .collect(),
            relation_ancestors: ReadFact::Known(BTreeMap::new()),
            catalog_names: needed
                .arriving_names
                .iter()
                .map(|name| (name.clone(), CatalogName::Clear))
                .collect(),
            engine_named_relations: BTreeMap::new(),
            candidate_names: needed
                .arriving_candidates
                .iter()
                .map(|name| (name.clone(), NameAvailability::Free))
                .collect(),
        };

        let Case::NotNeeded { assessment } = case_with_facts(
            Some(&selected),
            base_side,
            desired_side,
            &ordinary,
            &dialect,
            Some(&facts),
        ) else {
            panic!("the facts prove every kept surface");
        };
        let report = ResolverAssessment::not_needed(&assessment);
        assert_eq!(
            (
                report.unaffected,
                report.proven_by_dependency,
                report.body_not_checked,
                report.rebuilt
            ),
            (3, 2, 1, 0)
        );
        let reason = report.not_needed_reason();
        assert!(
            reason.contains("2 of them proven by dependency"),
            "{reason}"
        );
        assert!(!reason.contains("moves no name"), "{reason}");
        let notice = report.body_notice().expect("one body was not checked");
        assert!(notice.starts_with("1 kept routine has a body"), "{notice}");

        // Without the facts the same plan is a question, not a proof.
        assert!(matches!(
            case(
                Some(&selected),
                base_side,
                desired_side,
                &ordinary,
                &dialect
            ),
            Case::Required { .. }
        ));
    }

    /// A plan that moves no name has nothing to prove and nothing unchecked
    /// to report, so neither wording appears.
    #[test]
    fn a_plan_that_moves_no_name_claims_no_proof_and_prints_no_body_line() {
        let (base, _) = arrival_beside_three_kept_surfaces();
        let selected = selection();
        let Case::NotNeeded { assessment } = decide(Some(&selected), &base, &base) else {
            panic!("an empty plan raises no question that needs an engine");
        };
        let report = ResolverAssessment::not_needed(&assessment);
        assert_eq!(
            (
                report.unaffected,
                report.proven_by_dependency,
                report.body_not_checked
            ),
            (3, 0, 0)
        );
        assert!(
            report
                .not_needed_reason()
                .starts_with("this plan moves no name")
        );
        assert_eq!(report.body_notice(), None);
        let several = ResolverAssessment {
            body_not_checked: 2,
            ..report
        };
        assert!(
            several
                .body_notice()
                .unwrap()
                .starts_with("2 kept routines have bodies")
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
            changes: vec![pbps_model::PlannedChange::new(pbps_model::Change::Index(
                pbps_model::IndexChange::DropIndex {
                    table: "app.t".parse().unwrap(),
                    name: "ix_gone".into(),
                },
            ))],
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

    #[test]
    fn failed_dependency_facts_read_falls_back_to_plan_level_assessment_with_one_note() {
        let (base, desired) = arrival();
        let selected = selection();
        let base_ids = ids(&base, &IdsFile::default());
        let desired_ids = ids(&desired, &base_ids);
        let base_side = pbps_diff::Side {
            schema: &base,
            ids: &base_ids,
        };
        let desired_side = pbps_diff::Side {
            schema: &desired,
            ids: &desired_ids,
        };
        let pg = pbps_pg::Postgres::new();
        let ordinary =
            pbps_diff::resolver::ordinary(base_side, desired_side, &pg, &Hints::default()).unwrap();

        let (facts, note) = unwrap_dependency_facts(Err(pbps_db::DbError::Refused(
            "permission denied for table pg_depend".into(),
        )));
        assert!(facts.is_none());
        let note = note.expect("a failed read emits one advisory note");
        assert_eq!(note.id, DEPENDENCY_FACTS_UNREAD_ID);
        assert_eq!(note.severity, crate::output::Severity::Note);
        assert!(
            note.message
                .contains("falling back to plan-level assessment")
        );
        assert!(
            note.message
                .contains("permission denied for table pg_depend")
        );

        // With fallback facts = None, the answer matches plan-level `case(...)`.
        let Case::Required { assessment, .. } = case_with_facts(
            Some(&selected),
            base_side,
            desired_side,
            &ordinary,
            &pg,
            facts.as_ref(),
        ) else {
            panic!("fallback with no facts must keep plan-level Required");
        };
        assert_eq!(named(&assessment), "app.v");

        // Positive: Ok(Some(facts)) emits no note and narrows unrelated views to NotNeeded.
        let good_facts = pbps_model::resolver::DependencyFacts {
            surfaces: std::collections::BTreeMap::from([(
                pbps_model::resolver::Surface::Module("app.v".parse().unwrap()),
                pbps_model::resolver::SurfaceDependencies::Parsed(Default::default()),
            )]),
            relation_ancestors: pbps_model::resolver::ReadFact::Known(Default::default()),
            catalog_names: std::collections::BTreeMap::from([(
                "t".into(),
                pbps_model::resolver::CatalogName::Clear,
            )]),
            engine_named_relations: Default::default(),
            candidate_names: Default::default(),
        };
        let (ok_facts, no_note) = unwrap_dependency_facts(Ok(Some(good_facts)));
        assert!(no_note.is_none());
        assert!(matches!(
            case_with_facts(
                Some(&selected),
                base_side,
                desired_side,
                &ordinary,
                &pg,
                ok_facts.as_ref(),
            ),
            Case::NotNeeded { .. }
        ));
    }
}

#[cfg(test)]
mod authorization_notice_tests {
    use super::*;
    use pbps_model::resolver::AuthorizationReproduction;

    #[test]
    fn only_a_successful_supplied_postgres_run_has_an_authorization_notice() {
        let mut report = ResolverAssessment {
            need: ResolutionNeed::NotNeeded,
            unaffected: 0,
            proven_by_dependency: 0,
            body_not_checked: 0,
            rebuilt: 0,
            authorization: None,
            authorization_notice: None,
        };
        let json = serde_json::to_value(&report).unwrap();
        assert!(json.get("authorization").is_none());
        assert!(json.get("authorization_notice").is_none());
        report.need = ResolutionNeed::Resolved;
        report.record_authorization(AuthorizationReproduction::LoginStandin);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["authorization"], "login_standin");
        let notice = json["authorization_notice"].as_str().unwrap();
        for expected in [
            "Authorization was not reproduced",
            "Docker",
            "run-owned",
            "https://github.com/pongbiphang/pbps/issues/1163",
        ] {
            assert!(notice.contains(expected), "{notice}");
        }
        report.record_authorization(AuthorizationReproduction::Reproduced);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["authorization"], "reproduced");
        assert!(json.get("authorization_notice").is_none());
    }
}
