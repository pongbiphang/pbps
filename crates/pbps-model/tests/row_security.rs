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
    for version in 6..pbps_model::state::CURRENT_VERSION {
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

#[test]
fn policy_creation_records_authored_predicates_without_inventing_missing_ones() {
    use pbps_model::{Change, ChangeSet, Declared, PlannedChange, RowSecurityOrigin};
    let name = TableName::new("app", "docs");
    let policy = RowPolicy {
        using: Some(" tenant_id=7 /* authored */".into()),
        ..Default::default()
    };
    let schema = Schema {
        tables: [(
            name.clone(),
            Table {
                row_policies: [("p".into(), policy.clone())].into(),
                ..Default::default()
            },
        )]
        .into(),
        ..Default::default()
    };
    let declared = Declared::from_schema(&schema);
    let mut live = schema.clone();
    live.tables
        .get_mut(&name)
        .unwrap()
        .row_policies
        .get_mut("p")
        .unwrap()
        .using = Some("(tenant_id = 7)".into());
    assert_eq!(declared.overlay(&live), schema);
    live.tables
        .get_mut(&name)
        .unwrap()
        .row_policies
        .get_mut("p")
        .unwrap()
        .with_check = Some("(tenant_id = 7)".into());
    assert!(
        declared.overlay(&live).tables[&name].row_policies["p"]
            .with_check
            .is_some()
    );
    live.tables
        .get_mut(&name)
        .unwrap()
        .row_policies
        .get_mut("p")
        .unwrap()
        .using = None;
    assert!(
        declared.overlay(&live).tables[&name].row_policies["p"]
            .using
            .is_none()
    );
    live.tables.get_mut(&name).unwrap().row_policies.clear();
    assert!(
        declared.overlay(&live).tables[&name]
            .row_policies
            .is_empty()
    );

    let create = Change::CreatePolicy {
        table: name.clone(),
        name: "p".into(),
        policy,
        origin: RowSecurityOrigin::Created,
    };
    let mut changes = ChangeSet::default();
    changes.changes.push(PlannedChange::new(create.clone()));
    let mut advanced = Declared::default();
    advanced.advance(&changes);
    assert_eq!(advanced, declared);
    let encoded = serde_json::to_value(&create).unwrap();
    assert_eq!(encoded["origin"], "created");
    assert_eq!(encoded["policy"]["to"], "public");
    assert!(encoded["policy"].get("with_check").is_none());
    for malformed in [
        {
            let mut v = encoded.clone();
            v.as_object_mut().unwrap().remove("origin");
            v
        },
        {
            let mut v = encoded.clone();
            v["origin"] = "forged".into();
            v
        },
        {
            let mut v = encoded.clone();
            v["extra"] = true.into();
            v
        },
    ] {
        assert!(serde_json::from_value::<Change>(malformed).is_err());
    }
    let uid: pbps_model::Uid = "t_117200".parse().unwrap();
    let renamed = TableName::new("app", "renamed");
    changes.changes = vec![PlannedChange::new(Change::RenameTable {
        uid: uid.clone(),
        from: name.clone(),
        to: renamed.clone(),
        defaults: vec![],
    })];
    advanced.advance(&changes);
    assert!(!advanced.expressions.policies.contains_key(&name));
    assert!(advanced.expressions.policies.contains_key(&renamed));
    changes.changes = vec![PlannedChange::new(Change::DropTable {
        uid,
        name: renamed,
        detach_from: None,
    })];
    advanced.advance(&changes);
    assert!(advanced.is_empty());
}

#[test]
fn written_policy_comparison_keeps_metadata_and_predicate_presence() {
    let policy = RowPolicy {
        using: Some("id=1".into()),
        ..Default::default()
    };
    let mut read = policy.clone();
    read.using = Some("(id = 1)".into());
    assert!(policy.agrees_when_written(&read));
    assert_ne!(policy, read);
    for field in 0..5 {
        let mut wrong = read.clone();
        match field {
            0 => wrong.using = None,
            1 => wrong.with_check = Some("id=1".into()),
            2 => wrong.restrictive = true,
            3 => wrong.command = PolicyCommand::Delete,
            _ => wrong.to = PolicyRoles::Roles(["app_user".into()].into()),
        }
        assert!(!policy.agrees_when_written(&wrong));
    }
}

#[test]
fn standing_policy_and_flag_risks_are_intrinsic_even_when_policies_are_latent() {
    use pbps_model::{Change, RiskClass, RowSecurityOrigin};
    use std::collections::BTreeSet;
    let table = TableName::new("app", "docs");
    let uid: pbps_model::Uid = "t_117300".parse().unwrap();
    for restrictive in [false, true] {
        let policy = RowPolicy {
            restrictive,
            using: Some("id > 0".into()),
            ..Default::default()
        };
        let create = Change::CreatePolicy {
            table: table.clone(),
            name: "p".into(),
            policy: policy.clone(),
            origin: RowSecurityOrigin::Standing,
        };
        let drop = Change::DropPolicy {
            table: table.clone(),
            name: "p".into(),
            policy,
        };
        assert_eq!(
            create.intrinsic_risks(),
            [if restrictive {
                RiskClass::Revoke
            } else {
                RiskClass::RowSecurity
            }]
            .into()
        );
        assert_eq!(
            drop.intrinsic_risks(),
            [if restrictive {
                RiskClass::RowSecurity
            } else {
                RiskClass::Revoke
            }]
            .into()
        );
        let mut created = create;
        if let Change::CreatePolicy { origin, .. } = &mut created {
            *origin = RowSecurityOrigin::Created;
        }
        assert!(created.intrinsic_risks().is_empty());
    }
    let flags = [
        RowSecurity::default(),
        RowSecurity {
            enabled: true,
            forced: false,
        },
        RowSecurity {
            enabled: false,
            forced: true,
        },
        RowSecurity {
            enabled: true,
            forced: true,
        },
    ];
    for from in flags {
        for to in flags {
            let change = Change::SetRowSecurity {
                uid: uid.clone(),
                table: table.clone(),
                from,
                to,
                origin: RowSecurityOrigin::Standing,
            };
            let mut expected = BTreeSet::new();
            if (!from.enabled && to.enabled) || (!from.forced && to.forced) {
                expected.insert(RiskClass::Revoke);
            }
            if (from.enabled && !to.enabled) || (from.forced && !to.forced) {
                expected.insert(RiskClass::RowSecurity);
            }
            assert_eq!(change.intrinsic_risks(), expected, "{from:?} -> {to:?}");
            let mut created = change;
            if let Change::SetRowSecurity { origin, .. } = &mut created {
                *origin = RowSecurityOrigin::Created;
            }
            assert!(created.intrinsic_risks().is_empty());
        }
    }
}

#[test]
fn dropped_policy_wire_payload_retains_every_field_and_is_strict() {
    use pbps_model::{Change, RowSecurityOrigin};
    let table = TableName::new("app", "docs");
    let policy = RowPolicy {
        command: PolicyCommand::Update,
        restrictive: true,
        to: PolicyRoles::Roles(["app_user".into()].into()),
        using: Some("id > 0".into()),
        with_check: Some("id < 100".into()),
    };
    let drop = Change::DropPolicy {
        table: table.clone(),
        name: "p".into(),
        policy: policy.clone(),
    };
    let value = serde_json::to_value(&drop).unwrap();
    assert_eq!(
        serde_json::from_value::<Change>(value.clone()).unwrap(),
        drop
    );
    assert_eq!(value["policy"], serde_json::to_value(&policy).unwrap());
    for field in ["table", "name", "policy"] {
        let mut bad = value.clone();
        bad.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<Change>(bad).is_err(), "{field}");
    }
    for field in ["origin", "extra"] {
        let mut bad = value.clone();
        bad[field] = "standing".into();
        assert!(serde_json::from_value::<Change>(bad).is_err(), "{field}");
    }
    let create = Change::CreatePolicy {
        table,
        name: "p".into(),
        policy,
        origin: RowSecurityOrigin::Standing,
    };
    let value = serde_json::to_value(&create).unwrap();
    assert_eq!(value["origin"], "standing");
    assert_eq!(serde_json::from_value::<Change>(value).unwrap(), create);
}

#[test]
fn row_security_is_a_distinct_gated_risk_in_wire_and_command_spellings() {
    use pbps_model::RiskClass;
    assert!(RiskClass::ALL.contains(&RiskClass::RowSecurity));
    assert_eq!(
        "row-security".parse::<RiskClass>().unwrap(),
        RiskClass::RowSecurity
    );
    assert_eq!(
        serde_json::to_string(&RiskClass::RowSecurity).unwrap(),
        "\"row-security\""
    );
    assert!(RiskClass::RowSecurity.is_gated());
    assert!(!RiskClass::GrantWiden.is_gated());
    assert!(!RiskClass::RowSecurity.why().is_empty());
    assert!("row_security".parse::<RiskClass>().is_err());
}
