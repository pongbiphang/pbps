//! Per-test databases keep catalog reads independent of sibling DDL.

use pbps_db::{Conn, Driver};

pub(crate) struct TestDb {
    name: String,
    connection: String,
}

pub(crate) async fn shared() -> Conn {
    Conn::connect(
        Driver::Postgres,
        &std::env::var("PBPS_TEST_PG_DB").expect("PBPS_TEST_PG_DB"),
    )
    .await
    .expect("connect to shared PostgreSQL database")
}

impl TestDb {
    pub(crate) async fn create(tag: &str) -> Self {
        let base = std::env::var("PBPS_TEST_PG_DB").expect("PBPS_TEST_PG_DB");
        assert!(
            !base.contains("://"),
            "live tests require libpq keyword form"
        );
        let name = format!("pbps_bin_{tag}_{}", std::process::id());
        let mut admin = shared().await;
        admin
            .execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
            .await
            .expect("remove a fixture left by an interrupted run");
        admin
            .execute(&format!("CREATE DATABASE {name}"))
            .await
            .expect("create isolated database");
        // libpq accepts repeated keywords, with the last value winning. Keep
        // quoted passwords and other settings intact instead of splitting them.
        Self {
            connection: format!("{base} dbname={name}"),
            name,
        }
    }

    pub(crate) async fn connect(&self) -> Conn {
        Conn::connect(Driver::Postgres, &self.connection)
            .await
            .expect("connect to isolated database")
    }

    pub(crate) async fn drop(self) {
        shared()
            .await
            .execute(&format!("DROP DATABASE {} WITH (FORCE)", self.name))
            .await
            .expect("drop isolated database");
    }
}
