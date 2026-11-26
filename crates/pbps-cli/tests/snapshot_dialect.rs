//! A state snapshot records its engine dialect, and a snapshot-file reader refuses another engine's.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use pbps_model::{IdsFile, StateKind, StateSnapshot};

struct TempProject {
    root: PathBuf,
    dialect: &'static str,
}

impl TempProject {
    fn new(dialect: &'static str, table_file: &str, table_yaml: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "pbps-snapshot-dialect-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("schema")).unwrap();
        std::fs::write(root.join("pbps.yml"), format!("dialect: {dialect}\n")).unwrap();
        std::fs::write(root.join("schema").join(table_file), table_yaml).unwrap();
        Self { root, dialect }
    }

    fn snapshot(&self) -> StateSnapshot {
        let loaded = pbps_load::load_schema_dir(&self.root.join("schema")).unwrap();
        StateSnapshot::new(
            StateKind::Apply,
            self.dialect,
            loaded.schema,
            IdsFile::default(),
            "snapshot-dialect-test",
        )
    }

    fn write_snapshot(&self, name: &str, value: &serde_json::Value) -> PathBuf {
        let path = self.root.join(name);
        std::fs::write(&path, serde_json::to_string_pretty(value).unwrap()).unwrap();
        path
    }

    fn run_plan(&self, base: &Path, extra: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pbps"))
            .arg("--project")
            .arg(&self.root)
            .arg("plan")
            .arg("--base")
            .arg(base)
            .args(extra)
            .output()
            .unwrap()
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn mssql_project() -> TempProject {
    TempProject::new(
        "mssql",
        "dbo.accounts.yml",
        "table: dbo.accounts\ncolumns:\n  id: { type: int, nullable: false }\nprimary_key: [id]\n",
    )
}

fn postgres_project() -> TempProject {
    TempProject::new(
        "postgres",
        "app.accounts.yml",
        "table: app.accounts\ncolumns:\n  id: { type: integer, nullable: false }\nprimary_key: [id]\n",
    )
}

#[test]
fn a_snapshot_records_each_server_dialect_and_round_trips() {
    for project in [mssql_project(), postgres_project()] {
        let snap = project.snapshot();
        assert_eq!(snap.dialect.as_deref(), Some(project.dialect));
        let json = serde_json::to_string(&snap).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["dialect"], serde_json::json!(project.dialect));
        let back = StateSnapshot::from_json(&json).unwrap();
        assert_eq!(back, snap);
    }
}

#[test]
fn plan_base_refuses_a_snapshot_from_another_engine_without_writing_anything() {
    for (source, target) in [
        (mssql_project(), postgres_project()),
        (postgres_project(), mssql_project()),
    ] {
        let mut snap = source.snapshot();
        // Give the foreign snapshot an inconsistent identity mapping as well:
        // the dialect refusal must fire before identity validation or comparison.
        snap.ids
            .tables
            .insert("t_aaaaaa".parse().unwrap(), "dbo.ghost".parse().unwrap());
        let base_path =
            source.write_snapshot("foreign.state.json", &serde_json::to_value(&snap).unwrap());
        let plan_path = target.root.join("plan.json");
        let sql_path = target.root.join("plan.sql");
        let ids_dir = target.root.join(".pbps/ids");

        let out = target.run_plan(
            &base_path,
            &[
                "--out",
                plan_path.to_str().unwrap(),
                "--sql",
                sql_path.to_str().unwrap(),
            ],
        );
        assert!(
            !out.status.success(),
            "expected refusal for {} snapshot in {} project; stdout={}",
            source.dialect,
            target.dialect,
            String::from_utf8_lossy(&out.stdout)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        let expected = format!(
            "`{}` was recorded for {} and this project is {}",
            base_path.display(),
            source.dialect,
            target.dialect
        );
        assert!(
            stderr.contains(&expected),
            "expected `{expected}` in stderr:\n{stderr}"
        );
        assert!(
            !stderr.contains("identity mapping is inconsistent"),
            "dialect refusal must precede identity validation:\n{stderr}"
        );
        assert!(!plan_path.exists(), "must not write --out plan on refusal");
        assert!(!sql_path.exists(), "must not write --sql on refusal");
        assert!(
            !ids_dir.exists(),
            "must not write identity files on refusal"
        );

        // JSON envelope mode reports unanswerable baseline naming both dialects.
        let json_out = target.run_plan(
            &base_path,
            &["--format", "json", "--out", plan_path.to_str().unwrap()],
        );
        assert!(!json_out.status.success());
        let envelope: serde_json::Value =
            serde_json::from_slice(&json_out.stdout).expect("valid JSON envelope");
        assert_eq!(envelope["result"], serde_json::json!("unanswerable"));
        let findings = envelope["findings"].as_array().unwrap();
        assert!(
            findings.iter().any(|f| {
                f["id"] == "baseline.unreadable"
                    && f["message"].as_str().is_some_and(|m| m.contains(&expected))
            }),
            "expected baseline.unreadable finding naming both dialects: {envelope}"
        );
        assert!(!plan_path.exists(), "must not write plan in JSON mode");
    }
}

#[test]
fn plan_base_accepts_a_snapshot_with_no_dialect_as_before() {
    for project in [mssql_project(), postgres_project()] {
        // Negative case 1: a current-version snapshot without a `dialect` key.
        let mut current = serde_json::to_value(project.snapshot()).unwrap();
        current.as_object_mut().unwrap().shift_remove("dialect");
        let current_path = project.write_snapshot("no-dialect.state.json", &current);
        let plan_path = project.root.join("plan-current.json");
        let out = project.run_plan(&current_path, &["--out", plan_path.to_str().unwrap()]);
        assert!(
            out.status.success(),
            "snapshot without dialect must be accepted ({}): {}",
            project.dialect,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(plan_path.exists());

        // Negative case 2: an older-version snapshot (predating both fingerprint and dialect).
        let older = project
            .snapshot()
            .as_written_at(pbps_model::state::CURRENT_VERSION - 1);
        let older_value = serde_json::to_value(&older).unwrap();
        assert!(older_value.get("dialect").is_none());
        let older_path = project.write_snapshot("older.state.json", &older_value);
        let older_plan_path = project.root.join("plan-older.json");
        let out = project.run_plan(&older_path, &["--out", older_plan_path.to_str().unwrap()]);
        assert!(
            out.status.success(),
            "older snapshot without dialect must be accepted ({}): {}",
            project.dialect,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(older_plan_path.exists());
    }
}

#[test]
fn plan_base_with_a_matching_dialect_snapshot_plans_as_today() {
    for project in [mssql_project(), postgres_project()] {
        let with_dialect = serde_json::to_value(project.snapshot()).unwrap();
        assert_eq!(with_dialect["dialect"], serde_json::json!(project.dialect));
        let mut without_dialect = with_dialect.clone();
        without_dialect
            .as_object_mut()
            .unwrap()
            .shift_remove("dialect");

        let with_path = project.write_snapshot("with.state.json", &with_dialect);
        let without_path = project.write_snapshot("without.state.json", &without_dialect);
        let plan_with_path = project.root.join("plan-with.json");
        let plan_without_path = project.root.join("plan-without.json");

        let out_without = project.run_plan(
            &without_path,
            &["--out", plan_without_path.to_str().unwrap()],
        );
        assert!(
            out_without.status.success(),
            "{}",
            String::from_utf8_lossy(&out_without.stderr)
        );
        let out_with = project.run_plan(&with_path, &["--out", plan_with_path.to_str().unwrap()]);
        assert!(
            out_with.status.success(),
            "{}",
            String::from_utf8_lossy(&out_with.stderr)
        );

        let plan_without: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&plan_without_path).unwrap()).unwrap();
        let plan_with: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&plan_with_path).unwrap()).unwrap();
        assert_eq!(plan_with["changes"], plan_without["changes"]);
        assert_eq!(plan_with["dialect"], serde_json::json!(project.dialect));
    }
}

#[test]
fn plan_base_escapes_a_hostile_dialect_name_in_its_refusal() {
    let target = postgres_project();
    let mut snap = target.snapshot();
    snap.dialect = Some("mssql\n\u{1b}[31m".to_owned());
    let base_path =
        target.write_snapshot("hostile.state.json", &serde_json::to_value(&snap).unwrap());
    let plan_path = target.root.join("plan.json");

    let out = target.run_plan(&base_path, &["--out", plan_path.to_str().unwrap()]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("was recorded for") && stderr.contains("and this project is postgres"),
        "{stderr}"
    );
    assert!(
        !stderr.contains('\u{1b}'),
        "control characters must be escaped: {stderr:?}"
    );
    assert!(!plan_path.exists());
}
