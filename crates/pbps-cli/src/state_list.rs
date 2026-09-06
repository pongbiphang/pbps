//! `pbps state list` — the ledger as a timeline (SPEC §14.1, first half of the
//! `state show / diff / export` row).
//!
//! # Why a command of its own rather than a flag on `status`
//!
//! `status` answers "where is every environment now", and carries the newest
//! entry alone. A history is a different question with a different shape — one
//! environment, many rows — and folding it into `status` would either widen
//! every row with a list nobody asked for or make `--history` mean "print
//! something else entirely".
//!
//! # Why absent, empty and unreadable are three answers here
//!
//! An empty timeline is the one rendering that must never stand for "the
//! database could not be reached" or "this environment has no ledger yet".
//! Each of those is a different thing for an operator to do next, so the
//! envelope carries `initialized` beside the entries and a finding that names
//! which case this is, and an unreachable environment is `unanswerable` with
//! no `data` at all (SPEC §9.8).

use pbps_config::Project;

use crate::{db, output};

/// One ledger row, without opening its recorded schema.
///
/// The snapshot's `schema` and `ids` are deliberately not here: they are the
/// whole database, they are what `state show` and `state export` will be for,
/// and a timeline that carried them would send megabytes to a page that draws
/// a list of dates.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct LedgerRow {
    pub id: i64,

    /// The server's clock, not the client's (`pbps_db::LedgerEntry`).
    pub applied_at: String,

    /// `apply`, `baseline`, `bootstrap`, `staged` or `failed`.
    pub kind: &'static str,

    /// The version of the recorded state's own format. A consumer that renders
    /// a field a later version added has to know which entries can hold it.
    pub state_version: u32,

    pub operator: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_checksum: Option<String>,

    /// Why a baseline skipped the differences, or how an attempt failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,

    /// Present only on a `staged` checkpoint: how far through the plan this
    /// environment is. Its absence is what "not mid-deployment" means, so it is
    /// carried rather than flattened into `kind`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staged: Option<StagedRow>,

    pub tables: usize,
    pub modules: usize,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct StagedRow {
    pub completed: usize,
    pub total: usize,
}

/// What `state list` produced, for `--format json`.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct StateListData {
    /// The environment's name, or a redacted description of the connection —
    /// never a connection string (SPEC §8.1).
    pub environment: String,

    /// Whether the ledger tables exist at all. `false` with no entries is a
    /// database this tool has never written to; `true` with none is one whose
    /// first apply has not finished. The two are different problems.
    pub initialized: bool,

    /// How many rows were asked for, so a reader knows whether the list is the
    /// whole history or the head of it.
    pub limit: u32,

    /// Newest first, like the ledger's own order.
    pub entries: Vec<LedgerRow>,
}

fn row(entry: pbps_db::LedgerEntry) -> LedgerRow {
    let snapshot = entry.snapshot;
    LedgerRow {
        id: entry.id,
        applied_at: entry.applied_at,
        kind: snapshot.kind.as_str(),
        state_version: snapshot.version,
        operator: snapshot.operator,
        git_sha: snapshot.git_sha,
        plan_checksum: snapshot.plan_checksum,
        reason: snapshot.reason,
        staged: snapshot.staged.map(|s| StagedRow {
            completed: s.completed,
            total: s.total,
        }),
        tables: snapshot.schema.tables.len(),
        modules: snapshot.schema.modules.len(),
    }
}

/// `pbps state list` — the environment's history, newest first.
pub fn cmd_state_list(
    project: &Project,
    target: &db::Target,
    limit: u32,
    json: bool,
) -> anyhow::Result<()> {
    output::or_unanswerable(
        "state list",
        json,
        "project.dialect-unsupported",
        db::require_mssql(project, "state list"),
    )?;
    let runtime = output::or_unanswerable("state list", json, "db.runtime", db::runtime())?;

    runtime.block_on(async {
        let mut conn = output::or_unanswerable(
            "state list",
            json,
            "db.unreachable",
            db::connect(target).await,
        )?;

        let (initialized, entries) = match pbps_mssql::state::history(&mut conn, limit).await {
            Ok(entries) => (true, entries),
            // Not an error: a database this tool has never written to has no
            // history, and saying so is the answer. It is *not* an empty
            // history — the finding below is what keeps the two apart.
            Err(pbps_db::LedgerError::NotInitialized) => (false, Vec::new()),
            Err(e) => {
                let findings = vec![output::Finding::error(
                    "state.unreadable",
                    format!("{}: the ledger could not be read: {e}", target.label),
                )];
                if json {
                    output::unanswerable("state list", findings);
                } else {
                    eprint!("{}", output::human(&findings));
                }
                return Err(crate::Found::reported().into());
            }
        };

        let mut findings = Vec::new();
        if !initialized {
            findings.push(
                output::Finding::note(
                    "state.uninitialized",
                    format!(
                        "{}: this database has no pbps ledger yet, so there is no history",
                        target.label
                    ),
                )
                .remedy("pbps bootstrap, or pbps baseline to adopt the database as it stands"),
            );
        } else if entries.is_empty() {
            findings.push(output::Finding::note(
                "state.no-entries",
                format!(
                    "{}: the ledger exists but has no entries; a first apply has not finished",
                    target.label
                ),
            ));
        }

        let data = StateListData {
            environment: target.label.clone(),
            initialized,
            limit,
            entries: entries.into_iter().map(row).collect(),
        };

        if json {
            return output::Report::new("state list", findings, Some(&data)).emit_json();
        }
        if !findings.is_empty() {
            print!("{}", output::human(&findings));
        }
        print!("{}", render(&data));
        Ok(())
    })
}

/// The table an operator reads.
///
/// Widths are computed rather than fixed: an operator's name and a reason are
/// both free text, and a column that truncated one would hide the field the row
/// exists for.
fn render(data: &StateListData) -> String {
    let mut out = String::new();
    if data.entries.is_empty() {
        return out;
    }
    let head = ["ID", "APPLIED", "KIND", "OPERATOR", "GIT", "DETAIL"];
    let rows: Vec<[String; 6]> = data
        .entries
        .iter()
        .map(|e| {
            [
                e.id.to_string(),
                e.applied_at.clone(),
                match &e.staged {
                    Some(s) => format!("{} {}/{}", e.kind, s.completed, s.total),
                    None => e.kind.to_owned(),
                },
                e.operator.clone(),
                e.git_sha
                    .as_deref()
                    // Seven characters is what `git log --oneline` shows and
                    // what a reviewer pastes back; the full sha is in the JSON.
                    .map(|s| s.chars().take(7).collect())
                    .unwrap_or_else(|| "-".to_owned()),
                e.reason.clone().unwrap_or_default(),
            ]
        })
        .collect();

    let mut width = head.map(str::len);
    for r in &rows {
        for (w, cell) in width.iter_mut().zip(r) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: &[String; 6], out: &mut String| {
        for (i, (cell, w)) in cells.iter().zip(width).enumerate() {
            if i + 1 == cells.len() {
                out.push_str(cell.trim_end());
            } else {
                out.push_str(cell);
                for _ in cell.chars().count()..w + 2 {
                    out.push(' ');
                }
            }
        }
        while out.ends_with(' ') {
            out.pop();
        }
        out.push('\n');
    };
    line(&head.map(str::to_owned), &mut out);
    for r in &rows {
        line(r, &mut out);
    }
    out
}
