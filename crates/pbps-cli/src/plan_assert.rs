//! `plan --assert`: reviewed read-only assertions run against the target while
//! the plan is computed (#2401, ADR-0019 §7, DEC-2401.4).
//!
//! The engines own the boundary: PR A's `run_target` entry points verify the
//! target database, a separate login and its effective permissions before any
//! user SQL. This module only selects the files, supplies the assertion
//! connection and decides what each outcome means for the plan: a pass is
//! recorded, everything else stops planning and writes nothing.
//!
//! An observation, not a guarantee. Nothing here runs at `apply`, so a row
//! added after `plan` is not caught there.

use std::path::{Path, PathBuf};
use std::time::Duration;

use pbps_db::query::{AssertionResult, EvidenceCell, QueryEvidence};
use pbps_db::{Conn, DbError, Driver};
use pbps_model::escape::Escaped;
use pbps_model::{AssertionOutcome, TargetAssertion};

use crate::output::Finding;
use crate::plan_test::files;

/// The only source of the assertion login. Never derived from the planning
/// connection: a login that can plan can write, and the engines refuse it.
pub(crate) const CONNECTION_VARIABLE: &str = "PBPS_ASSERTION_CONN";

/// The rows shown with a violation. The evidence itself is bounded by the
/// driver; this keeps a terminal readable.
const SHOWN_ROWS: usize = 3;

/// One assertion file as it was read: its record and the SQL that will run.
struct Frozen {
    record: TargetAssertion,
    text: String,
}

/// The assertions a plan was asked to run and the connection to run them on.
pub(crate) struct Selection {
    assertions: Vec<Frozen>,
    connection: String,
}

/// Why planning stopped. The first is an answer (exit 2); the second is a
/// question the engine could not answer (exit 1). Neither writes a plan.
pub(crate) enum Stop {
    Refused(Finding),
    Unanswerable(Finding),
}

/// An unanswerable assertion as an error, so the one place that turns planning
/// failures into an envelope keeps its specific finding.
#[derive(Debug, thiserror::Error)]
#[error("{}", .0.message)]
pub(crate) struct Unanswerable(pub Finding);

impl Selection {
    /// Reads every file once and returns every problem found, not the first.
    /// `None` when no `--assert` was given, even if the variable is set.
    pub(crate) fn prepare(
        project_root: &Path,
        paths: &[PathBuf],
        connection: Result<String, std::env::VarError>,
    ) -> Result<Option<Self>, Vec<Finding>> {
        if paths.is_empty() {
            return Ok(None);
        }
        let mut problems = Vec::new();
        let connection = match connection {
            Ok(value) if !value.trim().is_empty() => Some(value),
            Err(std::env::VarError::NotUnicode(_)) => {
                problems.push(connection_problem(format!(
                    "{CONNECTION_VARIABLE} is not valid UTF-8"
                )));
                None
            }
            _ => {
                problems.push(connection_problem(format!(
                    "--assert needs {CONNECTION_VARIABLE}, the connection string of a \
                     separate read-only assertion login; it is not set"
                )));
                None
            }
        };
        let root = match project_root.canonicalize() {
            Ok(root) => root,
            Err(e) => {
                problems.push(Finding::error(
                    "plan.assertion.input",
                    format!("cannot read the project root: {e}"),
                ));
                return Err(problems);
            }
        };
        let mut seen = std::collections::BTreeMap::new();
        let mut assertions = Vec::new();
        for path in paths {
            let shown = path.to_string_lossy();
            let read = match files::read(&root, path) {
                Ok(read) => read,
                Err(problem) => {
                    problems.push(input_problem(&shown, &problem.to_string()));
                    continue;
                }
            };
            let text = match read.text {
                Ok(text) => text,
                Err(problem) => {
                    problems.push(input_problem(&shown, &problem.to_string()));
                    continue;
                }
            };
            // Two spellings of one file would be run and recorded twice.
            if let Some(first) = seen.insert(read.resolved.clone(), shown.to_string()) {
                problems.push(input_problem(
                    &shown,
                    &format!("is the same file as `{}`", Escaped(&first)),
                ));
                continue;
            }
            // An empty file has no result shape; the engine would only say so
            // after connecting.
            if text.trim().is_empty() {
                problems.push(input_problem(&shown, "is empty"));
                continue;
            }
            assertions.push(Frozen {
                record: TargetAssertion {
                    path: read.project_path.as_str().into(),
                    sha256: read.sha256,
                    outcome: AssertionOutcome::Pass,
                },
                text,
            });
        }
        match (problems.is_empty(), connection) {
            (true, Some(connection)) => Ok(Some(Self {
                assertions,
                connection,
            })),
            _ => Err(problems),
        }
    }

    /// Runs each assertion in order on its own fresh connection and stops at
    /// the first one that is not a pass. The planning connection supplies the
    /// identity the assertion login must match, never the session itself.
    pub(crate) async fn run(&self, planning: &mut Conn) -> Result<Vec<TargetAssertion>, Stop> {
        let timeout = Duration::from_millis(u64::from(
            pbps_config::test_manifest::Timeouts::default()
                .assertion_ms
                .get(),
        ));
        let expected = match planning.driver() {
            Driver::Postgres => Expected::Postgres(
                pbps_pg::assertions::Target::read(planning)
                    .await
                    .map_err(|e| stop("the planning target", e))?,
            ),
            Driver::Mssql => Expected::Mssql(
                pbps_mssql::assertions::Target::read(planning)
                    .await
                    .map_err(|e| stop("the planning target", e))?,
            ),
        };
        let mut passed = Vec::new();
        for assertion in &self.assertions {
            let path = assertion.record.path.raw();
            let result = match &expected {
                Expected::Postgres(target) => {
                    pbps_pg::assertions::run_target(
                        &self.connection,
                        target,
                        &assertion.text,
                        timeout,
                    )
                    .await
                }
                Expected::Mssql(target) => {
                    pbps_mssql::assertions::run_target(
                        &self.connection,
                        target,
                        &assertion.text,
                        timeout,
                    )
                    .await
                }
            };
            classify(path, result)?;
            passed.push(assertion.record.clone());
        }
        Ok(passed)
    }
}

enum Expected {
    Postgres(pbps_pg::assertions::Target),
    Mssql(pbps_mssql::assertions::Target),
}

fn connection_problem(message: String) -> Finding {
    Finding::error("plan.assertion.connection", message).remedy(format!(
        "Set {CONNECTION_VARIABLE} to the connection string of a login with read-only \
         access to the planning database. It is never taken from --db or the environment's \
         own connection."
    ))
}

fn input_problem(path: &str, problem: &str) -> Finding {
    Finding::error(
        "plan.assertion.input",
        format!("assertion file `{}` {problem}", Escaped(path)),
    )
    .at(path, None)
}

/// A failure before or around the assertion's own SQL. A refusal is the
/// engine's answer about the login or target and the user can fix it; a
/// deadline or an unreadable answer is not an answer at all.
fn stop(what: &str, error: DbError) -> Stop {
    match error {
        DbError::Refused(reason) => Stop::Refused(
            Finding::error(
                "plan.assertion.refused",
                format!("{what} refused: {}", Escaped(&reason)),
            )
            .remedy(format!(
                "Point {CONNECTION_VARIABLE} at a separate login for the planning database \
                 with no write or DDL permission."
            )),
        ),
        DbError::DeadlineExceeded(reason) => Stop::Unanswerable(Finding::error(
            "plan.assertion.unanswerable",
            format!("{what}: {reason}"),
        )),
        other @ (DbError::BadConnectionString(_)
        | DbError::Connect { .. }
        | DbError::ConnectTimeout { .. }
        | DbError::Driver { .. }
        | DbError::Context { .. }
        | DbError::WrongSession { .. }
        | DbError::UnsupportedServer { .. }
        | DbError::BadRow(_)) => Stop::Unanswerable(Finding::error(
            "plan.assertion.unanswerable",
            format!(
                "{what} could not be checked: {}",
                Escaped(&other.to_string())
            ),
        )),
    }
}

/// `Ok` only for a pass. Every other outcome names the file and which kind of
/// failure it was; none can be read as a pass.
fn classify(path: &str, result: Result<AssertionResult, DbError>) -> Result<(), Stop> {
    let name = format!("assertion `{}`", Escaped(path));
    let finding = |id: &str, message: String| Finding::error(id, message).at(path, None);
    match result {
        Err(error) => Err(match stop(&name, error) {
            Stop::Refused(f) => Stop::Refused(f.at(path, None)),
            Stop::Unanswerable(f) => Stop::Unanswerable(f.at(path, None)),
        }),
        Ok(AssertionResult::Pass(_)) => Ok(()),
        Ok(AssertionResult::Violation(evidence)) => Err(Stop::Refused(
            finding("plan.assertion.rows", violation(&name, &evidence)).remedy(
                "Fix the data or the declarations, then plan again. A plan is written only \
                 when every assertion returns no rows.",
            ),
        )),
        Ok(AssertionResult::InvalidResult { message }) => Err(Stop::Refused(finding(
            "plan.assertion.sql",
            format!("{name} returned an invalid result: {}", Escaped(&message)),
        ))),
        Ok(AssertionResult::SqlError(error)) => Err(Stop::Refused(finding(
            "plan.assertion.sql",
            format!(
                "{name} failed with an error, which is not a pass: {}",
                Escaped(&error.to_string())
            ),
        ))),
        Ok(AssertionResult::BoundaryViolation) => Err(Stop::Refused(finding(
            "plan.assertion.boundary",
            format!("{name} left its read-only transaction, so no result can be trusted"),
        ))),
        Ok(AssertionResult::Unanswerable(error)) => Err(Stop::Unanswerable(finding(
            "plan.assertion.unanswerable",
            format!(
                "{name} could not be answered: {}",
                Escaped(&error.to_string())
            ),
        ))),
    }
}

fn violation(name: &str, evidence: &QueryEvidence) -> String {
    let more = if evidence.truncated {
        " or more (the result was cut at its bound)"
    } else {
        ""
    };
    let mut out = format!(
        "{name} returned {} row(s){more}; any row is a violation",
        evidence.rows.len()
    );
    let columns: Vec<_> = evidence.columns.iter().map(|c| c.name.as_str()).collect();
    for row in evidence.rows.iter().take(SHOWN_ROWS) {
        let cells: Vec<_> = columns
            .iter()
            .zip(row)
            .map(|(column, cell)| match cell {
                EvidenceCell::Null => format!("{}=NULL", Escaped(column)),
                EvidenceCell::Text { value, truncated } => format!(
                    "{}={}{}",
                    Escaped(column),
                    Escaped(value),
                    if *truncated { "…" } else { "" }
                ),
                EvidenceCell::Unrenderable => format!("{}=<unrenderable>", Escaped(column)),
            })
            .collect();
        out.push_str(&format!("\n    {}", cells.join(", ")));
    }
    out
}

#[cfg(test)]
mod tests;
