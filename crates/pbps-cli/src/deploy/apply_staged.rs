//! The staged apply path: statements run one at a time with the ledger
//! written between them, and a resumed deployment re-checked.

use super::*;

pub(super) async fn apply_staged_under_lock<D: crate::emission::Engine>(
    conn: &mut Conn,
    d: &Deployment<'_, D>,
    resume: bool,
) -> anyhow::Result<i64> {
    let Deployment {
        project,
        target,
        plan,
        plan_checksum,
        statements,
        rename_targets,
        dialect,
        operator,
        progress: _,
    } = *d;
    let staged = guard::Staged::new(guard::Plan {
        changes: &plan.changes,
        ids: &plan.ids,
    })?;
    let Some(mut entry) = crate::engine::latest(conn).await? else {
        bail!(
            "`{}` has a ledger but no entries; a plan cannot be pinned to a state that was never recorded.",
            pbps_model::escape::Escaped(&target.label)
        );
    };

    let original_ids = entry.snapshot.ids.clone();
    pbps_diff::drop_requirements::validate(&plan.changes, &original_ids, &plan.ids)
        .map_err(anyhow::Error::msg)?;
    let role_renames = crate::engine::external_role_renames(conn, &original_ids, &plan.ids)
        .await?
        .renames;
    rename_snapshot_roles(&mut entry.snapshot, &role_renames);
    let externals = external_uids(&entry.snapshot, &plan.changes);

    // Both branches read once and hand back what they validated: the
    // statement to start at, and the state the checkpoints are measured
    // against. Returned together so there is no way to reach the loop with a
    // baseline from some other read (DECISIONS 174).
    // A fresh run asks what a transactional one does; a resumed one created
    // the history itself, before its checkpoint (DEC-1176.1).
    // First, for a fresh run and a resumed one alike, as in
    // [`apply_under_lock`] (DEC-1916.1).
    refuse_moved_time_zone(conn, plan, &target.label)?;
    if !resume {
        refuse_taken_history_names(conn, &plan.changes, &target.label).await?;
        // Nothing binds a saved plan to a server: asked again here (#1502).
        crate::engine::refuse_unsupported_temporal(conn, &plan.changes).await?;
    }
    let (start, mut previous) = if resume {
        let progress = match (&entry.snapshot.staged, entry.snapshot.kind) {
            (Some(p), StateKind::Staged | StateKind::Failed) => p.clone(),
            _ => bail!(
                "`{}` has no staged apply in progress: its newest entry (#{}) is an ordinary \
                 {} state.\n\
                 Run `pbps apply --staged` without `--resume` to start this plan.",
                pbps_model::escape::Escaped(&target.label),
                entry.id,
                entry.snapshot.kind
            ),
        };
        if entry.snapshot.plan_checksum.as_deref() != Some(plan_checksum) {
            bail!(
                "entry #{} is a checkpoint of a different plan (checksum {}).\n\
                 Resume the plan that was interrupted, not this one.",
                entry.id,
                entry.snapshot.plan_checksum.as_deref().unwrap_or("none")
            );
        }
        // The drift check, applied to a half-finished plan. The mapping is the
        // checkpoint's own — which names every managed table as the catalog had
        // it at that moment, intermediate rename names included. Using the
        // plan's here instead would leave a table that is mid-rename out of both
        // sides, and a change someone made to it while the deployment was
        // paused would pass unseen into the closing entry.
        let after_modules = modules_after(&entry.snapshot, &plan.changes, Settled::SoFar);
        let at_checkpoint = &entry.snapshot.ids;
        // The state the checkpoints to come are measured against comes out of
        // *this* read — the one the comparison below validates — and not out
        // of a second one taken after these checks (DECISIONS 174).
        let (scoped, mut watching, unrevocable) = staged_baseline(
            conn,
            at_checkpoint,
            &after_modules,
            &after_modules,
            &externals_under(&externals, at_checkpoint),
            target.catalog_policy(project.config.unmanaged),
            &entry.snapshot.schema.data_scopes(),
            &entry.snapshot.schema,
            &held_before_the_plan(
                plan,
                &entry.snapshot.schema,
                at_checkpoint,
                &scopes_at(plan, at_checkpoint),
            ),
            CatalogWarnings::for_progress(d.progress),
        )
        .await?;

        trim_to_the_planned_scope(
            plan,
            at_checkpoint,
            &scopes_at(plan, at_checkpoint),
            &mut watching,
        );
        let live = pbps_model::drift_checksum(&scoped.schema, at_checkpoint);
        let checkpoint = pbps_model::drift_checksum(&entry.snapshot.schema, at_checkpoint);
        if live != checkpoint {
            bail!(
                "`{}` has moved since the checkpoint at entry #{}.\n\
                 checkpoint: {checkpoint}\n\
                 database:   {live}\n\
                 Either something changed while this deployment was half-finished, or the \
                 statement after the checkpoint committed and the checkpoint for it could not \
                 be written (a lost connection between the two). `pbps verify` shows which: if \
                 the difference is exactly the next statement of this plan, that statement is \
                 already done. Resuming cannot decide that for you — the remaining statements \
                 must not run against a database nobody planned for — so take the database as \
                 it stands with `pbps baseline --reason ...` and plan the rest from there.",
                pbps_model::escape::Escaped(&target.label),
                entry.id
            );
        }
        // Nor the database's default collation, which the plan pinned and
        // the checkpoint's checksum cannot see move (#1247 review): a
        // statement still to run that leaves a column with no collation
        // would take the new one.
        refuse_moved_database_collation(conn, plan, &target.label).await?;
        // Nor can it see a permission the declarations cannot hold, for the
        // same reason: it is beside the comparison, not in it (110). A
        // resume that ran on would close the deployment with a snapshot
        // written without it.
        refuse_unexpressible(&scoped, &target.label, "resume again")?;
        // And ownership, which is beside the checksum for the same reason
        // (#692), and whether this connection can revoke what is left to
        // revoke (#1057).
        crate::engine::owned_targets(
            conn.driver(),
            &plan.changes,
            &plan.ids,
            &scoped.owners,
            &scoped.session_role,
        )?;
        crate::engine::unrevocable_grants(conn.driver(), &plan.changes, &plan.ids, &unrevocable)?;
        // The role a `DROP ROLE` will meet is the role as the plan left it:
        // the members whose `DROP MEMBER` has not run yet, and nothing
        // owned. Membership and ownership are outside the checksum on
        // purpose, so the check above cannot see a member added while the
        // deployment was paused (DECISIONS 102).
        check_role_drops(
            conn,
            &role_drop_expectations(plan, dialect, plan.mode, progress.completed)?,
        )
        .await?;
        // And a principal created while it was paused, under a name a
        // remaining statement needs (119).
        let (wanted, vacated) =
            role_name_expectations(plan, dialect, plan.mode, progress.completed)?;
        refuse_taken_role_names(conn, &wanted, &vacated).await?;
        println!(
            "Resuming at statement {} of {} (checkpoint entry #{}).",
            progress.completed + 1,
            statements.len(),
            entry.id
        );
        if progress.total != statements.len() {
            bail!(
                "the checkpoint recorded {} statement(s) and this plan emits {}; they are not the \
                 same run.",
                progress.total,
                statements.len()
            );
        }
        (progress.completed, watching)
    } else {
        if entry.snapshot.staged.is_some() {
            bail!(
                "`{}` is mid-deployment: entry #{} is a staged checkpoint ({} of {} statements).\n\
                 Finish it with `pbps apply --staged --resume --plan ...`, or take the database \
                 as it stands with `pbps baseline --reason ...`.",
                pbps_model::escape::Escaped(&target.label),
                entry.id,
                entry.snapshot.staged.as_ref().map_or(0, |p| p.completed),
                entry.snapshot.staged.as_ref().map_or(0, |p| p.total)
            );
        }
        let recorded_ids = role_scope(&entry.snapshot.ids, &plan.ids, dialect);
        let recorded_modules = managed_modules(Some(&entry.snapshot), None);
        // Two cuts of one read: the checksum over the managed set the plan was
        // pinned to, and beside it the state the checkpoints are measured
        // against, which watches every module the plan names as well
        // (DECISIONS 174).
        let (scoped, mut watching, unrevocable) = staged_baseline(
            conn,
            &recorded_ids,
            &recorded_modules,
            &modules_after(&entry.snapshot, &plan.changes, Settled::SoFar),
            &externals_under(&externals, &recorded_ids),
            target.catalog_policy(project.config.unmanaged),
            &pinned_baseline_scopes(
                &entry.snapshot.schema.data_scopes(),
                &scopes_under(&plan.data, &plan.ids, &entry.snapshot.ids),
                &plan.changes,
                &plan.ids,
                (&entry.snapshot.schema, &entry.snapshot.ids),
            ),
            &entry.snapshot.schema,
            &held_before_the_plan(
                plan,
                &entry.snapshot.schema,
                &entry.snapshot.ids,
                &scopes_at(plan, &recorded_ids),
            ),
            CatalogWarnings::for_progress(d.progress),
        )
        .await?;
        retain_managed_roles(&mut watching, &plan.ids, dialect);
        trim_to_the_planned_scope(
            plan,
            &entry.snapshot.ids,
            &scopes_at(plan, &recorded_ids),
            &mut watching,
        );

        let live = pbps_model::drift_checksum(&scoped.schema, &recorded_ids);
        if live != plan.baseline.checksum {
            bail!(
                "`{}` is no longer the database this plan was computed against.\n\
                 plan baseline: {}\n\
                 database now:  {live}\n\
                 Something changed since the plan was approved. `pbps verify` shows what; \
                 then recompute the plan with `pbps plan --db --staged`.",
                pbps_model::escape::Escaped(&target.label),
                plan.baseline.checksum
            );
        }
        refuse_moved_database_collation(conn, plan, &target.label).await?;
        refuse_unexpressible(&scoped, &target.label, "apply again")?;
        // Ownership and revocability, as in the transactional apply (#692,
        // #1057).
        crate::engine::owned_targets(
            conn.driver(),
            &plan.changes,
            &plan.ids,
            &scoped.owners,
            &scoped.session_role,
        )?;
        crate::engine::unrevocable_grants(conn.driver(), &plan.changes, &plan.ids, &unrevocable)?;
        // Before the probes, as in the transactional apply (DEC-319.1).
        crate::pins::check(
            conn,
            project,
            target,
            plan,
            &pinned_managed(&entry.snapshot, &plan.changes, &plan.ids),
            crate::pins::Check::BeforeProbes,
        )
        .await?;
        // Only on a fresh start. The probes name objects as the catalog had
        // them before the first statement, and after a partial run some of
        // those names have already moved — a probe answered about the wrong
        // object is worse than one that was not asked.
        preflight(
            conn,
            dialect,
            plan,
            rename_targets,
            &scoped.schema,
            &recorded_ids,
            &crate::pins::Approved {
                project,
                target,
                plan,
                managed: &pinned_managed(&entry.snapshot, &plan.changes, &plan.ids),
            },
            None,
        )
        .await?;
        (0, watching)
    };

    // The settings this dialect's writes must run under, on the connection,
    // because this mode opens no transaction to carry them (`session_pins`).
    // Established here rather than once per statement: it covers a `--resume`
    // on a fresh connection, which starts partway through the plan, and every
    // statement after the one that fails.
    //
    // Kept even though `preflight` now pins as its first act: a `--resume`
    // skips `preflight` entirely, so this is the only place the resumed
    // statements get their settings from (DECISIONS 415).
    pin_session(conn, dialect).await?;

    // A staged run has no transaction of its own to carry the census: it reads
    // in one that it rolls back, at a start and at a resume alike.
    if start < statements.len() {
        conn.begin(&dialect.transaction_framing(&pin_zone(conn)))
            .await?;
        let mut seen =
            recheck_enum_rebuilds(conn, &plan.changes, &entry.snapshot.ids, resume).await;
        if seen.is_ok() && !resume {
            seen = recheck_domain_alterations(conn, &plan.changes, &entry.snapshot.ids).await;
        }
        // Every statement starts from a target that still allows it: a resume
        // only re-asks for the packages whose statement has not run.
        if seen.is_ok() && !resume {
            seen = recheck_extension_creates(conn, plan).await;
        }
        let rolled = conn
            .rollback(&dialect.transaction_framing(&pin_zone(conn)))
            .await;
        seen?;
        rolled?;
    }

    let mut retained_verified = None;
    let total = statements.len();
    // Staged plans carry one logical change. A completed DROP has no catalog
    // target left; a closing-only resume must not ask for that old target.
    if start < total {
        drop_preflight(
            conn,
            dialect,
            &plan.changes,
            &plan.ids,
            &entry.snapshot.schema,
            &entry.snapshot.ids,
        )
        .await?;
    }
    // The names the catalog has right now. It starts at whatever the newest
    // entry recorded — the last ordinary state on a fresh run, the checkpoint
    // on a resume — and each statement moves it, using what the emitter said
    // that statement does (`Statement::renames`).
    let mut live_ids = entry.snapshot.ids.clone();
    // Include adopted types missing from the older state. Each lifecycle
    // statement updates its uid below before the checkpoint is read.
    live_ids.types = plan.ids.types.clone();
    if !dialect.manages_roles() {
        live_ids.roles = plan.ids.roles.clone();
    }
    // The state each checkpoint is measured against, read in the shape a
    // checkpoint is read in so the two compare like with like — the newest
    // entry's own schema is spelled by whichever command wrote it, and a cell
    // at its default has three spellings (`ObservedRow`).
    //
    // A staged run cannot roll back, so it cannot refuse its way out of a
    // change somebody else makes underneath it (147, 150). What it can do is
    // notice: nothing else in the run compares one checkpoint with the last,
    // so an edit that landed between two statements was carried into every
    // later read and finally into the closing ordinary snapshot, which is the
    // state `verify` measures against ever after (DECISIONS 159).
    println!(
        "Applying {} statement(s) with per-statement commits...",
        total - start
    );
    // A resume starts from a checkpoint, whose modules include what earlier
    // steps created; the union keeps those managed too.
    let pinned_managed = pinned_managed(&entry.snapshot, &plan.changes, &plan.ids);
    let written_routines = crate::engine::written_routines(&plan.changes);
    // Whether this run has a checkpoint to resume from: a resumed run starts
    // on one.
    let mut checkpointed = entry.snapshot.staged.is_some();
    let domain_defaults = pbps_pg::type_refs::declared::capture_defaults(
        conn,
        &plan.declared_type_pins,
        &plan.changes,
        pbps_pg::pins::Within::OwnTransaction,
    )
    .await?;
    for (i, stmt) in statements.iter().enumerate().skip(start) {
        // Before each step, including the first after a resume, which skips
        // the check before the probes along with the probes.
        crate::pins::check(
            conn,
            project,
            target,
            plan,
            &pinned_managed,
            crate::pins::Check::BeforeStep {
                step: i + 1,
                total,
                executed: &statements[..i],
            },
        )
        .await?;
        let executed = if stmt.row_write.is_some() {
            // A staged row is still one atomic statement. Hold its trigger
            // locks through the write, then commit before the checkpoint;
            // non-transactional DDL retains the ordinary staged path.
            conn.begin(&dialect.transaction_framing(&pin_zone(conn)))
                .await?;
            let result = async {
                // The plan's own admission, so a routine its review pinned is
                // admitted here as it was at the start (DEC-2563.9).
                let admission = crate::pins::applying(project, plan);
                let guard = crate::engine::prepare_data_writes(
                    conn,
                    &admission,
                    &plan.changes,
                    &entry.snapshot,
                    &plan.ids,
                )
                .await?;
                crate::engine::check_data_write(conn, &admission, stmt, &guard).await?;
                conn.execute(&stmt.sql).await?;
                // A deferred trigger would otherwise run at COMMIT, after the
                // check below, and a function replaced while an earlier one
                // held the commit would run unchecked. The step commits right
                // after, so settling here refuses nothing the commit would
                // have allowed (473).
                crate::engine::settle_data_writes(conn).await?;
                // Before this step commits, not after (#428). The trigger
                // check and the row statement are two server commands, and a
                // trigger function replaced between them has already run by
                // now. Its pin moved, and this transaction can still take the
                // write back with it. After the commit, the check below could
                // only record the step and stop.
                crate::pins::check(
                    conn,
                    project,
                    target,
                    plan,
                    &pinned_managed,
                    crate::pins::Check::BeforeStepCommits {
                        step: i + 1,
                        total,
                        executed: &statements[..=i],
                    },
                )
                .await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            finish_transaction(conn, dialect, result).await
        } else if stmt.transactional
            && (stmt.declared_type.is_some()
                || stmt.rewrites.is_some()
                || stmt.introduces.is_some()
                || stmt.extension.is_some()
                || !written_routines.is_empty()
                || !plan.type_pins.is_empty()
                || !plan.provider_pins.is_empty()
                || !plan.declared_type_pins.is_empty())
        {
            // A step of a plan that writes routines runs in a transaction of
            // its own, so that a SECURITY DEFINER routine outside the safe
            // form is refused before its step commits rather than after
            // (#322). A routine a later step creates is not there yet.
            conn.begin(&dialect.transaction_framing(&pin_zone(conn)))
                .await?;
            let result = async {
                let retained = crate::engine::hold_retained(
                    conn,
                    &plan.changes,
                    Some(&entry.snapshot.declared.overlay(&previous)),
                )
                .await?;
                if let Some(extension) = &stmt.extension {
                    vet_before_extension(conn, extension, Some(plan)).await?;
                }
                if stmt.rewrites.is_some() {
                    // The expressions the rewrite re-evaluates, held still by
                    // the lock the guard takes, judged by the plan's own
                    // admission (DEC-2563.9).
                    let admission = crate::pins::applying(project, plan);
                    let guard = crate::engine::prepare_data_writes(
                        conn,
                        &admission,
                        &plan.changes,
                        &entry.snapshot,
                        &plan.ids,
                    )
                    .await?;
                    crate::engine::check_data_write(conn, &admission, stmt, &guard).await?;
                }
                if let Some(change) = &stmt.introduces {
                    // Judged by the plan's own admission before the statement
                    // runs, with what the earlier steps created in place.
                    let admission = crate::pins::applying(project, plan);
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
                conn.execute(&stmt.sql).await?;
                retained_verified =
                    crate::engine::verify_retained(conn, retained.as_ref(), &previous).await?;
                if let Some(type_statement) = &stmt.declared_type {
                    pbps_pg::type_refs::declared::vet(
                        conn,
                        type_statement,
                        &crate::pins::applying(project, plan),
                        &plan.declared_type_pins,
                        &plan.changes,
                        &statements[..=i],
                    )
                    .await?;
                }
                if let Some(extension) = &stmt.extension {
                    vet_after_extension(conn, extension, Some(plan)).await?;
                }
                crate::engine::refuse_unsafe_definers(
                    conn,
                    &written_routines,
                    crate::engine::Absent::NotYetCreated,
                )
                .await?;
                crate::pins::check(
                    conn,
                    project,
                    target,
                    plan,
                    &pinned_managed,
                    crate::pins::Check::BeforeStepCommits {
                        step: i + 1,
                        total,
                        executed: &statements[..=i],
                    },
                )
                .await
            }
            .await;
            let result = finish_transaction(conn, dialect, result).await;
            // The preceding ADD committed its NOT VALID CHECK. Recover it
            // after rollback, when catalog reads can run again (DEC-1638.1).
            match result {
                Err(error) => {
                    match crate::engine::recover_failed_staged_validation(conn, stmt).await {
                        Some(outcome) => Err(error.context(outcome)),
                        None => Err(error),
                    }
                }
                Ok(()) => Ok(()),
            }
        } else {
            crate::engine::execute_staged_statement(conn, stmt)
                .await
                .map_err(anyhow::Error::from)
        };
        if let Err(e) = executed {
            // `.context()` on the already-built `anyhow::Error`, not
            // `anyhow::anyhow!("...{e}")`: the latter would bake `e`'s
            // `Display` — which can carry the driver's own sentence — into an
            // opaque string with nothing left to downcast. This keeps
            // whatever `e` already carries (a `DbError::Driver`, most often)
            // as the source, so `crate::engine::ledger_safe_reason` can find
            // and redact just that frame later (DECISIONS 455).
            let recovery = if !stmt.transactional && i == 0 && entry.snapshot.staged.is_none() {
                "There is no staged checkpoint: the first non-transactional statement failed. \
                 Fix the cause, then start this plan again with `pbps apply --staged` without `--resume`."
            } else if is_scan_check(stmt, pbps_dialect::ScanCheckStep::Validate) && !checkpointed {
                // The `ADD` before it took no checkpoint, and the failed
                // `VALIDATE` took its check out again (#1638).
                "There is no staged checkpoint: the attach's temporary CHECK did not validate. \
                 Fix the rows it names, then start this plan again with `pbps apply --staged` \
                 without `--resume`."
            } else {
                "The ledger records everything that did complete. Fix the cause, then continue \
                 with `pbps apply --staged --resume`."
            };
            return Err(e.context(format!(
                "the database rejected statement {} of {total}; earlier committed statements were not rolled back:\n{}\n\n\
                 {recovery}",
                i + 1,
                stmt.sql
            )));
        }

        // Applied before the checkpoint is taken: this statement has committed,
        // so the names it moved are the names the catalog has now.
        for (from, to) in &stmt.renames {
            live_ids.rename_table(from, to);
        }
        for (from, to) in &stmt.role_renames {
            live_ids.rename_role(from, to);
        }
        if let Some(type_statement) = &stmt.declared_type {
            if type_statement.definition.is_some() {
                live_ids
                    .types
                    .insert(type_statement.uid.clone(), type_statement.name.clone());
            } else {
                live_ids.types.remove(&type_statement.uid);
            }
        }
        if let Some(installed) = &stmt.extension {
            match installed.phase {
                pbps_dialect::ExtensionPhase::Drop => {
                    live_ids.extensions.remove(&installed.uid);
                }
                pbps_dialect::ExtensionPhase::Create => {
                    live_ids
                        .extensions
                        .insert(installed.uid.clone(), installed.name.clone());
                }
                // The package keeps its identity.
                pbps_dialect::ExtensionPhase::Update { .. }
                | pbps_dialect::ExtensionPhase::SetSchema { .. } => {}
            }
        }
        adopt_created(&mut live_ids, &plan.ids, &stmt.creates);

        // No checkpoint after a staged attach's `ADD … NOT VALID`: measured,
        // every read of a table holding a `NOT VALID` check refuses as a fact
        // the model cannot hold, so this read would stop the run on a
        // statement that worked. The `VALIDATE` after it checkpoints both,
        // and takes the check out again if it fails (#1638, DEC-1638.1).
        if is_scan_check(stmt, pbps_dialect::ScanCheckStep::Add) {
            crate::pins::check(
                conn,
                project,
                target,
                plan,
                &pinned_managed,
                crate::pins::Check::AfterStep {
                    step: i + 1,
                    total,
                    executed: &statements[..=i],
                },
            )
            .await?;
            println!(
                "  statement {} of {total} done (checkpointed with the next)",
                i + 1
            );
            continue;
        }

        // The unmanaged policy is deliberately not enforced between two
        // committed statements. It is a hygiene gate for the start of a
        // command, where it already ran, and failing it here would abort
        // *after* the DDL committed and leave no checkpoint to resume from.
        // The rows under the plan's scope even mid-way: a declared row the
        // remaining statements have not inserted yet is simply not there, and
        // the checkpoint says so.
        let after = managed_state(
            conn,
            &live_ids,
            &modules_after(&entry.snapshot, &plan.changes, Settled::SoFar),
            &externals_under(&externals, &live_ids),
            target.catalog_policy(pbps_config::Unmanaged::Ignore),
            // The plan's own scope, not the widened one: what a statement
            // leaves is read under the keys the plan leaves, as the closing
            // read is, so a row the renames leave unmanaged is not compared
            // (DEC-2346.2).
            &scopes_at(plan, &live_ids),
            &Schema::default(),
            crate::engine::Read::Snapshot,
            CatalogWarnings::for_progress(d.progress),
        )
        .await?;
        // Scoped and identified by `live_ids`, not by the plan's mapping: a
        // checkpoint records the database as it stands, and halfway through a
        // rename that moves both the schema and the name, the table stands at
        // neither end. `--resume` reads this mapping back and compares against
        // it, so a hand-made change to that table while the deployment is
        // paused is seen rather than carried silently into the closing entry.
        let module_deps = dependency_hints_for_schema(&entry.snapshot, plan, &after.schema);
        let mut after = after;
        after.schema.carry_unidentified_from(&entry.snapshot.schema);
        let mut checkpoint = pbps_model::StateSnapshot::new(
            StateKind::Staged,
            after.schema,
            live_ids.clone(),
            operator,
        );
        checkpoint.baseline_evidence = after.baseline_evidence;
        checkpoint.module_deps = module_deps;
        // A checkpoint records the database as it stands, and what was
        // declared is what the previous state said until the plan finishes:
        // the closing entry advances it by the whole plan, and nothing plans
        // against a checkpoint (`refuse_mid_deployment`).
        checkpoint.declared = entry.snapshot.declared.clone();
        checkpoint.git_sha = plan.git_sha.clone().or_else(|| db::git_sha(project.root()));
        checkpoint.plan_checksum = Some(plan_checksum.to_owned());
        checkpoint.staged = Some(pbps_model::StagedProgress {
            completed: i + 1,
            total,
            last_statement: stmt.sql.clone(),
        });
        let recorded = checkpoint.schema.clone();
        // After the step, and before its checkpoint, so that a routine
        // replaced while the step ran is caught at that step — the last one
        // included. The statement has committed, so a mismatch is a
        // post-commit guard failure: the checkpoint is written first and the
        // run stops after it (DECISIONS 159).
        let pins_after_step = crate::pins::check(
            conn,
            project,
            target,
            plan,
            &pinned_managed,
            crate::pins::Check::AfterStep {
                step: i + 1,
                total,
                executed: &statements[..=i],
            },
        )
        .await;
        let id = crate::engine::record(conn, &checkpoint).await?;
        checkpointed = true;
        println!("  statement {} of {total} done (checkpoint #{id})", i + 1);
        pins_after_step.map_err(|e| {
            e.context(format!(
                "statement {} of {total} committed, and checkpoint #{id} records it; nothing \
                 was rolled back. A resume checks the same pins, so either put the routine back \
                 as it was and run `pbps apply --staged --resume`, or accept the database with \
                 `pbps baseline --reason ...` and plan from there",
                i + 1
            ))
        })?;
        // The checkpoint is written *first*, and then the run stops. It says
        // what the database holds, which is the one thing a resume needs to be
        // true — refusing before writing it would lose the record of a
        // statement that has already committed, which is what checkpoints are
        // for. Stopping is the whole remedy a staged run has.
        let statement_plan = staged.statement(stmt, statements.get(i.wrapping_sub(1)));
        staged_pinned_movement(
            dialect,
            statement_plan.staged(),
            &without_scan_checks(&previous, statements),
            &without_scan_checks(&recorded, statements),
            &target.label,
            target.environment(),
            StagedRead::Checkpoint {
                completed: i + 1,
                total,
            },
            retained_verified.as_ref(),
            Some(&domain_defaults),
        )?;
        previous = recorded;
    }

    let retaining = pbps_model::retained::expressions(&plan.changes)
        .next()
        .is_some();
    if retaining {
        conn.begin(&dialect.transaction_framing(&pin_zone(conn)))
            .await?;
    }
    // Keep the closing guard's large future off every caller's apply future,
    // including transactional applies that never enter this staged path.
    let closing = Box::pin(async {
        let retained = crate::engine::hold_retained(conn, &plan.changes, None).await?;
        // The closing entry is an ordinary apply with no staged marker: its absence
        // is what tells every later command this environment is no longer
        // mid-deployment.
        let after = managed_state(
            conn,
            &plan.ids,
            &modules_after(&entry.snapshot, &plan.changes, Settled::Whole),
            &externals_under(&externals, &plan.ids),
            target.catalog_policy(project.config.unmanaged),
            &plan.data,
            &Schema::default(),
            if retaining {
                crate::engine::Read::InsideOwnTransaction
            } else {
                crate::engine::Read::Snapshot
            },
            CatalogWarnings::for_progress(d.progress),
        )
        .await?;
        refuse_recreated_tables(conn, &plan.changes, statements, &after.unmanaged)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "{e:#}\n\n\
             All {total} statement(s) completed, but the staged deployment cannot close. \
             Nothing was rolled back; the ledger retains the last checkpoint. \
             Remove the recreated table and run `pbps apply --staged --resume`, \
             or accept the database with `pbps baseline --reason ...` and plan from there."
                )
            })?;
        let verified =
            crate::engine::verify_retained(conn, retained.as_ref(), &after.schema).await?;
        // And the last window of all: between the final checkpoint and this read.
        // Refused *before* the ordinary entry is written, because that entry is
        // what says the deployment finished — leaving the environment on its last
        // checkpoint is the honest answer, and `refuse_mid_deployment` then makes
        // every other command say so.
        staged_pinned_movement(
            dialect,
            staged,
            &previous,
            &after.schema,
            &target.label,
            target.environment(),
            StagedRead::Closing { total },
            verified.as_ref(),
            Some(&domain_defaults),
        )?;
        // The same read, and the half of it no `Schema` holds. Refused here for
        // the reason the movement check above is: this is the last window, and
        // the ordinary entry that follows is what says the deployment finished.
        refuse_unmet_public_execution(&plan.changes, &after, &target.label).map_err(|e| {
            anyhow::anyhow!(
                "{e:#}\n\n\
             All {total} statement(s) completed, but the staged deployment cannot close. \
             Nothing was rolled back; the ledger retains the last checkpoint. Settle what \
             moved and run `pbps apply --staged --resume`, or accept the database with \
             `pbps baseline --reason ...` and plan from there."
            )
        })?;
        // The pins once more, because a `--resume` after the last checkpoint runs
        // no step and so reaches no other check (DEC-319.1).
        crate::pins::check(
            conn,
            project,
            target,
            plan,
            &pinned_managed,
            if retaining {
                crate::pins::Check::BeforeRecording {
                    executed: statements.statements(),
                }
            } else {
                crate::pins::Check::BeforeClosing {
                    executed: statements.statements(),
                }
            },
        )
        .await
        .map_err(|e| {
            e.context(format!(
                "all {total} statement(s) completed, but the staged deployment cannot close; \
             nothing was rolled back and the ledger retains the last checkpoint. Either put \
             the routine back as it was and run `pbps apply --staged --resume`, or accept the \
             database with `pbps baseline --reason ...` and plan from there"
            ))
        })?;
        let mut after = after;
        after.schema.carry_unidentified_from(&entry.snapshot.schema);
        let mut snapshot = pbps_model::StateSnapshot::new(
            pbps_model::StateKind::Apply,
            after.schema,
            plan.ids.clone(),
            operator,
        );
        snapshot.baseline_evidence = after.baseline_evidence;
        snapshot.module_deps = plan.module_deps.clone();
        snapshot.git_sha = plan.git_sha.clone().or_else(|| db::git_sha(project.root()));
        snapshot.plan_checksum = Some(plan_checksum.to_owned());
        snapshot.target_assertions = plan.target_assertions.clone();
        // What the previous state declared, advanced by what this plan wrote: from
        // the plan alone, since `apply --plan` needs nothing else (SPEC §7.3).
        snapshot.declared = entry.snapshot.declared.clone();
        snapshot
            .declared
            .advance_with_partitions(&plan.changes, &snapshot.schema);
        let id = crate::engine::record(conn, &snapshot).await?;
        if retaining {
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
            .await?;
        }
        Ok(id)
    })
    .await;
    if retaining {
        finish_transaction(conn, dialect, closing).await
    } else {
        closing
    }
}

/// The movement check a staged run can make: each read compared with the one
/// before it, over everything this plan does not touch.
///
/// A split table rename uses the current statement's exact endpoints at each
/// checkpoint (DECISIONS 436). Its intermediate name is still compared for
/// unchanged shape, rows and incoming references. Other change shapes retain
/// the logical plan's exemptions: a later statement of that change must not
/// be demanded at an earlier checkpoint (152 to 158, 161). The closing read
/// retains the full logical postconditions and checks the vacated rename
/// endpoints, including intermediate names, under SPEC §7.6.
///
/// Which read of a staged run is being compared with the one before it.
///
/// The two are told apart because the remedy differs. A change found at a
/// checkpoint is *in* that checkpoint — the read is what the checkpoint
/// records — so a `--resume` measures the live database against a record that
/// already holds it, and goes on. A change found at the closing read landed
/// after the last checkpoint was written, and no record holds it: the same
/// `--resume` finds the database moved since that checkpoint and refuses. One
/// message told the operator to resume in both cases (DECISIONS 190).
#[derive(Clone, Copy)]
pub(super) enum StagedRead {
    /// The read a checkpoint records, once `completed` of `total` statements
    /// have run.
    Checkpoint { completed: usize, total: usize },
    /// The read after the last checkpoint, which becomes the closing entry.
    Closing { total: usize },
}

/// Ids follow a renamed table and omit a dropped one, so a recreated name
/// falls outside the managed schema. Its absence remains a plan promise
/// (SPEC §7.6). Use the same read's inventory without adopting unrelated
/// unmanaged tables into the guard (SPEC §8.2); the net promise also permits
/// a plan that deliberately reuses a name.
pub(super) async fn refuse_recreated_tables(
    conn: &mut Conn,
    changes: &pbps_model::ChangeSet,
    statements: &[pbps_dialect::Statement],
    unmanaged: &[TableName],
) -> anyhow::Result<()> {
    let absent = removed_table_names(changes, statements);
    if let Some(name) = crate::engine::matching_table_names(conn, &absent, unmanaged)
        .await?
        .first()
    {
        anyhow::bail!(
            "{name} is still there, and this plan removes it",
            name = name.shown()
        );
    }
    Ok(())
}

/// The logical rename omits its intermediate names (DECISIONS 436). Their
/// emitted moves still promise absence at closing (SPEC §7.6), unless a
/// later move or the logical plan deliberately gives the name to a table.
pub(super) fn removed_table_names(
    changes: &pbps_model::ChangeSet,
    statements: &[pbps_dialect::Statement],
) -> Vec<TableName> {
    let mut expected = BTreeMap::new();
    for statement in statements {
        for (from, to) in &statement.renames {
            expected.insert(from, pbps_model::Presence::Absent);
            expected.insert(to, pbps_model::Presence::Present);
        }
    }
    expected.extend(
        changes
            .changes
            .iter()
            .flat_map(|change| change.change.tables_after()),
    );
    expected
        .into_iter()
        .filter(|(_, presence)| *presence == pbps_model::Presence::Absent)
        .map(|(name, _)| name.clone())
        .collect()
}

/// What the plan promised about `PUBLIC`, asked of the read that closes the
/// run.
///
/// # Why this is not part of the movement comparison
///
/// `refuse_pinned_movement` compares two `Schema`s, and what `PUBLIC` holds
/// is never in one: it is the engine's default rather than a grant, so putting
/// it there would make every routine read back as drift (DECISIONS 371, 95).
/// The decision this plan carries is therefore checked against the read's own
/// `public_execute` context instead — which is what SPEC §7.6 requires of it.
///
/// # Why the closing read in particular
///
/// SPEC §7.6 does not promise that a checkpoint catches a concurrent change
/// to **the field the plan is itself changing**: the field is expected to move
/// there, and the checkpoint cannot tell the plan's statement from the other
/// session's. It does promise the closing read catches it, by holding the
/// field to the value the plan promised. Without this the promise was not
/// kept: a staged run's `GRANT` and its `CREATE` commit separately, another
/// session revoking `PUBLIC` in between would be invisible to the checkpoint
/// by design and to the closing read because nothing looked, and the run
/// would record a routine the declaration asked to leave open as a success.
///
/// A decision this engine does not make is not asked about — the check reads
/// the plan's own decisions, and a dialect that emits none has none to fail.
pub(super) fn refuse_unmet_public_execution(
    changes: &pbps_model::ChangeSet,
    after: &pbps_diff::Scoped,
    label: &str,
) -> anyhow::Result<()> {
    let mut unmet = Vec::new();
    for planned in &changes.changes {
        let pbps_model::Change::PublicExecution {
            routine, access, ..
        } = &planned.change
        else {
            continue;
        };
        let id = pbps_model::ModuleId::Routine(routine.clone());
        let open = after.public_execute.contains(&id);
        match access {
            pbps_model::PublicAccess::Kept if !open => unmet.push(format!(
                "`{routine}` is not executable by PUBLIC, which this plan declared it would stay",
                routine = routine.shown()
            )),
            pbps_model::PublicAccess::Revoked if open => unmet.push(format!(
                "`{routine}` is executable by PUBLIC, which this plan closed",
                routine = routine.shown()
            )),
            pbps_model::PublicAccess::Kept | pbps_model::PublicAccess::Revoked => {}
        }
    }
    if !unmet.is_empty() {
        bail!(
            "{label} does not hold what this plan settled about PUBLIC execution:\n  {}",
            unmet.join("\n  "),
            label = pbps_model::escape::Escaped(label),
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn staged_pinned_movement<'a>(
    dialect: &dyn pbps_dialect::Dialect,
    plan: guard::Staged<'a>,
    before: &Schema,
    after: &Schema,
    label: &str,
    environment: Option<&str>,
    read: StagedRead,
    verified: Option<&'a pbps_pg::retained::Verified>,
    defaults: Option<&'a pbps_pg::type_refs::declared::DomainDefaults>,
) -> anyhow::Result<()> {
    refuse_pinned_movement(
        dialect,
        plan.read(read)
            .with_retained(verified)
            .with_domain_defaults(defaults),
        before,
        after,
        label,
    )
    .map_err(|e| {
        match read {
            StagedRead::Checkpoint { completed, total } => anyhow::anyhow!(
                "{e:#}\n\n\
                 {completed} of {total} statement(s) completed and the ledger records them; a \
                 staged apply runs outside a transaction, so nothing was rolled back. The \
                 checkpoint holds the database as it stands, this change included — resuming \
                 accepts it. The list above is the record of what moved: the checkpoint \
                 already holds it, so `pbps verify` reads clean, and this refusal is recorded \
                 as the failed entry's reason{}.",
                // Named only to a caller who can run it. `status` takes no
                // target and reports on the environments `pbps.yml` configures
                // (SPEC 9.2), so for a `--db` run it answers about other
                // databases or says none are configured — a pointer that cannot
                // be followed, in a message whose whole job is to say where the
                // record is. Without one the sentence is already complete: the
                // list above is that record (DECISIONS 196).
                match environment {
                    Some(_) => ", which `pbps status` shows",
                    None => "",
                }
            ),
            StagedRead::Closing { total } => anyhow::anyhow!(
                "{e:#}\n\n\
                 All {total} statement(s) completed and the ledger records them; a staged \
                 apply runs outside a transaction, so nothing was rolled back. This change \
                 landed after the last checkpoint was written, so no checkpoint holds it, and \
                 `--resume` will refuse the database as having moved since that checkpoint. \
                 `pbps verify` shows what it is: undo it and resume, or take the database as \
                 it stands with `pbps baseline --reason ...` and plan from there."
            ),
        }
    })
}
