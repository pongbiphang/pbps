//! Apply's engine work at its existing positions (DEC-2798.2, DEC-2798.4).
//!
//! Positions keep source order and end wherever neutral code can refuse,
//! print, add a finding or change an input. Reads repeated today stay repeated;
//! a pin reader receives the original Check, never a guessed closing phase.
//! The service selects the concrete dialect with with_plan_dialect! before
//! opening one default state per attempt. Existing connected adapters remain
//! available to planning until its own route moves.
//! Staged apply, snapshot, baseline and bootstrap use the positions.
use super::*;
use crate::pins::{Approved, Check};
use anyhow::Context as _;
use pbps_dialect::routines::RoutineAdmission;

pub(crate) mod extensions;
mod mssql;
mod postgres;

/// SQL execution has different owners; statement count cannot identify them.
#[derive(Clone, Copy)]
pub(crate) enum Execution<'a> {
    Transactional,
    StagedGuardedDdl { previous: &'a Schema },
    StagedRow,
}

/// What a bootstrap's sealing step read of the prerequisites its declarations
/// rely on: the pins to replay, and the roots of the routines a declared type's
/// input path would run. Facts only: whether a root is refused is neutral code's
/// decision, because bootstrap's gate is stronger than planning's, which defers
/// the same roots to review.
pub(crate) struct SealedPrerequisites {
    pub pins: Prerequisites,
    pub input_roots: BTreeSet<i64>,
}

/// A history-table name a plan creates that the database already holds under
/// something else, which the engine would take as the history rather than
/// refuse (#1176).
pub(crate) struct TakenHistoryName {
    /// The system-versioned table that names it.
    pub table: TableName,
    /// The name as the plan spells it.
    pub history: TableName,
    /// What holds it, as the engine's catalog names the kind.
    pub kind: String,
    /// The name as the catalog spells it, which the database's collation may
    /// match to `history` without being equal to it.
    pub name: TableName,
}

/// Evidence lives for this attempt, while closing verification is always fresh.
#[derive(Default)]
pub(crate) struct Transaction {
    pub data_guard: DataWriteGuard,
    pub defaults: BTreeMap<pbps_model::Uid, pbps_model::ManagedType>,
}

pub(crate) struct Opening<'a> {
    pub approved: &'a Approved<'a>,
    pub baseline: &'a StateSnapshot,
    pub before: &'a Schema,
    pub scoped: &'a Schema,
    pub recorded_ids: &'a IdsFile,
    pub original_ids: &'a IdsFile,
}

pub(crate) trait ApplySession: crate::emission::Engine {
    type State: Default;
    fn state<'s>(&self, state: &'s mut Self::State) -> &'s mut Transaction;
    fn evidence<'s>(&self, state: &'s Self::State) -> &'s Transaction;
    fn retained_comparison<'s>(
        &self,
        state: &'s Self::State,
    ) -> Option<&'s dyn pbps_dialect::RetainedComparison>;
    fn rebuild_mode(&self, changes: &ChangeSet, staged: bool) -> anyhow::Result<()>;
    fn rename_targets(&self, names: &pbps_model::plan_names::PlanNames<'_>) -> Vec<RenameTarget>;

    async fn connection_support(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
    ) -> anyhow::Result<()> {
        self.permission_support(conn, &plan.changes, &plan.ids)
            .await?;
        self.generation_support(conn, &plan.changes, &plan.ids)
            .await?;
        Ok(())
    }
    async fn history_before_intent(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        label: &str,
    ) -> anyhow::Result<()> {
        crate::deploy::refuse_taken_history_names(self, conn, changes, label).await?;
        // Nothing binds a saved plan to a server: asked again here (#1502).
        self.refuse_unsupported_temporal(conn, changes).await
    }
    /// The connected database's default collation, where the engine's model has
    /// column collations (#1175); `None` where it has none.
    ///
    /// Required: `None` is the engine's statement that its columns have no
    /// collation to compare, and an answer from an engine that was never asked
    /// would let a declaration be compared as if the database had none.
    async fn baseline_collation(&self, conn: &mut Conn) -> Result<Option<String>, DbError>;
    fn baseline_grants(
        &self,
        plan: &pbps_model::SavedPlan,
        scoped: &pbps_diff::Scoped,
        unrevocable: &[pbps_db::catalog::Unrevocable],
    ) -> anyhow::Result<()> {
        self.owned_targets(
            &plan.changes,
            &plan.ids,
            &scoped.owners,
            &scoped.session_role,
        )?;
        // And whether *this* connection can take back what the plan revokes,
        // which `plan` asked of the connection it planned over (#1057). A plan
        // made as the owner and applied as a least-privilege deployer otherwise
        // passes the baseline and fails at its `REVOKE`, after earlier statements
        // of a staged run are already committed.
        self.unrevocable_grants(&plan.changes, &plan.ids, unrevocable)?;
        Ok(())
    }
    fn rename_notes(&self, driver: Driver, report: &ImpactReport) -> Vec<String> {
        impact_notes(driver, report)
    }
    /// The columns a drop's row requirement reads of `table`, once the engine
    /// has bound and vetted `predicate`. Required: a requirement the engine did
    /// not judge must not read as one that reads no column.
    async fn requirement(
        &self,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        table: &TableName,
        predicate: &str,
    ) -> anyhow::Result<BTreeSet<pbps_model::ColumnName>>;
    /// Holds a statement that carries a drop guard to the requirement the plan
    /// recorded for it, just before it runs. A statement without a guard is
    /// let through. Required, for the reason of [`Self::requirement`].
    async fn check_drop_requirement(
        &self,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        statement: &pbps_dialect::Statement,
    ) -> Result<(), DbError>;
    /// Judges the expressions the plan gives a table before any probe evaluates
    /// them, and answers whether one was left unjudged.
    ///
    /// An expression that is not admitted yet, or cannot be bound before the
    /// plan creates what it uses, is left to the check just before its
    /// statement; the probes that would run it are skipped, not run unjudged.
    ///
    /// Required: `false` is the engine's statement that it leaves none unjudged,
    /// and an answer from an engine that was never asked would run the probes
    /// over an expression nothing judged.
    async fn table_sites(
        &self,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        changes: &[pbps_model::PlannedChange],
    ) -> bool;
    async fn transaction_opened(
        &self,
        state: &mut Self::State,
        conn: &mut Conn,
        plan: &pbps_diff::order::AdmittedPlan<Self>,
        opening: Opening<'_>,
    ) -> anyhow::Result<()> {
        let admission = opening.approved.admission();
        self.rebuild_window(
            conn,
            &admission,
            pbps_diff::Side {
                schema: &opening.baseline.schema,
                ids: opening.recorded_ids,
            },
            &plan.changes,
        )
        .await?;
        self.hold_retained(
            state,
            conn,
            &plan.changes,
            Some(&opening.baseline.declared.overlay(opening.before)),
        )
        .await?;
        self.external_roles(conn, opening.original_ids, &plan.ids)
            .await?;
        self.check_module_dependents(conn, plan, opening.scoped, &plan.ids, opening.recorded_ids)
            .await?;
        self.check_module_rebuilds(conn, &plan.changes, false)
            .await?;
        self.check_enum_rebuild_census(conn, &plan.changes, opening.recorded_ids, false)
            .await?;
        self.check_domain_alterations(conn, &plan.changes, opening.recorded_ids)
            .await?;
        crate::deploy::recheck_extension_creates(conn, plan).await?;
        self.drop_blockers(conn, plan, opening.scoped, opening.recorded_ids)
            .await?;
        self.state(state).data_guard = self
            .prepare_data_writes(
                conn,
                &opening.approved.admission(),
                &plan.changes,
                opening.baseline,
                &plan.ids,
            )
            .await?;
        Ok(())
    }
    async fn after_statements(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
        original_ids: &IdsFile,
    ) -> anyhow::Result<()> {
        self.check_module_rebuilds(conn, &plan.changes, true)
            .await?;
        self.refuse_unsafe_definers(conn, &written_routines(&plan.changes), Absent::Refused)
            .await?;
        self.external_roles(conn, original_ids, &plan.ids).await?;
        Ok(())
    }
    /// The census again, where the plan runs: a dependent that appeared on an
    /// enum after the plan was made is refused by name before the first statement
    /// (DEC-1635.7). Inside the apply's own transaction, after the lock.
    ///
    /// The tightened domains again, where the plan runs: a column of an array or
    /// a composite of the domain that appeared after the plan was made is refused
    /// by name before the first statement (DEC-1635.8).
    async fn staged_census(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
        stored: &IdsFile,
        resuming: bool,
    ) -> anyhow::Result<()> {
        self.check_enum_rebuild_census(conn, &plan.changes, stored, resuming)
            .await?;
        if !resuming {
            self.check_domain_alterations(conn, &plan.changes, stored)
                .await?;
            // Every statement starts from a target that still allows it: a resume
            // only re-asks for the packages whose statement has not run.
            crate::deploy::recheck_extension_creates(conn, plan).await?;
        }
        Ok(())
    }
    /// Checks the declared types a plan relies on against the pins it holds.
    ///
    /// Required: an engine that was never asked must not read as one whose
    /// declared types need no pin.
    async fn declared_type_replay(
        &self,
        conn: &mut Conn,
        approved: &Approved<'_>,
        at: Check<'_>,
    ) -> anyhow::Result<()>;
    async fn external_catalog(
        &self,
        conn: &mut Conn,
        approved: &Approved<'_>,
        at: Check<'_>,
    ) -> anyhow::Result<()> {
        let plan = approved.plan;
        let trusted = &approved.project.config.trusted_roles;
        if &plan.trusted_roles != trusted {
            anyhow::bail!("trusted_roles changed since planning; plan again against the database");
        }
        require_type_pins(&plan.type_pins, &approved.managed.references)?;
        self.replay_type_pins(conn, plan, trusted, at.within())
            .await
    }
    /// Reads the routines `roots` name, with their sources, where `at` says who
    /// owns the transaction the read runs in: the routines a plan's type input
    /// paths would run, to compare with what the plan pinned (DEC-2181.2).
    ///
    /// Required: an engine that pins no routine refuses by name. An empty read
    /// from one never asked would let a pin check pass over routines nothing
    /// read.
    async fn selected_rows(
        &self,
        conn: &mut Conn,
        at: Check<'_>,
        roots: &[i64],
    ) -> Result<(pbps_dialect::routines::Read, Vec<pbps_model::TypeRoutine>), DbError>;
    /// Reads the routines a plan pins against replacement (DEC-319.1), where
    /// `at` says who owns the transaction the read runs in. `None` is the
    /// engine's statement that it pins no routine, which a caller reads as
    /// "nothing to check".
    ///
    /// Required, for the reason of [`Self::baseline_collation`].
    async fn replaceable_rows(
        &self,
        conn: &mut Conn,
        at: Check<'_>,
    ) -> Result<Option<pbps_dialect::routines::Read>, DbError>;
    async fn before_transactional_statement(
        &self,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        stmt: &pbps_dialect::Statement,
        guard: &DataWriteGuard,
        plan: Option<&pbps_model::SavedPlan>,
    ) -> anyhow::Result<()> {
        self.check_data_write(conn, admission, stmt, guard).await?;
        before_statement(self, conn, admission, stmt, plan).await
    }
    #[allow(clippy::too_many_arguments)]
    async fn before_staged_statement(
        &self,
        state: &mut Self::State,
        conn: &mut Conn,
        approved: &Approved<'_>,
        stmt: &pbps_dialect::Statement,
        baseline: &StateSnapshot,
        previous: &Schema,
    ) -> anyhow::Result<()> {
        let plan = approved.plan;
        if stmt.row_write.is_some() {
            // The plan's own admission, so a routine its review pinned is
            // admitted here as it was at the start (DEC-2563.9).
            let admission = approved.admission();
            let guard = self
                .prepare_data_writes(conn, &admission, &plan.changes, baseline, &plan.ids)
                .await?;
            self.check_data_write(conn, &admission, stmt, &guard)
                .await?;
        } else {
            self.hold_retained(
                state,
                conn,
                &plan.changes,
                Some(&baseline.declared.overlay(previous)),
            )
            .await?;
            if let Some(extension) = &stmt.extension {
                crate::deploy::vet_before_extension(conn, extension, Some(plan)).await?;
            }
            if stmt.rewrites.is_some() {
                // The expressions the rewrite re-evaluates, held still by
                // the lock the guard takes, judged by the plan's own
                // admission (DEC-2563.9).
                let admission = approved.admission();
                let guard = self
                    .prepare_data_writes(conn, &admission, &plan.changes, baseline, &plan.ids)
                    .await?;
                self.check_data_write(conn, &admission, stmt, &guard)
                    .await?;
            }
            if let Some(change) = &stmt.introduces {
                // Judged by the plan's own admission before the statement
                // runs, with what the earlier steps created in place.
                let admission = approved.admission();
                self.vet_introduced(conn, &admission, change).await?;
            }
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    async fn after_statement(
        &self,
        state: &mut Self::State,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        stmt: &pbps_dialect::Statement,
        plan: Option<&pbps_model::SavedPlan>,
        changes: &ChangeSet,
        executed: &[pbps_dialect::Statement],
        execution: Execution<'_>,
    ) -> anyhow::Result<()> {
        match execution {
            Execution::StagedRow => {
                // A deferred trigger would otherwise run at COMMIT, after the
                // check below, and a function replaced while an earlier one
                // held the commit would run unchecked. The step commits right
                // after, so settling here refuses nothing the commit would
                // have allowed (473).
                self.settle_writes(conn).await?;
                Ok(())
            }
            Execution::StagedGuardedDdl { previous } => {
                self.verify_retained(state, conn, previous).await?;
                after_statement(self, conn, admission, stmt, plan, changes, executed).await?;
                self.refuse_unsafe_definers(conn, &written_routines(changes), Absent::NotYetCreated)
                    .await
            }
            Execution::Transactional => {
                after_statement(self, conn, admission, stmt, plan, changes, executed).await
            }
        }
    }
    async fn staged_closing(
        &self,
        state: &mut Self::State,
        conn: &mut Conn,
        changes: &ChangeSet,
    ) -> anyhow::Result<()> {
        self.hold_retained(state, conn, changes, None).await?;
        Ok(())
    }
    /// Recover validation after a staged step's caller has rolled its transaction back.
    ///
    /// `None` is the engine's statement that the statement left no validation
    /// to recover. Required: an answer from an engine that was never asked
    /// would leave a failed validation unexplained.
    async fn recover_failed_validation(
        &self,
        conn: &mut Conn,
        statement: &pbps_dialect::Statement,
    ) -> Option<String>;
    /// Reads the physical defaults of the declared types the plan pins into this
    /// attempt's evidence, where `owner` says who owns the transaction the read
    /// runs in.
    ///
    /// Required: whether an engine declares types at all is a fact about it,
    /// and an answer from an engine that was never asked would leave a declared
    /// type's default to be compared as if the database had none.
    async fn capture_physical_defaults(
        &self,
        state: &mut Self::State,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
        owner: pbps_db::TransactionOwner,
    ) -> anyhow::Result<()>;
    /// Judges the code a table expression calls, just before the statement that
    /// introduces it, with what the plan created earlier in place, inside the
    /// caller's transaction (DEC-2563.4). A refusal carries the fix.
    ///
    /// Required: an engine that was never asked must not read as one whose
    /// statements introduce no expression to judge.
    async fn vet_introduced(
        &self,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        change: &pbps_model::Change,
    ) -> anyhow::Result<()>;
    /// Compares the declared type a statement left with what the catalog holds,
    /// inside the transaction that ran it: `pins` are what the plan pinned and
    /// `executed` the statements that ran before it.
    ///
    /// Required, for the reason of [`Self::vet_introduced`].
    async fn vet_declared_type(
        &self,
        conn: &mut Conn,
        statement: &pbps_dialect::DeclaredTypeStatement,
        admission: &RoutineAdmission,
        pins: &[pbps_model::DeclaredTypePin],
        changes: &ChangeSet,
        executed: &[pbps_dialect::Statement],
    ) -> anyhow::Result<()>;
    async fn hold_retained(
        &self,
        state: &mut Self::State,
        conn: &mut Conn,
        changes: &ChangeSet,
        authored: Option<&Schema>,
    ) -> anyhow::Result<()>;
    async fn verify_retained(
        &self,
        state: &mut Self::State,
        conn: &mut Conn,
        after: &Schema,
    ) -> anyhow::Result<()>;
    /// What the database holds, for a recording that takes missing objects out
    /// of the state (#1600, #1606): proves the tables the catalog read did not
    /// return are absent and not hidden, then returns the roles it proves
    /// absent. Neutral code narrows the held identities with the returned
    /// roles afterwards.
    ///
    /// The tables come first, as the recordings asked them: a refusal about a
    /// table is the one an operator reads when both a table and a role are
    /// hidden from the login.
    ///
    /// Required, and not one shared body: the two engines share nothing but the
    /// order. Whether a missing object can be hidden from the login is a fact
    /// about the engine's catalog views, and an answer from an engine that was
    /// never asked would record a hidden table as one that is gone.
    async fn recording_absence(
        &self,
        conn: &mut Conn,
        missing: &[TableName],
        missing_roles: &[pbps_model::RoleName],
    ) -> anyhow::Result<Vec<pbps_model::RoleName>>;
    /// The temporary CHECKs a staged attach left on its tables, which
    /// `baseline` takes out before it takes the database as it stands (#1638,
    /// DEC-1638.1).
    ///
    /// Only under the exact name a table's identity in the project's ids file
    /// gives: never by a prefix. Not from the ledger: `baseline` is the remedy
    /// for an entry this build cannot read (DECISIONS 203), and a run that dies
    /// between the `ADD` and the `VALIDATE`'s checkpoint records nothing, so no
    /// entry's kind or ids can be asked first. No declared constraint can hold
    /// the name, the database cannot be read while a `NOT VALID` one stands, and
    /// a validated one is pbps's own.
    ///
    /// Required: whether an engine's staged plans write such a CHECK is a fact
    /// about the engine, and an empty answer from an engine that was never
    /// asked would let `baseline` read a database with one standing.
    async fn standing_scan_checks(
        &self,
        conn: &mut Conn,
        ids: &IdsFile,
    ) -> anyhow::Result<Vec<pbps_dialect::ScanCheck>>;
    /// Drops `checks`, inside the transaction `baseline` records in (#2127).
    /// Neutral code decides there is something to drop and names each for the
    /// record.
    async fn drop_scan_checks(
        &self,
        conn: &mut Conn,
        checks: &[pbps_dialect::ScanCheck],
    ) -> anyhow::Result<()>;
    /// The opaque catalog evidence `baseline` asks before it hands a held
    /// partition over (DEC-1852.1, ADR-0018): whether the engine records one
    /// at all, and what it read. Neutral code keeps the refusal, and keeps the
    /// retained parent-and-bound refusal when the answer is `None`.
    ///
    /// Required, for the reason of [`Self::standing_scan_checks`]: `None` is
    /// the engine's statement that it records no such evidence.
    async fn handover_catalog(
        &self,
        conn: &mut Conn,
    ) -> Result<Option<pbps_model::baseline_evidence::BaselineCatalog>, DbError>;
    /// Seals the type and provider prerequisites of `declared` and reads the
    /// roots of the routines a declared type's input path would run, as
    /// bootstrap does at its probe and on its execution connection. One
    /// implementation for the sealing and the read, so the two cannot disagree
    /// about what was sealed; the returned facts are bootstrap's to refuse.
    ///
    /// Required: whether an engine has type prerequisites at all is a fact about
    /// it, and an empty answer from an engine that was never asked would let a
    /// bootstrap build over types nothing sealed.
    async fn seal_prerequisites(
        &self,
        conn: &mut Conn,
        declared: &Schema,
        changes: &ChangeSet,
        trusted: &BTreeSet<pbps_model::RoleName>,
        pin_routines: bool,
    ) -> anyhow::Result<SealedPrerequisites>;
    /// Reads the prerequisites again and refuses what changed since `expected`
    /// was sealed: before the transaction begins, when it opens, before every
    /// statement and before the recording. `within` says who owns the
    /// transaction the reads run in.
    ///
    /// Required, for the reason of [`Self::seal_prerequisites`].
    async fn recheck_prerequisites(
        &self,
        conn: &mut Conn,
        expected: &Prerequisites,
        changes: &ChangeSet,
        trusted: &BTreeSet<pbps_model::RoleName>,
        within: pbps_db::TransactionOwner,
    ) -> anyhow::Result<()>;
    /// Refuses a plan the server's version or the deployer's ownership cannot
    /// carry (a permission the server lacks, a row-security table the deployer
    /// does not own), before the first statement. Asked alone by bootstrap and
    /// beside the generation support by a saved plan's connection.
    ///
    /// Required: an engine that was never asked must not read as one that has
    /// nothing to refuse.
    async fn permission_support(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        ids: &IdsFile,
    ) -> anyhow::Result<()>;
    /// Refuses what this server cannot take of a system-versioned table, before
    /// the first statement (#1502). A plan with nothing of the kind asks
    /// nothing. Required, for the reason of [`Self::permission_support`].
    async fn refuse_unsupported_temporal(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
    ) -> anyhow::Result<()>;
    /// Refuses a grant on what the connected role will own (#261), which the
    /// engine writes only as the owner's default entry. Apart from
    /// [`Self::unrevocable_grants`] because bootstrap asks only this one: it
    /// revokes nothing. Required, for the reason of [`Self::permission_support`].
    fn owned_targets(
        &self,
        changes: &ChangeSet,
        ids: &IdsFile,
        owners: &BTreeMap<pbps_model::GrantTarget, pbps_model::RoleName>,
        session_role: &pbps_model::RoleName,
    ) -> anyhow::Result<()>;
    /// Refuses a plan that revokes a grant no `REVOKE` from this connection
    /// could take away (#251). Required, for the reason of
    /// [`Self::permission_support`].
    fn unrevocable_grants(
        &self,
        changes: &ChangeSet,
        ids: &IdsFile,
        unrevocable: &[pbps_db::catalog::Unrevocable],
    ) -> anyhow::Result<()>;
    /// Which of `named` the engine's cluster no longer has, in the order the
    /// names come. The refusal that prints them is
    /// [`Self::refuse_missing_cluster_roles`].
    ///
    /// Required: an engine whose roles belong to a cluster must ask it, and an
    /// empty answer from one never asked would read as "every role exists".
    async fn absent_cluster_roles(
        &self,
        conn: &mut Conn,
        named: &BTreeSet<pbps_model::RoleName>,
    ) -> anyhow::Result<Vec<pbps_model::RoleName>>;
    /// Refuses the cluster roles a bootstrap's plan names that the cluster no
    /// longer has. Bootstrap can build database roles, but a cluster role must
    /// already exist and still exist when its successful snapshot is recorded.
    /// Required: an engine that was never asked must not read as one whose
    /// roles are all the database's own.
    fn refuse_missing_cluster_roles(&self, missing: &[pbps_model::RoleName]) -> anyhow::Result<()>;
    /// Refuses a `SECURITY DEFINER` routine the plan wrote that is not in the
    /// engine's safe form (#322), read back inside the transaction that wrote
    /// it. Apart from the module-rebuild and external-role questions
    /// [`Self::after_statements`] asks beside it, because bootstrap asks only
    /// this one. Required, for the reason of [`Self::permission_support`].
    async fn refuse_unsafe_definers(
        &self,
        conn: &mut Conn,
        routines: &[(pbps_model::ModuleId, pbps_model::ModuleKind)],
        absent: Absent,
    ) -> anyhow::Result<()>;
    /// The history-table names `names`, which `changes` create (#1176), that the
    /// database still holds when the plan creates them: empty when the engine
    /// has no system-versioned table, so no name can be taken. Neutral code
    /// keeps the refusal. Required, so an engine never asked is not read as one
    /// with no occupant.
    async fn taken_history_names(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        names: &[TableName],
    ) -> anyhow::Result<Vec<TakenHistoryName>>;
    /// What a plan over generated columns needs of this server and of the
    /// columns already there, asked before anything is written (DEC-1168.1).
    ///
    /// `SET EXPRESSION` is PostgreSQL 17's; on 16 a plan that changes an
    /// expression is refused by name, never turned into a drop and re-add that
    /// would take the column's dependents and its place with it. And a column a
    /// live generated column is computed from cannot be retyped, nor dropped
    /// without `CASCADE` — measured on 16, 17 and 18 — so a plan that retypes or
    /// drops one is refused here rather than by the engine halfway through. Two
    /// things release it: the same plan dropping the generated column, or an
    /// earlier change in the plan to the generated column's expression whose text
    /// does not name the input (DEC-1316.1).
    ///
    /// Required, for the reason of [`Self::permission_support`].
    async fn generation_support(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        ids: &IdsFile,
    ) -> anyhow::Result<ConnectedCheck>;
    /// Cluster-owned identities move before pbps plans their database grants;
    /// the rename evidence this reads is the engine's. Required: an engine that
    /// was never asked must not read as one with no role to move.
    async fn external_roles(
        &self,
        conn: &mut Conn,
        recorded: &IdsFile,
        declared: &IdsFile,
    ) -> anyhow::Result<ExternalRoleRenames>;
    /// Predicts existing table, column and key `DROP` dependencies in the
    /// caller's transaction. The other engine's rename reader does not answer
    /// this question (issue 254). Required, for the reason of
    /// [`Self::permission_support`].
    async fn drop_blockers(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
        stored: &Schema,
        stored_ids: &IdsFile,
    ) -> anyhow::Result<()>;
    /// What the connected server can take of a saved plan, asked again where the
    /// plan runs: a catalog read refused in the caller's transaction (a failed
    /// read is an error, never "no referencers" or "unpublished"), then the
    /// edition verdict (ADR-0003). Required, for the reason of
    /// [`Self::permission_support`].
    async fn preflight_limits(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
    ) -> Result<EditionVerdict, anyhow::Error>;
    /// The members and the owned securables of every user-defined role, for the
    /// `DROP ROLE` that has to remove them first and that the engine will not
    /// run over an owner (ADR-0005). Required: an engine whose roles are not
    /// this tool's to drop refuses by name, and an empty answer from one never
    /// asked would read as "no member, owns nothing".
    async fn role_drop_facts(
        &self,
        conn: &mut Conn,
    ) -> anyhow::Result<(
        BTreeMap<pbps_model::RoleName, Vec<pbps_model::RoleName>>,
        BTreeMap<pbps_model::RoleName, Vec<String>>,
    )>;
    /// Among `names`, the pairs the database reads as one name (DECISIONS 123).
    /// Required, for the reason of [`Self::role_drop_facts`].
    async fn alike_role_names(
        &self,
        conn: &mut Conn,
        names: &[&str],
    ) -> anyhow::Result<Vec<(String, String)>>;
    /// The database principals holding any of `names`, except the ones this plan
    /// vacates (DECISIONS 118, 119). Required, for the reason of
    /// [`Self::role_drop_facts`].
    async fn principal_name_holders(
        &self,
        conn: &mut Conn,
        names: &[&str],
        vacated: &[&str],
    ) -> anyhow::Result<Vec<(String, String, String)>>;
    /// What a rename touches, read from the engine's own catalog.
    async fn rename_report(
        &self,
        conn: &mut Conn,
        target: &RenameTarget,
    ) -> Result<ImpactReport, ImpactError>;
    /// Binds and vets every recorded conversion's expression, before the
    /// pre-flight evaluates it over the rows or the `ALTER` runs it (#2331,
    /// DEC-2331.1). Nothing to vet on an engine that records none.
    async fn conversions(
        &self,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        changes: &[pbps_model::PlannedChange],
    ) -> anyhow::Result<BTreeSet<pbps_model::ColumnRef>>;
    /// Which requested names occur in an already captured table inventory.
    async fn removed_name_matches(
        &self,
        conn: &mut Conn,
        wanted: &[TableName],
        observed: &[TableName],
    ) -> anyhow::Result<Vec<TableName>>;
    /// Holds the source of each partitioned-table rebuild still and recertifies,
    /// before the first statement, that the shape and the dependency closure the
    /// plan was made over are the ones now there. Required: an engine that was
    /// never asked must not read as one with no window to hold.
    async fn rebuild_window(
        &self,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        before: pbps_diff::Side<'_>,
        changes: &ChangeSet,
    ) -> anyhow::Result<()>;
    /// Called inside the transaction before the `DROP` and after the `CREATE` of
    /// a rebuilt module. Required: an engine that was never asked must not read
    /// as one whose replacement keeps its catalog state.
    async fn check_module_rebuilds(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        after: bool,
    ) -> anyhow::Result<ConnectedCheck>;
    /// Recounts, where the plan runs, the dependents of the enums it rebuilds: a
    /// dependent that appeared on an enum after the plan was made is refused by
    /// name before the first statement (DEC-1635.7). Inside the apply's own
    /// transaction, after the lock.
    ///
    /// Required: an engine that was never asked must not read as one with no enum
    /// to rebuild.
    async fn check_enum_rebuild_census(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        stored: &IdsFile,
        resuming: bool,
    ) -> anyhow::Result<()>;
    /// What a tightened domain validates, and a refusal where the engine would
    /// refuse (DEC-1635.8). `None` when no domain in the plan gains a check or
    /// `NOT NULL`, so a plan without one reports nothing new, and `None` from an
    /// engine with no domain.
    async fn check_domain_alterations(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        stored: &IdsFile,
    ) -> anyhow::Result<Option<ConnectedCheck>>;
    /// The apply's half of #314: the saved plan must still remove, before each
    /// module's drop, everything that depends on that module now. A dependent
    /// created after planning is one the approver never saw; the plan is refused
    /// rather than extended, and planning again shows it.
    ///
    /// `stored` is the database as the plan's baseline found it, which says
    /// which table is whose partition (DEC-1699.1).
    async fn check_module_dependents<D: ?Sized>(
        &self,
        conn: &mut Conn,
        plan: &(dyn pbps_diff::order::PlanView<D> + 'static),
        stored: &Schema,
        ids: &IdsFile,
        stored_ids: &IdsFile,
    ) -> anyhow::Result<()> {
        let changes = pbps_dialect::PlanPayload::changes(plan);
        let names = pbps_model::plan_names::PlanNames::of(&changes.changes, ids)?;
        self.refuse_policy_dependents(conn, plan, ids, stored)
            .await?;
        self.check_module_dependency_facts(conn, changes, names, stored, stored_ids)
            .await
    }
    /// Apply validates the approved order against fresh catalog bindings. It
    /// never schedules a newly discovered dependent into the saved plan.
    /// Required, for the reason of [`Self::check_module_rebuilds`].
    async fn refuse_policy_dependents<D: ?Sized>(
        &self,
        conn: &mut Conn,
        plan: &(dyn pbps_diff::order::PlanView<D> + 'static),
        ids: &IdsFile,
        managed: &Schema,
    ) -> anyhow::Result<()>;
    /// Reads every dependent of every module the plan drops, inside the caller's
    /// transaction, and refuses the ones the plan does not remove first. Apart
    /// from [`Self::refuse_policy_dependents`] because the check reads only
    /// these facts. Required, for the reason of [`Self::check_module_rebuilds`].
    async fn check_module_dependency_facts(
        &self,
        conn: &mut Conn,
        changes: &ChangeSet,
        names: pbps_model::plan_names::PlanNames<'_>,
        stored: &Schema,
        stored_ids: &IdsFile,
    ) -> anyhow::Result<()>;
    /// Authenticates the data-write triggers in the caller's open transaction;
    /// what it holds is evidence for [`Self::check_data_write`]. Required: an
    /// engine that was never asked must not read as one with no trigger.
    async fn prepare_data_writes(
        &self,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        changes: &ChangeSet,
        baseline: &StateSnapshot,
        planned_ids: &IdsFile,
    ) -> anyhow::Result<DataWriteGuard>;
    /// Refuses a row write or a rewrite a trigger the guard did not
    /// authenticate would run. Required, for the reason of
    /// [`Self::prepare_data_writes`].
    async fn check_data_write(
        &self,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        statement: &pbps_dialect::Statement,
        guard: &DataWriteGuard,
    ) -> anyhow::Result<()>;
    /// Whether deferred work was flushed and its effects need a managed-state
    /// check. Required, for the reason of [`Self::prepare_data_writes`].
    async fn settle_writes(&self, conn: &mut Conn) -> anyhow::Result<bool>;
    /// Refuses changed type prerequisites before replay proceeds; neutral code
    /// has already refused a changed `trusted_roles` and a type without a pin.
    /// `within` says who owns the transaction the reads run in. Required: an
    /// engine that was never asked must not read as one whose plan carries no
    /// type prerequisite.
    async fn replay_type_pins(
        &self,
        conn: &mut Conn,
        plan: &pbps_model::SavedPlan,
        trusted: &BTreeSet<pbps_model::RoleName>,
        within: pbps_db::TransactionOwner,
    ) -> anyhow::Result<()>;
    /// The problems with the package state a scratch baseline would assume, one
    /// sentence each (empty when it holds exactly what `required` names). A
    /// scratch baseline may assume only package state the scratch actually
    /// holds. Neutral code refuses with all of them, joined.
    ///
    /// Required: whether an engine has packages at all is a fact about it, and
    /// an empty answer from an engine that was never asked would let a scratch
    /// stand in for a target whose packages it does not hold.
    async fn extension_prerequisites(
        &self,
        conn: &mut Conn,
        required: &BTreeMap<pbps_model::ObjectLocalName, pbps_model::Extension>,
    ) -> anyhow::Result<Vec<String>>;
    /// Reads the potential path of every pinned type, including unchanged
    /// columns, as the roots of the routines a type's input would run. Required,
    /// for the reason of [`Self::replay_type_pins`].
    async fn reviewed_input_roots(
        &self,
        conn: &mut Conn,
        approved: &Approved<'_>,
        at: Check<'_>,
    ) -> anyhow::Result<BTreeSet<i64>>;
}

/// Preserved adapters have only a connection; the apply service already has
/// its concrete dialect and never selects again (DEC-2798.2).
pub(crate) fn selected(driver: Driver) -> crate::emission::SelectedDialect {
    match driver {
        Driver::Mssql => crate::emission::SelectedDialect::Mssql(pbps_mssql::Mssql),
        Driver::Postgres => crate::emission::SelectedDialect::Postgres(pbps_pg::Postgres::new()),
    }
}

// Bootstrap keeps its trusted-only admission, and staged keeps its own hooks.
// These adapters preserve the statement annotations' original reachability.
pub(crate) async fn before_statement<S: ApplySession>(
    session: &S,
    conn: &mut Conn,
    admission: &RoutineAdmission,
    stmt: &pbps_dialect::Statement,
    plan: Option<&pbps_model::SavedPlan>,
) -> anyhow::Result<()> {
    session
        .check_drop_requirement(conn, admission, stmt)
        .await?;
    // The routines a table expression calls are judged just before the statement
    // that introduces it, with what the plan created earlier in place (DEC-2563.4).
    if let Some(change) = &stmt.introduces {
        session.vet_introduced(conn, admission, change).await?;
    }
    // A drop is vetted before it runs: what it would delete is judged while
    // it can still be refused, not after (DEC-2447.8).
    if let Some(extension) = &stmt.extension {
        crate::deploy::vet_before_extension(conn, extension, plan).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn after_statement<S: ApplySession>(
    session: &S,
    conn: &mut Conn,
    admission: &RoutineAdmission,
    stmt: &pbps_dialect::Statement,
    plan: Option<&pbps_model::SavedPlan>,
    changes: &ChangeSet,
    executed: &[pbps_dialect::Statement],
) -> anyhow::Result<()> {
    if let Some(type_statement) = &stmt.declared_type {
        session
            .vet_declared_type(
                conn,
                type_statement,
                admission,
                plan.map_or(&[][..], |p| p.declared_type_pins.as_slice()),
                changes,
                executed,
            )
            .await?;
    }
    if let Some(extension) = &stmt.extension {
        crate::deploy::vet_after_extension(conn, extension, plan).await?;
    }
    Ok(())
}
