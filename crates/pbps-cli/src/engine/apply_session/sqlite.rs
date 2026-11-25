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
//! - *needs logic nobody has written yet* (`requirement`, `drop_blockers`,
//!   `rename_report`, `check_module_dependency_facts`): refuses by name naming
//!   SQLite and the unsupported operation, never returning an empty answer
//!   that would read as "nothing blocks" or "nothing depends".

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
    /// Does not exist on this engine (partitioned-table rebuilds) plus the
    /// refusing staged-module-rebuild default: SQLite has no partitioned-table
    /// rebuilds, and staged view replacement has no catalog-preservation step.
    fn rebuild_mode(&self, changes: &ChangeSet, staged: bool) -> anyhow::Result<()> {
        refuse_if(
            has_partition_rebuild(changes),
            "SQLite does not support partitioned-table rebuilds",
        )?;
        plan_session::defaults::transactional_rebuilds("SQLite", changes, staged)
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
        refuse_if(
            pbps_mssql::temporal::needs_the_question(changes),
            "SQLite does not support system-versioned tables",
        )
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
    /// `pbps_sqlite::validate` as not built yet; refuses if a plan alters a
    /// generated column's expression.
    async fn generation_support(
        &self,
        _conn: &mut Conn,
        changes: &ChangeSet,
        _ids: &IdsFile,
    ) -> anyhow::Result<ConnectedCheck> {
        let alters = has_change(changes, |c| {
            matches!(
                c,
                Change::Column(pbps_model::ColumnChange::AlterColumnExpression { .. })
            )
        });
        refuse_if(alters, "SQLite does not support generated columns yet")?;
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
    /// Needs logic nobody has written yet: `pbps-sqlite` has no catalog
    /// drop-blocker reader for tables, columns, keys or indexes. Passes when
    /// the plan drops or retypes nothing a dependency could block, and refuses
    /// by name whenever it does.
    async fn drop_blockers(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
        _stored: &Schema,
        _stored_ids: &IdsFile,
    ) -> anyhow::Result<()> {
        let _ = sqlite_conn(conn)?;
        plan_session::defaults::drop_blockers("SQLite", &plan.changes).map(|_| ())
    }
    /// Exists in `pbps-sqlite` already: runs `pbps_sqlite::preflight::before_apply`
    /// (connection pragmas, library version against every in-place form, ledger
    /// integrity, file writability probe, and floor notes) in place, and refuses
    /// any change carrying `strategy: online` (SQLite has no online operations).
    async fn preflight_limits(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
    ) -> Result<EditionVerdict, anyhow::Error> {
        let c = sqlite_conn(conn)?;
        let online: Vec<String> = plan
            .changes
            .changes
            .iter()
            .filter(|p| p.strategy.online)
            .map(|p| pbps_model::escape::Escaped(&p.change.subject()).to_string())
            .collect();
        if !online.is_empty() {
            anyhow::bail!(
                "SQLite does not support `strategy: online` (requested for {}); remove \
                 `strategy: online` and plan again",
                online.join(", ")
            );
        }
        let verdict = pbps_sqlite::preflight::before_apply(
            c,
            &plan.changes,
            self.floor(),
            pbps_sqlite::preflight::Stage::Apply,
        )?;
        if !verdict.problems.is_empty() {
            anyhow::bail!("{}", pbps_sqlite::preflight::refusal(&verdict.problems));
        }
        Ok(EditionVerdict {
            refused_online: Vec::new(),
            runs: format!("SQLite {}", c.version()?),
            warnings: verdict.notes,
        })
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
    /// Needs logic nobody has written yet: `pbps-sqlite` has no rename-impact
    /// catalog reader. Refused by name naming SQLite and the target rather
    /// than returning an empty report.
    async fn rename_report(
        &self,
        _conn: &mut Conn,
        target: &RenameTarget,
    ) -> Result<ImpactReport, ImpactError> {
        Err(ImpactError::Query(DbError::Refused(format!(
            "SQLite does not support rename impact analysis yet (cannot inspect referrers of \
             {target})"
        ))))
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
    /// Needs logic nobody has written yet: `pbps-sqlite` has no view-dependents
    /// reader. Passes when the plan drops or replaces no view/module, and
    /// refuses by name whenever it does.
    async fn check_module_dependency_facts(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        _names: pbps_model::plan_names::PlanNames<'_>,
        _stored: &Schema,
        _stored_ids: &IdsFile,
    ) -> anyhow::Result<()> {
        let _ = sqlite_conn(conn)?;
        refuse_if(
            !pbps_diff::order::deployment::dropped_modules(changes).is_empty(),
            "SQLite does not support checking catalog dependents of dropped or replaced views yet",
        )
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

        // 1. requirement
        let req =
            now(dialect.requirement(&mut conn, &admission, &TableName::new("main", "t"), "x > 0"))
                .unwrap_err();
        assert!(
            req.to_string().contains("SQLite") && req.to_string().contains("drop-requirement"),
            "{req}"
        );

        // 2. drop_blockers: passes when nothing is dropped, refuses a DropTable naming SQLite.
        let empty = saved(Vec::new(), IdsFile::default());
        now(dialect.drop_blockers(&mut conn, &empty, &Schema::default(), &IdsFile::default()))
            .expect("nothing dropped");
        let drop_table = saved(
            vec![PlannedChange::new(Change::Table(TableChange::DropTable {
                uid: uid(UidKind::Table, "main.t"),
                name: TableName::new("main", "t"),
                detach_from: None,
            }))],
            IdsFile::default(),
        );
        let drop_err = now(dialect.drop_blockers(
            &mut conn,
            &drop_table,
            &Schema::default(),
            &IdsFile::default(),
        ))
        .unwrap_err();
        assert!(
            drop_err.to_string().contains("SQLite") && drop_err.to_string().contains("depends"),
            "{drop_err}"
        );

        // 3. rename_report
        let target = RenameTarget::Table(TableName::new("main", "t"));
        let report_err = now(dialect.rename_report(&mut conn, &target)).unwrap_err();
        assert!(
            report_err.to_string().contains("SQLite")
                && report_err.to_string().contains("rename impact"),
            "{report_err}"
        );

        // 4. check_module_dependency_facts: passes when no view is dropped, refuses DropModule.
        let empty_ids = IdsFile::default();
        let empty_names = pbps_model::plan_names::PlanNames::of(&[], &empty_ids).expect("names");
        now(dialect.check_module_dependency_facts(
            &mut conn,
            &ChangeSet::default(),
            empty_names,
            &Schema::default(),
            &empty_ids,
        ))
        .expect("no dropped view");
        let drop_view = ChangeSet {
            changes: vec![PlannedChange::new(Change::Module(
                ModuleChange::DropModule {
                    id: ModuleId::Named(TableName::new("main", "v")),
                    kind: ModuleKind::View,
                },
            ))],
        };
        let view_names =
            pbps_model::plan_names::PlanNames::of(&drop_view.changes, &empty_ids).expect("names");
        let mod_err = now(dialect.check_module_dependency_facts(
            &mut conn,
            &drop_view,
            view_names,
            &Schema::default(),
            &empty_ids,
        ))
        .unwrap_err();
        assert!(
            mod_err.to_string().contains("SQLite") && mod_err.to_string().contains("views"),
            "{mod_err}"
        );
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
}
