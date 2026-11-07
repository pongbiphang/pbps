//! Test evidence is a report; no executable artifact is a field (ADR-0019 §9).
use crate::output::{Finding, Report};
use pbps_config::test_manifest::CommitId;
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Default, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Passed,
    Failed,
    Unanswerable,
    #[default]
    Skipped,
}
#[derive(Default, Serialize, JsonSchema)]
pub(crate) struct Phase {
    pub phase: String,
    pub identity: String,
    /// Which comparison database a re-pin phase worked in (`original` or
    /// `proposed`); absent for an ordinary run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    pub outcome: Status,
    pub elapsed_ms: u64,
    pub findings: Vec<Finding>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub distance: Option<u64>,
    pub changes: Option<usize>,
    pub statements: Option<usize>,
    pub checksum: Option<String>,
    pub accepted: bool,
    pub started: bool,
    pub entry: Option<i64>,
    pub risks: Vec<String>,
    pub unavailable: BTreeMap<String, String>,
    /// The engine whose error numbers `record` reads. Not part of the report.
    #[serde(skip)]
    #[schemars(skip)]
    engine: Option<pbps_db::Driver>,
}
impl Phase {
    /// A phase that works against `engine`'s database. The engine is required
    /// because a server error code means different things on each: a SQLSTATE
    /// prefix on PostgreSQL, a plain number on SQL Server (`unanswerable`).
    /// Nothing defaults to an engine, so a caller that has none does not compile.
    pub fn on(engine: pbps_db::Driver, phase: &str, identity: impl Into<String>) -> Self {
        Self {
            engine: Some(engine),
            ..Self::offline(phase, identity)
        }
    }
    /// A phase that never holds a database error (the input check, cleanup, a
    /// manifest write). A coded error recorded here is a finding, never a guess.
    pub fn offline(phase: &str, identity: impl Into<String>) -> Self {
        Self {
            phase: phase.into(),
            identity: identity.into(),
            ..Self::default()
        }
    }
    /// Tests that do not exercise classification need no engine.
    #[cfg(test)]
    pub fn new(phase: &str, identity: impl Into<String>) -> Self {
        Self::offline(phase, identity)
    }
    /// Refreshing grants may add time and findings to an already completed
    /// phase; it must not erase that phase's measured cost or stronger outcome.
    pub(crate) fn record<T>(
        &mut self,
        started: std::time::Instant,
        id: &str,
        result: &anyhow::Result<T>,
    ) {
        self.elapsed_ms = self
            .elapsed_ms
            .saturating_add(super::execution::elapsed(started));
        match result {
            Ok(_) if matches!(self.outcome, Status::Skipped) => self.outcome = Status::Passed,
            Ok(_) => {}
            Err(error) => {
                let unknown = id == "test.infrastructure" || unanswerable(self.engine, error);
                self.outcome = if unknown || matches!(self.outcome, Status::Unanswerable) {
                    Status::Unanswerable
                } else {
                    Status::Failed
                };
                self.findings.push(Finding::error(id, format!("{error:#}")));
            }
        }
    }
}
/// Whether a failure says the run could not look, rather than what it found.
/// A tool-authored refusal answered; an uncoded transport or deadline failure
/// did not. With a code, the engine decides. A phase with no database never
/// reads a coded error as a fault by its number (ADR-0019 §9).
pub(super) fn unanswerable(engine: Option<pbps_db::Driver>, error: &anyhow::Error) -> bool {
    use pbps_db::{DbError, LedgerError};
    // A transparent ledger error skips the DbError in source(); a rows-read
    // source downcasts to Box<DbError>. Recover both shapes, as ledger_safe_reason
    // does, without replacing the error whose original cause the report keeps.
    let db = error.chain().find_map(|frame| {
        frame
            .downcast_ref::<DbError>()
            .or_else(|| frame.downcast_ref::<Box<DbError>>().map(AsRef::as_ref))
            .or_else(|| match frame.downcast_ref::<LedgerError>() {
                Some(LedgerError::Db(inner)) => Some(inner),
                _ => None,
            })
    });
    if let Some(mut db) = db {
        // Context has no verdict of its own, including around a code-free refusal.
        while let DbError::Context { source, .. } = db {
            db = source;
        }
        return !matches!(db, DbError::Refused(_))
            && db.server_error_code().is_none_or(|c| match engine {
                Some(driver) => fault_code(driver, &c),
                None => false,
            });
    }
    error
        .chain()
        .any(|e| e.is::<tokio::time::error::Elapsed>() || e.is::<std::io::Error>())
}
/// Whether a server-coded error is the engine or its host failing, by the
/// engine's own numbering.
fn fault_code(driver: pbps_db::Driver, code: &str) -> bool {
    match driver {
        pbps_db::Driver::Postgres => {
            ["08", "53", "57", "58", "XX"]
                .iter()
                .any(|p| code.starts_with(p))
                || matches!(code, "25P03" | "25P04")
        }
        pbps_db::Driver::Mssql => pbps_mssql::test_scratch::infrastructure_fault(code),
    }
}
/// [`fault_code`] for a database error an assertion returned.
pub(super) fn engine_fault(driver: pbps_db::Driver, error: &pbps_db::DbError) -> bool {
    error
        .server_error_code()
        .is_some_and(|code| fault_code(driver, &code))
}
#[derive(Serialize, JsonSchema)]
pub(crate) struct Column {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
}
#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
pub(crate) enum Cell {
    Null,
    Text { value: String, truncated: bool },
    Unrenderable { unrenderable: bool },
}
#[derive(Serialize, JsonSchema)]
pub(crate) struct Assertion {
    pub name: String,
    pub path: String,
    pub sha256: String,
    pub outcome: String,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
    pub truncated: bool,
}
impl Assertion {
    pub fn evidence(&mut self, evidence: pbps_db::query::QueryEvidence) {
        use pbps_db::query::EvidenceCell;
        self.columns = evidence
            .columns
            .into_iter()
            .map(|c| Column {
                name: c.name,
                type_name: c.type_name,
            })
            .collect();
        self.rows = evidence
            .rows
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|c| match c {
                        EvidenceCell::Null => Cell::Null,
                        EvidenceCell::Text { value, truncated } => Cell::Text { value, truncated },
                        EvidenceCell::Unrenderable => Cell::Unrenderable { unrenderable: true },
                    })
                    .collect()
            })
            .collect();
        self.truncated = evidence.truncated;
    }
}
/// What a verified re-pin compared and decided (ADR-0019 §5, §9).
#[derive(Serialize, JsonSchema)]
pub(crate) struct Repin {
    pub fixture: String,
    pub path: String,
    pub from: CommitId,
    pub to: CommitId,
    /// Everything the original side consumed: the committed manifest and fixture
    /// at `HEAD`, and each replayed revision's files.
    pub original_inputs: Vec<super::ConsumedFile>,
    /// Everything the proposed side consumed: the working files and revisions.
    pub proposed_inputs: Vec<super::ConsumedFile>,
    pub sides: Vec<Side>,
    pub tables: Vec<TableComparison>,
    pub allocators: Vec<AllocatorComparison>,
    pub manifest: ManifestWrite,
}
#[derive(Serialize, JsonSchema)]
pub(crate) struct Side {
    pub label: String,
    pub run_id: Option<String>,
    pub acquired_image: Option<String>,
    pub engine_build: Option<String>,
}
#[derive(Serialize, JsonSchema)]
pub(crate) struct RowDifference {
    pub original_count: u64,
    pub proposed_count: u64,
    pub row: Vec<Cell>,
}
#[derive(Serialize, JsonSchema)]
pub(crate) struct TableComparison {
    pub table: String,
    pub equal: bool,
    pub original_columns: Vec<String>,
    pub proposed_columns: Vec<String>,
    pub original_rows: u64,
    pub proposed_rows: u64,
    /// Bounded: the equality test sees every row, the report only some.
    pub differences: Vec<RowDifference>,
    pub truncated: bool,
}
#[derive(Serialize, JsonSchema)]
pub(crate) struct AllocatorComparison {
    pub sequence: String,
    pub used_by: Option<String>,
    pub original_next: Option<String>,
    pub proposed_next: Option<String>,
    pub equal: bool,
}
#[derive(Serialize, JsonSchema)]
pub(crate) struct ManifestWrite {
    /// `not_attempted`, `written` or `failed`.
    pub status: String,
    pub old_sha256: String,
    pub new_sha256: Option<String>,
    pub reason: Option<String>,
}
#[derive(Default, Serialize, JsonSchema)]
pub(crate) struct TestData {
    pub operation: String,
    pub input: Option<super::InputIdentity>,
    pub observed_input: super::ObservedInputs,
    pub inputs_reverified: bool,
    pub run_id: Option<String>,
    pub acquired_image: Option<String>,
    pub engine_build: Option<String>,
    pub unavailable: BTreeMap<String, String>,
    pub recovery_removed: Vec<String>,
    pub recovery_live: Vec<String>,
    pub phases: Vec<Phase>,
    pub assertions: Vec<Assertion>,
    pub candidate_sealed_checksum: Option<String>,
    pub candidate_accepted_checksum: Option<String>,
    pub candidate_executed_checksum: Option<String>,
    pub replay_hops: usize,
    pub replay_ms: u64,
    pub cleanup_confirmed: bool,
    /// Present only for `operation: "repin"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repin: Option<Repin>,
}
// Preserve the JSON findings while showing optional values and lists plainly to people.
fn summary_values<I>(values: I, human: bool) -> String
where
    I: IntoIterator + std::fmt::Debug,
    I::Item: std::fmt::Display,
{
    if !human {
        return format!("{values:?}");
    }
    let values: Vec<_> = values
        .into_iter()
        .map(|v| pbps_model::escape::Escaped(&v.to_string()).to_string())
        .collect();
    if values.is_empty() {
        "none".into()
    } else {
        values.join(", ")
    }
}
impl TestData {
    pub fn new(cleanup: bool) -> Self {
        Self {
            operation: if cleanup { "cleanup" } else { "run" }.into(),
            ..Self::default()
        }
    }
    pub fn for_repin() -> Self {
        Self {
            operation: "repin".into(),
            ..Self::default()
        }
    }
    pub(crate) fn summary(&self, human: bool) -> Vec<Finding> {
        use pbps_model::escape::Escaped;
        let mut notes = Vec::new();
        if let Some(input) = &self.input {
            notes.push(Finding::note(
                "test.identity",
                format!(
                    "{} candidate at {}; base {}; image {}",
                    if input.candidate.uncommitted {
                        "uncommitted working-tree"
                    } else {
                        "committed"
                    },
                    input.candidate.commit,
                    input.base,
                    input.image
                ),
            ));
        }
        notes.push(Finding::note(
            "test.runtime",
            format!(
                "run {}; acquired image {}; engine {}",
                summary_values(&self.run_id, human),
                summary_values(&self.acquired_image, human),
                summary_values(&self.engine_build, human)
            ),
        ));
        let files = self
            .input
            .as_ref()
            .map_or(&self.observed_input.files, |i| &i.files);
        for f in files {
            notes.push(Finding::note(
                "test.input.hash",
                format!(
                    "{} `{}` at {}: {}",
                    f.role,
                    Escaped(&f.path),
                    f.revision,
                    f.sha256
                ),
            ));
        }
        for (label, commit) in &self.observed_input.resolved_commits {
            notes.push(Finding::note(
                "test.input.commit",
                format!("{}: {commit}", Escaped(label)),
            ));
        }
        for p in &self.phases {
            let outcome = if human {
                match p.outcome {
                    Status::Passed => "passed",
                    Status::Failed => "failed",
                    Status::Unanswerable => "unanswerable",
                    Status::Skipped => "skipped",
                }
                .into()
            } else {
                format!("{:?}", p.outcome)
            };
            notes.push(Finding::note(
                "test.phase",
                format!(
                    concat!(
                        "{} `{}`: {}, {} ms; checksum {}; accepted {}; started {}; ",
                        "from {} to {}, distance {}; changes {}; statements {}; ",
                        "entry {}; risks {}"
                    ),
                    p.phase,
                    Escaped(&match &p.side {
                        Some(side) => format!("{side}: {}", p.identity),
                        None => p.identity.clone(),
                    }),
                    outcome,
                    p.elapsed_ms,
                    summary_values(&p.checksum, human),
                    p.accepted,
                    p.started,
                    summary_values(&p.from, human),
                    summary_values(&p.to, human),
                    summary_values(p.distance, human),
                    summary_values(p.changes, human),
                    summary_values(p.statements, human),
                    summary_values(p.entry, human),
                    summary_values(&p.risks, human)
                ),
            ));
        }
        for (field, reason) in self
            .unavailable
            .iter()
            .chain(self.phases.iter().flat_map(|p| p.unavailable.iter()))
        {
            notes.push(Finding::note(
                "test.unavailable",
                format!("{}: {reason}", Escaped(field)),
            ));
        }
        if let Some(repin) = &self.repin {
            notes.extend(repin.summary(human));
        }
        for assertion in &self.assertions {
            let evidence =
                serde_json::to_string(assertion).expect("typed test evidence serializes");
            notes.push(Finding::note("test.assertion.evidence", evidence));
        }
        notes.push(Finding::note(
            "test.cleanup",
            format!(
                concat!(
                    "cleanup confirmed: {}; replay hops: {}, {} ms; ",
                    "recovered {}; live {}"
                ),
                self.cleanup_confirmed,
                self.replay_hops,
                self.replay_ms,
                summary_values(&self.recovery_removed, human),
                summary_values(&self.recovery_live, human)
            ),
        ));
        notes
    }
    pub fn failure(&mut self, phase: usize, id: &str, message: String, unknown: bool) {
        let p = &mut self.phases[phase];
        p.outcome = if unknown || matches!(p.outcome, Status::Unanswerable) {
            Status::Unanswerable
        } else {
            Status::Failed
        };
        p.findings.push(Finding::error(id, message));
    }
    pub(crate) fn startup_failure(&mut self, phase: usize, error: &crate::test_scratch::Error) {
        if let crate::test_scratch::Error::StartCleanup { cleanup, .. } = error {
            // An earlier scratch's confirmation says nothing about this one.
            self.cleanup_confirmed = false;
            self.run_id = Some(cleanup.run_id.clone());
            let mut p = Phase::offline("cleanup", &cleanup.run_id);
            p.outcome = Status::Unanswerable;
            p.findings
                .push(Finding::error("test.cleanup", cleanup.to_string()).remedy(&cleanup.remedy));
            self.phases.push(p);
        }
        self.failure(phase, "test.infrastructure", error.to_string(), true);
    }
    pub(crate) fn reverify(&mut self, inputs: &super::Inputs) {
        let mut phase = Phase::offline("input", "final input verification");
        let started = std::time::Instant::now();
        match inputs.reverify() {
            Ok(()) => {
                self.inputs_reverified = true;
                phase.outcome = Status::Passed;
            }
            Err(refusals) => {
                phase.outcome = Status::Unanswerable;
                phase.findings.extend(
                    refusals
                        .into_iter()
                        .map(|r| Finding::error("test.input", r.to_string())),
                );
            }
        }
        phase.elapsed_ms = super::execution::elapsed(started);
        self.phases.push(phase);
    }
    pub fn finish(mut self) -> Report<Self> {
        if self.input.is_some()
            && !self.inputs_reverified
            && !self
                .phases
                .iter()
                .any(|p| matches!(p.outcome, Status::Failed | Status::Unanswerable))
        {
            let mut phase = Phase::offline("input", "final input verification");
            phase.outcome = Status::Unanswerable;
            phase.findings.push(Finding::error(
                "test.input",
                "the frozen inputs were not reverified; the result cannot be qualified",
            ));
            self.phases.push(phase);
        }
        if self.run_id.is_some() && !self.cleanup_confirmed {
            let mut cleanup = Phase::offline("cleanup", "owned resource confirmation");
            cleanup.outcome = Status::Unanswerable;
            cleanup.findings.push(Finding::error(
                "test.cleanup",
                "owned cleanup was not confirmed; run pbps test --cleanup",
            ));
            self.phases.push(cleanup);
        }
        for (field, absent, reason) in [
            (
                "input",
                self.input.is_none(),
                "input validation did not establish a complete input identity",
            ),
            (
                "run_id",
                self.run_id.is_none(),
                "scratch acquisition did not identify an owned run",
            ),
            (
                "acquired_image",
                self.acquired_image.is_none(),
                "the acquired image was not observed",
            ),
            (
                "engine_build",
                self.engine_build.is_none(),
                "the owned server build was not observed",
            ),
            (
                "candidate_sealed_checksum",
                self.candidate_sealed_checksum.is_none(),
                "no sealed candidate checksum was observed",
            ),
            (
                "candidate_accepted_checksum",
                self.candidate_accepted_checksum.is_none(),
                "candidate admission was not observed",
            ),
            (
                "candidate_executed_checksum",
                self.candidate_executed_checksum.is_none(),
                "candidate statement execution was not observed",
            ),
        ] {
            if absent {
                self.unavailable.insert(
                    field.into(),
                    match self.operation.as_str() {
                        "cleanup" => "not applicable to cleanup-only operation".to_owned(),
                        "repin" if field.starts_with("candidate_") => {
                            "not applicable to a re-pin: it plans no candidate".to_owned()
                        }
                        _ => reason.to_owned(),
                    },
                );
            }
        }
        self.observed_input.files.sort();
        self.observed_input.files.dedup();
        for p in &mut self.phases {
            for (field, absent) in [
                ("from", p.from.is_none()),
                ("to", p.to.is_none()),
                ("distance", p.distance.is_none()),
                ("changes", p.changes.is_none()),
                ("statements", p.statements.is_none()),
                ("checksum", p.checksum.is_none()),
                ("entry", p.entry.is_none()),
            ] {
                if absent {
                    p.unavailable.insert(
                        field.into(),
                        format!("not produced by the {} phase ({:?})", p.phase, p.outcome),
                    );
                }
            }
        }
        let unknown = self
            .phases
            .iter()
            .any(|p| matches!(p.outcome, Status::Unanswerable));
        let findings: Vec<Finding> = self
            .phases
            .iter()
            .flat_map(|p| p.findings.clone())
            .collect();
        let report = Report::new("test", findings, Some(self));
        if unknown {
            report.unanswerable()
        } else {
            report
        }
    }
}

impl Repin {
    fn summary(&self, human: bool) -> Vec<Finding> {
        use pbps_model::escape::Escaped;
        let header = (
            "test.repin",
            format!(
                "fixture `{}` ({}): {} -> {}",
                Escaped(&self.fixture),
                Escaped(&self.path),
                self.from,
                self.to
            ),
        );
        let tables = self.tables.iter().map(|t| {
            (
                "test.repin.table",
                format!(
                    "`{}`: {}; original {} rows, proposed {} rows; {} difference(s){}",
                    Escaped(&t.table),
                    if t.equal { "equal" } else { "different" },
                    t.original_rows,
                    t.proposed_rows,
                    t.differences.len(),
                    if t.truncated {
                        " shown (more exist)"
                    } else {
                        ""
                    }
                ),
            )
        });
        let allocators = self.allocators.iter().map(|a| {
            (
                "test.repin.allocator",
                format!(
                    "`{}` (used by {}): original next {}, proposed next {}: {}",
                    Escaped(&a.sequence),
                    summary_values(&a.used_by, human),
                    summary_values(&a.original_next, human),
                    summary_values(&a.proposed_next, human),
                    if a.equal { "equal" } else { "different" }
                ),
            )
        });
        let manifest = (
            "test.repin.manifest",
            format!(
                "manifest write {}; old {}; new {}",
                self.manifest.status,
                self.manifest.old_sha256,
                summary_values(&self.manifest.new_sha256, human)
            ),
        );
        std::iter::once(header)
            .chain(tables)
            .chain(allocators)
            .chain([manifest])
            .map(|(id, message)| Finding::note(id, message))
            .collect()
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(test, target_os = "linux"))]
mod live_tests;
