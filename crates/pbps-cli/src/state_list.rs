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
///
/// The fields that come from the recorded state are optional, and the ones
/// beside them are not, because a row whose state this build cannot read is
/// still a row: an environment upgraded across a state-format change keeps
/// entries older than `OLDEST_READABLE_VERSION`, and `unreadable` is what says
/// so on the row rather than in place of the whole history.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct LedgerRow {
    pub id: i64,

    /// The server's clock, not the client's (`pbps_db::LedgerEntry`).
    pub applied_at: String,

    /// `apply`, `baseline`, `bootstrap`, `staged` or `failed`, read from the
    /// ledger's own column rather than from the recorded state, so it is there
    /// whether or not that state can be read.
    pub kind: String,

    /// The version of the recorded state's own format. A consumer that renders
    /// a field a later version added has to know which entries can hold it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_version: Option<u32>,

    /// Why the recorded state could not be read, when it could not — an entry
    /// older than this build understands, or one that does not parse. Set
    /// exactly when the fields taken from that state are absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unreadable: Option<String>,

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

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tables: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modules: Option<usize>,
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

fn row(entry: pbps_db::TimelineEntry) -> LedgerRow {
    // The projected columns are the row; the recorded state only adds to it.
    let mut out = LedgerRow {
        id: entry.id,
        applied_at: entry.applied_at,
        kind: entry.kind,
        state_version: None,
        unreadable: entry.unreadable,
        operator: entry.operator,
        git_sha: entry.git_sha,
        plan_checksum: entry.plan_checksum,
        reason: entry.reason,
        staged: None,
        tables: None,
        modules: None,
    };
    if let Some(snapshot) = entry.snapshot {
        out.state_version = Some(snapshot.version);
        out.staged = snapshot.staged.map(|s| StagedRow {
            completed: s.completed,
            total: s.total,
        });
        out.tables = Some(snapshot.schema.tables.len());
        out.modules = Some(snapshot.schema.modules.len());
    }
    out
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

        let found = match pbps_mssql::state::timeline(&mut conn, limit).await {
            Ok(entries) => Some(entries),
            // Not an error: a database this tool has never written to has no
            // history, and saying so is the answer. It is *not* an empty
            // history — the finding below is what keeps the two apart, and a
            // ledger that exists but cannot be read is neither (DECISIONS 218).
            Err(pbps_db::LedgerError::NotInitialized) => None,
            // Reached the server and could not read the ledger: a tool
            // failure, which exits 1. Never `Found` — that is exit 2, the code
            // reserved for a difference this command established, and an
            // envelope whose result is `unanswerable` beside an exit code
            // meaning "there are findings" routes the failure to the wrong
            // person (DECISIONS 219). `or_unanswerable` is what every
            // other step here uses, for the same reason.
            Err(e) => Some(output::or_unanswerable(
                "state list",
                json,
                "state.unreadable",
                Err(anyhow::anyhow!(
                    "{}: the ledger could not be read: {e}",
                    target.label
                )),
            )?),
        };
        let initialized = found.is_some();
        let entries = found.unwrap_or_default();

        let mut findings = Vec::new();
        // Named one by one, and as a warning rather than a note: a row this
        // build cannot read is a gap in what the page can show, and a reader
        // who is told nothing would take the missing counts for zero.
        for e in entries.iter().filter(|e| e.unreadable.is_some()) {
            let why = e.unreadable.as_deref().unwrap_or_default();
            findings.push(output::Finding::warning(
                "state.entry-unreadable",
                format!(
                    "{}: entry #{} was recorded by a version this build cannot read ({why}); \
                     its date, kind and operator are shown and the rest is left out",
                    target.label, e.id
                ),
            ));
        }
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
            // Sound because `--limit` refuses zero: with at least one row
            // asked for, nothing coming back means there is nothing to come
            // back. `TOP (0)` would make this sentence a lie about an
            // environment with a full history.
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
        // The findings to stderr and the table to stdout, as the other
        // commands split them: the table is what a reader pipes, and a
        // warning inside it would arrive as a row.
        if !findings.is_empty() {
            eprint!("{}", output::human(&findings));
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
                    None => e.kind.clone(),
                },
                e.operator.clone(),
                e.git_sha
                    .as_deref()
                    // Seven characters is what `git log --oneline` shows and
                    // what a reviewer pastes back; the full sha is in the JSON.
                    .map(|s| s.chars().take(7).collect())
                    .unwrap_or_else(|| "-".to_owned()),
                match (&e.reason, &e.unreadable) {
                    // The reason is the row's own text; an unreadable entry
                    // has none of its own and says why instead of nothing.
                    (Some(r), _) => r.clone(),
                    (None, Some(_)) => "(recorded by a newer or older format)".to_owned(),
                    (None, None) => String::new(),
                },
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
