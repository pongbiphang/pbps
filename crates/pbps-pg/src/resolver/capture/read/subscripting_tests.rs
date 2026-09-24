use crate::resolver::capture::{CaptureScope, capture, recapture};
use pbps_db::resolver::capture::ObjectIdentity;
use pbps_db::{Conn, Driver};
use std::collections::BTreeSet;

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn subscripting_captures_elements_slices_assignments_and_composite_fields() {
    let mut outcomes = Vec::new();
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
        assert!(!base.contains("://"));
        let name = format!(
            "pbps_subscript612_{}",
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
                async move { tokio::task::spawn_local(exercise_subscripting(connection)).await },
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
        "subscripting capture failed after fixture cleanup: {outcomes:?}"
    );
}

async fn exercise_subscripting(connection: String) {
    let mut conn = Conn::connect(Driver::Postgres, &connection).await.unwrap();
    conn.execute(
        r#"
        CREATE SCHEMA app;
        CREATE TYPE app.pair AS (id integer, label text);
        CREATE TABLE app.t(ints integer[], labels text[] COLLATE "C", pairs app.pair[]);
        CREATE FUNCTION app.lo() RETURNS integer LANGUAGE sql RETURN 1;
        CREATE FUNCTION app.hi() RETURNS integer LANGUAGE sql RETURN 2;
        CREATE VIEW app.element AS SELECT ints[1] AS x FROM app.t;
        CREATE VIEW app.slice AS SELECT ints[app.lo():app.hi()] AS x FROM app.t;
        CREATE VIEW app.multidimensional AS SELECT ints[1][2] AS x FROM app.t;
        CREATE VIEW app.open_slice AS SELECT ints[:][2:] AS x FROM app.t;
        CREATE VIEW app.collation AS SELECT labels[1] AS x FROM app.t;
        CREATE VIEW app.composite AS SELECT (pairs[1]).label AS x FROM app.t;
        CREATE FUNCTION app.assign() RETURNS integer[] LANGUAGE sql BEGIN ATOMIC
            UPDATE app.t SET ints[app.lo()] = app.hi() RETURNING ints;
        END;
    "#,
    )
    .await
    .unwrap();
    let identity = |class: &str, name: &str| ObjectIdentity {
        class: class.into(),
        name: vec!["app".into(), name.into()],
        signature: vec![],
    };
    let views = [
        "element",
        "slice",
        "multidimensional",
        "open_slice",
        "collation",
        "composite",
    ];
    let mut retained: BTreeSet<_> = views
        .iter()
        .map(|name| identity("pg_class", name))
        .collect();
    let assign = identity("pg_proc", "assign");
    retained.insert(assign.clone());
    let scope = CaptureScope {
        retained,
        candidates: BTreeSet::new(),
    };
    let before = capture(&mut conn, &scope)
        .await
        .expect("ordinary array subscripting must qualify");
    let bindings = |name: &str| {
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
        before
            .bound_objects(rule)
            .unwrap()
            .cloned()
            .collect::<BTreeSet<_>>()
    };
    for name in views {
        assert!(bindings(name).contains(&identity("pg_class", "t")));
    }
    for function in ["lo", "hi"] {
        assert!(bindings("slice").contains(&identity("pg_proc", function)));
        assert!(
            before
                .bound_objects(&assign)
                .unwrap()
                .any(|id| id == &identity("pg_proc", function))
        );
    }
    assert!(
        bindings("collation")
            .iter()
            .any(|id| id.class == "pg_collation" && id.name == ["pg_catalog", "C", "-1"])
    );
    assert!(
        bindings("composite")
            .iter()
            .any(|id| id.class == "column" && id.name == ["label"])
    );
    // The container's handler is a prerequisite even though the node stores
    // the container type, not a separate routine OID for subscripting.
    assert!(before.objects().any(|id| id.class == "pg_proc" && id.name == ["pg_catalog", "array_subscript_handler"]));
    let (_, changes) = recapture(&mut conn, &before).await.unwrap();
    assert!(changes.is_empty());
    conn.execute("ALTER TYPE app.pair RENAME ATTRIBUTE label TO renamed")
        .await
        .unwrap();
    let (_, changes) = recapture(&mut conn, &before).await.unwrap();
    assert!(
        !changes.is_empty(),
        "a composite element's changed field must invalidate captured inputs"
    );
    conn.execute("DROP VIEW app.element").await.unwrap();
    assert!(
        recapture(&mut conn, &before).await.is_err(),
        "subscripting support must still refuse a missing retained root"
    );
}
