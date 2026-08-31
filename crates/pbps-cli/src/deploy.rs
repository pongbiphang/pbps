//! The commands that need a database: recording state, adopting a database, and
//! building one from the declarations (SPEC §8, §9.2).
//!
//! # The three ways a state gets recorded, and why they are not one command
//!
//! | Command | Records | Guard |
//! |---|---|---|
//! | `snapshot` | the database as it stands | refuses when it differs from the last recorded state |
//! | `baseline` | the same thing, unconditionally | requires `--reason` |
//! | `bootstrap` | the database it just built | refuses unless the managed set is empty |
//!
//! `snapshot` is what CI runs after `apply`, where there is provably nothing to
//! skip; it refuses on a difference precisely because that case — someone
//! changed the schema by hand and the pipeline is about to bless it — is the one
//! this tool exists to catch. `baseline` is the deliberate override, and the
//! reason it demands is what an audit asks for later.

use anyhow::{Context as _, bail};

use pbps_config::Project;
use pbps_db::Conn;
use pbps_model::{IdsFile, Schema, StateKind, StateSnapshot};

use crate::db::{self, Target};

/// Introspects, cuts the result down to the managed set, and reports whatever
/// the read itself could not express.
///
/// Every connected command starts here. The warnings are printed rather than
/// returned because ignoring them is never right: a drift check that silently
/// skipped a computed column would report "no drift" about a database it only
/// half read.
async fn managed_state(
    conn: &mut Conn,
    ids: &IdsFile,
    modules: &std::collections::BTreeSet<pbps_model::ObjectName>,
    unmanaged: pbps_config::Unmanaged,
) -> anyhow::Result<pbps_diff::Scoped> {
    let pulled = pbps_mssql::catalog::introspect(conn)
        .await
        .context("cannot read the database catalog")?;
    for w in &pulled.warnings {
        eprintln!("warning: {w}");
    }
    // A module pbps cannot read is not a module it can leave to chance: it is
    // inside the managed set by name and outside it in fact, so the next plan
    // would propose creating one that is already there.
    for m in &pulled.unmanaged_modules {
        if modules.contains(&m.name.parse().unwrap_or_else(|_| unreachable_name())) {
            eprintln!(
                "warning: {} {} is declared, but {}; it is left alone",
                m.kind, m.name, m.why
            );
        }
    }

    let scoped = pbps_diff::scope(&pulled.schema, ids, modules);
    report_unmanaged(&scoped, unmanaged)?;
    Ok(scoped)
}

/// A name the catalog produced cannot fail to parse; this exists only so the
/// comparison above needs no `unwrap` that could one day fire.
fn unreachable_name() -> pbps_model::ObjectName {
    pbps_model::ObjectName::new("\u{0}", "\u{0}")
}

/// The modules the declarations name, for the commands that record a state.
///
/// `snapshot` and `baseline` write down what the environment is; a module the
/// declarations manage has to be in that record, or the next `verify` would not
/// be watching it. The ids file is already required by both, so requiring the
/// declarations to parse as well changes nothing about when they can run.
fn declared_modules(
    project: &Project,
) -> anyhow::Result<std::collections::BTreeSet<pbps_model::ObjectName>> {
    Ok(managed_modules(None, Some(&crate::load(project)?.schema)))
}

/// The modules a plan leaves the environment holding.
///
/// Built from the recorded state plus the plan's own changes rather than from
/// the declarations, so that `apply` still needs nothing but the plan file — the
/// same reason the plan carries its ids (SPEC §7.3, and constraint 23).
fn modules_after(
    recorded: &pbps_model::StateSnapshot,
    changes: &pbps_model::ChangeSet,
) -> std::collections::BTreeSet<pbps_model::ObjectName> {
    let mut set: std::collections::BTreeSet<_> = recorded.schema.modules.keys().cloned().collect();
    for p in &changes.changes {
        // Every module change names its module and nothing else does, so the
        // accessor is the whole classification; only the direction is left.
        let Some(name) = p.change.module_name() else {
            continue;
        };
        if matches!(p.change, pbps_model::Change::DropModule { .. }) {
            set.remove(name);
        } else {
            set.insert(name.clone());
        }
    }
    set
}

/// The modules a command is answerable for.
///
/// Two callers, two questions (see [`pbps_diff::scope`]): `verify` asks whether
/// this environment has moved since it was recorded, so the recorded state's
/// modules are the set; everything that plans or records asks what the state
/// should be, so the declarations count too.
fn managed_modules(
    recorded: Option<&pbps_model::StateSnapshot>,
    declared: Option<&pbps_model::Schema>,
) -> std::collections::BTreeSet<pbps_model::ObjectName> {
    let mut set = std::collections::BTreeSet::new();
    if let Some(s) = recorded {
        set.extend(s.schema.modules.keys().cloned());
    }
    if let Some(s) = declared {
        set.extend(s.modules.keys().cloned());
    }
    set
}

/// Applies the `unmanaged:` policy of SPEC §8.2 to what fell outside the scope.
fn report_unmanaged(
    scoped: &pbps_diff::Scoped,
    policy: pbps_config::Unmanaged,
) -> anyhow::Result<()> {
    if scoped.unmanaged.is_empty() && scoped.unmanaged_modules.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = scoped
        .unmanaged
        .iter()
        .chain(&scoped.unmanaged_modules)
        .map(ToString::to_string)
        .collect();
    match policy {
        pbps_config::Unmanaged::Ignore => {}
        pbps_config::Unmanaged::Warn => eprintln!(
            "warning: {} object(s) in this database are not declared and are left alone: {}",
            names.len(),
            names.join(", ")
        ),
        pbps_config::Unmanaged::Error => bail!(
            "`unmanaged: error` in pbps.yml, and {} object(s) here are not declared: {}.\n\
             Declare them (`pbps pull` reverse-generates them) or relax the setting.",
            names.len(),
            names.join(", ")
        ),
    }
    Ok(())
}

/// A declared table the database does not have is worth saying out loud
/// wherever it turns up: it is either a hand-dropped table or an identity file
/// that describes a different database.
fn report_missing(scoped: &pbps_diff::Scoped) {
    if scoped.missing.is_empty() {
        return;
    }
    let names: Vec<String> = scoped.missing.iter().map(ToString::to_string).collect();
    eprintln!(
        "warning: the identity file names {} table(s) this database does not have: {}",
        names.len(),
        names.join(", ")
    );
}

/// `pbps verify` — the drift check (SPEC §8.2).
///
/// The comparison is scoped and identified by the **recorded** state's identity
/// file, not the working tree's. The question this command answers is "has this
/// environment moved since pbps last recorded it", and answering it with a
/// mapping the environment has never seen would report every uncommitted local
/// rename as drift in production.
pub fn cmd_verify(project: &Project, target: &Target, json: bool) -> anyhow::Result<()> {
    db::require_mssql(project, "verify")?;
    let dialect = crate::dialect(project)?;
    let checked_at = crate::now();

    let report = db::runtime()?.block_on(async {
        let mut conn = Conn::connect(target.connection())
            .await
            .context("cannot connect to the database")?;

        let Some(baseline) = pbps_mssql::state::latest(&mut conn).await? else {
            bail!(
                "`{}` has a ledger but no entries; there is nothing to compare against.\n\
                 Record one with `pbps snapshot` or `pbps baseline --reason ...`.",
                target.label
            );
        };

        let recorded_ids = baseline.snapshot.ids.clone();
        let recorded_modules = managed_modules(Some(&baseline.snapshot), None);
        let scoped = managed_state(
            &mut conn,
            &recorded_ids,
            &recorded_modules,
            project.config.unmanaged,
        )
        .await?;

        // The live side is identified by what is actually there, not by the
        // recorded mapping. Comparing two sides that share one identity file
        // can only surface attribute changes on objects present in both — so a
        // hand-added or hand-dropped column, the two things a drift check most
        // needs to catch, would be exactly what it missed.
        let observed = pbps_diff::observed_ids(&scoped.schema, &recorded_ids);

        let changes = pbps_diff::diff(
            pbps_diff::Side {
                schema: &baseline.snapshot.schema,
                ids: &recorded_ids,
            },
            pbps_diff::Side {
                schema: &scoped.schema,
                ids: &observed,
            },
            dialect.as_ref(),
            // Drift emits no SQL, so there is no execution to give a hint
            // about; passing the declarations' strategies here would put a
            // hint nobody can act on into a report about what already happened.
            &pbps_model::Hints::default(),
        )
        .map_err(|errs| {
            for e in &errs {
                eprintln!("  {e}");
            }
            anyhow::anyhow!(
                "the live database differs in {} way(s) that cannot even be expressed as changes",
                errs.len()
            )
        })?;

        Ok(pbps_model::DriftReport {
            version: pbps_model::drift::CURRENT_VERSION,
            environment: target.label.clone(),
            checked_at,
            baseline: pbps_model::DriftBaseline {
                entry_id: baseline.id,
                applied_at: baseline.applied_at.clone(),
                checksum: pbps_model::state_checksum(&baseline.snapshot.schema, &recorded_ids),
            },
            // The checksum compares like with like: both sides fingerprinted
            // with the recorded mapping, so a derived identity for a hand-added
            // column cannot by itself make the two differ.
            live_checksum: pbps_model::state_checksum(&scoped.schema, &recorded_ids),
            changes,
            unmanaged: scoped.unmanaged,
        })
    })?;

    // The hook always receives JSON, whatever the human asked for on stdout:
    // a script's payload should not change shape because someone added a flag
    // for their own eyes.
    let payload = format!("{}\n", serde_json::to_string_pretty(&report)?);
    if json {
        print!("{payload}");
    } else {
        print!("{}", crate::report::drift(&report));
    }

    if !report.has_drift() {
        return Ok(());
    }
    if let Some(hook) = &project.config.hooks.on_drift {
        crate::hooks::run(hook, &payload, "on_drift");
    }
    // A distinct exit code so a scheduled pipeline can tell "the database moved"
    // from "the tool could not run" — the two need different people woken up.
    Err(crate::DriftFound.into())
}

/// `pbps snapshot` — record the current state, refusing to bless a difference.
pub fn cmd_snapshot(project: &Project, target: &Target, force: bool) -> anyhow::Result<()> {
    db::require_mssql(project, "snapshot")?;
    let ids = crate::read_ids(project)?;
    let declared_modules = declared_modules(project)?;
    let operator = crate::operator();

    db::runtime()?.block_on(async {
        let mut conn = Conn::connect(target.connection())
            .await
            .context("cannot connect to the database")?;
        let scoped =
            managed_state(&mut conn, &ids, &declared_modules, project.config.unmanaged).await?;
        report_missing(&scoped);

        // Comparing against the recorded state is the whole guard. A snapshot
        // that overwrites a state it differs from is exactly "somebody SSHed in
        // and changed the schema" being quietly adopted by a pipeline.
        match pbps_mssql::state::latest(&mut conn).await {
            Ok(Some(previous)) if !previous.snapshot.matches(&scoped.schema) && !force => {
                bail!(
                    "`{}` differs from the state recorded at {} (entry #{}).\n\
                     `pbps verify` shows what changed. Then either put the database back and \
                     re-run, or accept it with `pbps baseline --reason ...`.\n\
                     `--force` records it anyway.",
                    target.label,
                    previous.applied_at,
                    previous.id
                );
            }
            Ok(_) => {}
            // A database with no ledger is being recorded for the first time,
            // which is a decision, not a refresh.
            Err(pbps_db::LedgerError::NotInitialized) if !force => bail!(
                "pbps has never recorded a state for `{}`.\n\
                 Adopt it deliberately with `pbps baseline --reason ...`, or `--force` to \
                 record it as it stands.",
                target.label
            ),
            Err(pbps_db::LedgerError::NotInitialized) => {}
            Err(e) => return Err(e.into()),
        }

        let snapshot = with_provenance(StateSnapshot::new(
            StateKind::Apply,
            scoped.schema,
            ids.clone(),
            &operator,
        ));
        let id = pbps_mssql::state::record(&mut conn, &snapshot).await?;
        println!(
            "Recorded the state of `{}` as entry #{id} ({} table(s)).",
            target.label,
            snapshot.schema.tables.len()
        );
        Ok(())
    })
}

/// `pbps baseline` — take the database as it stands as the new starting point.
pub fn cmd_baseline(project: &Project, target: &Target, reason: &str) -> anyhow::Result<()> {
    db::require_mssql(project, "baseline")?;
    let ids = crate::read_ids(project)?;
    let declared_modules = declared_modules(project)?;
    let operator = crate::operator();

    db::runtime()?.block_on(async {
        let mut conn = Conn::connect(target.connection())
            .await
            .context("cannot connect to the database")?;
        let scoped =
            managed_state(&mut conn, &ids, &declared_modules, project.config.unmanaged).await?;
        report_missing(&scoped);

        let mut snapshot = with_provenance(StateSnapshot::new(
            StateKind::Baseline,
            scoped.schema,
            ids.clone(),
            &operator,
        ));
        snapshot.reason = Some(reason.to_owned());

        let id = pbps_mssql::state::record(&mut conn, &snapshot).await?;
        println!(
            "Baselined `{}` as entry #{id}: {} table(s) are now the starting point.",
            target.label,
            snapshot.schema.tables.len()
        );
        println!("Reason recorded: {reason}");
        Ok(())
    })
}

/// `pbps bootstrap` — build the whole schema from the declarations.
///
/// Two halves that are useful separately: `--sql` writes the CREATE script for a
/// DR runbook or an air-gapped host, and a target executes it. Neither implies
/// the other — the script is worth having on a machine that cannot reach the
/// database at all.
pub fn cmd_bootstrap(
    project: &Project,
    target: Option<&Target>,
    sql_out: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let loaded = crate::load(project)?;
    let ids = crate::read_ids(project)?;
    let dialect = crate::dialect(project)?;
    let declared_modules = managed_modules(None, Some(&loaded.schema));

    if ids.tables.is_empty() && !loaded.schema.tables.is_empty() {
        bail!(
            "the identity file names no tables, so there is nothing to build from.\n\
             Run `pbps plan` first to mint identities for the declarations."
        );
    }

    // Bootstrap is the plan from nothing: every declared table is created.
    let cs = pbps_diff::diff(
        pbps_diff::Side {
            schema: &Schema::default(),
            ids: &IdsFile::default(),
        },
        pbps_diff::Side {
            schema: &loaded.schema,
            ids: &ids,
        },
        dialect.as_ref(),
        // Bootstrap builds into an empty database: every table is created from
        // nothing, and there are no rows for an online operation to spare.
        &pbps_model::Hints::default(),
    )
    .map_err(|errs| {
        for e in &errs {
            eprintln!("  {e}");
        }
        anyhow::anyhow!("{} change(s) cannot be expressed", errs.len())
    })?;

    if let Some(path) = sql_out {
        let script = crate::render_sql(&cs, dialect.as_ref(), "an empty database")?;
        std::fs::write(path, script)
            .with_context(|| format!("cannot write `{}`", path.display()))?;
        println!("wrote {}", path.display());
    }

    let Some(target) = target else {
        if sql_out.is_none() {
            bail!("bootstrap needs somewhere to go: pass --sql <file>, --db or --env");
        }
        return Ok(());
    };

    db::require_mssql(project, "bootstrap")?;
    let statements = crate::statements(&cs, dialect.as_ref())?;
    let operator = crate::operator();

    db::runtime()?.block_on(async {
        let mut conn = Conn::connect(target.connection())
            .await
            .context("cannot connect to the database")?;

        // Bootstrap means "into an empty database". Running it over an existing
        // managed set would fail halfway through on the first CREATE of a table
        // that is already there, leaving a partly built schema and a ledger that
        // never mentioned it.
        let existing =
            managed_state(&mut conn, &ids, &declared_modules, project.config.unmanaged).await?;
        if !existing.schema.tables.is_empty() {
            bail!(
                "`{}` already has {} of the declared table(s); bootstrap builds into an empty \
                 database.\nUse `pbps plan --db` and `pbps apply` to migrate it instead.",
                target.label,
                existing.schema.tables.len()
            );
        }

        pbps_mssql::state::lock(&mut conn, &operator).await?;
        let result = run_in_transaction(&mut conn, &statements).await;
        let unlocked = pbps_mssql::state::unlock(&mut conn).await;
        result?;
        unlocked?;

        // The state recorded is what the engine actually built, read back — not
        // what was declared. Expressions come back in the engine's stored form,
        // and only that form compares equal on the next drift check (SPEC §8.2).
        let built =
            managed_state(&mut conn, &ids, &declared_modules, project.config.unmanaged).await?;
        let snapshot = with_provenance(StateSnapshot::new(
            StateKind::Bootstrap,
            built.schema,
            ids.clone(),
            &operator,
        ));
        let id = pbps_mssql::state::record(&mut conn, &snapshot).await?;
        println!(
            "Bootstrapped `{}`: {} table(s) created, recorded as entry #{id}.",
            target.label,
            snapshot.schema.tables.len()
        );
        Ok(())
    })
}

/// `pbps state prune` — drop old snapshots.
pub fn cmd_prune(project: &Project, target: &Target, keep: u32) -> anyhow::Result<()> {
    db::require_mssql(project, "state prune")?;
    db::runtime()?.block_on(async {
        let mut conn = Conn::connect(target.connection())
            .await
            .context("cannot connect to the database")?;
        let removed = pbps_mssql::state::prune(&mut conn, keep).await?;
        // The effective figure, not the one asked for: `--keep 0` still keeps
        // the newest entry, and reporting "0 remain" would describe an
        // environment with no baseline — which is not what happened.
        let kept = keep.max(1);
        println!(
            "Pruned {removed} old snapshot(s) from `{}`; the {kept} newest remain.",
            target.label
        );
        if keep == 0 {
            println!(
                "(--keep 0 still keeps the current baseline: dropping it would switch drift detection off.)"
            );
        }
        Ok(())
    })
}

/// `pbps unlock` — clear a lock left behind by a process that died.
pub fn cmd_unlock(project: &Project, target: &Target) -> anyhow::Result<()> {
    db::require_mssql(project, "unlock")?;
    db::runtime()?.block_on(async {
        let mut conn = Conn::connect(target.connection())
            .await
            .context("cannot connect to the database")?;
        let holder = pbps_mssql::state::lock_holder(&mut conn).await?;
        match pbps_mssql::state::unlock(&mut conn).await? {
            true => {
                let who = holder
                    .map(|h| format!("`{}` since {}", h.locked_by, h.locked_at))
                    .unwrap_or_else(|| "an unnamed holder".to_owned());
                println!("Released the lock on `{}`, held by {who}.", target.label);
                // Nothing about a released lock proves the operation it guarded
                // finished, and a half-finished apply is exactly what drift
                // detection is for.
                println!("Run `pbps verify` before applying anything else.");
            }
            false => println!("`{}` was not locked.", target.label),
        }
        Ok(())
    })
}

/// `pbps plan --db` — the applyable plan, computed against the target
/// environment as queried (SPEC §7.3).
///
/// This is the deployment layer, so it changes no files. The identity file was
/// settled when the MR was reviewed; a `plan --db` that quietly rewrote it
/// would mean the artifact the gate approves was computed against a mapping
/// nobody read.
pub fn cmd_plan_db(
    project: &Project,
    target: &Target,
    out: Option<&std::path::Path>,
    sql_out: Option<&std::path::Path>,
    staged: bool,
) -> anyhow::Result<()> {
    db::require_mssql(project, "plan --db")?;
    let loaded = crate::load(project)?;
    let ids = crate::read_ids(project)?;
    let dialect = crate::dialect(project)?;
    let created_at = crate::now();

    let resolved =
        match pbps_diff::resolve(&loaded.schema, &ids, &loaded.intents, &crate::context()) {
            Ok(r) => r,
            Err(blockers) => {
                eprintln!("{}", crate::report::blockers(&blockers));
                bail!("some changes could not be decided automatically");
            }
        };
    if resolved.ids != ids {
        bail!(
            "the identity file is out of date; run `pbps plan` locally and commit `{}`.\n\
             A deployment plan must be computed against the identity its reviewers read.",
            project.ids_file().display()
        );
    }

    let (cs, baseline_checksum, baseline_description) = db::runtime()?.block_on(async {
        let mut conn = Conn::connect(target.connection())
            .await
            .context("cannot connect to the database")?;

        let Some(entry) = pbps_mssql::state::latest(&mut conn).await? else {
            bail!(
                "`{}` has a ledger but no entries; there is nothing to plan against.\n\
                 Record one with `pbps baseline --reason ...`.",
                target.label
            );
        };
        refuse_mid_deployment(&entry, &target.label)?;
        let recorded_ids = entry.snapshot.ids.clone();
        // The baseline's module scope is the **recorded** state's, never the
        // declarations': `apply` has only the plan file and the ledger, so a
        // scope that needed a checkout would make the two checksums disagree on
        // a host with none. A declared module that exists but was never
        // recorded is simply created again, which `CREATE OR ALTER` makes
        // harmless.
        let recorded_modules = managed_modules(Some(&entry.snapshot), None);
        let scoped = managed_state(
            &mut conn,
            &recorded_ids,
            &recorded_modules,
            project.config.unmanaged,
        )
        .await?;

        // The plan is computed against the environment *as queried*, so the
        // queried state had better be the recorded one. When it is not, the
        // plan would be pinned to a checksum that describes neither — and the
        // difference is drift, which has its own command and its own three
        // remedies.
        let live = pbps_model::state_checksum(&scoped.schema, &recorded_ids);
        let recorded = pbps_model::state_checksum(&entry.snapshot.schema, &recorded_ids);
        if live != recorded {
            bail!(
                "`{}` has drifted from the state recorded at {} (entry #{}).\n\
                 Run `pbps verify --env/--db ...` to see how, and resolve it before planning.",
                target.label,
                entry.applied_at,
                entry.id
            );
        }

        let cs = pbps_diff::diff(
            pbps_diff::Side {
                schema: &scoped.schema,
                ids: &recorded_ids,
            },
            pbps_diff::Side {
                schema: &loaded.schema,
                ids: &resolved.ids,
            },
            dialect.as_ref(),
            &loaded.hints,
        )
        .map_err(|errs| {
            for e in &errs {
                eprintln!("  {e}");
            }
            anyhow::anyhow!("{} change(s) cannot be expressed", errs.len())
        })?;

        // The edition is a connection-time fact, and it is the only place the
        // two edition-dependent questions of ADR-0003 can be answered
        // honestly: whether ONLINE will be accepted at all, and whether an
        // addition that is metadata-only on Enterprise rewrites every row
        // here. An offline plan has to assume the conservative answer.
        let edition = pbps_mssql::edition::edition(&mut conn).await?;
        let refused = pbps_mssql::edition::online_not_supported(&cs, &edition);
        if !refused.is_empty() {
            bail!(
                "`strategy: online` is declared for {}, and `{}` runs {}, which has no online \
                 index operations.\n\
                 The statement would fail partway through the apply. Remove the hint, or deploy \
                 this change to an edition that supports it.",
                refused.join(", "),
                target.label,
                edition.name()
            );
        }
        for w in pbps_mssql::edition::size_of_data_warnings(&cs, &edition) {
            eprintln!("warning: {w}");
        }

        Ok((
            cs,
            live,
            format!("{} as queried (entry #{})", target.label, entry.id),
        ))
    })?;

    let statements = crate::statements(&cs, dialect.as_ref())?;
    if staged {
        // A staged plan is one logical change isolated in a deployment of its
        // own (ADR-0003). The limit is the whole point: what cannot be rolled
        // back must not be able to take four unrelated changes down with it,
        // and a resume that had to reason about which of five changes were
        // half-done would be guessing.
        if cs.changes.len() > 1 {
            bail!(
                "--staged applies one logical change, and this plan has {}.\n\
                 Stage the change that needs it in a revision of its own; the rest can go \
                 through an ordinary transactional apply.",
                cs.changes.len()
            );
        }
        if cs.is_empty() {
            bail!("there is nothing to stage: this plan is empty");
        }
    } else {
        // §7.5: a statement that cannot run inside a transaction fails **here**,
        // not halfway through an apply with no way back.
        reject_non_transactional(&statements)?;
    }

    println!("Baseline: {baseline_description}");
    print!("{}", crate::report::plan(&cs));

    let mut plan = pbps_model::SavedPlan::new(
        pbps_model::PlanOrigin::Database,
        dialect.name(),
        created_at,
        pbps_model::PlanBaseline {
            description: baseline_description,
            checksum: baseline_checksum,
        },
        cs,
        resolved.ids,
    );
    plan.git_sha = db::git_sha();
    if staged {
        plan = plan.staged();
        println!(
            "\nThis is a staged plan: {} statement(s) will run outside a transaction, each \n\
             recorded in the ledger as it completes. Apply it with `pbps apply --staged`, and \n\
             continue an interrupted run with `--staged --resume`.",
            statements.len()
        );
    }

    if let Some(path) = out {
        crate::write_plan(path, &plan)?;
        println!("\nwrote {} (checksum {})", path.display(), plan.checksum());
    }
    if let Some(path) = sql_out {
        let script = format!(
            "-- Generated by pbps against {}\n-- Applyable only through `pbps apply --plan`; never hand-edit this file.\n\n{}",
            plan.baseline.description,
            pbps_dialect::render_script(&statements, dialect.batch_separator())
        );
        std::fs::write(path, script)
            .with_context(|| format!("cannot write `{}`", path.display()))?;
        println!("wrote {}", path.display());
    }
    if out.is_none() {
        println!(
            "\nThis plan was not saved. `--out plan.json` writes the artifact `pbps apply` takes."
        );
    }
    Ok(())
}

/// `pbps apply` — run an approved plan (SPEC §7.3, §7.5).
pub fn cmd_apply(
    project: &Project,
    target: &Target,
    plan_path: &std::path::Path,
    allow: &std::collections::BTreeSet<pbps_model::RiskClass>,
    staged: bool,
    resume: bool,
) -> anyhow::Result<()> {
    db::require_mssql(project, "apply")?;
    let dialect = crate::dialect(project)?;
    let operator = crate::operator();

    let raw = std::fs::read_to_string(plan_path)
        .with_context(|| format!("cannot read `{}`", plan_path.display()))?;
    let plan: pbps_model::SavedPlan = serde_json::from_str(&raw)
        .with_context(|| format!("`{}` is not a pbps plan", plan_path.display()))?;
    let plan_checksum = plan.checksum();

    if plan.version != pbps_model::plan::CURRENT_VERSION {
        bail!(
            "`{}` is a version {} plan and this tool understands version {}",
            plan_path.display(),
            plan.version,
            pbps_model::plan::CURRENT_VERSION
        );
    }
    // A preview's baseline is a git revision, so its checksum describes
    // something that is not this environment at all. There is no flag for this:
    // comparing it would be theatre.
    if !plan.origin.is_applyable() {
        bail!(
            "`{}` is an offline preview, not an applyable plan.\n\
             Recompute it against the target with `pbps plan --db ... --out {}`.",
            plan_path.display(),
            plan_path.display()
        );
    }
    if plan.dialect != dialect.name() {
        bail!(
            "`{}` was computed for {} and this project is {}",
            plan_path.display(),
            plan.dialect,
            dialect.name()
        );
    }
    // The mode lives in the file because that is what the gate approved; the
    // flag exists so that the CI configuration says out loud which kind of
    // deployment this is. A disagreement between them is somebody's mistake,
    // and running whichever was typed would be the tool choosing the loser.
    if plan.mode.is_staged() != staged {
        bail!(
            "`{}` is a {} plan and `--staged` was {}.\n\
             A staged plan runs outside a transaction and is applied with `--staged`; a \
             transactional one is not.",
            plan_path.display(),
            plan.mode,
            if staged { "given" } else { "not given" }
        );
    }
    if resume && !staged {
        bail!("--resume continues a staged apply; pass --staged as well");
    }

    if plan.changes.is_empty() {
        println!("The plan is empty; nothing to apply.");
        return Ok(());
    }

    // The gate. It can be this coarse precisely because the checksum pins the
    // plan: what `--allow destructive` approves is this plan's destruction and
    // no other (SPEC §7.3).
    let unapproved = plan.changes.unapproved_risks(allow);
    if !unapproved.is_empty() {
        let names: Vec<&str> = unapproved.iter().map(|r| r.as_str()).collect();
        bail!(
            "this plan carries risks that were not approved: {}.\n\
             Read {} and, if it is what you intend, re-run with --allow {}",
            names.join(", "),
            plan_path.display(),
            plan.changes
                .risks()
                .iter()
                .map(|r| r.as_str())
                .collect::<Vec<_>>()
                .join(",")
        );
    }

    let statements = crate::statements(&plan.changes, dialect.as_ref())?;
    reject_non_transactional(&statements)?;
    let targets = pbps_mssql::impact::RenameTarget::from_changes(&plan.changes);

    let recorded = db::runtime()?.block_on(async {
        let mut conn = Conn::connect(target.connection())
            .await
            .context("cannot connect to the database")?;

        // The lock comes first, before the checks and not after them: a
        // pre-flight that passed while another pipeline was mid-apply would
        // have been answered about a database that is already moving.
        pbps_mssql::state::lock(&mut conn, &operator).await?;
        let result = if staged {
            apply_staged_under_lock(
                &mut conn,
                project,
                target,
                &plan,
                &plan_checksum,
                &statements,
                &targets,
                dialect.as_ref(),
                &operator,
                resume,
            )
            .await
        } else {
            apply_under_lock(
                &mut conn,
                project,
                target,
                &plan,
                &plan_checksum,
                &statements,
                &targets,
                dialect.as_ref(),
                &operator,
            )
            .await
        };
        // Released whatever happened. A lock left behind by a failed apply
        // blocks the very pipeline that would fix it.
        let released = pbps_mssql::state::unlock(&mut conn).await;
        let recorded = result?;
        released?;
        Ok::<i64, anyhow::Error>(recorded)
    })?;

    println!(
        "Applied {} change(s) to `{}`{}; recorded as entry #{recorded}.",
        plan.changes.changes.len(),
        target.label,
        if staged { " (staged)" } else { "" }
    );
    if let Some(hook) = &project.config.hooks.on_apply {
        crate::hooks::run(hook, &raw, "on_apply");
    }
    Ok(())
}

/// Everything between taking the lock and releasing it. Returns the ledger id.
#[allow(clippy::too_many_arguments)]
async fn apply_under_lock(
    conn: &mut Conn,
    project: &Project,
    target: &Target,
    plan: &pbps_model::SavedPlan,
    plan_checksum: &str,
    statements: &[pbps_dialect::Statement],
    rename_targets: &[pbps_mssql::impact::RenameTarget],
    dialect: &dyn pbps_dialect::Dialect,
    operator: &str,
) -> anyhow::Result<i64> {
    let Some(entry) = pbps_mssql::state::latest(conn).await? else {
        bail!(
            "`{}` has a ledger but no entries; a plan cannot be pinned to a state that was never recorded.",
            target.label
        );
    };
    refuse_mid_deployment(&entry, &target.label)?;
    let recorded_ids = entry.snapshot.ids.clone();
    let recorded_modules = managed_modules(Some(&entry.snapshot), None);
    let scoped = managed_state(
        conn,
        &recorded_ids,
        &recorded_modules,
        project.config.unmanaged,
    )
    .await?;

    // The drift check, and the whole reason a coarse `--allow` is safe: this
    // plan is only valid against the environment it was computed against, down
    // to the identity mapping.
    let live = pbps_model::state_checksum(&scoped.schema, &recorded_ids);
    if live != plan.baseline.checksum {
        bail!(
            "`{}` is no longer the database this plan was computed against.\n\
             plan baseline: {}\n\
             database now:  {live}\n\
             Something changed since the plan was approved. `pbps verify` shows what; \
             then recompute the plan with `pbps plan --db`.",
            target.label,
            plan.baseline.checksum
        );
    }

    preflight(conn, dialect, plan, rename_targets).await?;

    println!("Applying {} statement(s)...", statements.len());
    run_in_transaction(conn, statements).await?;

    // What gets recorded is the database read back, not the plan applied to the
    // old state. Expressions come back in the engine's stored form, and only
    // that form compares equal on the next drift check (SPEC §8.2).
    let after = managed_state(
        conn,
        &plan.ids,
        &modules_after(&entry.snapshot, &plan.changes),
        project.config.unmanaged,
    )
    .await?;
    let mut snapshot = pbps_model::StateSnapshot::new(
        pbps_model::StateKind::Apply,
        after.schema,
        // The plan's mapping, not the baseline's: after a rename the two
        // disagree, and recording the old one would say the rename never
        // happened — leaving the next plan to propose it all over again.
        plan.ids.clone(),
        operator,
    );
    snapshot.git_sha = plan.git_sha.clone().or_else(db::git_sha);
    snapshot.plan_checksum = Some(plan_checksum.to_owned());
    Ok(pbps_mssql::state::record(conn, &snapshot).await?)
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
#[allow(clippy::too_many_arguments)]
async fn apply_staged_under_lock(
    conn: &mut Conn,
    project: &Project,
    target: &Target,
    plan: &pbps_model::SavedPlan,
    plan_checksum: &str,
    statements: &[pbps_dialect::Statement],
    rename_targets: &[pbps_mssql::impact::RenameTarget],
    dialect: &dyn pbps_dialect::Dialect,
    operator: &str,
    resume: bool,
) -> anyhow::Result<i64> {
    let Some(entry) = pbps_mssql::state::latest(conn).await? else {
        bail!(
            "`{}` has a ledger but no entries; a plan cannot be pinned to a state that was never recorded.",
            target.label
        );
    };

    let start = if resume {
        let progress = match (&entry.snapshot.staged, entry.snapshot.kind) {
            (Some(p), StateKind::Staged) => p.clone(),
            _ => bail!(
                "`{}` has no staged apply in progress: its newest entry (#{}) is an ordinary \
                 {} state.\n\
                 Run `pbps apply --staged` without `--resume` to start this plan.",
                target.label,
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
        // The drift check, applied to a half-finished plan. The mapping used is
        // the plan's on both sides, so the comparison is like with like
        // whichever statement the run stopped after.
        let after_modules = modules_after(&entry.snapshot, &plan.changes);
        let scoped =
            managed_state(conn, &plan.ids, &after_modules, project.config.unmanaged).await?;
        let live = pbps_model::state_checksum(&scoped.schema, &plan.ids);
        let checkpoint = pbps_model::state_checksum(&entry.snapshot.schema, &plan.ids);
        if live != checkpoint {
            bail!(
                "`{}` has moved since the checkpoint at entry #{}.\n\
                 checkpoint: {checkpoint}\n\
                 database:   {live}\n\
                 Something changed while this deployment was half-finished. `pbps verify` shows \
                 what; the remaining statements must not run against a database nobody planned \
                 for.",
                target.label,
                entry.id
            );
        }
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
        progress.completed
    } else {
        if entry.snapshot.staged.is_some() {
            bail!(
                "`{}` is mid-deployment: entry #{} is a staged checkpoint ({} of {} statements).\n\
                 Finish it with `pbps apply --staged --resume --plan ...`, or take the database \
                 as it stands with `pbps baseline --reason ...`.",
                target.label,
                entry.id,
                entry.snapshot.staged.as_ref().map_or(0, |p| p.completed),
                entry.snapshot.staged.as_ref().map_or(0, |p| p.total)
            );
        }
        let recorded_ids = entry.snapshot.ids.clone();
        let recorded_modules = managed_modules(Some(&entry.snapshot), None);
        let scoped = managed_state(
            conn,
            &recorded_ids,
            &recorded_modules,
            project.config.unmanaged,
        )
        .await?;
        let live = pbps_model::state_checksum(&scoped.schema, &recorded_ids);
        if live != plan.baseline.checksum {
            bail!(
                "`{}` is no longer the database this plan was computed against.\n\
                 plan baseline: {}\n\
                 database now:  {live}\n\
                 Something changed since the plan was approved. `pbps verify` shows what; \
                 then recompute the plan with `pbps plan --db --staged`.",
                target.label,
                plan.baseline.checksum
            );
        }
        // Only on a fresh start. The probes name objects as the catalog had
        // them before the first statement, and after a partial run some of
        // those names have already moved — a probe answered about the wrong
        // object is worse than one that was not asked.
        preflight(conn, dialect, plan, rename_targets).await?;
        0
    };

    let total = statements.len();
    println!(
        "Applying {} statement(s) without a transaction...",
        total - start
    );
    for (i, stmt) in statements.iter().enumerate().skip(start) {
        if let Err(e) = conn.execute(&stmt.sql).await {
            return Err(anyhow::anyhow!(
                "the database rejected statement {} of {total}, and nothing was rolled back \
                 (a staged apply runs outside a transaction):\n{}\n\n{e}\n\n\
                 The ledger records everything that did complete. Fix the cause, then continue \
                 with `pbps apply --staged --resume`.",
                i + 1,
                stmt.sql
            ));
        }

        let after = managed_state(
            conn,
            &plan.ids,
            &modules_after(&entry.snapshot, &plan.changes),
            project.config.unmanaged,
        )
        .await?;
        let mut checkpoint = pbps_model::StateSnapshot::new(
            StateKind::Staged,
            after.schema,
            plan.ids.clone(),
            operator,
        );
        checkpoint.git_sha = plan.git_sha.clone().or_else(db::git_sha);
        checkpoint.plan_checksum = Some(plan_checksum.to_owned());
        checkpoint.staged = Some(pbps_model::StagedProgress {
            completed: i + 1,
            total,
            last_statement: stmt.sql.clone(),
        });
        let id = pbps_mssql::state::record(conn, &checkpoint).await?;
        println!("  statement {} of {total} done (checkpoint #{id})", i + 1);
    }

    // The closing entry is an ordinary apply with no staged marker: its absence
    // is what tells every later command this environment is no longer
    // mid-deployment.
    let after = managed_state(
        conn,
        &plan.ids,
        &modules_after(&entry.snapshot, &plan.changes),
        project.config.unmanaged,
    )
    .await?;
    let mut snapshot = pbps_model::StateSnapshot::new(
        pbps_model::StateKind::Apply,
        after.schema,
        plan.ids.clone(),
        operator,
    );
    snapshot.git_sha = plan.git_sha.clone().or_else(db::git_sha);
    snapshot.plan_checksum = Some(plan_checksum.to_owned());
    Ok(pbps_mssql::state::record(conn, &snapshot).await?)
}

/// Refuses to act on an environment that is half-way through a staged apply.
///
/// Planning or applying anything else on top of an unfinished staged plan
/// builds on a state nobody approved: the recorded baseline is a checkpoint,
/// not a deployment anybody signed off.
fn refuse_mid_deployment(entry: &pbps_db::LedgerEntry, label: &str) -> anyhow::Result<()> {
    let Some(progress) = &entry.snapshot.staged else {
        return Ok(());
    };
    bail!(
        "`{label}` is mid-deployment: entry #{} is a staged checkpoint ({} of {} statements).\n\
         Finish it with `pbps apply --staged --resume --plan ...`, or take the database as it \
         stands with `pbps baseline --reason ...`.",
        entry.id,
        progress.completed,
        progress.total
    )
}

/// The pre-flight of §7.5: dependency impact, then probes against the data.
async fn preflight(
    conn: &mut Conn,
    dialect: &dyn pbps_dialect::Dialect,
    plan: &pbps_model::SavedPlan,
    rename_targets: &[pbps_mssql::impact::RenameTarget],
) -> anyhow::Result<()> {
    let mut blocked = Vec::new();
    for target in rename_targets {
        let report = pbps_mssql::impact::rename_impact(conn, target).await?;
        if report.is_empty() {
            continue;
        }
        println!("\nRenaming {} affects:", report.target);
        for r in &report.advisory {
            let detail = r
                .detail
                .as_deref()
                .map(|d| format!(" — {d}"))
                .unwrap_or_default();
            println!("  {} {}{detail}", r.kind, r.name);
        }
        for r in &report.blocking {
            println!(
                "  {} {} — SCHEMABINDING, which blocks the rename",
                r.kind, r.name
            );
            blocked.push(format!("{} {}", r.kind, r.name));
        }
        if !report.advisory.is_empty() {
            // The list is of what the catalog can see. Applications, reports
            // and downstream ELT are invisible to any query, and implying
            // otherwise is worse than saying nothing.
            println!(
                "  Nothing outside the database is visible here: applications and downstream \
                 consumers need a human's checklist."
            );
        }
    }
    if !blocked.is_empty() {
        bail!(
            "the engine will refuse this rename while these exist: {}.\n\
             Drop or recreate them without SCHEMABINDING first.",
            blocked.join(", ")
        );
    }

    let mut failures = Vec::new();
    let mut ran = 0usize;
    let probes = dialect.preflight(&plan.changes);
    for probe in &probes {
        ran += 1;
        // A probe can still legitimately fail to run — a check whose expression
        // names a column this plan renames, say, since expression text is never
        // rewritten by substitution. That is not a violation and it is not
        // silence either: the engine enforces the constraint inside the
        // transaction, where a failure rolls everything back.
        let rows = match conn.query(&probe.sql).await {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!(
                    "warning: could not check {} ({e}); the engine will enforce it during the apply",
                    probe.description
                );
                continue;
            }
        };
        let count = rows
            .first()
            .and_then(|r| r.try_get::<i32, _>(0).ok().flatten())
            .unwrap_or(0);
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
    if ran > 0 {
        println!("Pre-flight: {ran} probe(s) passed against the live data.");
    }

    // "No probe" is not "no risk". Saying so keeps the operator's attention
    // where the approval already put it, rather than letting a clean pre-flight
    // read as a clean bill of health.
    let unprobed: Vec<&str> = plan
        .changes
        .risks()
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
        println!(
            "Pre-flight: {} risk(s) cannot be probed and rest on the approval alone.",
            unprobed.join(", ")
        );
    }
    Ok(())
}

/// §7.5: a plan containing a non-transactional statement fails before it runs.
fn reject_non_transactional(statements: &[pbps_dialect::Statement]) -> anyhow::Result<()> {
    let offenders: Vec<&str> = statements
        .iter()
        .filter(|s| !s.transactional)
        .map(|s| s.sql.as_str())
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

/// Runs statements as one transaction: all or nothing (SPEC §7.5).
///
/// The rollback is attempted on every failure path and its own error is
/// deliberately not allowed to replace the original one — the statement that
/// broke is what the operator needs to see.
pub async fn run_in_transaction(
    conn: &mut Conn,
    statements: &[pbps_dialect::Statement],
) -> anyhow::Result<()> {
    conn.begin().await.context("cannot open a transaction")?;
    for stmt in statements {
        if let Err(e) = conn.execute(&stmt.sql).await {
            let _ = conn.rollback().await;
            return Err(anyhow::anyhow!(
                "the database rejected this statement, and the whole plan was rolled back:\n\
                 {}\n\n{e}",
                stmt.sql
            ));
        }
    }
    if let Err(e) = conn.commit().await {
        let _ = conn.rollback().await;
        return Err(anyhow::Error::new(e).context("the transaction could not be committed"));
    }
    Ok(())
}

/// Stamps a snapshot with where it came from.
fn with_provenance(mut snapshot: StateSnapshot) -> StateSnapshot {
    snapshot.git_sha = db::git_sha();
    snapshot
}
