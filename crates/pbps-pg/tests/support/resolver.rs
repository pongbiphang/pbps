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

// ---------------------------------------------------------------------------
// #610: analysis-scope facts and the compatibility rule against real servers.
// ---------------------------------------------------------------------------

mod scope610 {
    use super::*;
    use pbps_db::resolver::Observation;
    use pbps_db::resolver::environment::{
        EnvironmentFacts, ExecutableIdentity, ExecutableRole, ExecutableSet, FactStatus,
        Provenance, Side, Verdict,
    };
    use pbps_pg::resolver::compatibility::compare;
    use pbps_pg::resolver::environment::{Scope, read};

    /// Executables are read from the kernel, not SQL; these tests give both
    /// sides the same content so only the catalog facts decide.
    fn with_executables(catalog: pbps_db::resolver::environment::CatalogFacts) -> EnvironmentFacts {
        let engine = ExecutableIdentity {
            role: ExecutableRole::Engine,
            path: "/usr/lib/postgresql/bin/postgres".into(),
            digest: Some("00".repeat(32)),
            provenance: Provenance::LoadedContent,
            disk_differs_from_loaded: Some(false),
        };
        EnvironmentFacts {
            catalog,
            executables: ExecutableSet {
                engine,
                libraries: vec![],
            },
        }
    }

    async fn connect(server: &str) -> Conn {
        Conn::connect(Driver::Postgres, server).await.unwrap()
    }

    #[tokio::test]
    #[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
    async fn scope_facts_read_the_deployers_own_visibility_and_leave_the_session_untouched() {
        let server = std::env::var("PBPS_TEST_PG_DB").unwrap();
        let pid = std::process::id();
        let database = format!("pbps_scope610_{pid}");
        let deployer = format!("pbps_dep610_{pid}");
        let mut admin = connect(&server).await;
        admin
            // A versioned libc locale (the image ships en_US.utf8), so the
            // provider's actual collation version is a value; `C` has none.
            .execute(&format!(
                "CREATE DATABASE {database} TEMPLATE template0 LC_COLLATE 'en_US.utf8' LC_CTYPE 'en_US.utf8'"
            ))
            .await
            .unwrap();
        admin
            .execute(&format!(
                "CREATE ROLE {deployer} LOGIN PASSWORD 'pbps-dep-610' NOSUPERUSER NOCREATEDB NOCREATEROLE"
            ))
            .await
            .unwrap();
        let mut setup = connect(&format!("{server} dbname={database}")).await;
        // Competing objects on two schemas; the deployer may use only `b`.
        setup
            .execute(&format!(
                "CREATE SCHEMA a; CREATE SCHEMA b; CREATE TABLE a.t (x int); CREATE TABLE b.t (y int); \
                 REVOKE ALL ON SCHEMA a FROM PUBLIC; GRANT USAGE ON SCHEMA b TO {deployer}; \
                 GRANT CONNECT ON DATABASE {database} TO {deployer}; CREATE EXTENSION hstore"
            ))
            .await
            .unwrap();
        let schemas = ["a".to_owned(), "b".to_owned()];
        let scope = Scope {
            schemas: &schemas,
            write_path_extras: &[],
        };

        // The deployer's planning session, with one setting set on itself.
        let deployer_url = server
            .split_whitespace()
            .filter(|part| !part.starts_with("user=") && !part.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" ");
        let mut planning = connect(&format!(
            "{deployer_url} dbname={database} user={deployer} password=pbps-dep-610"
        ))
        .await;
        planning
            .execute("SET DateStyle = 'German, DMY'")
            .await
            .unwrap();
        let before = planning
            .query("SELECT current_setting('search_path') AS p")
            .await
            .unwrap()[0]
            .try_get::<&str>("p")
            .unwrap()
            .unwrap()
            .to_owned();
        let seen_by_deployer = read(&mut planning, &scope).await.unwrap();
        let after = planning
            .query("SELECT current_setting('search_path') AS p")
            .await
            .unwrap()[0]
            .try_get::<&str>("p")
            .unwrap()
            .unwrap()
            .to_owned();
        assert_eq!(
            before, after,
            "evaluating visibility must not change the session's path"
        );

        // The engine's own answer: `a` is on the path but not usable, so it is
        // dropped; `b` is kept.
        assert_eq!(
            seen_by_deployer.visibility["a"],
            Observation::reported(Some("{pg_catalog}"))
        );
        assert_eq!(
            seen_by_deployer.visibility["b"],
            Observation::reported(Some("{pg_catalog,b}"))
        );
        // A libc database: provider `c`, no ICU locale or rules, and the
        // provider's actual version is a value, not NULL.
        assert_eq!(
            seen_by_deployer.observations["database_locale_provider"].value(),
            Some("c")
        );
        assert_eq!(
            seen_by_deployer.observations["database_locale"],
            Observation::NotReported
        );
        assert_eq!(seen_by_deployer.collations[0].key, "default");
        assert!(
            seen_by_deployer.collations[0]
                .actual_version
                .value()
                .is_some_and(|v| !v.is_empty()),
            "{:?}",
            seen_by_deployer.collations[0]
        );
        // The extension's native library is what a resolver has to load.
        let hstore = seen_by_deployer
            .extensions
            .iter()
            .find(|e| e.name == "hstore")
            .expect("hstore is installed");
        assert_eq!(hstore.libraries, vec!["$libdir/hstore".to_owned()]);
        assert!(hstore.requires.is_empty());
        assert!(seen_by_deployer.available_extensions.contains_key("hstore"));
        // Where a setting came from is read with it.
        assert_eq!(seen_by_deployer.settings["DateStyle"].source, "session");
        assert_ne!(seen_by_deployer.settings["TimeZone"].source, "session");

        // The setup administrator sees `a`; a resolver compiled as that role
        // would bind `t` differently, and the rule says so. The session-set
        // DateStyle is unknown on the deployer's side, never a match.
        let seen_by_admin = read(&mut setup, &scope).await.unwrap();
        let report = compare(
            &with_executables(seen_by_deployer.clone()),
            &with_executables(seen_by_admin),
            &[],
        );
        assert_eq!(
            report.facts["visibility:a"],
            FactStatus::Mismatch {
                target: "{pg_catalog}".into(),
                resolver: "{pg_catalog,a}".into()
            }
        );
        assert_eq!(report.facts["visibility:b"], FactStatus::Match);
        assert!(matches!(
            report.facts["setting:DateStyle"],
            FactStatus::Unknown {
                side: Side::Target,
                ..
            }
        ));
        assert!(matches!(report.verdict(), Verdict::Unknown(_)));
        // The same principal against itself: visibility matches, and only
        // the session-set setting keeps the scope from verifying.
        let same = compare(
            &with_executables(seen_by_deployer.clone()),
            &with_executables(seen_by_deployer),
            &[],
        );
        assert_eq!(same.facts["visibility:a"], FactStatus::Match);
        // A least-privilege login cannot see the superuser-only settings at
        // all (pg_settings hides the rows), and the refusal names the grant.
        assert_eq!(
            same.verdict(),
            Verdict::Unknown(vec![
                "setting:DateStyle".into(),
                "setting:dynamic_library_path".into(),
                "setting:session_preload_libraries".into(),
                "setting:shared_preload_libraries".into(),
            ])
        );
        match &same.facts["setting:shared_preload_libraries"] {
            FactStatus::Unknown { reason, .. } => {
                assert!(reason.contains("pg_read_all_settings"), "{reason}")
            }
            FactStatus::Match | FactStatus::Mismatch { .. } => panic!("{same:?}"),
        }
        // The remedy is a read-only role, not superuser: granted, the settings
        // are reported, and only the session-set DateStyle keeps the scope
        // from verifying; reset, it verifies.
        admin
            .execute(&format!("GRANT pg_read_all_settings TO {deployer}"))
            .await
            .unwrap();
        drop(planning);
        let mut planning = connect(&format!(
            "{deployer_url} dbname={database} user={deployer} password=pbps-dep-610"
        ))
        .await;
        planning
            .execute("SET DateStyle = 'German, DMY'")
            .await
            .unwrap();
        let granted = read(&mut planning, &scope).await.unwrap();
        assert_eq!(
            granted.settings["shared_preload_libraries"].context,
            "postmaster"
        );
        let same = compare(
            &with_executables(granted.clone()),
            &with_executables(granted),
            &[],
        );
        assert_eq!(
            same.verdict(),
            Verdict::Unknown(vec!["setting:DateStyle".into()])
        );
        planning.execute("RESET DateStyle").await.unwrap();
        let reset = read(&mut planning, &scope).await.unwrap();
        let same = compare(
            &with_executables(reset.clone()),
            &with_executables(reset),
            &[],
        );
        assert_eq!(same.verdict(), Verdict::Verified, "{same:?}");

        drop(planning);
        drop(setup);
        admin
            .execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
            .await
            .unwrap();
        admin
            .execute(&format!("DROP ROLE {deployer}"))
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
    async fn the_rule_refuses_version_and_collation_differences_between_real_servers_and_verifies_a_matching_side()
     {
        let current = std::env::var("PBPS_TEST_PG_DB").unwrap();
        let old = std::env::var("PBPS_TEST_PG_OLD_DB").unwrap();
        let pid = std::process::id();
        let name = format!("pbps_scope610_pair_{pid}");
        let schemas = ["public".to_owned()];
        let scope = Scope {
            schemas: &schemas,
            write_path_extras: &[],
        };

        // An ICU database on the current server, a libc one on the old server.
        let mut current_admin = connect(&current).await;
        current_admin
            .execute(&format!(
                "CREATE DATABASE {name} TEMPLATE template0 LOCALE_PROVIDER icu ICU_LOCALE 'en-US' \
                 LC_COLLATE 'C' LC_CTYPE 'C'"
            ))
            .await
            .unwrap();
        let mut old_admin = connect(&old).await;
        old_admin
            .execute(&format!(
                "CREATE DATABASE {name} TEMPLATE template0 LC_COLLATE 'C' LC_CTYPE 'C'"
            ))
            .await
            .unwrap();
        let mut target = connect(&format!("{current} dbname={name}")).await;
        target.execute("CREATE EXTENSION hstore").await.unwrap();
        let mut resolver = connect(&format!("{old} dbname={name}")).await;

        let target_facts = read(&mut target, &scope).await.unwrap();
        let resolver_facts = read(&mut resolver, &scope).await.unwrap();
        assert_eq!(
            target_facts.observations["database_locale_provider"].value(),
            Some("i")
        );
        assert_eq!(
            target_facts.observations["database_locale"].value(),
            Some("en-US")
        );

        let report = compare(
            &with_executables(target_facts.clone()),
            &with_executables(resolver_facts.clone()),
            &[],
        );
        let Verdict::Mismatch(keys) = report.verdict() else {
            panic!("18/ICU against 16/libc must mismatch: {report:?}");
        };
        for expected in [
            "server_version_num",
            "database_locale_provider",
            "database_locale",
            "collation:default",
        ] {
            assert!(
                keys.iter().any(|k| k == expected),
                "{expected} missing from {keys:?}"
            );
        }
        // The same image family ships hstore at the same version: the
        // extension itself is not what differs.
        assert_eq!(report.facts["extension:hstore"], FactStatus::Match);
        // ...and would be, if the resolver could not install it.
        let mut without = resolver_facts.clone();
        without.available_extensions.remove("hstore");
        assert!(matches!(
            compare(
                &with_executables(target_facts.clone()),
                &with_executables(without),
                &[]
            )
            .facts["extension:hstore"],
            FactStatus::Mismatch { .. }
        ));

        // A side against itself is the positive control: every fact matches
        // and the scope verifies.
        let same = compare(
            &with_executables(target_facts.clone()),
            &with_executables(target_facts),
            &[],
        );
        assert_eq!(same.verdict(), Verdict::Verified, "{same:?}");

        drop(target);
        drop(resolver);
        current_admin
            .execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .await
            .unwrap();
        old_admin
            .execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .await
            .unwrap();
    }
}
