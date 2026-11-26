//! A state-file baseline carries the ordering edges of its historical declarations.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use pbps_model::{IdsFile, StateKind, StateSnapshot};

struct Project {
    root: PathBuf,
    dialect: &'static str,
}

impl Project {
    fn new(dialect: &'static str, dependency: bool) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "pbps-state-baseline-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("schema")).unwrap();
        std::fs::write(root.join("pbps.yml"), format!("dialect: {dialect}\n")).unwrap();
        let p = Self { root, dialect };
        p.git(&["init", "-q"]);
        p.git(&["config", "user.name", "Baseline test"]);
        p.git(&["config", "user.email", "baseline@example.com"]);
        p.view("a", dependency.then_some("b"), 1);
        p.view("b", None, 2);
        p.plan(&[]);
        p.git(&["add", "-A"]);
        p.git(&["commit", "-qm", "Baseline declarations"]);
        p
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn view(&self, name: &str, target: Option<&str>, value: u32) {
        let edge = target.map_or(String::new(), |t| format!("depends_on: [app.{t}]\n"));
        // Neither body names the other view, so only the annotation can order them.
        std::fs::write(
            self.root.join(format!("schema/{name}.yml")),
            format!("view: app.{name}\n{edge}definition: SELECT {value} AS n\n"),
        )
        .unwrap();
    }

    fn snapshot(&self) -> serde_json::Value {
        let loaded = pbps_load::load_schema_dir(&self.root.join("schema")).unwrap();
        let mut snap = StateSnapshot::new(
            StateKind::Apply,
            self.dialect,
            loaded.schema,
            IdsFile::default(),
            "baseline test",
        );
        snap.object_deps = loaded.hints.annotations.wire().unwrap();
        // Match the revision baseline's provenance so retirement diagnostics
        // do not hide whether annotations changed the plan.
        snap.git_sha = Some(self.git(&["rev-parse", "HEAD"]).into());
        serde_json::to_value(snap).unwrap()
    }

    fn state(&self, row: &serde_json::Value) -> String {
        let path = self.root.join("baseline.json");
        std::fs::write(&path, serde_json::to_vec(row).unwrap()).unwrap();
        path.to_str().unwrap().to_owned()
    }

    fn remove_views(&self) {
        for name in ["a", "b"] {
            std::fs::remove_file(self.root.join(format!("schema/{name}.yml"))).unwrap();
        }
    }

    fn plan(&self, baseline: &[&str]) -> serde_json::Value {
        let path = self.root.join("plan.json");
        let _ = std::fs::remove_file(&path);
        let mut command = Command::new(env!("CARGO_BIN_EXE_pbps"));
        command.arg("--project").arg(&self.root).arg("plan");
        command.args(baseline);
        let out = command.arg("--out").arg(&path).output().unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn steps(plan: &serde_json::Value) -> Vec<String> {
    plan["changes"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| {
            format!(
                "{} {}",
                change["op"].as_str().unwrap(),
                change["id"].as_str().unwrap()
            )
        })
        .collect()
}

#[test]
fn a_removed_modules_recorded_edge_orders_drops_like_the_git_baseline() {
    let mut orders = Vec::new();
    for dialect in ["postgres", "mssql"] {
        let p = Project::new(dialect, true);
        let state = p.state(&p.snapshot());
        p.remove_views();
        let git = steps(&p.plan(&["--since", "HEAD"]));
        let file = steps(&p.plan(&["--base", &state]));
        eprintln!("{dialect}: git={git:?}, state={file:?}");
        orders.push((dialect, git, file));
    }
    for (dialect, git, file) in orders {
        assert_eq!(git, ["drop_module app.a", "drop_module app.b"]);
        assert_eq!(file, git, "{dialect}");
    }
}

#[test]
fn a_state_without_edges_keeps_the_unannotated_drop_order() {
    for dialect in ["postgres", "mssql"] {
        let p = Project::new(dialect, false);
        let row = p.snapshot();
        assert_eq!(row["object_deps"], serde_json::json!([]));
        let state = p.state(&row);
        p.remove_views();
        let git_plan = p.plan(&["--since", "HEAD"]);
        let file_plan = p.plan(&["--base", &state]);
        assert_eq!(file_plan["changes"], git_plan["changes"], "{dialect}");
        assert_eq!(
            file_plan["object_deps"], git_plan["object_deps"],
            "{dialect}"
        );
        let git = steps(&git_plan);
        let file = steps(&file_plan);
        eprintln!("{dialect}: unannotated git={git:?}, state={file:?}");
        assert_eq!(git, ["drop_module app.b", "drop_module app.a"]);
        assert_eq!(file, git, "{dialect}");
    }
}

#[test]
fn a_kept_modules_declared_edge_replaces_its_recorded_edge() {
    for dialect in ["postgres", "mssql"] {
        let p = Project::new(dialect, true);
        let mut row = p.snapshot();
        let state = p.state(&row);
        // Reverse the historical edge. Keeping both would invent a cycle.
        p.view("a", None, 3);
        p.view("b", Some("a"), 4);
        let recorded = p.plan(&["--base", &state]);
        row["object_deps"] = serde_json::json!([]);
        let state = p.state(&row);
        let plain = p.plan(&["--base", &state]);
        assert!(!plain["changes"]["changes"].as_array().unwrap().is_empty());
        eprintln!(
            "{dialect}: kept recorded={:?}, without recorded={:?}",
            steps(&recorded),
            steps(&plain)
        );
        assert_eq!(recorded["changes"], plain["changes"], "{dialect}");
        assert_eq!(recorded["object_deps"], plain["object_deps"], "{dialect}");
        assert_eq!(plain["object_deps"].as_array().unwrap().len(), 1);
        assert_eq!(steps(&plain), ["alter_module app.a", "alter_module app.b"]);
    }
}

#[test]
fn a_legacy_states_migrated_edges_order_removed_modules() {
    for dialect in ["postgres", "mssql"] {
        let p = Project::new(dialect, true);
        let mut row = p.snapshot();
        row["version"] = (pbps_model::state::OBJECT_DEPS_VERSION - 1).into();
        row.as_object_mut().unwrap().remove("object_deps");
        row["module_deps"] = serde_json::json!({"app.a": ["app.b"]});
        let migrated = StateSnapshot::from_json(&row.to_string()).unwrap();
        assert_eq!(migrated.object_deps.len(), 1);
        let state = p.state(&row);
        p.remove_views();
        let git = steps(&p.plan(&["--since", "HEAD"]));
        let file = steps(&p.plan(&["--base", &state]));
        eprintln!("{dialect}: legacy git={git:?}, state={file:?}");
        assert_eq!(file, git, "{dialect}");
        // An old row without either field already means no recorded edges.
        row.as_object_mut().unwrap().remove("module_deps");
        assert!(
            StateSnapshot::from_json(&row.to_string())
                .unwrap()
                .object_deps
                .is_empty()
        );
        let state = p.state(&row);
        assert_eq!(
            steps(&p.plan(&["--base", &state])),
            ["drop_module app.b", "drop_module app.a"],
            "{dialect}"
        );
    }
}
