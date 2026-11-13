//! The engine's steps of a connected plan, reached at named positions (#2798).
//!
//! `plan --db` used to call, one by one, about seventy functions of [`super`]
//! whose bodies `match` on the driver, plus a few engine crates directly. A
//! third engine meant reading the whole pipeline to find where it owed an
//! answer, and the state one step computed for a later step (the type-routine
//! review flag that the misspelling probe reads) was a local of the pipeline
//! named in an engine's terms.
//!
//! [`PlanSession`] is the other shape: the pipeline calls the engine at fixed,
//! named **positions**, and each engine implements those positions. A position
//! is not a phase. The steps of the pipeline do not fall into five generic
//! phases (facts, rewrites, gates, rows) because they interleave: a rewrite of
//! the change set is followed by a gate that reads the positions the rewrite
//! set, a catalog fact can force a second diff that discards an earlier
//! reorder, engine steps also rewrite things that are not the change set (the
//! declared schema, the recorded baseline, the dialect, the diff hints), and
//! the change set changes type when the schedule runs. A position is therefore
//! the place where a cluster of engine steps sits today, called where it sits
//! today. This module holds the positions before the resolver block and the ones
//! inside the planning transaction (the pre-schedule reads, the schedule, the
//! catalog reorders and the final-order checks); the later ones are further
//! methods of the same trait.
//!
//! # Rules every position follows
//!
//! - **The pipeline's order does not change.** Inside a position the steps keep
//!   their source order. Two steps share a position only when the neutral code
//!   between them neither refuses, prints, pushes a finding nor mutates what
//!   either step reads; otherwise they are two positions.
//! - **Static dispatch on the concrete dialect.** The pipeline is instantiated
//!   once per engine by `with_plan_dialect!`, so `dialect` is a `Postgres` or an
//!   `Mssql`, not a trait object. That is what lets a position hand back a
//!   replaced dialect of its own type ([`PlanSession::staged_attach_dialect`])
//!   and keeps the lowering brand of `emission::Engine` out of any erasure.
//! - **State that crosses positions is the engine's.** [`PlanSession::State`]
//!   is created once per plan by the pipeline (`Default`) and handed back to the
//!   positions as an opaque value. A fact that neutral code reads too is not
//!   state: it is the position's return value.
//! - **A default is never a silent lie.** A provided method returns the neutral
//!   value when the plan uses nothing the position is about; it refuses, naming
//!   the engine and the feature, when the plan asks for something the engine
//!   has no step for; and it answers `not_applicable`, naming the engine, where
//!   its output is a `connected_checks` row. A position with no neutral value
//!   (the spelling probe, the schema spellings, the computed-column screen, which
//!   every plan needs) is required, so an engine that lacks it does not compile.
//!   The two engines that exist override every provided method whose behaviour
//!   or wording differs from the default, so the default texts are reachable
//!   only by an engine that does not exist yet.
//!
//! The defaults are free functions in [`defaults`] taking the engine's name, so
//! that they can be tested without an engine: `emission::Engine` is sealed and
//! needs a full `PlanDialect`, and unsealing it for a test stub would let any
//! type reach the lowering brand.
//!
//! # Why a companion of `emission::Engine` and not a supertrait
//!
//! `emission::Engine` is about lowering a certified plan. The positions are
//! about reading a database while planning one; they name CLI types, so they
//! cannot live in `pbps-diff` or `emission` (a library module). Bounding the
//! companion by `emission::Engine` seals it with that trait: only the engines
//! `with_plan_dialect!` can select implement it, and adding a third arm to the
//! macro is a compile error until the new engine has implemented every
//! required position.
//!
//! # Why `async fn` in the trait and not a boxed future
//!
//! The crate's MSRV (1.89) has native `async fn` in traits. The pipeline is
//! monomorphic in each arm of the macro, so the futures are concrete and no
//! `Send` bound or indirection is needed; a boxed future would add one
//! allocation per position and a frame the size of the largest position to the
//! planning future that `plan_db_async` already keeps small (#2661).

use super::*;

pub(super) mod mssql;
pub(super) mod postgres;

/// A `NOT VALID` check of a standing domain, as a position reports it.
///
/// The shape of `pbps_pg::domain_alter::NotValid`, owned by the pipeline's
/// side of the seam so that no engine crate's type appears in `plan_db.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StandingNotValid {
    pub domain: pbps_model::TypeRef,
    pub check: String,
    /// The statement that validates it, for a person to run.
    pub remedy: String,
}

/// The one admission rule of every site that runs code the plan holds
/// (DEC-2563.2), shared by the positions that vet.
///
/// Its type is still defined in `pbps-pg`; the alias is the single name the
/// positions use, so that moving it is a change here and in `pins.rs`, not in
/// every signature. SQL Server's positions ignore it.
pub(crate) type SiteAdmission = pbps_pg::admission::Admission;

/// The members of the roles a plan drops, as the schedule takes them.
pub(crate) type RoleMembers = crate::dependents::schedule::RoleMembers;

/// What the positions inside the planning transaction read of the plan
/// besides its change set: the sides it compares and the facts the pipeline
/// has settled by then. Copied into every position, so that a position names
/// what it reads and nothing else of the pipeline's locals.
#[derive(Clone, Copy)]
pub(crate) struct Frame<'a> {
    /// The target's label, for the refusals that name it.
    pub label: &'a str,
    /// The recorded state against the identities the plan resolved: what the
    /// plan changes from.
    pub recorded: pbps_diff::Side<'a>,
    /// The declarations as written (spelling already settled by the engine's
    /// own read) against the same identities.
    pub written: pbps_diff::Side<'a>,
    /// The declarations overlaid on the read-back, against the recorded
    /// identities: the base the differ compared.
    pub base: pbps_diff::Side<'a>,
    /// The declarations as compared, against the resolved identities.
    pub declared: pbps_diff::Side<'a>,
    /// The hints every diff of this plan was made with.
    pub hints: &'a pbps_model::Hints,
    /// The newest recorded snapshot, role renames applied.
    pub baseline: &'a StateSnapshot,
}

/// What a position inside the transaction reads before the schedule, which
/// neutral code then writes into the plan or plans again around.
#[derive(Debug, Default)]
pub(crate) struct ScheduleReads {
    /// The members of every role the plan drops; `None` when it drops none.
    pub role_members: Option<RoleMembers>,
    /// The modules the plan must rebuild around the enums it rebuilds and
    /// does not yet.
    pub enum_rebuild: BTreeSet<pbps_model::ModuleId>,
}

/// What the catalog reorders settled, for the neutral code after them.
#[derive(Debug, Default)]
pub(crate) struct Ordered {
    /// Whether the engine verified the order it saved (a table rebuild's
    /// verifier ran), which the debug observer then does not repeat.
    pub verified: bool,
    /// The name facts the order was settled from, for the debug observer.
    pub facts: Option<pbps_dialect::slots::CatalogFacts>,
}

/// The rows and the answer of the checks that read the final order.
#[derive(Debug)]
pub(crate) struct FinalOrderChecks {
    pub rebuilds: ConnectedCheck,
    pub drop_blockers: ConnectedCheck,
    pub logical_slots: ConnectedCheck,
    pub numbered_column_fills: ConnectedCheck,
}

/// The engine's steps of a connected plan, at the positions of the pipeline.
///
/// The methods are listed in the order the pipeline calls them. See the module
/// documentation for the rules.
#[allow(async_fn_in_trait)]
pub(crate) trait PlanSession: crate::emission::Engine {
    /// What the engine keeps for itself from one position to a later one in
    /// the same plan. The pipeline creates it empty and passes it back.
    type State: Default;

    /// Compares the declared cross-database dependencies with the target.
    ///
    /// Neutral when none are declared; refuses when some are and the engine
    /// has no step for them.
    async fn compare_dependencies(
        &self,
        _conn: &mut Conn,
        declarations: &pbps_model::dependency::Dependencies,
    ) -> anyhow::Result<pbps_model::dependency::PlannedDependencies> {
        defaults::dependencies(self.name(), declarations)
    }

    /// Seals the type prerequisites of the declarations before anything reads
    /// the target further, and decides whether the routines a type's input
    /// path calls still need review. The decision is the engine's: it is kept
    /// in [`Self::State`] for [`Self::misspelt`].
    ///
    /// Neutral when `trusted_roles` and `pin_type_routines` are unused;
    /// refuses either when used and the engine has no step for it.
    async fn seal_declared_type_pins(
        &self,
        _state: &mut Self::State,
        _conn: &mut Conn,
        _declared: &Schema,
        trusted: &BTreeSet<pbps_model::RoleName>,
        pin_routines: bool,
    ) -> anyhow::Result<()> {
        defaults::type_review(self.name(), trusted, pin_routines).map(|_| ())
    }

    /// Confirms, per external cluster-role rename the declarations make, that
    /// the rename is complete. The pipeline uses the renames; the row is kept
    /// for the position inside the planning transaction that repeats the step.
    ///
    /// Neutral (and `not_applicable`) when no role is renamed; refuses when
    /// one is and the engine has no step for it.
    async fn external_role_renames(
        &self,
        _conn: &mut Conn,
        recorded: &IdsFile,
        declared: &IdsFile,
    ) -> anyhow::Result<ExternalRoleRenames> {
        defaults::external_role_renames(self.name(), recorded, declared)
    }

    /// Asks the engine how it reads every declared value (DECISIONS 101, 106).
    /// Required: every engine reads back what it stores, and there is no
    /// neutral answer to what that is.
    async fn misspelt(
        &self,
        state: &Self::State,
        conn: &mut Conn,
        schema: &Schema,
        at: &CatalogNames,
    ) -> Result<Spellings, RowsError>;

    /// How the database spells each declared schema name (DECISIONS 142).
    /// Required, for the same reason as [`Self::misspelt`].
    async fn schema_spellings(
        &self,
        conn: &mut Conn,
        names: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, Option<String>>, DbError>;

    /// Refuses a declared column collation the target does not have (#1175).
    ///
    /// Neutral when no column declares a collation; refuses when one does and
    /// the engine has no step to ask the server.
    async fn refuse_unknown_collations(
        &self,
        _conn: &mut Conn,
        declared: &Schema,
    ) -> anyhow::Result<()> {
        defaults::unknown_collations(self.name(), declared)
    }

    /// Refuses a foreign key to a declared history under another spelling of
    /// its name, which only the database's collation can join (#1625).
    ///
    /// Neutral when no table is system-versioned; refuses when one is and the
    /// engine has no step to ask the server.
    async fn refuse_history_references_alike(
        &self,
        _conn: &mut Conn,
        declared: &Schema,
    ) -> anyhow::Result<()> {
        defaults::history_references(self.name(), declared)
    }

    /// Reconciles incoming and renamed cluster roles against the read the
    /// baseline checksum was computed from, inserting what it found into
    /// `expected`. Not an `async fn`: it asks no further question of the
    /// database, by design (a second query could approve a role that arrived
    /// after the read).
    ///
    /// `not_applicable` when no role is incoming or renamed; refuses when one
    /// is and the engine has no step to check it exists.
    fn reconcile_cluster_roles(
        &self,
        recorded: &IdsFile,
        queried: &IdsFile,
        renames: &BTreeMap<pbps_model::RoleName, pbps_model::RoleName>,
        _live: &Schema,
        _expected: &mut Schema,
    ) -> anyhow::Result<ConnectedCheck> {
        defaults::cluster_roles(self.name(), recorded, queried, renames)
    }

    /// The dialect a staged plan is made with: the attaching identities
    /// observed under their current names, before the differ chooses their
    /// optional scan CHECK (DEC-2126.1). Returns `Self`, which is why the
    /// positions are statically dispatched.
    ///
    /// Neutral (the dialect unchanged) for an engine that has no attach names.
    async fn staged_attach_dialect(
        &self,
        _conn: &mut Conn,
        _before: pbps_diff::Side<'_>,
        _after: pbps_diff::Side<'_>,
    ) -> anyhow::Result<Self> {
        Ok(self.clone())
    }

    /// Adds to `hints` the facts about the target that planning a type or an
    /// extension change needs and the differ cannot see (DEC-1635.7,
    /// DEC-1635.9, #2447).
    ///
    /// Neutral when the base declares no type and no extension; refuses when
    /// it does and the engine has no step to read their facts.
    async fn type_rebuild_hints(
        &self,
        _conn: &mut Conn,
        base: &Schema,
        declared: &Schema,
        _recorded_ids: &IdsFile,
        _resolved_ids: &IdsFile,
        _hints: &mut pbps_model::Hints,
    ) -> anyhow::Result<()> {
        defaults::rebuild_hints(self.name(), base, declared)
    }

    /// How the differ judges a computed column that stands throughout the plan
    /// (#1460). Required: it applies to every plan, so no value is neutral.
    fn computed_column_screen(&self) -> pbps_diff::Screen;

    /// Asks the resolver's scratch to compare the expressions a plan retains
    /// by parse tree, and records the answers in `hints`.
    ///
    /// Neutral for an engine that compares them as text.
    #[allow(clippy::too_many_arguments)]
    async fn retained_expression_hints(
        &self,
        _scratch: Option<&mut crate::resolution::Entry<'_>>,
        _conn: &mut Conn,
        _target: &crate::db::Target,
        _before: pbps_diff::Side<'_>,
        _after: pbps_diff::Side<'_>,
        _read_back: &Schema,
        _hints: &mut pbps_model::Hints,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// The `NOT VALID` checks of the domains standing in the database. The
    /// pipeline asks only when the base holds a domain.
    ///
    /// Refuses by default: a domain is declared and the engine has no step to
    /// read its checks, which is not "none stand".
    async fn standing_not_valid_domain_checks(
        &self,
        _conn: &mut Conn,
    ) -> anyhow::Result<Vec<StandingNotValid>> {
        defaults::standing_domain_checks(self.name())
    }

    /// Seals the type prerequisites of the ordinary plan, from the base and
    /// the declarations and the first diff, and says whether the routines the
    /// types' input paths call still need review (DEC-2181.2). Neutral code
    /// reads the answer, so it is a return value and not state.
    ///
    /// Neutral (`false`) when `trusted_roles` and `pin_type_routines` are
    /// unused; refuses either when used and the engine has no step for it.
    async fn seal_plan_type_review(
        &self,
        _conn: &mut Conn,
        _base: pbps_diff::Side<'_>,
        _desired: pbps_diff::Side<'_>,
        _changes: &ChangeSet,
        trusted: &BTreeSet<pbps_model::RoleName>,
        pin_routines: bool,
    ) -> anyhow::Result<bool> {
        defaults::type_review(self.name(), trusted, pin_routines)
    }

    /// Refuses a name this plan creates that the target already uses outside
    /// what the project records (#316, #320, #323, DEC-316.1). Asked before
    /// anything is checked against the plan, and before the planning
    /// transaction opens.
    ///
    /// Neutral when the plan creates no relation name; refuses when it does
    /// and the engine has no step to ask the namespace. An engine that asks
    /// it later, with the passes that order the plan, overrides this to
    /// accept (SQL Server, #1077).
    fn refuse_occupied_relation_names(
        &self,
        changes: &ChangeSet,
        _scoped: &pbps_diff::Scoped,
        _unreadable: &[(pbps_db::catalog::LimitationTarget, String)],
        _unmanaged_relations: &[TableName],
        _label: &str,
    ) -> anyhow::Result<()> {
        defaults::occupied_relation_names(self.name(), changes)
    }

    /// Refuses a plan whose rebuilds need the transaction a staged run does
    /// not have. Asked once before the planning transaction opens, over the
    /// plan as given, and again after the passes, over what the dependents
    /// added (a rebuild the plan now synthesizes needs the transaction as
    /// much as one it was given).
    ///
    /// The table rebuild and the closing `PUBLIC` revoke are refused for every
    /// engine; the default also refuses a staged module rebuild, because the
    /// engine has no step that preserves a replaced module's catalog state
    /// across commits.
    fn require_transactional_rebuilds(
        &self,
        changes: &ChangeSet,
        staged: bool,
    ) -> anyhow::Result<()> {
        defaults::transactional_rebuilds(self.name(), changes, staged)
    }

    /// Reads the target's names in the planning transaction, orders the
    /// moves the plan carries after what holds their names, and refuses what
    /// is left: the names the plan creates or takes, an `UNLOGGED` partition
    /// under a parent a permanent table references, a table made unlogged
    /// while a publication names it (#951, #1595, #1489). What it read stays
    /// in [`Self::State`] for [`Self::order_by_catalog`], which orders and
    /// checks again once the schedule may have planned the revision afresh.
    ///
    /// Neutral when the plan creates or renames no name and makes nothing
    /// unlogged; refuses when it does and the engine has no step for it.
    async fn check_names_against_catalog<S: Clone>(
        &self,
        _state: &mut Self::State,
        _conn: &mut Conn,
        changes: &mut PlanningPlan<S>,
        _frame: Frame<'_>,
        _staged: bool,
    ) -> anyhow::Result<()> {
        defaults::catalog_names(self.name(), changes)
    }

    /// Reads, inside the planning transaction and before the schedule, what
    /// the schedule needs and the plan cannot see: the members of the roles
    /// it drops, written into the plan by name so the artifact the gate
    /// approves lists who is removed (ADR-0005), and the modules a rebuilt
    /// enum's columns carry (DEC-1635.7). A non-empty second answer makes
    /// the pipeline plan again, so it is returned and not kept.
    ///
    /// Neutral when the plan drops no role and rebuilds no enum; refuses
    /// when it does and the engine has no step for it.
    async fn read_before_schedule(
        &self,
        _conn: &mut Conn,
        changes: &ChangeSet,
        _declared: &Schema,
        _recorded_ids: &IdsFile,
    ) -> anyhow::Result<ScheduleReads> {
        defaults::schedule_reads(self.name(), changes)?;
        Ok(ScheduleReads::default())
    }

    /// Schedules the plan: reads what depends on every module it drops and
    /// puts each dependent on the right side of the drop (#314), or expands a
    /// table rebuild, and brings the plan to its canonical form. Returns the
    /// scheduled plan and the `module_dependents` (or `blocking_rewrite`)
    /// row.
    ///
    /// `again` plans the same revision again with the given unchanged modules
    /// rebuilt: a catalog fact read here can force a second diff, and the
    /// pipeline owns the diff. It refuses when a resolver's answer would be
    /// dropped by planning again.
    ///
    /// Neutral (a `not_applicable` row) when the plan drops or rebuilds no module and
    /// rebuilds no table; refuses when it does and the engine has no step for
    /// it.
    #[allow(clippy::too_many_arguments)]
    async fn schedule<S: pbps_diff::order::RouteState<Self>>(
        &self,
        _state: &mut Self::State,
        _conn: &mut Conn,
        _admission: &SiteAdmission,
        changes: PlanningPlan<S>,
        frame: Frame<'_>,
        role_members: Option<&RoleMembers>,
        _again: &dyn Fn(&BTreeSet<pbps_model::ModuleId>) -> anyhow::Result<PlanningPlan<S>>,
    ) -> anyhow::Result<(PlanningPlan<S::Output>, ConnectedCheck)> {
        defaults::module_dependents(self.name(), &changes)?;
        let (declarations, read) = defaults::scheduled_inputs(
            frame.written.schema,
            crate::dependents::Sources {
                before: frame.recorded,
                after: frame.written.ids,
            },
            &frame.hints.annotations,
            role_members,
        );
        let scheduled = crate::dependents::schedule::schedule(changes, &declarations, &read, self)?;
        Ok((
            scheduled.changes,
            defaults::not_applicable(
                self.name(),
                "module_dependents",
                "the plan drops or rebuilds no module",
            ),
        ))
    }

    /// The passes that order the plan from what the catalog holds, last of
    /// the passes that order it, so that what they settle is what is checked
    /// and saved: a generated column's inputs (DEC-1316.1), a computed
    /// column's function drops (DEC-1431.1, #1459), the names the plan
    /// creates or moves (#1366), the carried moves again over the same
    /// reads (#1767), and the verifier of a table rebuild (#1954).
    ///
    /// Neutral when the plan has nothing those passes order; refuses when it
    /// does and the engine has no step for it.
    async fn order_by_catalog<S: Clone>(
        &self,
        _state: &mut Self::State,
        _conn: &mut Conn,
        changes: &mut PlanningPlan<S>,
        frame: Frame<'_>,
    ) -> anyhow::Result<Ordered> {
        defaults::ordering_by_catalog(
            self.name(),
            self,
            frame.written.ids,
            frame.declared.schema,
            changes,
        )?;
        Ok(Ordered::default())
    }

    /// The checks that read the final order, in the planning transaction: a
    /// module rebuild's catalog state, what a dropped or retyped object
    /// blocks, the logical slots a row move is decoded by, the publications
    /// and slots a numbered column's fill misses, and the guard that
    /// authenticates the triggers the plan's row writes meet. The rows are
    /// returned for neutral code to put in their contract order.
    ///
    /// A `not_applicable` row naming the engine for each when the plan asks
    /// nothing of it; refuses when it does and the engine has no step.
    async fn check_final_order(
        &self,
        _conn: &mut Conn,
        _admission: &SiteAdmission,
        changes: &ChangeSet,
        _frame: Frame<'_>,
    ) -> anyhow::Result<FinalOrderChecks> {
        defaults::final_order(self.name(), changes)
    }
}

/// The tables and modules a plan touches that a SQL Server expression edge can
/// name, under the names the catalog has them by: a computed column, a column
/// it reads, its table, or a module. The first selection of
/// `order_computed_by_edges`, shared with the default of
/// [`PlanSession::order_by_catalog`] so the two cannot select differently; the
/// step then narrows it to what an edge could refuse or move, which needs the
/// catalog's answer to know, so the default takes this superset.
// The complement is every change that touches no object an expression edge
// can name: a computed column, a column it reads, its table, or a module.
#[allow(clippy::wildcard_enum_match_arm)]
pub(in crate::engine) fn computed_edge_objects(
    names: &pbps_model::plan_names::PlanNames<'_>,
    cs: &ChangeSet,
) -> Vec<TableName> {
    let catalog = |at, table: &TableName| computed_catalog_table(names, table, at);
    let mut objects: Vec<pbps_model::TableName> = Vec::new();
    for (at, p) in cs.changes.iter().enumerate() {
        let touched = match &p.change {
            pbps_model::Change::AddComputedColumn { table, .. }
            | pbps_model::Change::DropComputedColumn { table, .. }
            | pbps_model::Change::RenameColumn { table, .. } => Some(catalog(at, table)),
            pbps_model::Change::DropTable { name, .. } => Some(name.clone()),
            pbps_model::Change::DropColumn { column, .. }
            | pbps_model::Change::AlterColumnType { column, .. }
            | pbps_model::Change::AlterColumnNullability { column, .. } => {
                Some(catalog(at, &column.table))
            }
            pbps_model::Change::AlterModule { id, .. }
            | pbps_model::Change::DropModule { id, .. } => Some(id.object_name()),
            _ => None,
        };
        if let Some(object) = touched
            && !objects.contains(&object)
        {
            objects.push(object);
        }
    }
    objects
}

/// What a provided method answers, as functions of the engine's name.
///
/// Separate from the trait so they can be tested directly (see the module
/// documentation). Every refusal names the engine and the feature.
pub(crate) mod defaults {
    use super::*;

    pub(crate) fn dependencies(
        engine: &str,
        declarations: &pbps_model::dependency::Dependencies,
    ) -> anyhow::Result<pbps_model::dependency::PlannedDependencies> {
        anyhow::ensure!(
            declarations.is_empty(),
            "{engine} does not support cross-database dependencies"
        );
        Ok(Default::default())
    }

    /// Both `trusted_roles` and `pin_type_routines` are PostgreSQL features; an
    /// engine that has no step for them must not read "unused" from "ignored".
    /// The answer is that no routine needs review.
    pub(crate) fn type_review(
        engine: &str,
        trusted: &BTreeSet<pbps_model::RoleName>,
        pin_routines: bool,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            trusted.is_empty(),
            "trusted_roles is not supported by {engine}: it has no step that seals type prerequisites"
        );
        anyhow::ensure!(
            !pin_routines,
            "pin_type_routines is not supported by {engine}: it has no step that reads type input routines"
        );
        Ok(false)
    }

    pub(crate) fn external_role_renames(
        engine: &'static str,
        recorded: &IdsFile,
        declared: &IdsFile,
    ) -> anyhow::Result<ExternalRoleRenames> {
        anyhow::ensure!(
            recorded.role_renames(declared).next().is_none(),
            "rename_evidence: {engine} has no step that confirms an external cluster-role rename"
        );
        Ok(ExternalRoleRenames {
            renames: BTreeMap::new(),
            check: not_applicable(
                engine,
                "rename_evidence",
                "no external cluster-role rename is declared",
            ),
        })
    }

    pub(crate) fn unknown_collations(engine: &str, declared: &Schema) -> anyhow::Result<()> {
        anyhow::ensure!(
            !declared
                .tables
                .values()
                .flat_map(|t| t.columns.values())
                .any(|c| c.collation.is_some()),
            "{engine} has no step that checks a declared column collation against the server"
        );
        Ok(())
    }

    pub(crate) fn history_references(engine: &str, declared: &Schema) -> anyhow::Result<()> {
        anyhow::ensure!(
            !declared.tables.values().any(|t| t
                .system_time
                .as_ref()
                .and_then(|s| s.versioning.as_ref())
                .is_some()),
            "{engine} has no step that checks a foreign key to a system-versioned history under the database's collation"
        );
        Ok(())
    }

    /// The roles `reconcile_cluster_roles` checks: incoming or renamed ones.
    pub(crate) fn cluster_roles(
        engine: &'static str,
        recorded: &IdsFile,
        queried: &IdsFile,
        renames: &BTreeMap<pbps_model::RoleName, pbps_model::RoleName>,
    ) -> anyhow::Result<ConnectedCheck> {
        anyhow::ensure!(
            !queried.roles.values().any(|name| {
                renames.values().any(|to| to == name)
                    || !recorded.roles.values().any(|old| old == name)
            }),
            "missing_roles: {engine} has no step that checks that a declared cluster role exists"
        );
        Ok(not_applicable(
            engine,
            "missing_roles",
            "no cluster role is incoming or renamed",
        ))
    }

    pub(crate) fn rebuild_hints(
        engine: &str,
        base: &Schema,
        declared: &Schema,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            base.types.is_empty() && base.extensions.is_empty() && declared.extensions.is_empty(),
            "{engine} has no step that reads the target facts a type or an extension change needs"
        );
        Ok(())
    }

    pub(crate) fn standing_domain_checks(engine: &str) -> anyhow::Result<Vec<StandingNotValid>> {
        anyhow::bail!("{engine} has no step that reads the NOT VALID checks of its domains")
    }

    /// Whether the plan creates, takes or moves a name in a namespace the
    /// target holds, selected by the same functions the PostgreSQL name read
    /// asks with: the relation names it creates (tables, views, indexes,
    /// owned sequences), the names its table renames take and the tables a
    /// rename moves between schemas, the types it creates or renames, and the
    /// partitions it detaches.
    #[allow(clippy::wildcard_enum_match_arm)] // The complement names nothing.
    fn names_a_thing(changes: &ChangeSet) -> bool {
        !crate::deploy::created_relation_names(changes).is_empty()
            || !crate::deploy::rename_claims(changes).is_empty()
            || !crate::deploy::transferred_tables(changes).is_empty()
            || changes.changes.iter().any(|p| {
                matches!(
                    p.change,
                    pbps_model::Change::CreateType { .. }
                        | pbps_model::Change::RenameType { .. }
                        | pbps_model::Change::DetachPartition { .. }
                )
            })
    }

    /// What the namespace check reads: a table or view it creates, an index it
    /// names, and any other module it creates (a routine or trigger is
    /// compared with the unreadable modules, not only the relations).
    pub(crate) fn occupied_relation_names(engine: &str, changes: &ChangeSet) -> anyhow::Result<()> {
        anyhow::ensure!(
            crate::deploy::created_relation_names(changes).is_empty()
                && !changes
                    .changes
                    .iter()
                    .any(|p| matches!(p.change, pbps_model::Change::CreateModule { .. })),
            "{engine} has no step that checks whether a name this plan creates is already in use"
        );
        Ok(())
    }

    /// Whether the plan drops a role, whose members the schedule writes in.
    pub(in crate::engine) fn drops_a_role(changes: &ChangeSet) -> bool {
        changes
            .changes
            .iter()
            .any(|p| matches!(p.change, pbps_model::Change::DropRole { .. }))
    }

    /// What the schedule is given of the declarations and of the roles the
    /// plan drops; the catalog facts an engine reads are added to the second.
    pub(in crate::engine) fn scheduled_inputs<'a>(
        declared: &'a Schema,
        sources: crate::dependents::Sources<'a>,
        deps: &'a pbps_model::object_ref::Annotations,
        role_members: Option<&'a RoleMembers>,
    ) -> (
        crate::dependents::schedule::Declarations<'a>,
        crate::dependents::schedule::Facts<'a>,
    ) {
        let declarations = crate::dependents::schedule::Declarations {
            schema: declared,
            sources,
            annotations: deps,
        };
        // The members of the roles this plan drops, which the caller read when
        // there were any: every connected plan goes through the schedule once,
        // whichever facts its engine supplies (#1955 review).
        let read = match role_members {
            Some(members) => {
                crate::dependents::schedule::Facts::default().with_role_members(members)
            }
            None => crate::dependents::schedule::Facts::default(),
        };
        (declarations, read)
    }

    /// The refusals every engine makes of a staged run, with `module_check`
    /// between the first and the last: the order of the three is the order a
    /// plan met them in. A table rebuild and the revoke that closes a routine
    /// are the same for every engine; whether a replaced module keeps its
    /// catalog state across the commits of a staged run is the engine's.
    pub(in crate::engine) fn staged_rebuilds(
        changes: &ChangeSet,
        staged: bool,
        module_check: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !staged
                || !changes
                    .changes
                    .iter()
                    .any(|p| matches!(p.change, pbps_model::Change::RebuildPartitionedTable(_))),
            "table rebuild requires transactional mode; --staged is unsupported"
        );
        module_check()?;
        // The `CREATE` and the revoke that closes the routine are two statements,
        // and a staged run commits each one on its own (DECISIONS 517). Between
        // those two commits the routine is visible to the whole cluster holding
        // this engine's default `EXECUTE` to `PUBLIC` — and the revoke sorts with
        // the grants, after every row the plan writes, so the window is the rest
        // of the plan rather than an instant. A `SECURITY DEFINER` routine open
        // for that long is the exposure this revoke exists to prevent, so the
        // plan is refused rather than run with a gap in it.
        let closes: Vec<String> = changes
            .changes
            .iter()
            .filter_map(|p| {
                // Only the half whose window is an over-exposure. `Kept`
                // writes a `GRANT` of its own, but the state between its
                // `CREATE` and that grant is a routine *less* reachable than the
                // declaration asks for — a caller sees a permission error, not
                // somebody else's privileges — so staging it is merely slow.
                if let pbps_model::Change::PublicExecution {
                    routine,
                    access: pbps_model::PublicAccess::Revoked,
                    ..
                } = &p.change
                {
                    Some(routine.shown().to_string())
                } else {
                    None
                }
            })
            .collect();
        if staged && !closes.is_empty() {
            anyhow::bail!(
                "a staged run commits each statement on its own, so {} would stay executable by \
                 every principal in the cluster between its `CREATE` and the revoke that closes it \
                 (DECISIONS 517): {}.\nRemove --staged and plan again, or declare \
                 `public_execute: true` on the routines that are meant to stay open",
                if closes.len() == 1 {
                    "1 routine".to_owned()
                } else {
                    format!("{} routines", closes.len())
                },
                closes.join(", ")
            );
        }
        Ok(())
    }

    pub(crate) fn transactional_rebuilds(
        engine: &str,
        changes: &ChangeSet,
        staged: bool,
    ) -> anyhow::Result<()> {
        staged_rebuilds(changes, staged, || {
            anyhow::ensure!(
                !staged || rebuilt_modules(changes).is_empty(),
                "{engine} has no step that keeps a replaced module's catalog state across the \
                 commits of a staged run; remove --staged and plan again"
            );
            Ok(())
        })
    }

    pub(crate) fn catalog_names(engine: &str, changes: &ChangeSet) -> anyhow::Result<()> {
        anyhow::ensure!(
            !names_a_thing(changes),
            "{engine} has no step that reads the target's names to order and check the ones this plan creates or renames"
        );
        anyhow::ensure!(
            !crate::deploy::leaves_something_unlogged(changes),
            "{engine} has no step that checks the foreign keys and publications of a table or partition this plan creates, attaches or makes unlogged"
        );
        Ok(())
    }

    /// The role members and the enum dependents the schedule needs.
    pub(crate) fn schedule_reads(engine: &str, changes: &ChangeSet) -> anyhow::Result<()> {
        anyhow::ensure!(
            !changes
                .changes
                .iter()
                .any(|p| matches!(p.change, pbps_model::Change::DropRole { .. })),
            "{engine} has no step that reads the members of a role this plan drops"
        );
        anyhow::ensure!(
            !changes
                .changes
                .iter()
                .any(|p| matches!(p.change, pbps_model::Change::EnumRebuild { .. })),
            "{engine} has no step that reads what depends on an enum this plan rebuilds"
        );
        Ok(())
    }

    pub(crate) fn module_dependents(engine: &str, changes: &ChangeSet) -> anyhow::Result<()> {
        // The classifier the schedule itself uses: a module dropped for good and
        // one the plan drops and recreates (`AlterModule`) both have dependents.
        anyhow::ensure!(
            pbps_diff::order::deployment::dropped_modules(changes).is_empty(),
            "{engine} has no step that reads what depends on a module this plan drops or rebuilds"
        );
        anyhow::ensure!(
            !changes
                .changes
                .iter()
                .any(|p| matches!(p.change, pbps_model::Change::RebuildPartitionedTable(_))),
            "{engine} has no step that plans a table rebuild"
        );
        Ok(())
    }

    /// The passes of [`PlanSession::order_by_catalog`], each refused for what
    /// its own step selects from the plan:
    /// - the generated-input release, any column retype or drop
    ///   (`row_security::column_edit`), because the generated columns that
    ///   read a column are in the catalog and not always in `declared`;
    /// - the computed-column edges, the superset the SQL Server step reads
    ///   edges for (`computed_edge_objects`), and the calls the collation
    ///   comparison covers (`added_call_spellings`);
    /// - the name walk, the objects and parents `object_reads` selects, and
    ///   any module change, which can bind to a name the walk compares
    ///   (`schema_bound_call_names`) — the narrowest plan-only superset,
    ///   because what the walk then asks of the catalog decides the rest.
    pub(crate) fn ordering_by_catalog(
        engine: &str,
        dialect: &dyn pbps_dialect::Dialect,
        ids: &IdsFile,
        declared: &Schema,
        changes: &ChangeSet,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !changes
                .changes
                .iter()
                .any(|p| crate::row_security::column_edit(&p.change).is_some()),
            "{engine} has no step that orders the retype or drop of a column a generated column reads"
        );
        let names = pbps_model::plan_names::PlanNames::of(&changes.changes, ids)?;
        anyhow::ensure!(
            computed_edge_objects(&names, changes).is_empty()
                && crate::computed_order::added_call_spellings(changes, declared, dialect)
                    .is_empty(),
            "{engine} has no step that orders a computed column's dependencies from the catalog"
        );
        let (created, parents) = crate::deploy::object_reads(changes, &names);
        anyhow::ensure!(
            created.is_empty()
                && parents.is_empty()
                && !changes
                    .changes
                    .iter()
                    .any(|p| matches!(p.change, pbps_model::change_family!(modules))),
            "{engine} has no step that orders the names this plan creates or moves from the catalog"
        );
        Ok(())
    }

    pub(crate) fn module_rebuilds(
        engine: &'static str,
        changes: &ChangeSet,
    ) -> anyhow::Result<ConnectedCheck> {
        anyhow::ensure!(
            rebuilt_modules(changes).is_empty(),
            "{engine} has no step that checks the catalog state a module rebuild cannot preserve"
        );
        Ok(not_applicable(
            engine,
            "before_a_rebuild",
            "the plan rebuilds no module",
        ))
    }

    /// The drops both engines' readers select: a table, a column, a primary
    /// key replaced, a unique constraint, an index, a numbering source (the
    /// PostgreSQL reader keeps the ones it gives up; the default takes every
    /// change of numbering), and the retype SQL Server counts under a key.
    #[allow(clippy::wildcard_enum_match_arm)] // The complement drops nothing.
    pub(crate) fn drop_blockers(
        engine: &'static str,
        changes: &ChangeSet,
    ) -> anyhow::Result<ConnectedCheck> {
        anyhow::ensure!(
            !changes.changes.iter().any(|p| matches!(
                p.change,
                pbps_model::Change::DropTable { .. }
                    | pbps_model::Change::DropColumn { .. }
                    | pbps_model::Change::DropUnique { .. }
                    | pbps_model::Change::DropIndex { .. }
                    | pbps_model::Change::AlterColumnNumbering { .. }
                    | pbps_model::Change::AlterColumnType { .. }
                    | pbps_model::Change::SetPrimaryKey { from: Some(_), .. }
            )),
            "{engine} has no step that reads what depends on an object this plan drops or retypes"
        );
        Ok(not_applicable(
            engine,
            "drop_blockers",
            "the plan drops and retypes nothing a dependency could block",
        ))
    }

    pub(crate) fn logical_slots(
        engine: &'static str,
        changes: &ChangeSet,
    ) -> anyhow::Result<ConnectedCheck> {
        anyhow::ensure!(
            !changes.changes.iter().any(|p| matches!(
                p.change,
                pbps_model::Change::CreateTable {
                    beside_default: Some(_),
                    ..
                }
            )),
            "{engine} has no step that lists the logical slots that would decode the rows a new partition takes from its DEFAULT"
        );
        Ok(not_applicable(
            engine,
            "logical_slots",
            "the plan makes no partition beside a DEFAULT",
        ))
    }

    pub(crate) fn numbered_column_fills(
        engine: &'static str,
        changes: &ChangeSet,
    ) -> anyhow::Result<ConnectedCheck> {
        anyhow::ensure!(
            !changes.changes.iter().any(|p| matches!(
                &p.change,
                pbps_model::Change::AddColumn { column, .. }
                    if matches!(
                        column.value_source,
                        pbps_model::ValueSource::Identity(_) | pbps_model::ValueSource::Sequence(_)
                    )
            )),
            "{engine} has no step that lists the publications and slots that would miss the numbers a new identity or serial column gives the rows already there"
        );
        Ok(not_applicable(
            engine,
            "numbered_column_fills",
            "the plan adds no identity or serial column",
        ))
    }

    /// The changes the PostgreSQL guard prepares for: the row statements, and
    /// whatever rewrites a table's stored rows (`rewrite_write`).
    pub(crate) fn data_writes(engine: &str, changes: &ChangeSet) -> anyhow::Result<()> {
        anyhow::ensure!(
            !changes.changes.iter().any(|p| matches!(
                p.change,
                pbps_model::Change::InsertRow { .. }
                    | pbps_model::Change::UpdateRow { .. }
                    | pbps_model::Change::DeleteRow { .. }
            ) || pbps_pg::existing_expressions::rewrite_write(
                &p.change
            )
            .is_some()),
            "{engine} has no step that authenticates the triggers a row write or rewrite of this plan meets"
        );
        Ok(())
    }

    pub(crate) fn final_order(
        engine: &'static str,
        changes: &ChangeSet,
    ) -> anyhow::Result<FinalOrderChecks> {
        // The order the checks were asked in.
        let rebuilds = module_rebuilds(engine, changes)?;
        let drop_blockers = drop_blockers(engine, changes)?;
        let logical_slots = logical_slots(engine, changes)?;
        let numbered_column_fills = numbered_column_fills(engine, changes)?;
        data_writes(engine, changes)?;
        Ok(FinalOrderChecks {
            rebuilds,
            drop_blockers,
            logical_slots,
            numbered_column_fills,
        })
    }

    /// A `connected_checks` row for a check the engine has no step for.
    pub(crate) fn not_applicable(
        engine: &'static str,
        name: &'static str,
        why: &str,
    ) -> ConnectedCheck {
        ConnectedCheck {
            name,
            engine,
            status: "not_applicable",
            message: format!("{engine} has no step for this check: {why}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THIRD: &str = "thirdsql";

    fn roles(names: &[&str]) -> BTreeSet<pbps_model::RoleName> {
        names
            .iter()
            .map(|n| pbps_model::RoleName::new(*n))
            .collect()
    }

    fn refusal<T>(result: anyhow::Result<T>) -> String {
        match result {
            Ok(_) => panic!("the default must refuse"),
            Err(error) => error.to_string(),
        }
    }

    fn role_uid() -> pbps_model::Uid {
        pbps_model::Uid::derived(pbps_model::UidKind::Role, "role-1", 0)
    }

    #[test]
    fn a_plan_without_dependencies_gets_the_neutral_answer_from_the_default() {
        let planned = defaults::dependencies(THIRD, &Default::default()).unwrap();
        assert_eq!(planned, Default::default());
    }

    #[test]
    fn declared_dependencies_are_refused_naming_the_engine_and_the_feature() {
        let mut declared = pbps_model::dependency::Dependencies::default();
        declared.insert(
            "other.dbo.t".parse().expect("a three-part dependency name"),
            pbps_model::dependency::Dependency {
                kind: pbps_model::dependency::DependencyKind::Table,
                columns: BTreeMap::new(),
            },
        );
        let message = refusal(defaults::dependencies(THIRD, &declared));
        assert!(message.contains(THIRD), "{message}");
        assert!(message.contains("cross-database dependencies"), "{message}");
    }

    #[test]
    fn unused_type_pin_settings_review_nothing_by_default() {
        assert!(!defaults::type_review(THIRD, &BTreeSet::new(), false).unwrap());
    }

    #[test]
    fn trusted_roles_and_pinned_routines_are_refused_naming_the_engine() {
        let message = refusal(defaults::type_review(THIRD, &roles(&["admin"]), false));
        assert!(
            message.contains("trusted_roles") && message.contains(THIRD),
            "{message}"
        );
        let message = refusal(defaults::type_review(THIRD, &BTreeSet::new(), true));
        assert!(
            message.contains("pin_type_routines") && message.contains(THIRD),
            "{message}"
        );
        // The first of the two to fail is trusted_roles, as the sealing step
        // precedes the routine read it feeds.
        let message = refusal(defaults::type_review(THIRD, &roles(&["admin"]), true));
        assert!(message.contains("trusted_roles"), "{message}");
    }

    #[test]
    fn no_role_rename_gets_a_not_applicable_row_naming_the_engine() {
        let ids = IdsFile::default();
        let answer = defaults::external_role_renames(THIRD, &ids, &ids).unwrap();
        assert!(answer.renames.is_empty());
        assert_eq!(answer.check.name, "rename_evidence");
        assert_eq!(answer.check.status, "not_applicable");
        assert_eq!(answer.check.engine, THIRD);
        assert!(answer.check.message.contains(THIRD));
    }

    #[test]
    fn a_declared_role_rename_is_refused_by_an_engine_with_no_evidence_step() {
        let uid = role_uid();
        let mut recorded = IdsFile::default();
        recorded
            .roles
            .insert(uid.clone(), pbps_model::RoleName::new("old"));
        let mut declared = IdsFile::default();
        declared.roles.insert(uid, pbps_model::RoleName::new("new"));
        let message = refusal(defaults::external_role_renames(THIRD, &recorded, &declared));
        assert!(
            message.contains(THIRD) && message.contains("rename"),
            "{message}"
        );
    }

    #[test]
    fn an_incoming_cluster_role_is_refused_and_none_is_not_applicable() {
        let recorded = IdsFile::default();
        let mut queried = IdsFile::default();
        let none = defaults::cluster_roles(THIRD, &recorded, &queried, &BTreeMap::new()).unwrap();
        assert_eq!(
            (none.name, none.status, none.engine),
            ("missing_roles", "not_applicable", THIRD)
        );
        queried
            .roles
            .insert(role_uid(), pbps_model::RoleName::new("app"));
        let message = refusal(defaults::cluster_roles(
            THIRD,
            &recorded,
            &queried,
            &BTreeMap::new(),
        ));
        assert!(
            message.contains(THIRD) && message.contains("cluster role"),
            "{message}"
        );
        // A role the recorded state already holds is neither incoming nor renamed.
        let mut recorded = IdsFile::default();
        recorded
            .roles
            .insert(role_uid(), pbps_model::RoleName::new("app"));
        defaults::cluster_roles(THIRD, &recorded, &queried, &BTreeMap::new()).unwrap();
    }

    #[test]
    fn a_schema_with_nothing_the_catalog_steps_ask_about_passes_their_defaults() {
        let schema = Schema::default();
        defaults::unknown_collations(THIRD, &schema).unwrap();
        defaults::history_references(THIRD, &schema).unwrap();
        defaults::rebuild_hints(THIRD, &schema, &schema).unwrap();
    }

    #[test]
    fn a_declared_type_is_refused_when_the_engine_cannot_read_its_facts() {
        let mut base = Schema::default();
        base.types.insert(
            pbps_model::TypeRef::new("app", "mood").unwrap(),
            pbps_model::ManagedType::Enum {
                values: vec!["ok".into()],
            },
        );
        let message = refusal(defaults::rebuild_hints(THIRD, &base, &Schema::default()));
        assert!(
            message.contains(THIRD) && message.contains("type"),
            "{message}"
        );
    }

    #[test]
    fn a_standing_domain_check_question_is_refused_not_answered_with_none() {
        let message = refusal(defaults::standing_domain_checks(THIRD));
        assert!(
            message.contains(THIRD) && message.contains("NOT VALID"),
            "{message}"
        );
    }

    fn plan(changes: Vec<pbps_model::Change>) -> ChangeSet {
        ChangeSet {
            changes: changes
                .into_iter()
                .map(pbps_model::PlannedChange::new)
                .collect(),
        }
    }

    fn uid(kind: pbps_model::UidKind, address: &str) -> pbps_model::Uid {
        pbps_model::Uid::derived(kind, address, 0)
    }

    fn table_name() -> TableName {
        TableName::new("app", "t")
    }

    fn column_named(source: pbps_model::ValueSource) -> pbps_model::Column {
        let mut column = pbps_model::Column::new("integer".parse().unwrap());
        column.value_source = source;
        column
    }

    #[test]
    fn a_plan_that_creates_no_name_passes_the_occupied_names_default() {
        defaults::occupied_relation_names(THIRD, &ChangeSet::default()).unwrap();
        defaults::catalog_names(THIRD, &ChangeSet::default()).unwrap();
    }

    #[test]
    fn a_created_table_is_refused_by_the_name_defaults_naming_the_engine() {
        let changes = plan(vec![pbps_model::Change::CreateTable {
            uid: uid(pbps_model::UidKind::Table, "app.t"),
            name: table_name(),
            table: Default::default(),
            beside_default: None,
            moves_at_most: None,
        }]);
        let message = refusal(defaults::occupied_relation_names(THIRD, &changes));
        assert!(
            message.contains(THIRD) && message.contains("name this plan creates"),
            "{message}"
        );
        let message = refusal(defaults::catalog_names(THIRD, &changes));
        assert!(message.contains(THIRD), "{message}");
    }

    #[test]
    fn a_table_made_unlogged_is_refused_by_the_catalog_names_default() {
        let changes = plan(vec![pbps_model::Change::SetTablePersistence {
            uid: uid(pbps_model::UidKind::Table, "app.t"),
            table: table_name(),
            unlogged: true,
        }]);
        let message = refusal(defaults::catalog_names(THIRD, &changes));
        assert!(
            message.contains(THIRD) && message.contains("unlogged"),
            "{message}"
        );
        // Made logged again, it asks nothing of the target.
        let changes = plan(vec![pbps_model::Change::SetTablePersistence {
            uid: uid(pbps_model::UidKind::Table, "app.t"),
            table: table_name(),
            unlogged: false,
        }]);
        defaults::catalog_names(THIRD, &changes).unwrap();
    }

    #[test]
    fn a_staged_run_is_refused_the_table_rebuild_by_every_engine_and_the_module_rebuild_by_the_default()
     {
        defaults::transactional_rebuilds(THIRD, &ChangeSet::default(), true).unwrap();
        // Not staged: nothing is refused.
        defaults::transactional_rebuilds(THIRD, &ChangeSet::default(), false).unwrap();
        // The shared refusals do not depend on the engine's module check.
        let rebuild = plan(vec![pbps_model::Change::PublicExecution {
            routine: "app.f(integer)".parse().expect("a routine id"),
            access: pbps_model::PublicAccess::Revoked,
            origin: pbps_model::RoutineOrigin::Created,
        }]);
        let message = refusal(defaults::transactional_rebuilds(THIRD, &rebuild, true));
        assert!(message.contains("DECISIONS 517"), "{message}");
        defaults::transactional_rebuilds(THIRD, &rebuild, false).unwrap();
    }

    #[test]
    fn the_engines_module_check_sits_between_the_two_shared_refusals() {
        // The table rebuild is refused before the engine's check runs, and
        // the engine's check before the closing revoke: the order a plan
        // met them in.
        let mut asked = false;
        let rebuild = plan(vec![]);
        defaults::staged_rebuilds(&rebuild, true, || {
            asked = true;
            Ok(())
        })
        .unwrap();
        assert!(asked);
        let refused = defaults::staged_rebuilds(&rebuild, true, || anyhow::bail!("module check"));
        assert_eq!(refusal(refused), "module check");
    }

    #[test]
    fn a_dropped_role_or_rebuilt_enum_is_refused_by_the_schedule_reads_default() {
        defaults::schedule_reads(THIRD, &ChangeSet::default()).unwrap();
        let drop_role = plan(vec![pbps_model::Change::DropRole {
            uid: uid(pbps_model::UidKind::Role, "old"),
            name: pbps_model::RoleName::new("old"),
            members: Vec::new(),
        }]);
        let message = refusal(defaults::schedule_reads(THIRD, &drop_role));
        assert!(
            message.contains(THIRD) && message.contains("role"),
            "{message}"
        );
        assert!(defaults::drops_a_role(&drop_role));
        assert!(!defaults::drops_a_role(&ChangeSet::default()));
    }

    #[test]
    fn a_dropped_module_or_table_rebuild_is_refused_by_the_schedule_default() {
        defaults::module_dependents(THIRD, &ChangeSet::default()).unwrap();
        let drop_module = plan(vec![pbps_model::Change::DropModule {
            id: pbps_model::ModuleId::Named(table_name()),
            kind: pbps_model::ModuleKind::View,
        }]);
        let message = refusal(defaults::module_dependents(THIRD, &drop_module));
        assert!(
            message.contains(THIRD) && message.contains("module"),
            "{message}"
        );
    }

    #[test]
    fn an_empty_plan_gets_not_applicable_rows_naming_the_engine_from_the_final_order_default() {
        let checks = defaults::final_order("thirdsql", &ChangeSet::default()).unwrap();
        for (row, name) in [
            (&checks.rebuilds, "before_a_rebuild"),
            (&checks.drop_blockers, "drop_blockers"),
            (&checks.logical_slots, "logical_slots"),
            (&checks.numbered_column_fills, "numbered_column_fills"),
        ] {
            assert_eq!(row.name, name);
            assert_eq!(row.status, "not_applicable");
            assert_eq!(row.engine, "thirdsql");
            assert!(row.message.contains("thirdsql"), "{}", row.message);
        }
    }

    #[test]
    fn each_final_order_default_refuses_the_thing_it_covers() {
        let drop_table = plan(vec![pbps_model::Change::DropTable {
            uid: uid(pbps_model::UidKind::Table, "app.t"),
            name: table_name(),
            detach_from: None,
        }]);
        let message = refusal(defaults::drop_blockers(THIRD, &drop_table));
        assert!(
            message.contains(THIRD) && message.contains("depends"),
            "{message}"
        );
        let beside = plan(vec![pbps_model::Change::CreateTable {
            uid: uid(pbps_model::UidKind::Table, "app.d"),
            name: table_name(),
            table: Default::default(),
            beside_default: Some(TableName::new("app", "d")),
            moves_at_most: None,
        }]);
        let message = refusal(defaults::logical_slots(THIRD, &beside));
        assert!(message.contains("logical slots"), "{message}");
        let numbered = plan(vec![pbps_model::Change::AddColumn {
            uid: uid(pbps_model::UidKind::Column, "app.t.n"),
            table: table_name(),
            name: "n".into(),
            column: Box::new(column_named(pbps_model::ValueSource::Identity(
                pbps_model::Identity::new(1, 1),
            ))),
        }]);
        let message = refusal(defaults::numbered_column_fills(THIRD, &numbered));
        assert!(message.contains("identity or serial"), "{message}");
        let rewrite = retype_plan();
        let message = refusal(defaults::data_writes(THIRD, &rewrite));
        assert!(
            message.contains(THIRD) && message.contains("triggers"),
            "{message}"
        );
        // The row checks are asked in order: the first refusal wins.
        let mut both = plan(vec![pbps_model::Change::DropTable {
            uid: uid(pbps_model::UidKind::Table, "app.t"),
            name: table_name(),
            detach_from: None,
        }]);
        both.changes.extend(retype_plan().changes);
        let message = refusal(defaults::final_order(THIRD, &both));
        assert!(message.contains("depends"), "{message}");
    }

    fn ordering(changes: &ChangeSet, declared: &Schema) -> anyhow::Result<()> {
        defaults::ordering_by_catalog(
            THIRD,
            &pbps_pg::Postgres::new(),
            &crate::plan_fixture::ids(changes),
            declared,
            changes,
        )
    }

    fn retype_plan() -> ChangeSet {
        plan(vec![pbps_model::Change::AlterColumnType {
            uid: uid(pbps_model::UidKind::Column, "app.t.a"),
            column: column_ref("a"),
            from: "integer".parse().unwrap(),
            to: "bigint".parse().unwrap(),
            from_nullable: true,
            to_nullable: true,
            from_collation: None,
            to_collation: None,
            through_external_partitions: false,
            using: None,
        }])
    }

    fn column_ref(name: &str) -> pbps_model::ColumnRef {
        table_name().column(pbps_model::ColumnName::from(name))
    }

    #[test]
    fn the_catalog_ordering_default_passes_an_empty_plan_and_refuses_a_column_edit() {
        ordering(&ChangeSet::default(), &Schema::default()).unwrap();
        // The generated columns that read a column are in the catalog, not
        // necessarily in the declarations: any retype or drop is refused.
        let drop = plan(vec![pbps_model::Change::DropColumn {
            uid: uid(pbps_model::UidKind::Column, "app.t.a"),
            column: column_ref("a"),
        }]);
        let message = refusal(ordering(&drop, &Schema::default()));
        assert!(
            message.contains(THIRD) && message.contains("generated column"),
            "{message}"
        );
        // Negative: a change of persistence reads no generated column.
        let nullable = plan(vec![pbps_model::Change::SetTablePersistence {
            uid: uid(pbps_model::UidKind::Table, "app.t"),
            table: table_name(),
            unlogged: false,
        }]);
        ordering(&nullable, &Schema::default()).unwrap();
    }

    #[test]
    fn the_catalog_ordering_default_refuses_what_the_edge_read_selects() {
        // A computed column's function drops after it: the edge read is made
        // for a module change, and for a rename, though no schema holds a
        // computed column.
        let rebuilt = plan(vec![pbps_model::Change::AlterModule {
            id: pbps_model::ModuleId::Routine("app.f(integer)".parse().unwrap()),
            module: Box::new(pbps_model::Module {
                kind: pbps_model::ModuleKind::Function,
                description: None,
                definition: "SELECT 1".into(),
                options: Default::default(),
            }),
        }]);
        let message = refusal(ordering(&rebuilt, &Schema::default()));
        assert!(
            message.contains(THIRD) && message.contains("computed column"),
            "{message}"
        );
        let renamed = plan(vec![pbps_model::Change::RenameColumn {
            uid: uid(pbps_model::UidKind::Column, "app.t.a"),
            table: table_name(),
            from: "a".into(),
            to: "b".into(),
            table_was: None,
        }]);
        refusal(ordering(&renamed, &Schema::default()));
        // Negative: a change of persistence touches no table, column or module an edge names.
        let persistence = plan(vec![pbps_model::Change::SetTablePersistence {
            uid: uid(pbps_model::UidKind::Table, "app.t"),
            table: table_name(),
            unlogged: false,
        }]);
        ordering(&persistence, &Schema::default()).unwrap();
    }

    #[test]
    fn the_catalog_ordering_default_refuses_what_the_name_walk_selects() {
        let created = plan(vec![pbps_model::Change::CreateTable {
            uid: uid(pbps_model::UidKind::Table, "app.t"),
            name: table_name(),
            table: Default::default(),
            beside_default: None,
            moves_at_most: None,
        }]);
        let message = refusal(ordering(&created, &Schema::default()));
        assert!(
            message.contains("names this plan creates or moves"),
            "{message}"
        );
        // A table dropped is a parent whose constraints and defaults move.
        let dropped = plan(vec![pbps_model::Change::DropTable {
            uid: uid(pbps_model::UidKind::Table, "app.t"),
            name: table_name(),
            detach_from: None,
        }]);
        refusal(ordering(&dropped, &Schema::default()));
    }

    #[test]
    fn a_rebuilt_module_is_refused_by_the_schedule_default_like_a_dropped_one() {
        // `AlterModule` is a drop and a create to the schedule's own classifier.
        let rebuilt = plan(vec![pbps_model::Change::AlterModule {
            id: pbps_model::ModuleId::Named(table_name()),
            module: Box::new(pbps_model::Module {
                kind: pbps_model::ModuleKind::View,
                description: None,
                definition: "SELECT 1".into(),
                options: Default::default(),
            }),
        }]);
        assert!(!pbps_diff::order::deployment::dropped_modules(&rebuilt).is_empty());
        let message = refusal(defaults::module_dependents(THIRD, &rebuilt));
        assert!(
            message.contains(THIRD) && message.contains("drops or rebuilds"),
            "{message}"
        );
        // Negative: a module created from nothing has no dependents yet.
        let created = plan(vec![pbps_model::Change::CreateModule {
            id: pbps_model::ModuleId::Named(table_name()),
            module: Box::new(pbps_model::Module {
                kind: pbps_model::ModuleKind::View,
                description: None,
                definition: "SELECT 1".into(),
                options: Default::default(),
            }),
        }]);
        assert!(pbps_diff::order::deployment::dropped_modules(&created).is_empty());
        defaults::module_dependents(THIRD, &created).unwrap();
    }

    fn partition(unlogged: bool) -> pbps_model::Table {
        pbps_model::Table {
            unlogged,
            partition_of: Some(pbps_model::PartitionOf {
                parent: TableName::new("app", "ev"),
                bound: pbps_model::PartitionBound::Default,
                columns: Default::default(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_partition_created_or_attached_unlogged_is_refused_by_the_catalog_names_default() {
        let created = |unlogged| {
            plan(vec![pbps_model::Change::CreateTable {
                uid: uid(pbps_model::UidKind::Table, "app.t"),
                name: table_name(),
                table: Box::new(partition(unlogged)),
                beside_default: None,
                moves_at_most: None,
            }])
        };
        let attached = |unlogged| {
            plan(vec![pbps_model::Change::AttachPartition {
                uid: uid(pbps_model::UidKind::Table, "app.t"),
                table: table_name(),
                parent: TableName::new("app", "ev"),
                bound: pbps_model::PartitionBound::Default,
                key: Vec::new(),
                shape: Box::new(pbps_model::Table {
                    unlogged,
                    ..Default::default()
                }),
            }])
        };
        // Only the persistence read is asked of an attach: the name read is
        // not, so the message names unlogged tables and not names.
        let message = refusal(defaults::catalog_names(THIRD, &attached(true)));
        assert!(message.contains("unlogged"), "{message}");
        // The created partition also names a relation; the persistence
        // refusal is the second of the two, so either message is the engine's.
        let message = refusal(defaults::catalog_names(THIRD, &created(true)));
        assert!(message.contains(THIRD), "{message}");
        // Negative: a table attached permanent leaves nothing unlogged.
        defaults::catalog_names(THIRD, &attached(false)).unwrap();
        assert!(crate::deploy::leaves_something_unlogged(&attached(true)));
        assert!(!crate::deploy::leaves_something_unlogged(&attached(false)));
        assert!(crate::deploy::leaves_something_unlogged(&created(true)));
        assert!(!crate::deploy::leaves_something_unlogged(&created(false)));
    }

    #[test]
    fn a_table_moved_between_schemas_or_a_type_renamed_is_refused_by_the_catalog_names_default() {
        let moved = plan(vec![pbps_model::Change::RenameTable {
            uid: uid(pbps_model::UidKind::Table, "app.t"),
            from: table_name(),
            to: TableName::new("other", "t"),
            defaults: Vec::new(),
        }]);
        let message = refusal(defaults::catalog_names(THIRD, &moved));
        assert!(message.contains(THIRD), "{message}");
        // Negative: a change of persistence names nothing.
        let nullable = plan(vec![pbps_model::Change::SetTablePersistence {
            uid: uid(pbps_model::UidKind::Table, "app.t"),
            table: table_name(),
            unlogged: false,
        }]);
        defaults::catalog_names(THIRD, &nullable).unwrap();
    }

    #[test]
    fn any_created_module_is_refused_by_the_occupied_names_default() {
        // A routine is no relation, but the namespace check compares it with
        // the modules the project cannot read.
        let routine = plan(vec![pbps_model::Change::CreateModule {
            id: pbps_model::ModuleId::Routine("app.f(integer)".parse().unwrap()),
            module: Box::new(pbps_model::Module {
                kind: pbps_model::ModuleKind::Function,
                description: None,
                definition: "SELECT 1".into(),
                options: Default::default(),
            }),
        }]);
        assert!(crate::deploy::created_relation_names(&routine).is_empty());
        let message = refusal(defaults::occupied_relation_names(THIRD, &routine));
        assert!(message.contains(THIRD), "{message}");
        // Negative: dropping a module creates no name.
        let dropped = plan(vec![pbps_model::Change::DropModule {
            id: pbps_model::ModuleId::Routine("app.f(integer)".parse().unwrap()),
            kind: pbps_model::ModuleKind::Function,
        }]);
        defaults::occupied_relation_names(THIRD, &dropped).unwrap();
    }

    #[test]
    fn a_numbering_change_and_a_table_rewrite_are_read_by_the_final_order_defaults() {
        let renumbered = plan(vec![pbps_model::Change::AlterColumnNumbering {
            uid: uid(pbps_model::UidKind::Column, "app.t.a"),
            column: column_ref("a"),
            ty: "integer".parse().unwrap(),
            from: pbps_model::ValueSource::Identity(pbps_model::Identity::new(1, 1)),
            to: pbps_model::ValueSource::None,
        }]);
        let message = refusal(defaults::drop_blockers(THIRD, &renumbered));
        assert!(message.contains(THIRD), "{message}");
        // Negative: dropping a foreign key or a check is not a drop these
        // readers ask about.
        let check = plan(vec![pbps_model::Change::DropCheck {
            table: table_name(),
            name: "c".into(),
        }]);
        defaults::drop_blockers(THIRD, &check).unwrap();
        // A column type change rewrites the table's rows, which the guard
        // authenticates the triggers for even though no row is written.
        let retype = retype_plan();
        assert!(
            retype
                .changes
                .iter()
                .all(|p| pbps_pg::existing_expressions::rewrite_write(&p.change).is_some())
        );
        let message = refusal(defaults::data_writes(THIRD, &retype));
        assert!(message.contains("rewrite"), "{message}");
        defaults::data_writes(THIRD, &check).unwrap();
    }

    #[test]
    fn the_two_engines_inherit_no_default_text_for_their_own_features() {
        // The existing engines keep their exact wording: none of the generic
        // refusals above can be reached through them.
        assert_eq!(
            postgres::DEPENDENCIES_REFUSAL,
            "PostgreSQL does not support cross-database dependencies"
        );
        assert_eq!(
            mssql::TRUSTED_ROLES_REFUSAL,
            "trusted_roles is supported only on PostgreSQL"
        );
        assert_eq!(
            mssql::PIN_ROUTINES_REFUSAL,
            "pin_type_routines is supported only on PostgreSQL"
        );
    }
}
