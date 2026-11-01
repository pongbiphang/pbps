use super::*;

#[test]
fn d2b2_1165_empty_cleanup_has_no_project_or_plan_identity() {
    let mut data = TestData::new(true);
    data.cleanup_confirmed = true;
    let report = data.finish();
    assert!(matches!(report.result, crate::output::Outcome::Ok));
    let json = serde_json::to_value(&report).unwrap();
    assert!(json["data"]["input"].is_null());
    assert!(json["data"].get("plan").is_none());
    assert!(
        report
            .data
            .as_ref()
            .unwrap()
            .summary(true)
            .iter()
            .any(|f| f.message.contains("cleanup confirmed: true"))
    );
}

#[test]
fn d2b2_1165_the_composed_production_future_keeps_a_bounded_frame() {
    let mut data = TestData::new(false);
    let future = execute(None, &mut data);
    let bytes = std::mem::size_of_val(&future);
    eprintln!("D2b2 execute frame: {bytes} bytes");
    assert!(
        bytes < 64 * 1024,
        "owned command frame grew to {bytes}; use the existing boxing seam"
    );
}

#[test]
fn d2b2_1165_partial_input_commits_and_phase_costs_reach_the_human_renderer() {
    let mut data = TestData::new(false);
    data.observed_input.resolved_commits.insert(
        "candidate".into(),
        pbps_config::test_manifest::CommitId::parse(&"a".repeat(40)).unwrap(),
    );
    let mut p = Phase::new("candidate_apply", "working");
    p.elapsed_ms = 17;
    p.accepted = true;
    data.phases.push(p);
    let text = crate::output::human(&data.summary(true));
    assert!(text.contains(&"a".repeat(40)));
    assert!(text.contains("17 ms"));
    assert!(text.contains("accepted true"));
    let report = data.finish();
    assert!(matches!(report.result, crate::output::Outcome::Ok));
}

#[test]
fn d2b2_1165_publication_never_qualifies_unconfirmed_cleanup() {
    let mut data = TestData::new(false);
    data.run_id = Some("owned-but-not-confirmed-absent".into());
    let report = data.finish();
    assert!(matches!(
        report.result,
        crate::output::Outcome::Unanswerable
    ));
    let error = publish(report, false).unwrap_err();
    assert!(error.downcast_ref::<crate::Found>().is_none());
    assert!(error.to_string().contains("could not be completed"));
    let mut clean = TestData::new(true);
    clean.cleanup_confirmed = true;
    publish(clean.finish(), false).unwrap();
}

#[test]
fn d2b2_1165_human_values_are_plain_and_json_findings_keep_their_shape() {
    let mut data = TestData::new(false);
    data.run_id = Some("owned-run".into());
    data.engine_build = Some("PostgreSQL 18".into());
    data.recovery_live.push("live-run".into());
    let mut phase = Phase::new("candidate_apply", "working");
    phase.outcome = Status::Passed;
    phase.elapsed_ms = 17;
    phase.checksum = Some("sealed".into());
    phase.changes = Some(3);
    phase.risks = vec!["rewrite".into(), "lock".into()];
    data.phases.push(phase);
    let before = serde_json::to_value(&data).unwrap();
    let human = crate::output::human(&data.summary(true));
    assert!(human.contains("run owned-run; acquired image none; engine PostgreSQL 18"));
    assert!(human.contains("passed, 17 ms; checksum sealed"));
    assert!(human.contains("from none to none, distance none; changes 3; statements none"));
    assert!(human.contains("entry none; risks rewrite, lock"));
    assert!(human.contains("recovered none; live live-run"));
    for debug in ["Some(", "None", "[\"", "[]"] {
        assert!(!human.contains(debug), "{human}");
    }
    let json = data.summary(false);
    assert_eq!(
        json.iter()
            .find(|f| f.id == "test.runtime")
            .unwrap()
            .message,
        "run Some(\"owned-run\"); acquired image None; engine Some(\"PostgreSQL 18\")"
    );
    assert_eq!(
        json.iter().find(|f| f.id == "test.phase").unwrap().message,
        concat!(
            "candidate_apply `working`: Passed, 17 ms; checksum Some(\"sealed\"); ",
            "accepted false; started false; from None to None, distance None; ",
            "changes Some(3); statements None; entry None; risks [\"rewrite\", \"lock\"]"
        )
    );
    assert_eq!(serde_json::to_value(&data).unwrap(), before);
}
