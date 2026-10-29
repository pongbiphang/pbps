//! The rehearsal of a connected plan (#2379): its statements run in order on
//! a reproduction of the target's current managed state, before the plan is
//! written.
//!
//! The scratch is the one the project's selected resolver entry names, so a
//! rehearsal needs no second configuration. It is acquired through the
//! resolver's own layers (separation from the target, layout by account,
//! emptiness, externals, owned cleanup) and the externals it stages are
//! compared with the target as the resolver compares them. Authorization is
//! rehearsed under role names of the run's own (`RoleMapping`), or, where the
//! scratch account cannot create roles, not at all and the record says so: see
//! [`super::is_authorization`].

use super::{
    Base, Execution, Rejected, all_statements, local_statements, lower_from_empty, rehearse_in,
};
use crate::resolution::{Entry, Refused};
use pbps_cli::resolver::server::scratch::rehearse::{Roles, with_session};
use pbps_cli::resolver::server::{Error, ProduceError};
use pbps_db::Conn;
use pbps_model::{AuthorizationRehearsal, Hints, IdsFile, Schema, Text};

/// What a connected plan does about its rehearsal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Course {
    /// Nothing, and no record: no scratch entry is selected, or the engine
    /// has no connected rehearsal yet (SQL Server keeps today's behaviour
    /// until it does, #2379), whatever the switch says.
    Skip,
    /// The operator turned it off: the plan is sealed as not rehearsed.
    Off,
    /// Run it.
    Rehearse,
}

/// Decides the course from what the plan was asked and what it can do. Off
/// needs an entry to turn off, and an engine that rehearses.
pub fn course(driver: pbps_db::Driver, entry_selected: bool, off: bool) -> Course {
    match (driver, entry_selected, off) {
        (pbps_db::Driver::Postgres, true, true) => Course::Off,
        (pbps_db::Driver::Postgres, true, false) => Course::Rehearse,
        (pbps_db::Driver::Postgres, false, _) | (pbps_db::Driver::Mssql, _, _) => Course::Skip,
    }
}

/// Whether the per-run flag or the environment's `rehearse: false` turns the
/// rehearsal off.
pub fn switched_off(project: &pbps_config::Project, environment: Option<&str>, flag: bool) -> bool {
    flag || environment
        .and_then(|name| project.config.environments.get(name))
        .is_some_and(|environment| environment.rehearse == Some(false))
}

/// What a rehearsal decided about a plan.
pub enum Verdict {
    /// The plan converged; the record to seal into it.
    Sealed(pbps_model::Rehearsal),
    /// The plan was refused, and nothing is written.
    Refused(crate::output::Finding),
}

/// Everything the rehearsal reads of the plan being made.
pub struct Plan<'a, D> {
    pub dialect: &'a D,
    /// The target's current managed model and the identities it was read
    /// under: what the scratch is made to hold before the plan runs.
    pub base: pbps_diff::Side<'a>,
    /// The declarations as the compared engine reads them, and the plan's
    /// identity mapping.
    pub desired: pbps_diff::Side<'a>,
    /// The declarations, the comparison's other side.
    pub declared: &'a Schema,
    pub hints: &'a Hints,
    /// The hints the base was written under: the snapshot's `depends_on:`
    /// for every module it recorded. A revision that changes or drops a kept
    /// module's annotation must not reorder the definition the target still
    /// holds, which may need the old edge (an overload's caller does, since
    /// overloads are never ordered by the identifier scan).
    pub base_hints: &'a Hints,
    pub dependencies: &'a pbps_model::dependency::Dependencies,
    /// The plan as ordered, which lowers again under the run's role names.
    pub ordered: &'a pbps_diff::order::OrderedPlan<D>,
    /// The ordered plan's operations, and the statements they lowered to.
    pub operations: &'a pbps_model::operation::Operations,
    pub statements: &'a crate::emission::LoweredPlan,
    pub staged: bool,
}

impl<D> Plan<'_, D> {
    /// Every role the plan, the base it builds on and the declarations it is
    /// compared with name.
    fn named_roles(&self) -> std::collections::BTreeSet<pbps_model::RoleName> {
        let mut named = self.base.schema.named_roles();
        named.extend(self.desired.schema.named_roles());
        named.extend(self.declared.named_roles());
        for planned in &self.ordered.changes().changes {
            named.extend(planned.change.named_roles());
        }
        named
    }
}

/// Refuses, in the planner's words and with the `CREATE ROLE` it prints, a role
/// the plan names and the target's cluster no longer has: a scratch given the
/// target's names must not make a role the target lacks, or the rehearsal would
/// pass a plan that fails at apply.
async fn refuse_absent_roles(
    conn: &mut Conn,
    named: &std::collections::BTreeSet<pbps_model::RoleName>,
) -> anyhow::Result<()> {
    let absent = pbps_pg::roles::missing_roles(conn, &named.iter().collect()).await?;
    crate::engine::refuse_missing_cluster_roles(conn.driver(), &absent)
}

/// Runs the rehearsal on the entry's scratch, which is released before this
/// returns.
pub async fn run<D: crate::emission::Engine>(
    entry: &mut Entry<'_>,
    conn: &mut Conn,
    target: &crate::db::Target,
    plan: &Plan<'_, D>,
) -> anyhow::Result<Verdict> {
    let driver = conn.driver();
    let identity = match crate::resolution::planning_identity(conn, entry.selection()).await {
        Ok(Some(identity)) => identity,
        Ok(None) => anyhow::bail!("the planning read recorded no cluster identity to bind"),
        Err(Refused::Finding(finding)) => {
            return Ok(Verdict::Refused(finding.remedy(
                "grant the read, or turn the rehearsal off with `rehearse: false` or `--no-rehearse`",
            )));
        }
        Err(Refused::Unanswerable(error)) => return Err(error),
    };
    let dialect: &dyn pbps_dialect::Dialect = plan.dialect.as_ref();
    let named = plan.named_roles();
    // The planner refused every role it could see the target lack. A role dropped
    // since is refused in its words, before a scratch is started for it.
    refuse_absent_roles(conn, &named).await?;
    // The questions the resolver's fill asks of the bindings, asked of the
    // declarations from nothing as the resolver asks them.
    let (prerequisites, prerequisite_ids) = pbps_diff::scratch_base(plan.desired);
    let bootstrap = pbps_diff::diff(
        pbps_diff::Side {
            schema: &prerequisites,
            ids: &prerequisite_ids,
        },
        plan.desired,
        dialect,
        plan.hints,
    )
    .map_err(|errors| {
        anyhow::Error::new(crate::Unexpressible(errors))
            .context("the declarations' bootstrap cannot be formed")
    })?;
    let binding = pbps_cli::resolver::server::BindingRequest {
        dependencies: plan.dependencies,
        bootstrap: &bootstrap,
        desired: plan.desired.schema,
        base: plan.base.schema,
    };
    let open = entry
        .open(driver, &identity)
        .await
        .map_err(|refused| match refused {
            Refused::Finding(finding) => anyhow::anyhow!("{}", finding.message),
            Refused::Unanswerable(error) => error,
        })?;
    let zone = conn
        .session_zone()
        .cloned()
        .unwrap_or_else(pbps_model::SessionZone::utc);
    let ran = with_session(
        driver,
        &open.connection,
        target.connection(),
        &identity,
        &binding,
        plan.base,
        plan.desired,
        plan.hints,
        &open.standard,
        open.externals.as_deref(),
        &open.provenance,
        &named,
        async |session, roles| {
            let lowered = lowered(plan, roles)?;
            let findings = rehearse_in(
                session,
                lowered.base(
                    plan.base.ids,
                    Execution {
                        zone: &zone,
                        transactional: !plan.staged,
                    },
                ),
                &lowered.plan,
                plan.declared,
                plan.desired.ids,
                dialect,
                plan.hints,
            )
            .await?;
            Ok((findings, lowered.authorization))
        },
    )
    .await;
    let (layout, found) = match ran {
        Ok(done) => done,
        Err(failure) => return Err(scratch_failed(failure)),
    };
    verdict(
        &open.provenance.entry,
        &open.provenance.scratch,
        layout,
        found,
    )
}

/// The statements a rehearsal runs, and what they say about authorization.
struct Lowered {
    base: Vec<pbps_dialect::Statement>,
    plan: Vec<pbps_dialect::Statement>,
    /// The run's role names, where the statements were lowered with them.
    mapping: Option<pbps_model::role_mapping::RoleMapping>,
    authorization: AuthorizationRehearsal,
}

impl Lowered {
    /// The base the rehearsal of these statements runs on: compared with roles
    /// exactly when the statements were lowered with the run's.
    fn base<'a>(&'a self, ids: &'a IdsFile, execution: Execution<'a>) -> Base<'a> {
        Base::Target {
            statements: &self.base,
            ids,
            execution,
            roles: self.mapping.as_ref(),
        }
    }
}

/// Lowers the base and the plan for the roles the scratch can have. With the
/// run's own role names the statements are the whole model's, the roles'
/// grants and policies included, lowered as everything else is. Without them
/// the plan the caller lowered is cut down to the objects of the database.
fn lowered<D: crate::emission::Engine>(
    plan: &Plan<'_, D>,
    roles: Roles,
) -> anyhow::Result<Lowered> {
    match roles {
        Ok(mapping) => {
            let (built, _) = lower_from_empty(
                plan.dialect,
                &mapping.schema(plan.base.schema),
                &mapping.ids(plan.base.ids),
                plan.base_hints,
            )?;
            let ordered = plan.ordered.under_role_names(&mapping).map_err(|error| {
                anyhow::anyhow!("the plan cannot be ordered under the run's role names: {error}")
            })?;
            let statements = crate::statements(&ordered, plan.dialect)?;
            Ok(Lowered {
                base: all_statements(&built),
                plan: all_statements(&statements),
                mapping: Some(mapping),
                authorization: AuthorizationRehearsal::Rehearsed,
            })
        }
        Err(why) => {
            let (built, built_from) = lower_from_empty(
                plan.dialect,
                plan.base.schema,
                plan.base.ids,
                plan.base_hints,
            )?;
            Ok(Lowered {
                base: local_statements(&built, &built_from),
                plan: local_statements(plan.statements, plan.operations),
                mapping: None,
                authorization: AuthorizationRehearsal::NotRehearsed {
                    reason: Text::from(format!(
                        "this plan's role, grant and row-security policy changes were not run: {why}"
                    )),
                },
            })
        }
    }
}

/// What the rehearsal found, as the plan's verdict.
fn verdict(
    entry: &str,
    scratch: &pbps_model::resolver::ScratchSource,
    layout: pbps_model::resolver::ScratchLayout,
    found: anyhow::Result<(super::Findings, AuthorizationRehearsal)>,
) -> anyhow::Result<Verdict> {
    match found {
        Err(error) => match error.downcast_ref::<Rejected>() {
            Some(rejected) => Ok(Verdict::Refused(
                crate::output::Finding::error("rehearsal.statement", rejected.to_string()).remedy(
                    "fix the declarations, or turn the rehearsal off with `rehearse: false` or \
                     `--no-rehearse` if the scratch is the one at fault",
                ),
            )),
            None => Err(error.context("the rehearsal could not be completed")),
        },
        Ok((findings, _)) if !findings.converged() => Ok(Verdict::Refused(
            crate::output::Finding::error(
                "rehearsal.structural",
                format!(
                    "the plan does not converge on the declarations: after applying it to a \
                     reproduction of the target's managed state, the scratch still differs:\n  {}",
                    findings.structural.join("\n  ")
                ),
            )
            .remedy("fix the declarations or the plan; nothing was written"),
        )),
        Ok((findings, authorization)) => Ok(Verdict::Sealed(pbps_model::Rehearsal::Converged {
            entry: Text::from(entry),
            scratch: scratch.clone(),
            layout,
            spelling: findings
                .spelling
                .iter()
                .map(|line| Text::from(line.as_str()))
                .collect(),
            authorization,
        })),
    }
}

/// A scratch that could not be used answers nothing about the plan, as for the
/// resolver: it is not a finding. The ways it fails that belong to pbps and
/// not to the operator's scratch name the switch that gets past them.
fn scratch_failed(failure: ProduceError) -> anyhow::Error {
    const SWITCH: &str = "turn the rehearsal off with `rehearse: false` or `--no-rehearse`";
    match failure {
        ProduceError::Run(Error::Externals(findings)) => anyhow::anyhow!(
            "the rehearsal's externals file was refused: {}; {SWITCH} if the file is not at fault",
            findings.join("; ")
        ),
        // The externals fill and its comparison are the resolver's, and
        // answer only for the declarations the resolver does.
        ProduceError::Run(Error::Binding(reason)) => anyhow::anyhow!(
            "the rehearsal stages the entry's externals as the resolver does, and the resolver \
             cannot answer for these declarations: {reason}; {SWITCH}"
        ),
        other @ (ProduceError::Acquire(_) | ProduceError::Run(_) | ProduceError::Cleanup(_)) => {
            anyhow::Error::new(other).context("the rehearsal scratch could not be used")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_db::Driver;

    fn project() -> pbps_config::Project {
        pbps_config::Project {
            root: std::env::temp_dir(),
            config: pbps_config::Config::parse(
                "dialect: postgres\nenvironments:\n  prod: {url_env: P, rehearse: false}\n  \
                 stage: {url_env: S, rehearse: true}\n  dev: {url_env: D}\n",
                std::path::Path::new("pbps.yml"),
            )
            .unwrap(),
        }
    }

    fn findings(structural: &[&str], spelling: &[&str]) -> super::super::Findings {
        super::super::Findings {
            built: 1,
            applied: 1,
            structural: structural.iter().map(|line| (*line).to_owned()).collect(),
            spelling: spelling.iter().map(|line| (*line).to_owned()).collect(),
            externals_staged: false,
        }
    }

    fn judged(found: anyhow::Result<super::super::Findings>) -> anyhow::Result<Verdict> {
        verdict(
            "ci",
            &pbps_model::resolver::ScratchSource::Server {},
            pbps_model::resolver::ScratchLayout::Supplied,
            found.map(|findings| (findings, AuthorizationRehearsal::Rehearsed)),
        )
    }

    /// A plan that leaves a structural difference is refused, and the
    /// refusal names the difference; one that leaves only respellings is
    /// sealed with them.
    #[test]
    fn a_structural_difference_refuses_and_a_respelling_is_sealed() {
        let Ok(Verdict::Refused(finding)) = judged(Ok(findings(
            &["check ck on app.t is declared but the plan did not create it"],
            &["x"],
        ))) else {
            panic!("a structural difference refuses");
        };
        assert_eq!(finding.id, "rehearsal.structural");
        assert!(
            finding.message.contains("check ck on app.t is declared"),
            "{}",
            finding.message
        );
        let Ok(Verdict::Sealed(pbps_model::Rehearsal::Converged {
            entry,
            layout,
            spelling,
            ..
        })) = judged(Ok(findings(&[], &["check c on t", "index i on t"])))
        else {
            panic!("a respelling is sealed");
        };
        assert_eq!(entry.raw(), "ci");
        assert_eq!(layout, pbps_model::resolver::ScratchLayout::Supplied);
        assert_eq!(spelling.len(), 2);
        // Negative: nothing differing at all is sealed with no spelling.
        let Ok(Verdict::Sealed(pbps_model::Rehearsal::Converged { spelling, .. })) =
            judged(Ok(findings(&[], &[])))
        else {
            panic!("an exact match is sealed");
        };
        assert!(spelling.is_empty());
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(f)
    }

    /// A role the plan names and the cluster lacks is refused with the planner's
    /// text and the statement that makes it; roles the cluster has are not.
    #[test]
    #[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
    fn a_role_the_target_lost_since_planning_is_refused_in_the_planners_words() {
        block_on(async {
            let url = std::env::var("PBPS_TEST_PG_DB").expect("PBPS_TEST_PG_DB");
            let mut conn = Conn::connect(Driver::Postgres, &url)
                .await
                .expect("connect to the live PostgreSQL");
            let gone = format!("pbps_absent_{}", std::process::id());
            let named = std::collections::BTreeSet::from([
                pbps_model::RoleName::from("postgres"),
                pbps_model::RoleName::from(gone.as_str()),
            ]);
            let refusal = refuse_absent_roles(&mut conn, &named)
                .await
                .expect_err("a role the cluster lacks");
            let text = refusal.to_string();
            assert!(
                text.contains(&format!(
                    "role `{gone}` is declared here and the cluster does not have it"
                )),
                "{text}"
            );
            assert!(text.contains(&format!("CREATE ROLE \"{gone}\";")), "{text}");
            assert!(!text.contains("`postgres`"), "{text}");
            // Negative: every role present, nothing to refuse.
            let present =
                std::collections::BTreeSet::from([pbps_model::RoleName::from("postgres")]);
            refuse_absent_roles(&mut conn, &present)
                .await
                .expect("roles the cluster has");
        });
    }

    /// What the rehearsal says about authorization is sealed as it was, and
    /// does not soften a refusal: a downgraded rehearsal that left a structural
    /// difference refuses like any other.
    #[test]
    fn the_record_says_whether_authorization_ran_and_a_downgrade_still_refuses() {
        let skipped = AuthorizationRehearsal::NotRehearsed {
            reason: "no CREATEROLE".into(),
        };
        let Ok(Verdict::Sealed(pbps_model::Rehearsal::Converged { authorization, .. })) = verdict(
            "ci",
            &pbps_model::resolver::ScratchSource::Server {},
            pbps_model::resolver::ScratchLayout::Supplied,
            Ok((findings(&[], &[]), skipped.clone())),
        ) else {
            panic!("a downgraded rehearsal that converged is sealed");
        };
        assert_eq!(authorization, skipped);
        let Ok(Verdict::Sealed(pbps_model::Rehearsal::Converged { authorization, .. })) =
            judged(Ok(findings(&[], &[])))
        else {
            panic!("a full rehearsal is sealed");
        };
        assert_eq!(authorization, AuthorizationRehearsal::Rehearsed);
        // Negative: skipping a class is not a reason to pass a plan that does
        // not converge.
        let Ok(Verdict::Refused(finding)) = verdict(
            "ci",
            &pbps_model::resolver::ScratchSource::Server {},
            pbps_model::resolver::ScratchLayout::Supplied,
            Ok((findings(&["table app.t is declared"], &[]), skipped)),
        ) else {
            panic!("a structural difference refuses a downgraded rehearsal too");
        };
        assert_eq!(finding.id, "rehearsal.structural");
    }

    /// Roles are part of what the scratch is compared on exactly when the
    /// statements were lowered with the run's own names for them.
    #[test]
    fn roles_are_compared_exactly_when_the_statements_carry_the_runs_names() {
        let zone = pbps_model::SessionZone::utc();
        let execution = Execution {
            zone: &zone,
            transactional: true,
        };
        let ids = IdsFile::default();
        let lowered = |mapping| Lowered {
            base: Vec::new(),
            plan: Vec::new(),
            mapping,
            authorization: AuthorizationRehearsal::Rehearsed,
        };
        let mapped = lowered(Some(pbps_model::role_mapping::RoleMapping::default()));
        assert!(mapped.base(&ids, execution).compares_roles());
        assert!(!lowered(None).base(&ids, execution).compares_roles());
    }

    /// Only the engine refusing a statement refuses the plan. A scratch that
    /// could not be read back answers nothing about it, and is an error.
    #[test]
    fn only_a_rejected_statement_refuses_the_plan() {
        let rejected = anyhow::Error::new(Rejected::of_test(
            "the rehearsal scratch rejected statement 1",
        ));
        let Ok(Verdict::Refused(finding)) = judged(Err(rejected)) else {
            panic!("a rejected statement refuses");
        };
        assert_eq!(finding.id, "rehearsal.statement");
        assert!(finding.message.contains("rejected statement 1"));
        assert!(
            finding
                .remedy
                .as_deref()
                .is_some_and(|r| r.contains("--no-rehearse"))
        );
        assert!(judged(Err(anyhow::anyhow!("connection reset"))).is_err());
    }

    /// SQL Server keeps today's behaviour until its own rehearsal lands: no
    /// scratch is acquired and no record is made, whatever the entry and the
    /// switch say. A record that read "off" there would claim a choice nobody
    /// had, and a rehearsal would claim a check nobody ran.
    #[test]
    fn sql_server_never_rehearses_and_never_records() {
        for entry in [false, true] {
            for off in [false, true] {
                assert_eq!(
                    course(Driver::Mssql, entry, off),
                    Course::Skip,
                    "{entry} {off}"
                );
            }
        }
    }

    #[test]
    fn postgres_rehearses_by_default_when_an_entry_is_selected() {
        assert_eq!(course(Driver::Postgres, true, false), Course::Rehearse);
        assert_eq!(course(Driver::Postgres, true, true), Course::Off);
        // Negative: with no entry there is nothing to rehearse on or to turn
        // off, so there is no record either way.
        assert_eq!(course(Driver::Postgres, false, false), Course::Skip);
        assert_eq!(course(Driver::Postgres, false, true), Course::Skip);
    }

    #[test]
    fn the_switch_is_the_flag_or_an_environment_that_says_false() {
        let project = project();
        assert!(switched_off(&project, Some("prod"), false));
        assert!(switched_off(&project, Some("stage"), true));
        assert!(switched_off(&project, None, true));
        // Negative: `true`, unset, unknown and absent environments leave the
        // default on.
        assert!(!switched_off(&project, Some("stage"), false));
        assert!(!switched_off(&project, Some("dev"), false));
        assert!(!switched_off(&project, Some("nowhere"), false));
        assert!(!switched_off(&project, None, false));
    }
}
