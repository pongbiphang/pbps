use crate::resolver::capture::{CaptureScope, capture, recapture};
use pbps_db::resolver::capture::ObjectIdentity;
use pbps_db::{Conn, Driver};
use std::collections::BTreeSet;

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn xml_constructors_keep_named_and_positional_routine_bindings() {
    exercise_both_versions(true).await;
}

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn composite_expression_results_keep_the_selected_field_owner() {
    exercise_both_versions(false).await;
}

async fn exercise_both_versions(xml: bool) {
    let mut outcomes = Vec::new();
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
        assert!(!base.contains("://"));
        let name = format!(
            "pbps_expr612_{}",
            crate::catalog::probe_token().replace('-', "_")
        );
        let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
        admin
            .execute(&format!("CREATE DATABASE {name}"))
            .await
            .unwrap();
        let connection = format!("{base} dbname={name}");
        let result = tokio::task::LocalSet::new()
            .run_until(async move {
                tokio::task::spawn_local(exercise_expressions(connection, xml)).await
            })
            .await;
        admin
            .execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .await
            .unwrap();
        outcomes.push((variable, result.is_ok()));
    }
    assert!(
        outcomes.iter().all(|(_, passed)| *passed),
        "expression capture failed after fixture cleanup: {outcomes:?}"
    );
}

fn identity(class: &str, name: &str) -> ObjectIdentity {
    ObjectIdentity {
        class: class.into(),
        name: vec!["app".into(), name.into()],
        signature: vec![],
    }
}

async fn exercise_expressions(connection: String, xml: bool) {
    let mut conn = Conn::connect(Driver::Postgres, &connection).await.unwrap();
    conn.execute(r#"
        CREATE SCHEMA app;
        CREATE TYPE app.pair AS (id integer, label text);
        CREATE TABLE app.t(p app.pair);
        CREATE FUNCTION app.label(app.pair) RETURNS text LANGUAGE sql RETURN $1.label;
        CREATE FUNCTION app.document() RETURNS text LANGUAGE sql RETURN '<root/>';
        CREATE FUNCTION app.pick(app.pair,app.pair) RETURNS app.pair LANGUAGE sql RETURN COALESCE($1,$2);
        CREATE OPERATOR app.+ (LEFTARG=app.pair,RIGHTARG=app.pair,FUNCTION=app.pick);
        CREATE AGGREGATE app.first_pair(app.pair) (SFUNC=app.pick,STYPE=app.pair);
    "#).await.unwrap();
    let cases = if xml {
        vec![
            (
                "xml_element",
                "SELECT xmlelement(name root, xmlattributes(app.label(p) AS label), app.label(p)) AS value FROM app.t",
            ),
            (
                "xml_parse",
                "SELECT xmlparse(document app.document() preserve whitespace) AS value",
            ),
            (
                "xml_serialize",
                "SELECT xmlserialize(content xmlelement(name root, app.label(p)) AS text) AS value FROM app.t",
            ),
            (
                "xml_document",
                "SELECT xmlparse(document app.document()) IS DOCUMENT AS value",
            ),
        ]
    } else {
        vec![
            (
                "operator_field",
                "SELECT (p OPERATOR(app.+) p).label AS value FROM app.t",
            ),
            (
                "nullif_field",
                "SELECT (nullif(p,p)).label AS value FROM app.t",
            ),
            (
                "aggregate_field",
                "SELECT (app.first_pair(p)).label AS value FROM app.t",
            ),
            (
                "window_field",
                "SELECT (first_value(p) OVER ()).label AS value FROM app.t",
            ),
            (
                "minmax_field",
                "SELECT (greatest(p,p)).label AS value FROM app.t",
            ),
            (
                "sublink_field",
                "SELECT (SELECT p FROM app.t LIMIT 1).label AS value",
            ),
        ]
    };
    let mut captures = Vec::new();
    let mut failures = Vec::new();
    for (name, sql) in cases {
        conn.execute(&format!("CREATE VIEW app.{name} AS {sql}"))
            .await
            .unwrap();
        let scope = CaptureScope {
            retained: BTreeSet::from([identity("pg_class", name)]),
            candidates: BTreeSet::new(),
        };
        // Collect each case independently so one unsupported node does not
        // hide the other valid expression shapes in the same engine version.
        match capture(&mut conn, &scope).await {
            Ok(before) => captures.push((name, before)),
            Err(error) => failures.push((name, error)),
        }
    }
    assert!(
        failures.is_empty(),
        "valid expressions must qualify: {failures:?}"
    );
    for (name, before) in &captures {
        let rule = before
            .objects()
            .find(|id| {
                id.class == "pg_rewrite"
                    && id
                        .signature
                        .first()
                        .is_some_and(|owner| owner.name == ["app", name])
            })
            .unwrap();
        let bound: BTreeSet<_> = before.bound_objects(rule).unwrap().cloned().collect();
        if xml {
            let function = if matches!(*name, "xml_parse" | "xml_document") {
                identity("pg_proc", "document")
            } else {
                ObjectIdentity {
                    signature: vec![identity("pg_type", "pair")],
                    ..identity("pg_proc", "label")
                }
            };
            assert!(
                bound.contains(&function),
                "{name} lost its XML child routine"
            );
        } else {
            assert!(
                bound.contains(&ObjectIdentity {
                    class: "column".into(),
                    name: vec!["label".into()],
                    signature: vec![identity("pg_class", "pair")],
                }),
                "{name} lost the composite result's field owner"
            );
        }
        let (_, changes) = recapture(&mut conn, before).await.unwrap();
        assert!(changes.is_empty());
    }
    if xml {
        conn.execute("CREATE OR REPLACE FUNCTION app.label(app.pair) RETURNS text LANGUAGE sql RETURN upper($1.label); CREATE OR REPLACE FUNCTION app.document() RETURNS text LANGUAGE sql RETURN '<changed/>'").await.unwrap();
    } else {
        conn.execute("ALTER TYPE app.pair RENAME ATTRIBUTE label TO renamed")
            .await
            .unwrap();
    }
    for (name, before) in &captures {
        let (_, changes) = recapture(&mut conn, before).await.unwrap();
        assert!(
            !changes.is_empty(),
            "{name} must invalidate changed captured inputs"
        );
    }
    let (name, before) = &captures[0];
    conn.execute(&format!("DROP VIEW app.{name}"))
        .await
        .unwrap();
    assert!(
        recapture(&mut conn, before).await.is_err(),
        "a missing retained root cannot qualify"
    );
}
