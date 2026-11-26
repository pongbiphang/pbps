//! SQLite's apply session (#2950).
//!
//! Every required method of [`ApplySession`] falls into one of three kinds,
//! named in its doc comment:
//! - *does not exist on this engine* (no roles, routines, user-defined types,
//!   extensions, partitions, temporal tables, or row security): returns the
//!   true constant for SQLite plans, and refuses a non-empty foreign request
//!   where the input can carry one;
//! - *exists in `pbps-sqlite` or `pbps-db` already*: calls it in place on the
//!   calling thread through `Conn::sqlite()` (DEC-2950.17);
//! - *needs logic nobody has written yet* (`requirement`):
//!   refuses by name naming SQLite and the unsupported operation, never
//!   returning an empty answer that would read as "nothing blocks" or
//!   "nothing depends".

use super::*;
use crate::engine::plan_session::sqlite::{sqlite_conn, sqlite_has_no_roles};
use pbps_model::Change;
use pbps_sqlite::dialect::Sqlite;

fn has_change(changes: &ChangeSet, pred: impl Fn(&Change) -> bool) -> bool {
    changes.changes.iter().any(|p| pred(&p.change))
}

fn refuse_if(cond: bool, why: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!cond, "{why}");
    Ok(())
}

fn has_partition_rebuild(changes: &ChangeSet) -> bool {
    has_change(changes, |c| {
        matches!(
            c,
            Change::Partition(pbps_model::PartitionChange::RebuildPartitionedTable(_))
        )
    })
}

impl ApplySession for Sqlite {
    type State = Transaction;
    /// Trivial state accessor: returns the neutral transaction evidence.
    fn state<'s>(&self, state: &'s mut Self::State) -> &'s mut Transaction {
        state
    }
    /// Trivial state accessor: returns the neutral transaction evidence.
    fn evidence<'s>(&self, state: &'s Self::State) -> &'s Transaction {
        state
    }
    /// Does not exist on this engine: SQLite compares expressions as text and
    /// holds no parse-tree retained-expression comparator.
    fn retained_comparison<'s>(
        &self,
        _state: &'s Self::State,
    ) -> Option<&'s dyn pbps_dialect::RetainedComparison> {
        None
    }
    /// Does not exist on this engine (partitioned-table rebuilds) plus
    /// SQLite's staged-view-edit refusal: delegates to the helper shared with
    /// [`PlanSession::require_transactional_rebuilds`].
    fn rebuild_mode(&self, changes: &ChangeSet, staged: bool) -> anyhow::Result<()> {
        plan_session::sqlite::require_transactional_rebuilds(changes, staged)
    }
    /// Exists in `pbps-db` already: builds `RenameTarget::Table` and
    /// `RenameTarget::Column` in first-occurrence order using stored names
    /// (`At::Start`). Dropped views belong to [`Self::check_module_dependency_facts`].
    fn rename_targets(&self, names: &pbps_model::plan_names::PlanNames<'_>) -> Vec<RenameTarget> {
        pbps_db::impact::rename_targets(names, false)
    }
    /// Does not exist on this engine in v0: `pbps_sqlite::validate` refuses
    /// column collations as not built yet, and SQLite has no database-level
    /// default column collation to compare against.
    async fn baseline_collation(&self, conn: &mut Conn) -> Result<Option<String>, DbError> {
        let _ = sqlite_conn(conn)?;
        Ok(None)
    }
    /// Needs logic nobody has written yet: `pbps-sqlite` evaluates drop
    /// requirements at apply time, but has no binder that extracts which
    /// columns a predicate reads. Refused by name rather than answered empty.
    async fn requirement(
        &self,
        _conn: &mut Conn,
        _admission: &RoutineAdmission,
        _table: &TableName,
        _predicate: &str,
    ) -> anyhow::Result<BTreeSet<pbps_model::ColumnName>> {
        anyhow::bail!(
            "SQLite does not support binding drop-requirement predicates to their referenced \
             columns yet"
        )
    }
    /// Exists in `pbps-sqlite` already: evaluates the statement's drop guard
    /// in place via `pbps_sqlite::drop_requirements::check`.
    async fn check_drop_requirement(
        &self,
        conn: &mut Conn,
        _admission: &RoutineAdmission,
        statement: &pbps_dialect::Statement,
    ) -> Result<(), DbError> {
        let c = sqlite_conn(conn)?;
        let Some(guard) = &statement.drop_guard else {
            return Ok(());
        };
        pbps_sqlite::drop_requirements::check(c, guard).map_err(DbError::Refused)
    }
    /// Does not exist on this engine: SQLite has no stored routines that a
    /// table expression can call, so no table expression is left unjudged.
    async fn table_sites(
        &self,
        _conn: &mut Conn,
        _admission: &RoutineAdmission,
        _changes: &[pbps_model::PlannedChange],
    ) -> bool {
        false
    }
    /// Does not exist on this engine: SQLite declares no user-defined types,
    /// so no declared-type pins exist to replay (refused if a plan carries any).
    async fn declared_type_replay(
        &self,
        _conn: &mut Conn,
        approved: &Approved<'_>,
        _at: Check<'_>,
    ) -> anyhow::Result<()> {
        refuse_if(
            !approved.plan.declared_type_pins.is_empty()
                || !approved.managed.declared_types.is_empty(),
            "SQLite declares no types, so a plan carrying declared-type pins was not built for it",
        )
    }
    /// Does not exist on this engine (signature has no "no routines" value,
    /// only a refusal): SQLite has no stored routines and pins none.
    async fn selected_rows(
        &self,
        _conn: &mut Conn,
        _at: Check<'_>,
        _roots: &[i64],
    ) -> Result<(pbps_dialect::routines::Read, Vec<pbps_model::TypeRoutine>), DbError> {
        Err(DbError::Refused(
            "SQLite has no stored routines and pins none, so there is none to read for a plan's \
             type input paths"
                .to_owned(),
        ))
    }
    /// Does not exist on this engine: SQLite has no stored routines and pins none.
    async fn replaceable_rows(
        &self,
        conn: &mut Conn,
        _at: Check<'_>,
    ) -> Result<Option<pbps_dialect::routines::Read>, DbError> {
        let _ = sqlite_conn(conn)?;
        Ok(None)
    }
    /// Does not exist on this engine: SQLite has no partitions and never emits
    /// a staged attach's temporary `CHECK` validation step.
    async fn recover_failed_validation(
        &self,
        _conn: &mut Conn,
        _statement: &pbps_dialect::Statement,
    ) -> Option<String> {
        None
    }
    /// Does not exist on this engine: SQLite declares no user-defined types,
    /// so there are no physical type defaults to capture.
    async fn capture_physical_defaults(
        &self,
        _state: &mut Self::State,
        _conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
        _owner: pbps_db::TransactionOwner,
    ) -> anyhow::Result<()> {
        refuse_if(
            !plan.declared_type_pins.is_empty(),
            "SQLite declares no types, so a plan carrying declared-type pins was not built for it",
        )
    }
    /// Does not exist on this engine: SQLite has no stored routines that a
    /// table expression can call, so no statement introduces one to vet.
    async fn vet_introduced(
        &self,
        _conn: &mut Conn,
        _admission: &RoutineAdmission,
        _change: &Change,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    /// Does not exist on this engine: SQLite declares no types, so no
    /// statement of its own carries a `DeclaredTypeStatement` (only called
    /// when one is present, so refuse by name).
    async fn vet_declared_type(
        &self,
        _conn: &mut Conn,
        _statement: &pbps_dialect::DeclaredTypeStatement,
        _admission: &RoutineAdmission,
        _pins: &[pbps_model::DeclaredTypePin],
        _changes: &ChangeSet,
        _executed: &[pbps_dialect::Statement],
    ) -> anyhow::Result<()> {
        anyhow::bail!(
            "SQLite declares no types, so no statement of its own leaves a declared type to vet"
        )
    }
    /// Does not exist on this engine: retained parse-tree expressions are a
    /// PostgreSQL feature; refused if a plan carries any.
    async fn hold_retained(
        &self,
        _state: &mut Self::State,
        _conn: &mut Conn,
        changes: &ChangeSet,
        _authored: Option<&Schema>,
    ) -> anyhow::Result<()> {
        refuse_if(
            pbps_model::retained::expressions(changes).next().is_some(),
            "retained expressions are not supported on SQLite",
        )
    }
    /// Does not exist on this engine: no retained expressions are held.
    async fn verify_retained(
        &self,
        _state: &mut Self::State,
        _conn: &mut Conn,
        _after: &Schema,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    /// Exists in `pbps-sqlite` already: SQLite's catalog hides no table from a
    /// connection, so when `missing` is non-empty we verify via
    /// `pbps_sqlite::catalog` (matching whether a transaction is open) that
    /// none of `missing` is present in `main`. SQLite has no roles, so a
    /// non-empty `missing_roles` is refused rather than answered empty.
    async fn recording_absence(
        &self,
        conn: &mut Conn,
        missing: &[TableName],
        missing_roles: &[pbps_model::RoleName],
    ) -> anyhow::Result<Vec<pbps_model::RoleName>> {
        let c = sqlite_conn(conn)?;
        if !missing.is_empty() {
            let pulled = if c.in_transaction() {
                pbps_sqlite::catalog::introspect_within_transaction(c)?
            } else {
                pbps_sqlite::catalog::introspect(c)?
            };
            let present: Vec<String> = missing
                .iter()
                .filter(|w| {
                    pulled.schema.tables.keys().any(|f| {
                        pbps_sqlite::ident::same(&w.schema, &f.schema)
                            && pbps_sqlite::ident::same(&w.name, &f.name)
                    })
                })
                .map(|t| t.shown().to_string())
                .collect();
            if !present.is_empty() {
                anyhow::bail!(
                    "cannot record table(s) {} as missing: SQLite's catalog still holds them",
                    present.join(", ")
                );
            }
        }
        refuse_if(
            !missing_roles.is_empty(),
            "SQLite has no roles, so missing roles cannot be recorded",
        )?;
        Ok(Vec::new())
    }
    /// Does not exist on this engine: SQLite has no partitions and writes no
    /// staged-attach temporary `CHECK`.
    async fn standing_scan_checks(
        &self,
        conn: &mut Conn,
        _ids: &IdsFile,
    ) -> anyhow::Result<Vec<pbps_dialect::ScanCheck>> {
        let _ = sqlite_conn(conn)?;
        Ok(Vec::new())
    }
    /// Does not exist on this engine: SQLite writes no staged-attach temporary
    /// `CHECK`, so a non-empty list is refused by name.
    async fn drop_scan_checks(
        &self,
        _conn: &mut Conn,
        checks: &[pbps_dialect::ScanCheck],
    ) -> anyhow::Result<()> {
        refuse_if(
            !checks.is_empty(),
            "SQLite writes no temporary CHECK for a staged attach, so there is none to drop",
        )
    }
    /// Does not exist on this engine: SQLite has no partitions and records no
    /// baseline handover catalog evidence.
    async fn handover_catalog(
        &self,
        conn: &mut Conn,
    ) -> Result<Option<pbps_model::baseline_evidence::BaselineCatalog>, DbError> {
        let _ = sqlite_conn(conn)?;
        Ok(None)
    }
    /// Does not exist on this engine: SQLite has no roles, declared types,
    /// operator classes or stored routines; refuses if any are requested.
    async fn seal_prerequisites(
        &self,
        _conn: &mut Conn,
        declared: &Schema,
        _changes: &ChangeSet,
        trusted: &BTreeSet<pbps_model::RoleName>,
        pin_routines: bool,
    ) -> anyhow::Result<SealedPrerequisites> {
        plan_session::defaults::type_review("SQLite", trusted, pin_routines)?;
        refuse_if(
            !declared.types.is_empty(),
            "SQLite does not support declared types",
        )?;
        Ok(SealedPrerequisites {
            pins: Prerequisites::default(),
            input_roots: BTreeSet::new(),
        })
    }
    /// Does not exist on this engine: SQLite has no type, provider or
    /// trusted-role prerequisites; refuses if any are present.
    async fn recheck_prerequisites(
        &self,
        _conn: &mut Conn,
        expected: &Prerequisites,
        _changes: &ChangeSet,
        trusted: &BTreeSet<pbps_model::RoleName>,
        _within: pbps_db::TransactionOwner,
    ) -> anyhow::Result<()> {
        refuse_if(
            !expected.types.is_empty() || !expected.providers.is_empty() || !trusted.is_empty(),
            "SQLite does not support type, provider or trusted-role prerequisites",
        )
    }
    /// Does not exist on this engine: SQLite has no permissions, grants or
    /// row-security policies; refuses if a plan carries any.
    async fn permission_support(
        &self,
        _conn: &mut Conn,
        changes: &ChangeSet,
        ids: &IdsFile,
    ) -> anyhow::Result<()> {
        let has_perm = has_change(changes, |c| {
            matches!(c, Change::Permission(_) | Change::Policy(_))
        });
        refuse_if(
            has_perm || !crate::row_security::standing_owners(changes, ids)?.is_empty(),
            "SQLite has no permissions, grants or row-security policies",
        )
    }
    /// Does not exist on this engine: SQLite has no system-versioned tables;
    /// refuses if a plan asks about any.
    async fn refuse_unsupported_temporal(
        &self,
        _conn: &mut Conn,
        changes: &ChangeSet,
    ) -> anyhow::Result<()> {
        plan_session::sqlite::refuse_system_versioned(changes)
    }
    /// Does not exist on this engine: SQLite has no roles, object owners or
    /// grants; refuses if a plan grants anything.
    fn owned_targets(
        &self,
        changes: &ChangeSet,
        _ids: &IdsFile,
        _owners: &BTreeMap<pbps_model::GrantTarget, pbps_model::RoleName>,
        _session_role: &pbps_model::RoleName,
    ) -> anyhow::Result<()> {
        let grants = has_change(changes, |c| {
            matches!(c, Change::Permission(pbps_model::GrantChange::Grant { .. }))
        });
        refuse_if(grants, "SQLite has no roles or grants")
    }
    /// Does not exist on this engine: SQLite has no grants or revokes; refuses
    /// if any are present.
    fn unrevocable_grants(
        &self,
        changes: &ChangeSet,
        _ids: &IdsFile,
        unrevocable: &[pbps_db::catalog::Unrevocable],
    ) -> anyhow::Result<()> {
        let revokes = has_change(changes, |c| {
            matches!(
                c,
                Change::Permission(pbps_model::GrantChange::Revoke { .. })
            )
        });
        refuse_if(
            !unrevocable.is_empty() || revokes,
            "SQLite has no grants or revokes",
        )
    }
    /// Does not exist on this engine: SQLite has no cluster roles; refuses if
    /// any are named.
    async fn absent_cluster_roles(
        &self,
        _conn: &mut Conn,
        named: &BTreeSet<pbps_model::RoleName>,
    ) -> anyhow::Result<Vec<pbps_model::RoleName>> {
        refuse_if(!named.is_empty(), "SQLite has no cluster roles")?;
        Ok(Vec::new())
    }
    /// Does not exist on this engine: SQLite has no cluster roles; refuses if
    /// any are missing.
    fn refuse_missing_cluster_roles(&self, missing: &[pbps_model::RoleName]) -> anyhow::Result<()> {
        refuse_if(!missing.is_empty(), "SQLite has no cluster roles")
    }
    /// Does not exist on this engine: SQLite has no stored routines or
    /// `SECURITY DEFINER`; refuses if any routines are handed in.
    async fn refuse_unsafe_definers(
        &self,
        _conn: &mut Conn,
        routines: &[(pbps_model::ModuleId, pbps_model::ModuleKind)],
        _absent: Absent,
    ) -> anyhow::Result<()> {
        refuse_if(
            !routines.is_empty(),
            "SQLite has no stored routines, so SECURITY DEFINER checks do not apply",
        )
    }
    /// Does not exist on this engine: SQLite has no system-versioned tables or
    /// history tables; refuses if any history names are asked about.
    async fn taken_history_names(
        &self,
        _conn: &mut Conn,
        _changes: &ChangeSet,
        names: &[TableName],
    ) -> anyhow::Result<Vec<TakenHistoryName>> {
        refuse_if(
            !names.is_empty(),
            "SQLite has no system-versioned tables or history tables",
        )?;
        Ok(Vec::new())
    }
    /// Does not exist on this engine in v0: generated columns are refused by
    /// `pbps_sqlite::validate` as not built yet and SQLite has no exclusion
    /// constraints; refuses a plan that carries either, and answers
    /// `not_applicable` otherwise (including plain column drops and retypes).
    async fn generation_support(
        &self,
        _conn: &mut Conn,
        changes: &ChangeSet,
        _ids: &IdsFile,
    ) -> anyhow::Result<ConnectedCheck> {
        let generated = has_change(changes, |c| {
            matches!(
                c,
                Change::Column(pbps_model::ColumnChange::AlterColumnExpression { .. })
            ) || matches!(
                c,
                Change::Column(pbps_model::ColumnChange::AddColumn { column, .. })
                    if matches!(column.value_source, pbps_model::ValueSource::Generated(_))
            ) || matches!(
                c,
                Change::Table(pbps_model::TableChange::CreateTable { table, .. })
                    if table
                        .columns
                        .values()
                        .any(|col| matches!(col.value_source, pbps_model::ValueSource::Generated(_)))
            )
        });
        refuse_if(generated, "SQLite does not support generated columns yet")?;
        let exclusion = has_change(changes, |c| {
            matches!(
                c,
                Change::Constraint(pbps_model::ConstraintChange::SetExclusion { .. })
            )
        });
        refuse_if(exclusion, "SQLite does not support exclusion constraints")?;
        Ok(plan_session::defaults::not_applicable(
            "SQLite",
            "generation_support",
            "generated columns are not supported on SQLite yet",
        ))
    }
    /// Does not exist on this engine: SQLite has no cluster roles; answers
    /// `not_applicable` when no role rename is declared and refuses naming
    /// SQLite when one is.
    async fn external_roles(
        &self,
        _conn: &mut Conn,
        recorded: &IdsFile,
        declared: &IdsFile,
    ) -> anyhow::Result<ExternalRoleRenames> {
        plan_session::defaults::external_role_renames("SQLite", recorded, declared)
    }
    /// Exists in `pbps-sqlite` already: checks existing SQLite catalog drop
    /// blockers inside the caller's transaction through the same function as
    /// [`PlanSession::check_final_order`].
    async fn drop_blockers(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
        stored: &Schema,
        stored_ids: &IdsFile,
    ) -> anyhow::Result<()> {
        plan_session::sqlite::check_drop_blockers(
            conn,
            &plan.changes,
            &plan.ids,
            stored,
            stored_ids,
        )
        .map(|_| ())
    }
    /// Exists in `pbps-sqlite` already: delegates to the helper shared with
    /// [`PlanSession::server_limits`] at `Stage::Apply` (adding the
    /// file-writability probe to the connection, library, ledger, floor-note
    /// and `strategy: online` checks).
    async fn preflight_limits(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
    ) -> Result<EditionVerdict, anyhow::Error> {
        plan_session::sqlite::preflight_verdict(
            self,
            conn,
            &plan.changes,
            pbps_sqlite::preflight::Stage::Apply,
        )
    }
    /// Does not exist on this engine: SQLite has no roles; refused rather than
    /// answered empty because callers ask this only for a plan that drops a role.
    async fn role_drop_facts(
        &self,
        _conn: &mut Conn,
    ) -> anyhow::Result<(
        BTreeMap<pbps_model::RoleName, Vec<pbps_model::RoleName>>,
        BTreeMap<pbps_model::RoleName, Vec<String>>,
    )> {
        Err(sqlite_has_no_roles(
            "reading role memberships and owned securables",
        ))
    }
    /// Does not exist on this engine: SQLite has no roles; refused rather than
    /// answered empty because callers ask this only for a plan creating or
    /// renaming roles.
    async fn alike_role_names(
        &self,
        _conn: &mut Conn,
        _names: &[&str],
    ) -> anyhow::Result<Vec<(String, String)>> {
        Err(sqlite_has_no_roles("comparing declared role names"))
    }
    /// Does not exist on this engine: SQLite has no roles; refused rather than
    /// answered empty because callers ask this only for a plan creating or
    /// renaming roles.
    async fn principal_name_holders(
        &self,
        _conn: &mut Conn,
        _names: &[&str],
        _vacated: &[&str],
    ) -> anyhow::Result<Vec<(String, String, String)>> {
        Err(sqlite_has_no_roles("reading which principals hold a name"))
    }
    /// Exists in `pbps-sqlite` already: reads the SQLite catalog inventory and
    /// classifies every referrer of a renamed table or column as carried or
    /// advisory (`blocking` is always empty on SQLite).
    async fn rename_report(
        &self,
        conn: &mut Conn,
        target: &RenameTarget,
    ) -> Result<ImpactReport, ImpactError> {
        let c = sqlite_conn(conn)?;
        pbps_sqlite::catalog::inventory::rename_report(c, target)
    }
    /// Does not exist on this engine: SQLite records no `USING` conversion
    /// expressions; refuses if a change carries one.
    async fn conversions(
        &self,
        _conn: &mut Conn,
        _admission: &RoutineAdmission,
        changes: &[pbps_model::PlannedChange],
    ) -> anyhow::Result<BTreeSet<pbps_model::ColumnRef>> {
        let uses = changes.iter().any(|p| {
            matches!(
                p.change,
                Change::Column(pbps_model::ColumnChange::AlterColumnType { using: Some(_), .. })
            )
        });
        refuse_if(
            uses,
            "SQLite does not support recorded `USING` conversion expressions",
        )?;
        Ok(BTreeSet::new())
    }
    /// Exists in `pbps-sqlite` already: compares schema and table names with
    /// `pbps_sqlite::ident::same` (SQLite's measured ASCII-only case folding).
    async fn removed_name_matches(
        &self,
        conn: &mut Conn,
        wanted: &[TableName],
        observed: &[TableName],
    ) -> anyhow::Result<Vec<TableName>> {
        let _ = sqlite_conn(conn)?;
        Ok(wanted
            .iter()
            .filter(|w| {
                observed.iter().any(|o| {
                    pbps_sqlite::ident::same(&w.schema, &o.schema)
                        && pbps_sqlite::ident::same(&w.name, &o.name)
                })
            })
            .cloned()
            .collect())
    }
    /// Does not exist on this engine: SQLite has no partitioned-table rebuild
    /// window; refuses if a plan carries one.
    async fn rebuild_window(
        &self,
        _conn: &mut Conn,
        _admission: &RoutineAdmission,
        _before: pbps_diff::Side<'_>,
        changes: &ChangeSet,
    ) -> anyhow::Result<()> {
        refuse_if(
            has_partition_rebuild(changes),
            "SQLite does not support partitioned-table rebuilds",
        )
    }
    /// Does not exist on this engine: SQLite has no grants, comments, or
    /// module options for a view drop-and-recreate to lose.
    async fn check_module_rebuilds(
        &self,
        _conn: &mut Conn,
        _changes: &ChangeSet,
        _after: bool,
    ) -> anyhow::Result<ConnectedCheck> {
        Ok(plan_session::defaults::not_applicable(
            "SQLite",
            "before_a_rebuild",
            "SQLite has no grants or module options for a view replacement to lose",
        ))
    }
    /// Does not exist on this engine: SQLite has no enum types; refuses if a
    /// plan carries an enum rebuild.
    async fn check_enum_rebuild_census(
        &self,
        _conn: &mut Conn,
        changes: &ChangeSet,
        _stored: &IdsFile,
        _resuming: bool,
    ) -> anyhow::Result<()> {
        let rebuilds = has_change(changes, |c| {
            matches!(
                c,
                Change::TypeValue(pbps_model::TypeValueChange::EnumRebuild { .. })
            )
        });
        refuse_if(rebuilds, "SQLite has no enum types to rebuild")
    }
    /// Does not exist on this engine: SQLite has no domains; refuses if a plan
    /// tightens one.
    async fn check_domain_alterations(
        &self,
        _conn: &mut Conn,
        changes: &ChangeSet,
        _stored: &IdsFile,
    ) -> anyhow::Result<Option<ConnectedCheck>> {
        refuse_if(tightens_a_domain(changes), "SQLite has no domains to alter")?;
        Ok(None)
    }
    /// Does not exist on this engine: SQLite has no row-security policies;
    /// refuses if a plan carries any policy change.
    async fn refuse_policy_dependents<D: ?Sized>(
        &self,
        _conn: &mut Conn,
        plan: &(dyn pbps_diff::order::PlanView<D> + 'static),
        _ids: &IdsFile,
        _managed: &Schema,
    ) -> anyhow::Result<()> {
        let policies = has_change(pbps_dialect::PlanPayload::changes(plan), |c| {
            matches!(c, Change::Policy(_))
        });
        refuse_if(policies, "SQLite has no row-security policies")
    }
    /// Reads `main.sqlite_schema` inside the apply's open transaction and
    /// checks every view and trigger that may read a view dropped or replaced
    /// by this plan, refusing any reader the plan does not remove first.
    async fn check_module_dependency_facts(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        names: pbps_model::plan_names::PlanNames<'_>,
        stored: &Schema,
        stored_ids: &IdsFile,
    ) -> anyhow::Result<()> {
        let c = sqlite_conn(conn)?;
        if crate::dependents::dropped_modules(changes).is_empty() {
            return Ok(());
        }
        let inv = pbps_sqlite::catalog::inventory::read(c, &[])?;
        let known = plan_session::sqlite::known_module_ids(changes, [stored]);
        let found = plan_session::sqlite::module_dependents_from(&inv, changes, &known)?;
        let partitions = crate::dependents::standing_partitions(stored, changes).at(
            &names,
            pbps_model::plan_names::At::Start,
            pbps_model::plan_names::At::End,
        );
        let sources = pbps_model::physical_sources::PhysicalSources::of(names, stored, stored_ids)?;
        let left = crate::dependents::unaccounted(changes, &found, &partitions, &sources);
        if !left.is_empty() {
            anyhow::bail!(
                "module_dependents (SQLite): the database now holds dependents this plan does not \
                 remove before dropping what they depend on, so dropping them would leave readers \
                 broken or drop attached triggers:\n  {}\n\
                 Plan again, so the plan accounts for them where the approver can see it.",
                left.join("\n  ")
            );
        }
        Ok(())
    }
    /// Does not exist on this engine in v0: managed rows and triggers are both
    /// refused by `pbps_sqlite::validate` as not built yet; refuses if a plan
    /// carries row changes, otherwise returns an empty guard.
    async fn prepare_data_writes(
        &self,
        _conn: &mut Conn,
        _admission: &RoutineAdmission,
        changes: &ChangeSet,
        _baseline: &StateSnapshot,
        _planned_ids: &IdsFile,
    ) -> anyhow::Result<DataWriteGuard> {
        refuse_if(
            has_change(changes, |c| matches!(c, Change::Row(_))),
            "SQLite does not support managed row writes yet",
        )?;
        Ok(DataWriteGuard::default())
    }
    /// Does not exist on this engine in v0: managed row writes are not built
    /// yet; refuses a row-write statement and accepts non-row DDL.
    async fn check_data_write(
        &self,
        _conn: &mut Conn,
        _admission: &RoutineAdmission,
        statement: &pbps_dialect::Statement,
        _guard: &DataWriteGuard,
    ) -> anyhow::Result<()> {
        refuse_if(
            statement.row_write.is_some(),
            "SQLite does not support managed row writes yet",
        )
    }
    /// Does not exist on this engine: SQLite has no PostgreSQL deferred
    /// constraint triggers to settle before closing.
    async fn settle_writes(&self, _conn: &mut Conn) -> anyhow::Result<bool> {
        Ok(false)
    }
    /// Does not exist on this engine: SQLite has no type, provider or
    /// trusted-role pins; refuses if a plan carries any.
    async fn replay_type_pins(
        &self,
        _conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
        trusted: &BTreeSet<pbps_model::RoleName>,
        _within: pbps_db::TransactionOwner,
    ) -> anyhow::Result<()> {
        refuse_if(
            !plan.type_pins.is_empty() || !plan.provider_pins.is_empty() || !trusted.is_empty(),
            "this SQLite plan carries PostgreSQL type prerequisites",
        )
    }
    /// Does not exist on this engine: SQLite has no extensions/packages; empty
    /// requires nothing, non-empty refuses by name.
    async fn extension_prerequisites(
        &self,
        _conn: &mut Conn,
        required: &BTreeMap<pbps_model::ObjectLocalName, pbps_model::Extension>,
    ) -> anyhow::Result<Vec<String>> {
        refuse_if(
            !required.is_empty(),
            "PostgreSQL extension prerequisites are unsupported on SQLite",
        )?;
        Ok(Vec::new())
    }
    /// Does not exist on this engine: SQLite has no type input routines to
    /// review; refuses if a plan asks to pin or carries type pins.
    async fn reviewed_input_roots(
        &self,
        _conn: &mut Conn,
        approved: &Approved<'_>,
        _at: Check<'_>,
    ) -> anyhow::Result<BTreeSet<i64>> {
        refuse_if(
            approved.plan.pin_type_routines || !approved.plan.type_pins.is_empty(),
            "SQLite has no type input routines to review",
        )?;
        Ok(BTreeSet::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::plan_session::sqlite::test_support::{TempDb, now};
    use pbps_model::{
        ColumnChange, DropGuard, DropRequirement, ModuleChange, ModuleId, ModuleKind, PlanBaseline,
        PlanOrigin, PlannedChange, SavedPlan, Strategy, TableChange, Uid, UidKind,
    };

    fn saved(changes: Vec<PlannedChange>, ids: IdsFile) -> SavedPlan {
        SavedPlan::new(
            PlanOrigin::Database,
            "sqlite",
            "2026-01-01T00:00:00Z",
            PlanBaseline {
                description: "test.db".into(),
                checksum: "0".repeat(64),
                database_collation: None,
                time_zone: None,
            },
            ChangeSet { changes },
            ids,
        )
    }

    fn uid(kind: UidKind, name: &str) -> Uid {
        Uid::derived(kind, name, 0)
    }

    #[test]
    fn selected_returns_sqlite_dialect_for_sqlite_driver() {
        let selected = crate::engine::apply_session::selected(Driver::Sqlite);
        assert_eq!(selected.name(), "sqlite");
        // Negative: server drivers still select their own dialects.
        assert_eq!(
            crate::engine::apply_session::selected(Driver::Mssql).name(),
            "mssql"
        );
        assert_eq!(
            crate::engine::apply_session::selected(Driver::Postgres).name(),
            "postgres"
        );
    }

    #[test]
    fn rename_targets_builds_table_and_column_targets_at_their_starting_names() {
        let t_uid = uid(UidKind::Table, "main.old_t");
        let c_uid = uid(UidKind::Column, "main.old_t.old_c");
        let changes = vec![
            PlannedChange::new(Change::Table(TableChange::RenameTable {
                uid: t_uid.clone(),
                from: TableName::new("main", "old_t"),
                to: TableName::new("main", "new_t"),
                defaults: Vec::new(),
            })),
            PlannedChange::new(Change::Column(ColumnChange::RenameColumn {
                uid: c_uid.clone(),
                table: TableName::new("main", "new_t"),
                from: "old_c".into(),
                to: "new_c".into(),
                table_was: Some(TableName::new("main", "old_t")),
            })),
        ];
        let mut ids = IdsFile::default();
        ids.tables.insert(t_uid, TableName::new("main", "new_t"));
        ids.columns
            .insert(c_uid, TableName::new("main", "new_t").column("new_c"));
        let names = pbps_model::plan_names::PlanNames::of(&changes, &ids).expect("names");
        let targets = Sqlite::default().rename_targets(&names);
        assert_eq!(
            targets,
            vec![
                RenameTarget::Table(TableName::new("main", "old_t")),
                RenameTarget::Column(TableName::new("main", "old_t").column("old_c")),
            ]
        );
        // Negative: a view drop produces no rename target here (handled by module dependency facts).
        let drop_view = vec![PlannedChange::new(Change::Module(
            ModuleChange::DropModule {
                id: ModuleId::Named(TableName::new("main", "v")),
                kind: ModuleKind::View,
            },
        ))];
        let empty_ids = IdsFile::default();
        let view_names =
            pbps_model::plan_names::PlanNames::of(&drop_view, &empty_ids).expect("names");
        assert!(Sqlite::default().rename_targets(&view_names).is_empty());
    }

    #[test]
    fn check_drop_requirement_evaluates_real_rows_on_a_sqlite_file() {
        let (_tmp, mut conn) = TempDb::create();
        now(conn.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, active INTEGER NOT NULL)"))
            .expect("table");
        now(conn.execute("INSERT INTO items VALUES (1, 0)")).expect("row");

        let table_uid = uid(UidKind::Table, "main.items");
        let mut requirements = BTreeMap::new();
        requirements.insert(
            table_uid.clone(),
            DropRequirement {
                table_uid: table_uid.clone(),
                predicate: "active = 0".into(),
                required_at: "2026-01-01T00:00:00Z".into(),
                operator: "test".into(),
            },
        );
        let guard = DropGuard {
            table_uid,
            table: TableName::new("main", "items"),
            detach_from: None,
            requirements,
        };
        let mut stmt = pbps_dialect::Statement::new("DROP TABLE \"items\"");
        stmt.drop_guard = Some(guard);

        let admission = RoutineAdmission::default();
        now(Sqlite::default().check_drop_requirement(&mut conn, &admission, &stmt))
            .expect("every row satisfies active = 0");

        // Negative: a violating row refuses removal naming the predicate and count.
        now(conn.execute("INSERT INTO items VALUES (2, 1)")).expect("violating row");
        let refused = now(Sqlite::default().check_drop_requirement(&mut conn, &admission, &stmt))
            .unwrap_err();
        assert!(
            refused.to_string().contains("active = 0") && refused.to_string().contains("1"),
            "{refused}"
        );
    }

    #[test]
    fn recording_absence_accepts_absent_tables_and_refuses_present_ones_or_roles() {
        let (_tmp, mut conn) = TempDb::create();
        now(conn.execute("CREATE TABLE present (id INTEGER PRIMARY KEY)")).expect("table");
        let dialect = Sqlite::default();

        let gone =
            now(dialect.recording_absence(&mut conn, &[TableName::new("main", "gone")], &[]))
                .expect("absent table passes");
        assert!(gone.is_empty());

        // Also works inside an open transaction (as baseline and bootstrap call it).
        now(conn.begin(&pbps_sqlite::TRANSACTION)).expect("begin");
        assert!(
            now(dialect.recording_absence(&mut conn, &[TableName::new("main", "gone")], &[]))
                .expect("absent inside tx")
                .is_empty()
        );
        // Negative: a table still present in SQLite (matched ASCII case-insensitively) is refused.
        let still_there =
            now(dialect.recording_absence(&mut conn, &[TableName::new("main", "PRESENT")], &[]))
                .unwrap_err();
        assert!(
            still_there.to_string().contains("SQLite")
                && still_there.to_string().contains("PRESENT"),
            "{still_there}"
        );
        now(conn.rollback(&pbps_sqlite::TRANSACTION)).expect("rollback");

        // Negative: non-empty missing_roles is refused naming SQLite.
        let roles_err =
            now(dialect.recording_absence(&mut conn, &[], &[pbps_model::RoleName::new("app")]))
                .unwrap_err();
        assert!(roles_err.to_string().contains("SQLite"), "{roles_err}");
    }

    #[test]
    fn removed_name_matches_folds_ascii_case_only() {
        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        let wanted = vec![
            TableName::new("main", "Users"),
            TableName::new("main", "café"),
            TableName::new("main", "other"),
        ];
        let observed = vec![
            TableName::new("MAIN", "users"),
            TableName::new("main", "CAFÉ"),
        ];
        let matched =
            now(dialect.removed_name_matches(&mut conn, &wanted, &observed)).expect("matched");
        assert_eq!(matched, vec![TableName::new("main", "Users")]);
        // Negative: non-ASCII case differs in SQLite (`é` != `É`), so `café` did not match `CAFÉ`.
        assert!(!matched.contains(&TableName::new("main", "café")));
    }

    #[test]
    fn preflight_limits_checks_connection_pragmas_and_refuses_online_strategy() {
        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        let empty = saved(Vec::new(), IdsFile::default());

        let verdict = now(dialect.preflight_limits(&mut conn, &empty)).expect("clean connection");
        assert!(verdict.refused_online.is_empty());
        assert!(verdict.runs.starts_with("SQLite 3."), "{}", verdict.runs);
        // Default floor 3.35.0 is below bundled 3.54.x, so the floor note is populated.
        assert_eq!(verdict.warnings.len(), 1, "{:?}", verdict.warnings);

        // Negative: a bad connection pragma is refused via `pbps_sqlite::preflight::before_apply`.
        now(conn.execute("PRAGMA legacy_alter_table = ON")).expect("set pragma");
        let bad_pragma = now(dialect.preflight_limits(&mut conn, &empty)).unwrap_err();
        assert!(
            bad_pragma.to_string().contains("legacy_alter_table"),
            "{bad_pragma}"
        );
        now(conn.execute("PRAGMA legacy_alter_table = OFF")).expect("restore pragma");

        // Negative: `strategy: online` is refused naming SQLite rather than returned as honoured.
        let mut online_change = PlannedChange::new(Change::Table(TableChange::DropTable {
            uid: uid(UidKind::Table, "main.t"),
            name: TableName::new("main", "t"),
            detach_from: None,
        }));
        online_change.strategy = Strategy { online: true };
        let online_plan = saved(vec![online_change], IdsFile::default());
        let refused = now(dialect.preflight_limits(&mut conn, &online_plan)).unwrap_err();
        let msg = refused.to_string();
        assert!(msg.contains("SQLite") && msg.contains("online"), "{msg}");
    }

    #[test]
    fn unimplemented_step_b_methods_refuse_naming_sqlite_and_what_is_unsupported() {
        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        let admission = RoutineAdmission::default();

        // 1. requirement (only remaining unimplemented Step B method on ApplySession)
        let req =
            now(dialect.requirement(&mut conn, &admission, &TableName::new("main", "t"), "x > 0"))
                .unwrap_err();
        assert!(
            req.to_string().contains("SQLite") && req.to_string().contains("drop-requirement"),
            "{req}"
        );
    }

    #[test]
    fn check_module_dependency_facts_accepts_accounted_plans_and_refuses_post_plan_readers_and_unreadable_catalog()
     {
        let (tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        for sql in [
            "CREATE TABLE items (id INTEGER PRIMARY KEY, qty INTEGER)",
            "CREATE VIEW v AS SELECT id, qty FROM items",
            "CREATE VIEW u AS SELECT qty FROM v",
        ] {
            now(conn.execute(sql)).expect(sql);
        }

        let v_id = ModuleId::Named(TableName::new("main", "v"));
        let u_id = ModuleId::Named(TableName::new("main", "u"));
        let mut stored = Schema::default();
        stored.modules.insert(
            v_id.clone(),
            pbps_model::Module {
                kind: ModuleKind::View,
                description: None,
                definition: "SELECT id, qty FROM items".into(),
                options: Default::default(),
            },
        );
        stored.modules.insert(
            u_id.clone(),
            pbps_model::Module {
                kind: ModuleKind::View,
                description: None,
                definition: "SELECT qty FROM v".into(),
                options: Default::default(),
            },
        );
        let stored_ids = IdsFile::default();

        // 1. Accounted edit: managed reader `u` is dropped before `v` and
        //    restored after `v`. Passes inside BEGIN IMMEDIATE.
        let edit_with_reader = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::Module(ModuleChange::DropModule {
                    id: u_id.clone(),
                    kind: ModuleKind::View,
                })),
                PlannedChange::new(Change::Module(ModuleChange::AlterModule {
                    id: v_id.clone(),
                    module: Box::new(pbps_model::Module {
                        kind: ModuleKind::View,
                        description: None,
                        definition: "SELECT id, qty + 1 AS qty FROM items".into(),
                        options: Default::default(),
                    }),
                })),
                PlannedChange::new(Change::Module(ModuleChange::CreateModule {
                    id: u_id.clone(),
                    module: Box::new(stored.modules[&u_id].clone()),
                })),
            ],
        };
        let check_in_tx = |conn: &mut Conn, cs: &ChangeSet| {
            let names =
                pbps_model::plan_names::PlanNames::of(&cs.changes, &stored_ids).expect("names");
            now(conn.begin(&pbps_sqlite::TRANSACTION)).expect("begin");
            let res =
                now(dialect.check_module_dependency_facts(conn, cs, names, &stored, &stored_ids));
            now(conn.rollback(&pbps_sqlite::TRANSACTION)).expect("rollback");
            res
        };
        check_in_tx(&mut conn, &edit_with_reader).expect("accounted rebuild passes");

        // 2. Accounted drop: `u` dropped before `v`.
        let drop_both = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::Module(ModuleChange::DropModule {
                    id: u_id.clone(),
                    kind: ModuleKind::View,
                })),
                PlannedChange::new(Change::Module(ModuleChange::DropModule {
                    id: v_id.clone(),
                    kind: ModuleKind::View,
                })),
            ],
        };
        check_in_tx(&mut conn, &drop_both).expect("accounted drop passes");

        // Negative: wrong order (dropping `u` after `v`) refuses naming SQLite, `v` and `u`.
        let wrong_order = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::Module(ModuleChange::DropModule {
                    id: v_id.clone(),
                    kind: ModuleKind::View,
                })),
                PlannedChange::new(Change::Module(ModuleChange::DropModule {
                    id: u_id.clone(),
                    kind: ModuleKind::View,
                })),
            ],
        };
        let order_err = check_in_tx(&mut conn, &wrong_order)
            .unwrap_err()
            .to_string();
        assert!(
            order_err.contains("SQLite")
                && order_err.contains("main.v")
                && order_err.contains("main.u"),
            "{order_err}"
        );

        // Negative: a view created after planning reading `v` (or transitively
        // through `u`) refuses apply before any statement runs.
        now(conn.execute("CREATE VIEW late_reader AS SELECT qty FROM u")).expect("late view");
        let late_err = check_in_tx(&mut conn, &edit_with_reader)
            .unwrap_err()
            .to_string();
        assert!(
            late_err.contains("SQLite")
                && late_err.contains("late_reader")
                && (late_err.contains("main.v") || late_err.contains("main.u")),
            "{late_err}"
        );
        now(conn.execute("DROP VIEW late_reader")).expect("cleanup");

        // Negative: a trigger reading `v` created after planning refuses apply.
        now(conn
            .execute("CREATE TRIGGER tr_late AFTER INSERT ON items BEGIN SELECT qty FROM v; END"))
        .expect("late trigger");
        let trig_err = check_in_tx(&mut conn, &edit_with_reader)
            .unwrap_err()
            .to_string();
        assert!(
            trig_err.contains("SQLite")
                && trig_err.contains("tr_late")
                && trig_err.contains("main.v"),
            "{trig_err}"
        );
        now(conn.execute("DROP TRIGGER tr_late")).expect("cleanup");

        // Negative: an INSTEAD OF trigger on `v` created after planning refuses apply.
        now(conn.execute(
            "CREATE TRIGGER tr_instead INSTEAD OF INSERT ON v BEGIN INSERT INTO items(qty) VALUES (NEW.qty); END",
        ))
        .expect("instead trigger");
        let instead_err = check_in_tx(&mut conn, &edit_with_reader)
            .unwrap_err()
            .to_string();
        assert!(
            instead_err.contains("SQLite")
                && instead_err.contains("tr_instead")
                && instead_err.contains("main.v"),
            "{instead_err}"
        );
        now(conn.execute("DROP TRIGGER tr_instead")).expect("cleanup");

        // Negative: an unreadable catalog (exclusive lock held elsewhere) is an
        // error, never "no readers".
        let locker =
            pbps_db::sqlite::Connection::open(tmp.path(), pbps_db::sqlite::OpenMode::ExistingOnly)
                .expect("second connection");
        locker.execute("BEGIN EXCLUSIVE", &[]).expect("lock");
        let names =
            pbps_model::plan_names::PlanNames::of(&drop_both.changes, &stored_ids).expect("names");
        let lock_err = now(dialect.check_module_dependency_facts(
            &mut conn,
            &drop_both,
            names,
            &stored,
            &stored_ids,
        ))
        .expect_err("locked file must error");
        assert!(
            !lock_err.to_string().is_empty(),
            "expected lock error: {lock_err}"
        );
        locker.execute("ROLLBACK", &[]).expect("unlock");
    }

    #[test]
    fn absent_engine_features_return_true_constants_and_refuse_foreign_inputs() {
        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        let admission = RoutineAdmission::default();
        let empty_cs = ChangeSet::default();
        let empty_ids = IdsFile::default();

        assert!(
            now(dialect.baseline_collation(&mut conn))
                .expect("ok")
                .is_none()
        );
        assert!(!now(dialect.table_sites(&mut conn, &admission, &[])));
        assert!(
            now(dialect.replaceable_rows(&mut conn, Check::BeforeProbes))
                .expect("ok")
                .is_none()
        );
        assert!(
            now(dialect.standing_scan_checks(&mut conn, &empty_ids))
                .expect("ok")
                .is_empty()
        );
        assert!(
            now(dialect.handover_catalog(&mut conn))
                .expect("ok")
                .is_none()
        );
        assert!(!now(dialect.settle_writes(&mut conn)).expect("ok"));
        assert!(
            now(dialect.extension_prerequisites(&mut conn, &BTreeMap::new()))
                .expect("ok")
                .is_empty()
        );
        assert_eq!(
            now(dialect.generation_support(&mut conn, &empty_cs, &empty_ids))
                .expect("ok")
                .status,
            "not_applicable"
        );
        assert_eq!(
            now(dialect.check_module_rebuilds(&mut conn, &empty_cs, false))
                .expect("ok")
                .status,
            "not_applicable"
        );
        assert!(
            now(dialect.check_domain_alterations(&mut conn, &empty_cs, &empty_ids))
                .expect("ok")
                .is_none()
        );
        assert!(
            now(dialect.external_roles(&mut conn, &empty_ids, &empty_ids))
                .expect("ok")
                .renames
                .is_empty()
        );

        // Negative: foreign engine features refuse naming SQLite.
        let sel_err = now(dialect.selected_rows(&mut conn, Check::BeforeProbes, &[1])).unwrap_err();
        assert!(
            sel_err.to_string().contains("SQLite") && sel_err.to_string().contains("routines"),
            "{sel_err}"
        );
        for (what, err) in [
            (
                "role_drop_facts",
                now(dialect.role_drop_facts(&mut conn)).unwrap_err(),
            ),
            (
                "alike_role_names",
                now(dialect.alike_role_names(&mut conn, &["a"])).unwrap_err(),
            ),
            (
                "principal_name_holders",
                now(dialect.principal_name_holders(&mut conn, &["a"], &[])).unwrap_err(),
            ),
        ] {
            assert!(
                err.to_string().contains("SQLite") && err.to_string().contains("roles"),
                "{what}: {err}"
            );
        }
        let mut exts = BTreeMap::new();
        exts.insert(
            "citext".into(),
            pbps_model::Extension {
                schema: "public".into(),
                version: "1.6".into(),
            },
        );
        let ext_err = now(dialect.extension_prerequisites(&mut conn, &exts)).unwrap_err();
        assert!(ext_err.to_string().contains("SQLite"), "{ext_err}");

        // Negative: staged view rebuild is refused naming SQLite.
        let alter_view = ChangeSet {
            changes: vec![PlannedChange::new(Change::Module(
                ModuleChange::AlterModule {
                    id: ModuleId::Named(TableName::new("main", "v")),
                    module: Box::new(pbps_model::Module {
                        kind: ModuleKind::View,
                        description: None,
                        definition: "SELECT 1".into(),
                        options: Default::default(),
                    }),
                },
            ))],
        };
        dialect
            .rebuild_mode(&alter_view, false)
            .expect("transactional ok");
        let staged_err = dialect.rebuild_mode(&alter_view, true).unwrap_err();
        assert!(staged_err.to_string().contains("SQLite"), "{staged_err}");
    }

    #[test]
    fn drop_blockers_accepts_cleared_drops_and_refuses_post_plan_blockers_and_unreadable_file() {
        let (tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        for sql in [
            "CREATE TABLE parent (id INTEGER PRIMARY KEY, k INTEGER)",
            "CREATE UNIQUE INDEX uq_k ON parent(k)",
            "CREATE TABLE solo (id INTEGER PRIMARY KEY, extra INTEGER)",
            "CREATE INDEX ix_extra ON solo(extra)",
        ] {
            now(conn.execute(sql)).expect(sql);
        }

        let mut stored = Schema::default();
        let mut stored_ids = IdsFile::default();
        for (name, cols) in [("parent", &["id", "k"][..]), ("solo", &["id", "extra"][..])] {
            let tn = TableName::new("main", name);
            let mut tbl = pbps_model::Table::default();
            for &c in cols {
                tbl.columns.insert(
                    c.into(),
                    pbps_model::Column::new("integer".parse().expect("ty")),
                );
                stored_ids.columns.insert(
                    uid(UidKind::Column, &format!("main.{name}.{c}")),
                    tn.column(c),
                );
            }
            stored.tables.insert(tn.clone(), tbl);
            stored_ids
                .tables
                .insert(uid(UidKind::Table, &format!("main.{name}")), tn);
        }

        let check_in_tx = |conn: &mut Conn, changes: Vec<Change>| {
            let mut plan_ids = stored_ids.clone();
            for ch in &changes {
                if let Change::Table(TableChange::DropTable { uid, name, .. }) = ch {
                    plan_ids.tables.remove(uid);
                    plan_ids.columns.retain(|_, c| c.table != *name);
                } else if let Change::Column(pbps_model::ColumnChange::DropColumn { uid, .. }) = ch
                {
                    plan_ids.columns.remove(uid);
                }
            }
            let plan = saved(
                changes.into_iter().map(PlannedChange::new).collect(),
                plan_ids,
            );
            now(conn.begin(&pbps_sqlite::TRANSACTION)).expect("begin");
            let res = now(dialect.drop_blockers(conn, &plan, &stored, &stored_ids));
            now(conn.rollback(&pbps_sqlite::TRANSACTION)).expect("rollback");
            res
        };

        // 1. No-drop plan and unreferenced drops pass inside BEGIN IMMEDIATE.
        check_in_tx(&mut conn, Vec::new()).expect("empty plan passes");
        check_in_tx(
            &mut conn,
            vec![
                Change::Index(pbps_model::IndexChange::DropIndex {
                    table: TableName::new("main", "solo"),
                    name: "ix_extra".into(),
                }),
                Change::Column(pbps_model::ColumnChange::DropColumn {
                    uid: uid(UidKind::Column, "main.solo.extra"),
                    column: TableName::new("main", "solo").column("extra"),
                }),
                Change::Table(TableChange::DropTable {
                    uid: uid(UidKind::Table, "main.parent"),
                    name: TableName::new("main", "parent"),
                    detach_from: None,
                }),
            ],
        )
        .expect("unreferenced drops pass");

        // 2. Post-plan inbound FK, unmanaged view, and trigger block at apply time naming SQLite and both objects.
        now(conn.execute(
            "CREATE TABLE late_child (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent(id), pk INTEGER REFERENCES parent(k))",
        ))
        .expect("late child");
        now(conn.execute("CREATE VIEW late_v AS SELECT extra FROM solo")).expect("late view");
        now(conn.execute(
            "CREATE TRIGGER late_tr AFTER INSERT ON solo BEGIN SELECT NEW.extra; END",
        ))
        .expect("late trigger");

        let parent_err = check_in_tx(
            &mut conn,
            vec![Change::Table(TableChange::DropTable {
                uid: uid(UidKind::Table, "main.parent"),
                name: TableName::new("main", "parent"),
                detach_from: None,
            })],
        )
        .unwrap_err()
        .to_string();
        assert!(
            parent_err.contains("SQLite")
                && parent_err.contains("main.parent")
                && parent_err.contains("late_child"),
            "{parent_err}"
        );

        let idx_err = check_in_tx(
            &mut conn,
            vec![Change::Index(pbps_model::IndexChange::DropIndex {
                table: TableName::new("main", "parent"),
                name: "uq_k".into(),
            })],
        )
        .unwrap_err()
        .to_string();
        assert!(
            idx_err.contains("SQLite")
                && idx_err.contains("uq_k")
                && idx_err.contains("late_child"),
            "{idx_err}"
        );

        let col_err = check_in_tx(
            &mut conn,
            vec![Change::Column(pbps_model::ColumnChange::DropColumn {
                uid: uid(UidKind::Column, "main.solo.extra"),
                column: TableName::new("main", "solo").column("extra"),
            })],
        )
        .unwrap_err()
        .to_string();
        assert!(
            col_err.contains("SQLite")
                && col_err.contains("main.solo.extra")
                && col_err.contains("late_v")
                && col_err.contains("late_tr"),
            "{col_err}"
        );

        // 3. Unreadable (exclusively locked) file errors, never "no blockers".
        let locker =
            pbps_db::sqlite::Connection::open(tmp.path(), pbps_db::sqlite::OpenMode::ExistingOnly)
                .expect("second conn");
        locker.execute("BEGIN EXCLUSIVE", &[]).expect("lock");
        let plan = saved(
            vec![PlannedChange::new(Change::Index(
                pbps_model::IndexChange::DropIndex {
                    table: TableName::new("main", "solo"),
                    name: "ix_extra".into(),
                },
            ))],
            stored_ids.clone(),
        );
        let lock_err = now(dialect.drop_blockers(&mut conn, &plan, &stored, &stored_ids))
            .expect_err("locked file must error");
        assert!(!lock_err.to_string().is_empty(), "{lock_err}");
        locker.execute("ROLLBACK", &[]).expect("unlock");
    }

    #[test]
    fn rename_report_classifies_referrers_excuses_dropped_views_and_passes_apply_preflight_end_to_end()
     {
        use pbps_model::{ForeignKey, Index, IndexColumn, Module, PrimaryKey};

        let dialect = Sqlite::default();
        let seed_sql = [
            "CREATE TABLE items (id INTEGER PRIMARY KEY, qty INTEGER)",
            "CREATE INDEX ix_items_qty ON items(qty)",
            "CREATE TABLE child (id INTEGER PRIMARY KEY, iid INTEGER, CONSTRAINT fk_child_items FOREIGN KEY (iid) REFERENCES items(id))",
            "CREATE VIEW v_items AS SELECT id, qty FROM items",
        ];
        let build_stored = || -> (Schema, IdsFile) {
            let mut schema = Schema::default();
            let mut ids = IdsFile::default();
            let items_tn = TableName::new("main", "items");
            let child_tn = TableName::new("main", "child");
            let mut items = pbps_model::Table::default();
            for c in ["id", "qty"] {
                items.columns.insert(
                    c.into(),
                    pbps_model::Column::new("integer".parse().expect("ty")),
                );
                ids.columns.insert(
                    uid(UidKind::Column, &format!("main.items.{c}")),
                    items_tn.column(c),
                );
            }
            items.primary_key = Some(PrimaryKey {
                name: None,
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
                filegroup: None,
            });
            items.indexes.insert(
                "ix_items_qty".into(),
                Index {
                    columns: vec![IndexColumn::column("qty")],
                    include: Vec::new(),
                    unique: false,
                    filter: None,
                    method: Default::default(),
                    storage_parameters: Default::default(),
                    filegroup: None,
                },
            );
            let mut child = pbps_model::Table::default();
            for c in ["id", "iid"] {
                child.columns.insert(
                    c.into(),
                    pbps_model::Column::new("integer".parse().expect("ty")),
                );
                ids.columns.insert(
                    uid(UidKind::Column, &format!("main.child.{c}")),
                    child_tn.column(c),
                );
            }
            child.foreign_keys.insert(
                "fk_child_items".into(),
                ForeignKey {
                    columns: vec!["iid".into()],
                    references_table: items_tn.clone(),
                    references_columns: vec!["id".into()],
                    on_delete: Default::default(),
                    on_update: Default::default(),
                },
            );
            schema.tables.insert(items_tn.clone(), items);
            schema.tables.insert(child_tn.clone(), child);
            schema.modules.insert(
                ModuleId::Named(TableName::new("main", "v_items")),
                Module {
                    kind: ModuleKind::View,
                    description: None,
                    definition: "SELECT id, qty FROM items".into(),
                    options: Default::default(),
                },
            );
            ids.tables
                .insert(uid(UidKind::Table, "main.items"), items_tn);
            ids.tables
                .insert(uid(UidKind::Table, "main.child"), child_tn);
            (schema, ids)
        };

        // 1. Direct `ApplySession::rename_report` and locked-file error.
        {
            let (tmp, mut conn) = TempDb::create();
            for sql in seed_sql {
                now(conn.execute(sql)).expect(sql);
            }
            let target = RenameTarget::Table(TableName::new("main", "items"));
            let rep = now(dialect.rename_report(&mut conn, &target)).expect("rename_report");
            assert!(rep.blocking.is_empty(), "{rep:#?}");
            assert!(rep.carried.iter().any(|r| r.name == "view main.v_items"
                && r.removed_with
                    == Some((
                        ModuleKind::View,
                        ModuleId::Named(TableName::new("main", "v_items"))
                    ))));
            assert!(
                rep.carried
                    .iter()
                    .any(|r| r.name == "index main.ix_items_qty on table main.items")
            );
            assert!(
                rep.carried
                    .iter()
                    .any(|r| r.name == "foreign key `fk_child_items` on table `main.child`")
            );
            assert!(
                rep.advisory
                    .iter()
                    .any(|r| r.kind == "index" && r.name == "ix_items_qty")
            );
            assert!(
                rep.advisory
                    .iter()
                    .any(|r| r.kind == "view" && r.name == "main.v_items")
            );

            // Negative: exclusively locked file errors, never empty.
            let locker = pbps_db::sqlite::Connection::open(
                tmp.path(),
                pbps_db::sqlite::OpenMode::ExistingOnly,
            )
            .expect("second conn");
            locker.execute("BEGIN EXCLUSIVE", &[]).expect("lock");
            assert!(now(dialect.rename_report(&mut conn, &target)).is_err());
            locker.execute("ROLLBACK", &[]).expect("unlock");
        }

        // 2. End-to-end `apply::preflight` for rename table, case-only rename, rename column,
        //    and rename table paired with dropping `main.v_items` (verifying `impact_lines`,
        //    `rename_notes`, and `excuse_dropped_referrers` through `Progress`).
        let (stored, stored_ids) = build_stored();
        let project = pbps_config::Project {
            root: std::path::PathBuf::from("."),
            config: pbps_config::Config::parse(
                "dialect: postgres\n",
                std::path::Path::new("pbps.yml"),
            )
            .expect("config"),
        };
        let managed = crate::pins::Managed {
            modules: BTreeSet::new(),
            references: BTreeSet::new(),
            declared_types: BTreeSet::new(),
        };
        let renamed_records_ids = {
            let mut ids = stored_ids.clone();
            ids.tables.insert(
                uid(UidKind::Table, "main.items"),
                TableName::new("main", "records"),
            );
            ids.columns.insert(
                uid(UidKind::Column, "main.items.id"),
                TableName::new("main", "records").column("id"),
            );
            ids.columns.insert(
                uid(UidKind::Column, "main.items.qty"),
                TableName::new("main", "records").column("qty"),
            );
            ids
        };
        let cases: Vec<(&str, Vec<Change>, IdsFile, bool)> = vec![
            (
                "rename table",
                vec![Change::Table(TableChange::RenameTable {
                    uid: uid(UidKind::Table, "main.items"),
                    from: TableName::new("main", "items"),
                    to: TableName::new("main", "records"),
                    defaults: Vec::new(),
                })],
                renamed_records_ids.clone(),
                false,
            ),
            (
                "rename table dropping view",
                vec![
                    Change::Module(ModuleChange::DropModule {
                        id: ModuleId::Named(TableName::new("main", "v_items")),
                        kind: ModuleKind::View,
                    }),
                    Change::Table(TableChange::RenameTable {
                        uid: uid(UidKind::Table, "main.items"),
                        from: TableName::new("main", "items"),
                        to: TableName::new("main", "records"),
                        defaults: Vec::new(),
                    }),
                ],
                renamed_records_ids,
                true,
            ),
            (
                "case-only rename table",
                vec![Change::Table(TableChange::RenameTable {
                    uid: uid(UidKind::Table, "main.items"),
                    from: TableName::new("main", "items"),
                    to: TableName::new("main", "ITEMS"),
                    defaults: Vec::new(),
                })],
                {
                    let mut ids = stored_ids.clone();
                    ids.tables.insert(
                        uid(UidKind::Table, "main.items"),
                        TableName::new("main", "ITEMS"),
                    );
                    ids.columns.insert(
                        uid(UidKind::Column, "main.items.id"),
                        TableName::new("main", "ITEMS").column("id"),
                    );
                    ids.columns.insert(
                        uid(UidKind::Column, "main.items.qty"),
                        TableName::new("main", "ITEMS").column("qty"),
                    );
                    ids
                },
                false,
            ),
            (
                "rename column",
                vec![Change::Column(ColumnChange::RenameColumn {
                    uid: uid(UidKind::Column, "main.items.qty"),
                    table: TableName::new("main", "items"),
                    from: "qty".into(),
                    to: "amount".into(),
                    table_was: None,
                })],
                {
                    let mut ids = stored_ids.clone();
                    ids.columns.insert(
                        uid(UidKind::Column, "main.items.qty"),
                        TableName::new("main", "items").column("amount"),
                    );
                    ids
                },
                false,
            ),
        ];
        for (label, changes, after_ids, drops_view) in cases {
            let (tmp, mut conn) = TempDb::create();
            for sql in seed_sql {
                now(conn.execute(sql)).expect(sql);
            }
            let plan = saved(
                changes.into_iter().map(PlannedChange::new).collect(),
                after_ids,
            );
            let checksum = plan.wire_fingerprint();
            let admitted = pbps_diff::order::admit(
                plan,
                &dialect,
                std::path::Path::new("plan.json"),
                &checksum,
            )
            .unwrap_or_else(|e| panic!("{label}: admit failed: {e}"));
            let names =
                pbps_model::plan_names::PlanNames::of(&admitted.changes.changes, &admitted.ids)
                    .expect("names");
            let targets = dialect.rename_targets(&names);
            assert_eq!(targets.len(), 1, "{label}: {targets:?}");
            let target_cfg = crate::db::Target::test_owned_with_driver(
                pbps_db::Driver::Sqlite,
                tmp.path().to_string_lossy().into_owned(),
                Vec::new(),
            );
            let approved = Approved {
                project: &project,
                target: &target_cfg,
                plan: &admitted,
                managed: &managed,
            };
            let progress = crate::deploy::services::Progress::default();
            now(crate::deploy::preflight(
                &mut conn,
                &dialect,
                &admitted,
                &targets,
                &stored,
                &stored_ids,
                &approved,
                Some(&progress),
            ))
            .unwrap_or_else(|e| panic!("{label}: preflight failed: {e}"));

            let notes: Vec<String> = progress
                .findings
                .borrow()
                .iter()
                .map(|f| f.message.clone())
                .collect();
            assert!(
                notes.iter().any(|l| l.contains("legacy_alter_table")),
                "{label}: {notes:#?}"
            );
            if drops_view {
                assert!(
                    !notes.iter().any(|l| l.contains("v_items")),
                    "{label}: dropped view must be excused from carried and advisory: {notes:#?}"
                );
            } else {
                assert!(
                    notes.iter().any(|l| l.contains("view main.v_items")
                        && l.contains("carried into the new name")),
                    "{label}: {notes:#?}"
                );
            }

            // Emit and execute the plan's statements on the live SQLite file to confirm
            // the rename executes cleanly after preflight passes.
            let stmts = crate::statements(&admitted, &dialect)
                .unwrap_or_else(|e| panic!("{label}: statements failed: {e}"));
            now(conn.begin(&pbps_sqlite::TRANSACTION)).expect("begin");
            for stmt in &stmts {
                now(conn.execute(&stmt.sql))
                    .unwrap_or_else(|e| panic!("{label}: `{}` failed: {e}", stmt.sql));
            }
            now(conn.rollback(&pbps_sqlite::TRANSACTION)).expect("rollback");
        }
    }
}
