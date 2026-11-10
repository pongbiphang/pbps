#[test]
fn a_failed_apply_clears_the_previous_data_work_payload() {
    let before = pbps_model::StateSnapshot::new(
        pbps_model::StateKind::Apply,
        pbps_model::Schema::default(),
        pbps_model::IdsFile::default(),
        "deployer",
    );
    let work = pbps_model::DataWorkRecord {
        identity: pbps_model::DataWorkIdentity::ExternalVersion {
            version: "v7".into(),
        },
        expand_checksum: "a".repeat(64),
        outcome: pbps_model::DataWorkOutcome::Failed,
        notes: None,
    };
    let current = before
        .recording_data_work(
            work,
            "reporter".into(),
            pbps_model::Reason::new("backfill").unwrap(),
            None,
        )
        .unwrap();
    let mut current = current;
    current.target_assertions = vec![pbps_model::TargetAssertion {
        path: "checks/a.sql".into(),
        sha256: "c".repeat(64),
        outcome: pbps_model::AssertionOutcome::Pass,
    }];
    let failed = super::failed_apply_snapshot(
        pbps_db::Driver::Mssql,
        current,
        "deployer",
        None,
        &"b".repeat(64),
        &anyhow::anyhow!("refused"),
    );
    assert_eq!(failed.kind, pbps_model::StateKind::Failed);
    assert!(failed.data_work.is_none());
    assert!(failed.target_assertions.is_empty());
    assert_eq!(failed.schema, before.schema);
    assert!(pbps_model::StateSnapshot::from_json(&serde_json::to_string(&failed).unwrap()).is_ok());
}
