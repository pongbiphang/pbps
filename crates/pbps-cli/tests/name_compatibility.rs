//! Name spellings captured on master 1c3db445 before #2084 step 1a.
//! Format versions and dependent checksums track the current plan and state formats.
//! Byte comparisons pin field order as well as values and approved checksums.

use pbps_dialect::Dialect;
use pbps_model::{
    Change, ChangeSet, Column, Declared, IdsFile, PlanBaseline, PlanOrigin, PlannedChange,
    SavedPlan, Schema, StateKind, StateSnapshot, Table, TableName, Uid, state_checksum,
};

const NAME: &str = "name\nline\r\u{1b}[31m\u{9b}\\\u{202e}名前";
const SHOWN: &str = r"name\nline\r\u{1b}[31m\u{9b}\\\u{202e}名前";

fn artifacts(text: &str) -> (Schema, IdsFile, SavedPlan, StateSnapshot) {
    let table = TableName::new("app", text);
    let uid = |s: &str| -> Uid { s.parse().unwrap() };
    let kept = format!("{text}1");
    let dropped = format!("{text}2");
    let ids = IdsFile {
        tables: [(uid("t_aaaaaa"), table.clone())].into(),
        columns: [(uid("c_aaaaaa"), table.column(&kept))].into(),
        ..Default::default()
    };
    let planned = |change: Change| {
        let mut p = PlannedChange::new(change);
        p.risks = pbps_pg::Postgres::new().change_risks(&p.change);
        p
    };
    let changes = ChangeSet {
        changes: vec![
            planned(Change::AddColumn {
                uid: uid("c_aaaaaa"),
                table: table.clone(),
                name: kept.clone().into(),
                column: Box::new(Column::new("int".parse().unwrap())),
            }),
            planned(Change::DropColumn {
                uid: uid("c_bbbbbb"),
                column: table.column(dropped),
            }),
            planned(Change::AlterColumnNullability {
                uid: uid("c_aaaaaa"),
                column: table.column(&kept),
                ty: "int".parse().unwrap(),
                to_nullable: false,
                collation: None,
                through_external_partitions: false,
            }),
        ],
    };
    let schema = Schema {
        tables: [(
            table.clone(),
            Table {
                columns: [(kept.into(), Column::new("int".parse().unwrap()).not_null())].into(),
                ..Default::default()
            },
        )]
        .into(),
        ..Default::default()
    };
    let mut plan = SavedPlan::new(
        PlanOrigin::Database,
        "postgres",
        "2026-10-17T00:00:00Z",
        PlanBaseline {
            time_zone: None,
            description: "hostile-name master fixture".into(),
            checksum: state_checksum(&schema, &ids),
            database_collation: None,
        },
        changes,
        ids.clone(),
    );
    plan.data.insert(
        table,
        pbps_model::data::DataScope {
            mode: pbps_model::data::DataMode::Ensure,
            keys: Default::default(),
        },
    );
    let mut state = StateSnapshot::new(StateKind::Apply, schema.clone(), ids.clone(), "fixture");
    state.declared = Declared::from_schema(&schema);
    state.plan_checksum = Some(plan.checksum());
    (schema, ids, plan, state)
}

fn sql(plan: &SavedPlan, dialect: &dyn Dialect) -> String {
    plan.changes
        .changes
        .iter()
        .flat_map(|p| dialect.emit(&p.change, Default::default()).unwrap())
        .map(|s| s.sql)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn hostile_names_keep_master_artifact_bytes_checksums_and_sql() {
    let (schema, ids, mut plan, mut state) = artifacts(NAME);
    // These fixtures pin historical bytes, independently of a new envelope's
    // deliberately advanced version (#1172).
    plan.version = 42;
    state.version = 25;
    state.plan_checksum = Some(plan.checksum());
    let outputs = [
        ("schema.json", serde_json::to_string(&schema).unwrap()),
        ("ids.json", serde_json::to_string(&ids).unwrap()),
        ("plan.json", serde_json::to_string(&plan).unwrap()),
        ("state.json", serde_json::to_string(&state).unwrap()),
        ("state.sha256", state_checksum(&schema, &ids)),
        ("plan.sha256", plan.checksum()),
        ("postgres.sql", sql(&plan, &pbps_pg::Postgres::new())),
        ("mssql.sql", sql(&plan, &pbps_mssql::Mssql)),
    ];
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/name-compatibility");
    for (name, actual) in outputs {
        assert_eq!(
            actual,
            std::fs::read_to_string(root.join(name)).unwrap(),
            "{name}"
        );
    }
    assert_eq!(
        serde_json::from_str::<SavedPlan>(
            &std::fs::read_to_string(root.join("plan.json")).unwrap()
        )
        .unwrap(),
        plan
    );
    assert_eq!(
        StateSnapshot::from_json(&std::fs::read_to_string(root.join("state.json")).unwrap())
            .unwrap(),
        state
    );
    // Negative: changing the actual name still changes the approved bytes.
    let (_, _, mut ordinary, _) = artifacts("ordinary");
    ordinary.version = plan.version;
    assert_ne!(ordinary.checksum(), plan.checksum());
}

#[test]
fn explain_shows_hostile_names_exactly_once_and_keeps_master_json() {
    let (_, _, plan, _) = artifacts(NAME);
    let dir =
        std::env::temp_dir().join(format!("pbps-name-compatibility-{}", rand::random::<u64>()));
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("plan.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
    let mut outputs = Vec::new();
    for format in ["text", "json"] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_pbps"))
            .current_dir(&dir)
            .args(["explain", "--plan", "plan.json", "--format", format])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        outputs.push(String::from_utf8(output.stdout).unwrap());
    }
    std::fs::remove_dir_all(dir).unwrap();
    let text = &outputs[0];
    for expected in [
        format!("\n  app.{SHOWN}\n"),
        format!("    + add column {SHOWN}1 int\n"),
        format!("    - drop column {SHOWN}2  [destructive]\n"),
    ] {
        assert!(
            text.contains(&expected),
            "expected {expected:?} in {text:?}"
        );
    }
    assert!(
        !text.chars().any(|c| c.is_control() && c != '\n'),
        "{text:?}"
    );
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/name-compatibility");
    let mut historical = plan.clone();
    historical.version = 42;
    // Explain must exercise the current applicable format. Only the checksum
    // changes when its version advances; all names and JSON stay pinned.
    let expected = std::fs::read_to_string(root.join("explain.json"))
        .unwrap()
        .replace(&historical.checksum(), &plan.checksum());
    assert_eq!(outputs[1], expected);
}

#[test]
fn a_dialect_refusal_escapes_a_table_name_exactly_once() {
    for dialect in [
        &pbps_pg::Postgres::new() as &dyn Dialect,
        &pbps_mssql::Mssql,
    ] {
        for (part, shown) in [(NAME, SHOWN), ("名前_ünï", "名前_ünï")] {
            let change = Change::CreateTable {
                uid: "t_aaaaaa".parse().unwrap(),
                name: TableName::new("app", part),
                table: Box::new(Table::default()),
                beside_default: None,
                moves_at_most: None,
            };
            let error = dialect.emit(&change, Default::default()).unwrap_err();
            let pbps_dialect::DialectError::Invalid { dialect, .. } = &error else {
                panic!("the empty table must be refused: {error:?}");
            };
            assert_eq!(
                error.to_string(),
                format!("{dialect}: table `app.{shown}` has no columns")
            );
        }
    }
}

#[test]
fn a_policy_finding_keeps_raw_json_and_is_shown_exactly_once() {
    let (schema, _, mut plan, _) = artifacts(NAME);
    let policies = serde_json::from_str(
        r#"{"rules":{"naming.table":{"severity":"warning","pattern":"never"}}}"#,
    )
    .unwrap();
    let context = pbps_policy::Context {
        now: 0,
        max_rows: 1000,
        connected: false,
        only: None,
        deprecated_types: &[],
    };
    let findings = pbps_policy::declarations(&schema, &policies, &context);
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(
        findings[0].subject.as_deref(),
        Some(format!("app.{NAME}").as_str())
    );
    assert_eq!(
        findings[0].message,
        format!("app.{NAME}: table `{NAME}` does not match `never`")
    );
    plan.changes.changes[0].findings = findings;
    let dir = std::env::temp_dir().join(format!("pbps-name-policy-{}", rand::random::<u64>()));
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("plan.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pbps"))
        .current_dir(&dir)
        .args(["explain", "--plan", "plan.json", "--format", "text"])
        .output()
        .unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains(&format!(
            "      warning: naming.table — app.{SHOWN}: table `{SHOWN}` does not match `never`\n"
        )),
        "{text:?}"
    );
}

#[test]
fn table_change_descriptions_keep_raw_json_and_are_shown_once_under_approval() {
    let (_, _, mut plan, _) = artifacts(NAME);
    plan.changes.changes = vec![PlannedChange::new(Change::DropTable {
        uid: "t_aaaaaa".parse().unwrap(),
        name: TableName::new("app", NAME),
        detach_from: None,
    })];
    plan.ids = IdsFile::default();
    plan.data.clear();
    let dir = std::env::temp_dir().join(format!("pbps-name-description-{}", rand::random::<u64>()));
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("plan.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
    let mut outputs = Vec::new();
    for format in ["text", "json"] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_pbps"))
            .current_dir(&dir)
            .args(["explain", "--plan", "plan.json", "--format", format])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        outputs.push(String::from_utf8(out.stdout).unwrap());
    }
    std::fs::remove_dir_all(dir).unwrap();
    assert!(
        outputs[0].contains(&format!("    - drop table app.{SHOWN}  [destructive]\n")),
        "{:?}",
        outputs[0]
    );
    assert!(
        outputs[0].contains(&format!(
            "               app.{SHOWN}  - drop table app.{SHOWN}\n"
        )),
        "{:?}",
        outputs[0]
    );
    let json: serde_json::Value = serde_json::from_str(&outputs[1]).unwrap();
    assert_eq!(
        json["data"]["risks"][0]["changes"][0],
        format!("app.{NAME}  - drop table app.{NAME}")
    );
}
