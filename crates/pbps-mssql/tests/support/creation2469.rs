use super::*;
#[path = "../../../pbps-diff/tests/fixtures/creation2469.rs"]
mod fixture;
use fixture::{FACETS, Fixture};
use pbps_dialect::lower::lower_all;
use pbps_model::{PlanMode, object_ref::ObjectDeps};
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
