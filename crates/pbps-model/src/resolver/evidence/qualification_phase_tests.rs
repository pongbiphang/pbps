//! Reader-side phase boundaries for target environment evidence (#1274).
//!
//! A planned schema USAGE grant may change effective visibility between the
//! opening target read and the approved closing state. The two observations
//! therefore need distinct required fingerprints even when a particular plan
//! happens to leave them equal.

use super::*;
use serde_json::Value;

fn resolved_plan() -> crate::SavedPlan {
    super::tests::plan()
}

fn qualification(plan: &crate::SavedPlan) -> &Qualification {
    let PlanAnalysis::Resolved(evidence) = &plan.analysis else {
        panic!("the fixture must carry resolved evidence");
    };
    evidence.qualification()
}

#[test]
fn an_unchanged_resolved_plan_keeps_explicit_opening_and_closing_environment_facts() {
    let plan = resolved_plan();
    let observed = qualification(&plan);
    assert_eq!(
        observed.target_environment, observed.target_environment_after,
        "the fixture has no planned authorization change"
    );
    plan.validate_analysis().unwrap();
    let restored: crate::SavedPlan = serde_json::from_value(serde_json::to_value(&plan).unwrap())
        .expect("a version-two artifact round-trips");
    restored.validate_analysis().unwrap();
}

#[test]
fn ordinary_plans_remain_valid_without_resolver_environment_evidence() {
    let plan = crate::SavedPlan::new(
        crate::PlanOrigin::Database,
        "postgres",
        "fixture",
        crate::PlanBaseline {
            description: "ordinary fixture".into(),
            checksum: "00".repeat(32),
            database_collation: None,
        },
        crate::ChangeSet::default(),
        crate::IdsFile::default(),
    );
    plan.validate_analysis().unwrap();
    let restored: crate::SavedPlan = serde_json::from_value(serde_json::to_value(&plan).unwrap())
        .expect("the ordinary format remains readable");
    assert!(matches!(restored.analysis, PlanAnalysis::Ordinary));
    restored.validate_analysis().unwrap();
}

#[test]
fn a_missing_or_malformed_closing_environment_fingerprint_is_not_opening_evidence() {
    let original = serde_json::to_value(resolved_plan()).unwrap();
    for malformed in [
        None,
        Some(Value::Null),
        Some(Value::String(String::new())),
        Some(Value::String("zz".repeat(32))),
        Some(Value::String("ab".repeat(31))),
    ] {
        let mut edited = original.clone();
        let fields = edited["analysis"]["evidence"]["qualification"]
            .as_object_mut()
            .unwrap();
        match malformed {
            None => {
                fields.remove("target_environment_after");
                assert!(
                    serde_json::from_value::<crate::SavedPlan>(edited).is_err(),
                    "a missing closing field cannot inherit opening facts"
                );
            }
            Some(value) => {
                fields.insert("target_environment_after".into(), value);
                let decoded = serde_json::from_value::<crate::SavedPlan>(edited);
                assert!(
                    decoded.is_err() || decoded.unwrap().validate_analysis().is_err(),
                    "malformed closing facts cannot validate"
                );
            }
        }
    }
}

#[test]
fn version_one_evidence_cannot_be_read_as_phase_aware_evidence() {
    let mut old = serde_json::to_value(resolved_plan()).unwrap();
    old["analysis"]["evidence"]["version"] = Value::from(1);
    let decoded = serde_json::from_value::<crate::SavedPlan>(old);
    assert!(
        decoded.is_err() || decoded.unwrap().validate_analysis().is_err(),
        "the previous single-phase format must refuse instead of defaulting"
    );
}
