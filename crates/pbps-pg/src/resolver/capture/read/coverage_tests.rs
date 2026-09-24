use crate::resolver::capture::{CaptureScope, capture, recapture};
use pbps_db::resolver::capture::ObjectIdentity;
use pbps_db::{Conn, Driver};
use std::collections::BTreeSet;

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn inherited_row_coercions_keep_parent_type_and_field_bindings() {
    exercise_both_versions(false).await;
}

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_large_constant_does_not_refuse_a_supported_stored_expression() {
    exercise_both_versions(true).await;
}

async fn exercise_both_versions(large: bool) {
    let mut outcomes = Vec::new();
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
        assert!(!base.contains("://"));
        let name = format!(
            "pbps_coverage612_{}",
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
                tokio::task::spawn_local(async move {
                    let mut conn = Conn::connect(Driver::Postgres, &connection).await.unwrap();
                    if large {
                        large_expression(&mut conn).await;
                    } else {
                        row_coercion(&mut conn).await;
                    }
                })
                .await
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
        "coverage capture failed after fixture cleanup: {outcomes:?}"
    );
}

fn identity(class: &str, name: &str) -> ObjectIdentity {
    ObjectIdentity {
        class: class.into(),
        name: vec!["app".into(), name.into()],
        signature: vec![],
    }
}

fn views(names: &[&str]) -> CaptureScope {
    CaptureScope {
        retained: names
            .iter()
            .map(|name| identity("pg_class", name))
            .collect(),
        candidates: BTreeSet::new(),
    }
}

async fn row_coercion(conn: &mut Conn) {
    conn.execute("CREATE SCHEMA app; CREATE TABLE app.parent(id integer,label text); CREATE TABLE app.child(extra integer) INHERITS(app.parent); CREATE VIEW app.row_cast AS SELECT c::app.parent AS value FROM app.child c; CREATE VIEW app.field_cast AS SELECT (c::app.parent).label AS value FROM app.child c").await.unwrap();
    let before = capture(conn, &views(&["row_cast", "field_cast"]))
        .await
        .expect("valid inherited-row coercions must qualify");
    for name in ["row_cast", "field_cast"] {
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
        assert!(bound.contains(&identity("pg_type", "parent")));
        assert!(bound.contains(&identity("pg_class", "child")));
        if name == "field_cast" {
            assert!(bound.contains(&ObjectIdentity {
                class: "column".into(),
                name: vec!["label".into()],
                signature: vec![identity("pg_class", "parent")]
            }));
        }
    }
    let (_, changes) = recapture(conn, &before).await.unwrap();
    assert!(changes.is_empty());
    conn.execute("ALTER TABLE app.parent RENAME COLUMN label TO renamed")
        .await
        .unwrap();
    let (_, changes) = recapture(conn, &before).await.unwrap();
    assert!(
        !changes.is_empty(),
        "a converted row's changed parent field must invalidate captured inputs"
    );
}

async fn large_expression(conn: &mut Conn) {
    // Build compressible synthetic source in the disposable database. No
    // multi-megabyte literal or node dump is retained in the repository/log.
    conn.execute("CREATE SCHEMA app; DO $fixture$ BEGIN EXECUTE format('CREATE VIEW app.large_value AS SELECT %L::text AS value', repeat('x', 5 * 1024 * 1024)); END $fixture$").await.unwrap();
    let rows = conn.query("SELECT octet_length(ev_action::text)::text AS bytes FROM pg_rewrite WHERE ev_class='app.large_value'::regclass AND rulename='_RETURN'").await.unwrap();
    let bytes = rows[0]
        .try_get::<&str>("bytes")
        .unwrap()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert!(
        bytes > 16 * 1024 * 1024,
        "the actual engine tree must cross the former private ceiling"
    );
    let before = capture(conn, &views(&["large_value"]))
        .await
        .expect("a valid large stored expression must qualify");
    let rule = before
        .objects()
        .find(|id| {
            id.class == "pg_rewrite"
                && id
                    .signature
                    .first()
                    .is_some_and(|owner| owner.name == ["app", "large_value"])
        })
        .unwrap();
    assert!(
        before
            .bound_objects(rule)
            .unwrap()
            .any(|id| id.class == "pg_type" && id.name == ["pg_catalog", "text"])
    );
    let (_, changes) = recapture(conn, &before).await.unwrap();
    assert!(changes.is_empty());
    conn.execute("DO $fixture$ BEGIN EXECUTE format('CREATE OR REPLACE VIEW app.large_value AS SELECT %L::text AS value', repeat('y', 5 * 1024 * 1024)); END $fixture$").await.unwrap();
    let (_, changes) = recapture(conn, &before).await.unwrap();
    assert!(
        !changes.is_empty(),
        "ignoring datum bytes for binding extraction must not hide changed source properties"
    );
}
