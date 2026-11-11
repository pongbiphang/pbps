//! PostgreSQL's positions of a connected plan.
//!
//! The bodies are the `Driver::Postgres` arms of the functions of
//! [`super::super`] that the pipeline used to call one by one. Those functions
//! keep their other callers (bootstrap, apply, the pins) and reach these bodies
//! through the free functions here, so each body is written once.

use super::*;

/// PostgreSQL's own wording, kept as it was, not the default's.
pub(super) const DEPENDENCIES_REFUSAL: &str =
    "PostgreSQL does not support cross-database dependencies";

/// What PostgreSQL carries from one position of a plan to a later one.
#[derive(Debug, Default)]
pub(crate) struct PlanState {
    /// Whether a routine a declared type's input path calls still needs review
    /// (sealed at the start, read by the misspelling probe, which would
    /// otherwise call that routine on a value it parses). Neutral code never
    /// reads it, so it is state and not a return value.
    type_routines_need_review: bool,
}

pub(in crate::engine) async fn seal_type_pins(
    conn: &mut Conn,
    schemas: &[&Schema],
    changes: &ChangeSet,
    trusted: &BTreeSet<pbps_model::RoleName>,
    pin_routines: bool,
) -> anyhow::Result<Prerequisites> {
    let pins = pbps_pg::type_refs::seal(conn, schemas, changes, trusted).await?;
    type_input_routines(conn, &pins, trusted, pin_routines, false).await?;
    let providers = pbps_pg::class_pins::seal(conn, schemas, changes).await?;
    Ok(Prerequisites {
        types: pins,
        providers,
    })
}

pub(in crate::engine) async fn seal_plan_type_pins(
    conn: &mut Conn,
    base: pbps_diff::Side<'_>,
    desired: pbps_diff::Side<'_>,
    changes: &ChangeSet,
    trusted: &BTreeSet<pbps_model::RoleName>,
    pin_routines: bool,
) -> anyhow::Result<Prerequisites> {
    let pins = pbps_pg::type_refs::seal_sides(conn, base, desired, changes, trusted).await?;
    type_input_routines(conn, &pins, trusted, pin_routines, false).await?;
    let providers =
        pbps_pg::class_pins::seal(conn, &[base.schema, desired.schema], changes).await?;
    Ok(Prerequisites {
        types: pins,
        providers,
    })
}

pub(in crate::engine) async fn type_input_routines(
    conn: &mut Conn,
    pins: &[pbps_model::TypePin],
    trusted: &BTreeSet<pbps_model::RoleName>,
    pin_routines: bool,
    inside: bool,
) -> anyhow::Result<BTreeSet<i64>> {
    let within = if inside {
        pbps_pg::pins::Within::CallersTransaction
    } else {
        pbps_pg::pins::Within::OwnTransaction
    };
    // Where each type is now: a placeholder names a type the plan itself
    // brings and is not read, and a type its package's move carried is
    // found under the name the move gave it (DEC-2447.12).
    let references = pbps_pg::type_refs::current_references(conn, within, pins).await?;
    let admission = pbps_pg::admission::Admission::new(trusted.iter().cloned(), pin_routines);
    Ok(pbps_pg::type_refs::input_routines(conn, within, &references, &admission).await?)
}

pub(in crate::engine) async fn external_role_renames(
    conn: &mut Conn,
    recorded: &IdsFile,
    declared: &IdsFile,
) -> anyhow::Result<ExternalRoleRenames> {
    let mut renames = BTreeMap::new();
    for (from, to) in recorded.role_renames(declared) {
        let evidence = pbps_pg::roles::rename_evidence(conn, from, to).await?;
        if let Some(error) = pbps_pg::roles::refuse_rename(from, to, evidence) {
            anyhow::bail!("rename_evidence (PostgreSQL): {error}");
        }
        renames.insert(from.clone(), to.clone());
    }
    let check = ConnectedCheck {
        name: "rename_evidence",
        engine: "PostgreSQL",
        status: "passed",
        message: format!(
            "{} external cluster-role rename(s) confirmed complete",
            renames.len()
        ),
    };
    Ok(ExternalRoleRenames { renames, check })
}

/// Reconcile only incoming and renamed cluster roles against the same read
/// used for the baseline checksum. A second presence query could approve a
/// role that arrived after that read, while the saved baseline still lacks it.
pub(in crate::engine) fn reconcile_cluster_roles(
    recorded: &IdsFile,
    queried: &IdsFile,
    renames: &BTreeMap<pbps_model::RoleName, pbps_model::RoleName>,
    live: &Schema,
    expected: &mut Schema,
) -> anyhow::Result<ConnectedCheck> {
    let mut checked = 0;
    for to in queried.roles.values().filter(|name| {
        renames.values().any(|to| to == *name) || !recorded.roles.values().any(|old| old == *name)
    }) {
        let Some(role) = live.roles.get(to) else {
            anyhow::bail!(
                "missing_roles (PostgreSQL): declared cluster role `{to}` is missing; create or rename it first, then plan again",
                to = to.shown(),
            );
        };
        expected.roles.insert(to.clone(), role.clone());
        checked += 1;
    }
    Ok(ConnectedCheck {
        name: "missing_roles",
        engine: "PostgreSQL",
        status: "passed",
        message: format!(
            "{checked} incoming or renamed cluster role(s) present in the queried baseline"
        ),
    })
}

impl PlanSession for pbps_pg::Postgres {
    type State = PlanState;

    async fn compare_dependencies(
        &self,
        _conn: &mut Conn,
        declarations: &pbps_model::dependency::Dependencies,
    ) -> anyhow::Result<pbps_model::dependency::PlannedDependencies> {
        if declarations.is_empty() {
            Ok(Default::default())
        } else {
            anyhow::bail!("{DEPENDENCIES_REFUSAL}")
        }
    }

    async fn seal_declared_type_pins(
        &self,
        state: &mut PlanState,
        conn: &mut Conn,
        declared: &Schema,
        trusted: &BTreeSet<pbps_model::RoleName>,
        pin_routines: bool,
    ) -> anyhow::Result<()> {
        let initial = seal_type_pins(
            conn,
            &[declared],
            &Default::default(),
            trusted,
            pin_routines,
        )
        .await?;
        state.type_routines_need_review =
            !type_input_routines(conn, &initial.types, trusted, pin_routines, false)
                .await?
                .is_empty();
        Ok(())
    }

    async fn external_role_renames(
        &self,
        conn: &mut Conn,
        recorded: &IdsFile,
        declared: &IdsFile,
    ) -> anyhow::Result<ExternalRoleRenames> {
        external_role_renames(conn, recorded, declared).await
    }

    async fn misspelt(
        &self,
        state: &PlanState,
        conn: &mut Conn,
        schema: &Schema,
        at: &CatalogNames,
    ) -> Result<Spellings, RowsError> {
        pbps_pg::catalog::misspelt(conn, schema, at, state.type_routines_need_review).await
    }

    async fn schema_spellings(
        &self,
        conn: &mut Conn,
        names: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, Option<String>>, DbError> {
        pbps_pg::catalog::schema_spellings(conn, names).await
    }

    /// PostgreSQL declares no column collation: `validate` refuses the field
    /// there, so there is nothing to ask the server.
    async fn refuse_unknown_collations(
        &self,
        _conn: &mut Conn,
        _declared: &Schema,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// PostgreSQL has no system-versioned table.
    async fn refuse_history_references_alike(
        &self,
        _conn: &mut Conn,
        _declared: &Schema,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Reconcile only incoming and renamed cluster roles against the same read
    /// used for the baseline checksum. A second presence query could approve a
    /// role that arrived after that read, while the saved baseline still lacks
    /// it.
    fn reconcile_cluster_roles(
        &self,
        recorded: &IdsFile,
        queried: &IdsFile,
        renames: &BTreeMap<pbps_model::RoleName, pbps_model::RoleName>,
        live: &Schema,
        expected: &mut Schema,
    ) -> anyhow::Result<ConnectedCheck> {
        reconcile_cluster_roles(recorded, queried, renames, live, expected)
    }

    /// A staged plan's table-local name facts, read beside its schema facts.
    /// Identities map a renamed attaching table back to its current catalog
    /// name. Preserve the project's dialect and ordinary planning's unknown
    /// server version.
    async fn staged_attach_dialect(
        &self,
        conn: &mut Conn,
        before: pbps_diff::Side<'_>,
        after: pbps_diff::Side<'_>,
    ) -> anyhow::Result<Self> {
        let attaching = after
            .ids
            .tables
            .iter()
            .filter_map(|(uid, name)| {
                let table = after.schema.tables.get(name)?;
                table.partition_of.as_ref()?;
                let live = before.ids.tables.get(uid)?;
                let standing = before.schema.tables.get(live)?;
                standing
                    .partition_of
                    .is_none()
                    .then(|| (uid.clone(), live.clone()))
            })
            .collect();
        Ok(crate::emission::Engine::with_attach_constraint_names(
            self,
            pbps_pg::staged::attach_constraint_names(conn, &attaching).await?,
        ))
    }

    /// Adds to `hints` the facts about the target that planning a type or an
    /// extension change needs and the differ cannot see: a rebuilt enum or
    /// domain keeps its comment, which `CREATE TYPE` does not copy, and which
    /// domains an array holds, which PostgreSQL refuses a new check on in place
    /// and a rebuild takes instead (DEC-1635.7, DEC-1635.9), and the extension
    /// facts (#2447). All are facts about the target, read before the differ
    /// writes the swap that carries them.
    async fn type_rebuild_hints(
        &self,
        conn: &mut Conn,
        base: &Schema,
        declared: &Schema,
        recorded_ids: &IdsFile,
        resolved_ids: &IdsFile,
        hints: &mut pbps_model::Hints,
    ) -> anyhow::Result<()> {
        let types: Vec<_> = base.types.keys().cloned().collect();
        hints.target_read = true;
        hints.enum_comments = pbps_pg::type_rebuild::comments(conn, &types).await?;
        let domains: Vec<_> = base
            .types
            .iter()
            .filter(|(_, ty)| matches!(ty, pbps_model::ManagedType::Domain { .. }))
            .filter_map(|(name, _)| Some((recorded_ids.type_uid(name)?.clone(), name.clone())))
            .collect();
        hints.domain_array_holders = pbps_pg::domain_alter::array_holders(conn, &domains).await?;
        hints.extension_requires =
            crate::deploy::extension_creates(conn, base, declared, resolved_ids).await?;
        hints.extension_updates =
            crate::deploy::extension_update_facts(conn, base, declared, recorded_ids, resolved_ids)
                .await?;
        hints.extension_moves =
            crate::deploy::extension_move_facts(conn, base, declared, recorded_ids, resolved_ids)
                .await?;
        hints.extension_drops = crate::deploy::extension_drop_facts(
            conn,
            &crate::deploy::retired_extensions(base, declared, recorded_ids, resolved_ids),
        )
        .await?;
        Ok(())
    }

    fn computed_column_screen(&self) -> pbps_diff::Screen {
        computed_column_screen(Driver::Postgres)
    }

    /// Asks the resolver's scratch to compare the expressions a plan retains,
    /// which the engine accepts by parse tree, and records the answers in
    /// `hints`.
    ///
    /// Only PostgreSQL retains expressions this way. Without a scratch entry
    /// there is nobody to ask, and a database that cannot be asked either is
    /// left unanswered.
    async fn retained_expression_hints(
        &self,
        scratch: Option<&mut crate::resolution::Entry<'_>>,
        conn: &mut Conn,
        target: &crate::db::Target,
        before: pbps_diff::Side<'_>,
        after: pbps_diff::Side<'_>,
        read_back: &Schema,
        hints: &mut pbps_model::Hints,
    ) -> anyhow::Result<()> {
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

    /// Reads the `NOT VALID` checks of the domains standing in the database.
    ///
    /// PostgreSQL has them.
    async fn standing_not_valid_domain_checks(
        &self,
        conn: &mut Conn,
    ) -> anyhow::Result<Vec<StandingNotValid>> {
        Ok(pbps_pg::domain_alter::not_valid(conn)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .into_iter()
            .map(|found| StandingNotValid {
                domain: found.domain,
                check: found.check,
                remedy: found.remedy,
            })
            .collect())
    }

    async fn seal_plan_type_review(
        &self,
        conn: &mut Conn,
        base: pbps_diff::Side<'_>,
        desired: pbps_diff::Side<'_>,
        changes: &ChangeSet,
        trusted: &BTreeSet<pbps_model::RoleName>,
        pin_routines: bool,
    ) -> anyhow::Result<bool> {
        let pins = seal_plan_type_pins(conn, base, desired, changes, trusted, pin_routines).await?;
        Ok(
            !type_input_routines(conn, &pins.types, trusted, pin_routines, false)
                .await?
                .is_empty(),
        )
    }
}
