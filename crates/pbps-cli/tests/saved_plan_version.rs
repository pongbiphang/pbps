//! Version diagnostics must not weaken current saved-plan validation.

use pbps_model::{ChangeSet, IdsFile, PlanBaseline, PlanOrigin, SavedPlan};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};

// The pre-analysis v11 wire shape, rather than a current plan with only its
// number changed. It deliberately has no analysis member. This is the
// write_plan fixture from master immediately before #614 introduced v12.
const V11: &str = include_str!("fixtures/plan-v11.json");

struct Artifact(PathBuf);
impl Artifact {
    fn new(raw: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("pbps-plan-version-{}", rand::random::<u64>()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("plan.json"), raw).unwrap();
        std::fs::write(dir.join("pbps.yml"), "dialect: mssql\n").unwrap();
        Self(dir)
    }

    fn run(&self, command: &str, checksum: &str) -> Output {
        // A listening endpoint proves refusal happened before target contact.
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pbps"));
        cmd.current_dir(&self.0).arg("--no-input");
        match command {
            "apply" => {
                cmd.args(["apply", "--plan", "plan.json", "--checksum", checksum, "--db"])
                    .arg(format!("Server=127.0.0.1,{};Database=unused;User Id=u;Password=p;TrustServerCertificate=true", target.local_addr().unwrap().port()));
            }
            format => {
                cmd.args(["explain", "--plan", "plan.json", "--format", format]);
            }
        }
        let out = cmd.output().unwrap();
        assert_eq!(
            target.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        out
    }
}
impl Drop for Artifact {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn refused(out: &Output) -> String {
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("pbps apply"), "{stdout}");
    if !stdout.is_empty() {
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["result"], "unanswerable", "{report}");
        assert!(
            report.get("data").is_none_or(serde_json::Value::is_null),
            "{report}"
        );
    }
    format!("{stdout}{}", String::from_utf8_lossy(&out.stderr))
}

#[test]
fn old_wire_shape_names_the_version_and_replan_remedy_before_any_target_contact() {
    let fixture: serde_json::Value = serde_json::from_str(V11).unwrap();
    assert_eq!(fixture["version"], 11);
    assert!(fixture.get("analysis").is_none());
    for command in ["text", "json", "apply"] {
        let out = Artifact::new(V11).run(command, &"00".repeat(32));
        let diagnostic = refused(&out);
        assert!(
            diagnostic.contains("version 11 plan"),
            "{command}: {diagnostic}"
        );
        assert!(
            diagnostic.contains(&format!(
                "understands version {}",
                pbps_model::plan::CURRENT_VERSION
            )),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("pbps plan --db"), "{diagnostic}");
        assert!(!diagnostic.contains("not a pbps plan"), "{diagnostic}");
    }
}

#[test]
fn future_wire_changes_still_report_the_unsupported_version() {
    let raw = V11
        .replace("\"version\": 11", "\"version\": 999")
        .replace("\"origin\": \"database\"", "\"origin\": \"future-origin\"");
    for command in ["text", "json", "apply"] {
        let diagnostic = refused(&Artifact::new(&raw).run(command, &"00".repeat(32)));
        assert!(diagnostic.contains("version 999 plan"), "{diagnostic}");
        assert!(diagnostic.contains("pbps plan --db"), "{diagnostic}");
    }
}

#[test]
fn malformed_versions_and_missing_current_analysis_remain_invalid_artifacts() {
    let mut cases = vec!["{".to_owned(), V11.replace("  \"version\": 11,\n", "")];
    for value in ["null", "\"11\"", "-1", "11.5", "4294967296"] {
        cases.push(V11.replace("\"version\": 11", &format!("\"version\": {value}")));
    }
    cases.push(V11.replace("\"version\": 11", "\"version\": 11, \"version\": 999"));
    cases.push(V11.replace(
        "\"version\": 11",
        &format!("\"version\": {}", pbps_model::plan::CURRENT_VERSION),
    ));
    cases.push(format!("{V11} trailing"));
    for raw in cases {
        for command in ["text", "json", "apply"] {
            let diagnostic = refused(&Artifact::new(&raw).run(command, &"00".repeat(32)));
            assert!(
                diagnostic.contains("not a pbps plan"),
                "{command}: {diagnostic}"
            );
            assert!(!diagnostic.contains("understands version"), "{diagnostic}");
        }
    }
}

#[test]
fn ordinary_current_artifacts_keep_their_checksum_and_approval_gate() {
    let plan = SavedPlan::new(
        PlanOrigin::Database,
        "mssql",
        "fixture",
        PlanBaseline {
            description: "fixture".into(),
            checksum: "00".repeat(32),
            database_collation: None,
        },
        ChangeSet::default(),
        IdsFile::default(),
    );
    let checksum = plan.checksum();
    let artifact = Artifact::new(&serde_json::to_string_pretty(&plan).unwrap());
    let out = artifact.run("json", &checksum);
    assert!(out.status.success(), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["data"]["checksum"], checksum);
    assert_eq!(report["data"]["applyable"], true);
    let out = artifact.run("apply", &checksum);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("nothing to apply"));
    let diagnostic = refused(&artifact.run("apply", &"ff".repeat(32)));
    assert!(
        diagnostic.contains("no longer matches the artifact approved"),
        "{diagnostic}"
    );
}
