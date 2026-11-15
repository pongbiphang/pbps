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

/// The newest state as the build before `object_deps` wrote it: an older
/// version, edges as `module_deps`.
fn newest_state_from_an_older_build() -> pbps_model::StateSnapshot {
    let mut row = serde_json::to_value(pbps_model::StateSnapshot::new(
        pbps_model::StateKind::Apply,
        pbps_model::Schema::default(),
        pbps_model::IdsFile::default(),
        "deployer",
    ))
    .unwrap();
    let fields = row.as_object_mut().unwrap();
    fields.shift_remove("object_deps");
    fields.insert(
        "version".into(),
        (pbps_model::state::OBJECT_DEPS_VERSION - 1).into(),
    );
    fields.insert(
        "module_deps".into(),
        serde_json::json!({ "app.v": ["app.base"] }),
    );
    pbps_model::StateSnapshot::from_json(&row.to_string()).unwrap()
}

#[test]
fn a_failed_apply_after_an_older_row_writes_a_row_the_next_read_accepts() {
    let current = newest_state_from_an_older_build();
    let older = current.version;
    assert!(older < pbps_model::state::CURRENT_VERSION);
    let edges = current.object_deps.clone();
    assert!(!edges.is_empty());

    let failed = super::failed_apply_snapshot(
        pbps_db::Driver::Postgres,
        current,
        "deployer",
        None,
        &"b".repeat(64),
        &anyhow::anyhow!("refused"),
    );
    assert_eq!(failed.version, pbps_model::state::CURRENT_VERSION);
    // Both drivers write the row with `serde_json::to_string`.
    let read = pbps_model::StateSnapshot::from_json(&serde_json::to_string(&failed).unwrap())
        .expect("the failed row is the newest one, so every later command reads it");
    assert_eq!(read.object_deps, edges);

    // The same row under the version it was read with is one no build may
    // write: the reader refuses it.
    let mut stale = failed;
    stale.version = older;
    let why = pbps_model::StateSnapshot::from_json(&serde_json::to_string(&stale).unwrap())
        .expect_err("an older version cannot hold `object_deps`");
    assert!(why.to_string().contains("object_deps"), "{why}");
}
