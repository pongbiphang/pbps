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

    /// `ok`, `staged`, `drift`, `uninitialized`, `unreachable` or `unconfigured`.
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
        let mut row = rt.block_on(one(&connection, name, &checked_at));
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

async fn one(connection: &str, name: &str, checked_at: &str) -> EnvStatus {
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
        // Not here, though: `NotInitialized` means `dbo.__pbps_state` is
        // absent, and nothing can have taken a lock without `ensure_tables`
        // creating that table first. Asking anyway would query a lock table
        // that does not exist and report `lock-unknown` on every environment
        // pbps has never deployed to — a warning about the ordinary case.
        Err(pbps_db::LedgerError::NotInitialized) => {
            return EnvStatus::failed(
                name,
                "uninitialized",
                "pbps has never recorded a state here".to_owned(),
                checked_at,
            );
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
    };

    // An environment sitting on a staged checkpoint is mid-deployment, and that
    // is the first thing an operator needs to know about it: `plan --db` and
    // `apply` both refuse until it is finished, so a screen that read "ok"
    // would leave them puzzled at the refusal.
    if let Some(progress) = &entry.snapshot.staged {
        row.state = "staged";
        // Through `env_arg`, like every other command this tool prints. An
        // environment name is a YAML map key, so `US West` is a valid one, and
        // interpolated bare it becomes two arguments — or, with a shell
        // metacharacter, something that runs. These `detail` strings advertise
        // commands exactly as the remedies do, and were the one place that
        // bypassed the helper.
        row.detail = Some(format!(
            "a staged apply stopped after {} of {} statement(s); continue it with \
             `pbps apply --staged --resume --env {} --plan ...`",
            progress.completed,
            progress.total,
            crate::report::env_arg(name)
        ));
    }

    // The drift verdict is the checksum, computed exactly as `verify` computes
    // it. Two commands that disagreed about whether an environment has drifted
    // would be worse than one of them not existing.
    let pulled = match pbps_mssql::catalog::introspect(&mut conn).await {
        Ok(p) => p,
        Err(e) => {
            row.state = "unreachable";
            row.detail = Some(e.to_string());
            return row;
        }
    };
    // Scoped by what was recorded, exactly as `verify` scopes it: two commands
    // that disagreed about which objects are managed would disagree about
    // whether an environment has drifted.
    let recorded_modules: std::collections::BTreeSet<_> =
        entry.snapshot.schema.modules.keys().cloned().collect();
    let scoped = pbps_diff::scope(&pulled.schema, &recorded_ids, &recorded_modules);
    let live = pbps_model::state_checksum(&scoped.schema, &recorded_ids);
    let recorded = pbps_model::state_checksum(&entry.snapshot.schema, &recorded_ids);
    if live != recorded {
        record_drift(&mut row, entry.id, name);
    }
    row
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
        let so_far = row.detail.take().unwrap_or_default();
        let arg = crate::report::env_arg(name);
        row.detail = Some(format!(
            "{so_far} — and it has moved since that checkpoint, which \
             `pbps verify --env {arg}` will show"
        ));
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
    let mut out: Vec<output::Finding> = rows
        .iter()
        .filter(|r| r.state != "ok")
        .map(|r| {
            let mut f = output::Finding::warning(
                match r.state {
                    "drift" => "state.drift",
                    "staged" => "state.mid-deployment",
                    "uninitialized" => "state.uninitialized",
                    "unreachable" => "environment.unreachable",
                    _ => "environment.unconfigured",
                },
                match &r.detail {
                    Some(d) => format!("{}: {} — {d}", r.environment, r.state),
                    None => format!("{}: {}", r.environment, r.state),
                },
            );
            if r.state == "staged" {
                // Named, for the same reason as `doctor`'s: `apply` requires a
                // target, and `status` is the command that reports on six
                // environments at once — a remedy without the name leaves the
                // reader to work out which of the six it meant.
                f = f.remedy(format!(
                    "pbps apply --env {} --plan <plan.json> --staged --resume",
                    crate::report::env_arg(&r.environment)
                ));
            }
            f
        })
        .collect();

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
        let detail = r.detail.unwrap();
        assert!(detail.contains("2 of 5"), "{detail}");
        assert!(detail.contains("moved since that checkpoint"), "{detail}");
    }

    #[test]
    fn an_ordinary_environment_is_marked_drifted() {
        let mut r = row("prod", "ok");
        record_drift(&mut r, 7, "prod");
        assert_eq!(r.state, "drift");
        assert!(r.detail.unwrap().contains("entry #7"));
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
        let ids: Vec<&str> = findings(&[r]).iter().map(|f| f.id).collect();
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
            detail.contains("--env <environment>"),
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
