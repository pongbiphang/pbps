//! Running a plan's probes and statements, and vetting extension changes
//! around them.

use super::*;

/// What this plan implies about the data, asked of the engine (SPEC §7.5).
///
/// **Pins its own session**, and does not take the caller's word for it. A
/// probe reads a rendering, parses a plan literal and parses the operator's own
/// declared expression, and all three are decided by settings the framing pins
/// — which used to be established only *after* this ran (DECISIONS 415). A
/// second `SET` batch costs nothing; a probe answered under the wrong settings
/// refuses a plan this engine takes.
/// `partitions` is the relation as the database stands before the plan: the
/// read whose checksum matched the plan's baseline (DEC-1699.1).
#[cfg(test)]
pub(super) async fn run_probes(
    conn: &mut Conn,
    dialect: &dyn pbps_dialect::Dialect,
    names: &pbps_model::plan_names::PlanNames<'_>,
    partitions: &pbps_model::Partitions,
    sources: Option<pbps_model::operation::Snapshot<'_>>,
    approved: Option<&crate::pins::Approved<'_>>,
) -> anyhow::Result<()> {
    run_probes_observed(
        conn,
        &pbps_pg::admission::Admission::default(),
        dialect,
        names,
        partitions,
        sources,
        approved,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_probes_observed(
    conn: &mut Conn,
    admission: &pbps_pg::admission::Admission,
    dialect: &dyn pbps_dialect::Dialect,
    names: &pbps_model::plan_names::PlanNames<'_>,
    partitions: &pbps_model::Partitions,
    sources: Option<pbps_model::operation::Snapshot<'_>>,
    approved: Option<&crate::pins::Approved<'_>>,
    progress: Option<&services::Progress>,
) -> anyhow::Result<()> {
    pin_session(conn, dialect).await?;
    // A recorded conversion's expression is judged before any probe evaluates
    // it: the probe would run non-built-in code as the deployer, unvetted.
    let mark = admission.mark();
    let immutable = crate::engine::vet_conversions(conn, admission, names.changes()).await?;
    crate::engine::vet_drop_requirements(conn, admission, names).await?;
    // The expressions the plan gives a table are evaluated by the probes too, so
    // they are judged first. One that is not admitted yet, or cannot be bound
    // before the plan creates what it uses, is left to the check just before its
    // statement; the probes that would run it are skipped, not run unjudged.
    let mut table_unjudged = false;
    if conn.driver() == pbps_db::Driver::Postgres {
        for planned in names.changes() {
            match crate::engine::vet_table_expressions(
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
    }
    if table_unjudged || admission.relied_since(mark) {
        // Plan time, and a conversion or requirement is admitted only by the
        // review: no probe evaluates it before the human has read the source.
        // Apply runs every probe after the approval (DEC-2563.2).
        services::notice(
            progress,
            "apply.preflight",
            "Pre-flight: the probes are unchecked until source review, because an expression calls a routine that only `pin_type_routines` admits, or that is not admitted yet; apply judges it before its statement and checks them."
                .to_owned(),
        );
        return Ok(());
    }
    let mut failures = Vec::new();
    let mut passed = 0usize;
    let mut unchecked = 0usize;
    let report = dialect.preflight_with_conversions(names, partitions, sources, &immutable);
    for skipped in &report.unchecked {
        unchecked += 1;
        eprintln!(
            "warning: could not check {} ({}); the engine will enforce it during the apply",
            skipped.description, skipped.reason
        );
    }
    for probe in &report.probes {
        // A probe can still legitimately fail to run — a check whose expression
        // names a column this plan renames, say, since expression text is never
        // rewritten by substitution. That is not a violation and it is not
        // silence either: the engine enforces the constraint inside the
        // transaction, where a failure rolls everything back.
        if let Some(approved) = approved {
            approved
                .check(conn, crate::pins::Check::BeforeProbes)
                .await?;
        }
        let answer = probe_query(conn, dialect, &probe.sql).await?;
        if let Some(approved) = approved {
            approved
                .check(conn, crate::pins::Check::BeforeProbes)
                .await?;
        }
        let rows = match answer {
            Ok(rows) => rows,
            // The server's text is left out: it quotes the value it could not
            // convert, and this failure may be recorded (DECISIONS 455).
            Err(e) if probe.refuses_when_it_fails => {
                let code = e.server_error_code();
                failures.push(format!(
                    "{} fails on a stored row (SQLSTATE {})",
                    probe.description,
                    code.as_deref().unwrap_or("unknown")
                ));
                continue;
            }
            Err(e) => {
                unchecked += 1;
                eprintln!(
                    "warning: could not check {} ({e}); the engine will enforce it during the apply",
                    probe.description
                );
                continue;
            }
        };
        // A query that answered but whose count cannot be read is in exactly
        // the same position as one that failed: nobody looked. Defaulting to
        // zero read that silence as "no rows violate this" — the one answer a
        // gate must never give by accident — and, worse, the probe was then
        // counted among those that *passed*.
        let count = match rows
            .first()
            .and_then(|r| r.try_get_at::<i32>(0).ok().flatten())
        {
            Some(c) => c,
            None => {
                unchecked += 1;
                eprintln!(
                    "warning: {} returned no readable count; the engine will enforce it during \
                     the apply",
                    probe.description
                );
                continue;
            }
        };
        passed += 1;
        if count > 0 {
            failures.push(format!("{count} {}", probe.description));
        }
    }
    if !failures.is_empty() {
        bail!(
            "the data will not accept this plan, and nothing has been changed:\n  {}",
            failures.join("\n  ")
        );
    }
    if passed > 0 {
        services::notice(
            progress,
            "apply.preflight",
            format!("Pre-flight: {passed} probe(s) passed against the live data."),
        );
    }
    if unchecked > 0 {
        // Said on stdout as well as in the warnings above: an operator reading
        // "3 probes passed" while a fourth went unchecked has been told
        // something true and something misleading in the same breath.
        services::notice(
            progress,
            "apply.preflight",
            format!(
                "Pre-flight: {unchecked} probe(s) could not be checked here; the engine enforces \
             them inside the transaction."
            ),
        );
    }

    // "No probe" is not "no risk". Saying so keeps the operator's attention
    // where the approval already put it, rather than letting a clean pre-flight
    // read as a clean bill of health.
    let unprobed: Vec<&str> = names
        .changes()
        .iter()
        .flat_map(|change| change.risks.iter().copied())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|r| {
            matches!(
                r,
                pbps_model::RiskClass::Rename | pbps_model::RiskClass::Destructive
            )
        })
        .map(|r| r.as_str())
        .collect();
    if !unprobed.is_empty() {
        services::notice(
            progress,
            "apply.preflight",
            format!(
                "Pre-flight: {} risk(s) cannot be probed and rest on the approval alone.",
                unprobed.join(", ")
            ),
        );
    }
    Ok(())
}

/// One probe, in the dialect's probe transaction when it has one.
///
/// The outer `Result` is the framing, the inner one the probe. A probe the
/// engine refuses — a `nextval` in a `CHECK`, under `READ ONLY` — is an
/// unchecked probe and the caller says so. A framing that cannot open or
/// close is not: a transaction left open would carry the plan's own `BEGIN`
/// inside it, where PostgreSQL only warns, so it stops the apply instead
/// (DECISIONS 537).
pub(super) async fn probe_query(
    conn: &mut Conn,
    dialect: &dyn pbps_dialect::Dialect,
    sql: &str,
) -> anyhow::Result<Result<Vec<pbps_db::Row>, pbps_db::DbError>> {
    let Some(framing) = dialect.probe_framing() else {
        return Ok(conn.query(sql).await);
    };
    conn.begin(&framing)
        .await
        .context("could not open the transaction a pre-flight probe runs in")?;
    let rows = conn.query(sql).await;
    conn.rollback(&framing)
        .await
        .context("could not close the transaction a pre-flight probe ran in")?;
    Ok(rows)
}

/// §7.5: a plan containing a non-transactional statement fails before it runs.
pub(super) fn reject_non_transactional(
    statements: &[pbps_dialect::Statement],
) -> anyhow::Result<()> {
    // The statement is SQL out of the plan's names, and a quoted identifier may
    // hold a line break or a terminal sequence; it is shown escaped, with the
    // layout around it the tool's own (DEC-2032.1, #2035).
    let offenders: Vec<String> = statements
        .iter()
        .filter(|s| !s.transactional)
        .map(|s| pbps_model::escape::Escaped(&s.sql).to_string())
        .collect();
    if offenders.is_empty() {
        return Ok(());
    }
    bail!(
        "{} statement(s) in this plan cannot run inside a transaction, and \"one plan, one \
         transaction\" is not negotiable:\n  {}\n\
         Isolate the change in a revision of its own and plan it with `pbps plan --db --staged`, \n\
         which runs it statement by statement with a checkpoint in the ledger (ADR-0003).",
        offenders.len(),
        offenders.join("\n  ")
    )
}

/// The statements as one transaction: all or nothing (SPEC §7.5), left
/// **open** for the caller.
///
/// The rollback is attempted on every failure path and its own error is
/// deliberately not allowed to replace the original one — the statement that
/// broke is what the operator needs to see.
///
/// What follows an apply is the read-back that becomes the recorded state and
/// the ledger entry that records it, and both belong inside this transaction.
/// Committing first left a window another session could write in — a declared
/// row edited, a managed role's grant changed — and the read-back would take
/// that in and record it as this plan's own result: `apply` reporting success,
/// `verify` clean against the newly blessed state, and only the next connected
/// plan proposing the declaration back (DECISIONS 147). It also makes the
/// ledger entry as atomic as the change it describes, where before a failure
/// to write it left the environment changed with nothing saying so.
///
/// [`finish_transaction`] is what closes it, on both paths.
pub(super) async fn execute_transaction_body(
    conn: &mut Conn,
    dialect: &dyn Dialect,
    statements: &crate::emission::LoweredPlan,
    type_pins: &crate::engine::Prerequisites,
    changes: &pbps_model::ChangeSet,
    trusted: &BTreeSet<pbps_model::RoleName>,
) -> anyhow::Result<()> {
    conn.begin(&dialect.transaction_framing(&pin_zone(conn)))
        .await
        .context("cannot open a transaction")?;
    crate::engine::recheck_type_prerequisites(conn, type_pins, changes, trusted, true).await?;
    for position in 0..statements.len() {
        crate::engine::recheck_type_prerequisites(conn, type_pins, changes, trusted, true).await?;
        execute_statement(
            conn,
            statements,
            position,
            &crate::engine::DataWriteGuard::default(),
            None,
            changes,
            trusted,
            None,
        )
        .await?;
    }
    Ok(())
}

pub(super) async fn execute_statements(
    conn: &mut Conn,
    statements: &crate::emission::LoweredPlan,
    data_guard: &crate::engine::DataWriteGuard,
    reviewed: &crate::pins::Approved<'_>,
    progress: Option<&services::Progress>,
) -> anyhow::Result<()> {
    for position in 0..statements.len() {
        execute_statement(
            conn,
            statements,
            position,
            data_guard,
            Some(reviewed),
            &reviewed.plan.changes,
            &reviewed.project.config.trusted_roles,
            progress,
        )
        .await?;
    }
    Ok(())
}

// Bootstrap checks prerequisites between statements without turning the
// certified collection into a raw slice that another caller could execute.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_statement(
    conn: &mut Conn,
    statements: &crate::emission::LoweredPlan,
    position: usize,
    data_guard: &crate::engine::DataWriteGuard,
    reviewed: Option<&crate::pins::Approved<'_>>,
    changes: &pbps_model::ChangeSet,
    trusted: &BTreeSet<pbps_model::RoleName>,
    progress: Option<&services::Progress>,
) -> anyhow::Result<()> {
    let stmt = &statements[position];
    if let Some(approved) = reviewed {
        approved
            .check(
                conn,
                crate::pins::Check::BeforeStatements {
                    executed: &statements[..position],
                },
            )
            .await
            .context("Nothing has been applied — the transaction was rolled back.")?;
    }
    let admission = reviewed.map_or_else(
        || pbps_pg::admission::Admission::without_review(trusted.iter().cloned()),
        crate::pins::Approved::admission,
    );
    crate::engine::check_data_write(conn, &admission, stmt, data_guard).await?;
    crate::engine::check_drop_requirement(conn, &admission, stmt).await?;
    // The routines a table expression calls are judged just before the statement
    // that introduces it, with what the plan created earlier in place (DEC-2563.4).
    if let Some(change) = &stmt.introduces {
        pbps_pg::table_expressions::vet_change(
            conn,
            &admission,
            pbps_pg::pins::Within::CallersTransaction,
            pbps_pg::table_expressions::Moment::Statement,
            change,
        )
        .await
        .and_then(pbps_pg::table_expressions::Vetted::admitted)
        .map_err(anyhow::Error::msg)?;
    }
    // A drop is vetted before it runs: what it would delete is judged while
    // it can still be refused, not after (DEC-2447.8).
    if let Some(extension) = &stmt.extension {
        vet_before_extension(conn, extension, reviewed.map(|a| a.plan)).await?;
    }
    if let Some(progress) = progress {
        progress.started.set(true);
    }
    if let Err(e) = conn.execute(&stmt.sql).await {
        // `.context()`, not `anyhow::anyhow!("...{e}")`: interpolating the
        // driver error into the message text would bake its `Display`
        // into an opaque string with no error left inside to find. Kept
        // as the source instead, `e` survives in this anyhow::Error's
        // chain, which is what lets `crate::engine::ledger_safe_reason`
        // downcast to `DbError::Driver` and redact only that frame later —
        // the server's sentence still reaches the operator through the
        // top-level context here plus `{:#}` at the CLI's own top level
        // (DECISIONS 455).
        return Err(anyhow::Error::new(e).context(format!(
            "the database rejected this statement, and the whole plan was rolled back:\n{}",
            stmt.sql
        )));
    }
    if let Some(type_statement) = &stmt.declared_type {
        pbps_pg::type_refs::declared::vet(
            conn,
            type_statement,
            &admission,
            reviewed.map_or(&[][..], |a| a.plan.declared_type_pins.as_slice()),
            changes,
            &statements[..=position],
        )
        .await?;
    }
    if let Some(extension) = &stmt.extension {
        vet_after_extension(conn, extension, reviewed.map(|a| a.plan)).await?;
    }
    Ok(())
}

/// What an extension statement is held to before it runs, by what it does:
/// a drop's manifest and reach (DEC-2447.8), an update's manifest, versions
/// and dependents (DEC-2447.10). An install is vetted after only.
pub(super) async fn vet_before_extension(
    conn: &mut Conn,
    extension: &pbps_dialect::ExtensionStatement,
    plan: Option<&pbps_model::SavedPlan>,
) -> anyhow::Result<()> {
    use pbps_dialect::ExtensionPhase;
    match &extension.phase {
        ExtensionPhase::Create => Ok(()),
        ExtensionPhase::Drop => {
            let pins = plan.map_or(&[][..], |p| p.extension_drops.as_slice());
            vet_before_extension_drop(conn, extension, pins).await
        }
        ExtensionPhase::Update { from } | ExtensionPhase::SetSchema { from } => {
            let pin = sealed_change(plan, extension, "vetted")?;
            let moving = matches!(extension.phase, ExtensionPhase::SetSchema { .. });
            pbps_pg::extension_provider::vet_before_change(
                conn,
                extension.name.raw(),
                pin,
                from,
                moving,
            )
            .await
            .map_err(|error| {
                anyhow::Error::new(error).context("nothing was changed and nothing was recorded")
            })
        }
    }
}

/// The pin a package change was sealed with, which every update and move of the
/// plan has (admission demands it, so a miss here is a plan that did not come
/// through it).
pub(super) fn sealed_change<'a>(
    plan: Option<&'a pbps_model::SavedPlan>,
    extension: &pbps_dialect::ExtensionStatement,
    what: &str,
) -> anyhow::Result<&'a pbps_model::ExtensionChangePin> {
    plan.into_iter()
        .flat_map(|p| p.extension_changes.iter())
        .find(|p| p.uid == extension.uid && p.name == extension.name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "extension `{}` has no pinned manifest in the plan, so its change cannot be {what}",
                extension.name.shown()
            )
        })
}

/// What it is held to right after, before its transaction commits.
pub(super) async fn vet_after_extension(
    conn: &mut Conn,
    extension: &pbps_dialect::ExtensionStatement,
    plan: Option<&pbps_model::SavedPlan>,
) -> anyhow::Result<()> {
    use pbps_dialect::ExtensionPhase;
    match &extension.phase {
        ExtensionPhase::Create => {
            let pins = plan.map_or(&[][..], |p| p.extension_pins.as_slice());
            vet_installed_extension(conn, extension, pins).await
        }
        ExtensionPhase::Drop => confirm_extension_dropped(conn, extension).await,
        ExtensionPhase::Update { .. } | ExtensionPhase::SetSchema { .. } => {
            let pin = sealed_change(plan, extension, "confirmed")?;
            let moving = matches!(extension.phase, ExtensionPhase::SetSchema { .. });
            pbps_pg::extension_provider::confirm_changed(conn, extension.name.raw(), pin, moving)
                .await
                .map_err(|error| {
                    anyhow::Error::new(error)
                        .context("the change was rolled back and nothing was recorded")
                })
        }
    }
}

/// Holds what a `CREATE EXTENSION` just installed to the manifest the plan was
/// approved over. Called before the transaction that ran it commits, so any
/// difference, unreadable read or missing pin takes the install back with it
/// and records nothing (DEC-2447.6).
pub(super) async fn vet_installed_extension(
    conn: &mut Conn,
    installed: &pbps_dialect::ExtensionStatement,
    pins: &[pbps_model::ExtensionPin],
) -> anyhow::Result<()> {
    let pin = pins
        .iter()
        .find(|p| p.uid == installed.uid && p.name == installed.name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "extension `{}` has no pinned manifest in the plan, so what it installed cannot be vetted",
                installed.name.shown()
            )
        })?;
    pbps_pg::extension_provider::vet(conn, installed.name.raw(), &pin.manifest)
        .await
        .map_err(|error| {
            anyhow::Error::new(error)
                .context("the install was rolled back and nothing was recorded")
        })
}

/// The check before a `DROP EXTENSION` runs, in the statement's own
/// transaction: the manifest and the objects outside it must be what the plan
/// was approved over, and a pin must exist (DEC-2447.8).
pub(super) async fn vet_before_extension_drop(
    conn: &mut Conn,
    dropped: &pbps_dialect::ExtensionStatement,
    pins: &[pbps_model::ExtensionDropPin],
) -> anyhow::Result<()> {
    let pin = pins
        .iter()
        .find(|p| p.uid == dropped.uid && p.name == dropped.name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "extension `{}` has no pinned manifest in the plan, so what its drop reaches cannot be vetted",
                dropped.name.shown()
            )
        })?;
    pbps_pg::extension_provider::vet_before_drop(conn, dropped.name.raw(), pin)
        .await
        .map_err(|error| {
            anyhow::Error::new(error).context("nothing was dropped and nothing was recorded")
        })
}

/// The check right after a `DROP EXTENSION`, before its transaction commits:
/// the package and everything reachable from it are gone, or the drop is taken
/// back with the rest (DEC-2447.8).
pub(super) async fn confirm_extension_dropped(
    conn: &mut Conn,
    dropped: &pbps_dialect::ExtensionStatement,
) -> anyhow::Result<()> {
    pbps_pg::extension_provider::confirm_dropped(conn, dropped.name.raw())
        .await
        .map_err(|error| {
            anyhow::Error::new(error).context("the drop was rolled back and nothing was recorded")
        })
}

/// The opening check of an apply that installs packages, under the lock: the
/// target must still be one the plan was made against, with the same sentences
/// the planner gives. Nothing has run when it refuses.
pub(super) async fn recheck_extension_creates(
    conn: &mut Conn,
    plan: &pbps_model::SavedPlan,
) -> anyhow::Result<()> {
    if !plan.extension_drops.is_empty() {
        // Each package this plan drops is still installed as it was pinned and
        // the role may still drop it: the planner's sentences, before a
        // statement runs.
        let names: BTreeSet<String> = plan
            .extension_drops
            .iter()
            .map(|p| p.name.raw().to_owned())
            .collect();
        let mut problems = pbps_pg::extension_provider::drop_problems(conn, &names).await?;
        let held = pbps_pg::extension_provider::installed(conn).await?;
        for pin in &plan.extension_drops {
            if held
                .get(pin.name.raw())
                .is_some_and(|d| *d != pin.definition)
            {
                problems.push(format!(
                    "extension `{}` is no longer installed as it was planned",
                    pin.name.shown()
                ));
            }
        }
        anyhow::ensure!(
            problems.is_empty(),
            "nothing was applied: {}",
            problems.join("; ")
        );
    }
    if !plan.extension_changes.is_empty() {
        // Each package this plan updates or moves is still installed as it was
        // pinned, the engine still has the path, and the role may do each, with
        // the planner's sentences.
        let (mut updates, mut moves) = (BTreeMap::new(), BTreeMap::new());
        for planned in &plan.changes.changes {
            if let pbps_model::Change::UpdateExtension { name, from, to, .. } = &planned.change {
                updates.insert(name.raw().to_owned(), (from.clone(), to.clone()));
            } else if let pbps_model::Change::SetExtensionSchema { name, from, to, .. } =
                &planned.change
            {
                moves.insert(name.raw().to_owned(), (from.clone(), to.clone()));
            }
        }
        let creating: BTreeMap<String, pbps_model::Extension> = plan
            .extension_pins
            .iter()
            .map(|p| (p.name.raw().to_owned(), p.definition.clone()))
            .collect();
        let mut problems =
            pbps_pg::extension_provider::update_problems(conn, &updates, &creating).await?;
        problems.extend(
            pbps_pg::extension_provider::move_problems(
                conn,
                &moves,
                &updates.keys().cloned().collect(),
            )
            .await?,
        );
        anyhow::ensure!(
            problems.is_empty(),
            "nothing was applied: {}",
            problems.join("; ")
        );
    }
    if plan.extension_pins.is_empty() {
        return Ok(());
    }
    let target = pbps_pg::extension_provider::target(conn).await?;
    let pinned: BTreeMap<String, pbps_model::Extension> = plan
        .extension_pins
        .iter()
        .map(|p| (p.name.raw().to_owned(), p.definition.clone()))
        .collect();
    let problems: Vec<String> = plan
        .extension_pins
        .iter()
        .flat_map(|p| {
            pbps_pg::extension_provider::create_problems(
                &target,
                p.name.raw(),
                &p.definition,
                &pinned,
            )
        })
        .collect();
    anyhow::ensure!(
        problems.is_empty(),
        "nothing was applied: {}",
        problems.join("; ")
    );
    Ok(())
}
