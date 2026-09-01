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
    pub risks: Vec<RiskDetail>,
    /// The exact command that approves this plan, `--allow` included.
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
    /// `ready`, `mid-deployment` or `uninitialized`.
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
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the plan `{}`", path.display()))?;
    let plan: SavedPlan = serde_json::from_str(&raw)
        .with_context(|| format!("`{}` is not a pbps plan", path.display()))?;
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
    let tables: std::collections::BTreeSet<String> = cs
        .changes
        .iter()
        .map(|p| p.change.table().to_string())
        .collect();

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

    // `apply` requires exactly one of --db / --env, so a command printed
    // without one fails the moment it is pasted. The environment's *name* is
    // used when there is one; a --db target contributes only its redacted
    // label, which is not a connection string and must never be printed as if
    // it were, so that case gets the placeholder too.
    let mut approve = format!(
        "pbps apply --env {} --plan {}",
        env.unwrap_or("<environment>"),
        path.display()
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
        table_count: tables.len(),
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
    let checked = db::runtime()?.block_on(async {
        let mut conn = pbps_db::Conn::connect(target.connection()).await?;
        pbps_mssql::state::latest(&mut conn).await
    });
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
        "\nWhat it changes\n  {} change(s) across {} table(s), {} statement(s).\n",
        e.change_count, e.table_count, e.statement_count
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

    out.push_str(&format!("\nTo approve and run it:\n  {}\n", e.approve_with));
    out
}
