//! Saved resolver artifacts remain reviewable before guarded apply is available.

use pbps_model::resolver::*;
use pbps_model::{ChangeSet, IdsFile, PlanBaseline, PlanOrigin, SavedPlan};
use std::collections::BTreeSet;
use std::process::{Command, Output};

#[path = "support/envelope_archives.rs"]
mod envelope_archives;

fn resolved_plan() -> SavedPlan {
    // Synthetic, source-free reader evidence, not a claim of live qualification.
    // Use the public sealing constructors so an invalid fixture cannot turn an
    // explanation regression into an unrelated deserialization refusal.
    let changes = ChangeSet::default();
    let manifest = InputManifest::new(
        "postgres-catalog-inputs-v1".into(),
        18,
        "01".repeat(8),
        ReadScope {
            retained: BTreeSet::new(),
            candidates: BTreeSet::new(),
        },
        "02".repeat(32),
        "03".repeat(32),
        vec![],
        vec![],
        BTreeSet::new(),
        vec![],
    )
    .unwrap();
    let evidence = ResolverEvidence::new(
        &changes,
        Qualification {
            rule: pbps_pg::resolver::compatibility::RULE.into(),
            target_environment: "04".repeat(32),
            target_environment_after: "04".repeat(32),
            resolver_environment: "05".repeat(32),
            target_build: "06".repeat(32),
            resolver_build: "07".repeat(32),
            channels: "08".repeat(32),
            runtime: ResolverRuntime::Supplied {
                profile: "linux-dedicated-v1".into(),
                identity: "09".repeat(32),
            },
        },
        AuthorizationCondition {
            rule: pbps_pg::resolver::authorization::RULE.into(),
            before: "10".repeat(32),
            after: "10".repeat(32),
            changes: BTreeSet::new(),
        },
        manifest.clone(),
        &manifest,
        vec![],
        vec![],
        OrderingProof::new(&changes, BTreeSet::new()).unwrap(),
    )
    .unwrap();
    pbps_pg::resolver::validate_evidence(&evidence).unwrap();
    pbps_cli::resolver::sealing::validate_runtime(&evidence.qualification().runtime).unwrap();
    SavedPlan::new(
        PlanOrigin::Database,
        "postgres",
        "fixture",
        PlanBaseline {
            description: "target database".into(),
            checksum: "00".repeat(32),
            database_collation: None,
        },
        changes,
        IdsFile::default(),
    )
    .with_resolution(evidence)
    .unwrap()
}

struct Artifact(std::path::PathBuf);
impl Artifact {
    fn new(plan: &serde_json::Value) -> Self {
        let dir = std::env::temp_dir().join(format!("pbps-explain-1217-{}", rand::random::<u64>()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("plan.json"), serde_json::to_vec(plan).unwrap()).unwrap();
        Self(dir)
    }

    fn explain(&self, format: &str) -> Output {
        // No project, key, target, resolver selection or credentials: the file
        // is enough, including for a Linux artifact reviewed on Windows.
        Command::new(env!("CARGO_BIN_EXE_pbps"))
            .current_dir(&self.0)
            .arg("--project")
            .arg(&self.0)
            .args(["explain", "--plan", "plan.json", "--format", format])
            .output()
            .unwrap()
    }
}
impl Drop for Artifact {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn report(out: &Output) -> serde_json::Value {
    let value = serde_json::from_slice(&out.stdout).unwrap();
    envelope_archives::assert_accepted_by_archives(&value, "resolver explanation");
    pbps_ui::contract::parse("explain", &out.stdout, out.status.code().unwrap())
        .expect("the viewer must accept the CLI's actual envelope");
    value
}

#[test]
fn unsupported_resolver_apply_is_not_offered_as_an_approval_or_preview() {
    let plan = resolved_plan();
    plan.validate_analysis().unwrap();
    assert_eq!(plan.origin, PlanOrigin::ResolvedDatabase);
    assert!(
        plan.origin.is_applyable(),
        "database provenance is preserved"
    );
    let artifact = Artifact::new(&serde_json::to_value(&plan).unwrap());
    for format in ["text", "json"] {
        let out = artifact.explain(format);
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{text} {:?}", out.stderr);
        assert!(out.stderr.is_empty(), "{:?}", out.stderr);
        assert!(
            text.contains("cannot yet enforce resolver pre/postconditions"),
            "{text}"
        );
        for forbidden in [
            "To approve and run it",
            "pbps apply",
            "pbps plan",
            "offline",
            "plan.preview",
        ] {
            assert!(!text.contains(forbidden), "{forbidden}: {text}");
        }
        if format == "json" {
            let value = report(&out);
            assert_eq!(value["data"]["applyable"], false, "{value}");
            assert_eq!(value["data"]["approve_with"], "", "{value}");
            assert_eq!(value["data"]["checksum"], plan.checksum());
            assert!(value["data"]["apply_limitation"].is_string(), "{value}");
            assert_eq!(value["findings"][0]["id"], "plan.apply-unsupported");
            assert!(value["data"].get("target").is_none(), "{value}");
        } else {
            assert!(
                text.contains("computed against the target with sealed resolver evidence"),
                "{text}"
            );
        }
    }
}

#[test]
fn ordinary_database_approval_and_preview_remedy_keep_their_meaning() {
    for origin in [PlanOrigin::Database, PlanOrigin::Preview] {
        let mut plan = resolved_plan();
        plan.origin = origin;
        plan.analysis = PlanAnalysis::Ordinary;
        let artifact = Artifact::new(&serde_json::to_value(&plan).unwrap());
        for format in ["text", "json"] {
            let out = artifact.explain(format);
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success(), "{text} {:?}", out.stderr);
            assert!(!text.contains("cannot yet enforce"), "{text}");
            if format == "json" {
                let value = report(&out);
                assert_eq!(value["data"]["applyable"], origin == PlanOrigin::Database);
                assert!(value["data"].get("apply_limitation").is_none(), "{value}");
            }
            if origin == PlanOrigin::Database {
                assert!(text.contains("pbps apply --env"), "{text}");
                assert!(text.contains(&plan.checksum()), "{text}");
                assert!(!text.contains("pbps plan"), "{text}");
            } else {
                assert!(text.contains("pbps plan --env"), "{text}");
                assert!(text.contains("offline"), "{text}");
                assert!(!text.contains("pbps apply"), "{text}");
            }
        }
    }
}

#[test]
fn malformed_or_unsupported_resolver_evidence_is_refused_before_presentation() {
    let valid = serde_json::to_value(resolved_plan()).unwrap();
    for (path, replacement) in [
        ("/analysis", serde_json::json!({"kind": "ordinary"})),
        ("/analysis/evidence/version", serde_json::json!(99)),
        (
            "/analysis/evidence/before/adapter",
            serde_json::json!("postgres-catalog-inputs-v99"),
        ),
        (
            "/analysis/evidence/qualification/runtime/profile",
            serde_json::json!("linux-dedicated-v99"),
        ),
    ] {
        let mut value = valid.clone();
        *value.pointer_mut(path).unwrap() = replacement;
        let artifact = Artifact::new(&value);
        for format in ["text", "json"] {
            let out = artifact.explain(format);
            assert_eq!(out.status.code(), Some(1), "{path}: {out:?}");
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(!text.contains("pbps apply"), "{text}");
            if format == "json" {
                let value = report(&out);
                assert_eq!(value["result"], "unanswerable", "{value}");
                assert!(
                    value.get("data").is_none_or(serde_json::Value::is_null),
                    "{value}"
                );
            } else {
                assert!(text.is_empty(), "{text}");
            }
        }
    }
}

#[test]
fn current_resolver_artifacts_cannot_bypass_required_evidence_on_apply() {
    let plan = resolved_plan();
    let valid = serde_json::to_value(&plan).unwrap();
    let mut missing = valid.clone();
    missing.as_object_mut().unwrap().remove("analysis");
    let mut cases = vec![missing];
    for (path, replacement) in [
        ("/analysis", serde_json::json!({"kind": "ordinary"})),
        ("/analysis/evidence/version", serde_json::json!(99)),
        (
            "/analysis/evidence/before/adapter",
            serde_json::json!("postgres-catalog-inputs-v99"),
        ),
        (
            "/analysis/evidence/qualification/runtime/profile",
            serde_json::json!("linux-dedicated-v99"),
        ),
    ] {
        let mut value = valid.clone();
        *value.pointer_mut(path).unwrap() = replacement;
        cases.push(value);
    }
    for value in cases {
        // If the wire shape is readable, supply its own checksum so a checksum
        // refusal cannot conceal a missing evidence/provenance validation.
        let checksum = serde_json::from_value::<SavedPlan>(value.clone())
            .map(|p| p.checksum())
            .unwrap_or_else(|_| plan.checksum());
        let artifact = Artifact::new(&value);
        std::fs::write(artifact.0.join("pbps.yml"), "dialect: postgres\n").unwrap();
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let connection = format!(
            "postgres://unused@{}/unused?sslmode=disable",
            target.local_addr().unwrap()
        );
        let out = Command::new(env!("CARGO_BIN_EXE_pbps"))
            .current_dir(&artifact.0)
            .args([
                "apply",
                "--plan",
                "plan.json",
                "--db",
                &connection,
                "--checksum",
                &checksum,
            ])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(
            !error.contains("no longer matches the artifact approved"),
            "{error}"
        );
        assert!(
            !error.contains("cannot yet enforce resolver pre/postconditions"),
            "invalid evidence must fail validation before the capability refusal: {error}"
        );
        assert!(!error.is_empty(), "{out:?}");
        assert_eq!(
            target.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[test]
fn unsupported_resolver_apply_still_refuses_before_contacting_the_target() {
    let plan = resolved_plan();
    let artifact = Artifact::new(&serde_json::to_value(&plan).unwrap());
    std::fs::write(artifact.0.join("pbps.yml"), "dialect: postgres\n").unwrap();
    let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let connection = format!(
        "postgres://unused@{}/unused?sslmode=disable",
        target.local_addr().unwrap()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_pbps"))
        .current_dir(&artifact.0)
        .args([
            "apply",
            "--plan",
            "plan.json",
            "--db",
            &connection,
            "--checksum",
            &plan.checksum(),
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("cannot yet enforce resolver pre/postconditions"),
        "{out:?}"
    );
    assert_eq!(
        target.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn the_viewer_distinguishes_unsupported_database_plans_from_offline_previews() {
    use std::io::Write;
    use std::process::Stdio;
    let mut cases = vec![];
    for (origin, title) in [
        (PlanOrigin::ResolvedDatabase, "Unsupported deployment plan"),
        (PlanOrigin::Database, "Deployment plan"),
        (PlanOrigin::Preview, "Preview plan"),
    ] {
        let mut plan = resolved_plan();
        if origin != PlanOrigin::ResolvedDatabase {
            plan.origin = origin;
            plan.analysis = PlanAnalysis::Ordinary;
        }
        let artifact = Artifact::new(&serde_json::to_value(plan).unwrap());
        let out = artifact.explain("json");
        assert!(out.status.success(), "{out:?}");
        let served = pbps_ui::contract::parse("explain", &out.stdout, 0).unwrap();
        cases.push(
            serde_json::json!({"title": title, "response": String::from_utf8(served).unwrap()}),
        );
    }
    let node = std::env::var_os("PBPS_TEST_NODE").unwrap_or_else(|| "node".into());
    let mut child = Command::new(node)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "const CASES = {}; const RESPONSES = {{status: JSON.stringify({{data: [], findings: []}})}};", serde_json::to_string(&cases).unwrap()).unwrap();
    stdin
        .write_all(include_bytes!("../../pbps-ui/tests/app-browser.cjs"))
        .unwrap();
    stdin
        .write_all(include_bytes!("../../pbps-ui/assets/app.js"))
        .unwrap();
    stdin.write_all(br#"
const settle = () => new Promise(resolve => setImmediate(resolve));
(async () => {
  await settle();
  nav.find(button => button.dataset.view === "plan").fire("click");
  elements["selection-value"].value = "plan.json";
  for (const item of CASES) {
    RESPONSES.plan = item.response;
    elements.selection.fire("submit");
    await settle();
    const titles = [...elements.content.walk()].filter(e => e.tagName === "h2").map(e => e.textContent);
    assert(titles.includes(item.title), JSON.stringify(titles));
    for (const other of CASES.filter(other => other.title !== item.title)) assert(!titles.includes(other.title));
  }
})().catch(error => { console.error(error); process.exit(1); });
"#).unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
