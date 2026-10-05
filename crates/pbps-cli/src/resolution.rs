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
    pub base: pbps_diff::Side<'a>,
    pub desired: pbps_diff::Side<'a>,
    pub hints: &'a pbps_model::Hints,
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
        .map_err(|failure| match failure {
            ProduceError::Run(error) => match error {
                // Answered: a declared surface had no sound verdict, or the
                // environment was measured and differs, or this run's profile
                // cannot serve the engine.
                Error::Binding(_) | Error::Incompatible(_) | Error::UnsupportedProfile { .. } => {
                    unresolved(request, error)
                }
                // The supplied server was measured and does not meet the
                // profile it claims: also an answer.
                Error::Container(_)
                | Error::Configuration(_)
                | Error::EngineExecutable
                | Error::Containment(_)
                | Error::Mount(_)
                | Error::TargetInstance => unresolved(request, error),
                // Not answered: the run could not establish what it needed.
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
                | Error::Scope(_) => Refused::Unanswerable(error.into()),
            },
            ProduceError::Target(_)
            | ProduceError::Recipe(_)
            | ProduceError::Acquire(_)
            | ProduceError::Cleanup(_) => Refused::Unanswerable(failure.into()),
        })
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

    fn selection() -> ResolverSelection {
        ResolverSelection {
            name: "local".into(),
            source: pbps_config::resolver::SelectionSource::Cli,
            profile: pbps_config::resolver::ResolverProfile::Server {
                url_env: "PBPS_RESOLVER_UNSET_1515".into(),
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
