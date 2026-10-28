//! Recorded before extracting the property translator (#2162).

use super::super::read::{Constraint, Type, WholeRow};
use super::*;
use serde_json::{Value, json};

fn row(values: &[(&str, Option<&str>)]) -> WholeRow {
    values
        .iter()
        .map(|(k, v)| (k.to_string(), v.map(str::to_owned)))
        .collect()
}

fn fixture() -> Catalog {
    let mut c = super::super::tests::catalog();
    c.types.push(Type {
        id: 256,
        schema: 5,
        name: "Amount".into(),
        base: 56,
        max_length: 4,
        precision: 10,
        scale: 0,
        collation: None,
        nullable: true,
        user_defined: true,
        table_type: false,
    });
    c.objects.push(Object {
        id: 300,
        schema: 1,
        name: "CK__engine_number".into(),
        kind: "C".into(),
        parent: Some(100),
        owner: None,
    });
    c.constraints.insert(
        300,
        Constraint {
            column: 0,
            definition: "([amount]>(0))".into(),
            disabled: Some(false),
            trusted: Some(true),
            not_for_replication: Some(false),
        },
    );
    c.whole.insert(
        "check_constraints",
        vec![row(&[
            ("object_id", Some("300")),
            ("parent_object_id", Some("100")),
            ("name", Some("CK__engine_number")),
            ("is_system_named", Some("1")),
            ("definition", Some("([amount]>(0))")),
        ])],
    );
    c.whole.insert(
        "columns",
        vec![row(&[
            ("object_id", Some("100")),
            ("column_id", Some("1")),
            ("name", Some("amount")),
        ])],
    );
    c.whole.insert(
        "indexes",
        vec![
            row(&[
                ("object_id", Some("100")),
                ("index_id", Some("0")),
                ("name", None),
            ]),
            row(&[
                ("object_id", Some("100")),
                ("index_id", Some("2")),
                ("name", Some("ux_price")),
            ]),
        ],
    );
    c.data_spaces
        .insert(7, json!({"kind":"filegroup","name":"primary"}));
    c.xml_collections
        .insert(9, ("dbo".into(), "xmlc".into(), "<schema/>".into()));
    c.encryption_keys
        .insert(13, json!({"name":"cek","values":[]}));
    c.fulltext.insert(("catalog", 11), json!({"name":"search"}));
    c
}

#[test]
fn mssql_translation_golden_preserves_property_bytes() {
    let c = fixture();
    let lookup = Index::new(&c, &super::super::tests::producer()).unwrap();
    let whole = super::super::whole::Whole::new(&c);
    let cases = [
        (
            "object",
            "objects",
            row(&[
                ("object_id", Some("200")),
                ("name", Some("v")),
                ("schema_id", Some("5")),
                ("principal_id", Some("1")),
                ("parent_object_id", Some("0")),
                ("create_date", Some("discarded date")),
                ("future_scalar", None),
            ]),
        ),
        (
            "column",
            "columns",
            row(&[
                ("object_id", Some("100")),
                ("column_id", Some("1")),
                ("name", Some("amount")),
                ("user_type_id", Some("256")),
                ("system_type_id", Some("56")),
                ("xml_collection_id", Some("9")),
                ("column_encryption_key_id", Some("13")),
            ]),
        ),
        (
            "columns_in_owner",
            "foreign_key_columns",
            row(&[
                ("constraint_object_id", Some("400")),
                ("parent_object_id", Some("100")),
                ("referenced_object_id", Some("100")),
                ("parent_column_id", Some("1")),
                ("referenced_column_id", Some("0")),
            ]),
        ),
        (
            "index_in_owner",
            "foreign_keys",
            row(&[
                ("object_id", Some("400")),
                ("referenced_object_id", Some("100")),
                ("key_index_id", Some("2")),
            ]),
        ),
        (
            "heap_name",
            "foreign_keys",
            row(&[
                ("referenced_object_id", Some("100")),
                ("key_index_id", Some("0")),
            ]),
        ),
        (
            "spaces",
            "tables",
            row(&[
                ("object_id", Some("100")),
                ("history_table_id", None),
                ("ledger_view_id", Some("200")),
                ("lob_data_space_id", Some("7")),
                ("filestream_data_space_id", Some("0")),
                ("max_column_id_used", Some("900")),
            ]),
        ),
        (
            "fulltext",
            "fulltext_indexes",
            row(&[
                ("object_id", Some("100")),
                ("unique_index_id", Some("2")),
                ("fulltext_catalog_id", Some("11")),
                ("stoplist_id", Some("0")),
                ("property_list_id", None),
            ]),
        ),
        (
            "caller",
            "sql_modules",
            row(&[
                ("object_id", Some("200")),
                ("execute_as_principal_id", None),
                (
                    "definition",
                    Some("CREATE VIEW app.v AS SELECT N'quote''\\line'\n"),
                ),
                ("uses_ansi_nulls", Some("1")),
            ]),
        ),
        (
            "owner",
            "sql_modules",
            row(&[("execute_as_principal_id", Some("-2"))]),
        ),
        (
            "principal",
            "sql_modules",
            row(&[("execute_as_principal_id", Some("7"))]),
        ),
        (
            "null_identity",
            "columns",
            row(&[("user_type_id", None), ("principal_id", None)]),
        ),
        (
            "unnamed_constraint",
            "check_constraints",
            c.whole["check_constraints"][0].clone(),
        ),
    ];
    let mut maps = BTreeMap::<String, Value>::new();
    for (label, view, row) in cases {
        maps.insert(label.into(), whole.translate(view, &row, &lookup).unwrap());
    }
    maps.insert("referenced_ids".into(), json!(whole.take_referenced()));
    let bytes = serde_json::to_string(&maps).unwrap();
    assert_eq!(
        bytes,
        include_str!("fixtures/translation2162.json").trim_end()
    );
}

#[test]
fn mssql_translation_golden_keeps_unreadable_distinct_from_null() {
    let c = fixture();
    let lookup = Index::new(&c, &super::super::tests::producer()).unwrap();
    let whole = super::super::whole::Whole::new(&c);
    for (view, input) in [
        ("objects", row(&[("future_id", None)])),
        ("objects", row(&[("schema_id", Some("999"))])),
        ("columns", row(&[("user_type_id", Some("not a number"))])),
        ("periods", row(&[("start_column_id", Some("1"))])),
        (
            "foreign_keys",
            row(&[
                ("referenced_object_id", Some("100")),
                ("key_index_id", Some("99")),
            ]),
        ),
    ] {
        assert!(whole.translate(view, &input, &lookup).is_err(), "{view}");
    }
}
