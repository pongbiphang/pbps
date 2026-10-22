//! Byte fixtures were captured before the exclusive value-source migration.
//! A JSON-value comparison misses field ordering, which changes approved checksums.

use pbps_model::{
    Change, ChangeSet, Collation, Column, ColumnRef, Declared, Generated, Identity, IdsFile,
    PlanBaseline, PlanOrigin, PlannedChange, SavedPlan, Schema, StateKind, StateSnapshot, Table,
    TableName, state_checksum,
};

fn artifacts() -> (Schema, IdsFile, SavedPlan, StateSnapshot) {
    let name = TableName::new("app", "sources");
    let mut table = Table::default();
    let mut ids = IdsFile::default();
    let table_uid: pbps_model::Uid = "t_1402aa".parse().unwrap();
    ids.tables.insert(table_uid.clone(), name.clone());
    let mut adds = Vec::new();
    for (i, (key, mut column)) in [
        ("plain", Column::new("integer".parse().unwrap())),
        ("defaulted", Column::new("integer".parse().unwrap())),
        (
            "identity",
            Column::new("integer".parse().unwrap()).not_null(),
        ),
        ("generated", Column::new("integer".parse().unwrap())),
    ]
    .into_iter()
    .enumerate()
    {
        match key {
            "plain" => {}
            "defaulted" => {
                column.value_source =
                    pbps_model::ValueSource::Default("  app.answer() + 1  ".into())
            }
            "identity" => {
                column.value_source = pbps_model::ValueSource::Identity(Identity {
                    seed: -7,
                    increment: 3,
                    options: Default::default(),
                })
            }
            "generated" => {
                column.value_source = pbps_model::ValueSource::Generated(Generated {
                    expression: "(plain * 2) /* declared */".into(),
                    stored: true,
                });
            }
            _ => unreachable!(),
        }
        column.collation = Some(Collation::new("C"));
        column.description = Some("Café — source".into());
        column.deprecated = Some("kept for byte compatibility".into());
        let uid: pbps_model::Uid = format!("c_1402a{i}").parse().unwrap();
        ids.columns
            .insert(uid.clone(), ColumnRef::new(name.clone(), key));
        adds.push(PlannedChange::new(Change::AddColumn {
            uid,
            table: name.clone(),
            name: key.into(),
            column: Box::new(column.clone()),
        }));
        table.columns.insert(key.into(), column);
    }
    let mut schema = Schema::default();
    schema.tables.insert(name.clone(), table.clone());
    let mut changes = vec![PlannedChange::new(Change::CreateTable {
        uid: table_uid,
        name,
        table: Box::new(table),
        beside_default: None,
        moves_at_most: None,
    })];
    changes.extend(adds);
    let mut plan = SavedPlan::new(
        PlanOrigin::Database,
        "postgres",
        "2026-10-12T00:00:00Z",
        PlanBaseline {
            time_zone: None,
            description: "value-source byte fixture".into(),
            checksum: state_checksum(&schema, &ids),
            database_collation: None,
        },
        ChangeSet { changes },
        ids.clone(),
    );
    // These bytes pin the historical value-source wire layout. A later plan
    // format must not silently change the fixture's version or checksum.
    plan.version = 36;
    let mut state = StateSnapshot::new(StateKind::Apply, schema.clone(), ids.clone(), "fixture");
    // Pin this historical envelope as the plan above is pinned. Adding
    // default-omitted table fields must preserve every fixture byte (#1172).
    state.version = 25;
    state.declared = Declared::from_schema(&schema);
    state.plan_checksum = Some(plan.wire_fingerprint());
    (schema, ids, plan, state)
}

#[test]
fn every_value_source_keeps_approved_bytes_and_fingerprints() {
    let (schema, ids, plan, state) = artifacts();
    let outputs = [
        ("schema.json", serde_json::to_string(&schema).unwrap()),
        ("plan.json", serde_json::to_string(&plan).unwrap()),
        ("state.json", serde_json::to_string(&state).unwrap()),
        (
            "declared.json",
            serde_json::to_string(&state.declared).unwrap(),
        ),
        ("state.sha256", state_checksum(&schema, &ids)),
        ("plan.sha256", plan.wire_fingerprint()),
    ];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/value-source");
    for (name, actual) in outputs {
        assert_eq!(
            actual,
            std::fs::read_to_string(root.join(name)).unwrap(),
            "{name}"
        );
    }
    assert_eq!(
        serde_json::from_str::<SavedPlan>(
            &std::fs::read_to_string(root.join("plan.json")).unwrap()
        )
        .unwrap(),
        plan
    );
    assert_eq!(
        StateSnapshot::from_json(&std::fs::read_to_string(root.join("state.json")).unwrap())
            .unwrap(),
        state
    );
    // The state reader supports historical envelopes, whose omitted optional
    // fields must not be confused with a new value-source tag.
    let mut historical = state.clone();
    historical.version = pbps_model::state::OLDEST_READABLE_VERSION;
    assert_eq!(
        StateSnapshot::from_json(&serde_json::to_string(&historical).unwrap()).unwrap(),
        historical
    );

    let mut changed = schema.clone();
    changed.tables.values_mut().next().unwrap().columns["defaulted"].value_source =
        pbps_model::ValueSource::Default("8".into());
    assert_ne!(
        state_checksum(&changed, &ids),
        state_checksum(&schema, &ids)
    );
}

/// The free text a plan and a state carry is `Text` (#2084), which serializes
/// as the bare string a `String` did: the fixture's own bytes, and its
/// fingerprints, are the other half of this pin. The fields the fixture leaves
/// out are inserted where the field order puts them, holding the characters a
/// hand-made file can hold, and must be exactly the JSON string of each.
#[test]
fn free_text_keeps_the_bytes_a_string_had() {
    let text = "a\nb\r\u{1b}[31mred\u{9b}2J \\ \"q\" \u{202e} 名前";
    let json = serde_json::to_string(text).unwrap();
    let (_, _, mut plan, mut state) = artifacts();
    let plan_before = serde_json::to_string(&plan).unwrap();
    let state_before = serde_json::to_string(&state).unwrap();

    plan.git_sha = Some(text.into());
    plan.created_at = text.into();
    plan.baseline.description = text.into();
    let expected = plan_before
        .replacen(
            r#""created_at":"2026-10-12T00:00:00Z""#,
            &format!(r#""created_at":{json},"git_sha":{json}"#),
            1,
        )
        .replacen(
            r#""description":"value-source byte fixture""#,
            &format!(r#""description":{json}"#),
            1,
        );
    assert_eq!(serde_json::to_string(&plan).unwrap(), expected);
    assert_eq!(
        serde_json::from_str::<SavedPlan>(&expected).unwrap(),
        plan,
        "the same bytes read back as the same plan"
    );

    state.git_sha = Some(text.into());
    state.reason = Some(text.into());
    let expected = state_before
        .replacen(
            r#""plan_checksum":"#,
            &format!(r#""git_sha":{json},"plan_checksum":"#),
            1,
        )
        .replacen(
            r#""operator":"fixture""#,
            &format!(r#""operator":"fixture","reason":{json}"#),
            1,
        );
    assert_eq!(serde_json::to_string(&state).unwrap(), expected);
    assert_eq!(StateSnapshot::from_json(&expected).unwrap(), state);

    // Negative: a value that is not a string is still refused, as it was.
    let number = plan_before.replacen(
        r#""created_at":"2026-10-12T00:00:00Z""#,
        r#""created_at":7"#,
        1,
    );
    assert!(serde_json::from_str::<SavedPlan>(&number).is_err());
}

#[test]
fn absent_and_explicit_null_sources_keep_the_historical_empty_spelling() {
    for input in [
        r#"{"type":"integer","nullable":true}"#,
        r#"{"type":"integer","nullable":true,"default":null,"identity":null,"generated":null}"#,
    ] {
        let column: Column = serde_json::from_str(input).unwrap();
        assert_eq!(column, Column::new("integer".parse().unwrap()));
        assert_eq!(
            serde_json::to_string(&column).unwrap(),
            r#"{"type":"integer","nullable":true}"#
        );
    }
    for input in [
        r#"{"type":"integer","nullable":true,"future_source":1}"#,
        r#"{"type":"integer","nullable":true,"default":"1","default":"2"}"#,
        r#"{"type":"integer"}"#,
    ] {
        assert!(serde_json::from_str::<Column>(input).is_err(), "{input}");
    }
}

#[test]
fn conflicting_or_repeated_sources_cannot_enter_a_saved_artifact() {
    let fields = [
        r#""default":"7""#,
        r#""identity":{"seed":1,"increment":1}"#,
        r#""generated":{"expression":"n * 2","stored":true}"#,
    ];
    for mask in [3_u8, 5, 6, 7] {
        let selected = fields
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, field)| *field)
            .collect::<Vec<_>>()
            .join(",");
        let input = format!(r#"{{"type":"integer","nullable":true,{selected}}}"#);
        let error = serde_json::from_str::<Column>(&input)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("only one of default, identity, sequence or generated"),
            "{error}"
        );
        // The same boundary applies below changes and recorded schemas.
        let plan = include_str!("fixtures/value-source/plan.json").replacen(
            r#""type":"integer","nullable":true"#,
            &format!(r#""type":"integer","nullable":true,{selected}"#),
            1,
        );
        assert!(serde_json::from_str::<SavedPlan>(&plan).is_err());
        let state = include_str!("fixtures/value-source/state.json").replacen(
            r#""type":"integer","nullable":true"#,
            &format!(r#""type":"integer","nullable":true,{selected}"#),
            1,
        );
        assert!(StateSnapshot::from_json(&state).is_err());
    }
    for field in [
        "default",
        "identity",
        "generated",
        "collation",
        "description",
        "deprecated",
    ] {
        let input =
            format!(r#"{{"type":"integer","nullable":true,"{field}":null,"{field}":null}}"#);
        assert!(
            serde_json::from_str::<Column>(&input)
                .unwrap_err()
                .to_string()
                .contains("duplicate field")
        );
    }
    for selected in fields {
        let input = format!(r#"{{"type":"integer","nullable":true,{selected}}}"#);
        assert!(serde_json::from_str::<Column>(&input).is_ok(), "{input}");
    }
}

/// A `serial` is written in its own `sequence` field, where the other three
/// sources are absent, and reads back as the source it was; a name the
/// engine generated is not written (#1631, DEC-1631.1).
#[test]
fn a_serial_source_round_trips_in_its_own_field() {
    let mut column = Column::new("integer".parse().unwrap()).not_null();
    column.value_source = pbps_model::ValueSource::Sequence(pbps_model::OwnedSequence {
        start: 100,
        increment: 5,
        name: None,
        options: Default::default(),
    });
    let text = serde_json::to_string(&column).unwrap();
    assert_eq!(
        text,
        r#"{"type":"integer","nullable":false,"sequence":{"start":100,"increment":5}}"#
    );
    assert_eq!(serde_json::from_str::<Column>(&text).unwrap(), column);
    // The negative case: beside a default it is a second source.
    let twice =
        r#"{"type":"integer","nullable":false,"default":"1","sequence":{"start":1,"increment":1}}"#;
    assert!(serde_json::from_str::<Column>(twice).is_err());
}
