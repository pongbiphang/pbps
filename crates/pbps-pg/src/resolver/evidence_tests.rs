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
            rule: compatibility::REPORTED_RULE.into(),
            target_environment: "04".repeat(32),
            target_environment_after: "04".repeat(32),
            resolver_environment: "05".repeat(32),
            target_build: "06".repeat(32),
            resolver_build: "07".repeat(32),
            channels: "08".repeat(32),
            target_cluster: "09".repeat(32),
            limitations: Default::default(),
            target_scope: pbps_model::resolver::TargetScope {
                schemas: vec!["app".into()],
                write_path_extras: vec![],
            },
            runtime: ResolverRuntime::Scratch {
                entry: "scratch".into(),
                scratch: ScratchSource::Server {},
                layout: ScratchLayout::Supplied,
                outside_writes: OutsideWrites::Refused,
                same_instance: pbps_model::resolver::SameInstance::NotApplicable,
                dependencies: Default::default(),
            },
        },
        AuthorizationCondition {
            rule: authorization::RULE.into(),
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
    .unwrap()
}

#[test]
fn saved_evidence_requires_a_supported_adapter_major_and_qualification_rule() {
    let supported = evidence();
    validate_evidence(&supported).unwrap();
    let json = serde_json::to_value(&supported).unwrap();
    for (path, value) in [
        // v1 fingerprinted owners and ACLs; its closing manifests cannot be
        // compared under the current property rule.
        (
            "/before/adapter",
            serde_json::json!("postgres-catalog-inputs-v1"),
        ),
        (
            "/before/adapter",
            serde_json::json!("postgres-catalog-inputs-v2"),
        ),
        (
            "/before/adapter",
            serde_json::json!("postgres-catalog-inputs-v4"),
        ),
        ("/before/engine_major", serde_json::json!(17)),
        ("/authorization/rule", serde_json::json!("pg-auth-v1")),
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

/// The compared facts move with the rule's identifier (DEC-2012.1), so
/// evidence naming another one is refused by that name and with the remedy,
/// never read as this build's coverage.
#[test]
fn evidence_sealed_under_another_qualification_rule_is_refused_by_name() {
    assert_eq!(compatibility::REPORTED_RULE, "pg-reported-scope-v2");
    let json = serde_json::to_value(evidence()).unwrap();
    let sealed_under = |rule: &str| -> ResolverEvidence {
        let mut changed = json.clone();
        *changed.pointer_mut("/qualification/rule").unwrap() = serde_json::json!(rule);
        serde_json::from_value(changed).unwrap()
    };
    // The current identifier is read.
    validate_evidence(&sealed_under("pg-reported-scope-v2")).unwrap();
    for refused in [
        // The previous identifier, which compared `setting:xmloption` (#2010):
        // a build that required it must not be read as this one's coverage,
        // nor this one's evidence as theirs.
        "pg-reported-scope-v1",
        // An identifier no build writes yet is not read on the chance that it
        // is a superset.
        "pg-reported-scope-v3",
        // The removed measured profiles' rule: its evidence compared
        // executables the one resolver never observes (#1636).
        "pg-analysis-scope-v1",
        // The other engine's rule is not this adapter's.
        "mssql-reported-scope-v1",
        "mssql-reported-scope-v2",
    ] {
        let error = validate_evidence(&sealed_under(refused)).unwrap_err();
        assert_eq!(error, EvidenceError::Rule(refused.into()), "{refused}");
        let message = error.to_string();
        assert!(message.contains(&format!("`{refused}`")), "{message}");
        assert!(message.contains("plan again"), "{message}");
    }
}

#[test]
fn evidence_requires_the_schema_acl_mode_authorization_rule() {
    assert_eq!(authorization::RULE, "pg-auth-v2");
    let current = evidence();
    validate_evidence(&current).unwrap();
    for rule in ["pg-auth-v1", "pg-auth-v3"] {
        let mut json = serde_json::to_value(&current).unwrap();
        *json.pointer_mut("/authorization/rule").unwrap() = serde_json::json!(rule);
        let old: ResolverEvidence = serde_json::from_value(json).unwrap();
        assert_eq!(validate_evidence(&old), Err(EvidenceError::Version));
    }
}
