use super::*;
use pbps_model::resolver::*;
use pbps_model::{ModuleId, TableName};
use std::collections::BTreeSet;

fn evidence() -> ResolverEvidence {
    let changes = pbps_model::ChangeSet::default();
    let manifest = InputManifest::new(
        capture::RULE.into(),
        17,
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
                same_instance: SameInstance::Refused,
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

fn changed(pointer: &str, value: serde_json::Value) -> ResolverEvidence {
    let mut json = serde_json::to_value(evidence()).unwrap();
    *json.pointer_mut(pointer).unwrap() = value;
    serde_json::from_value(json).unwrap()
}

#[test]
fn evidence_sealed_by_this_adapter_is_read() {
    validate_evidence(&evidence()).unwrap();
}

#[test]
fn an_unsupported_adapter_major_or_authorization_rule_is_refused() {
    for (pointer, value) in [
        (
            "/before/adapter",
            serde_json::json!("postgres-catalog-inputs-v5"),
        ),
        ("/before/adapter", serde_json::json!("mssql-capture-v3")),
        ("/after/adapter", serde_json::json!("mssql-capture-v5")),
        ("/before/engine_major", serde_json::json!(16)),
        ("/before/engine_major", serde_json::json!(18)),
        ("/after/engine_major", serde_json::json!(18)),
        ("/authorization/rule", serde_json::json!("pg-auth-v2")),
    ] {
        assert_eq!(
            validate_evidence(&changed(pointer, value.clone())),
            Err(EvidenceError::Version),
            "{pointer} = {value}"
        );
    }
}

#[test]
fn evidence_sealed_under_another_qualification_rule_is_refused_by_name() {
    assert_eq!(compatibility::REPORTED_RULE, "mssql-reported-scope-v2");
    for refused in [
        "mssql-reported-scope-v1",
        "mssql-reported-scope-v3",
        // The other engine's rule is not this adapter's.
        "pg-reported-scope-v2",
    ] {
        let error = validate_evidence(&changed("/qualification/rule", serde_json::json!(refused)))
            .unwrap_err();
        assert_eq!(error, EvidenceError::Rule(refused.into()), "{refused}");
        assert!(error.to_string().contains("plan again"), "{error}");
    }
}

#[test]
fn a_run_that_never_decided_the_same_instance_policy_is_refused() {
    let error = validate_evidence(&changed(
        "/qualification/runtime/same_instance",
        serde_json::json!("not-applicable"),
    ));
    assert_eq!(error, Err(EvidenceError::Incomplete));
    for decided in ["refused", "allowed"] {
        validate_evidence(&changed(
            "/qualification/runtime/same_instance",
            serde_json::json!(decided),
        ))
        .unwrap();
    }
}

fn class(name: &str) -> ObjectIdentity {
    ObjectIdentity {
        class: name.into(),
        name: vec!["app".into(), "x".into()],
        signature: vec![],
    }
}

/// A literal PostgreSQL class would match no SQL Server record, and an empty
/// match reads as a complete sweep; each surface names its catalog or none.
#[test]
fn every_surface_names_a_sql_server_catalog_or_none() {
    let table = TableName::new("app", "t");
    let module: ModuleId = "app.v".parse().unwrap();
    for surface in [
        Surface::Default(table.column("c")),
        Surface::Check {
            table: table.clone(),
            name: "ck".into(),
        },
        Surface::Module(module),
    ] {
        let classes = binding_classes(&surface).expect("a bound surface");
        assert!(classes.iter().all(|c| !c.starts_with("pg_")), "{classes:?}");
        assert!(classes.contains(&capture::class::OBJECT), "{surface:?}");
    }
    // A computed column binds through its column; a DEFAULT through its object.
    let default = binding_classes(&Surface::Default(table.column("c"))).unwrap();
    assert!(default.contains(&capture::class::COLUMN));
    // SQL Server has no expression index, and the other surfaces bind nothing external.
    for surface in [
        Surface::Table(table.clone()),
        Surface::Column(table.column("c")),
    ] {
        assert_eq!(binding_classes(&surface), None, "{surface:?}");
    }
}

#[test]
fn a_dependent_is_found_only_through_a_binding_that_reaches_a_changed_object() {
    let (changed_object, other, dependent) = (
        class("sys.objects"),
        ObjectIdentity {
            name: vec!["app".into(), "other".into()],
            ..class("sys.objects")
        },
        ObjectIdentity {
            name: vec!["app".into(), "v".into()],
            ..class("sys.objects")
        },
    );
    let reached = BTreeSet::from([changed_object.clone()]);
    let binding = |target: &ObjectIdentity| Binding {
        node: "n".into(),
        path: vec![],
        target: target.clone(),
    };
    assert_eq!(
        dependent_object(&dependent, &[binding(&changed_object)], &reached),
        Some(dependent.clone())
    );
    assert_eq!(
        dependent_object(&dependent, &[binding(&other)], &reached),
        None
    );
    assert_eq!(dependent_object(&dependent, &[], &reached), None);
}
