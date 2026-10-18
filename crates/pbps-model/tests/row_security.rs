use pbps_model::{
    IdsFile, PolicyCommand, PolicyRoles, RowPolicy, RowSecurity, Schema, StateKind, StateSnapshot,
    Table, TableName, state_checksum,
};

#[test]
fn row_security_flags_and_policy_predicate_presence_round_trip_independently() {
    for enabled in [false, true] {
        for forced in [false, true] {
            let mut table = Table {
                row_security: RowSecurity { enabled, forced },
                ..Default::default()
            };
            for command in [
                PolicyCommand::All,
                PolicyCommand::Select,
                PolicyCommand::Insert,
                PolicyCommand::Update,
                PolicyCommand::Delete,
            ] {
                for restrictive in [false, true] {
                    let mut policy = RowPolicy {
                        command,
                        restrictive,
                        ..Default::default()
                    };
                    table.row_policies.insert("p".into(), policy.clone());
                    let missing = serde_json::to_string(&table).unwrap();
                    assert!(!missing.contains("with_check"));
                    assert_eq!(serde_json::from_str::<Table>(&missing).unwrap(), table);
                    policy.using = Some(" tenant = 7\n/* authored */ ".into());
                    policy.with_check = policy.using.clone();
                    policy.to = PolicyRoles::Roles(["z".into(), "PUBLIC".into()].into());
                    table.row_policies.insert("p".into(), policy);
                    let explicit = serde_json::to_string(&table).unwrap();
                    assert_ne!(missing, explicit);
                    assert!(explicit.contains(r#""to":{"roles":["PUBLIC","z"]}"#));
                    assert_eq!(serde_json::from_str::<Table>(&explicit).unwrap(), table);
                }
            }
        }
    }
    assert_eq!(
        serde_json::to_string(&PolicyRoles::Public).unwrap(),
        "\"public\""
    );
    assert_eq!(
        serde_json::from_str::<RowPolicy>("{}").unwrap(),
        RowPolicy::default()
    );
    for invalid in [
        r#"{"command":"truncate"}"#,
        r#"{"invented":true}"#,
        r#"{"to":{"roles":["app"],"extra":true}}"#,
        r#"{"using":false}"#,
    ] {
        assert!(
            serde_json::from_str::<RowPolicy>(invalid).is_err(),
            "{invalid}"
        );
    }
    assert!(serde_json::from_str::<RowSecurity>(r#"{"enable":true}"#).is_err());
}

#[test]
fn row_security_state_changes_the_checksum_but_default_fields_keep_old_bytes() {
    let plain: Table = serde_json::from_str(r#"{"columns":{}}"#).unwrap();
    assert_eq!(serde_json::to_string(&plain).unwrap(), r#"{"columns":{}}"#);
    let mut schema = Schema {
        tables: [(TableName::new("app", "t"), plain)].into(),
        ..Default::default()
    };
    let ids = IdsFile::default();
    let checksum = state_checksum(&schema, &ids);
    schema
        .tables
        .values_mut()
        .next()
        .unwrap()
        .row_security
        .forced = true;
    assert_ne!(state_checksum(&schema, &ids), checksum);
    schema.tables.values_mut().next().unwrap().row_security = RowSecurity::default();
    schema
        .tables
        .values_mut()
        .next()
        .unwrap()
        .row_policies
        .insert("p".into(), RowPolicy::default());
    assert_ne!(state_checksum(&schema, &ids), checksum);
    let snap = StateSnapshot::new(StateKind::Apply, schema, ids, "test");
    assert_eq!(snap.version, pbps_model::state::CURRENT_VERSION);
    let json = serde_json::to_string(&snap).unwrap();
    assert_eq!(StateSnapshot::from_json(&json).unwrap(), snap);
}

#[test]
fn row_security_absence_in_legacy_states_is_disabled_without_policies() {
    let mut state = StateSnapshot::new(
        StateKind::Apply,
        Schema {
            tables: [(TableName::new("app", "t"), Table::default())].into(),
            ..Default::default()
        },
        IdsFile::default(),
        "test",
    );
    for version in 6..=25 {
        state.version = version;
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("row_security"));
        assert!(!json.contains("row_policies"));
        let back = StateSnapshot::from_json(&json).unwrap();
        assert!(
            !back
                .schema
                .tables
                .values()
                .next()
                .unwrap()
                .has_row_security()
        );
        assert_eq!(serde_json::to_string(&back).unwrap(), json);
    }
    // A version newer than this build's is refused, not read partially.
    state.version = pbps_model::state::CURRENT_VERSION + 1;
    assert!(StateSnapshot::from_json(&serde_json::to_string(&state).unwrap()).is_err());
}
