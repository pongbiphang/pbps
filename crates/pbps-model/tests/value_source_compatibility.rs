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
    state.declared = Declared::from_schema(&schema);
    state.plan_checksum = Some(plan.checksum());
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
        ("plan.sha256", plan.checksum()),
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
            error.contains("only one of default, identity or generated"),
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
