//! Test evidence is a report; no executable artifact is a field (ADR-0019 §9).
use crate::output::{Finding, Report};
use schemars::JsonSchema;
use serde::Serialize;

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
}
impl Phase {
    pub fn new(phase: &str, identity: impl Into<String>) -> Self {
        Self {
            phase: phase.into(),
            identity: identity.into(),
            ..Self::default()
        }
    }
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
#[derive(Default, Serialize, JsonSchema)]
pub(crate) struct TestData {
    pub operation: String,
    pub input: Option<super::InputIdentity>,
    pub run_id: Option<String>,
    pub acquired_image: Option<String>,
    pub engine_build: Option<String>,
    pub unavailable_reason: Option<String>,
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
}
impl TestData {
    pub fn new(cleanup: bool) -> Self {
        Self {
            operation: if cleanup { "cleanup" } else { "run" }.into(),
            ..Self::default()
        }
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
    pub fn finish(mut self) -> Report<Self> {
        if self.run_id.is_some() && !self.cleanup_confirmed {
            let mut cleanup = Phase::new("cleanup", "owned resource confirmation");
            cleanup.outcome = Status::Unanswerable;
            cleanup.findings.push(Finding::error(
                "test.cleanup",
                "owned cleanup was not confirmed; run pbps test --cleanup",
            ));
            self.phases.push(cleanup);
        }
        let unknown = self
            .phases
            .iter()
            .any(|p| matches!(p.outcome, Status::Unanswerable));
        let findings = self
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
