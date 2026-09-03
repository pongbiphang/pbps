//! One machine-readable shape for every read-only command (SPEC §14.1).
//!
//! # Why one shape rather than one per command
//!
//! A frontend — a CI annotator, the optional UI of [ADR-0006], someone's own
//! dashboard — has to render what `validate`, `fmt --check`, `plan --check` and
//! `verify` each found. If every command invents its own JSON, each of those
//! consumers grows a parser per command, and the fifth command ships without one
//! because nobody noticed. So the *diagnostics* have one shape here, and a
//! command's own payload rides beside them in `data` rather than replacing them.
//!
//! # Why the vendor annotations are not produced here
//!
//! GitHub's `::error file=,line=::` and GitLab's code-quality JSON are formats
//! that change on someone else's schedule, and each one that enters the binary
//! has to be kept working by this project forever. A converter in `scripts/`
//! reading this envelope is the same feature with the maintenance in a place
//! where breaking it costs a script rather than a release (SPEC §14.1).
//!
//! [ADR-0006]: ../../../docs/ADR-0006-optional-ui.md

use std::path::Path;

use serde::Serialize;

/// The version of *this envelope*, bumped when a consumer would have to change.
///
/// Separate from the tool version, which moves for reasons a parser does not
/// care about (SPEC §14.2, acceptance criterion 6).
pub const SCHEMA_VERSION: u32 = 1;

/// How severely a finding should be taken.
///
/// `Error` is what decides the exit code; the other two are carried so a
/// consumer can show them without the command having to fail. Nothing here is
/// suppressed by severity — a warning that is never rendered is a warning that
/// does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Note,
}

/// Where in the working tree a finding is.
///
/// The file is a `String`, not a `PathBuf`, and that is the whole point: serde's
/// `Path` impl **fails** on a path that is not UTF-8, and a `PathBuf` here made
/// that failure the whole envelope's. `explain --plan <non-UTF-8> --format json`
/// on an unreadable plan printed nothing at all and exited 1 with `error: path
/// contains invalid UTF-8 characters` — the one-envelope contract broken by the
/// envelope itself. Holding a `String` makes that unrepresentable.
#[derive(Debug, Clone, Serialize)]
pub struct Location {
    pub file: String,
    /// 1-based, when the diagnostic knows one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

/// One thing the user has to act on.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    /// A stable, dotted identifier: `load.semantic`, `identity.stale`,
    /// `fmt.not-canonical`.
    ///
    /// Stable is the operative word. It is what a `policies:` block will raise
    /// or lower the severity of (SPEC §14.1), and what a team suppresses by,
    /// so it must survive a reworded message.
    pub id: String,

    pub severity: Severity,

    /// What is wrong, in the user's vocabulary.
    pub message: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,

    /// A command to copy and paste.
    ///
    /// The same rule as [`crate::report`]: a non-interactive run is never
    /// prompted, so the finding itself has to be the instructions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
}

impl Finding {
    pub fn error(id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            severity: Severity::Error,
            message: message.into(),
            location: None,
            remedy: None,
        }
    }

    pub fn warning(id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            ..Self::error(id, message)
        }
    }

    pub fn note(id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Note,
            ..Self::error(id, message)
        }
    }

    /// Attaches a location — unless the path cannot be spelled.
    ///
    /// `to_str`, not `display()`. A lossy rendering would put a name in
    /// `location.file` that points at a *different* file, and a consumer keying
    /// off that field would open the wrong one or none; the same reasoning that
    /// keeps a lossy path out of an advertised command. Dropping the location
    /// loses the pointer and keeps the finding, which is the smaller loss — the
    /// message still names what happened.
    pub fn at(mut self, file: impl AsRef<Path>, line: Option<usize>) -> Self {
        self.location = file.as_ref().to_str().map(|file| Location {
            file: file.to_owned(),
            line,
        });
        self
    }

    pub fn remedy(mut self, remedy: impl Into<String>) -> Self {
        self.remedy = Some(remedy.into());
        self
    }

    /// The severity as a word, for a line of prose.
    pub fn severity_word(&self) -> &'static str {
        match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }
}

/// What a command ended on — the same three-way split as the exit codes.
///
/// Three values, not two, because a consumer that has to re-derive "could not
/// answer" from the findings has re-implemented the routing rule the exit codes
/// exist to carry. `scripts/findings-to-github.py` maps this straight to the
/// process's own exit code, and a two-valued `result` made it turn `doctor`'s
/// exit 1 into a 2 — routing an unreachable database to the author of the
/// schema change, which is exactly what §9.8 is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// Nothing to act on. Exit 0.
    Ok,
    /// The command answered and found something. Exit 2.
    Findings,
    /// The command could not answer. Exit 1.
    Unanswerable,
}

/// What a read-only command produced.
///
/// Generic over the payload so a command with one — `verify`'s drift report,
/// `status`'s environment rows — carries it without inventing a second
/// envelope. Commands with nothing but diagnostics use [`Report::plain`].
#[derive(Debug, Serialize)]
pub struct Report<T: Serialize> {
    pub schema_version: u32,
    pub tool_version: &'static str,
    pub command: &'static str,

    /// Derived from the findings rather than set by the caller, so it cannot
    /// come to disagree with them — except for [`Outcome::Unanswerable`], which
    /// no finding can imply and which the command marks explicitly with
    /// [`Report::unanswerable`].
    pub result: Outcome,

    pub findings: Vec<Finding>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
}

impl Report<()> {
    /// A report that is nothing but its diagnostics.
    pub fn plain(command: &'static str, findings: Vec<Finding>) -> Self {
        Report::new(command, findings, None)
    }
}

impl<T: Serialize> Report<T> {
    pub fn new(command: &'static str, findings: Vec<Finding>, data: Option<T>) -> Self {
        let failed = findings.iter().any(|f| f.severity == Severity::Error);
        Self {
            schema_version: SCHEMA_VERSION,
            tool_version: env!("CARGO_PKG_VERSION"),
            command,
            result: if failed {
                Outcome::Findings
            } else {
                Outcome::Ok
            },
            findings,
            data,
        }
    }

    /// Marks this report as one the command could not answer (exit 1).
    ///
    /// Deliberately explicit. No pattern in the findings distinguishes "the
    /// database is unreachable" from "the declarations are invalid" — both are
    /// errors — so the command that knows which it was has to say so.
    pub fn unanswerable(mut self) -> Self {
        self.result = Outcome::Unanswerable;
        self
    }

    pub fn has_errors(&self) -> bool {
        self.findings.iter().any(|f| f.severity == Severity::Error)
    }

    /// Prints as JSON and returns the outcome the command should end on.
    ///
    /// Returning rather than exiting: a command may have cleanup — a lock to
    /// release — and a print that also exited would skip it.
    pub fn emit_json(&self) -> anyhow::Result<()> {
        println!("{}", serde_json::to_string_pretty(self)?);
        self.outcome()
    }

    /// `Ok` when nothing failed, [`crate::Found`] when something did.
    pub fn outcome(&self) -> anyhow::Result<()> {
        if self.has_errors() {
            // The findings have been printed in full; a summary line underneath
            // them would say less than they already do.
            Err(crate::Found::reported().into())
        } else {
            Ok(())
        }
    }
}

/// Runs a step that must not escape the one-envelope contract.
///
/// Every read-only command has setup that can fail before it has anything to
/// report — selecting the dialect, listing the declarations, opening a runtime.
/// A `?` on any of them leaves stdout empty in JSON mode, and a consumer then
/// gets the converter's generic "produced no output" instead of a typed finding
/// naming the problem.
///
/// This was found seven separate times on this branch, one command at a time,
/// which is what a per-site fix looks like from the outside. Routing every such
/// step through one function is the structural version: a new fallible step is
/// wrapped or it is not, and that is visible at the call site.
///
/// The result is always [`Outcome::Unanswerable`] — the command did not get far
/// enough to answer — and the error is returned unchanged, so the exit code and
/// the human path are exactly what they were.
pub fn or_unanswerable<T>(
    command: &'static str,
    json: bool,
    id: &'static str,
    step: anyhow::Result<T>,
) -> anyhow::Result<T> {
    match step {
        Ok(v) => Ok(v),
        Err(e) => {
            if json {
                let report = Report::plain(command, vec![Finding::error(id, format!("{e:#}"))])
                    .unanswerable();
                // A serialization failure here would be a bug in this crate's
                // own types; printing nothing is still better than panicking on
                // top of the error being reported.
                if let Ok(text) = serde_json::to_string_pretty(&report) {
                    println!("{text}");
                }
            }
            Err(e)
        }
    }
}

/// Renders the findings for a person.
///
/// Deliberately plain: the rich span-annotated form belongs to the loader's own
/// miette diagnostics, which quote the source. This is the fallback for findings
/// that have no source to quote, and the whole of the `--format human` output
/// for commands whose findings never do.
pub fn human(findings: &[Finding]) -> String {
    let mut out = String::new();
    for f in findings {
        let where_ = match &f.location {
            Some(l) => match l.line {
                Some(line) => format!("{}:{line}: ", l.file),
                None => format!("{}: ", l.file),
            },
            None => String::new(),
        };
        let label = match f.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        };
        out.push_str(&format!("  {where_}{label}: {}\n", f.message));
        if let Some(r) = &f.remedy {
            out.push_str(&format!("    {r}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_with_only_warnings_is_still_ok() {
        let r = Report::plain("validate", vec![Finding::warning("x.y", "careful")]);
        assert_eq!(r.result, Outcome::Ok);
        assert!(!r.has_errors());
        assert!(r.outcome().is_ok());
    }

    /// The severity decides the exit code, so a command must not be able to
    /// report an error and still be counted a success by forgetting to say so.
    #[test]
    fn one_error_among_warnings_makes_the_report_fail() {
        let r = Report::plain(
            "validate",
            vec![
                Finding::note("a.b", "fyi"),
                Finding::error("c.d", "broken"),
                Finding::warning("e.f", "careful"),
            ],
        );
        assert_eq!(r.result, Outcome::Findings);
        assert!(r.outcome().is_err());
    }

    /// A consumer keys off `id`, so it has to survive into the JSON unchanged,
    /// and an absent location or remedy must be absent rather than null — a
    /// consumer that has to distinguish `null` from missing has two cases where
    /// there is one fact.
    #[test]
    fn the_json_omits_what_a_finding_does_not_have() {
        let r = Report::plain(
            "fmt",
            vec![Finding::error("fmt.not-canonical", "rewrite me")],
        );
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["schema_version"], SCHEMA_VERSION);
        assert_eq!(v["findings"][0]["id"], "fmt.not-canonical");
        assert_eq!(v["findings"][0]["severity"], "error");
        assert!(v["findings"][0].get("location").is_none());
        assert!(v["findings"][0].get("remedy").is_none());
        assert!(v.get("data").is_none());
    }

    /// The converter maps `result` straight to its own exit code, so the third
    /// value has to survive into the JSON under the name it is documented by.
    #[test]
    fn an_unanswerable_report_says_so_in_the_json() {
        let r = Report::plain(
            "doctor",
            vec![Finding::error("environment.unreachable", "no")],
        )
        .unanswerable();
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["result"], "unanswerable");
        // The findings are unchanged: only the routing differs.
        assert_eq!(v["findings"][0]["id"], "environment.unreachable");
    }

    /// The error is returned unchanged, so the exit code and the human path are
    /// exactly what they were before the envelope existed.
    #[test]
    fn a_wrapped_failure_is_still_the_same_failure() {
        let err = or_unanswerable::<()>(
            "doctor",
            false,
            "project.unsupported-dialect",
            Err(anyhow::anyhow!("no postgres yet")),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "no postgres yet");

        // And a step that succeeds passes straight through.
        let ok = or_unanswerable("doctor", true, "x.y", Ok(7)).unwrap();
        assert_eq!(ok, 7);
    }

    #[test]
    fn a_located_finding_carries_its_file_and_line() {
        let f = Finding::error("load.semantic", "bad type").at("schema/dbo.t.yml", Some(7));
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["location"]["file"], "schema/dbo.t.yml");
        assert_eq!(v["location"]["line"], 7);
        assert!(human(std::slice::from_ref(&f)).contains("schema/dbo.t.yml:7: error: bad type"));
    }
}
