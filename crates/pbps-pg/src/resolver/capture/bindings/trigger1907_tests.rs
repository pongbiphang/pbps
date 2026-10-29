use super::*;
use serde_json::json;
use std::collections::BTreeMap;

fn catalog() -> Catalog {
    let row = |v: serde_json::Value| v.as_object().unwrap().clone();
    Catalog::new(BTreeMap::from([
        (
            "pg_namespace".into(),
            vec![row(json!({"oid":1,"nspname":"app"}))],
        ),
        (
            "pg_class".into(),
            vec![
                row(json!({"oid":2,"relname":"t","relnamespace":1})),
                row(json!({"oid":3,"relname":"other","relnamespace":1})),
            ],
        ),
        (
            "pg_attribute".into(),
            vec![
                row(json!({"attrelid":2,"attnum":4,"attname":"actual","attisdropped":false})),
                row(json!({"attrelid":3,"attnum":4,"attname":"other_column","attisdropped":false})),
            ],
        ),
    ]))
    .unwrap()
}

fn var(varno: u32, level: u32) -> Node {
    Node {
        tag: "VAR".into(),
        fields: BTreeMap::from([
            ("varno".into(), Value::Atom(varno.to_string())),
            ("varattno".into(), Value::Atom("4".into())),
            ("varlevelsup".into(), Value::Atom(level.to_string())),
        ]),
    }
}

#[test]
fn old_and_new_bind_the_same_live_column_only_in_a_trigger_qualifier() {
    let catalog = catalog();
    let expected = catalog.column(2, 4).unwrap();
    for varno in [1, 2] {
        assert_eq!(
            variable(&catalog, &var(varno, 0), Some(Relation::Trigger(2)), &[]),
            Ok(expected.clone())
        );
    }
    assert_eq!(
        variable(&catalog, &var(1, 0), Some(Relation::Table(2)), &[]),
        Ok(expected)
    );
    assert_eq!(
        variable(&catalog, &var(2, 0), Some(Relation::Table(2)), &[]),
        Err(Uncovered::Context)
    );
}

#[test]
fn trigger_context_does_not_admit_other_ranges_levels_or_an_absent_relation() {
    let catalog = catalog();
    for relation in [Some(Relation::Trigger(2)), Some(Relation::Table(2)), None] {
        for varno in [0, 3, u32::MAX] {
            assert_eq!(
                variable(&catalog, &var(varno, 0), relation, &[]),
                Err(Uncovered::Context)
            );
        }
        for varno in [1, 2] {
            assert_eq!(
                variable(&catalog, &var(varno, 1), relation, &[]),
                Err(Uncovered::Context)
            );
        }
    }
    assert_eq!(
        variable(&catalog, &var(2, 0), None, &[]),
        Err(Uncovered::Context)
    );
    assert_eq!(
        variable(&catalog, &var(2, 0), Some(Relation::Trigger(99)), &[]),
        Err(Uncovered::Reference)
    );
}

#[test]
fn a_query_range_keeps_its_own_relation_even_with_a_trigger_context() {
    let catalog = catalog();
    let range = |oid: &str| {
        Value::Node(Node {
            tag: "RANGETBLENTRY".into(),
            fields: BTreeMap::from([
                ("rtekind".into(), Value::Atom("0".into())),
                ("relid".into(), Value::Atom(oid.into())),
            ]),
        })
    };
    let query = Node {
        tag: "QUERY".into(),
        fields: BTreeMap::from([("rtable".into(), Value::List(vec![range("2"), range("3")]))]),
    };
    let frames = [Frame {
        query: &query,
        path: vec![],
    }];
    assert_eq!(
        variable(&catalog, &var(2, 0), Some(Relation::Trigger(2)), &frames),
        Ok(catalog.column(3, 4).unwrap())
    );
    assert_eq!(
        variable(&catalog, &var(3, 0), Some(Relation::Trigger(2)), &frames),
        Err(Uncovered::Context)
    );
}
