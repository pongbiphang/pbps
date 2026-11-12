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
//! today. This module holds the positions before the resolver block; the later
//! ones are further methods of the same trait.
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
