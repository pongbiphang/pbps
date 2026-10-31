use super::*;
#[path = "../../../pbps-diff/tests/fixtures/creation2469.rs"]
mod fixture;
use fixture::{FACETS, Fixture, generated_facet};
use pbps_dialect::lower::lower_all;
use pbps_model::{Change, PlanMode, object_ref::ObjectDeps};
type Engine = Mssql;

async fn execute(db: &mut TestDb, plan: pbps_diff::order::CanonicalPlan<Engine>, engine: &Engine) {
    let ordered = plan.ordered(PlanMode::Transactional, engine);
    let lowered = lower_all(&ordered as &dyn pbps_diff::order::PlanView<Engine>, engine).unwrap();
    for statement in &lowered {
        db.conn.execute(&statement.sql).await.unwrap();
    }
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn demanded_computed_columns_and_not_null_wait_for_the_function() {
    let engine = Mssql;
    for facet in ["column", "not_null"] {
        let mut db = TestDb::create(&format!("creation2469_computed_{facet}")).await;
        db.conn.execute("CREATE SCHEMA app").await.unwrap();
        db.conn.execute("BEGIN TRANSACTION").await.unwrap();
        let f = generated_facet(facet, engine.name());
        let plan = f.compile(&engine).unwrap();
        let function = plan
            .changes()
            .changes
            .iter()
            .position(|p| matches!(p.change, Change::CreateModule { .. }))
            .unwrap();
        let ordered = plan.ordered(PlanMode::Transactional, &engine);
        let lowered =
            lower_all(&ordered as &dyn pbps_diff::order::PlanView<Engine>, &engine).unwrap();
        for (at, group) in lowered.groups().iter().enumerate() {
            for statement in group.statements() {
                db.conn.execute(&statement.sql).await.unwrap();
            }
            if at == function {
                let rows = db.conn.query("SELECT column_name FROM information_schema.columns WHERE table_schema='app' AND table_name='t' ORDER BY ordinal_position").await.unwrap();
                let names: Vec<_> = rows
                    .iter()
                    .map(|row| row.try_get::<&str>("column_name").unwrap().unwrap())
                    .collect();
                assert_eq!(names, ["anchor"], "{facet}");
            }
        }
        db.conn
            .execute("INSERT INTO app.t(anchor) VALUES(1)")
            .await
            .unwrap();
        let rows = db.conn.query("SELECT g AS value FROM app.t").await.unwrap();
        assert_eq!(rows[0].try_get::<i32>("value").unwrap(), Some(7));
        let rows = db.conn.query("SELECT is_persisted, is_nullable FROM sys.computed_columns WHERE object_id=OBJECT_ID('app.t') AND name='g'").await.unwrap();
        assert_eq!(rows[0].try_get::<bool>("is_persisted").unwrap(), Some(true));
        if facet == "not_null" {
            assert_eq!(rows[0].try_get::<bool>("is_nullable").unwrap(), Some(false));
        }
        db.conn.execute("COMMIT").await.unwrap();
        db.drop().await;
    }
}
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn demanded_facets_preserve_native_values_and_declared_column_order() {
    let engine = Mssql;
    for facet in FACETS {
        let mut db = TestDb::create(&format!("creation2469_{facet}")).await;
        db.conn.execute("CREATE SCHEMA app").await.unwrap();
        db.conn.execute("BEGIN TRANSACTION").await.unwrap();
        let f = Fixture::new(facet, "mssql");
        let plan = f
            .compile(&engine)
            .unwrap_or_else(|e| panic!("{facet}: {e}"));
        assert!(
            plan.annotation_edges()
                .unwrap()
                .iter()
                .all(|e| e.before < e.after)
        );
        execute(&mut db, plan, &engine).await;
        db.conn
            .execute("INSERT INTO app.p(anchor) VALUES(7)")
            .await
            .unwrap();
        db.conn
            .execute(if *facet == "default" {
                "INSERT INTO app.t(anchor) VALUES(1)"
            } else {
                "INSERT INTO app.t(anchor,x) VALUES(1,7)"
            })
            .await
            .unwrap();
        let rows = db
            .conn
            .query(if *facet == "generation" {
                "SELECT g AS value FROM app.t"
            } else {
                "SELECT x AS value FROM app.t"
            })
            .await
            .unwrap();
        assert_eq!(rows[0].try_get::<i32>("value").unwrap(), Some(7), "{facet}");
        let rows=db.conn.query("SELECT column_name FROM information_schema.columns WHERE table_schema='app' AND table_name='t' ORDER BY ordinal_position").await.unwrap();
        let actual: Vec<_> = rows
            .iter()
            .map(|r| r.try_get::<&str>("column_name").unwrap().unwrap())
            .collect();
        let expected = if *facet == "generation" {
            vec!["anchor", "x", "g"]
        } else {
            vec!["anchor", "x"]
        };
        assert_eq!(actual, expected, "{facet}");
        let bad = match *facet {
            "not_null" => Some("INSERT INTO app.t(anchor,x) VALUES(2,NULL)"),
            "primary_key" => Some("INSERT INTO app.t(anchor,x) VALUES(1,8)"),
            "unique" => Some("INSERT INTO app.t(anchor,x) VALUES(2,7)"),
            "foreign_key" => Some("INSERT INTO app.t(anchor,x) VALUES(2,999)"),
            "check" => Some("INSERT INTO app.t(anchor,x) VALUES(2,-1)"),
            _ => None,
        };
        if let Some(sql) = bad {
            db.conn.execute("SAVE TRANSACTION bad2469").await.unwrap();
            assert!(db.conn.execute(sql).await.is_err(), "{facet}");
            db.conn
                .execute("ROLLBACK TRANSACTION bad2469")
                .await
                .unwrap();
        }
        db.conn.execute("COMMIT").await.unwrap();
        db.drop().await;
    }
    let mut db = TestDb::create("creation2469_standing").await;
    db.conn.execute("CREATE SCHEMA app").await.unwrap();
    db.conn.execute("BEGIN TRANSACTION").await.unwrap();
    let mut f = Fixture::new("default", "mssql");
    f.schema
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .columns
        .get_mut("x")
        .unwrap()
        .nullable = false;
    let mut base = f.clone();
    base.schema
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .columns
        .shift_remove("x");
    base.ids.columns.remove(&"c_bbbbbb".parse().unwrap());
    execute(
        &mut db,
        base.compile_from(
            &Default::default(),
            &Default::default(),
            &ObjectDeps::default(),
            &engine,
        )
        .unwrap(),
        &engine,
    )
    .await;
    db.conn
        .execute("INSERT INTO app.t(anchor) VALUES(1)")
        .await
        .unwrap();
    execute(
        &mut db,
        f.compile_from(&base.schema, &base.ids, &f.deps, &engine)
            .unwrap(),
        &engine,
    )
    .await;
    let rows = db.conn.query("SELECT x AS value FROM app.t").await.unwrap();
    assert_eq!(rows[0].try_get::<i32>("value").unwrap(), Some(7));
    db.conn.execute("COMMIT").await.unwrap();
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn scratch_creation_facets_keep_auxiliary_setup_boundaries() {
    use pbps_mssql::resolver::reconstruct::{AuxiliaryStep, Reconstruction};
    let engine = Mssql;
    for facet in FACETS {
        let mut db = TestDb::create(&format!("creation2469_scratch_{facet}")).await;
        db.conn.execute("CREATE SCHEMA app").await.unwrap();
        let f = Fixture::new(facet, "mssql");
        let input = f.bootstrap(&engine).unwrap();
        let scratch =
            Reconstruction::with_chained(&engine, &input, &f.ids, &Default::default(), None, true)
                .unwrap();
        let last = scratch.len() - 1;
        assert!(matches!(
            scratch.auxiliary(last),
            Some(AuxiliaryStep::Deferred)
        ));
        scratch.compile_steps(&mut db.conn, 0..last).await.unwrap();
        let error = scratch
            .compile_steps(&mut db.conn, last..last + 1)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("requires its setup executor"),
            "{error}"
        );
        db.conn
            .execute(if *facet == "default" {
                "INSERT INTO app.t(anchor) VALUES(1)"
            } else {
                // The FK facet references the separately reconstructed parent.
                "INSERT INTO app.p(anchor) VALUES(7)"
            })
            .await
            .unwrap();
        if *facet != "default" {
            db.conn
                .execute("INSERT INTO app.t(anchor,x) VALUES(1,7)")
                .await
                .unwrap();
        }
        let rows = db
            .conn
            .query(if *facet == "generation" {
                "SELECT g AS value FROM app.t"
            } else {
                "SELECT x AS value FROM app.t"
            })
            .await
            .unwrap();
        assert_eq!(rows[0].try_get::<i32>("value").unwrap(), Some(7), "{facet}");
        let rows = db.conn.query("SELECT column_name FROM information_schema.columns WHERE table_schema='app' AND table_name='t' ORDER BY ordinal_position").await.unwrap();
        let columns: Vec<_> = rows
            .iter()
            .map(|r| r.try_get::<&str>("column_name").unwrap().unwrap())
            .collect();
        assert_eq!(
            columns,
            if *facet == "generation" {
                vec!["anchor", "x", "g"]
            } else {
                vec!["anchor", "x"]
            }
        );
        db.drop().await;
    }
}

#[path = "../../../pbps-diff/tests/fixtures/forward2469.rs"]
mod forward_fixture;

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn demanded_forward_columns_preserve_generation_and_column_order() {
    for pairs in [1, 2] {
        for validators in [false, true] {
            let mut db =
                TestDb::create(&format!("creation2469_forward_{pairs}_{validators}")).await;
            db.conn.execute("CREATE SCHEMA app").await.unwrap();
            db.conn.execute("BEGIN TRANSACTION").await.unwrap();
            let engine = Mssql;
            let f = forward_fixture::Scene::new(pairs, "mssql", validators);
            {
                let plan = f.compile(&engine).unwrap();
                execute(&mut db, plan, &engine).await;
                if validators {
                    db.conn
                        .execute("INSERT INTO app.p(k) VALUES(7)")
                        .await
                        .unwrap();
                }
                db.conn
                    .execute("INSERT INTO app.t(anchor) VALUES(1)")
                    .await
                    .unwrap();
                let rows = db.conn.query("SELECT a, b FROM app.t").await.unwrap();
                assert_eq!(rows[0].try_get::<i32>("a").unwrap(), Some(8));
                assert_eq!(rows[0].try_get::<i32>("b").unwrap(), Some(7));
                if pairs == 2 {
                    let rows = db.conn.query("SELECT c, d FROM app.t").await.unwrap();
                    assert_eq!(rows[0].try_get::<i32>("c").unwrap(), Some(12));
                    assert_eq!(rows[0].try_get::<i32>("d").unwrap(), Some(11));
                }
                let rows = db.conn.query("SELECT column_name FROM information_schema.columns WHERE table_schema='app' AND table_name='t' ORDER BY ordinal_position").await.unwrap();
                let actual: Vec<_> = rows
                    .iter()
                    .map(|r| r.try_get::<&str>("column_name").unwrap().unwrap())
                    .collect();
                assert_eq!(actual, f.columns);
                if validators {
                    let rows = db.conn.query("SELECT COUNT(*) AS n FROM sys.indexes WHERE object_id=OBJECT_ID('app.t') AND name='ix_t'").await.unwrap();
                    assert_eq!(rows[0].try_get::<i32>("n").unwrap(), Some(1));
                }
            }
            for invalid in [
                "CREATE TABLE app.bad_default(a integer DEFAULT (b + 1), b integer)",
                "CREATE TABLE app.bad_generation(b integer, c AS (b + 1), a AS (c + 1))",
            ] {
                db.conn
                    .execute("SAVE TRANSACTION invalid_forward")
                    .await
                    .unwrap();
                let error = db.conn.execute(invalid).await.unwrap_err();
                assert!(error.server_error_code().is_some(), "{error}");
                db.conn
                    .execute("ROLLBACK TRANSACTION invalid_forward")
                    .await
                    .unwrap();
            }
            db.conn.execute("COMMIT").await.unwrap();
            db.drop().await;
        }
    }
}

#[path = "../../../pbps-diff/tests/fixtures/prefix2469.rs"]
mod prefix_fixture;

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn prefix_facets_precede_the_function_and_later_column_in_native_order() {
    let engine = Mssql;
    for facet in prefix_fixture::FACETS {
        let mut db = TestDb::create(&format!("prefix2469_{facet}")).await;
        db.conn.execute("CREATE SCHEMA app").await.unwrap();
        db.conn.execute("BEGIN TRANSACTION").await.unwrap();
        let f = prefix_fixture::scene(facet, engine.name(), false);
        let plan = f.compile(&engine).unwrap();
        let (part, function, column) = prefix_fixture::positions(plan.changes(), facet);
        assert!(part < function && function < column, "{facet}");
        let ordered = plan.ordered(PlanMode::Transactional, &engine);
        let lowered =
            lower_all(&ordered as &dyn pbps_diff::order::PlanView<Engine>, &engine).unwrap();
        for (at, group) in lowered.groups().iter().enumerate() {
            for statement in group.statements() {
                db.conn.execute(&statement.sql).await.unwrap();
            }
            if at == part || at == function {
                let rows = db.conn.query("SELECT column_name FROM information_schema.columns WHERE table_schema='app' AND table_name='t' ORDER BY ordinal_position").await.unwrap();
                let names: Vec<_> = rows
                    .iter()
                    .map(|row| row.try_get::<&str>("column_name").unwrap().unwrap())
                    .collect();
                assert_eq!(
                    names,
                    ["anchor"],
                    "the facet and function precede the physical append: {facet}"
                );
            }
        }
        db.conn
            .execute("INSERT INTO app.p(anchor) VALUES(7)")
            .await
            .unwrap();
        db.conn
            .execute(if *facet == "default" {
                "INSERT INTO app.t(x) VALUES(11)"
            } else {
                "INSERT INTO app.t(anchor,x) VALUES(7,11)"
            })
            .await
            .unwrap();
        let rows = db.conn.query("SELECT anchor,x FROM app.t").await.unwrap();
        assert_eq!(rows[0].try_get::<i32>("anchor").unwrap(), Some(7));
        assert_eq!(rows[0].try_get::<i32>("x").unwrap(), Some(11));
        db.conn.execute("COMMIT").await.unwrap();
        db.drop().await;
        let mut scratch_db = TestDb::create(&format!("prefix2469_scratch_{facet}")).await;
        scratch_db.conn.execute("CREATE SCHEMA app").await.unwrap();
        let input = f.bootstrap(&engine).unwrap();
        let scratch = pbps_mssql::resolver::reconstruct::Reconstruction::with_chained(
            &engine,
            &input,
            &f.ids,
            &Default::default(),
            None,
            true,
        )
        .unwrap();
        let last = scratch.len() - 1;
        scratch
            .compile_steps(&mut scratch_db.conn, 0..last)
            .await
            .unwrap();
        scratch_db.drop().await;
    }
}
