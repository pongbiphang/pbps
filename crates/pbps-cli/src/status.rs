//! `pbps status` — one screen across every environment (SPEC §9.4).
//!
//! # Why there is nothing to host
//!
//! Every environment already self-reports: its ledger is in its own database
//! (§8.1). So the dashboard is a query, not a service — no agent, no cloud, no
//! state leaving the team's estate. Atlas answers this need with an agent
//! reporting to its own cloud; here the same answer costs one command.
//!
//! # Why it always exits zero
//!
//! `status` is a report, not a gate. An environment that has drifted is
//! information here and a failure in `pbps verify`, which is the command CI
//! runs and the one with an exit code that means something (see
//! [`crate::Found`]). Two commands failing on the same condition would
//! make the pipeline's intent ambiguous — and a `status` that failed because
//! one of six environments was unreachable would be useless for exactly the
//! situation it is best at.

use pbps_config::Project;
use pbps_db::Conn;

use crate::{db, output};

/// One environment's line. Serialized as-is for `--format json`, so anyone who
/// wants their own web view has a stable shape to render.
#[derive(Debug, serde::Serialize)]
pub struct EnvStatus {
    pub environment: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// `ok`, `warning`, `policy`, `failed`, `staged`, `drift`, `uninitialized`,
    /// `unreachable` or `unconfigured`.
    pub state: &'static str,

    /// What went wrong, when something did. Never a connection string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_entry: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,

    /// Set when an apply is in progress, or when one died without releasing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locked_by: Option<String>,

    /// Set when the lock could not be read, so an absent `locked_by` means
    /// "not determined" rather than "no lock held".
    ///
    /// The same distinction `doctor` and `explain` make, and for the same
    /// reason: a lock that could not be read is not an absent lock, and telling
    /// an operator the environment is free when nobody looked is the one answer
    /// this row must never give.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_unknown: Option<String>,

    /// When this line was produced, by the reporting machine's clock.
    pub checked_at: String,

    /// Independently discovered verdicts that do not win the single-state
    /// human summary. They are omitted from the row because the stable JSON
    /// representation already exposes them through `findings`.
    #[serde(skip)]
    issues: Vec<StatusIssue>,
}

#[derive(Debug)]
struct StatusIssue {
    state: &'static str,
    detail: String,
}

impl EnvStatus {
    fn failed(environment: &str, state: &'static str, detail: String, checked_at: &str) -> Self {
        Self {
            environment: environment.to_owned(),
            description: None,
            state,
            detail: Some(detail),
            last_entry: None,
            last_kind: None,
            applied_at: None,
            git_sha: None,
            operator: None,
            locked_by: None,
            lock_unknown: None,
            checked_at: checked_at.to_owned(),
            issues: Vec::new(),
        }
    }
}

pub fn cmd_status(project: &Project, json: bool) -> anyhow::Result<()> {
    if project.config.environments.is_empty() {
        // Still through the envelope when JSON was asked for. A consumer that
        // got prose here would have to special-case the one project shape it is
        // most likely to meet first — and it would meet it as a parse error,
        // which reads as "the tool broke" rather than "there is nothing to
        // report" (SPEC §9.8).
        if json {
            let report = output::Report::new(
                "status",
                vec![
                    output::Finding::warning(
                        "project.no-environments",
                        "no environments are configured",
                    )
                    .at(project.config_file(), None)
                    .remedy("add `environments:` to pbps.yml with `url_env:` naming the variable"),
                ],
                Some(Vec::<EnvStatus>::new()),
            );
            println!("{}", serde_json::to_string_pretty(&report)?);
            return Ok(());
        }
        println!(
            "No environments are configured. Add them to pbps.yml:\n\n\
             environments:\n  prod:\n    url_env: PROD_CONN\n\n\
             The variable name goes in the file, never the connection string."
        );
        return Ok(());
    }
    output::or_unanswerable(
        "status",
        json,
        "project.unsupported-dialect",
        db::require_mssql(project, "status"),
    )?;
    let checked_at = crate::now();

    let rt = output::or_unanswerable("status", json, "runtime.unavailable", db::runtime())?;
    let mut rows = Vec::new();
    for (name, environment) in &project.config.environments {
        // Each environment is reported independently. One unreachable database
        // must not cost the operator the other five lines — being able to see
        // the whole estate at once is the entire point of the command.
        let connection = match environment.connection_string(name) {
            Ok(c) => c,
            Err(e) => {
                rows.push(EnvStatus::failed(
                    name,
                    "unconfigured",
                    e.to_string(),
                    &checked_at,
                ));
                continue;
            }
        };
        let mut row = rt.block_on(one(
            &connection,
            name,
            &checked_at,
            project.config.unmanaged,
        ));
        row.description = environment.description.clone();
        rows.push(row);
    }

    if json {
        let findings = findings(&rows);
        println!(
            "{}",
            serde_json::to_string_pretty(&output::Report::new("status", findings, Some(&rows)))?
        );
    } else {
        print!("{}", render(&rows));
    }
    Ok(())
}

/// The lock, as the two row fields carry it: who holds it, or why that could
/// not be determined.
///
/// `.ok().flatten()` here read an unreadable lock as no lock, which is the same
/// mistake this branch corrected in `doctor` and then in `explain`. Kept apart
/// from `state` on purpose: the ledger answered, so the row is still worth
/// printing — what is undetermined is only the lock.
///
/// A function rather than two copies of the match, because it is now asked at
/// two points in `one` and the copies would have to stay identical about which
/// of the two failure readings is the safe one.
async fn read_lock(conn: &mut Conn) -> (Option<String>, Option<String>) {
    match pbps_mssql::state::lock_holder(conn).await {
        Ok(held) => (
            held.map(|l| format!("{} since {}", l.locked_by, l.locked_at)),
            None,
        ),
        Err(e) => (None, Some(e.to_string())),
    }
}

async fn one(
    connection: &str,
    name: &str,
    checked_at: &str,
    unmanaged: pbps_config::Unmanaged,
) -> EnvStatus {
    let mut conn = match Conn::connect(connection).await {
        Ok(c) => c,
        Err(e) => return EnvStatus::failed(name, "unreachable", e.to_string(), checked_at),
    };

    let entry = match pbps_mssql::state::latest(&mut conn).await {
        Ok(Some(entry)) => entry,
        // The ledger exists and is empty, which is exactly what a *first*
        // `bootstrap` looks like while it runs: `state::lock` calls
        // `ensure_tables` before anything records a snapshot, so the lock is
        // taken and the ledger is still empty. Returning here without reading
        // it reported "uninitialized" and nothing else — hiding both the apply
        // in flight and the stale lock an interrupted first bootstrap leaves
        // behind, in the human view and the JSON alike.
        Ok(None) => {
            let mut row = EnvStatus::failed(
                name,
                "uninitialized",
                "the ledger exists but has no entries".to_owned(),
                checked_at,
            );
            (row.locked_by, row.lock_unknown) = read_lock(&mut conn).await;
            return row;
        }
        // And here too. `NotInitialized` means `dbo.__pbps_state` is absent,
        // and no path *the tool controls* takes a lock without `ensure_tables`
        // creating that table first — but a hand-dropped state table leaves the
        // lock behind, live row and all, which is the same half-present ledger
        // `doctor` already reports on. The next apply recreates the state table
        // and then fails to take the lock, so hiding it here costs the operator
        // the one line that explains that failure.
        //
        // Safe on a database pbps has never touched, because `lock_holder` now
        // asks whether the lock table exists first: absent answers `None`,
        // which is a different thing from unreadable, which stays an error.
        Err(pbps_db::LedgerError::NotInitialized) => {
            let mut row = EnvStatus::failed(
                name,
                "uninitialized",
                "pbps has never recorded a state here".to_owned(),
                checked_at,
            );
            (row.locked_by, row.lock_unknown) = read_lock(&mut conn).await;
            return row;
        }
        Err(e) => return EnvStatus::failed(name, "unreachable", e.to_string(), checked_at),
    };

    let (locked_by, lock_unknown) = read_lock(&mut conn).await;

    let recorded_ids = entry.snapshot.ids.clone();
    let mut row = EnvStatus {
        environment: name.to_owned(),
        description: None,
        state: "ok",
        detail: None,
        last_entry: Some(entry.id),
        last_kind: Some(entry.snapshot.kind.to_string()),
        applied_at: Some(entry.applied_at.clone()),
        git_sha: entry.snapshot.git_sha.clone(),
        operator: Some(entry.snapshot.operator.clone()),
        locked_by,
        lock_unknown,
        checked_at: checked_at.to_owned(),
        issues: Vec::new(),
    };

    if entry.snapshot.kind == pbps_model::StateKind::Failed {
        row.state = "failed";
        row.detail = Some(match &entry.snapshot.reason {
            Some(reason) => format!("the last deployment attempt failed: {reason}"),
            None => "the last deployment attempt failed".to_owned(),
        });
    }

    // An environment sitting on a staged checkpoint is mid-deployment, and that
    // is the first thing an operator needs to know about it: `plan --db` and
    // `apply` both refuse until it is finished, so a screen that read "ok"
    // would leave them puzzled at the refusal.
    if let Some(progress) = &entry.snapshot.staged {
        record_staged(
            &mut row,
            name,
            progress.completed,
            progress.total,
            entry.snapshot.plan_checksum.as_deref(),
        );
    }

    // The drift verdict is the checksum, computed exactly as `verify` computes
    // it. Two commands that disagreed about whether an environment has drifted
    // would be worse than one of them not existing.
    let pulled = match pbps_mssql::catalog::introspect(&mut conn).await {
        Ok(p) => p,
        Err(e) => {
            record_unreachable(&mut row, e.to_string());
            return row;
        }
    };
    // Scoped by what was recorded, exactly as `verify` scopes it: two commands
    // that disagreed about which objects are managed would disagree about
    // whether an environment has drifted.
    let recorded_modules = recorded_modules(&entry);
    let scoped = pbps_diff::scope(&pulled.schema, &recorded_ids, &recorded_modules);
    // The rows too, under the recorded scope, as `verify` reads them. Only the
    // read happens here; what it means is decided beside every other catalog
    // fact, below, so that a read which failed cannot hide any of them.
    let read: std::collections::BTreeMap<_, _> = entry
        .snapshot
        .schema
        .data_scopes()
        .iter()
        .map(|(n, s)| (n.clone(), s.rows_to_read()))
        .collect();
    let rows = pbps_mssql::catalog::read_rows(&mut conn, &scoped.schema, &read)
        .await
        .map_err(|e| format!("the declared rows could not be read back: {e}"));
    record_catalog_findings(&mut row, &entry, &pulled, &scoped, rows, unmanaged);
    row
}

fn recorded_modules(
    entry: &pbps_db::LedgerEntry,
) -> std::collections::BTreeSet<pbps_model::ModuleId> {
    entry.snapshot.schema.modules.keys().cloned().collect()
}

/// Everything `status` concludes about an environment from its catalog and its
/// rows, once the connection work is done.
///
/// Each finding here is established independently of the others, and none of
/// them may erase another: an unexpressible permission, a row that moved, a
/// fact the projection cannot hold, an object `unmanaged: error` refuses, and a
/// row read that failed. Returning on whichever came first hid every check
/// after it (DECISIONS 159, 168); the last such return was the row read's, and
/// the inventory below it needs only the catalog, which had already succeeded
/// — so a read that failed was reported *instead of* a stray object rather
/// than beside it. The read's failure therefore lands last, through
/// `record_unreachable`, which keeps what is already on the row
/// (DECISIONS 192).
///
/// Sync and connection-free so that a test can hand it a read that failed.
fn record_catalog_findings(
    row: &mut EnvStatus,
    entry: &pbps_db::LedgerEntry,
    pulled: &pbps_mssql::introspect::Pulled,
    scoped: &pbps_diff::Scoped,
    rows: Result<pbps_model::ObservedRows, String>,
    unmanaged: pbps_config::Unmanaged,
) {
    let recorded_ids = &entry.snapshot.ids;
    let recorded_modules = recorded_modules(entry);
    let name = row.environment.clone();

    // A permission on a managed role that the declarations cannot hold is
    // drift to `verify` and stops every command that would record a state;
    // it lives beside the schema, not in it, so the checksum below cannot
    // see it. The same filter `verify` applies: an unmanaged role's grants
    // are its own business (DECISIONS 95, 125).
    let unexpressible =
        crate::deploy::unexpressible_permissions(pulled, recorded_ids, &recorded_modules);
    if !unexpressible.is_empty() {
        record_drift(row, entry.id, &name);
        record_status_issue(row, "drift", unexpressible.join("; "));
    }

    // Decided here and recorded in two places: drift right away, so that it
    // keeps outranking an `unmanaged: warn` on the row; a failure at the very
    // end, so that nothing established in between is dropped.
    let moved = rows.and_then(|rows| {
        // Cloned, because the inventories below still need `scoped` whole and
        // `with_observed_rows` takes the schema by value.
        scoped
            .schema
            .clone()
            .with_observed_rows(
                &rows,
                &entry.snapshot.schema.data_scopes(),
                &entry.snapshot.schema,
            )
            // Two recorded spellings of one row: the recorded state cannot be
            // compared against the database, which is the same answer as a
            // read that failed, never "no drift".
            .map_err(|e| format!("the recorded rows cannot be compared: {e}"))
            .map(|live_schema| {
                pbps_model::state_checksum(&live_schema, recorded_ids)
                    != pbps_model::state_checksum(&entry.snapshot.schema, recorded_ids)
            })
    });
    if moved == Ok(true) {
        record_drift(row, entry.id, &name);
    }

    // The same inventory `verify` reports as unexpressible drift, including a
    // recorded module the catalog can no longer read back: one screen must not
    // call an environment clean where the other calls it drifted.
    let limitations = crate::deploy::managed_limitations(pulled, recorded_ids, &recorded_modules);
    if !limitations.is_empty() {
        record_status_issue(
            row,
            "drift",
            format!(
                "{} fact(s) inside the managed set could not be compared: {}",
                limitations.len(),
                limitations.join(", ")
            ),
        );
    }

    let unreadable = crate::deploy::unreadable_modules(&pulled.unmanaged_modules);
    let unmanaged_objects =
        crate::deploy::unmanaged_objects(scoped, &unreadable, &recorded_modules);
    if unmanaged != pbps_config::Unmanaged::Ignore && !unmanaged_objects.is_empty() {
        record_status_issue(
            row,
            if unmanaged == pbps_config::Unmanaged::Error {
                "policy"
            } else {
                "warning"
            },
            format!(
                "`unmanaged: {}` sees {} object(s) outside the managed set: {}",
                if unmanaged == pbps_config::Unmanaged::Error {
                    "error"
                } else {
                    "warn"
                },
                unmanaged_objects.len(),
                unmanaged_objects.join(", ")
            ),
        );
    }

    // Last, on top of everything above. A read that failed is reported as the
    // failure it is: never as "no drift", and never as the only thing wrong.
    if let Err(why) = moved {
        record_unreachable(row, why);
    }
}

/// Makes an interrupted staged apply the primary human state while retaining
/// a failed ledger entry as a separate machine-readable finding.
fn record_staged(
    row: &mut EnvStatus,
    environment: &str,
    completed: usize,
    total: usize,
    checksum: Option<&str>,
) {
    let failure = row.detail.take();
    if row.state == "failed"
        && let Some(detail) = &failure
    {
        record_supplemental_issue(row, "failed", detail);
    }
    row.state = "staged";

    // Through `env_arg`, like every other command this tool prints. An
    // environment name is a YAML map key, so `US West` is valid, and
    // interpolated bare it becomes multiple arguments or executable syntax.
    let mut detail = format!(
        "a staged apply stopped after {completed} of {total} statement(s); continue it with \
         `pbps apply --staged --resume --env {} --plan ... --checksum {}`",
        crate::report::env_arg(environment),
        checksum.map_or_else(
            || crate::report::placeholder("approved-checksum"),
            str::to_owned
        )
    );
    if let Some(failure) = failure {
        append_detail(&mut detail, &failure);
    }
    row.detail = Some(detail);
}

/// Adds an independently discovered state issue without hiding a staged apply
/// or another, more important verdict already on the row.
fn record_status_issue(row: &mut EnvStatus, state: &'static str, detail: String) {
    if row.state == "ok" {
        row.state = state;
        row.detail = Some(detail);
        return;
    }
    if row.state != state {
        record_supplemental_issue(row, state, &detail);
    }
    let previous = row.detail.take().unwrap_or_default();
    row.detail = Some(if previous.is_empty() {
        detail
    } else {
        format!("{previous} — {detail}")
    });
}

fn record_supplemental_issue(row: &mut EnvStatus, state: &'static str, detail: &str) {
    match row.issues.iter_mut().find(|issue| issue.state == state) {
        Some(issue) => append_detail(&mut issue.detail, detail),
        None => row.issues.push(StatusIssue {
            state,
            detail: detail.to_owned(),
        }),
    }
}

fn append_detail(existing: &mut String, detail: &str) {
    if !existing.is_empty() {
        existing.push_str(" — ");
    }
    existing.push_str(detail);
}

/// Makes a failed catalog read the primary status without discarding facts
/// already established from the ledger. In particular, a failed deployment is
/// still failed when the principal can read `__pbps_state` but not `sys.*`.
fn record_unreachable(row: &mut EnvStatus, detail: String) {
    let previous_state = row.state;
    let previous_detail = row.detail.take();
    let mut detail = detail;
    if previous_state != "ok"
        && previous_state != "unreachable"
        && let Some(previous_detail) = &previous_detail
    {
        record_supplemental_issue(row, previous_state, previous_detail);
    }
    if let Some(previous_detail) = previous_detail {
        append_detail(&mut detail, &previous_detail);
    }
    row.state = "unreachable";
    row.detail = Some(detail);
}

/// Notes a checksum mismatch on a row, without ever displacing `staged`.
///
/// An environment sitting on a staged checkpoint is mid-deployment whatever
/// else is true of it: `plan --db` and a fresh `apply` both still refuse, and
/// the way out is `--resume` or a new baseline. Overwriting the word with
/// "drift" would send the operator to the drift workflow instead, which cannot
/// finish the deployment they are actually in.
fn record_drift(row: &mut EnvStatus, entry_id: i64, name: &str) {
    if row.state == "staged" {
        let arg = crate::report::env_arg(name);
        record_status_issue(
            row,
            "drift",
            format!(
                "it has moved since that checkpoint, which `pbps verify --env {arg}` will show"
            ),
        );
        return;
    }
    if row.state != "ok" {
        let arg = crate::report::env_arg(name);
        record_status_issue(
            row,
            "drift",
            format!(
                "the database no longer matches entry #{entry_id}; run `pbps verify --env {arg}`"
            ),
        );
        return;
    }
    row.state = "drift";
    let arg = crate::report::env_arg(name);
    row.detail = Some(format!(
        "the database no longer matches entry #{entry_id}; run `pbps verify --env {arg}`"
    ));
}

/// One line per environment, aligned so a wide estate stays scannable.
fn render(rows: &[EnvStatus]) -> String {
    let width = |f: fn(&EnvStatus) -> String| {
        rows.iter()
            .map(|r| f(r).chars().count())
            .max()
            .unwrap_or(0)
            .max(1)
    };
    let env_w = width(|r| r.environment.clone()).max("ENVIRONMENT".len());
    let state_w = width(|r| r.state.to_owned()).max("STATE".len());
    let when_w = width(|r| r.applied_at.clone().unwrap_or_default()).max("LAST RECORDED".len());

    let mut out = format!(
        "{:env_w$}  {:state_w$}  {:when_w$}  {:9}  {}\n",
        "ENVIRONMENT", "STATE", "LAST RECORDED", "GIT", "BY"
    );
    for r in rows {
        out.push_str(&format!(
            "{:env_w$}  {:state_w$}  {:when_w$}  {:9}  {}\n",
            r.environment,
            r.state,
            r.applied_at.as_deref().unwrap_or("-"),
            // Eight characters is what a person recognizes a commit by.
            r.git_sha
                .as_deref()
                .map(|s| &s[..s.len().min(8)])
                .unwrap_or("-"),
            r.operator.as_deref().unwrap_or("-"),
        ));
    }
    for r in rows {
        if let Some(detail) = &r.detail {
            out.push_str(&format!("\n  {}: {detail}\n", r.environment));
        }
        if let Some(lock) = &r.locked_by {
            // A lock is either an apply in flight or one that died. Both are
            // things an operator wants to know before they start typing.
            out.push_str(&format!("\n  {}: locked by {lock}\n", r.environment));
        }
        if let Some(why) = &r.lock_unknown {
            out.push_str(&format!(
                "\n  {}: could not tell whether an apply is running ({why})\n",
                r.environment
            ));
        }
    }
    out
}

/// The findings half of `status --format json`.
///
/// Extracted so it can be tested without a database: the disagreement it exists
/// to prevent — the human view warning about a lock the JSON view never
/// mentioned — is exactly the kind that only shows up when the two are compared,
/// and it was reachable only through a live server before this.
///
/// Every finding here is a warning, and deliberately so. `status` is a report,
/// not a gate (see the module header): it always exits 0, so a finding at error
/// severity would make `result` say the command failed while the exit code said
/// it succeeded. The per-environment truth is in `state`, which is the field a
/// consumer should key off.
fn findings(rows: &[EnvStatus]) -> Vec<output::Finding> {
    let mut out = Vec::new();
    for r in rows {
        if r.state != "ok" {
            out.push(status_finding(&r.environment, r.state, r.detail.as_deref()));
        }
        for issue in &r.issues {
            out.push(status_finding(
                &r.environment,
                issue.state,
                Some(&issue.detail),
            ));
        }
    }

    // A held lock is not a `state`: the environment's recorded state is whatever
    // the ledger says, and an apply running on top of it is a second,
    // independent fact — which is why the filter above cannot carry it. It still
    // has to reach `findings`, or a consumer keying off them alone is told
    // nothing about an environment where a new apply cannot start, while the
    // human view prints "locked by ..." in the same run.
    for r in rows {
        if let Some(lock) = &r.locked_by {
            out.push(output::Finding::warning(
                "state.locked",
                format!(
                    "{}: an apply is in progress (held by {lock}); a new one cannot start \
                     until it finishes or the lock is released",
                    r.environment
                ),
            ));
        }
        if let Some(why) = &r.lock_unknown {
            out.push(output::Finding::warning(
                "state.lock-unknown",
                format!(
                    "{}: whether an apply is in progress could not be determined ({why})",
                    r.environment
                ),
            ));
        }
    }
    out
}

fn status_finding(environment: &str, state: &'static str, detail: Option<&str>) -> output::Finding {
    let mut finding = output::Finding::warning(
        match state {
            "drift" => "state.drift",
            "policy" => "state.unmanaged-refused",
            "warning" => "state.unmanaged",
            "failed" => "state.failed",
            "staged" => "state.mid-deployment",
            "uninitialized" => "state.uninitialized",
            "unreachable" => "environment.unreachable",
            _ => "environment.unconfigured",
        },
        match detail {
            Some(detail) => format!("{environment}: {state} — {detail}"),
            None => format!("{environment}: {state}"),
        },
    );
    if state == "staged" {
        // Named, for the same reason as `doctor`'s: `apply` requires a target,
        // and `status` reports on several environments at once.
        finding = finding.remedy(format!(
            "pbps apply --env {} --plan {} --checksum {} --staged --resume",
            crate::report::env_arg(environment),
            crate::report::placeholder("plan.json"),
            crate::report::placeholder("approved-checksum"),
        ));
    }
    finding
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(environment: &str, state: &'static str) -> EnvStatus {
        EnvStatus {
            environment: environment.into(),
            description: None,
            state,
            detail: None,
            last_entry: Some(7),
            last_kind: Some("apply".into()),
            applied_at: Some("2026-08-31T09:14:22.517".into()),
            git_sha: Some("bd4be7412ab9c0".into()),
            operator: Some("ci-deploy".into()),
            locked_by: None,
            lock_unknown: None,
            checked_at: "2026-08-31T09:20:00Z".into(),
            issues: Vec::new(),
        }
    }

    /// Mid-deployment outranks drift: the operator needs `--resume`, and the
    /// drift workflow cannot finish a half-applied staged plan.
    #[test]
    fn a_staged_environment_stays_staged_when_it_also_drifts() {
        let mut r = row("prod", "staged");
        r.detail = Some("a staged apply stopped after 2 of 5 statement(s)".into());
        record_drift(&mut r, 7, "prod");
        assert_eq!(r.state, "staged");
        let detail = r.detail.as_deref().unwrap();
        assert!(detail.contains("2 of 5"), "{detail}");
        assert!(detail.contains("moved since that checkpoint"), "{detail}");
        let found = findings(&[r]);
        let ids: Vec<&str> = found.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["state.mid-deployment", "state.drift"]);
    }

    #[test]
    fn a_failed_staged_attempt_exposes_both_findings() {
        let mut r = row("prod", "failed");
        r.detail = Some("the last deployment attempt failed: denied".into());
        record_staged(&mut r, "prod", 2, 5, Some("abc123"));

        assert_eq!(r.state, "staged");
        let detail = r.detail.as_deref().unwrap();
        assert!(detail.contains("2 of 5"), "{detail}");
        assert!(detail.contains("attempt failed"), "{detail}");
        let found = findings(&[r]);
        let ids: Vec<&str> = found.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["state.mid-deployment", "state.failed"]);
    }

    #[test]
    fn an_ordinary_environment_is_marked_drifted() {
        let mut r = row("prod", "ok");
        record_drift(&mut r, 7, "prod");
        assert_eq!(r.state, "drift");
        assert!(r.detail.unwrap().contains("entry #7"));
    }

    #[test]
    fn a_failed_attempt_stays_visible_when_the_database_also_drifted() {
        let mut r = row("prod", "failed");
        r.detail = Some("the last deployment attempt failed: denied".into());
        record_drift(&mut r, 7, "prod");
        assert_eq!(r.state, "failed");
        let detail = r.detail.as_deref().unwrap();
        assert!(detail.contains("attempt failed"), "{detail}");
        assert!(detail.contains("no longer matches"), "{detail}");
        let found = findings(&[r]);
        let ids: Vec<&str> = found.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["state.failed", "state.drift"]);
    }

    #[test]
    fn a_failed_attempt_stays_visible_when_catalog_inspection_fails() {
        let mut r = row("prod", "failed");
        r.detail = Some("the last deployment attempt failed: denied".into());
        record_unreachable(&mut r, "permission denied reading sys.tables".into());

        assert_eq!(r.state, "unreachable");
        let detail = r.detail.as_deref().unwrap();
        assert!(detail.contains("permission denied reading sys.tables"));
        assert!(detail.contains("deployment attempt failed"));
        let findings = findings(&[r]);
        let ids: Vec<&str> = findings.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["environment.unreachable", "state.failed"]);
        assert!(findings[1].message.contains("deployment attempt failed"));
    }

    #[test]
    fn an_unmanaged_refusal_reaches_findings_alongside_drift() {
        let mut r = row("prod", "ok");
        record_drift(&mut r, 7, "prod");
        record_status_issue(
            &mut r,
            "policy",
            "`unmanaged: error` sees dbo.surprise outside the managed set".into(),
        );

        let findings = findings(&[r]);
        let ids: Vec<&str> = findings.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["state.drift", "state.unmanaged-refused"]);
        assert!(findings[1].message.contains("dbo.surprise"));
    }

    /// A ledger entry that recorded an empty schema, and a catalog that has one
    /// table the entry never named.
    fn stray_table() -> (
        pbps_db::LedgerEntry,
        pbps_mssql::introspect::Pulled,
        pbps_diff::Scoped,
    ) {
        let entry = pbps_db::LedgerEntry {
            id: 7,
            applied_at: "2026-08-31T09:14:22.517".into(),
            snapshot: pbps_model::StateSnapshot::new(
                pbps_model::StateKind::Apply,
                pbps_model::Schema::default(),
                pbps_model::IdsFile::default(),
                "ci-deploy",
            ),
        };
        let mut live = pbps_model::Schema::default();
        live.tables.insert(
            "dbo.surprise".parse().unwrap(),
            pbps_model::Table::default(),
        );
        let pulled = pbps_mssql::introspect::Pulled {
            schema: live,
            warnings: Vec::new(),
            unexpressible: Vec::new(),
            limitations: Vec::new(),
            unmanaged_modules: Vec::new(),
        };
        let scoped = pbps_diff::scope(
            &pulled.schema,
            &entry.snapshot.ids,
            &recorded_modules(&entry),
        );
        (entry, pulled, scoped)
    }

    /// The row read is the last thing `status` reads, and the inventory after
    /// it needs only the catalog, which had already succeeded. A read that
    /// failed is reported beside the stray object, never instead of it.
    #[test]
    fn a_failed_row_read_keeps_an_unmanaged_object_visible() {
        let (entry, pulled, scoped) = stray_table();
        let mut r = row("prod", "ok");
        record_catalog_findings(
            &mut r,
            &entry,
            &pulled,
            &scoped,
            Err("the declared rows could not be read back: denied".into()),
            pbps_config::Unmanaged::Warn,
        );

        assert_eq!(r.state, "unreachable");
        let findings = findings(&[r]);
        let ids: Vec<&str> = findings.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["environment.unreachable", "state.unmanaged"]);
        assert!(findings[0].message.contains("denied"), "{findings:?}");
        assert!(findings[1].message.contains("dbo.surprise"), "{findings:?}");
    }

    /// The same, where the object is one `unmanaged: error` refuses: the
    /// refusal is the finding `plan --db` and `apply` will act on, and a
    /// read that failed does not lift it.
    #[test]
    fn a_failed_row_read_keeps_an_unmanaged_refusal_visible() {
        let (entry, pulled, scoped) = stray_table();
        let mut r = row("prod", "ok");
        record_catalog_findings(
            &mut r,
            &entry,
            &pulled,
            &scoped,
            Err("the recorded rows cannot be compared: two spellings".into()),
            pbps_config::Unmanaged::Error,
        );

        let findings = findings(&[r]);
        let ids: Vec<&str> = findings.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["environment.unreachable", "state.unmanaged-refused"]);
    }

    /// Negative case: the stray object is a finding only when the project asks
    /// for it, and a read that succeeded on a database that matches its entry
    /// leaves the row clean.
    #[test]
    fn a_clean_environment_stays_ok_when_the_stray_object_is_ignored() {
        let (entry, pulled, scoped) = stray_table();
        let mut r = row("prod", "ok");
        record_catalog_findings(
            &mut r,
            &entry,
            &pulled,
            &scoped,
            Ok(pbps_model::ObservedRows::default()),
            pbps_config::Unmanaged::Ignore,
        );

        assert_eq!(r.state, "ok", "{r:?}");
        assert!(r.issues.is_empty(), "{r:?}");
        assert!(findings(&[r]).is_empty());
    }

    #[test]
    fn the_table_lines_up_and_shortens_the_sha() {
        let out = render(&[row("prod", "ok"), row("staging-eu", "drift")]);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("ENVIRONMENT"), "{out}");
        assert_eq!(lines[1].len(), lines[2].len(), "columns must align:\n{out}");
        assert!(out.contains("bd4be741"), "{out}");
        assert!(!out.contains("bd4be7412ab9c0"), "{out}");
    }

    /// The finding this round found missing. A locked environment whose ledger
    /// is otherwise healthy has `state: "ok"`, so the state filter cannot see
    /// it — and the human view prints "locked by ..." in the same run. The two
    /// views disagreeing about whether an apply can start is the worst place
    /// for them to disagree.
    #[test]
    fn a_held_lock_reaches_the_findings_even_when_the_state_is_ok() {
        let mut r = row("prod", "ok");
        r.locked_by = Some("ci-deploy since 2026-08-31T09:19:00".into());
        let f = findings(&[r]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].id, "state.locked");
        assert!(f[0].message.contains("ci-deploy"), "{f:?}");
    }

    /// A lock that could not be read is not an absent lock — the same
    /// distinction `doctor` and `explain` make. Reporting the environment as
    /// free when nobody looked is the one answer this row must never give.
    #[test]
    fn a_lock_that_could_not_be_read_is_not_reported_as_no_lock() {
        let mut r = row("prod", "ok");
        r.lock_unknown = Some("permission denied on dbo.__pbps_lock".into());
        let f = findings(&[r]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].id, "state.lock-unknown");
        assert!(f[0].message.contains("permission denied"), "{f:?}");
    }

    /// The negative case: a healthy, unlocked environment produces nothing.
    /// `status` exits 0 and a finding here would be the report crying wolf on
    /// the state it exists to confirm.
    #[test]
    fn a_healthy_unlocked_environment_produces_no_findings() {
        assert!(findings(&[row("prod", "ok")]).is_empty());
    }

    /// Both facts are independent, so both are reported: a locked environment
    /// that has also drifted is two different things an operator must act on.
    #[test]
    fn a_locked_environment_that_also_drifted_reports_both() {
        let mut r = row("prod", "drift");
        r.locked_by = Some("ci-deploy since 2026-08-31T09:19:00".into());
        let found = findings(&[r]);
        let ids: Vec<&str> = found.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["state.drift", "state.locked"]);
    }

    /// Every command this tool prints is copy-pastable or it is not printed.
    /// The `detail` strings advertise commands exactly as the remedies do, and
    /// were the one place that interpolated the environment name bare — so a
    /// name a shell would split, or one carrying a metacharacter, produced a
    /// line that does the wrong thing when pasted.
    #[test]
    fn a_command_inside_a_detail_quotes_the_environment_like_every_other() {
        // Only the drift path is reachable without a connection; the staged
        // detail is built inside `one()`. Asserting a hand-built string for it
        // would test the test, so that one is left to inspection — the two call
        // sites are a few lines apart and go through the same helper.
        let mut r = row("US West", "ok");
        record_drift(&mut r, 7, "US West");
        let detail = r.detail.unwrap();
        assert!(
            detail.contains("--env \"US West\""),
            "a name a shell would split must be quoted: {detail}"
        );

        // A name no spelling can carry becomes the placeholder rather than a
        // command that would run something when pasted.
        let mut r = row("prod&rm", "ok");
        record_drift(&mut r, 7, "prod&rm");
        let detail = r.detail.unwrap();
        assert!(
            detail.contains("--env \"<environment>\""),
            "an unquotable name must not be interpolated: {detail}"
        );
    }

    /// A short sha must not panic the slice that shortens a long one.
    #[test]
    fn a_short_sha_survives() {
        let mut r = row("prod", "ok");
        r.git_sha = Some("abc".into());
        assert!(render(&[r]).contains("abc"));
    }

    #[test]
    fn a_missing_value_shows_a_dash_not_a_blank() {
        let mut r = row("prod", "unreachable");
        r.applied_at = None;
        r.git_sha = None;
        r.operator = None;
        let out = render(&[r]);
        assert!(out.contains(" -  "), "{out}");
    }

    /// A lock is either an apply in flight or one that died; either way it is
    /// the first thing an operator needs before they start typing.
    #[test]
    fn a_lock_is_called_out_under_the_table() {
        let mut r = row("prod", "ok");
        r.locked_by = Some("ci-deploy since 2026-08-31T09:19:00".into());
        let out = render(&[r]);
        assert!(out.contains("locked by ci-deploy"), "{out}");
    }

    /// The JSON shape is what anyone rendering their own view depends on.
    #[test]
    fn json_omits_absent_fields_rather_than_nulling_them() {
        let mut r = row("prod", "ok");
        r.git_sha = None;
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("git_sha"), "{json}");
        assert!(json.contains(r#""state":"ok""#), "{json}");
        assert!(json.contains(r#""environment":"prod""#), "{json}");
    }
}
