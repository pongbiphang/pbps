//! SQL Server's positions of a connected plan.
//!
//! The bodies are the `Driver::Mssql` arms of the functions of [`super::super`]
//! that the pipeline used to call one by one. Those functions keep their other
//! callers (bootstrap, apply, the pins) and reach these bodies through the free
//! functions here, so each body is written once.

use super::*;

/// SQL Server has no `trusted_roles`; its wording is its own, not the
/// default's.
pub(super) const TRUSTED_ROLES_REFUSAL: &str = "trusted_roles is supported only on PostgreSQL";
pub(super) const PIN_ROUTINES_REFUSAL: &str = "pin_type_routines is supported only on PostgreSQL";

pub(in crate::engine) fn seal_type_pins(
    trusted: &BTreeSet<pbps_model::RoleName>,
) -> anyhow::Result<Prerequisites> {
    if trusted.is_empty() {
        Ok(Prerequisites::default())
    } else {
        anyhow::bail!("{TRUSTED_ROLES_REFUSAL}")
    }
}

pub(in crate::engine) fn type_input_routines(
    pins: &[pbps_model::TypePin],
    pin_routines: bool,
) -> anyhow::Result<BTreeSet<i64>> {
    if !pin_routines && pins.is_empty() {
        Ok(BTreeSet::new())
    } else {
        anyhow::bail!("{PIN_ROUTINES_REFUSAL}")
    }
}

pub(in crate::engine) fn external_role_renames() -> ExternalRoleRenames {
    ExternalRoleRenames {
        renames: BTreeMap::new(),
        check: ConnectedCheck {
            name: "rename_evidence",
            engine: "SQL Server",
            status: "not_applicable",
            message: "SQL Server renames database roles through the typed plan; external cluster-role rename evidence does not apply".into(),
        },
    }
}

pub(in crate::engine) async fn refuse_unknown_collations(
    conn: &mut Conn,
    schema: &Schema,
) -> anyhow::Result<()> {
    let names: Vec<String> = schema
        .tables
        .values()
        .flat_map(|t| t.columns.values())
        .filter_map(|c| c.collation.as_ref().map(|c| c.as_str().to_owned()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let unknown = pbps_mssql::catalog::unknown_collations(conn, &names).await?;
    if !unknown.is_empty() {
        anyhow::bail!(
            "this server has no collation named {}; a `COLLATE` naming it would be refused \
             mid-apply",
            unknown
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

pub(in crate::engine) async fn refuse_history_references_alike(
    conn: &mut Conn,
    schema: &Schema,
    dialect: &dyn pbps_dialect::Dialect,
) -> anyhow::Result<()> {
    let histories: Vec<&TableName> = schema
        .tables
        .values()
        .filter_map(|t| t.system_time.as_ref()?.versioning.as_ref())
        .map(|v| &v.history)
        .collect();
    // A key to a history as written is the offline check's, already refused.
    let referenced: BTreeSet<&TableName> = schema
        .tables
        .values()
        .flat_map(|t| t.foreign_keys.values())
        .map(|fk| &fk.references_table)
        .filter(|r| !histories.contains(r))
        .collect();
    if histories.is_empty() || referenced.is_empty() {
        return Ok(());
    }
    let names: Vec<TableName> = histories
        .iter()
        .copied()
        .chain(referenced)
        .cloned()
        .collect();
    let alike = pbps_mssql::catalog::object_names_alike(conn, &names).await?;
    let problems = pbps_dialect::history_references_under(schema, dialect, |a, b| {
        a == b
            || alike
                .iter()
                .any(|(x, y)| (x == a && y == b) || (x == b && y == a))
    });
    if !problems.is_empty() {
        anyhow::bail!(
            "the declarations have a foreign key the engine refuses (13565):\n  {}",
            problems.join("\n  ")
        );
    }
    Ok(())
}

pub(in crate::engine) fn reconcile_cluster_roles() -> ConnectedCheck {
    ConnectedCheck {
        name: "missing_roles",
        engine: "SQL Server",
        status: "not_applicable",
        message: "SQL Server creates and renames database roles through the typed plan; the external cluster-role presence check does not apply".into(),
    }
}

/// Nothing crosses positions for SQL Server: its misspelling probe asks
/// nothing of a type-routine review. A unit struct and not `()`, so the
/// pipeline's `let mut state = Default::default()` is not a unit binding in
/// this engine's instantiation of the macro.
#[derive(Debug, Default)]
pub(crate) struct PlanState;

impl PlanSession for pbps_mssql::Mssql {
    type State = PlanState;

    async fn compare_dependencies(
        &self,
        conn: &mut Conn,
        declarations: &pbps_model::dependency::Dependencies,
    ) -> anyhow::Result<pbps_model::dependency::PlannedDependencies> {
        Ok(pbps_mssql::dependencies::compare(conn, declarations).await?)
    }

    async fn seal_declared_type_pins(
        &self,
        _state: &mut PlanState,
        _conn: &mut Conn,
        _declared: &Schema,
        trusted: &BTreeSet<pbps_model::RoleName>,
        pin_routines: bool,
    ) -> anyhow::Result<()> {
        // The sealing step, then the routine read it feeds: the order of the
        // two refusals is the order of the two steps.
        let pins = seal_type_pins(trusted)?;
        type_input_routines(&pins.types, pin_routines)?;
        Ok(())
    }

    async fn external_role_renames(
        &self,
        _conn: &mut Conn,
        _recorded: &IdsFile,
        _declared: &IdsFile,
    ) -> anyhow::Result<ExternalRoleRenames> {
        Ok(external_role_renames())
    }

    async fn misspelt(
        &self,
        _state: &PlanState,
        conn: &mut Conn,
        schema: &Schema,
        at: &CatalogNames,
    ) -> Result<Spellings, RowsError> {
        pbps_mssql::catalog::misspelt(conn, schema, at).await
    }

    async fn schema_spellings(
        &self,
        conn: &mut Conn,
        names: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, Option<String>>, DbError> {
        pbps_mssql::catalog::schema_spellings(conn, names).await
    }

    async fn refuse_unknown_collations(
        &self,
        conn: &mut Conn,
        declared: &Schema,
    ) -> anyhow::Result<()> {
        refuse_unknown_collations(conn, declared).await
    }

    async fn refuse_history_references_alike(
        &self,
        conn: &mut Conn,
        declared: &Schema,
    ) -> anyhow::Result<()> {
        refuse_history_references_alike(conn, declared, self).await
    }

    fn reconcile_cluster_roles(
        &self,
        _recorded: &IdsFile,
        _queried: &IdsFile,
        _renames: &BTreeMap<pbps_model::RoleName, pbps_model::RoleName>,
        _live: &Schema,
        _expected: &mut Schema,
    ) -> anyhow::Result<ConnectedCheck> {
        Ok(reconcile_cluster_roles())
    }

    /// Adds to `hints` the facts about the target that planning a type or an
    /// extension change needs and the differ cannot see.
    ///
    /// SQL Server has no such types or extensions, so it adds nothing.
    async fn type_rebuild_hints(
        &self,
        _conn: &mut Conn,
        _base: &Schema,
        _declared: &Schema,
        _recorded_ids: &IdsFile,
        _resolved_ids: &IdsFile,
        _hints: &mut pbps_model::Hints,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn computed_column_screen(&self) -> pbps_diff::Screen {
        computed_column_screen(Driver::Mssql)
    }

    /// Reads the `NOT VALID` checks of the domains standing in the database.
    ///
    /// SQL Server has no domains, so it answers none.
    async fn standing_not_valid_domain_checks(
        &self,
        _conn: &mut Conn,
    ) -> anyhow::Result<Vec<StandingNotValid>> {
        Ok(Vec::new())
    }

    async fn seal_plan_type_review(
        &self,
        _conn: &mut Conn,
        _base: pbps_diff::Side<'_>,
        _desired: pbps_diff::Side<'_>,
        _changes: &ChangeSet,
        trusted: &BTreeSet<pbps_model::RoleName>,
        pin_routines: bool,
    ) -> anyhow::Result<bool> {
        let pins = seal_type_pins(trusted)?;
        Ok(!type_input_routines(&pins.types, pin_routines)?.is_empty())
    }
}
