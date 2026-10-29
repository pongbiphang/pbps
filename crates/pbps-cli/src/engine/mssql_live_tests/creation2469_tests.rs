use super::*;
use crate::computed_order::annotations_tests::fixture;

#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB"]
async fn checked_generation_edges_use_complete_catalog_collation_facts() {
    for (collation, called) in [
        ("Latin1_General_100_CI_AI", "cafe"),
        ("Latin1_General_100_CS_AS", "café"),
    ] {
        let server = std::env::var("PBPS_TEST_DB").unwrap();
        let mut conn = Conn::connect(Driver::Mssql, &server).await.unwrap();
        let db = format!("pbps_creation2469_{}_{}", std::process::id(), collation);
        conn.execute(&format!("CREATE DATABASE [{db}] COLLATE {collation}"))
            .await
            .unwrap();
        conn.execute(&format!("USE [{db}]")).await.unwrap();
        conn.execute("CREATE SCHEMA app").await.unwrap();
        let mut f = fixture::Fixture::new("generation", "mssql");
        let module = f.schema.modules.remove(&"app.f".parse().unwrap()).unwrap();
        f.schema.modules.insert("app.café".parse().unwrap(), module);
        f.deps.0[0].1 = ["app.café".parse().unwrap()].into();
        f.deps.0[1].0 = "app.café".parse().unwrap();
        f.schema
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .computed
            .get_mut("g")
            .unwrap()
            .expression = format!("app.[{called}]()");
        let engine = pbps_mssql::Mssql;
        let plan = f.compile(&engine).unwrap();
        refuse_added_computed_calls(&mut conn, &plan, &f.schema)
            .await
            .unwrap();
        let plain = order::propose(plan.changes().clone());
        assert!(
            refuse_added_computed_calls(&mut conn, &plain, &f.schema)
                .await
                .is_err()
        );
        let ordered = plan.ordered(pbps_model::PlanMode::Transactional, &engine);
        let lowered = pbps_dialect::lower::lower_all(
            &ordered as &dyn order::PlanView<pbps_mssql::Mssql>,
            &engine,
        )
        .unwrap();
        for statement in &lowered {
            conn.execute(&statement.sql).await.unwrap();
        }
        conn.execute("INSERT INTO app.t(anchor,x) VALUES(1,7)")
            .await
            .unwrap();
        let rows = conn.query("SELECT g AS value FROM app.t").await.unwrap();
        assert_eq!(rows[0].try_get::<i32>("value").unwrap(), Some(7));
        conn.execute("USE master").await.unwrap();
        conn.execute(&format!("DROP DATABASE [{db}]"))
            .await
            .unwrap();
    }
}
