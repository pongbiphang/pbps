use crate::resolver::capture::{CaptureScope, capture, recapture};
use pbps_db::resolver::capture::ObjectIdentity;
use pbps_db::{Conn, Driver};
use std::collections::BTreeSet;

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions"]
async fn composite_assignments_retain_written_fields_and_child_bindings() {
    exercise_both_versions(0).await;
}
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions"]
async fn ranges_and_multiranges_qualify_complete_nested_output_paths() {
    exercise_both_versions(1).await;
}
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions"]
async fn sql_json_surfaces_retain_return_types_and_all_child_bindings() {
    exercise_both_versions(2).await;
}

async fn exercise_both_versions(group: u8) {
    let fixtures: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../fixtures/stored-surfaces.json")).unwrap();
    let mut outcomes = Vec::new();
    for (variable, fixture) in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"]
        .into_iter()
        .zip(fixtures)
    {
        let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
        assert!(!base.contains("://"));
        let name = format!(
            "pbps_stored612_{}",
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
                tokio::task::spawn_local(exercise(connection, fixture, group)).await
            })
            .await;
        admin
            .execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .await
            .unwrap();
        outcomes.push((variable, result.is_ok()));
    }
    assert!(
        outcomes.iter().all(|(_, ok)| *ok),
        "stored surface capture failed after cleanup: {outcomes:?}"
    );
}
fn identity(class: &str, namespace: &str, name: &str) -> ObjectIdentity {
    ObjectIdentity {
        class: class.into(),
        name: vec![namespace.into(), name.into()],
        signature: vec![],
    }
}
async fn exercise(connection: String, fixture: serde_json::Value, group: u8) {
    let mut conn = Conn::connect(Driver::Postgres, &connection).await.unwrap();
    conn.execute(fixture["setup"].as_str().unwrap())
        .await
        .unwrap();
    let mut captures = Vec::new();
    let mut failures = Vec::new();
    for (name, case) in fixture["cases"].as_object().unwrap() {
        let kind = case["kind"].as_str().unwrap();
        if match group {
            0 => kind != "routine",
            1 => name != "ranges",
            2 => !name.starts_with("json_"),
            _ => unreachable!(),
        } {
            continue;
        }
        let sql = case["sql"].as_str().unwrap();
        let ddl = if kind == "view" {
            format!("CREATE VIEW app.{name} AS {sql}")
        } else {
            sql.to_owned()
        };
        conn.execute(&ddl).await.unwrap();
        let root = identity(
            if kind == "view" {
                "pg_class"
            } else {
                "pg_proc"
            },
            "app",
            name,
        );
        match capture(
            &mut conn,
            &CaptureScope {
                retained: BTreeSet::from([root.clone()]),
                candidates: BTreeSet::new(),
            },
        )
        .await
        {
            Ok(before) => captures.push((name, kind, root, before)),
            Err(e) => failures.push((name, e)),
        }
    }
    assert!(
        failures.is_empty(),
        "valid stored surfaces must qualify: {failures:?}"
    );
    for (name, kind, root, before) in &captures {
        let owner = if *kind == "routine" {
            root.clone()
        } else {
            before
                .objects()
                .find(|id| id.class == "pg_rewrite" && id.signature.first() == Some(root))
                .unwrap()
                .clone()
        };
        let bound: BTreeSet<_> = before.bound_objects(&owner).unwrap().cloned().collect();
        if name.as_str() == "field_assignment" {
            // RHS expressions never read these fields. Their identities must
            // come from FIELDSTORE's assignment slots, not incidental reads.
            for field in ["id", "label"] {
                assert!(
                    bound.contains(&ObjectIdentity {
                        class: "column".into(),
                        name: vec![field.into()],
                        signature: vec![identity("pg_class", "app", "pair")]
                    }),
                    "missing written field {field}"
                );
            }
        }
        if name.as_str() == "json_composite" {
            assert!(bound.contains(&ObjectIdentity {
                class: "column".into(),
                name: vec!["id".into()],
                signature: vec![identity("pg_class", "app", "pair")]
            }));
        }
        let sql = fixture["cases"][name.as_str()]["sql"].as_str().unwrap();
        for (function, typ) in [("number", None), ("doc", None), ("label", Some("text"))] {
            if sql.contains(&format!("app.{function}(")) {
                let mut function = identity("pg_proc", "app", function);
                if let Some(typ) = typ {
                    function
                        .signature
                        .push(identity("pg_type", "pg_catalog", typ));
                }
                assert!(bound.contains(&function), "{name} lost child routine");
            }
        }
        let (_, changes) = recapture(&mut conn, before).await.unwrap();
        assert!(changes.is_empty(), "{name} unchanged");
    }
    match group {
        0=>conn.execute("ALTER TYPE app.pair RENAME ATTRIBUTE id TO renamed").await.unwrap(),
        1=>{let sql=fixture["cases"]["ranges"]["sql"].as_str().unwrap().replace("[1,2)","[1,3)");conn.execute(&format!("CREATE OR REPLACE VIEW app.ranges AS {sql}")).await.unwrap();},
        2=>conn.execute("CREATE OR REPLACE FUNCTION app.number() RETURNS integer LANGUAGE sql RETURN 2; CREATE OR REPLACE FUNCTION app.doc() RETURNS jsonb LANGUAGE sql RETURN '{\"x\":2,\"arr\":[3]}'::jsonb; CREATE OR REPLACE FUNCTION app.label(text) RETURNS text LANGUAGE sql RETURN lower($1)").await.unwrap(),
        _=>unreachable!(),
    }
    for (name, _, _, before) in &captures {
        let (_, changes) = recapture(&mut conn, before).await.unwrap();
        assert!(!changes.is_empty(), "{name} changed inputs");
    }
    let (name, kind, _, before) = &captures[0];
    let ddl = if *kind == "routine" {
        format!("DROP FUNCTION app.{name}()")
    } else {
        format!("DROP VIEW app.{name}")
    };
    conn.execute(&ddl).await.unwrap();
    assert!(
        recapture(&mut conn, before).await.is_err(),
        "missing retained root"
    );
}
