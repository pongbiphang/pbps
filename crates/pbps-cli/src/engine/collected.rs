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

/// Reads the `NOT VALID` checks of the domains standing in the database.
///
/// PostgreSQL has them. SQL Server has no domains, so it answers none.
pub(crate) async fn standing_not_valid_domain_checks(
    conn: &mut Conn,
) -> anyhow::Result<Vec<pbps_pg::domain_alter::NotValid>> {
    match conn.driver() {
        Driver::Postgres => pbps_pg::domain_alter::not_valid(conn)
            .await
            .map_err(|e| anyhow::anyhow!("{e}")),
        Driver::Mssql => Ok(Vec::new()),
    }
}

/// Adds to `hints` the facts about the target that planning a type or an
/// extension change needs and the differ cannot see: a rebuilt enum or domain
/// keeps its comment, which `CREATE TYPE` does not copy, and which domains an
/// array holds, which PostgreSQL refuses a new check on in place and a rebuild
/// takes instead (DEC-1635.7, DEC-1635.9), and the extension facts (#2447).
///
/// SQL Server has no such types or extensions, so it adds nothing.
pub(crate) async fn type_rebuild_hints(
    conn: &mut Conn,
    base: &Schema,
    declared: &Schema,
    recorded_ids: &IdsFile,
    resolved_ids: &IdsFile,
    hints: &mut pbps_model::Hints,
) -> anyhow::Result<()> {
    match conn.driver() {
        Driver::Postgres => {
            let types: Vec<_> = base.types.keys().cloned().collect();
            hints.enum_comments = pbps_pg::type_rebuild::comments(conn, &types).await?;
            let domains: Vec<_> = base
                .types
                .iter()
                .filter(|(_, ty)| matches!(ty, pbps_model::ManagedType::Domain { .. }))
                .filter_map(|(name, _)| Some((recorded_ids.type_uid(name)?.clone(), name.clone())))
                .collect();
            hints.domain_array_holders =
                pbps_pg::domain_alter::array_holders(conn, &domains).await?;
            hints.extension_requires =
                crate::deploy::extension_creates(conn, base, declared, resolved_ids).await?;
            hints.extension_updates = crate::deploy::extension_update_facts(
                conn,
                base,
                declared,
                recorded_ids,
                resolved_ids,
            )
            .await?;
            hints.extension_moves = crate::deploy::extension_move_facts(
                conn,
                base,
                declared,
                recorded_ids,
                resolved_ids,
            )
            .await?;
            hints.extension_drops = crate::deploy::extension_drop_facts(
                conn,
                &crate::deploy::retired_extensions(base, declared, recorded_ids, resolved_ids),
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
    match driver {
        Driver::Mssql => pbps_diff::Screen::Catalog,
        Driver::Postgres => pbps_diff::Screen::Text,
    }
}

/// Asks the resolver's scratch to compare the expressions a plan retains,
/// which the engine accepts by parse tree, and records the answers in `hints`.
///
/// Only PostgreSQL retains expressions this way; SQL Server leaves `hints`
/// as they are. Without a scratch entry there is nobody to ask, and a
/// database that cannot be asked either is left unanswered.
pub(crate) async fn retained_expression_hints(
    scratch: Option<&mut crate::resolution::Entry<'_>>,
    conn: &mut Conn,
    target: &crate::db::Target,
    before: pbps_diff::Side<'_>,
    after: pbps_diff::Side<'_>,
    read_back: &Schema,
    hints: &mut pbps_model::Hints,
) -> anyhow::Result<()> {
    match conn.driver() {
        Driver::Postgres => {
            if let Some(entry) = scratch {
                let requests = pbps_diff::retained::candidates(before, after, hints);
                if !requests.is_empty() && pbps_pg::retained::available(conn, &requests).await {
                    // Paired binding carries the resolver lifecycle. Keep its future
                    // separate from the connected planner's other binding phases.
                    hints.retained_expressions = Box::pin(crate::resolution::retained::compare(
                        entry, conn, target, before, after, read_back, hints, &requests,
                    ))
                    .await?;
                }
            }
            Ok(())
        }
        Driver::Mssql => Ok(()),
    }
}

/// Refuses a plan that creates a name the database already uses outside the
/// recorded scope (#316, #320, #323, DEC-316.1).
///
/// PostgreSQL has one namespace this check reads. SQL Server's is asked after
/// the passes of the connected plan instead (#1077), so here it accepts.
pub(crate) fn refuse_occupied_relation_names(
    driver: Driver,
    cs: &ChangeSet,
    scoped: &pbps_diff::Scoped,
    unreadable: &[(pbps_db::catalog::LimitationTarget, String)],
    unmanaged_relations: &[TableName],
    label: &str,
) -> anyhow::Result<()> {
    match driver {
        Driver::Postgres => {
            crate::deploy::refuse_occupied_names(cs, scoped, unreadable, unmanaged_relations, label)
        }
        Driver::Mssql => Ok(()),
    }
}

/// The modules a plan must rebuild around the enums it rebuilds and does not
/// yet, found in the planning transaction (DEC-1635.7).
///
/// What stands on a rebuilt enum comes down and goes back through the module
/// rebuild, and anything else is refused by name. Only PostgreSQL has enums
/// to rebuild; SQL Server has no module to add.
pub(crate) async fn enum_rebuild_dependents(
    conn: &mut Conn,
    changes: &ChangeSet,
    declared: Option<&Schema>,
    before: &IdsFile,
) -> anyhow::Result<BTreeSet<pbps_model::ModuleId>> {
    match conn.driver() {
        Driver::Postgres => {
            if !changes
                .changes
                .iter()
                .any(|p| matches!(p.change, pbps_model::Change::EnumRebuild { .. }))
            {
                return Ok(BTreeSet::new());
            }
            let census = pbps_pg::type_rebuild::census(conn, changes, declared, before, false)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(census.rebuild)
        }
        Driver::Mssql => Ok(BTreeSet::new()),
    }
}

/// Binds the expressions a plan gives a table, to learn which routines the
/// review admits (DEC-2563.4), and hands `on_note` each note about one that
/// cannot be bound yet because it uses an object the plan creates.
///
/// PostgreSQL binds them in the planning transaction; SQL Server does not
/// bind table expressions this way.
pub(crate) async fn table_expression_notes(
    conn: &mut Conn,
    admission: &pbps_pg::admission::Admission,
    changes: &[pbps_model::PlannedChange],
    mut on_note: impl FnMut(String),
) {
    match conn.driver() {
        Driver::Postgres => {
            for planned in changes {
                let Ok(pbps_pg::table_expressions::Vetted::Unjudged(notes)) =
                    vet_table_expressions(
                        conn,
                        admission,
                        pbps_pg::pins::Within::OwnTransaction,
                        pbps_pg::table_expressions::Moment::Early,
                        &planned.change,
                    )
                    .await
                else {
                    continue;
                };
                for note in notes {
                    on_note(note);
                }
            }
        }
        Driver::Mssql => {}
    }
}

/// Whether the rows an enum rebuild would strand are counted at plan time.
///
/// PostgreSQL counts the rows that hold a label an enum rebuild removes, or
/// that a domain's narrower base cannot hold, by the same probes apply runs,
/// so the plan is refused by name and with a count before anything is written
/// (DEC-1635.7, DEC-1635.9). SQL Server has neither enums nor domains.
pub(crate) fn counts_rows_an_enum_retype_strands(driver: Driver, changes: &ChangeSet) -> bool {
    match driver {
        Driver::Postgres => changes.changes.iter().any(|p| {
            matches!(&p.change, pbps_model::Change::EnumRebuild {
                step: pbps_model::EnumRebuildStep::Retype { values, .. }, ..
            } if match values.as_ref() {
                pbps_model::ValueChange::Cast { from, to, .. } => from != to,
                pbps_model::ValueChange::Relabel { removed } => !removed.is_empty(),
            })
        }),
        Driver::Mssql => false,
    }
}

/// Whether planning vets declared domain `CHECK` and `DEFAULT` code on the
/// resolver's scratch before the plan is sealed.
///
/// PostgreSQL has domains and does; SQL Server has none.
pub(crate) fn vets_declared_domain_code(driver: Driver) -> bool {
    match driver {
        Driver::Postgres => true,
        Driver::Mssql => false,
    }
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
    match driver {
        Driver::Postgres => Ok(()),
        Driver::Mssql => anyhow::bail!(
            "`pbps resolver externals` reads a PostgreSQL target; SQL Server has no binding \
             adapter yet (#619, #620)"
        ),
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
    match driver {
        Driver::Mssql => true,
        Driver::Postgres => false,
    }
}

/// How a target's refused or failed read of its engine lineage identity is
/// reported.
///
/// PostgreSQL's refusal is the deployment role's to fix and names the grant;
/// SQL Server's `family_guid` read has no such remedy and is unanswerable.
pub(crate) fn target_identity_unread(driver: Driver, error: DbError) -> crate::resolution::Refused {
    match driver {
        Driver::Postgres => crate::resolution::identity_unread(error),
        Driver::Mssql => crate::resolution::Refused::Unanswerable(anyhow::anyhow!(
            "the target's master family_guid could not be read: {error}"
        )),
    }
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
    match (driver, profile) {
        (Driver::Mssql, profile) => {
            let choice = match profile {
                ResolverProfile::Server { same_instance, .. } => same_instance.unwrap_or_default(),
                ResolverProfile::Docker { .. } => pbps_config::resolver::SameInstance::Refuse,
            };
            match choice {
                pbps_config::resolver::SameInstance::Refuse => {
                    pbps_model::resolver::SameInstance::Refused
                }
                pbps_config::resolver::SameInstance::Allow => {
                    pbps_model::resolver::SameInstance::Allowed
                }
            }
        }
        (Driver::Postgres, _) => pbps_model::resolver::SameInstance::NotApplicable,
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
