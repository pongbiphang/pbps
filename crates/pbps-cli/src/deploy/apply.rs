//! `apply`: the transactional path, its preflight, and what both apply paths
//! share around the statements they run.

use super::*;

/// What the operator asked `apply` to do, as typed.
pub struct ApplyRequest<'a> {
    pub plan_path: &'a std::path::Path,
    pub approved_checksum: &'a str,
    pub allow: &'a std::collections::BTreeSet<pbps_model::RiskClass>,
    pub staged: bool,
    pub resume: bool,
}

/// `pbps apply` — run an approved plan (SPEC §7.3, §7.5).
pub fn cmd_apply(
    project: &Project,
    target: &Target,
    request: &ApplyRequest<'_>,
) -> anyhow::Result<()> {
    let operator = crate::operator(project.root())?;
    let dialect = crate::dialect(project)?;
    let plan_path = request.plan_path;

    // Before this point there is no artifact to report on: the attempt event
    // carries a checksum, and a file that cannot be read or is not a plan has
    // none. From `plan_checksum` on, every outcome is an attempt on this
    // artifact against this environment, and the hook sees all of them.
    let raw = std::fs::read_to_string(plan_path)
        .with_context(|| format!("cannot read `{}`", plan_path.display()))?;
    let plan = crate::saved_plan::decode(&raw, plan_path)?;
    let plan_checksum = plan.wire_fingerprint();

    let attempt = apply_identified(
        project,
        target,
        request,
        &plan,
        &plan_checksum,
        &dialect,
        &operator,
    );
    let (error, released) = match attempt {
        Ok(Attempt::Empty) => {
            println!("The plan is empty; nothing to apply.");
            return Ok(());
        }
        Ok(Attempt::Applied { entry, released }) => {
            println!(
                "Applied {} change(s) to `{}`{}; recorded as entry #{entry}.",
                plan.changes.changes.len(),
                pbps_model::escape::Escaped(&target.label),
                if request.staged { " (staged)" } else { "" }
            );
            // Preserve the original public hook contract: successful applies
            // receive the exact approved plan JSON. Attempt events use their
            // own key so existing success-only integrations cannot be invoked
            // on a failure or misread an unrelated payload shape.
            if let Some(hook) = &project.config.hooks.on_apply {
                crate::hooks::run(hook, &raw, "on_apply", crate::hooks::HookStdout::Shared);
            }
            if let Some(hook) = &project.config.hooks.on_apply_attempt {
                crate::hooks::run_apply_attempt(
                    hook,
                    plan_path,
                    &plan_checksum,
                    &target.label,
                    Some(entry),
                    None,
                );
            }
            // Report the deployment before surfacing cleanup. The lock may
            // need an operator to clear it, but retrying this already-recorded
            // plan would be the wrong response.
            released?;
            return Ok(());
        }
        Ok(Attempt::Failed { error, released }) => (error, Some(released)),
        // Refused before the lock was taken, or the connection or the lock
        // itself failed: still an attempt on this artifact, with no lock to
        // clean up.
        Err(error) => (error, None),
    };
    if let Some(hook) = &project.config.hooks.on_apply_attempt {
        // `ledger_safe_reason`, not `error.to_string()`: the hook's own doc
        // comment calls this payload "stable input" a `pbps.yml` author wires
        // to an arbitrary shell command, which can relay it anywhere — a
        // wider and less accountable audience than the operator's own
        // terminal, and exactly the "emitted outward" case DECISIONS 455
        // exists for, beside the ledger it names directly.
        let message = crate::engine::ledger_safe_reason(&error);
        crate::hooks::run_apply_attempt(
            hook,
            plan_path,
            &plan_checksum,
            &target.label,
            None,
            Some(&message),
        );
    }
    // After the hook, so the payload it receives is the deployment's error
    // alone; before the return, because a lock this run could not clear is
    // the first thing the next run will hit.
    if let Some(released) = &released {
        warn_unreleased(&target.label, released);
    }
    Err(error)
}

/// Best-effort audit row for an apply that did not complete. The schema and ids
/// come from the newest trustworthy checkpoint; for a transactional apply that
/// is the unchanged pre-plan state, and for a staged apply it is the last
/// completed statement. A failure to write the audit must never replace the
/// original deployment error.
pub(super) async fn record_failed_apply<D: crate::emission::Engine>(
    conn: &mut Conn,
    d: &Deployment<'_, D>,
    error: &anyhow::Error,
) {
    let current = match crate::engine::latest(conn).await {
        Ok(Some(entry)) => entry.snapshot,
        Ok(None) | Err(pbps_db::LedgerError::NotInitialized) => return,
        Err(audit_error) => {
            eprintln!("warning: the failed apply could not be added to the ledger: {audit_error}");
            return;
        }
    };
    let failed = failed_apply_snapshot(
        conn.driver(),
        current,
        d.operator,
        d.plan
            .git_sha
            .clone()
            .or_else(|| db::git_sha(d.project.root())),
        d.plan_checksum,
        error,
    );
    match crate::engine::record(conn, &failed).await {
        Ok(id) => eprintln!("Apply failure recorded as ledger entry #{id}."),
        Err(audit_error) => {
            eprintln!("warning: the failed apply could not be added to the ledger: {audit_error}")
        }
    }
}

/// Turns the newest trustworthy state into a failed-attempt audit row.
///
/// An unfinished staged state belongs to the plan that produced its progress.
/// A later attempt with another plan may fail before running anything, but it
/// must not relabel that checkpoint with the attempted plan's checksum or git
/// revision: doing so could let the next `--resume` skip statements from the
/// wrong artifact. The operator and reason still describe the failed attempt.
///
/// `reason` is [`crate::engine::ledger_safe_reason`], not `error.to_string()`:
/// this row is durable — SPEC's own audit trail — and a driver's own sentence
/// can name a value nobody declared to this tool (DECISIONS 455). The
/// operator still sees the whole thing; only what gets written down is cut.
pub(super) fn failed_apply_snapshot(
    driver: pbps_db::Driver,
    mut current: StateSnapshot,
    operator: &str,
    attempted_git_sha: Option<pbps_model::Text>,
    attempted_plan_checksum: &str,
    error: &anyhow::Error,
) -> StateSnapshot {
    let is_staged_checkpoint = current.staged.is_some();
    current.kind = StateKind::Failed;
    current.data_work = None;
    // The newest row's assertions passed for the plan that wrote it; this row
    // records a failed attempt and must not attribute them to another plan.
    current.target_assertions.clear();
    current.operator = operator.to_owned();
    if !is_staged_checkpoint {
        current.git_sha = attempted_git_sha;
        current.plan_checksum = Some(attempted_plan_checksum.to_owned());
    }
    current.reason = Some(
        crate::engine::truncate_reason(driver, &crate::engine::ledger_safe_reason(error)).into(),
    );
    current
}

/// Everything one `apply` carries from its command body into the locked section.
///
/// Bundled rather than passed positionally because two of these are `&str` —
/// the plan's checksum and the operator's name — and they were adjacent
/// arguments to both functions below. Transposing them compiles, and what it
/// produces is a ledger entry whose `plan_checksum` is a username: the pin that
/// `apply --plan` exists to enforce, silently recording the wrong thing.
///
/// A struct does not make that *unrepresentable* — both fields are still
/// `&str`, and `plan_checksum: &operator` would still compile. What it does is
/// put the field name beside the value at the one call site, so the mistake has
/// to be written down rather than counted to. Nine positional arguments is a
/// place where it can be made by counting, which is what the two
/// `#[allow(clippy::too_many_arguments)]` this replaces were saying.
pub(super) struct Deployment<'a, D: crate::emission::Engine> {
    pub(super) project: &'a Project,
    pub(super) target: &'a Target,
    pub(super) plan: &'a pbps_diff::order::AdmittedPlan<D>,
    pub(super) plan_checksum: &'a str,
    pub(super) statements: &'a crate::emission::LoweredPlan,
    pub(super) rename_targets: &'a [pbps_db::impact::RenameTarget],
    pub(super) dialect: &'a D,
    pub(super) operator: &'a str,
    pub(super) progress: Option<&'a services::Progress>,
}

/// Everything between taking the lock and releasing it. Returns the ledger id.
/// `resolved` holds a resolved plan's checks, its key and cluster already
/// passed (#616).
pub(super) async fn apply_under_lock<D: crate::emission::Engine>(
    conn: &mut Conn,
    d: &Deployment<'_, D>,
    resolved: Option<&pbps_cli::resolver::apply::Guard<'_>>,
) -> anyhow::Result<Option<i64>> {
    // Destructured straight back into the names the body below already uses:
    // every field is a shared reference, so this copies nothing.
    let Deployment {
        project,
        target,
        plan,
        plan_checksum,
        statements,
        rename_targets,
        dialect,
        operator,
        progress,
    } = *d;
    let Some(mut entry) = crate::engine::latest(conn).await? else {
        bail!(
            "`{}` has a ledger but no entries; a plan cannot be pinned to a state that was never recorded.",
            pbps_model::escape::Escaped(&target.label)
        );
    };
    // First, before anything is read: in another zone every offset-less
    // value reads back as other text, so the drift check below would report
    // the zone as a change nobody made (DEC-1916.1).
    refuse_moved_time_zone(conn, plan, &target.label)?;
    refuse_mid_deployment(&entry, &target.label)?;
    // Under the lock, before anything runs: a table made at a history's name
    // since the plan was computed would be adopted, not refused (#1176).
    refuse_taken_history_names(conn, &plan.changes, &target.label).await?;
    // Nothing binds a saved plan to a server: asked again here (#1502).
    crate::engine::refuse_unsupported_temporal(conn, &plan.changes).await?;
    let original_ids = entry.snapshot.ids.clone();
    pbps_diff::drop_requirements::validate(&plan.changes, &original_ids, &plan.ids)
        .map_err(anyhow::Error::msg)?;
    let role_renames = crate::engine::external_role_renames(conn, &original_ids, &plan.ids)
        .await?
        .renames;
    let pinned_managed = pinned_managed(&entry.snapshot, &plan.changes, &plan.ids);
    if plan.changes.is_empty()
        && original_ids.roles == plan.ids.roles
        && original_ids.types == plan.ids.types
        && original_ids.extensions == plan.ids.extensions
    {
        // No statements still means replay of the approved type prerequisites
        // (DEC-2181.1). Reference-free no-ops keep their existing fast path.
        if !pinned_managed.references.is_empty()
            || !pinned_managed.declared_types.is_empty()
            || !plan.type_pins.is_empty()
            || !plan.provider_pins.is_empty()
            || !plan.declared_type_pins.is_empty()
        {
            crate::pins::check(
                conn,
                project,
                target,
                plan,
                &pinned_managed,
                crate::pins::Check::BeforeProbes,
            )
            .await?;
        }
        return Ok(None);
    }
    rename_snapshot_roles(&mut entry.snapshot, &role_renames);
    let externals = external_uids(&entry.snapshot, &plan.changes);
    let recorded_ids = role_scope(&entry.snapshot.ids, &plan.ids, dialect);
    let recorded_modules = managed_modules(Some(&entry.snapshot), None);
    // The scopes the closing read-back will use, expressed in the names the
    // database has *now* — the second projection is compared against that
    // read, so it has to ask it the same question.
    let planned_scopes = scopes_under(&plan.data, &plan.ids, &entry.snapshot.ids);
    // The database holds the labels it has now, which a rename of this plan
    // changes: the baseline is read under both (DEC-2346.2).
    let held_scopes = held_before_the_plan(
        plan,
        &entry.snapshot.schema,
        &entry.snapshot.ids,
        &planned_scopes,
    );
    let (scoped, mut before, unrevocable) = baseline_state(
        conn,
        &recorded_ids,
        &recorded_modules,
        &externals_under(&externals, &recorded_ids),
        target.catalog_policy(project.config.unmanaged),
        &pinned_baseline_scopes(
            &entry.snapshot.schema.data_scopes(),
            &planned_scopes,
            &plan.changes,
            &plan.ids,
            (&entry.snapshot.schema, &entry.snapshot.ids),
        ),
        &entry.snapshot.schema,
        &held_scopes,
        CatalogWarnings::for_progress(d.progress),
    )
    .await?;
    retain_managed_roles(&mut before, &plan.ids, dialect);
    trim_to_the_planned_scope(plan, &entry.snapshot.ids, &planned_scopes, &mut before);

    // The drift check, and the whole reason a coarse `--allow` is safe: this
    // plan is only valid against the environment it was computed against, down
    // to the identity mapping.
    let live = pbps_model::drift_checksum(&scoped.schema, &recorded_ids);
    if live != plan.baseline.checksum {
        bail!(
            "`{}` is no longer the database this plan was computed against.\n\
             plan baseline: {}\n\
             database now:  {live}\n\
             Something changed since the plan was approved. `pbps verify` shows what; \
             then recompute the plan with `pbps plan --db`.",
            pbps_model::escape::Escaped(&target.label),
            plan.baseline.checksum
        );
    }
    refuse_moved_database_collation(conn, plan, &target.label).await?;
    crate::row_security::check_dropped_policies(&plan.changes, &plan.ids, &scoped.schema)?;

    // And the same guard the recording commands use (110): a permission the
    // declarations cannot hold is invisible to the checksum above, so it
    // reaches here as a clean baseline, and the closing snapshot would write
    // the environment down without it. Refused before statement one, where a
    // refusal still costs nothing.
    refuse_unexpressible(&scoped, &target.label, "apply again")?;
    // Ownership is beside the checksum too (DECISIONS 371), so an `ALTER ...
    // OWNER TO` the grantee after planning reaches here as a clean baseline.
    // The same question `plan` asked, of the database as it stands now
    // (#692): a grant to a target's owner is a no-op no `REVOKE` undoes.
    crate::engine::owned_targets(
        conn.driver(),
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
    crate::engine::unrevocable_grants(conn.driver(), &plan.changes, &plan.ids, &unrevocable)?;

    // Before the probes, which run declared expressions and so the routines
    // they call (DEC-319.1). Every module the plan holds at any point of its
    // run is managed here, so its own creates, rebuilds and drops are the
    // read-back's to compare, not the pins'.
    crate::pins::check(
        conn,
        project,
        target,
        plan,
        &pinned_managed,
        crate::pins::Check::BeforeProbes,
    )
    .await?;

    let approved = crate::pins::Approved {
        project,
        target,
        plan,
        managed: &pinned_managed,
    };
    preflight(
        conn,
        dialect,
        plan,
        rename_targets,
        &scoped.schema,
        &recorded_ids,
        &approved,
        progress,
    )
    .await?;

    // Recorded when the plan was made and never repeated here: apply
    // acquires no scratch (#2379).
    if let Some(record) = &plan.rehearsal {
        services::notice(
            progress,
            "apply.progress",
            format!(
                "Rehearsal, recorded in the plan: {}",
                crate::explain::rehearsal_line(&crate::explain::rehearsal_detail(record))
            ),
        );
    }
    services::notice(
        progress,
        "apply.progress",
        format!("Applying {} statement(s)...", statements.len()),
    );
    let result = async {
        conn.begin(&dialect.transaction_framing(&pin_zone(conn)))
            .await?;
        let admission = approved.admission();
        crate::engine::rebuild_window(
            conn,
            &admission,
            pbps_diff::Side {
                schema: &entry.snapshot.schema,
                ids: &recorded_ids,
            },
            &plan.changes,
        )
        .await?;
        let retained = crate::engine::hold_retained(
            conn,
            &plan.changes,
            Some(&entry.snapshot.declared.overlay(&before)),
        )
        .await?;
        crate::engine::external_role_renames(conn, &original_ids, &plan.ids).await?;
        crate::engine::check_module_dependents(
            conn,
            plan,
            &scoped.schema,
            &plan.ids,
            &recorded_ids,
        )
        .await?;
        crate::engine::check_module_rebuilds(conn, &plan.changes, false).await?;
        recheck_enum_rebuilds(conn, &plan.changes, &recorded_ids, false).await?;
        recheck_domain_alterations(conn, &plan.changes, &recorded_ids).await?;
        recheck_extension_creates(conn, plan).await?;
        crate::engine::check_drop_blockers(
            conn,
            &plan.changes,
            &plan.ids,
            &scoped.schema,
            &recorded_ids,
        )
        .await?;
        let data_guard = crate::engine::prepare_data_writes(
            conn,
            &approved.admission(),
            &plan.changes,
            &entry.snapshot,
            &plan.ids,
        )
        .await?;
        // Again, because the probes ran in transactions of their own and a
        // routine could be replaced after them.
        crate::pins::check(
            conn,
            project,
            target,
            plan,
            &pinned_managed,
            crate::pins::Check::BeforeStatements { executed: &[] },
        )
        .await?;
        // Last before the first statement, inside the transaction and under
        // the lock: the target is still the one the evidence was sealed
        // against (#616).
        if let Some(resolved) = resolved {
            resolved.before_statements(conn).await?;
        }
        let domain_defaults = pbps_pg::type_refs::declared::capture_defaults(
            conn,
            &plan.declared_type_pins,
            &plan.changes,
            pbps_pg::pins::Within::CallersTransaction,
        )
        .await?;
        execute_statements(conn, statements, &data_guard, &approved, progress).await?;
        crate::engine::check_module_rebuilds(conn, &plan.changes, true).await?;
        crate::engine::refuse_unsafe_definers(
            conn,
            &crate::engine::written_routines(&plan.changes),
            crate::engine::Absent::Refused,
        )
        .await?;
        crate::engine::external_role_renames(conn, &original_ids, &plan.ids).await?;

        // What gets recorded is the database read back, not the plan applied to
        // the old state. Expressions come back in the engine's stored form, and
        // only that form compares equal on the next drift check (SPEC §8.2).
        // The rows under the plan's own scope, for the same reason it carries
        // its ids.
        //
        // Read-back and ledger insertion both happen before the commit, so a
        // failure in either rolls the DDL back as well — and so that no commit
        // opens a window another session could write a declared row or a
        // managed role's grants into, for this read to take in and record as
        // the plan's own result (DECISIONS 147).
        let mut after = managed_state(
            conn,
            &plan.ids,
            &modules_after(&entry.snapshot, &plan.changes, Settled::Whole),
            &externals_under(&externals, &plan.ids),
            target.catalog_policy(project.config.unmanaged),
            &plan.data,
            &Schema::default(),
            crate::engine::Read::InsideOwnTransaction,
            CatalogWarnings::for_progress(d.progress),
        )
        .await?;
        // A deferred trigger runs at COMMIT unless explicitly settled. Compare
        // its effects inside this transaction, after every planned write: an
        // earlier flush can refuse a constraint that the full plan satisfies,
        // while a later one records rows that the commit then changes (473).
        crate::pins::check(
            conn,
            project,
            target,
            plan,
            &pinned_managed,
            crate::pins::Check::BeforeRecording {
                executed: statements.statements(),
            },
        )
        .await
        .context("Nothing has been applied — the transaction was rolled back.")?;
        if crate::engine::settle_data_writes(conn).await? {
            let settled = managed_state(
                conn,
                &plan.ids,
                &modules_after(&entry.snapshot, &plan.changes, Settled::Whole),
                &externals_under(&externals, &plan.ids),
                target.catalog_policy(project.config.unmanaged),
                &plan.data,
                &Schema::default(),
                crate::engine::Read::InsideOwnTransaction,
                CatalogWarnings::for_progress(d.progress),
            )
            .await?;
            refuse_unexpressible(&settled, &target.label, "apply again")?;
            if settled.schema != after.schema {
                bail!(
                    "deferred constraint triggers changed the managed state after the plan's \
                     row checks. Nothing has been applied — the transaction was rolled back."
                );
            }
            after = settled;
        }
        refuse_recreated_tables(conn, &plan.changes, statements, &after.unmanaged)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "{e:#}\n\nNothing has been applied — the transaction was rolled back."
                )
            })?;
        // Everything this plan does not touch has to be what the baseline
        // held, down to the rows of a table it does touch that no change of
        // it names (DECISIONS 150, 153). The read above is what gets
        // recorded, so a change another session made between the two reads
        // would otherwise be blessed as this plan's result. And the same rule
        // the recording commands follow: a permission the declarations cannot
        // express stops a state being written down, whether it was there at
        // the start or arrived during the run (110).
        let verified =
            crate::engine::verify_retained(conn, retained.as_ref(), &after.schema).await?;
        refuse_pinned_movement(
            dialect,
            guard::Read::transactional(guard::Plan {
                changes: &plan.changes,
                ids: &plan.ids,
            })
            .with_retained(verified.as_ref())
            .with_domain_defaults(Some(&domain_defaults)),
            &before,
            &after.schema,
            &target.label,
        )
        .and_then(|()| refuse_unmet_public_execution(&plan.changes, &after, &target.label))
        .map_err(|e| {
            anyhow::anyhow!(
                "{e:#}\n\n\
                 Nothing has been applied — the transaction was rolled back. `pbps verify` \
                 shows what moved; then apply again."
            )
        })?;
        refuse_unexpressible(&after, &target.label, "apply again")?;
        // And a last time before recording: a routine replaced while the
        // statements ran was run by them, and the rollback is still available.
        crate::pins::check(
            conn,
            project,
            target,
            plan,
            &pinned_managed,
            crate::pins::Check::BeforeRecording {
                executed: statements.statements(),
            },
        )
        .await
        .map_err(|e| {
            anyhow::anyhow!("{e:#}\n\nNothing has been applied — the transaction was rolled back.")
        })?;
        // And the closing bindings, after the managed read-back has held the
        // plan's own records to what it said (DEC-1274.1, #616).
        if let Some(resolved) = resolved {
            resolved
                .before_recording(conn, &after.schema, &plan.ids)
                .await?;
        }
        // No plan can change those kinds yet, so the record keeps what it held of them
        // (DEC-2721.1).
        after.schema.carry_unidentified_from(&entry.snapshot.schema);
        let mut snapshot = pbps_model::StateSnapshot::new(
            pbps_model::StateKind::Apply,
            after.schema,
            // The plan's mapping, not the baseline's: after a rename the two
            // disagree, and recording the old one would say the rename never
            // happened — leaving the next plan to propose it all over again.
            plan.ids.clone(),
            operator,
        );
        snapshot.baseline_evidence = after.baseline_evidence;
        snapshot.module_deps = plan.module_deps.clone();
        snapshot.git_sha = plan.git_sha.clone().or_else(|| db::git_sha(project.root()));
        snapshot.plan_checksum = Some(plan_checksum.to_owned());
        snapshot.target_assertions = plan.target_assertions.clone();
        // What the previous state declared, advanced by what this plan wrote:
        // from the plan alone, since `apply --plan` needs nothing else.
        snapshot.declared = entry.snapshot.declared.clone();
        snapshot
            .declared
            .advance_with_partitions(&plan.changes, &snapshot.schema);
        let id = crate::engine::record(conn, &snapshot).await?;
        crate::pins::check(
            conn,
            project,
            target,
            plan,
            &pinned_managed,
            crate::pins::Check::BeforeCommit {
                executed: statements.statements(),
            },
        )
        .await
        .context("Nothing has been applied — the transaction was rolled back.")?;
        Ok::<_, anyhow::Error>(id)
    }
    .await;
    finish_transaction(conn, dialect, result).await.map(Some)
}

/// Every module the plan holds at some point of its run: what the ledger
/// recorded, and what the plan leaves or passes through.
///
/// The routine pins leave these to the drift check and the read-back
/// (DEC-319.1). The union is what makes the set stable across a run: a routine
/// the plan creates is in it before it exists, and one it drops stays in it
/// after it is gone, so neither reads as an unmanaged routine arriving or
/// leaving.
pub(super) fn pinned_managed(
    recorded: &StateSnapshot,
    changes: &pbps_model::ChangeSet,
    desired: &IdsFile,
) -> crate::pins::Managed {
    let approved_base = pbps_diff::identity::type_refs::before_changes(desired, changes);
    let base_ids = type_scope(&recorded.ids, &approved_base);
    let mut set = managed_modules(Some(recorded), None);
    set.extend(modules_after(recorded, changes, Settled::SoFar));
    // Existing-column operations carry no type. A missing saved pin must
    // still refuse, rather than erase a baseline reference from admission.
    let mut references = crate::pins::external_references(&recorded.schema, &base_ids);
    // A column a staged rebuild has already moved holds the replacement, which
    // is the declared type's own until the swap names it back (DEC-1635.7).
    for planned in &changes.changes {
        if let pbps_model::Change::EnumRebuild { tmp, .. } = &planned.change {
            references.remove(tmp);
        }
    }
    crate::pins::Managed {
        modules: set,
        references,
        declared_types: crate::pins::declared_requirements(&recorded.schema, &base_ids),
    }
}

/// The cluster already moved these identities. Both the planner and the
/// checkout-free apply must scope and pin the same names (DECISIONS 377).
pub(super) fn rename_snapshot_roles(
    snapshot: &mut StateSnapshot,
    renames: &BTreeMap<pbps_model::RoleName, pbps_model::RoleName>,
) {
    for (from, to) in renames {
        snapshot.ids.rename_role(from, to);
        snapshot.schema.rename_role(from, to);
    }
}

/// Incoming type declarations belong in the queried and pinned baseline even
/// before their UIDs have reached the ledger. Existing names keep their
/// baseline UID; only their post-apply identity changes when a UID is replaced.
pub(super) fn type_scope(recorded: &IdsFile, declared: &IdsFile) -> IdsFile {
    let mut scoped = recorded.clone();
    for (uid, name) in &declared.types {
        if !scoped.types.contains_key(uid)
            && matches!(
                TypeResolver::new(&scoped, declared).resolve(TypeSide::Base, name),
                TypeIdentity::ExternalOrUnrecorded(_)
            )
        {
            scoped.types.insert(uid.clone(), name.clone());
        }
    }
    scoped
}

pub(super) fn role_scope(recorded: &IdsFile, declared: &IdsFile, dialect: &dyn Dialect) -> IdsFile {
    let mut scoped = type_scope(recorded, declared);
    if !dialect.manages_roles() {
        for (uid, name) in &declared.roles {
            if !scoped.roles.values().any(|old| old == name) {
                scoped.roles.insert(uid.clone(), name.clone());
            }
        }
    }
    scoped
}

/// A role removed from a PostgreSQL declaration becomes unmanaged, rather
/// than being dropped from the cluster. Closing comparisons use that scope.
pub(super) fn retain_managed_roles(schema: &mut Schema, ids: &IdsFile, dialect: &dyn Dialect) {
    if !dialect.manages_roles() {
        schema
            .roles
            .retain(|name, _| ids.roles.values().any(|kept| kept == name));
    }
}

/// A staged apply: one logical change, run statement by statement outside a
/// transaction, with each completion recorded (ADR-0003 decision 2).
///
/// # Why the ledger is written between statements
///
/// Nothing here rolls back — that is the whole reason the plan is staged. So
/// the only thing that can make a mid-way failure visible rather than
/// mysterious is a record written as each statement completes; and once that
/// record exists, `--resume` has somewhere honest to start from.
///
/// # Why resume re-checks the database
///
/// A checkpoint says what the database looked like when the run stopped. Half a
/// deployment sitting in an environment is exactly when somebody reaches in by
/// hand, so the drift discipline applies to a half-finished plan as much as to
/// a finished one: the live state has to still equal the checkpoint, or the
/// remaining statements are being run against something nobody planned for.
/// The census again, where the plan runs: a dependent that appeared on an
/// enum after the plan was made is refused by name before the first statement
/// (DEC-1635.7). Inside the apply's own transaction, after the lock.
pub(super) async fn recheck_enum_rebuilds(
    conn: &mut Conn,
    changes: &pbps_model::ChangeSet,
    stored: &IdsFile,
    resuming: bool,
) -> anyhow::Result<()> {
    crate::engine::recheck_enum_rebuild_census(conn, changes, stored, resuming).await
}

/// The tightened domains again, where the plan runs: a column of an array or
/// a composite of the domain that appeared after the plan was made is refused
/// by name before the first statement (DEC-1635.8).
pub(super) async fn recheck_domain_alterations(
    conn: &mut Conn,
    changes: &pbps_model::ChangeSet,
    stored: &IdsFile,
) -> anyhow::Result<()> {
    crate::engine::check_domain_alterations(conn, changes, stored)
        .await
        .map(|_| ())
}

/// Refuses to act on an environment that is half-way through a staged apply.
///
/// Planning or applying anything else on top of an unfinished staged plan
/// builds on a state nobody approved: the recorded baseline is a checkpoint,
/// not a deployment anybody signed off.
pub(super) fn refuse_mid_deployment(
    entry: &pbps_db::LedgerEntry,
    label: &str,
) -> anyhow::Result<()> {
    let Some(progress) = &entry.snapshot.staged else {
        return Ok(());
    };
    bail!(
        "`{label}` is mid-deployment: entry #{} is a staged checkpoint ({} of {} statements).\n\
         Finish it with `pbps apply --staged --resume --plan ...`, or take the database as it \
         stands with `pbps baseline --reason ...`.",
        entry.id,
        progress.completed,
        progress.total,
        label = pbps_model::escape::Escaped(label),
    )
}

/// The pre-flight of §7.5: dependency impact, then probes against the data.
/// The members each dropped role is expected to still have when the plan's
/// next statement runs: every listed member before statement one, fewer
/// once some of its `DROP MEMBER` statements have committed, and no
/// expectation at all once the `DROP ROLE` has. Counted off the emitter's
/// own statements, in the emitter's order (one `DROP MEMBER` per listed
/// member, then the `DROP ROLE`), so a resume asks about the role as the
/// plan left it and not as the plan first saw it (DECISIONS 102).
pub(super) fn role_drop_expectations<D: crate::emission::Engine>(
    plan: &(dyn pbps_diff::order::PlanView<D> + 'static),
    dialect: &D,
    mode: pbps_model::PlanMode,
    completed: usize,
) -> anyhow::Result<Vec<(pbps_model::RoleName, Vec<pbps_model::RoleName>)>> {
    let cs = pbps_dialect::PlanPayload::changes(plan);
    let mut out = Vec::new();
    let mut at = 0usize;
    let emit = crate::emitter(dialect, plan, mode);
    for (position, p) in cs.changes.iter().enumerate() {
        let n = emit(position)?.len();
        if let pbps_model::Change::DropRole { name, members, .. } = &p.change {
            let done = completed.saturating_sub(at).min(n);
            if done <= members.len() {
                out.push((name.clone(), members[done..].to_vec()));
            }
        }
        at += n;
    }
    Ok(out)
}

/// Refuses, before anything runs, a role drop whose role is not the one the
/// plan was made against: a member the plan did not list, a listed member
/// no longer there, or a securable it has come to own. A staged apply would
/// otherwise commit every `DROP MEMBER` the reviewer saw and then fail on
/// the one nobody did, leaving the reviewed users without access and the
/// role in place (DECISIONS 92, 102).
pub(super) async fn check_role_drops(
    conn: &mut Conn,
    expected: &[(pbps_model::RoleName, Vec<pbps_model::RoleName>)],
) -> anyhow::Result<()> {
    if expected.is_empty() {
        return Ok(());
    }
    let members_now = crate::engine::role_members(conn)
        .await
        .context("cannot read the role memberships")?;
    let owned_now = crate::engine::role_owned_securables(conn)
        .await
        .context("cannot read what the roles own")?;
    for (name, listed) in expected {
        let listed: std::collections::BTreeSet<&pbps_model::RoleName> = listed.iter().collect();
        let now: std::collections::BTreeSet<&pbps_model::RoleName> = members_now
            .get(name)
            .map(|m| m.iter().collect())
            .unwrap_or_default();
        let added: Vec<&pbps_model::RoleName> = now.difference(&listed).copied().collect();
        let gone: Vec<&pbps_model::RoleName> = listed.difference(&now).copied().collect();
        if !added.is_empty() || !gone.is_empty() {
            let mut detail = Vec::new();
            if !added.is_empty() {
                detail.push(format!(
                    "member(s) the plan did not list: {}",
                    added
                        .iter()
                        .map(|name| name.shown().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !gone.is_empty() {
                detail.push(format!(
                    "listed member(s) no longer in it: {}",
                    gone.iter()
                        .map(|name| name.shown().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            bail!(
                "role `{name}` is not the role this plan was made against: {}.\n\
                 Its membership changed after `plan --db`, and the plan lists who loses \
                 the role so a reviewer can see it. Recompute it with `pbps plan --db`.",
                detail.join("; "),
                name = name.shown(),
            );
        }
        if let Some(securables) = owned_now.get(name)
            && !securables.is_empty()
        {
            bail!(
                "role `{name}` cannot be dropped: it owns {}, which it did not when this \
                 plan was made.\n\
                 Move the ownership first (`ALTER AUTHORIZATION ON {} TO dbo;`, by hand, \
                 since pbps does not decide who owns a securable), then recompute the plan \
                 with `pbps plan --db`.",
                securables.join(", "),
                securables[0],
                name = name.shown(),
            );
        }
    }
    Ok(())
}

/// The zone a deployment over `conn` pins (DEC-1916.1): the one the session
/// started in.
///
/// At `plan --db` that is the zone the plan records. At `apply` it is the
/// zone the plan recorded, because [`refuse_moved_time_zone`] has refused
/// before the first statement unless the two are one. An engine with no
/// session zone ignores the argument, which is why UTC is no guess there.
pub(super) fn pin_zone(conn: &Conn) -> SessionZone {
    conn.session_zone()
        .cloned()
        .unwrap_or_else(SessionZone::utc)
}

/// The settings this dialect's statements depend on, established on the
/// connection.
///
/// Idempotent by construction — it is a list of `SET`s — and issued from more
/// than one place on purpose. See [`run_probes`], which does not trust its
/// caller to have called this.
pub(super) async fn pin_session(
    conn: &mut Conn,
    dialect: &dyn pbps_dialect::Dialect,
) -> anyhow::Result<()> {
    if let Some(pins) = dialect.session_pins(&pin_zone(conn)) {
        conn.execute(&pins)
            .await
            .context("the session settings this dialect's statements depend on could not be set")?;
    }
    Ok(())
}

pub(super) async fn drop_preflight(
    conn: &mut Conn,
    dialect: &dyn pbps_dialect::Dialect,
    changes: &pbps_model::ChangeSet,
    ids: &IdsFile,
    stored: &Schema,
    stored_ids: &IdsFile,
) -> anyhow::Result<()> {
    conn.begin(&dialect.transaction_framing(&pin_zone(conn)))
        .await?;
    let result = crate::engine::check_drop_blockers(conn, changes, ids, stored, stored_ids).await;
    let rollback = conn
        .rollback(&dialect.transaction_framing(&pin_zone(conn)))
        .await;
    result?;
    rollback?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn preflight<D: crate::emission::Engine>(
    conn: &mut Conn,
    dialect: &D,
    plan: &pbps_diff::order::AdmittedPlan<D>,
    rename_targets: &[pbps_db::impact::RenameTarget],
    stored: &pbps_model::Schema,
    stored_ids: &IdsFile,
    approved: &crate::pins::Approved<'_>,
    progress: Option<&services::Progress>,
) -> anyhow::Result<()> {
    // **The pins before the questions.** Every read below — the edition, the
    // role checks, the rename impact scans and the probes — used to run under
    // whatever settings the operator's own session happened to carry, while
    // the statements they clear ran under the pinned ones, because the framing
    // that pins them is opened *after* this function returns and the staged
    // path sets them after this call. Measured on 18.6, that gap refuses valid
    // plans in three separate ways (DECISIONS 415).
    //
    // Here rather than at the two call sites: a caller that forgets is a
    // caller that gets the old bug back, and this function is the one whose
    // answers depend on it.
    pin_session(conn, dialect).await?;
    drop_preflight(conn, dialect, &plan.changes, &plan.ids, stored, stored_ids).await?;
    pbps_diff::drop_requirements::validate(&plan.changes, stored_ids, &plan.ids)
        .map_err(anyhow::Error::msg)?;
    // Asked again, as the edition is below: a table outside the declarations
    // can gain a key to the parent between the plan and this apply, and the
    // engine takes the unlogged partition under it all the same (#1595).
    crate::engine::refuse_unlogged_partition_referencers(
        conn,
        &plan.changes,
        &plan.ids,
        plan.mode.is_staged(),
    )
    .await?;
    // A publication can list the table between the plan and this apply (#1489).
    crate::engine::refuse_published_unlogged(conn, &plan.changes, &plan.ids).await?;
    // The edition, asked again. `plan --db` checked it, but nothing binds a
    // saved plan to an environment: the same file can be applied to a different
    // server, or to the same one after an edition change, and an `ONLINE = ON`
    // statement rejected halfway through an apply is precisely what the check
    // at plan time exists to prevent (ADR-0003).
    let verdict = crate::engine::edition_verdict(conn, &plan.changes).await?;
    if !verdict.refused_online.is_empty() {
        bail!(
            "this plan carries `strategy: online` for {}, and this server runs {}, which has no \
             online index operations.\n\
             The plan was approved against an edition that supports it. Recompute it against \
             this environment with `pbps plan --db`.",
            verdict
                .refused_online
                .iter()
                .map(|subject| pbps_model::escape::Escaped(subject).to_string())
                .collect::<Vec<_>>()
                .join(", "),
            verdict.runs
        );
    }

    // A dropped role's members were listed at plan time, and the ledger's
    // checksum cannot see a membership change — membership is each
    // environment's own and outside the managed state on purpose. Read
    // again here, before statement one (DECISIONS 92), and again on a
    // resume, for the members whose statements have not run yet (102).
    check_role_drops(conn, &role_drop_expectations(plan, dialect, plan.mode, 0)?).await?;
    // The same for a name this plan needs free: a user created since the
    // plan is outside the managed state, so the checksum above cannot see
    // it either, and `CREATE ROLE` would meet it after everything ordered
    // before had run (119).
    let (wanted, vacated) = role_name_expectations(plan, dialect, plan.mode, 0)?;
    refuse_taken_role_names(conn, &wanted, &vacated).await?;

    // A SCHEMABINDING referrer this plan is about to drop is not a blocker: the
    // module changes sort before the table changes precisely so that the drop
    // runs first (ADR-0002). The catalog is queried before anything executes, so
    // it still sees the dependency — and reporting it would refuse a plan whose
    // own first statement removes the obstacle.
    let dropped = dropped_referrer_names(&plan.changes);
    let dropped_modules = dropped_modules(&plan.changes);

    let mut blocked = Vec::new();
    for target in rename_targets {
        let mut report = crate::engine::rename_impact(conn, target).await?;
        excuse_dropped_referrers(&mut report, &dropped, &dropped_modules);
        if report.is_empty() {
            continue;
        }
        services::notice(
            progress,
            "apply.preflight",
            format!("\n{} {} affects:", target.verb(), report.target),
        );
        for line in impact_lines(
            &report,
            &crate::engine::impact_notes(conn.driver(), &report),
        ) {
            services::notice(progress, "apply.preflight", line);
        }
        for r in &report.blocking {
            blocked.push(format!("{} {}", r.kind, r.name));
        }
    }
    if !blocked.is_empty() {
        bail!(
            "the engine will refuse this rename while these exist: {}.\n\
             Drop or recreate them without SCHEMABINDING first.",
            blocked.join(", ")
        );
    }

    // The probes take the plan's changes, which name a parent it renames by
    // its new name, so the stored relation is brought under those names
    // (#1751 review).
    let names = pbps_model::plan_names::PlanNames::of(&plan.changes.changes, &plan.ids)?;
    let admission = approved.admission();
    run_probes_observed(
        conn,
        &admission,
        dialect,
        &names,
        &probe_partitions(stored, &names),
        Some(pbps_model::operation::Snapshot {
            schema: stored,
            ids: stored_ids,
        }),
        Some(approved),
        progress,
    )
    .await
}

/// The partitions the probes read the plan against: the relation as the
/// database stands before the plan, under the plan's final parent names. Every
/// caller of [`run_probes`] that probes a plan builds it here, so the plan-time
/// removed-label count and the apply-time probes see the same relation
/// (#2406).
pub(super) fn probe_partitions(
    stored: &pbps_model::Schema,
    names: &pbps_model::plan_names::PlanNames<'_>,
) -> pbps_model::Partitions {
    pbps_model::Partitions::of(stored).renamed(names)
}

/// The body of one rename's impact report: what breaks, what blocks, and what
/// the rename is carried into, each under its own wording.
///
/// `carried` used to be left out, so a PostgreSQL report holding only carried
/// objects printed an "affects:" heading with nothing under it, and one that
/// mixed them hid the half the engine handles (#307, DECISIONS 395). A carried
/// object is never described as breaking or blocking. The notes are the
/// engine's (`engine::impact_notes`), printed last: the list is of what the
/// catalog can see, and applications and downstream ELT are invisible to any
/// query.
pub(super) fn impact_lines(
    report: &pbps_db::impact::ImpactReport,
    notes: &[String],
) -> Vec<String> {
    let detail = |r: &pbps_db::impact::Referrer| {
        r.detail
            .as_deref()
            .map(|d| format!(" — {d}"))
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    for r in &report.advisory {
        out.push(format!("  {} {}{}", r.kind, r.name, detail(r)));
    }
    for r in &report.blocking {
        out.push(format!(
            "  {} {} — SCHEMABINDING, which blocks the rename",
            r.kind, r.name
        ));
    }
    for r in &report.carried {
        out.push(format!(
            "  {} — carried into the new name, keeps working{}",
            pbps_model::escape::Escaped(&r.name),
            detail(r)
        ));
    }
    out.extend(notes.iter().map(|n| format!("  {n}")));
    out
}

/// Commits only after every post-DDL operation succeeded; otherwise rolls back
/// while preserving the original error as the useful one.
///
/// Everything between the statements and the commit is inside the transaction,
/// so a failure there has to undo the statements too — an apply whose
/// read-back or ledger entry failed has changed nothing (147).
pub(super) async fn finish_transaction<T>(
    conn: &mut Conn,
    dialect: &dyn Dialect,
    result: anyhow::Result<T>,
) -> anyhow::Result<T> {
    let framing = dialect.transaction_framing(&pin_zone(conn));
    match result {
        Ok(value) => {
            if let Err(error) = conn.commit(&framing).await {
                let _ = conn.rollback(&framing).await;
                return Err(
                    anyhow::Error::new(error).context("the transaction could not be committed")
                );
            }
            Ok(value)
        }
        Err(error) => {
            let _ = conn.rollback(&framing).await;
            Err(error)
        }
    }
}

/// Stamps a snapshot with where it came from.
pub(super) fn with_provenance(
    root: &std::path::Path,
    mut snapshot: StateSnapshot,
) -> StateSnapshot {
    snapshot.git_sha = db::git_sha(root);
    snapshot
}

/// The modules this plan drops, spelled as the catalog spells a referrer.
///
/// `rename_impact` names a referrer `schema.name` from `sys.objects`, and a
/// trigger's `ModuleId` is `schema.table.name` — so the id's own string form
/// matched nothing, a trigger this plan drops before the rename stayed in the
/// report, and a schema-bound one refused the plan whose first statement
/// removes it. The object name is what the catalog has (ADR-0009 §1).
/// Leaves out of a rename's impact report the referrers this plan drops
/// first, before anything is counted or printed — so an empty remainder
/// prints no heading and the notes are computed from what is left.
///
/// Blocking and advisory referrers are matched by name, as they always were.
/// A carried one is matched only by the typed module the engine proved its
/// removal with: its display is the engine's description, so a name match
/// would never fire on it, and a looser one would excuse a same-named
/// survivor (#823).
pub(super) fn excuse_dropped_referrers(
    report: &mut pbps_db::impact::ImpactReport,
    dropped: &BTreeSet<String>,
    dropped_modules: &BTreeSet<(pbps_model::ModuleKind, pbps_model::ModuleId)>,
) {
    report.blocking.retain(|r| !dropped.contains(&r.name));
    report.advisory.retain(|r| !dropped.contains(&r.name));
    report.carried.retain(|r| {
        r.removed_with
            .as_ref()
            .is_none_or(|key| !dropped_modules.contains(key))
    });
}

/// Every module this plan drops outright, by its full typed identity. A
/// rebuild (`AlterModule`) keeps the module, so it is not here.
pub(super) fn dropped_modules(
    changes: &pbps_model::ChangeSet,
) -> BTreeSet<(pbps_model::ModuleKind, pbps_model::ModuleId)> {
    changes
        .changes
        .iter()
        .filter_map(|p| {
            if let pbps_model::Change::DropModule { id, kind } = &p.change {
                Some((*kind, id.clone()))
            } else {
                None
            }
        })
        .collect()
}

pub(super) fn dropped_referrer_names(changes: &pbps_model::ChangeSet) -> BTreeSet<String> {
    changes
        .changes
        .iter()
        .filter(|p| matches!(p.change, pbps_model::Change::DropModule { .. }))
        .filter_map(|p| p.change.module_id())
        .map(|id| id.object_name().raw().to_string())
        .collect()
}
