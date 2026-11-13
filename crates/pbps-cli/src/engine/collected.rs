//! Choices of engine that commands used to make for themselves (#2798).
//!
//! Each function here answers one question a command asks and routes it by the
//! driver, the same way the rest of [`super`] does, and each `match` is
//! exhaustive: a third engine is a compile error here until it has an answer.
//! Where only one engine can be asked, the other answers with the neutral
//! value the caller always got from it (`None`, an empty set, `Ok(())`), so a
//! command no longer needs to know which engine has what.

use super::*;

/// Reads the routines a plan pins against replacement (DEC-319.1).
///
/// PostgreSQL reads them from `pg_proc`. SQL Server pins no routine, so it
/// answers `None`, which a caller reads as "nothing to seal or check".
pub(crate) async fn read_pinned_routines(
    conn: &mut Conn,
    within: pbps_pg::pins::Within,
) -> Result<Option<pbps_pg::pins::Read>, DbError> {
    match conn.driver() {
        Driver::Postgres => pbps_pg::pins::read(conn, within).await.map(Some),
        Driver::Mssql => Ok(None),
    }
}

/// Checks the declared types a plan relies on against the pins it holds.
///
/// PostgreSQL requires a pin for every declared type and compares each with
/// the database. SQL Server has no such pin, so there is nothing to check.
pub(crate) async fn check_declared_type_pins(
    conn: &mut Conn,
    project: &pbps_config::Project,
    plan: &pbps_model::SavedPlan,
    managed: &crate::pins::Managed,
    at: crate::pins::Check<'_>,
) -> anyhow::Result<()> {
    match conn.driver() {
        Driver::Postgres => {
            crate::pins::require_declared_types(
                &managed.declared_types,
                &plan.declared_type_pins,
                &plan.changes,
            )?;
            pbps_pg::type_refs::declared::check(
                conn,
                &plan.declared_type_pins,
                &crate::pins::applying(project, plan),
                at.within(),
                matches!(
                    at,
                    crate::pins::Check::BeforeRecording { .. }
                        | crate::pins::Check::BeforeCommit { .. }
                        | crate::pins::Check::BeforeClosing { .. }
                ),
                &plan.changes,
                at.executed(),
            )
            .await?;
            Ok(())
        }
        Driver::Mssql => Ok(()),
    }
}

/// How the differ judges a computed column that stands throughout the plan.
///
/// SQL Server's are judged by the catalog's edges (`order_computed_by_edges`),
/// under the database's collation. PostgreSQL has no computed-column edges to
/// read, so its are judged by the text screen (#1460).
pub(crate) fn computed_column_screen(driver: Driver) -> pbps_diff::Screen {
    profile::profile(driver).computed_column_screen
}

/// Whether the rows an enum rebuild would strand are counted at plan time.
///
/// PostgreSQL counts the rows that hold a label an enum rebuild removes, or
/// that a domain's narrower base cannot hold, by the same probes apply runs,
/// so the plan is refused by name and with a count before anything is written
/// (DEC-1635.7, DEC-1635.9). SQL Server has neither enums nor domains.
pub(crate) fn counts_rows_an_enum_retype_strands(driver: Driver, changes: &ChangeSet) -> bool {
    profile::profile(driver).counts_stranded_rows_at_plan_time && enum_retype_strands_rows(changes)
}

/// Whether the plan retypes an enum so that rows could be stranded: a cast that
/// changes the labels' meaning, or a relabel that removes one. The selection
/// the plan-time probes are asked for, shared with the default of
/// `PlanSession::probe_stranded_rows`.
pub(crate) fn enum_retype_strands_rows(changes: &ChangeSet) -> bool {
    changes.changes.iter().any(|p| {
        matches!(&p.change, pbps_model::Change::EnumRebuild {
            step: pbps_model::EnumRebuildStep::Retype { values, .. }, ..
        } if match values.as_ref() {
            pbps_model::ValueChange::Cast { from, to, .. } => from != to,
            pbps_model::ValueChange::Relabel { removed } => !removed.is_empty(),
        })
    })
}

/// Whether planning vets declared domain `CHECK` and `DEFAULT` code on the
/// resolver's scratch before the plan is sealed.
///
/// PostgreSQL has domains and does; SQL Server has none.
pub(crate) fn vets_declared_domain_code(driver: Driver) -> bool {
    profile::profile(driver).vets_declared_domain_code
}

/// Recounts, where the plan runs, the dependents of the enums it rebuilds: a
/// dependent that appeared on an enum after the plan was made is refused by
/// name before the first statement (DEC-1635.7). Inside the apply's own
/// transaction, after the lock.
///
/// PostgreSQL only; SQL Server has no enum to rebuild.
pub(crate) async fn recheck_enum_rebuild_census(
    conn: &mut Conn,
    changes: &ChangeSet,
    stored: &IdsFile,
    resuming: bool,
) -> anyhow::Result<()> {
    match conn.driver() {
        Driver::Postgres => {
            if !changes
                .changes
                .iter()
                .any(|p| matches!(p.change, pbps_model::Change::EnumRebuild { .. }))
            {
                return Ok(());
            }
            pbps_pg::type_rebuild::census(conn, changes, None, stored, resuming)
                .await
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!("{e}"))
        }
        Driver::Mssql => Ok(()),
    }
}

/// The objects the database already holds under the history-table names a plan
/// would create (#1176).
///
/// SQL Server takes an existing table whose columns match as the history,
/// rows, layout and all, where any other `CREATE` would be refused (measured
/// on 17.0), so it is asked. PostgreSQL has no system-versioned tables and
/// answers `None`.
pub(crate) async fn history_name_occupants(
    conn: &mut Conn,
    names: &[TableName],
) -> anyhow::Result<Option<Vec<pbps_mssql::catalog::NameOccupant>>> {
    match conn.driver() {
        Driver::Mssql => Ok(Some(
            pbps_mssql::catalog::object_name_occupants(conn, names, &[]).await?,
        )),
        Driver::Postgres => Ok(None),
    }
}

/// Judges the expressions the plan gives a table before any probe evaluates
/// them, and answers whether one was left unjudged.
///
/// An expression that is not admitted yet, or cannot be bound before the plan
/// creates what it uses, is left to the check just before its statement; the
/// probes that would run it are skipped, not run unjudged. PostgreSQL only;
/// SQL Server leaves none unjudged.
pub(crate) async fn table_expressions_left_unjudged(
    conn: &mut Conn,
    admission: &pbps_pg::admission::Admission,
    changes: &[pbps_model::PlannedChange],
) -> bool {
    match conn.driver() {
        Driver::Postgres => {
            let mut table_unjudged = false;
            for planned in changes {
                match vet_table_expressions(
                    conn,
                    admission,
                    pbps_pg::pins::Within::OwnTransaction,
                    pbps_pg::table_expressions::Moment::Early,
                    &planned.change,
                )
                .await
                {
                    Ok(pbps_pg::table_expressions::Vetted::Admitted) => {}
                    // Not judged, however it came about: refused, or unbound here.
                    Ok(pbps_pg::table_expressions::Vetted::Unjudged(_)) | Err(_) => {
                        table_unjudged |= pbps_pg::table_expressions::probed(&planned.change);
                    }
                }
            }
            table_unjudged
        }
        Driver::Mssql => false,
    }
}

/// Refuses drafting a resolver's externals file from a target whose engine
/// has no binding adapter.
///
/// PostgreSQL has one; SQL Server does not yet (#619, #620).
pub(crate) fn refuse_externals_draft_unsupported(driver: Driver) -> anyhow::Result<()> {
    match profile::profile(driver).externals_draft_refusal {
        None => Ok(()),
        Some(refusal) => anyhow::bail!(refusal),
    }
}

/// Refuses, in the planner's words and with the `CREATE ROLE` it prints, a role
/// the plan names and the target's cluster no longer has.
///
/// PostgreSQL's roles belong to the cluster, so they are asked. SQL Server's
/// roles and users are the database's own, so there is no cluster to ask
/// (DEC-2379.3).
pub(crate) async fn refuse_roles_absent_from_cluster(
    conn: &mut Conn,
    named: &BTreeSet<pbps_model::RoleName>,
) -> anyhow::Result<()> {
    match conn.driver() {
        Driver::Postgres => {
            let absent = pbps_pg::roles::missing_roles(conn, &named.iter().collect()).await?;
            refuse_missing_cluster_roles(conn.driver(), &absent)
        }
        Driver::Mssql => Ok(()),
    }
}

/// The bootstrap of the base that the rehearsal's scratch is built from, when
/// the scratch needs one.
///
/// PostgreSQL's scratch compiles the base's statements in the plan order, so
/// it needs none. SQL Server's compiles the target's own namespace through the
/// engine's creation trials, which take the bootstrap of the base to build (a
/// default or computed column may call a function the base also holds).
pub(crate) fn scratch_base_bootstrap<D: crate::emission::Engine>(
    driver: Driver,
    plan: &crate::rehearsal::connected::Plan<'_, D>,
) -> anyhow::Result<Option<pbps_diff::order::DiffPlan>> {
    match driver {
        Driver::Postgres => Ok(None),
        Driver::Mssql => Ok(Some(crate::rehearsal::connected::base_bootstrap(plan)?)),
    }
}

/// Whether the rehearsal's scratch session has already compiled the namespace
/// of the base by the time the plan is lowered.
///
/// SQL Server's does, with the engine's trials; what is left of it is who may
/// do what. PostgreSQL's compiles the base's statements later, from the
/// lowering.
pub(crate) fn scratch_compiles_base_namespace(driver: Driver) -> bool {
    profile::profile(driver).scratch_compiles_base_namespace
}

/// How a target's refused or failed read of its engine lineage identity is
/// reported.
///
/// PostgreSQL's refusal is the deployment role's to fix and names the grant;
/// SQL Server's `family_guid` read has no such remedy and is unanswerable.
pub(crate) fn target_identity_unread(driver: Driver, error: DbError) -> crate::resolution::Refused {
    (profile::profile(driver).target_identity_unread)(error)
}

/// Whether the resolver's scratch may share the target's instance, which
/// omission never grants consent to; inapplicability stays distinct from
/// refusal.
///
/// SQL Server's scratch can land on the instance it rehearses; PostgreSQL's
/// scratch is a cluster of its own, so the question does not apply.
pub(crate) fn same_instance_consent(
    driver: Driver,
    profile: &pbps_config::resolver::ResolverProfile,
) -> pbps_model::resolver::SameInstance {
    use pbps_config::resolver::ResolverProfile;
    if !super::profile::profile(driver).scratch_may_share_the_instance {
        return pbps_model::resolver::SameInstance::NotApplicable;
    }
    let choice = match profile {
        ResolverProfile::Server { same_instance, .. } => same_instance.unwrap_or_default(),
        ResolverProfile::Docker { .. } => pbps_config::resolver::SameInstance::Refuse,
    };
    match choice {
        pbps_config::resolver::SameInstance::Refuse => pbps_model::resolver::SameInstance::Refused,
        pbps_config::resolver::SameInstance::Allow => pbps_model::resolver::SameInstance::Allowed,
    }
}

/// Reads what the target assertions are held against: the planning target's
/// own identity, which an assertion login must match.
///
/// An engine's error stops planning with the same finding for either engine.
pub(crate) async fn read_assertion_target(
    planning: &mut Conn,
) -> Result<crate::plan_assert::Expected, crate::plan_assert::Stop> {
    use crate::plan_assert::stop;
    match planning.driver() {
        Driver::Postgres => Ok(crate::plan_assert::Expected::Postgres(
            pbps_pg::assertions::Target::read(planning)
                .await
                .map_err(|e| stop("the planning target", e))?,
        )),
        Driver::Mssql => Ok(crate::plan_assert::Expected::Mssql(
            pbps_mssql::assertions::Target::read(planning)
                .await
                .map_err(|e| stop("the planning target", e))?,
        )),
    }
}
