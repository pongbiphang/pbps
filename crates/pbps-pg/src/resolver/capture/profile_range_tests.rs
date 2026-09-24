use super::*;
use serde_json::json;
use std::collections::BTreeMap;

fn rows() -> BTreeMap<String, Vec<logical::Row>> {
    let row = |v: serde_json::Value| v.as_object().unwrap().clone();
    BTreeMap::from([
        (
            "pg_namespace".into(),
            vec![
                row(json!({"oid":1,"nspname":"pg_catalog"})),
                row(json!({"oid":2,"nspname":"app"})),
            ],
        ),
        (
            "pg_language".into(),
            vec![
                row(json!({"oid":3,"lanname":"internal"})),
                row(json!({"oid":4,"lanname":"sql"})),
                row(json!({"oid":5,"lanname":"c"})),
                row(json!({"oid":6,"lanname":"plpgsql"})),
            ],
        ),
        (
            "pg_type".into(),
            vec![
                row(
                    json!({"oid":100,"typname":"span","typnamespace":2,"typtype":"r","typelem":0,"typoutput":200}),
                ),
                row(
                    json!({"oid":101,"typname":"spans","typnamespace":2,"typtype":"m","typelem":0,"typoutput":201}),
                ),
                row(
                    json!({"oid":102,"typname":"int4","typnamespace":1,"typtype":"b","typelem":0,"typoutput":202}),
                ),
            ],
        ),
        (
            "pg_proc".into(),
            vec![
                row(
                    json!({"oid":200,"proname":"range_out","pronamespace":1,"proargtypes":[100],"prolang":3,"prosrc":"range_out"}),
                ),
                row(
                    json!({"oid":201,"proname":"multirange_out","pronamespace":1,"proargtypes":[101],"prolang":3,"prosrc":"multirange_out"}),
                ),
                row(
                    json!({"oid":202,"proname":"int4out","pronamespace":1,"proargtypes":[102],"prolang":3,"prosrc":"int4out"}),
                ),
                row(json!({"oid":203,"prolang":3})),
                row(json!({"oid":204,"prolang":3})),
                row(json!({"oid":205,"prolang":3})),
            ],
        ),
        (
            "pg_range".into(),
            vec![row(
                json!({"rngtypid":100,"rngmultitypid":101,"rngsubtype":102,"rngsubopc":300,"rngcanonical":204,"rngsubdiff":205}),
            )],
        ),
        (
            "pg_opclass".into(),
            vec![row(
                json!({"oid":300,"opcmethod":301,"opcfamily":302,"opcintype":102}),
            )],
        ),
        (
            "pg_am".into(),
            vec![row(json!({"oid":301,"amname":"btree"}))],
        ),
        (
            "pg_amproc".into(),
            vec![row(
                json!({"amprocfamily":302,"amproclefttype":102,"amprocrighttype":102,"amprocnum":1,"amproc":203}),
            )],
        ),
    ])
}

#[test]
fn range_output_requires_each_wrapper_subtype_and_readable_metadata() {
    let original = rows();
    for root in [100, 101] {
        assert!(datum(&Catalog::new(original.clone()).unwrap(), root).is_ok());
        for case in [
            "absent",
            "empty",
            "duplicate",
            "unreadable",
            "missing_subtype",
            "zero_subtype",
            "cycle",
            "missing_opclass",
            "missing_comparator",
            "duplicate_comparator",
            "wrong_method",
            "wrong_comparator_slot",
        ] {
            let mut rows = original.clone();
            match case {
                "absent" => {
                    rows.remove("pg_range");
                }
                "empty" => rows.get_mut("pg_range").unwrap().clear(),
                "duplicate" => {
                    let row = rows["pg_range"][0].clone();
                    rows.get_mut("pg_range").unwrap().push(row);
                }
                "unreadable" => {
                    rows.get_mut("pg_range").unwrap()[0]
                        .insert("rngtypid".into(), json!("unreadable"));
                }
                "missing_subtype" => {
                    rows.get_mut("pg_range").unwrap()[0].remove("rngsubtype");
                }
                "zero_subtype" | "cycle" => {
                    rows.get_mut("pg_range").unwrap()[0].insert(
                        "rngsubtype".into(),
                        json!(if case == "cycle" { 100 } else { 0 }),
                    );
                }
                "missing_opclass" => rows.get_mut("pg_opclass").unwrap().clear(),
                "missing_comparator" => rows.get_mut("pg_amproc").unwrap().clear(),
                "duplicate_comparator" => {
                    let row = rows["pg_amproc"][0].clone();
                    rows.get_mut("pg_amproc").unwrap().push(row);
                }
                "wrong_method" => {
                    rows.get_mut("pg_am").unwrap()[0].insert("amname".into(), json!("hash"));
                }
                "wrong_comparator_slot" => {
                    rows.get_mut("pg_amproc").unwrap()[0].insert("amprocnum".into(), json!(2));
                }
                _ => unreachable!(),
            }
            assert!(
                datum(&Catalog::new(rows).unwrap(), root).is_err(),
                "{root}/{case}"
            );
        }
        // Every wrapper and subtype is independently qualified, including the
        // range wrapper used by multirange_out for each member.
        for index in 0..3 {
            if root == 100 && index == 1 {
                continue;
            }
            for (field, value) in [
                ("proname", json!("unexpected")),
                ("pronamespace", json!(2)),
                ("prosrc", json!("unexpected")),
                ("prolang", json!(5)),
            ] {
                let mut rows = original.clone();
                rows.get_mut("pg_proc").unwrap()[index].insert(field.into(), value);
                assert!(
                    datum(&Catalog::new(rows).unwrap(), root).is_err(),
                    "{root}/{index}/{field}"
                );
            }
        }
    }
}

#[test]
fn range_support_callbacks_must_not_load_unqualified_code() {
    for index in 3..6 {
        for language in [3, 4, 5, 6, 99] {
            let mut rows = rows();
            rows.get_mut("pg_proc").unwrap()[index].insert("prolang".into(), json!(language));
            let catalog = Catalog::new(rows).unwrap();
            for root in [100, 101] {
                assert_eq!(
                    datum(&catalog, root).is_ok(),
                    matches!(language, 3 | 4),
                    "{root}/{index}/{language}"
                );
            }
        }
    }
    let mut optional = rows();
    for name in ["rngcanonical", "rngsubdiff"] {
        optional.get_mut("pg_range").unwrap()[0].insert(name.into(), json!(0));
    }
    assert!(datum(&Catalog::new(optional).unwrap(), 101).is_ok());
}
