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
    unmanaged: pbps_config::Unmanaged,
) -> anyhow::Result<pbps_diff::Scoped> {
    let pulled = pbps_mssql::catalog::introspect(conn)
        .await
        .context("cannot read the database catalog")?;
    for w in &pulled.warnings {
        eprintln!("warning: {w}");
    }

    let scoped = pbps_diff::scope(&pulled.schema, ids);
    report_unmanaged(&scoped, unmanaged)?;
    Ok(scoped)
}

/// Applies the `unmanaged:` policy of SPEC §8.2 to what fell outside the scope.
fn report_unmanaged(
    scoped: &pbps_diff::Scoped,
    policy: pbps_config::Unmanaged,
) -> anyhow::Result<()> {
    if scoped.unmanaged.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = scoped.unmanaged.iter().map(ToString::to_string).collect();
    match policy {
        pbps_config::Unmanaged::Ignore => {}
        pbps_config::Unmanaged::Warn => eprintln!(
            "warning: {} table(s) in this database are not declared and are left alone: {}",
            names.len(),
            names.join(", ")
        ),
        pbps_config::Unmanaged::Error => bail!(
            "`unmanaged: error` in pbps.yml, and {} table(s) here are not declared: {}.\n\
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

/// `pbps snapshot` — record the current state, refusing to bless a difference.
pub fn cmd_snapshot(project: &Project, target: &Target, force: bool) -> anyhow::Result<()> {
    db::require_mssql(project, "snapshot")?;
    let ids = crate::read_ids(project)?;
    let operator = crate::operator();

    db::runtime()?.block_on(async {
        let mut conn = Conn::connect(target.connection())
            .await
            .context("cannot connect to the database")?;
        let scoped = managed_state(&mut conn, &ids, project.config.unmanaged).await?;
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
    let operator = crate::operator();

    db::runtime()?.block_on(async {
        let mut conn = Conn::connect(target.connection())
            .await
            .context("cannot connect to the database")?;
        let scoped = managed_state(&mut conn, &ids, project.config.unmanaged).await?;
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
        let existing = managed_state(&mut conn, &ids, project.config.unmanaged).await?;
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
        let built = managed_state(&mut conn, &ids, project.config.unmanaged).await?;
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
