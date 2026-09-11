//! Operational estimates are command output, never saved-plan risk or gates.

use schemars::JsonSchema;
use serde::Serialize;
use std::fmt::Write as _;

#[derive(Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CostReport {
    Available {
        engine: &'static str,
        changes: Vec<ChangeCost>,
    },
    Unavailable {
        engine: &'static str,
        reason: String,
    },
}

#[derive(Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ChangeCost {
    Available {
        /// Zero-based index in the saved plan's change set.
        change_index: usize,
        about: String,
        table: String,
        rewrite: Rewrite,
        reads: Reads,
        lock: String,
        blocks: String,
        also_locks: Vec<String>,
        rows: Rows,
    },
    Unavailable {
        change_index: usize,
        reason: String,
    },
}

#[derive(Serialize, JsonSchema)]
#[serde(tag = "value", rename_all = "snake_case")]
pub enum Rewrite {
    Yes,
    No,
    Unknown { reason: String },
}

#[derive(Serialize, JsonSchema)]
#[serde(tag = "value", rename_all = "snake_case")]
pub enum Reads {
    EveryRow,
    Nothing,
    Unknown { reason: String },
}

#[derive(Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Rows {
    Estimated { count: i64 },
    NeverAnalyzed,
    Unknown { reason: String },
}

pub fn render(report: &CostReport) -> String {
    let mut out = String::from("\nOperational cost estimate (separate from correctness risk):\n");
    match report {
        CostReport::Unavailable { engine, reason } => {
            let _ = writeln!(out, "  {engine}: unavailable — {reason}");
        }
        CostReport::Available { engine, changes } => {
            let _ = writeln!(
                out,
                "  {engine}; row counts are approximate catalog estimates."
            );
            if changes.is_empty() {
                out.push_str("  No planned changes.\n");
            }
            for change in changes {
                match change {
                    ChangeCost::Unavailable {
                        change_index,
                        reason,
                    } => {
                        let _ =
                            writeln!(out, "  Change {}: unavailable — {reason}", change_index + 1);
                    }
                    ChangeCost::Available {
                        change_index,
                        about,
                        table,
                        rewrite,
                        reads,
                        lock,
                        blocks,
                        also_locks,
                        rows,
                    } => {
                        let _ = writeln!(out, "  Change {}: {about} ({table})", change_index + 1);
                        let rewrite = match rewrite {
                            Rewrite::Yes => "yes".to_owned(),
                            Rewrite::No => "no".to_owned(),
                            Rewrite::Unknown { reason } => format!("unknown — {reason}"),
                        };
                        let reads = match reads {
                            Reads::EveryRow => "every row".to_owned(),
                            Reads::Nothing => "nothing".to_owned(),
                            Reads::Unknown { reason } => format!("unknown — {reason}"),
                        };
                        let rows = match rows {
                            Rows::Estimated { count } => format!("approximately {count}"),
                            Rows::NeverAnalyzed => "unknown — never analyzed".to_owned(),
                            Rows::Unknown { reason } => format!("unknown — {reason}"),
                        };
                        let _ = writeln!(out, "    Rewrite: {rewrite}; reads: {reads}");
                        let _ = writeln!(out, "    Lock: {lock}; blocks {blocks}");
                        if !also_locks.is_empty() {
                            let _ = writeln!(out, "    Also locks: {}", also_locks.join(", "));
                        }
                        let _ = writeln!(out, "    Rows: {rows}");
                    }
                }
            }
        }
    }
    out
}
