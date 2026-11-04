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

fn coded(code: &str) -> anyhow::Error {
    pbps_db::DbError::Driver {
        message: "server said".into(),
        code: Some(code.into()),
        diagnostic: None,
    }
    .into()
}

fn outcome_on(engine: pbps_db::Driver, error: anyhow::Error) -> Status {
    let mut phase = Phase::on(engine, "assertion", "a");
    phase.record(Instant::now(), "test.assertion", &Err::<(), _>(error));
    phase.outcome
}

#[test]
fn sql_server_engine_faults_are_unanswerable_and_the_users_own_errors_are_findings() {
    use pbps_db::Driver::Mssql;
    // Resources, storage and a session the server ended: the run could not look.
    for code in ["701", "1105", "9002", "823", "825", "596", "8645"] {
        assert!(
            matches!(outcome_on(Mssql, coded(code)), Status::Unanswerable),
            "{code}"
        );
    }
    // Negative: what the SQL itself causes is a finding, a write to the sealed
    // database (3906) and a missing permission (229) included.
    for code in ["102", "208", "229", "547", "3906", "8134", "50000"] {
        assert!(
            matches!(outcome_on(Mssql, coded(code)), Status::Failed),
            "{code}"
        );
    }
}

#[test]
fn a_number_that_is_a_postgres_class_is_only_a_fault_on_postgres() {
    use pbps_db::Driver::{Mssql, Postgres};
    // `THROW 53300` is the user's own number on SQL Server and `too_many_connections`
    // on PostgreSQL; the engine, not the digits, says which.
    for code in ["53300", "58030", "57014", "08006", "XX000"] {
        assert!(
            matches!(outcome_on(Postgres, coded(code)), Status::Unanswerable),
            "{code}"
        );
    }
    for code in ["53300", "58030"] {
        assert!(
            matches!(outcome_on(Mssql, coded(code)), Status::Failed),
            "{code}"
        );
    }
    // Negative: an ordinary PostgreSQL error stays a finding.
    assert!(matches!(
        outcome_on(Postgres, coded("42P01")),
        Status::Failed
    ));
}

#[test]
fn an_uncoded_database_error_never_answered_on_either_engine_and_an_offline_phase_reads_no_number()
{
    use pbps_db::Driver::{Mssql, Postgres};
    for engine in [Postgres, Mssql] {
        let timed_out = pbps_db::DbError::DeadlineExceeded("deadline").into();
        assert!(matches!(
            outcome_on(engine, timed_out),
            Status::Unanswerable
        ));
    }
    // Negative: a phase with no database gives a coded error no engine to be read by.
    let mut offline = Phase::offline("input", "x");
    offline.record(Instant::now(), "test.input", &Err::<(), _>(coded("53300")));
    assert!(matches!(offline.outcome, Status::Failed));
}
