//! A supplied PostgreSQL login is an explicit limitation in the saved plan.

use serde_json::{Value, json};

fn wire() -> Value {
    serde_json::to_value(super::tests::plan()).unwrap()
}

fn readable(value: Value) -> bool {
    serde_json::from_value::<crate::SavedPlan>(value)
        .is_ok_and(|plan| plan.validate_analysis().is_ok())
}

#[test]
fn supplied_login_evidence_records_its_authorization_limitation() {
    let value = wire();
    assert_eq!(value["analysis"]["evidence"]["version"], 8);
    assert_eq!(
        value["analysis"]["evidence"]["qualification"]["authorization"],
        "login_standin"
    );
    assert!(readable(value));
}

#[test]
fn missing_unknown_or_future_authorization_status_is_refused() {
    let original = wire();
    for status in [
        None,
        Some(Value::Null),
        Some(json!("")),
        Some(json!("verified_role")),
        Some(json!("unknown")),
    ] {
        let mut value = original.clone();
        let qualification = value["analysis"]["evidence"]["qualification"]
            .as_object_mut()
            .unwrap();
        match status {
            Some(status) => {
                qualification.insert("authorization".into(), status);
            }
            None => {
                qualification.remove("authorization");
            }
        }
        assert!(!readable(value.clone()), "{value}");
    }
}

#[test]
fn previous_or_future_evidence_cannot_claim_the_new_authorization_contract() {
    for version in [1, 7, 9, u32::MAX] {
        let mut value = wire();
        value["analysis"]["evidence"]["version"] = version.into();
        assert!(!readable(value), "version {version}");
    }
}

#[test]
fn authorization_status_must_agree_with_the_recorded_postgres_layout() {
    let mut supplied = wire();
    supplied["analysis"]["evidence"]["qualification"]["authorization"] = json!("reproduced");
    assert!(!readable(supplied));
    let mut owned = wire();
    owned["analysis"]["evidence"]["qualification"]["runtime"]["layout"] = json!("run-owned");
    owned["analysis"]["evidence"]["qualification"]["authorization"] = json!("reproduced");
    assert!(readable(owned.clone()));
    owned["analysis"]["evidence"]["qualification"]["authorization"] = json!("login_standin");
    assert!(!readable(owned));
}
