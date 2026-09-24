use crate::resolver::capture::{CaptureScope, capture, recapture};
use pbps_db::resolver::capture::ObjectIdentity;
use pbps_db::{Conn, Driver};
use std::collections::BTreeSet;

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn cte_search_cycle_and_xmltable_retain_their_complete_bindings() {
    exercise_both_versions(false).await;
}

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn derived_whole_rows_keep_output_shape_and_underlying_bindings() {
    exercise_both_versions(true).await;
}

async fn exercise_both_versions(whole: bool) {
    let mut outcomes = Vec::new();
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
        assert!(!base.contains("://"));
        let name = format!(
            "pbps_range612_{}",
            crate::catalog::probe_token().replace('-', "_")
        );
        let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
        admin
            .execute(&format!("CREATE DATABASE {name}"))
            .await
            .unwrap();
        let connection = format!("{base} dbname={name}");
        let result = tokio::task::LocalSet::new()
            .run_until(
                async move { tokio::task::spawn_local(exercise_ranges(connection, whole)).await },
            )
            .await;
        admin
            .execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .await
            .unwrap();
        outcomes.push((variable, result.is_ok()));
    }
    assert!(
        outcomes.iter().all(|(_, passed)| *passed),
        "range capture failed after fixture cleanup: {outcomes:?}"
    );
}

fn identity(class: &str, name: &str) -> ObjectIdentity {
    ObjectIdentity {
        class: class.into(),
        name: vec!["app".into(), name.into()],
        signature: vec![],
    }
}

async fn exercise_ranges(connection: String, whole: bool) {
    let mut conn = Conn::connect(Driver::Postgres, &connection).await.unwrap();
    conn.execute(r#"
        CREATE SCHEMA app;
        CREATE TYPE app.pair AS (id integer,label text);
        CREATE TABLE app.t(id integer,parent integer,label text);
        CREATE FUNCTION app.document() RETURNS xml LANGUAGE sql RETURN xmlelement(name root,xmlelement(name item,xmlattributes(1 AS id),xmlelement(name label,'one')));
        CREATE FUNCTION app.fallback() RETURNS text LANGUAGE sql RETURN 'fallback';
        CREATE FUNCTION app.rows() RETURNS TABLE(id integer,label text) LANGUAGE sql BEGIN ATOMIC SELECT t.id,t.label FROM app.t t; END;
        CREATE FUNCTION app.pairs() RETURNS SETOF app.pair LANGUAGE sql BEGIN ATOMIC SELECT ROW(t.id,t.label)::app.pair FROM app.t t; END;
    "#).await.unwrap();
    let cases = [
        (
            "search",
            "WITH RECURSIVE walk(id) AS (SELECT id FROM app.t UNION ALL SELECT t.id FROM app.t t JOIN walk w ON t.parent=w.id) SEARCH DEPTH FIRST BY id SET path SELECT id FROM walk",
        ),
        (
            "cycle",
            "WITH RECURSIVE walk(id) AS (SELECT id FROM app.t UNION ALL SELECT t.id FROM app.t t JOIN walk w ON t.parent=w.id) CYCLE id SET cycle USING path SELECT id,cycle FROM walk",
        ),
        (
            "xmltable",
            "SELECT x.id,x.label FROM XMLTABLE('/root/item' PASSING app.document() COLUMNS id integer PATH '@id', label text PATH 'label' DEFAULT app.fallback()) x",
        ),
        (
            "subquery_row",
            "SELECT row_to_json(s) AS value FROM (SELECT id,label FROM app.t) s",
        ),
        (
            "cte_row",
            "WITH s AS (SELECT id,label FROM app.t) SELECT row_to_json(s) AS value FROM s",
        ),
        (
            "function_row",
            "SELECT row_to_json(s) AS value FROM app.rows() s",
        ),
        ("named_function_row", "SELECT s AS value FROM app.pairs() s"),
        (
            "values_row",
            "SELECT row_to_json(s) AS value FROM (VALUES(1,'one'::text),(2,'two')) s(id,label)",
        ),
        (
            "join_row",
            "SELECT row_to_json(s) AS value FROM (app.t a JOIN app.t b USING(id)) s",
        ),
        (
            "xmltable_row",
            "SELECT row_to_json(x) AS value FROM XMLTABLE('/root/item' PASSING app.document() COLUMNS id integer PATH '@id', label text PATH 'label' DEFAULT app.fallback()) x",
        ),
    ];
    let mut captures = Vec::new();
    let mut failures = Vec::new();
    for (name, sql) in cases
        .into_iter()
        .filter(|(name, _)| name.ends_with("_row") == whole)
    {
        conn.execute(&format!("CREATE VIEW app.{name} AS {sql}"))
            .await
            .unwrap();
        let scope = CaptureScope {
            retained: BTreeSet::from([identity("pg_class", name)]),
            candidates: BTreeSet::new(),
        };
        match capture(&mut conn, &scope).await {
            Ok(before) => captures.push((name, sql, before)),
            Err(error) => failures.push((name, error)),
        }
    }
    assert!(
        failures.is_empty(),
        "valid query ranges must qualify: {failures:?}"
    );
    for (name, _, before) in &captures {
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
        let bindings: Vec<_> = before.bound_objects(rule).unwrap().cloned().collect();
        if whole {
            let row = bindings
                .iter()
                .find(|id| id.class == "query-output" && id.name.last().is_some_and(|n| n == "0"))
                .expect("a complete derived-row output");
            let labels: Vec<_> = row
                .signature
                .iter()
                .skip(1)
                .map(|field| field.name.last().unwrap().as_str())
                .collect();
            let expected = if *name == "join_row" {
                vec!["id", "parent", "label", "parent", "label"]
            } else {
                vec!["id", "label"]
            };
            assert_eq!(labels, expected, "{name} output order");
        }
        if name.contains("xmltable") {
            assert!(bindings.contains(&identity("pg_proc", "document")));
            assert!(bindings.contains(&identity("pg_proc", "fallback")));
        }
        if *name == "cycle" {
            assert!(
                bindings
                    .iter()
                    .any(|id| id.class == "pg_operator" && id.name == ["pg_catalog", "<>"])
            );
        }
        if matches!(
            *name,
            "search" | "cycle" | "subquery_row" | "cte_row" | "join_row"
        ) {
            assert!(bindings.contains(&identity("pg_class", "t")));
        }
        let (_, changes) = recapture(&mut conn, before).await.unwrap();
        assert!(changes.is_empty(), "{name} unchanged");
    }
    for (name, sql, before) in &captures {
        conn.execute(&format!(
            "CREATE OR REPLACE VIEW app.{name} AS {sql} WHERE false"
        ))
        .await
        .unwrap();
        let (_, changes) = recapture(&mut conn, before).await.unwrap();
        assert!(!changes.is_empty(), "{name} changed source must invalidate");
    }
    if whole {
        let (_, _, before) = captures
            .iter()
            .find(|(name, _, _)| *name == "subquery_row")
            .unwrap();
        conn.execute("CREATE OR REPLACE VIEW app.subquery_row AS SELECT row_to_json(s) AS value FROM (SELECT label,id::bigint AS renamed FROM app.t) s").await.unwrap();
        let (after, changes) = recapture(&mut conn, before).await.unwrap();
        assert!(!changes.is_empty());
        let rule = after
            .objects()
            .find(|id| {
                id.class == "pg_rewrite"
                    && id
                        .signature
                        .first()
                        .is_some_and(|owner| owner.name == ["app", "subquery_row"])
            })
            .unwrap();
        let bound: Vec<_> = after.bound_objects(rule).unwrap().collect();
        let row = bound
            .iter()
            .find(|id| id.class == "query-output" && id.name.last().is_some_and(|n| n == "0"))
            .unwrap();
        let labels: Vec<_> = row
            .signature
            .iter()
            .skip(1)
            .map(|field| field.name.last().unwrap().as_str())
            .collect();
        assert_eq!(labels, ["label", "renamed"]);
        assert!(
            bound
                .iter()
                .any(|id| id.class == "pg_type" && id.name == ["pg_catalog", "int8"])
        );
    }
    let (name, _, before) = &captures[0];
    conn.execute(&format!("DROP VIEW app.{name}"))
        .await
        .unwrap();
    assert!(
        recapture(&mut conn, before).await.is_err(),
        "a missing retained root cannot qualify"
    );
}
