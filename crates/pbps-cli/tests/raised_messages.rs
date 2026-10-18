//! Engine-raised prose escapes names independently of SQL identifiers (#2161).

use pbps_dialect::Dialect;
use pbps_model::{Change, RowKey, TableName};

const HOSTILE: &str = "name\n\u{1b}[31m\\'_%名前";
const SHOWN: &str = r"name\n\u{1b}[31m\\'_%名前";

fn sql(dialect: &dyn Dialect, change: &Change) -> String {
    dialect
        .emit(change, Default::default())
        .unwrap()
        .into_iter()
        .map(|s| s.sql)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn postgres_raised_row_messages_show_names_once_and_keep_sql_identifiers_raw() {
    row_messages(&pbps_pg::Postgres::new());
}

#[test]
fn mssql_raised_row_messages_show_names_once_and_keep_sql_identifiers_raw() {
    row_messages(&pbps_mssql::Mssql);
}

fn row_messages(dialect: &dyn Dialect) {
    let pg = dialect.name() == "postgres";
    for (name, shown) in [(HOSTILE, SHOWN), ("ordinary'_%名前", "ordinary'_%名前")] {
        let table = TableName::new("app", name);
        let insert = Change::InsertRow {
            table: table.clone(),
            key_column: "code".into(),
            identity_key: false,
            key: RowKey::from("x"),
            row: Default::default(),
            defaults: Default::default(),
            types: Default::default(),
        };
        let delete = Change::DeleteRow {
            table,
            key_column: "code".into(),
            key: RowKey::from("x"),
            cause: pbps_model::change::DeleteCause::Undeclared,
            row: Default::default(),
            dropped: Default::default(),
            types: Default::default(),
            after_types: Default::default(),
            key_type: None,
        };
        for (change, count) in [(insert, 1), (delete, if pg { 4 } else { 3 })] {
            let sql = sql(dialect, &change);
            let marker = if pg {
                "RAISE EXCEPTION USING MESSAGE = "
            } else {
                "THROW 50000, "
            };
            let prefix = if pg {
                format!(
                    "{marker}E'app.{} row `x` ",
                    shown.replace('\\', "\\\\").replace('\'', "''")
                )
            } else {
                format!("{marker}N'app.{} row `x` ", shown.replace('\'', "''"))
            };
            let named = format!("{marker}{}'app.", if pg { "E" } else { "N" });
            assert_eq!(sql.matches(&named).count(), count, "{sql}");
            assert_eq!(sql.matches(&prefix).count(), count, "{sql}");
            // Negative: escaping the prose must not address a different table.
            let identifier = if pg {
                format!("\"app\".\"{name}\"")
            } else {
                format!("[app].[{name}]")
            };
            assert!(sql.contains(&identifier), "{sql}");
        }
    }
}

fn partition(name: &str, allowed: Option<u64>) -> Change {
    Change::CreateTable {
        uid: "t_aaaaaa".parse().unwrap(),
        name: TableName::new(name, "part"),
        table: Box::new(pbps_model::Table {
            partition_of: Some(pbps_model::PartitionOf {
                parent: TableName::new(name, "parent"),
                bound: pbps_model::PartitionBound::Range {
                    from: vec![pbps_model::BoundDatum::Value("1".into())],
                    to: vec![pbps_model::BoundDatum::MaxValue],
                },
                columns: Default::default(),
            }),
            ..Default::default()
        }),
        beside_default: Some(TableName::new(name, "rest")),
        moves_at_most: allowed,
    }
}

#[test]
fn raised_partition_and_numbering_messages_show_names_once() {
    let pg = pbps_pg::Postgres::new();
    for (name, shown) in [(HOSTILE, SHOWN), ("ordinary'_%名前", "ordinary'_%名前")] {
        let column = TableName::new(name, "parent").column(name);
        let numbered = sql(
            &pg,
            &Change::AlterColumnNumbering {
                uid: "c_aaaaaa".parse().unwrap(),
                column,
                ty: "integer".parse().unwrap(),
                from: pbps_model::ValueSource::Sequence(pbps_model::OwnedSequence::serial()),
                to: pbps_model::ValueSource::None,
            },
        );
        let expected = format!(
            "{shown}.parent.{shown} is not numbered by a sequence it owns, as this plan expected; nothing was applied"
        );
        assert!(
            numbered.contains(&format!(
                "RAISE EXCEPTION USING MESSAGE = E'{}';",
                expected.replace('\\', "\\\\").replace('\'', "''")
            )),
            "{numbered}"
        );
        for allowed in [None, Some(2)] {
            let emitted = sql(&pg, &partition(name, allowed));
            let part = format!("{shown}.part");
            let rest = format!("{shown}.rest");
            let expected = [
                format!(
                    "a publication publishes inserts into every table, or every table of {shown}, so the rows moved into {part} before it is attached would reach subscribers as rows of a table they do not have; nothing was moved"
                ),
                format!(
                    "a publication publishes deletes from {rest}, so moving its rows of the range of {part} would send subscribers their delete and not their insert; nothing was moved"
                ),
                format!(
                    "a statement trigger or rule on {rest} fires on the delete that moves its rows of the range of {part} into it; nothing was moved"
                ),
                format!(
                    "a row trigger on {rest} fires on the delete that would move its rows of the range of {part} into it; nothing was moved"
                ),
                format!(
                    "a table with a foreign key whose action a delete from {rest} fires is under row-level security, so the rows referencing the rows the move would take into {part} cannot all be counted; nothing was moved"
                ),
                format!(
                    "rows reference rows of {rest} the move would take into {part}, and the delete would fire the action of their foreign key; nothing was moved"
                ),
                format!(
                    "moving its rows of the range of {part} out of {rest} would change the value of a generated column, which the new partition computes otherwise (as one of its tableoid does); nothing was moved"
                ),
            ];
            for message in expected {
                assert!(
                    emitted.contains(&format!(
                        "RAISE EXCEPTION USING MESSAGE = '{}';",
                        message.replace('\'', "''")
                    )),
                    "{emitted}"
                );
            }
            let movement = format!(" rows of {rest} are inside the range of {part}");
            let movement = format!(
                "RAISE EXCEPTION USING MESSAGE = n::text || '{}",
                movement.replace('\'', "''")
            );
            assert!(emitted.contains(&movement), "{emitted}");
        }
    }
}

#[test]
fn explain_shows_the_shared_movement_description_exactly_once() {
    use pbps_model::{ChangeSet, IdsFile, PlanBaseline, PlanOrigin, PlannedChange, SavedPlan};
    for name in [HOSTILE, "ordinary'_%名前"] {
        let plan = SavedPlan::new(
            PlanOrigin::Preview,
            "postgres",
            "2026-10-18T00:00:00Z",
            PlanBaseline {
                time_zone: None,
                description: "guard fixture".into(),
                checksum: String::new(),
                database_collation: None,
            },
            ChangeSet {
                changes: vec![PlannedChange::new(partition(name, None))],
            },
            IdsFile {
                tables: [("t_aaaaaa".parse().unwrap(), TableName::new(name, "part"))].into(),
                ..Default::default()
            },
        );
        let dir =
            std::env::temp_dir().join(format!("pbps-raised-messages-{}", rand::random::<u64>()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("plan.json");
        std::fs::write(&path, serde_json::to_vec(&plan).unwrap()).unwrap();
        let explain = |format: &str| {
            let out = std::process::Command::new(env!("CARGO_BIN_EXE_pbps"))
                .current_dir(&dir)
                .args([
                    "explain",
                    "--plan",
                    path.to_str().unwrap(),
                    "--format",
                    format,
                ])
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            String::from_utf8(out.stdout).unwrap()
        };
        let json: serde_json::Value = serde_json::from_str(&explain("json")).unwrap();
        let description = json["data"]["probes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(serde_json::Value::as_str)
            .find(|s| s.contains("no move of them was allowed"))
            .unwrap();
        let text = explain("text");
        std::fs::remove_dir_all(&dir).unwrap();
        // Negative: the shared JSON description retains raw names for its reader.
        assert!(
            description.starts_with(&format!("rows of {name}.rest are ")),
            "{description:?}"
        );
        assert!(
            text.contains(&format!(
                "  - {}\n",
                pbps_model::escape::Escaped(description)
            )),
            "{text:?}"
        );
    }
}
