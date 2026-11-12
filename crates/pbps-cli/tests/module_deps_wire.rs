//! `depends_on:` reaches the saved plan exactly as it did before the typed
//! annotations were carried through the differ (#2469 C3a).
//!
//! The expected bytes below were produced by this test's own scenario on the
//! tree that equals `origin/master` before any C3a edit, so they pin what
//! master wrote, not what this change writes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_pbps");

struct Project {
    root: PathBuf,
}

impl Project {
    fn new(name: &str, dialect: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("pbps-deps-wire-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("schema")).unwrap();
        std::fs::write(root.join("pbps.yml"), format!("dialect: {dialect}\n")).unwrap();
        let p = Self { root };
        p.git(&["init", "-q"]);
        p.git(&["config", "user.email", "d@e.f"]);
        p.git(&["config", "user.name", "demo"]);
        p
    }

    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
    }

    fn file(&self, name: &str, body: &str) {
        std::fs::write(self.root.join("schema").join(name), body).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .arg("--project")
            .arg(&self.root)
            .args(args)
            .output()
            .unwrap()
    }

    fn plan(&self, out: &Path) -> serde_json::Value {
        let o = self.run(&["plan", "--out", out.to_str().unwrap()]);
        assert_eq!(
            o.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&o.stderr)
        );
        serde_json::from_slice(&std::fs::read(out).unwrap()).unwrap()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn view(name: &str, depends: &[&str]) -> String {
    let on = if depends.is_empty() {
        String::new()
    } else {
        format!("depends_on: [{}]\n", depends.join(", "))
    };
    format!("view: {name}\n{on}definition: |-\n  SELECT id FROM dbo.t\n")
}

fn scenario(dialect: &str) -> serde_json::Value {
    let p = Project::new(dialect, dialect);
    p.file("dbo.t.yml", "table: dbo.t\ncolumns:\n  id: {type: int}\n");
    p.file("dbo.base.yml", &view("dbo.base", &[]));
    p.file("dbo.keep.yml", &view("dbo.keep", &["dbo.base"]));
    p.file("dbo.old.yml", &view("dbo.old", &["dbo.base"]));
    p.git(&["add", "-A"]);
    p.git(&["commit", "-qm", "base"]);
    // `old` is removed (its recorded edge still orders its drop), `keep` stays,
    // and `fresh` is new and waits for `keep`.
    std::fs::remove_file(p.root.join("schema/dbo.old.yml")).unwrap();
    p.file("dbo.fresh.yml", &view("dbo.fresh", &["dbo.keep"]));
    // Neither text names the other, and the name order is the reverse of the
    // dependency, so only the annotation puts `z_last` first.
    p.file("dbo.a_first.yml", &view("dbo.a_first", &["dbo.z_last"]));
    p.file("dbo.z_last.yml", &view("dbo.z_last", &[]));
    let out = p.root.join("plan.json");
    p.plan(&out)
}

/// The parts of a plan that depend on the annotations: the order of the
/// changes and the persisted `object_deps`. Times and checksums are left out
/// because they move with every run.
fn pinned(plan: &serde_json::Value) -> serde_json::Value {
    let changes: Vec<_> = plan["changes"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| serde_json::json!([c["op"], c["id"], c["name"]]))
        .collect();
    serde_json::json!({
        "version": plan["version"],
        "object_deps": plan["object_deps"],
        "changes": changes,
    })
}

const MSSQL: &str = r#"{"version":75,"object_deps":[{"owner":{"root":{"module":"dbo.a_first"},"part":"whole"},"target":{"root":{"module":"dbo.z_last"},"part":"whole"}},{"owner":{"root":{"module":"dbo.fresh"},"part":"whole"},"target":{"root":{"module":"dbo.keep"},"part":"whole"}},{"owner":{"root":{"module":"dbo.keep"},"part":"whole"},"target":{"root":{"module":"dbo.base"},"part":"whole"}}],"changes":[["drop_module","dbo.old",null],["create_table",null,"dbo.t"],["create_module","dbo.z_last",null],["create_module","dbo.a_first",null],["create_module","dbo.fresh",null]]}"#;
const POSTGRES: &str = r#"{"version":75,"object_deps":[{"owner":{"root":{"module":"dbo.a_first"},"part":"whole"},"target":{"root":{"module":"dbo.z_last"},"part":"whole"}},{"owner":{"root":{"module":"dbo.fresh"},"part":"whole"},"target":{"root":{"module":"dbo.keep"},"part":"whole"}},{"owner":{"root":{"module":"dbo.keep"},"part":"whole"},"target":{"root":{"module":"dbo.base"},"part":"whole"}}],"changes":[["drop_module","dbo.old",null],["create_table",null,"dbo.t"],["alter_module","dbo.base",null],["create_module","dbo.z_last",null],["create_module","dbo.a_first",null],["alter_module","dbo.keep",null],["create_module","dbo.fresh",null]]}"#;

fn expected(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap()
}

#[test]
fn a_module_only_plan_keeps_its_order_and_writes_object_deps() {
    assert_eq!(pinned(&scenario("mssql")), expected(MSSQL));
    assert_eq!(pinned(&scenario("postgres")), expected(POSTGRES));
    // Negative: the old key is gone from the plan, not written beside the new.
    assert!(scenario("mssql").get("module_deps").is_none());
}

/// A project with no `depends_on:` writes no `object_deps`: like the other
/// collections of a plan, an empty list is left out (DEC-2469.7).
#[test]
fn a_plan_without_annotations_omits_object_deps() {
    let p = Project::new("none", "mssql");
    p.file("dbo.t.yml", "table: dbo.t\ncolumns:\n  id: {type: int}\n");
    let out = p.root.join("plan.json");
    let plan = p.plan(&out);
    assert!(plan.get("object_deps").is_none(), "{plan}");
    // Negative: a project with an annotation still writes its edges.
    assert!(
        !scenario("mssql")["object_deps"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

/// A dependency that is not declared and a cycle are still refused at the
/// loader's boundary, under the same two finding codes.
#[test]
fn undeclared_and_cyclic_module_dependencies_keep_their_findings() {
    let p = Project::new("findings", "mssql");
    p.file("dbo.t.yml", "table: dbo.t\ncolumns:\n  id: {type: int}\n");
    p.file("dbo.a.yml", &view("dbo.a", &["dbo.ghost"]));
    let o = p.run(&["validate"]);
    let text = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(2), "{text}");
    assert!(
        text.contains("depends on `dbo.ghost`, which is not a declared module"),
        "{text}"
    );
    p.file("dbo.a.yml", &view("dbo.a", &["dbo.b"]));
    p.file("dbo.b.yml", &view("dbo.b", &["dbo.a"]));
    let o = p.run(&["validate"]);
    let text = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(2), "{text}");
    assert!(text.contains("close a cycle"), "{text}");
}
