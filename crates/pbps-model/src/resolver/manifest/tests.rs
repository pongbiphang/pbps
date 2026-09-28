use super::*;
use serde_json::{Value, json};

fn fixture() -> InputManifest {
    let object = ObjectIdentity {
        class: "routine".into(),
        name: vec!["app".into(), "f".into()],
        signature: vec![],
    };
    let absence = CandidateSet {
        class: "routine".into(),
        namespace: Some("earlier".into()),
        name: Some("f".into()),
    };
    InputManifest::new(
        "fixture-v1".into(),
        18,
        "01".repeat(8),
        ReadScope {
            retained: BTreeSet::from([object.clone()]),
            candidates: BTreeSet::from([absence.clone()]),
        },
        "02".repeat(32),
        "03".repeat(32),
        vec![Prerequisite {
            object,
            ownership: ObjectOwnership::Unqualified,
            canonicalization: "fixture-v1".into(),
            properties: "04".repeat(32),
            bindings: vec![],
        }],
        vec![Membership {
            predicate: absence,
            members: BTreeSet::new(),
        }],
        BTreeSet::new(),
        vec![],
    )
    .unwrap()
}

#[test]
fn every_observation_component_is_required_on_read() {
    let value = serde_json::to_value(fixture()).unwrap();
    for field in value.as_object().unwrap().keys() {
        let mut missing = value.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<InputManifest>(missing).is_err(),
            "{field}"
        );
    }
    assert_eq!(
        serde_json::from_value::<InputManifest>(value).unwrap(),
        fixture()
    );
}

#[test]
fn an_absence_predicate_cannot_be_omitted_or_replaced_by_an_empty_manifest() {
    let mut value = serde_json::to_value(fixture()).unwrap();
    value["membership"] = json!([]);
    assert!(serde_json::from_value::<InputManifest>(value).is_err());
    let mut value = serde_json::to_value(fixture()).unwrap();
    value["prerequisites"] = json!([]);
    assert!(serde_json::from_value::<InputManifest>(value).is_err());
}

#[test]
fn unsupported_versions_unkeyed_digests_and_incomplete_bindings_refuse() {
    for (field, replacement) in [
        ("version", json!(2)),
        ("key_id", json!("")),
        ("session", Value::Null),
        ("baseline", json!("plaintext")),
    ] {
        let mut value = serde_json::to_value(fixture()).unwrap();
        value[field] = replacement;
        assert!(
            serde_json::from_value::<InputManifest>(value).is_err(),
            "{field}"
        );
    }
    let mut value = serde_json::to_value(fixture()).unwrap();
    value["prerequisites"][0]["bindings"] = json!([{
        "node":"CALL", "path":["body"],
        "target":{"class":"routine","name":["missing"],"signature":[]}
    }]);
    assert!(serde_json::from_value::<InputManifest>(value).is_err());
}

#[test]
fn duplicate_records_and_unknown_source_fields_are_not_canonical_evidence() {
    let mut value = serde_json::to_value(fixture()).unwrap();
    let duplicate = value["prerequisites"][0].clone();
    value["prerequisites"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    assert!(serde_json::from_value::<InputManifest>(value).is_err());
    let mut value = serde_json::to_value(fixture()).unwrap();
    value["prerequisites"][0]["definition"] = json!("private source");
    assert!(serde_json::from_value::<InputManifest>(value).is_err());
}

#[test]
fn catalog_ownership_is_required_and_not_inferred_from_an_object_name() {
    let value = serde_json::to_value(fixture()).unwrap();
    let mut missing = value.clone();
    missing["prerequisites"][0]
        .as_object_mut()
        .unwrap()
        .remove("ownership");
    assert!(serde_json::from_value::<InputManifest>(missing).is_err());
    let mut unknown = value;
    unknown["prerequisites"][0]["ownership"] = json!({"kind":"assumed-owner"});
    assert!(serde_json::from_value::<InputManifest>(unknown).is_err());
    assert_eq!(
        fixture().prerequisites()[0].ownership,
        ObjectOwnership::Unqualified
    );
}
