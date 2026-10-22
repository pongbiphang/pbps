use super::{ArtifactView, PlanView, admit, seal, singleton};
use pbps_dialect::MinimalDialect;
use pbps_model::{
    Change, ChangeSet, IdsFile, PlanBaseline, PlanMode, PlanOrigin, PlannedChange, SavedPlan,
};
use std::path::Path;

fn wire() -> SavedPlan {
    SavedPlan::new(
        PlanOrigin::Database,
        "minimal",
        "2026-10-22T00:00:00Z",
        PlanBaseline {
            description: "target".into(),
            checksum: "baseline".into(),
            database_collation: None,
            time_zone: None,
        },
        ChangeSet {
            changes: vec![PlannedChange::new(Change::CreateRole {
                uid: "r_aaaaaa".parse().unwrap(),
                name: "reader".into(),
            })],
        },
        IdsFile::default(),
    )
}

fn refusal(plan: SavedPlan, approved: &str) -> String {
    admit(plan, &MinimalDialect, Path::new("plan.json"), approved)
        .unwrap_err()
        .to_string()
}

#[test]
fn admission_accepts_a_matching_plan_without_changing_its_order_or_checksum() {
    for mode in [PlanMode::Transactional, PlanMode::Staged] {
        let mut plan = wire();
        plan.mode = mode;
        let checksum = plan.wire_fingerprint();
        let admitted = admit(
            plan.clone(),
            &MinimalDialect,
            Path::new("plan.json"),
            &checksum,
        )
        .unwrap();
        let executable: &dyn PlanView<MinimalDialect> = &admitted;
        assert_eq!(executable.mode(), mode);
        assert_eq!(admitted.wire(), &plan);
        assert_eq!(admitted.checksum(), checksum);
    }
}

#[test]
fn admission_refuses_an_unapproved_checksum_before_preview_or_dialect_checks() {
    let mut plan = wire();
    let checksum = plan.wire_fingerprint();
    let error = refusal(plan.clone(), "approved");
    assert!(error.contains("approved checksum: approved"));
    assert!(error.contains(&format!("plan checksum now: {checksum}")));
    plan.origin = PlanOrigin::Preview;
    plan.dialect = "another".into();
    let checksum = plan.wire_fingerprint();
    assert_eq!(
        refusal(plan, "approved"),
        format!(
            "`plan.json` no longer matches the artifact approved at the deployment gate.\n\
             approved checksum: approved\n\
             plan checksum now: {checksum}\n\
             Do not approve the new value implicitly: retrieve the reviewed artifact or run \
             `pbps explain --plan ...` and send it through approval again."
        )
    );
}

#[test]
fn admission_refuses_a_preview_before_dialect_or_payload_checks() {
    let mut plan = wire();
    plan.origin = PlanOrigin::Preview;
    let expected = "`plan.json` is an offline preview, not an applyable plan.\n\
                    Recompute it against the target with `pbps plan --db ... --out plan.json`.";
    assert_eq!(refusal(plan.clone(), &plan.wire_fingerprint()), expected);
    plan.dialect = "another".into();
    plan.mode = PlanMode::Staged;
    plan.changes.changes.clear();
    let checksum = plan.wire_fingerprint();
    assert_eq!(refusal(plan, &checksum), expected);
}

#[test]
fn admission_refuses_another_dialect_before_payload_checks_and_escapes_its_name() {
    let mut plan = wire();
    plan.dialect = "another\nengine".into();
    let expected = "`plan.json` was computed for another\\nengine and this project is minimal";
    assert_eq!(refusal(plan.clone(), &plan.wire_fingerprint()), expected);
    plan.mode = PlanMode::Staged;
    plan.changes.changes.clear();
    let checksum = plan.wire_fingerprint();
    assert_eq!(refusal(plan, &checksum), expected);
}

#[test]
fn admission_refuses_an_old_version_before_checksum_checks() {
    let mut plan = wire();
    plan.version = 0;
    assert_eq!(
        refusal(plan, "unapproved"),
        format!(
            "`plan.json` is a version 0 plan and this tool understands version {}.\n\
             Recompute it with `pbps plan --db ... --out ...` and review the new artifact for approval.",
            pbps_model::plan::CURRENT_VERSION
        )
    );
}

#[test]
fn admission_still_refuses_invalid_payloads_after_the_outer_checks_pass() {
    let mut plan = wire().staged();
    plan.changes.changes.clear();
    let checksum = plan.wire_fingerprint();
    assert!(
        refusal(plan, &checksum)
            .contains("a staged plan must contain exactly one logical change; this artifact has 0")
    );
}

#[test]
fn sealing_refuses_a_mode_mismatch_in_both_directions_by_name() {
    for mode in [PlanMode::Transactional, PlanMode::Staged] {
        let mut plan = wire();
        let ordered = singleton(plan.changes.changes[0].clone(), mode, &MinimalDialect);
        plan.mode = match mode {
            PlanMode::Transactional => PlanMode::Staged,
            PlanMode::Staged => PlanMode::Transactional,
        };
        let expected = format!(
            "the saved plan mode {} does not match the ordered plan mode {mode}",
            plan.mode,
        );
        assert_eq!(seal(ordered, plan).unwrap_err().to_string(), expected);
    }
}

#[test]
fn sealing_still_refuses_payload_edits_with_a_matching_mode() {
    let mut plan = wire();
    let ordered = singleton(plan.changes.changes[0].clone(), plan.mode, &MinimalDialect);
    plan.changes.changes.clear();
    assert_eq!(
        seal(ordered, plan).unwrap_err().to_string(),
        "the resolver ordering proof does not describe this change sequence"
    );
}

#[test]
fn sealing_refuses_relabeling_the_ordered_engine_and_escapes_the_wire_name() {
    let mut plan = wire();
    let ordered = singleton(plan.changes.changes[0].clone(), plan.mode, &MinimalDialect);
    plan.dialect = "another\nengine".into();
    assert_eq!(
        seal(ordered, plan).unwrap_err().to_string(),
        "the saved plan dialect another\\nengine does not match the ordered plan dialect minimal"
    );
}
