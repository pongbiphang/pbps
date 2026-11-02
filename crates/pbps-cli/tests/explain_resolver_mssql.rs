//! SQL Server resolver evidence is readable and reviewable from the file alone,
//! and no build that cannot enforce it at apply accepts it there (#2143,
//! DEC-2143.3).

use pbps_model::resolver::*;
use pbps_model::{ChangeSet, IdsFile, PlanBaseline, PlanOrigin, SavedPlan};
use std::collections::BTreeSet;
use std::process::{Command, Output};

const PENDING: &str =
    "this build cannot yet enforce SQL Server resolver evidence during apply (#2144)";

/// Synthetic, source-free evidence built through the public sealing
/// constructors, as the reader sees it; not a claim of live qualification.
fn plan(dialect: &str, adapter: &str, major: u32, same_instance: SameInstance) -> SavedPlan {
    let changes = ChangeSet::default();
    let manifest = InputManifest::new(
        adapter.into(),
        major,
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
            rule: pbps_mssql::resolver::compatibility::REPORTED_RULE.into(),
            authorization: AuthorizationReproduction::Reproduced,
            target_environment: "04".repeat(32),
            target_environment_after: "04".repeat(32),
            resolver_environment: "05".repeat(32),
            target_build: "06".repeat(32),
            resolver_build: "07".repeat(32),
            channels: "08".repeat(32),
            target_cluster: "09".repeat(32),
            limitations: Default::default(),
            target_scope: TargetScope {
                schemas: vec!["app".into()],
                write_path_extras: vec![],
            },
            runtime: ResolverRuntime::Scratch {
                entry: "scratch".into(),
                scratch: ScratchSource::Server {},
                layout: ScratchLayout::Supplied,
                outside_writes: OutsideWrites::Refused,
                same_instance,
                dependencies: Default::default(),
            },
        },
        AuthorizationCondition {
            rule: pbps_mssql::resolver::authorization::RULE.into(),
            before: "10".repeat(32),
            after: "10".repeat(32),
            changes: BTreeSet::new(),
            object_grants: Vec::new(),
        },
        manifest.clone(),
        &manifest,
        vec![],
        vec![],
        OrderingProof::new(&changes, BTreeSet::new()).unwrap(),
    )
    .unwrap();
    SavedPlan::new(
        PlanOrigin::Database,
        dialect,
        "fixture",
        PlanBaseline {
            time_zone: None,
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

fn resolved() -> SavedPlan {
    plan(
        "mssql",
        pbps_mssql::resolver::capture::RULE,
        17,
        SameInstance::Refused,
    )
}

struct Artifact(std::path::PathBuf);
impl Artifact {
    fn new(plan: &SavedPlan) -> Self {
        let dir = std::env::temp_dir().join(format!("pbps-explain-2143-{}", rand::random::<u64>()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("plan.json"), serde_json::to_vec(plan).unwrap()).unwrap();
        std::fs::write(dir.join("pbps.yml"), "dialect: mssql\n").unwrap();
        Self(dir)
    }

    fn explain(&self, format: &str) -> Output {
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

#[test]
fn sql_server_evidence_reads_back_but_is_not_applyable() {
    let plan = resolved();
    plan.validate_analysis().unwrap();
    assert_eq!(plan.origin, PlanOrigin::ResolvedDatabase);
    let artifact = Artifact::new(&plan);
    let json = artifact.explain("json");
    assert!(json.status.success(), "{json:?}");
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["data"]["applyable"], false, "{value}");
    assert_eq!(value["data"]["approve_with"], "", "{value}");
    assert_eq!(value["data"]["resolver"]["layout"], "supplied", "{value}");
    let text = artifact.explain("text");
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(text.contains(PENDING), "{text}");
    assert!(text.contains("apply refused by this build"), "{text}");
    // Planning again would seal the same evidence, so no such advice is given.
    for advice in ["pbps plan", "pbps apply", "To approve and run it"] {
        assert!(!text.contains(advice), "{advice}: {text}");
    }
}

/// The first step of `apply`, before it reads a key or opens a connection.
fn admitted(plan: &SavedPlan) -> anyhow::Result<bool> {
    let config =
        pbps_config::Config::parse("dialect: mssql\n", std::path::Path::new("pbps.yml")).unwrap();
    let project = pbps_config::Project {
        root: std::env::temp_dir(),
        config,
    };
    // A bare target has no environment, hence no key: whatever refuses first
    // is the check under test.
    let key = pbps_cli::resolver::sealing::environment_key(&project, None);
    pbps_cli::resolver::apply::Keyed::check(plan, key, "target").map(|keyed| keyed.is_some())
}

#[test]
fn apply_refuses_sql_server_evidence_by_name_before_it_reads_a_key_or_connects() {
    let error = admitted(&resolved()).unwrap_err().to_string();
    assert_eq!(error, PENDING);
}

#[test]
fn the_barrier_leaves_other_plans_to_their_own_checks() {
    // A resolved plan of another dialect is refused for its key, as before.
    let postgres = plan(
        "postgres",
        "postgres-catalog-inputs-v3",
        18,
        SameInstance::NotApplicable,
    );
    let error = admitted(&postgres).unwrap_err().to_string();
    assert!(error.contains("fingerprint key"), "{error}");
    assert!(!error.contains(PENDING), "{error}");
    // An ordinary SQL Server plan has no evidence to enforce.
    let mut ordinary = resolved();
    ordinary.origin = PlanOrigin::Database;
    ordinary.analysis = PlanAnalysis::Ordinary;
    assert!(!admitted(&ordinary).unwrap());
}

#[test]
fn an_ordinary_sql_server_plan_is_still_applyable() {
    let mut plan = resolved();
    plan.origin = PlanOrigin::Database;
    plan.analysis = PlanAnalysis::Ordinary;
    let artifact = Artifact::new(&plan);
    let out = artifact.explain("json");
    assert!(out.status.success(), "{out:?}");
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["data"]["applyable"], true, "{value}");
    assert!(
        value["data"]["approve_with"]
            .as_str()
            .unwrap()
            .starts_with("pbps apply --env"),
        "{value}"
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains(PENDING));
}

#[test]
fn evidence_another_adapter_sealed_is_refused_before_presentation() {
    for (what, plan) in [
        (
            "a PostgreSQL adapter",
            plan(
                "mssql",
                "postgres-catalog-inputs-v3",
                17,
                SameInstance::Refused,
            ),
        ),
        (
            "an unmeasured engine major",
            plan(
                "mssql",
                pbps_mssql::resolver::capture::RULE,
                16,
                SameInstance::Refused,
            ),
        ),
        (
            "a run that never decided the same-instance policy",
            plan(
                "mssql",
                pbps_mssql::resolver::capture::RULE,
                17,
                SameInstance::NotApplicable,
            ),
        ),
    ] {
        let artifact = Artifact::new(&plan);
        for format in ["text", "json"] {
            let out = artifact.explain(format);
            assert_eq!(out.status.code(), Some(1), "{what} {format}: {out:?}");
            // Nothing of the plan is presented: the text report is empty and
            // the envelope carries only the refusal.
            let presented = String::from_utf8_lossy(&out.stdout);
            assert!(!presented.contains("\"applyable\""), "{what}: {presented}");
            assert!(!presented.contains("origin"), "{what}: {presented}");
        }
    }
}
