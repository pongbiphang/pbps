use crate::resolver::capture::{CaptureScope, capture, recapture};
use pbps_db::resolver::capture::ObjectIdentity;
use pbps_db::{Conn, Driver};
use std::collections::BTreeSet;

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_small_scope_is_not_refused_by_an_unrelated_large_catalog() {
    let mut outcomes = Vec::new();
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
        assert!(!base.contains("://"));
        let name = format!(
            "pbps_large612_{}",
            crate::catalog::probe_token().replace('-', "_")
        );
        let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
        admin
            .execute(&format!("CREATE DATABASE {name}"))
            .await
            .unwrap();
        let connection = format!("{base} dbname={name}");
        // Cleanup both versions even when the old aggregate-size guard fails.
        let result = tokio::task::LocalSet::new()
            .run_until(
                async move { tokio::task::spawn_local(exercise_large_catalog(connection)).await },
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
        "large catalog capture failed after fixture cleanup: {outcomes:?}"
    );
}

async fn exercise_large_catalog(connection: String) {
    let mut conn = Conn::connect(Driver::Postgres, &connection).await.unwrap();
    // Repeated comments compress well in the disposable database, but the
    // catalog's JSON aggregate still exceeds the former 32 MiB ceiling. No
    // production rows, extensions, shared roles or retained output are needed.
    conn.execute("CREATE SCHEMA bulk; CREATE SCHEMA app; CREATE TABLE app.t(id integer); CREATE VIEW app.v AS SELECT id + 1 AS id FROM app.t; DO $fixture$ BEGIN FOR i IN 1..520 LOOP EXECUTE format('CREATE FUNCTION bulk.f%s() RETURNS integer LANGUAGE sql AS %L', i, 'SELECT 1 /*' || repeat('x', 65536) || '*/'); END LOOP; END $fixture$").await.unwrap();
    let rows = conn.query("SELECT sum(octet_length(prosrc))::text AS bytes FROM pg_catalog.pg_proc WHERE pronamespace='bulk'::regnamespace").await.unwrap();
    let bytes = rows[0]
        .try_get::<&str>("bytes")
        .unwrap()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert!(
        bytes > 32 * 1024 * 1024,
        "fixture must cross the former catalog ceiling"
    );
    let scope = CaptureScope {
        retained: BTreeSet::from([ObjectIdentity {
            class: "pg_class".into(),
            name: vec!["app".into(), "v".into()],
            signature: vec![],
        }]),
        candidates: BTreeSet::new(),
    };
    let before = capture(&mut conn, &scope)
        .await
        .expect("unrelated catalog size must not refuse a valid small scope");
    let rule = before
        .objects()
        .find(|id| {
            id.class == "pg_rewrite"
                && id
                    .signature
                    .first()
                    .is_some_and(|owner| owner.name == ["app", "v"])
        })
        .unwrap();
    assert!(
        before
            .bound_objects(rule)
            .unwrap()
            .any(|id| id.class == "pg_class" && id.name == ["app", "t"])
    );
    let (_, changes) = recapture(&mut conn, &before).await.unwrap();
    assert!(
        changes.is_empty(),
        "unchanged paged inputs must compare equal"
    );
    let pulled = crate::catalog::introspect(&mut conn).await.unwrap();
    let snapshot = pbps_model::StateSnapshot::new(
        pbps_model::StateKind::Bootstrap,
        pulled.schema,
        pbps_model::IdsFile::default(),
        "large-capture-fixture",
    );
    assert!(serde_json::to_string(&snapshot).unwrap().len() > 32 * 1024 * 1024);
    crate::state::ensure_tables(&mut conn).await.unwrap();
    crate::state::record(&mut conn, &snapshot).await.unwrap();
    let recorded = capture(&mut conn, &scope)
        .await
        .expect("a supported recorded snapshot must not have a private size ceiling");
    let (_, changes) = recapture(&mut conn, &recorded).await.unwrap();
    assert!(
        changes.is_empty(),
        "unchanged large recorded baseline must compare equal"
    );
    conn.execute("DROP VIEW app.v").await.unwrap();
    assert!(
        recapture(&mut conn, &recorded).await.is_err(),
        "paging must still refuse a missing required root"
    );
    let empty = CaptureScope {
        retained: BTreeSet::new(),
        candidates: BTreeSet::new(),
    };
    capture(&mut conn, &empty)
        .await
        .expect("an empty request must not inherit a database-size ceiling");
}
