//! Operational estimates are command output, never saved-plan risk or gates.

use pbps_model::escape::Escaped;
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

impl From<pbps_dialect::estimate::Rewrite> for Rewrite {
    fn from(value: pbps_dialect::estimate::Rewrite) -> Self {
        use pbps_dialect::estimate::Rewrite as Work;
        match value {
            Work::Yes => Self::Yes,
            Work::No => Self::No,
            Work::Unknown(reason) => Self::Unknown { reason },
        }
    }
}

impl From<pbps_dialect::estimate::Reads> for Reads {
    fn from(value: pbps_dialect::estimate::Reads) -> Self {
        use pbps_dialect::estimate::Reads as Work;
        match value {
            Work::EveryRow => Self::EveryRow,
            Work::Nothing => Self::Nothing,
            Work::Unknown(reason) => Self::Unknown { reason },
        }
    }
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
                        let _ = writeln!(
                            out,
                            "  Change {}: {} ({})",
                            change_index + 1,
                            about,
                            Escaped(table)
                        );
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
                            let _ = writeln!(
                                out,
                                "    Also locks: {}",
                                also_locks
                                    .iter()
                                    .map(|name| Escaped(name).to_string())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            );
                        }
                        let _ = writeln!(out, "    Rows: {rows}");
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Change, ChangeSet, PlannedChange, TableName};

    #[test]
    fn offline_estimates_keep_raw_identity_fields_and_shown_prose() {
        for (part, shown) in [
            (
                "a\nb\r\u{1b}[31m\u{9b}\\c\u{200b}",
                r"a\nb\r\u{1b}[31m\u{9b}\\c\u{200b}",
            ),
            ("名前_ünï", "名前_ünï"),
        ] {
            let table = TableName::new("app", part);
            let changes = ChangeSet {
                changes: vec![PlannedChange::new(Change::AlterColumnType {
                    uid: "c_aaaaaa".parse().unwrap(),
                    column: table.column(part),
                    from: "int".parse().unwrap(),
                    to: "bigint".parse().unwrap(),
                    from_nullable: true,
                    to_nullable: true,
                    through_external_partitions: false,
                    from_collation: None,
                    to_collation: None,
                    using: None,
                })],
            };
            let ids = crate::plan_fixture::ids(&changes);
            let names = pbps_model::plan_names::PlanNames::of(&changes.changes, &ids).unwrap();
            let pg = pbps_pg::estimate::planned_estimates(&names).remove(0).1;
            let ms = pbps_mssql::estimate::planned_estimates(&names).remove(0).1;
            for (engine, about, suffix) in
                [("postgres", pg.about, ""), ("sqlserver", ms.about, " NULL")]
            {
                let report = CostReport::Available {
                    engine,
                    changes: vec![ChangeCost::Available {
                        change_index: 0,
                        about,
                        table: table.raw().to_string(),
                        rewrite: Rewrite::No,
                        reads: Reads::Nothing,
                        lock: "test".into(),
                        blocks: "test".into(),
                        also_locks: vec![table.raw().to_string()],
                        rows: Rows::Estimated { count: 0 },
                    }],
                };
                let json = serde_json::to_value(&report).unwrap();
                assert_eq!(
                    json["changes"][0]["about"],
                    format!("changing app.{shown}.{shown} to bigint{suffix}")
                );
                assert_eq!(json["changes"][0]["table"], format!("app.{part}"));
                assert_eq!(json["changes"][0]["also_locks"][0], format!("app.{part}"));
                let text = render(&report);
                assert!(
                    text.contains(&format!(
                        "  Change 1: changing app.{shown}.{shown} to bigint{suffix} (app.{shown})\n"
                    )),
                    "{text:?}"
                );
                assert!(
                    text.contains(&format!("    Also locks: app.{shown}\n")),
                    "{text:?}"
                );
                assert!(
                    !text.chars().any(|c| c.is_control() && c != '\n'),
                    "{text:?}"
                );
            }
        }
    }
}
