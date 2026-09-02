//! `pbps explain` — the reviewer's view of a saved plan (SPEC §14.1).
//!
//! # Who this is for
//!
//! Not the author of the change. The author has the declarations, the diff and
//! the MR. This is for whoever stands at the deployment gate holding
//! `plan.json` and deciding whether to type `--allow destructive`: a DBA, a
//! release manager, an auditor. They may have no checkout, no credentials and no
//! intention of reading T-SQL, and today the first human-facing artifact they
//! meet is effectively `plan.sql`.
//!
//! # Why it needs no connection
//!
//! Everything that decides the answer is already in the file: the changes, their
//! risk classes, the mode, the baseline it was computed against, and the
//! checksum that pins it to the apply. A command that needed credentials would
//! be a command the reviewer cannot run, which puts them back to being briefed
//! second-hand by the person asking for the approval.
//!
//! A target *may* be given, and then one more question can be answered — whether
//! that environment is mid-deployment, which no file can know. It stays optional
//! for the reason above.

use anyhow::Context as _;
use pbps_dialect::Dialect;
use pbps_model::{PlanMode, PlanOrigin, RiskClass, SavedPlan};

use crate::{db, output, report};

/// What stands in for an environment the reviewer has to supply.
///
/// A generated placeholder, never a value, so it is exempt from [`shell_arg`] —
/// quoting it would make it read as a literal directory called `<environment>`.
const PLACEHOLDER: &str = "<environment>";

/// What `explain` found, for `--format json`.
///
/// Serialized rather than re-derived by a consumer: the optional UI of ADR-0006
/// renders exactly this, and a UI that recomputed "which risks apply" from the
/// change list would be a second implementation of the gate's own arithmetic.
#[derive(serde::Serialize)]
pub struct Explanation {
    pub applyable: bool,
    pub dialect: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,
    pub baseline: String,
    /// `transactional` or `staged`.
    pub mode: &'static str,
    /// The checksum `apply` will re-compute and refuse a mismatch on.
    pub checksum: String,
    pub change_count: usize,
    pub table_count: usize,
    /// Counted apart from tables: `Change::table()` returns a module's own name
    /// for a module change, so folding them together called a one-view plan
    /// "1 table" (ADR-0002 — they are different kinds of object).
    pub module_count: usize,
    pub risks: Vec<RiskDetail>,
    /// The exact command that approves this plan, `--allow` included — or, for
    /// a preview, the command that produces an applyable plan instead. Which
    /// one it is is [`Explanation::applyable`].
    pub approve_with: String,
    pub probes: Vec<String>,
    /// Statements the plan will run. Counted, not listed: the SQL is
    /// `plan --sql`'s job, and duplicating it here would invite a reviewer to
    /// read one and approve the other.
    pub statement_count: usize,
    /// Only present when a target was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetState>,
}

#[derive(serde::Serialize)]
pub struct RiskDetail {
    pub class: &'static str,
    pub why: &'static str,
    pub changes: Vec<String>,
}

#[derive(serde::Serialize)]
pub struct TargetState {
    pub environment: String,
    /// `ready`, `locked`, `mid-deployment`, `uninitialized` or `unreachable`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

pub fn cmd_explain(
    path: &std::path::Path,
    target: Option<&db::Target>,
    env: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    // Read and checked before anything else, and reported through the envelope
    // when JSON was asked for: everything below can fail, and a failure that
    // escaped early left stdout empty — so a consumer got the converter's
    // generic "produced no output" instead of a report naming the bad plan.
    let plan = match read_plan(path) {
        Ok(p) => p,
        Err(e) => {
            if json {
                // Unanswerable, not a finding: `explain` was asked what this
                // plan does and could not read it, so it did not answer.
                let report = output::Report::plain(
                    "explain",
                    vec![
                        output::Finding::error("plan.unreadable", format!("{e:#}")).at(path, None),
                    ],
                )
                .unanswerable();
                println!("{}", serde_json::to_string_pretty(&report)?);
            }
            return Err(e);
        }
    };
    // The dialect comes from the *plan*, not from a pbps.yml. That is what lets
    // this command run in a directory that has no project at all — the reviewer
    // may have been handed nothing but the file. It is also more honest where a
    // project does exist: a plan computed for one engine must be explained as
    // that engine, not as whatever the local config happens to select.
    let dialect = dialect_of(&plan)?;

    let explanation = explain(&plan, dialect.as_ref(), path, target, env)?;
    let findings = findings(&plan, &explanation);

    if json {
        let report = output::Report::new("explain", findings, Some(&explanation));
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        // The findings are not echoed here. Every one of them is already a
        // section of the rendered explanation — the origin line, the execution
        // line, the risk table, the environment — and repeating them underneath
        // would train the reviewer to skip the bottom of the output, which is
        // where the approval command is.
        print!("{}", render(&plan, &explanation));
    }
    // Explaining a plan is not a gate. A plan full of destructive changes is
    // exactly what this command exists to describe well, and exiting non-zero on
    // one would make the reviewer's own tool look like a failure in their
    // terminal — the gate is `apply --allow`, which is where it belongs.
    Ok(())
}

/// Reads a plan file, refusing one this binary does not understand.
///
/// The version check is `apply`'s (see `deploy::cmd_apply`), made earlier. A
/// newer plan may carry semantics this build has no idea about, and explaining
/// it would quietly omit them — showing a reviewer an incomplete account of what
/// they are approving, and an approval command for an artifact `apply` will
/// refuse anyway. Better to say which version it is.
fn read_plan(path: &std::path::Path) -> anyhow::Result<SavedPlan> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the plan `{}`", path.display()))?;
    let plan: SavedPlan = serde_json::from_str(&raw)
        .with_context(|| format!("`{}` is not a pbps plan", path.display()))?;
    if plan.version != pbps_model::plan::CURRENT_VERSION {
        anyhow::bail!(
            "`{}` is a version {} plan and this tool understands version {}",
            path.display(),
            plan.version,
            pbps_model::plan::CURRENT_VERSION
        );
    }
    Ok(plan)
}

/// The dialect this plan was computed for.
///
/// A plan names its own engine (`SavedPlan::dialect`) precisely so a plan
/// computed for one and applied to another is nonsense the file can catch;
/// reading it here is the same check, made earlier and without a connection.
fn dialect_of(plan: &SavedPlan) -> anyhow::Result<Box<dyn Dialect>> {
    match plan.dialect.as_str() {
        "mssql" => Ok(Box::new(pbps_mssql::Mssql)),
        other => {
            anyhow::bail!("this plan was computed for `{other}`, which this build cannot explain")
        }
    }
}

fn explain(
    plan: &SavedPlan,
    dialect: &dyn Dialect,
    path: &std::path::Path,
    target: Option<&db::Target>,
    env: Option<&str>,
) -> anyhow::Result<Explanation> {
    let cs = &plan.changes;
    let (table_count, module_count) = report::touched(cs);

    let present = cs.risks();
    let risks: Vec<RiskDetail> = RiskClass::ALL
        .into_iter()
        .filter(|c| present.contains(c))
        .map(|class| RiskDetail {
            class: class.as_str(),
            why: class.why(),
            changes: cs
                .changes
                .iter()
                .filter(|p| p.risks.contains(&class))
                .map(|p| format!("{}  {}", p.change.table(), report::describe(&p.change)))
                .collect(),
        })
        .collect();

    // An offline plan has no approval command, because there is nothing to
    // approve: `apply` refuses a `Preview` structurally, whatever target and
    // whatever `--allow` it is given (§7.3). Printing one anyway would have the
    // report contradict its own first line — which says the plan is a preview —
    // and hand the reviewer something that cannot work.
    let approve = if plan.origin.is_applyable() {
        // `apply` requires exactly one of --db / --env, so a command printed
        // without one fails the moment it is pasted. The environment's *name*
        // is used when there is one; a --db target contributes only its
        // redacted label, which is not a connection string and must never be
        // printed as if it were, so that case gets the placeholder too.
        let mut approve = format!(
            "pbps apply --env {} --plan {}",
            // The placeholder is generated, not user input, and must stay
            // visibly a placeholder rather than become a quoted string.
            match env {
                Some(name) => shell_arg(name),
                None => PLACEHOLDER.to_owned(),
            },
            shell_arg(&path.display().to_string())
        );
        if !present.is_empty() {
            approve.push_str(&format!(
                " --allow {}",
                present
                    .iter()
                    .map(|r| r.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        if plan.mode == PlanMode::Staged {
            approve.push_str(" --staged");
        }
        approve
    } else {
        format!(
            "pbps plan --env {PLACEHOLDER} --out {}",
            shell_arg(&path.display().to_string())
        )
    };

    Ok(Explanation {
        applyable: plan.origin.is_applyable(),
        dialect: plan.dialect.clone(),
        created_at: plan.created_at.clone(),
        git_sha: plan.git_sha.clone(),
        baseline: plan.baseline.description.clone(),
        mode: match plan.mode {
            PlanMode::Transactional => "transactional",
            PlanMode::Staged => "staged",
        },
        checksum: plan.checksum(),
        change_count: cs.changes.len(),
        table_count,
        module_count,
        risks,
        approve_with: approve,
        probes: dialect
            .preflight(cs)
            .into_iter()
            .map(|p| p.description)
            .collect(),
        statement_count: crate::statements(cs, dialect)?.len(),
        target: match target {
            Some(t) => Some(target_state(t)?),
            None => None,
        },
    })
}

/// The one question the file cannot answer.
fn target_state(target: &db::Target) -> anyhow::Result<TargetState> {
    // The lock is read first, and a held one wins. During an ordinary
    // transactional apply the newest ledger entry is a completed snapshot, so
    // `latest` alone reports `ready` — handing the reviewer an approval command
    // while the environment is being changed underneath them, which is the one
    // thing this check exists to prevent.
    let checked = db::runtime()?.block_on(async {
        let mut conn = pbps_db::Conn::connect(target.connection()).await?;
        if let Ok(Some(lock)) = pbps_mssql::state::lock_holder(&mut conn).await {
            return Ok(Err(lock));
        }
        pbps_mssql::state::latest(&mut conn).await.map(Ok)
    });
    let checked = match checked {
        Ok(Err(lock)) => {
            return Ok(TargetState {
                environment: target.label.clone(),
                state: "locked",
                detail: Some(format!(
                    "an apply is running (held by {} since {}), so this environment is \
                     changing right now",
                    lock.locked_by, lock.locked_at
                )),
            });
        }
        Ok(Ok(entry)) => Ok(entry),
        Err(e) => Err(e),
    };
    Ok(match checked {
        Ok(Some(entry)) if entry.snapshot.staged.is_some() => {
            let progress = entry.snapshot.staged.as_ref().expect("just matched");
            TargetState {
                environment: target.label.clone(),
                state: "mid-deployment",
                detail: Some(format!(
                    "a staged apply stopped after {} of {} statement(s); finish it with \
                     `--staged --resume` before anything else is applied",
                    progress.completed, progress.total
                )),
            }
        }
        Ok(Some(_)) => TargetState {
            environment: target.label.clone(),
            state: "ready",
            detail: None,
        },
        Ok(None) => TargetState {
            environment: target.label.clone(),
            state: "uninitialized",
            detail: Some("this environment has a ledger but no entries".to_owned()),
        },
        // A database pbps has never touched is reachable, and saying otherwise
        // would send the reviewer to look at the network while the server sits
        // there answering. `latest` distinguishes "no ledger at all" from "a
        // ledger with no entries" precisely so callers can, and this one has to
        // — `uninitialized` is one of the states this report advertises.
        Err(pbps_db::LedgerError::NotInitialized) => TargetState {
            environment: target.label.clone(),
            state: "uninitialized",
            detail: Some("pbps has recorded no state in this database yet".to_owned()),
        },
        // Unreachable is reported, not fatal: the file half of the explanation
        // is the whole point of the command and must survive a reviewer who has
        // read access to nothing.
        Err(e) => TargetState {
            environment: target.label.clone(),
            state: "unreachable",
            detail: Some(format!("{e}")),
        },
    })
}

fn findings(plan: &SavedPlan, e: &Explanation) -> Vec<output::Finding> {
    let mut out = Vec::new();
    if !e.applyable {
        out.push(output::Finding::warning(
            "plan.preview",
            "this plan was computed offline; `apply` will refuse it. Only `pbps plan --db` \
             produces an applyable plan",
        ));
    }
    if plan.mode == PlanMode::Staged {
        out.push(output::Finding::warning(
            "plan.staged",
            "this plan runs outside a transaction, one statement at a time. A failure part-way \
             leaves the earlier statements applied and the environment mid-deployment",
        ));
    }
    for r in &e.risks {
        out.push(output::Finding::warning(
            "plan.risk",
            format!("{}: {} ({} change(s))", r.class, r.why, r.changes.len()),
        ));
    }
    if let Some(t) = &e.target
        && t.state != "ready"
    {
        out.push(output::Finding::warning(
            "target.not-ready",
            match &t.detail {
                Some(d) => format!("{}: {} — {d}", t.environment, t.state),
                None => format!("{}: {}", t.environment, t.state),
            },
        ));
    }
    out
}

fn render(plan: &SavedPlan, e: &Explanation) -> String {
    let mut out = String::from("Plan\n");
    out.push_str(&format!(
        "  origin      {}\n",
        match plan.origin {
            PlanOrigin::Database => "computed against the target environment — applyable",
            PlanOrigin::Preview => "computed offline — a preview; `apply` will refuse it",
        }
    ));
    out.push_str(&format!("  dialect     {}\n", e.dialect));
    out.push_str(&format!("  created     {}\n", e.created_at));
    if let Some(sha) = &e.git_sha {
        out.push_str(&format!("  git sha     {sha}\n"));
    }
    out.push_str(&format!("  baseline    {}\n", e.baseline));
    out.push_str(&format!(
        "  execution   {}\n",
        match plan.mode {
            PlanMode::Transactional =>
                "one transaction, all or nothing: a failure rolls the whole plan back",
            PlanMode::Staged =>
                "staged: outside a transaction, one statement at a time, each recorded",
        }
    ));
    // Printed because it is the thing that makes the approval mean something: it
    // is what `apply` recomputes, and a plan edited after this review no longer
    // matches it.
    out.push_str(&format!("  checksum    {}\n", e.checksum));

    out.push_str(&format!(
        "\nWhat it changes\n  {} change(s) across {}, {} statement(s).\n",
        e.change_count,
        report::objects(e.table_count, e.module_count),
        e.statement_count
    ));
    out.push_str(&report::changes(&plan.changes));

    if e.risks.is_empty() {
        out.push_str(
            "\nWhy it needs approval\n  No risk class applies; this plan needs no --allow.\n",
        );
    } else {
        out.push_str("\nWhy it needs approval\n");
        for r in &e.risks {
            out.push_str(&format!("  {:<12} {}\n", r.class, r.why));
            for c in &r.changes {
                out.push_str(&format!("               {c}\n"));
            }
        }
    }

    out.push_str("\nChecks that run before the first statement\n");
    if e.probes.is_empty() {
        // Not "everything is fine": a plan with nothing to probe is the ordinary
        // case, and phrasing it as reassurance would make the reviewer read a
        // silence as a clearance.
        out.push_str("  None: no change in this plan has a hazard that today's data can answer.\n");
    } else {
        for p in &e.probes {
            out.push_str(&format!("  - {p}\n"));
        }
        out.push_str(
            "  Each counts the rows that would break. A non-zero count stops the apply before\n  \
             the first statement runs.\n",
        );
    }

    out.push_str("\nThe environment\n");
    match &e.target {
        None => out.push_str(
            "  Not checked: no --db or --env was given. Whether the target is mid-deployment is\n  \
             the one question this file cannot answer; `pbps status` answers it.\n",
        ),
        Some(t) => {
            out.push_str(&format!("  {:<12} {}\n", t.environment, t.state));
            if let Some(d) = &t.detail {
                out.push_str(&format!("               {d}\n"));
            }
        }
    }

    out.push_str(&format!(
        "\n{}\n  {}\n",
        if e.applyable {
            "To approve and run it:"
        } else {
            "This plan cannot be applied. To produce one that can:"
        },
        e.approve_with
    ));
    out
}

/// One argument, quoted so that pasting it passes the value through unchanged.
///
/// # Why there is no single "correct escaping" to reach for
///
/// This line is read in POSIX shells, PowerShell and `cmd`, and their quoting
/// rules disagree: POSIX single quotes are literal, PowerShell treats `$` and a
/// backtick as live inside double quotes, and `cmd` knows neither single quotes
/// nor `$`. No one string is exactly right in all three, so the rule below picks
/// the form that is right where the characters in question are *dangerous*, and
/// merely wrong — inert, a path that will not be found — where they are not.
///
/// Three cases:
///
/// 1. Nothing a shell reinterprets: emitted bare, so the ordinary path does not
///    look like it needs care.
/// 2. Something a shell would split, but nothing it would expand (a space, most
///    often): double quotes, which all three understand.
/// 3. Anything a shell would *expand or redirect* — `$`, a backtick, `"`, `<`,
///    `>`, `|`, `;`, `&`, a trailing backslash: POSIX single quotes, with `'`
///    escaped as `'\''`. That is exactly right in the shells where those
///    characters mean something, and in `cmd`, where single quotes are ordinary
///    characters, it fails to find the file rather than running anything.
///
/// Failing safe is the whole design here. The value is the user's own `--plan`
/// argument rather than anything hostile, so this is a correctness problem, not
/// an injection one — but a printed command that silently redirects into a file
/// is a bad way to learn that.
fn shell_arg(value: &str) -> String {
    // `~` is safe away from the front: it means home-directory expansion as the
    // first character of a word and nothing at all elsewhere, and every Windows
    // short path is full of it (`C:\Users\RUNNER~1\...`).
    //
    // `<` and `>` are deliberately *not* here. They were, so that the
    // `<environment>` placeholder would stay bare, and the cost was that a path
    // containing one became a redirection. The placeholder is generated rather
    // than user input, so it is exempted where it is built instead.
    let bare = |c: char| c.is_ascii_alphanumeric() || "-_./:\\@+=~".contains(c);
    if !value.is_empty() && !value.starts_with('~') && value.chars().all(bare) {
        return value.to_owned();
    }

    // Double quotes leave these live in POSIX shells or PowerShell, and a
    // trailing backslash would escape the closing quote itself — which a
    // Windows directory path ends with more often than not.
    let expands =
        value.contains(['$', '`', '"', '<', '>', '|', ';', '&', '\n']) || value.ends_with('\\');
    if !expands {
        return format!("\"{value}\"");
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line is advertised as copy-pastable, so a path a shell would take
    /// apart has to survive the paste — and one it would not must stay bare, or
    /// the ordinary case looks like it needs care.
    #[test]
    fn only_a_value_a_shell_would_reinterpret_is_quoted() {
        assert_eq!(shell_arg("plan.json"), "plan.json");
        assert_eq!(shell_arg("/tmp/plans/plan.json"), "/tmp/plans/plan.json");
        assert_eq!(
            shell_arg("/tmp/release plans/plan.json"),
            "\"/tmp/release plans/plan.json\""
        );
    }

    /// Windows short paths are full of `~`, which means home-directory
    /// expansion only at the *start* of a word. Treating it as unsafe outright
    /// would quote the ordinary case on a whole platform — which is exactly how
    /// this was found, by the Windows job.
    #[test]
    fn a_tilde_is_special_only_at_the_front() {
        assert_eq!(
            shell_arg("C:\\Users\\RUNNER~1\\AppData\\Local\\Temp\\plan.json"),
            "C:\\Users\\RUNNER~1\\AppData\\Local\\Temp\\plan.json"
        );
        assert_eq!(shell_arg("~/plans/plan.json"), "\"~/plans/plan.json\"");
    }

    /// The cases that matter: a printed command must not redirect, expand or
    /// substitute anything when pasted. Double quotes do not stop `$`, a
    /// backtick or a redirection, so those take the single-quoted form, which
    /// is literal in every shell where they mean anything.
    #[test]
    fn nothing_a_shell_would_expand_or_redirect_survives_unquoted() {
        for value in [
            "/tmp/a>b/plan.json",
            "/tmp/a<b/plan.json",
            "/tmp/a|b/plan.json",
            "/tmp/a;b/plan.json",
            "/tmp/a&b/plan.json",
            "/tmp/$HOME/plan.json",
            "/tmp/$(id)/plan.json",
            "/tmp/`id`/plan.json",
        ] {
            let quoted = shell_arg(value);
            assert!(
                quoted.starts_with('\'') && quoted.ends_with('\''),
                "{value} must be single-quoted, got {quoted}"
            );
        }
    }

    /// A trailing backslash would escape the closing double quote itself, and a
    /// Windows directory path ends with one more often than not.
    #[test]
    fn a_trailing_backslash_does_not_escape_the_closing_quote() {
        let quoted = shell_arg("C:\\release plans\\");
        assert!(quoted.starts_with('\''), "{quoted}");
        assert!(!quoted.ends_with("\\\""), "{quoted}");
    }

    /// The single-quoted form has to close and reopen around a quote of its
    /// own, or it ends the string early — the one way this transformation can
    /// produce something actively wrong rather than merely unfound.
    #[test]
    fn a_single_quote_is_escaped_rather_than_ending_the_string() {
        assert_eq!(shell_arg("/tmp/o'brien/$x"), "'/tmp/o'\\''brien/$x'");
    }

    /// The negative case: an empty value must not vanish into the command line
    /// as though the argument had not been given.
    #[test]
    fn an_empty_value_is_not_silently_dropped() {
        assert_eq!(shell_arg(""), "\"\"");
    }
}
