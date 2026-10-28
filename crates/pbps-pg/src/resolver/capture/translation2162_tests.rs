//! Frozen against the engine-local translator before #2162 extraction.

use super::*;

fn fixture() -> Catalog {
    let rows = BTreeMap::from([
        (
            "pg_namespace".into(),
            vec![
                json!({"oid":42,"nspname":"app"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ],
        ),
        (
            "pg_type".into(),
            vec![
                json!({"oid":100,"typnamespace":42,"typname":"state"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ],
        ),
        (
            "pg_class".into(),
            vec![
                json!({"oid":200,"relnamespace":42,"relname":"t"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ],
        ),
        (
            "pg_attribute".into(),
            vec![
                json!({"attrelid":200,"attnum":1,"attname":"amount","attisdropped":false})
                    .as_object()
                    .unwrap()
                    .clone(),
            ],
        ),
    ]);
    Catalog::new(rows).unwrap()
}

fn shaped(class: &str, major: u32, values: Value, definitions: Value) -> Row {
    let mut row = fields(class, major)
        .unwrap()
        .iter()
        .map(|(name, _)| (name.to_string(), Value::Null))
        .collect::<Row>();
    row.extend(values.as_object().unwrap().clone());
    row.insert("__definitions".into(), definitions);
    row
}

#[test]
fn pg_translation_golden_preserves_property_bytes() {
    let c = fixture();
    let mut maps = BTreeMap::new();
    for major in [16, 18] {
        for (label, class, input) in [
            (
                "enum",
                "pg_enum",
                shaped(
                    "pg_enum",
                    major,
                    json!({"oid":300,"enumtypid":100,"enumsortorder":1.5,"enumlabel":"ready"}),
                    json!({}),
                ),
            ),
            (
                "zero_references",
                "pg_db_role_setting",
                shaped(
                    "pg_db_role_setting",
                    major,
                    json!({"setdatabase":0,"setrole":0,"setconfig":["work_mem=4MB"]}),
                    json!({}),
                ),
            ),
            (
                "definition_and_column",
                "pg_attrdef",
                shaped(
                    "pg_attrdef",
                    major,
                    json!({"oid":400,"adrelid":200,"adnum":1,"adbin":"private node"}),
                    json!({"adbin":"42::integer"}),
                ),
            ),
            (
                "index_columns",
                "pg_index",
                shaped(
                    "pg_index",
                    major,
                    json!({"indexrelid":200,"indrelid":200,"indkey":[1,0],"indclass":null,"indcollation":null,"indexprs":null,"indpred":null}),
                    json!({"indexprs":null,"indpred":null,"complete":"CREATE INDEX i ON app.t (amount, (42))"}),
                ),
            ),
        ] {
            maps.insert(
                format!("{major}-{label}"),
                normalize(&c, class, &input, major).unwrap(),
            );
        }
    }
    let bytes = serde_json::to_string(&maps).unwrap();
    assert_eq!(
        bytes,
        include_str!("fixtures/translation2162.json").trim_end()
    );
}

#[test]
fn pg_translation_golden_refuses_unknown_missing_and_null_references() {
    let c = fixture();
    for reference in [json!(999), Value::Null, json!("100")] {
        let input = shaped(
            "pg_enum",
            16,
            json!({"oid":300,"enumtypid":reference,"enumsortorder":1.5,"enumlabel":"ready"}),
            json!({}),
        );
        assert_eq!(
            normalize(&c, "pg_enum", &input, 16),
            Err(Uncovered::Reference)
        );
    }
    let mut missing = shaped(
        "pg_enum",
        18,
        json!({"oid":300,"enumtypid":100,"enumsortorder":1.5,"enumlabel":"ready"}),
        json!({}),
    );
    missing.remove("enumlabel");
    assert_eq!(
        normalize(&c, "pg_enum", &missing, 18),
        Err(Uncovered::Field)
    );
    assert_eq!(field("pg_enum", "future_oid", "oid"), Err(Uncovered::Field));
}
