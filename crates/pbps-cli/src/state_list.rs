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

    /// Why the recorded state could not be read, when it could not. Set exactly
    /// when the fields taken from that state are absent, and typed rather than
    /// a sentence, because the two cases send an operator to different places
    /// (DECISIONS 222).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unreadable: Option<Unreadable>,

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

/// Why one row's recorded state is missing from the timeline.
///
/// A tagged object rather than a string: a consumer that renders "upgrade pbps"
/// must not draw it for a row whose JSON is damaged, and reading that out of a
/// message is not something a schema can promise.
#[derive(serde::Serialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Unreadable {
    /// The state names a format this build does not read. `detail` is
    /// `StateSnapshot::check_version`'s own message, which names the direction
    /// and what to do about it.
    UnsupportedVersion { detail: String },

    /// The state did not parse. The row is damaged, and no version of this tool
    /// reads it; the ledger's own columns are all there is.
    Malformed { detail: String },
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
        unreadable: None,
        operator: entry.operator,
        git_sha: entry.git_sha,
        plan_checksum: entry.plan_checksum,
        reason: entry.reason,
        staged: None,
        tables: None,
        modules: None,
    };
    match entry.state {
        Ok(snapshot) => {
            out.state_version = Some(snapshot.version);
            out.staged = snapshot.staged.map(|s| StagedRow {
                completed: s.completed,
                total: s.total,
            });
            out.tables = Some(snapshot.schema.tables.len());
            out.modules = Some(snapshot.schema.modules.len());
        }
        Err(pbps_model::Unreadable::UnsupportedVersion(detail)) => {
            out.unreadable = Some(Unreadable::UnsupportedVersion { detail });
        }
        Err(pbps_model::Unreadable::Malformed(detail)) => {
            out.unreadable = Some(Unreadable::Malformed { detail });
        }
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
            // ledger that exists but cannot be read is neither (DECISIONS 219).
            Err(pbps_db::LedgerError::NotInitialized) => None,
            // Reached the server and could not read the ledger: a tool
            // failure, which exits 1. Never `Found` — that is exit 2, the code
            // reserved for a difference this command established, and an
            // envelope whose result is `unanswerable` beside an exit code
            // meaning "there are findings" routes the failure to the wrong
            // person (DECISIONS 220). `or_unanswerable` is what every
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
        // who is told nothing would take the missing counts for zero. Two ids,
        // because the two failures are two different jobs for whoever reads
        // them — one is a build to change, the other a damaged row
        // (DECISIONS 222).
        for e in &entries {
            let (id, what) = match &e.state {
                Ok(_) => continue,
                Err(pbps_model::Unreadable::UnsupportedVersion(detail)) => (
                    "state.entry-unsupported-version",
                    format!(
                        "was recorded in a state format this build does not read ({})",
                        one_line(detail)
                    ),
                ),
                Err(pbps_model::Unreadable::Malformed(detail)) => (
                    "state.entry-malformed",
                    format!(
                        "has a recorded state that does not parse, so the row is damaged ({})",
                        one_line(detail)
                    ),
                ),
            };
            findings.push(output::Finding::warning(
                id,
                format!(
                    "{}: entry #{} {what}; its date, kind and operator are shown \
                     and the rest is left out",
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

/// One line, whatever the ledger holds.
///
/// `operator` and `reason` are free text — `--reason $'ticket\nwhy'`, or a
/// driver's multi-line failure copied into a `failed` entry — and the table's
/// columns are laid out by counting characters. A cell holding a line break
/// ends its row early, so the rest of it starts again at column 1 and reads as
/// an entry of its own: a rendering that does not say "this reason had a
/// newline in it" but says something false about how many times this database
/// was deployed to. Control characters are shown escaped rather than dropped,
/// because what was recorded is the point of the column, and `--format json`
/// still carries the original (DECISIONS 221).
fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out
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
                    // has none of its own and says why instead of nothing —
                    // which of the two, because they are not the same news.
                    (Some(r), _) => r.clone(),
                    (None, Some(Unreadable::UnsupportedVersion { .. })) => {
                        "(state format not read by this build)".to_owned()
                    }
                    (None, Some(Unreadable::Malformed { .. })) => {
                        "(recorded state is damaged)".to_owned()
                    }
                    (None, None) => String::new(),
                },
            ]
            // Every cell, not the two that are free text today: a column that
            // cannot hold a line break is a column no later edit can break.
            .map(|cell| one_line(&cell))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row_with(operator: &str, reason: Option<&str>) -> LedgerRow {
        LedgerRow {
            id: 1,
            applied_at: "2026-09-06T10:00:00.000".to_owned(),
            kind: "apply".to_owned(),
            state_version: Some(1),
            unreadable: None,
            operator: operator.to_owned(),
            git_sha: Some("0123456789abcdef".to_owned()),
            plan_checksum: None,
            reason: reason.map(str::to_owned),
            staged: None,
            tables: Some(1),
            modules: Some(0),
        }
    }

    /// A row is one line of the table, whatever text the ledger holds.
    ///
    /// `--reason $'ticket-9\nwhy'` reaches the ledger as written. Copied into a
    /// cell it ended the row early, and the rest appeared at column 1 — an
    /// entry the database does not have, in a command whose whole output is a
    /// count of entries.
    #[test]
    fn a_reason_with_a_line_break_is_still_one_row() {
        let data = StateListData {
            environment: "demo".to_owned(),
            initialized: true,
            limit: 50,
            entries: vec![
                row_with("someone", Some("ticket-9\nexplained at length")),
                row_with("some\tone", Some("plain")),
            ],
        };
        let out = render(&data);
        assert_eq!(
            out.lines().count(),
            3,
            "one header and two rows, not four lines:\n{out}"
        );
        assert!(out.contains("ticket-9\\nexplained"), "{out}");
        assert!(out.contains("some\\tone"), "{out}");
        // The escape is visible, not a dropped character: the reason is the
        // reason the row exists.
        assert!(!out.contains("ticket-9explained"), "{out}");
    }

    /// The columns still line up once a cell has been escaped, because the
    /// width is measured on what is printed rather than on what was recorded.
    #[test]
    fn column_widths_are_measured_on_the_escaped_text() {
        let data = StateListData {
            environment: "demo".to_owned(),
            initialized: true,
            limit: 50,
            entries: vec![
                row_with("a\nb", Some("x")),
                row_with("wideoperator", Some("y")),
            ],
        };
        let out = render(&data);
        let lines: Vec<&str> = out.lines().collect();
        let detail = lines[0].find("DETAIL").expect("header has DETAIL");
        assert_eq!(
            lines[1].find('x'),
            Some(detail),
            "the escaped cell must not shift its row:\n{out}"
        );
    }

    /// The detail column says which kind of unreadable a row is.
    ///
    /// One placeholder for both meant a damaged row was labelled a format
    /// problem, which sends an operator looking for an upgrade that does not
    /// exist (DECISIONS 222).
    #[test]
    fn the_detail_column_names_which_kind_of_unreadable_a_row_is() {
        let unreadable = |u: Unreadable| {
            let mut r = row_with("someone", None);
            r.reason = None;
            r.unreadable = Some(u);
            r.state_version = None;
            r.tables = None;
            r.modules = None;
            StateListData {
                environment: "demo".to_owned(),
                initialized: true,
                limit: 50,
                entries: vec![r],
            }
        };
        let damaged = render(&unreadable(Unreadable::Malformed {
            detail: "expected value at line 1 column 1".to_owned(),
        }));
        assert!(damaged.contains("(recorded state is damaged)"), "{damaged}");
        assert!(
            !damaged.contains("format"),
            "a damaged row is not a format problem: {damaged}"
        );

        let old = render(&unreadable(Unreadable::UnsupportedVersion {
            detail: "this is a version 9 state".to_owned(),
        }));
        assert!(
            old.contains("(state format not read by this build)"),
            "{old}"
        );

        // A row that has a reason of its own keeps it: the placeholder stands
        // in for nothing, never over something.
        let mut data = unreadable(Unreadable::Malformed {
            detail: "x".to_owned(),
        });
        data.entries[0].reason = Some("ticket-9".to_owned());
        let kept = render(&data);
        assert!(
            kept.contains("ticket-9") && !kept.contains("damaged"),
            "{kept}"
        );
    }

    /// A control character with no short escape is shown, not dropped.
    #[test]
    fn an_unprintable_character_is_shown_rather_than_dropped() {
        assert_eq!(one_line("a\u{7}b"), "a\\u{7}b");
        assert_eq!(one_line("plain"), "plain");
    }
}
