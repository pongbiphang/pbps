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
    /// Nothing, and no record: no scratch entry is selected, whatever the
    /// switch says.
    Skip,
    /// The operator turned it off: the plan is sealed as not rehearsed.
    Off,
    /// Run it.
    Rehearse,
}

/// Decides the course from what the plan was asked. Off needs an entry to
/// turn off. Both engines rehearse (SQL Server since #2379 PR 3).
pub fn course(entry_selected: bool, off: bool) -> Course {
    match (entry_selected, off) {
        (true, true) => Course::Off,
        (true, false) => Course::Rehearse,
        (false, _) => Course::Skip,
    }
}

/// Whether the per-run flag or the environment's `rehearse: false` turns the
/// rehearsal off.
pub fn switched_off(project: &pbps_config::Project, environment: Option<&str>, flag: bool) -> bool {
    flag || environment
        .and_then(|name| project.config.environments.get(name))
        .is_some_and(|environment| environment.rehearse == Some(false))
}

/// What each package a plan installs owned on the scratch after the replay.
pub type Manifests =
    std::collections::BTreeMap<pbps_model::ObjectLocalName, pbps_model::ExtensionProvider>;

/// What a rehearsal decided about a plan.
pub enum Verdict {
    /// The plan converged; the record to seal into it, and the manifest of
    /// each package it installs as the scratch held it afterwards.
    Sealed(pbps_model::Rehearsal, Manifests),
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
    // (SQL Server's roles and users are the database's own, so there is no
    // cluster to ask: DEC-2379.3.)
    if driver == pbps_db::Driver::Postgres {
        refuse_absent_roles(conn, &named).await?;
    }
    // The questions the resolver's fill asks of the bindings, asked of the
    // declarations from nothing as the resolver asks them.
    let (prerequisites, prerequisite_ids) = pbps_diff::bootstrap_base(plan.desired);
    // As the resolver forms it (`resolution::resolve`): from nothing no module
    // stands to be rebuilt, so the plan is not observed, and the text screen of
    // a standing computed column has nothing standing to judge.
    let bootstrap = pbps_diff::bootstrap(
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
    // PostgreSQL's scratch compiles the base's statements in the plan order.
    // SQL Server's compiles the target's own namespace through the engine's
    // creation trials, which take the bootstrap of the base to build (a
    // default or computed column may call a function the base also holds).
    let base_bootstrap = match driver {
        pbps_db::Driver::Postgres => None,
        pbps_db::Driver::Mssql => Some(base_bootstrap(plan)?),
    };
    let binding = pbps_cli::resolver::server::BindingRequest {
        dependencies: plan.dependencies,
        bootstrap: base_bootstrap.as_ref().unwrap_or(&bootstrap),
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
        async |session, mut reader, roles| {
            let lowered = lowered(plan, driver, roles)?;
            let findings = rehearse_in(
                session,
                reader.as_mut(),
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
    driver: pbps_db::Driver,
    roles: Roles,
) -> anyhow::Result<Lowered> {
    match roles {
        // The namespace of the base is already compiled (the scratch session
        // did it with the engine's trials); what is left of it is who may do
        // what, which the plan's own statements then change.
        Ok(mapping) if driver == pbps_db::Driver::Mssql => Ok(Lowered {
            base: all_statements(&base_remainder(plan)?.0),
            plan: all_statements(plan.statements),
            mapping: Some(mapping),
            authorization: AuthorizationRehearsal::Rehearsed,
        }),
        Err(why) if driver == pbps_db::Driver::Mssql => Ok(Lowered {
            base: {
                let (remainder, operations) = base_remainder(plan)?;
                local_statements(&remainder, &operations)
            },
            plan: local_statements(plan.statements, plan.operations),
            mapping: None,
            authorization: AuthorizationRehearsal::NotRehearsed {
                reason: Text::from(format!(
                    "this plan's role, grant and row-security policy changes were not run: {why}"
                )),
            },
        }),
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

/// The bootstrap of the base: the plan from nothing to the target's managed
/// model, as the differ writes it. SQL Server's scratch builds the namespace
/// from it through the engine's creation trials.
fn base_bootstrap<D: crate::emission::Engine>(
    plan: &Plan<'_, D>,
) -> anyhow::Result<pbps_diff::order::DiffPlan> {
    let (prerequisites, prerequisite_ids) = pbps_diff::bootstrap_base(plan.base);
    pbps_diff::bootstrap(
        pbps_diff::Side {
            schema: &prerequisites,
            ids: &prerequisite_ids,
        },
        plan.base,
        plan.dialect.as_ref(),
        plan.base_hints,
    )
    .map_err(|errors| {
        anyhow::Error::new(crate::Unexpressible(errors))
            .context("the target's managed state cannot be formed from nothing")
    })
}

/// What a SQL Server base holds besides its namespace: the roles it declares,
/// the privileges they hold, the row-security policies that name them and the
/// rows it declares. The namespace is compiled first, so this is the plan from
/// a model that has all of that and none of this to the model itself, and the
/// ordinary differ and scheduler write it.
fn base_remainder<D: crate::emission::Engine>(
    plan: &Plan<'_, D>,
) -> anyhow::Result<(
    crate::emission::LoweredPlan,
    pbps_model::operation::Operations,
)> {
    let mut without = plan.base.schema.clone();
    without.roles.clear();
    for table in without.tables.values_mut() {
        table.row_security = Default::default();
        table.row_policies.clear();
        table.data = None;
    }
    let mut ids = plan.base.ids.clone();
    ids.roles.clear();
    super::lower_between(
        plan.dialect,
        pbps_diff::Side {
            schema: &without,
            ids: &ids,
        },
        plan.base,
        plan.base_hints,
    )
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
        Ok((findings, authorization)) => Ok(Verdict::Sealed(
            pbps_model::Rehearsal::Converged {
                entry: Text::from(entry),
                scratch: scratch.clone(),
                layout,
                spelling: findings
                    .spelling
                    .iter()
                    .map(|line| Text::from(line.as_str()))
                    .collect(),
                authorization,
            },
            findings.extensions,
        )),
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
            extensions: Default::default(),
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
        let Ok(Verdict::Sealed(
            pbps_model::Rehearsal::Converged {
                entry,
                layout,
                spelling,
                ..
            },
            _,
        )) = judged(Ok(findings(&[], &["check c on t", "index i on t"])))
        else {
            panic!("a respelling is sealed");
        };
        assert_eq!(entry.raw(), "ci");
        assert_eq!(layout, pbps_model::resolver::ScratchLayout::Supplied);
        assert_eq!(spelling.len(), 2);
        // Negative: nothing differing at all is sealed with no spelling.
        let Ok(Verdict::Sealed(pbps_model::Rehearsal::Converged { spelling, .. }, _)) =
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
        let Ok(Verdict::Sealed(pbps_model::Rehearsal::Converged { authorization, .. }, _)) =
            verdict(
                "ci",
                &pbps_model::resolver::ScratchSource::Server {},
                pbps_model::resolver::ScratchLayout::Supplied,
                Ok((findings(&[], &[]), skipped.clone())),
            )
        else {
            panic!("a downgraded rehearsal that converged is sealed");
        };
        assert_eq!(authorization, skipped);
        let Ok(Verdict::Sealed(pbps_model::Rehearsal::Converged { authorization, .. }, _)) =
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

    /// The course depends on the entry and the switch alone: both engines
    /// rehearse, so no engine reads as "never rehearses" and no record is made
    /// where there is nothing to rehearse on or to turn off.
    #[test]
    fn an_entry_decides_the_course_whatever_the_engine() {
        assert_eq!(course(true, false), Course::Rehearse);
        assert_eq!(course(true, true), Course::Off);
        // Negative: with no entry there is nothing to rehearse on or to turn
        // off, so there is no record either way.
        assert_eq!(course(false, false), Course::Skip);
        assert_eq!(course(false, true), Course::Skip);
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
