use super::*;
use pbps_model::resolver::*;
use std::collections::BTreeSet;

fn evidence() -> ResolverEvidence {
    let changes = pbps_model::ChangeSet::default();
    let manifest = InputManifest::new(
        capture::INPUT_RULE.into(),
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
    ResolverEvidence::new(
        &changes,
        Qualification {
            rule: compatibility::RULE.into(),
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
            rule: authorization::RULE.into(),
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
    .unwrap()
}

#[test]
fn saved_evidence_requires_a_supported_adapter_major_and_qualification_rule() {
    let supported = evidence();
    validate_evidence(&supported).unwrap();
    let json = serde_json::to_value(&supported).unwrap();
    for (path, value) in [
        // v1 fingerprinted owners and ACLs; its closing manifests cannot be
        // compared under the v2 property rule.
        (
            "/before/adapter",
            serde_json::json!("postgres-catalog-inputs-v1"),
        ),
        (
            "/before/adapter",
            serde_json::json!("postgres-catalog-inputs-v3"),
        ),
        ("/before/engine_major", serde_json::json!(17)),
        (
            "/qualification/rule",
            serde_json::json!("pg-analysis-scope-v2"),
        ),
        ("/authorization/rule", serde_json::json!("pg-auth-v2")),
    ] {
        let mut changed = json.clone();
        *changed.pointer_mut(path).unwrap() = value;
        let changed: ResolverEvidence = serde_json::from_value(changed).unwrap();
        assert_eq!(
            validate_evidence(&changed),
            Err(EvidenceError::Version),
            "{path}"
        );
    }
}
