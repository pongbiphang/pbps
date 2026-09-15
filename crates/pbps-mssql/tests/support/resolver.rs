use super::*;
use pbps_db::resolver::{Candidate, DiscoveryCompatibility};

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn resolver_discovery_reads_product_database_and_restricted_session_without_writes() {
    let mut db = TestDb::create("resolver597").await;
    db.conn.execute(&format!("ALTER DATABASE [{}] COLLATE Latin1_General_100_BIN2; ALTER DATABASE [{}] SET COMPATIBILITY_LEVEL = 150; CREATE USER resolver_reader WITHOUT LOGIN; EXECUTE AS USER = 'resolver_reader'; SET QUOTED_IDENTIFIER OFF; SET ARITHABORT ON; SET DATEFIRST 3;", db.name, db.name)).await.unwrap();
    let discovery = pbps_mssql::resolver::discover(&mut db.conn).await.unwrap();
    assert_eq!(discovery.compatibility, DiscoveryCompatibility::Unverified);
    assert_eq!(
        discovery.candidate,
        Candidate::Suggested {
            image: "mcr.microsoft.com/mssql/server:2025-latest".into()
        }
    );
    assert_eq!(discovery.extensions, None);
    let facts = &discovery.observations;
    for (key, value) in [
        ("product_major_version", "17"),
        ("engine_edition", "3"),
        ("database_collation", "Latin1_General_100_BIN2"),
        ("database_compatibility_level", "150"),
        ("session_current_user", "resolver_reader"),
        ("session_quoted_identifier", "0"),
        ("session_arithabort", "1"),
        ("session_date_first", "3"),
    ] {
        assert_eq!(facts[key].value(), Some(value), "{key}: {discovery:?}");
    }
    assert!(
        discovery
            .qualification
            .values()
            .all(|f| f.value().is_none())
    );
    // The session cannot do setup DDL, so success cannot depend on a write probe.
    assert!(
        db.conn
            .execute("CREATE TABLE dbo.unapproved_probe (id int)")
            .await
            .is_err()
    );
    db.conn
        .execute("REVERT; SET QUOTED_IDENTIFIER ON; SET DATEFIRST 7;")
        .await
        .unwrap();
    let changed = pbps_mssql::resolver::discover(&mut db.conn).await.unwrap();
    assert_eq!(
        changed.observations["session_quoted_identifier"].value(),
        Some("1")
    );
    assert_eq!(
        changed.observations["session_date_first"].value(),
        Some("7")
    );
    assert_ne!(changed.observations, discovery.observations);
    assert!(
        !pbps_mssql::state::is_initialized(&mut db.conn)
            .await
            .unwrap()
    );
    db.conn.execute("SET NOEXEC ON;").await.unwrap();
    assert!(
        pbps_mssql::resolver::discover(&mut db.conn).await.is_err(),
        "a session returning no catalog rows must not become a successful empty report"
    );
    db.conn.execute("SET NOEXEC OFF").await.unwrap();
    db.drop().await;
}
