use crate::resolver::capture::{CaptureScope, capture, recapture};
use pbps_db::resolver::capture::ObjectIdentity;
use pbps_db::{Conn, Driver};
use std::collections::BTreeSet;

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn merge_actions_keep_qualification_and_assignment_bindings() {
    exercise_both_versions(true).await;
}

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn builtin_scalar_constants_use_qualified_output_handlers() {
    exercise_both_versions(false).await;
}

async fn exercise_both_versions(merge: bool) {
    let mut outcomes = Vec::new();
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
        assert!(!base.contains("://"));
        let name = format!(
            "pbps_scalar612_{}",
            crate::catalog::probe_token().replace('-', "_")
        );
        let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
        admin
            .execute(&format!("CREATE DATABASE {name}"))
            .await
            .unwrap();
        let connection = format!("{base} dbname={name}");
        let result = tokio::task::LocalSet::new()
            .run_until(async move { tokio::task::spawn_local(exercise(connection, merge)).await })
            .await;
        admin
            .execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .await
            .unwrap();
        outcomes.push((variable, result.is_ok()));
    }
    assert!(
        outcomes.iter().all(|(_, passed)| *passed),
        "merge/scalar capture failed after fixture cleanup: {outcomes:?}"
    );
}

fn identity(class: &str, namespace: &str, name: &str) -> ObjectIdentity {
    ObjectIdentity {
        class: class.into(),
        name: vec![namespace.into(), name.into()],
        signature: vec![],
    }
}

async fn exercise(connection: String, merge: bool) {
    let mut conn = Conn::connect(Driver::Postgres, &connection).await.unwrap();
    conn.execute(r###"CREATE SCHEMA app; CREATE TABLE app.dst(id integer,label text); CREATE TABLE app.src(id integer,label text); CREATE FUNCTION app.matches(integer) RETURNS boolean LANGUAGE sql RETURN $1>0; CREATE FUNCTION app.project(text) RETURNS text LANGUAGE sql RETURN upper($1);"###).await.unwrap();
    let (retained, changed, dropped) = if merge {
        conn.execute(r###"CREATE FUNCTION app.merge_function() RETURNS void LANGUAGE sql BEGIN ATOMIC MERGE INTO app.dst d USING app.src s ON d.id=s.id WHEN MATCHED AND app.matches(s.id) THEN UPDATE SET label=app.project(s.label) WHEN MATCHED THEN DELETE WHEN NOT MATCHED AND app.matches(s.id) THEN INSERT(id,label) VALUES(s.id,app.project(s.label)) WHEN NOT MATCHED THEN DO NOTHING; END; CREATE PROCEDURE app.merge_procedure() LANGUAGE sql BEGIN ATOMIC MERGE INTO app.dst d USING app.src s ON d.id=s.id WHEN MATCHED AND app.matches(s.id) THEN UPDATE SET label=app.project(s.label) WHEN MATCHED THEN DELETE WHEN NOT MATCHED AND app.matches(s.id) THEN INSERT(id,label) VALUES(s.id,app.project(s.label)) WHEN NOT MATCHED THEN DO NOTHING; END;"###).await.unwrap();
        (
            BTreeSet::from([
                identity("pg_proc", "app", "merge_function"),
                identity("pg_proc", "app", "merge_procedure"),
            ]),
            "CREATE OR REPLACE FUNCTION app.matches(integer) RETURNS boolean LANGUAGE sql RETURN $1>1; CREATE OR REPLACE FUNCTION app.project(text) RETURNS text LANGUAGE sql RETURN lower($1)",
            "DROP FUNCTION app.merge_function()",
        )
    } else {
        conn.execute(r###"CREATE VIEW app.constants AS SELECT '127.0.0.1'::pg_catalog.inet AS c_inet, '10.0.0.0/8'::pg_catalog.cidr AS c_cidr, '08:00:2b:01:02:03'::pg_catalog.macaddr AS c_macaddr, '08:00:2b:01:02:03:04:05'::pg_catalog.macaddr8 AS c_macaddr8, '12.34'::pg_catalog.money AS c_money, '(1,2)'::pg_catalog.point AS c_point, '{1,2,3}'::pg_catalog.line AS c_line, '[(0,0),(1,1)]'::pg_catalog.lseg AS c_lseg, '(1,1),(0,0)'::pg_catalog.box AS c_box, '[(0,0),(1,1)]'::pg_catalog.path AS c_path, '((0,0),(1,0),(1,1))'::pg_catalog.polygon AS c_polygon, '<(0,0),1>'::pg_catalog.circle AS c_circle, '<root/>'::pg_catalog.xml AS c_xml, '$.a'::pg_catalog.jsonpath AS c_jsonpath, 'alpha:1 beta:2'::pg_catalog.tsvector AS c_tsvector, 'alpha & beta'::pg_catalog.tsquery AS c_tsquery, '0/16B6C50'::pg_catalog.pg_lsn AS c_pg_lsn, '10:20:12,15'::pg_catalog.pg_snapshot AS c_pg_snapshot, '10:20:12,15'::pg_catalog.txid_snapshot AS c_txid_snapshot, 'cursor_name'::pg_catalog.refcursor AS c_refcursor, '42'::pg_catalog.xid AS c_xid, '42'::pg_catalog.xid8 AS c_xid8, '42'::pg_catalog.cid AS c_cid, '(1,2)'::pg_catalog.tid AS c_tid, '1 2'::pg_catalog.int2vector AS c_int2vector, '1 2'::pg_catalog.oidvector AS c_oidvector"###).await.unwrap();
        (
            BTreeSet::from([identity("pg_class", "app", "constants")]),
            r###"CREATE OR REPLACE VIEW app.constants AS SELECT '127.0.0.2'::pg_catalog.inet AS c_inet, '10.0.0.0/8'::pg_catalog.cidr AS c_cidr, '08:00:2b:01:02:03'::pg_catalog.macaddr AS c_macaddr, '08:00:2b:01:02:03:04:05'::pg_catalog.macaddr8 AS c_macaddr8, '12.34'::pg_catalog.money AS c_money, '(1,2)'::pg_catalog.point AS c_point, '{1,2,3}'::pg_catalog.line AS c_line, '[(0,0),(1,1)]'::pg_catalog.lseg AS c_lseg, '(1,1),(0,0)'::pg_catalog.box AS c_box, '[(0,0),(1,1)]'::pg_catalog.path AS c_path, '((0,0),(1,0),(1,1))'::pg_catalog.polygon AS c_polygon, '<(0,0),1>'::pg_catalog.circle AS c_circle, '<root/>'::pg_catalog.xml AS c_xml, '$.a'::pg_catalog.jsonpath AS c_jsonpath, 'alpha:1 beta:2'::pg_catalog.tsvector AS c_tsvector, 'alpha & beta'::pg_catalog.tsquery AS c_tsquery, '0/16B6C50'::pg_catalog.pg_lsn AS c_pg_lsn, '10:20:12,15'::pg_catalog.pg_snapshot AS c_pg_snapshot, '10:20:12,15'::pg_catalog.txid_snapshot AS c_txid_snapshot, 'cursor_name'::pg_catalog.refcursor AS c_refcursor, '42'::pg_catalog.xid AS c_xid, '42'::pg_catalog.xid8 AS c_xid8, '42'::pg_catalog.cid AS c_cid, '(1,2)'::pg_catalog.tid AS c_tid, '1 2'::pg_catalog.int2vector AS c_int2vector, '1 2'::pg_catalog.oidvector AS c_oidvector"###,
            "DROP VIEW app.constants",
        )
    };
    let before = capture(
        &mut conn,
        &CaptureScope {
            retained,
            candidates: BTreeSet::new(),
        },
    )
    .await
    .expect("valid stored MERGE/scalar constants must qualify");
    if merge {
        for name in ["merge_function", "merge_procedure"] {
            let bound: BTreeSet<_> = before
                .bound_objects(&identity("pg_proc", "app", name))
                .unwrap()
                .cloned()
                .collect();
            for (function, typ) in [("matches", "int4"), ("project", "text")] {
                assert!(
                    bound.contains(&ObjectIdentity {
                        signature: vec![identity("pg_type", "pg_catalog", typ)],
                        ..identity("pg_proc", "app", function)
                    }),
                    "{name} lost an action child routine"
                );
            }
            for table in ["src", "dst"] {
                assert!(bound.contains(&identity("pg_class", "app", table)));
            }
        }
    } else {
        let handlers: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../fixtures/scalar-outputs.json")).unwrap();
        let objects: BTreeSet<_> = before.objects().cloned().collect();
        for handler in handlers {
            assert!(objects.contains(&identity(
                "pg_type",
                "pg_catalog",
                handler["name"].as_str().unwrap()
            )));
            assert!(objects.iter().any(|id| id.class == "pg_proc"
                && id.name == ["pg_catalog", handler["proname"].as_str().unwrap()]));
        }
    }
    let (_, changes) = recapture(&mut conn, &before).await.unwrap();
    assert!(changes.is_empty());
    conn.execute(changed).await.unwrap();
    let (_, changes) = recapture(&mut conn, &before).await.unwrap();
    assert!(
        !changes.is_empty(),
        "changed source or prerequisite must invalidate captured inputs"
    );
    conn.execute(dropped).await.unwrap();
    assert!(
        recapture(&mut conn, &before).await.is_err(),
        "a missing root cannot become an empty capture"
    );
}
