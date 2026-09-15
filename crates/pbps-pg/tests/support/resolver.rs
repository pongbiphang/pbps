use super::*;
use pbps_db::resolver::{Candidate, DiscoveryCompatibility};

#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn resolver_discovery_observes_both_catalog_versions_without_writes_or_qualification() {
    let current = std::env::var("PBPS_TEST_PG_DB").unwrap();
    let old = std::env::var("PBPS_TEST_PG_OLD_DB").unwrap();
    for (server, major) in [(current, 18), (old, 16)] {
        let mut admin = Conn::connect(Driver::Postgres, &server).await.unwrap();
        let name = format!("pbps_resolver597_{major}_{}", std::process::id());
        admin
            .execute(&format!(
                "CREATE DATABASE {name} TEMPLATE template0 LC_COLLATE 'C' LC_CTYPE 'C'"
            ))
            .await
            .unwrap();
        let mut conn = Conn::connect(Driver::Postgres, &format!("{server} dbname={name}"))
            .await
            .unwrap();
        conn.execute("CREATE SCHEMA ext; CREATE EXTENSION hstore WITH SCHEMA ext; SET search_path = ext, public; SET DateStyle = 'German, DMY'; SET TimeZone = 'Pacific/Auckland'; SET check_function_bodies = off; BEGIN READ ONLY").await.unwrap();
        let discovery = pbps_pg::resolver::discover(&mut conn).await.unwrap();
        assert_eq!(discovery.compatibility, DiscoveryCompatibility::Unverified);
        assert_eq!(
            discovery.candidate,
            Candidate::Suggested {
                image: format!("postgres:{major}")
            }
        );
        let facts = &discovery.observations;
        for (key, value) in [
            ("database_encoding", "UTF8"),
            ("database_collate", "C"),
            ("database_ctype", "C"),
            ("database_locale_provider", "c"),
            ("session_search_path", "ext, public"),
            ("session_date_style", "German, DMY"),
            ("session_time_zone", "Pacific/Auckland"),
            ("session_check_function_bodies", "off"),
        ] {
            assert_eq!(facts[key].value(), Some(value), "{key}: {discovery:?}");
        }
        assert_eq!(
            facts["database_locale"].value(),
            None,
            "libc has no ICU/builtin locale"
        );
        let extensions = discovery.extensions.as_ref().unwrap();
        assert!(
            extensions
                .iter()
                .any(|e| e.name == "hstore" && e.schema == "ext" && !e.version.is_empty()),
            "{discovery:?}"
        );
        assert!(
            discovery
                .qualification
                .values()
                .all(|f| f.value().is_none())
        );
        // Reads must not bootstrap even the ordinary ledger.
        assert!(!pbps_pg::state::is_initialized(&mut conn).await.unwrap());
        conn.execute("ROLLBACK; SET search_path = public; SET TimeZone = 'UTC'")
            .await
            .unwrap();
        let changed = pbps_pg::resolver::discover(&mut conn).await.unwrap();
        assert_eq!(
            changed.observations["session_search_path"].value(),
            Some("public")
        );
        assert_eq!(
            changed.observations["session_time_zone"].value(),
            Some("UTC")
        );
        assert_ne!(changed.observations, discovery.observations);

        // Denied inventory reads cannot become an empty successful inventory.
        // This revoke belongs only to this disposable database.
        let role = format!("pbps_resolver597_reader_{major}_{}", std::process::id());
        conn.execute(&format!("CREATE ROLE {role}; REVOKE SELECT ON pg_catalog.pg_extension FROM PUBLIC; SET ROLE {role}")).await.unwrap();
        let failure = pbps_pg::resolver::discover(&mut conn).await.unwrap_err();
        assert_eq!(failure.server_error_code().as_deref(), Some("42501"));
        conn.execute(&format!("RESET ROLE; DROP ROLE {role}"))
            .await
            .unwrap();
        drop(conn);
        admin
            .execute(&format!("DROP DATABASE {name}"))
            .await
            .unwrap();

        // A libc NULL alone cannot pin the 16/18 locale-column transition:
        // both a correct lookup and a missing key return NULL. ICU supplies
        // a real value that must survive each catalog spelling.
        let icu = format!("{name}_icu");
        admin
            .execute(&format!(
                "CREATE DATABASE {icu} TEMPLATE template0 LOCALE_PROVIDER icu ICU_LOCALE 'en-US'"
            ))
            .await
            .unwrap();
        let mut conn = Conn::connect(Driver::Postgres, &format!("{server} dbname={icu}"))
            .await
            .unwrap();
        conn.execute("BEGIN READ ONLY").await.unwrap();
        let observed = pbps_pg::resolver::discover(&mut conn).await.unwrap();
        assert_eq!(
            observed.observations["database_locale_provider"].value(),
            Some("i")
        );
        assert_eq!(
            observed.observations["database_locale"].value(),
            Some("en-US")
        );
        assert!(
            observed.observations["database_collation_recorded_version"]
                .value()
                .is_some_and(|v| !v.is_empty())
        );
        conn.execute("ROLLBACK; DROP EXTENSION plpgsql")
            .await
            .unwrap();
        let empty = pbps_pg::resolver::discover(&mut conn).await.unwrap();
        assert_eq!(
            empty.extensions,
            Some(vec![]),
            "a genuinely empty inventory remains distinct from the denied read"
        );
        drop(conn);
        admin
            .execute(&format!("DROP DATABASE {icu}"))
            .await
            .unwrap();
    }
}
