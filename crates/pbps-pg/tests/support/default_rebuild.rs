use super::{sqlstate, text};
use pbps_db::Conn;

async fn execute(db: &mut Conn, sql: &str) {
    println!("SQL: {sql}");
    db.execute(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

async fn defaults(db: &mut Conn) -> serde_json::Value {
    let raw = text(
        db,
        "SELECT json_agg(json_build_object(\
        'table', c.relname, 'type', format_type(a.atttypid,a.atttypmod), \
        'oid', d.oid::text, 'expression', pg_get_expr(d.adbin,d.adrelid), \
        'routines', (SELECT array_agg(dep.refobjid::text ORDER BY dep.refobjid) \
          FROM pg_depend dep WHERE dep.classid='pg_attrdef'::regclass \
          AND dep.objid=d.oid AND dep.refclassid='pg_proc'::regclass)) \
        ORDER BY c.relname)::text FROM pg_class c \
        JOIN pg_namespace n ON n.oid=c.relnamespace \
        JOIN pg_attribute a ON a.attrelid=c.oid AND a.attname='d' \
        LEFT JOIN pg_attrdef d ON d.adrelid=c.oid AND d.adnum=a.attnum \
        WHERE n.nspname='measurement1896'",
    )
    .await;
    serde_json::from_str(&raw).unwrap()
}

/// USING converts rows, not a held default. Measure the catalog and the
/// values together before assigning any extra ordering effect (#1896).
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_retyped_defaults_binding_and_assignment_are_measured_before_rebuild_rules() {
    let _live = super::live_ledger().await;
    // Each case rolls its private schema back; the two fixture sessions keep
    // both engine versions in the live gate without changing process settings.
    for variable in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let connection = std::env::var(variable).expect("the PostgreSQL 16/18 fixture");
        let mut db = Conn::connect(pbps_db::Driver::Postgres, &connection)
            .await
            .unwrap();
        let version = text(&mut db, "SHOW server_version").await;
        println!("server: {version}");
        for tree in [false, true] {
            for (kind, old, new, value, conversion, restoration, rebuilt) in [
                ("widen", "integer", "bigint", "7", "d::bigint", "f()", "17"),
                (
                    "using",
                    "text",
                    "integer",
                    "'7'",
                    "d::integer",
                    "f()::integer",
                    "'17'",
                ),
                (
                    "incompatible",
                    "boolean",
                    "integer",
                    "true",
                    "CASE WHEN d THEN 1 ELSE 0 END",
                    "f()",
                    "false",
                ),
            ] {
                for using in [false, true] {
                    println!("case: {kind}, tree={tree}, using={using}");
                    execute(&mut db, "BEGIN; CREATE SCHEMA measurement1896; SET LOCAL search_path=measurement1896,pg_catalog").await;
                    execute(&mut db, &format!("CREATE FUNCTION f() RETURNS {old} LANGUAGE sql IMMUTABLE AS $$ SELECT {value} $$")).await;
                    let partition = if tree { " PARTITION BY RANGE(k)" } else { "" };
                    execute(
                        &mut db,
                        &format!("CREATE TABLE t(k integer,d {old} DEFAULT f()){partition}"),
                    )
                    .await;
                    let own = match kind {
                        "widen" => "f()+1",
                        "using" => "f() || ''",
                        _ => "NOT f()",
                    };
                    if tree {
                        execute(&mut db, &format!("CREATE TABLE p PARTITION OF t FOR VALUES FROM (0) TO (100); ALTER TABLE p ALTER COLUMN d SET DEFAULT {own}")).await;
                    }
                    let cell = if old == "boolean" { "true" } else { "'7'" };
                    execute(&mut db, &format!("INSERT INTO t(k,d) VALUES(1,{cell})")).await;
                    let before = defaults(&mut db).await;
                    let old_routine = text(&mut db, "SELECT 'f()'::regprocedure::oid::text").await;
                    println!("before: {before}");
                    execute(&mut db, "SAVEPOINT original").await;
                    let clause = if using {
                        format!(" USING ({conversion})")
                    } else {
                        String::new()
                    };
                    let retype = format!("ALTER TABLE t ALTER COLUMN d TYPE {new}{clause}");
                    println!("SQL (held): {retype}");
                    match db.execute(&retype).await {
                        Ok(_) => {
                            assert_eq!(kind, "widen", "an incompatible held default was accepted");
                            let held = defaults(&mut db).await;
                            println!("held: {held}");
                            for (was, now) in before
                                .as_array()
                                .unwrap()
                                .iter()
                                .zip(held.as_array().unwrap())
                            {
                                assert_eq!(now["type"], new);
                                assert_ne!(now["oid"], was["oid"], "TYPE replaces pg_attrdef");
                                assert_eq!(now["routines"], serde_json::json!([old_routine]));
                                assert_eq!(now["expression"], was["expression"]);
                            }
                            execute(&mut db, "INSERT INTO t(k) VALUES(2)").await;
                            assert_eq!(text(&mut db, "SELECT d::text FROM t WHERE k=2").await, "7");
                        }
                        Err(error) => {
                            println!("held refused: {error:?}");
                            assert_ne!(kind, "widen");
                            assert_eq!(sqlstate(&error), "42804");
                        }
                    }
                    execute(&mut db, "ROLLBACK TO original").await;
                    assert_eq!(
                        defaults(&mut db).await,
                        before,
                        "refusal/rollback changes no default"
                    );
                    execute(&mut db, "ALTER TABLE t ALTER COLUMN d DROP DEFAULT").await;
                    if tree {
                        execute(&mut db, "ALTER TABLE p ALTER COLUMN d DROP DEFAULT").await;
                    }
                    execute(&mut db, &format!("DROP FUNCTION f(); CREATE FUNCTION f() RETURNS {old} LANGUAGE sql IMMUTABLE AS $$ SELECT {rebuilt} $$")).await;
                    let new_routine = text(&mut db, "SELECT 'f()'::regprocedure::oid::text").await;
                    assert_ne!(old_routine, new_routine);
                    println!("SQL (released): {retype}");
                    let result = db.execute(&retype).await;
                    if kind != "widen" && !using {
                        let error = result.expect_err("row conversion still needs USING");
                        println!("released type refused: {error:?}");
                        assert_eq!(sqlstate(&error), "42804");
                    } else {
                        result.unwrap();
                        let restore =
                            format!("ALTER TABLE t ALTER COLUMN d SET DEFAULT {restoration}");
                        println!("SQL: {restore}");
                        let result = db.execute(&restore).await;
                        if kind == "incompatible" {
                            let error = result.expect_err("an incompatible new default is refused");
                            println!("restoration refused: {error:?}");
                            assert_eq!(sqlstate(&error), "42804");
                        } else {
                            result.unwrap();
                            if tree {
                                execute(
                                    &mut db,
                                    &format!(
                                        "ALTER TABLE p ALTER COLUMN d SET DEFAULT ({restoration})+1"
                                    ),
                                )
                                .await;
                            }
                            let after = defaults(&mut db).await;
                            println!("rebuilt: {after}");
                            for now in after.as_array().unwrap() {
                                assert_eq!(now["type"], new);
                                assert_eq!(now["routines"], serde_json::json!([new_routine]));
                                assert!(now["expression"].as_str().unwrap().contains("f()"));
                            }
                            execute(&mut db, "INSERT INTO t(k) VALUES(3)").await;
                            assert_eq!(
                                text(&mut db, "SELECT d::text FROM t WHERE k=3").await,
                                "17"
                            );
                            if tree {
                                execute(&mut db, "INSERT INTO p(k) VALUES(4)").await;
                                assert_eq!(
                                    text(&mut db, "SELECT d::text FROM p WHERE k=4").await,
                                    "18"
                                );
                            }
                        }
                    }
                    execute(&mut db, "ROLLBACK").await;
                }
            }
        }
    }
}
