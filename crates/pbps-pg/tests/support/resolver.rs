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
            Observation::reported(Some(r#"["pg_catalog"]"#))
        );
        assert_eq!(
            seen_by_deployer.visibility["b"],
            Observation::reported(Some(r#"["pg_catalog","b"]"#))
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
                target: r#"["pg_catalog"]"#.into(),
                resolver: r#"["pg_catalog","a"]"#.into()
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

    /// #688: a schema name is one element of the visibility whatever it
    /// contains — a comma-separated fragment that looks like a temporary
    /// namespace, or the array NULL sentinel — because the order is taken as
    /// a JSON array and rendered by the shared renderer, not parsed out of
    /// the engine's array text. The session's own temporary namespace still
    /// goes.
    #[tokio::test]
    #[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
    async fn a_schema_name_is_one_visibility_element_whatever_it_contains() {
        let server = std::env::var("PBPS_TEST_PG_DB").unwrap();
        let database = format!("pbps_vis688_{}", std::process::id());
        let mut admin = connect(&server).await;
        admin
            .execute(&format!(
                "CREATE DATABASE {database} TEMPLATE template0 LC_COLLATE 'C' LC_CTYPE 'C'"
            ))
            .await
            .unwrap();
        let mut conn = connect(&format!("{server} dbname={database}")).await;
        conn.execute(
            "CREATE SCHEMA \"x,pg_temp_3,y\"; CREATE SCHEMA \"null\"; CREATE TEMP TABLE t (i int)",
        )
        .await
        .unwrap();
        let schemas = ["public".to_owned()];
        let extras = ["x,pg_temp_3,y".to_owned(), "null".to_owned()];
        let facts = read(
            &mut conn,
            &Scope {
                schemas: &schemas,
                write_path_extras: &extras,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            facts.visibility["public"],
            Observation::reported(Some(r#"["pg_catalog","public","x,pg_temp_3,y","null"]"#))
        );
        drop(conn);
        admin
            .execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
            .await
            .unwrap();
    }

    /// #688: the two target-side reads share one snapshot, and the
    /// transaction that gives them it ends with the call — after a completed
    /// read and after a refused one alike — so the planning connection is
    /// never left inside a block for the next read to trip over.
    #[tokio::test]
    #[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
    async fn scope_facts_end_their_snapshot_whether_the_read_completes_or_is_refused() {
        use pbps_pg::resolver::scope_facts;
        async fn isolation(conn: &mut Conn) -> String {
            conn.query("SELECT pg_catalog.current_setting('transaction_isolation') AS level")
                .await
                .unwrap()[0]
                .try_get::<&str>("level")
                .unwrap()
                .unwrap()
                .to_owned()
        }
        let server = std::env::var("PBPS_TEST_PG_DB").unwrap();
        let mut conn = connect(&server).await;
        // Outside any transaction the probe answers the session default; a
        // read's own snapshot, still open, would answer `repeatable read`.
        let outside = isolation(&mut conn).await;
        assert_ne!(outside, "repeatable read");
        let schemas = ["public".to_owned()];
        let scope = Scope {
            schemas: &schemas,
            write_path_extras: &[],
        };
        let (catalog, authorization) = scope_facts(&mut conn, &scope, &schemas).await.unwrap();
        assert!(catalog.visibility.contains_key("public"));
        assert!(authorization.schemas.contains_key("public"));
        assert_eq!(
            isolation(&mut conn).await,
            outside,
            "the snapshot outlived a completed read"
        );
        // A schema the principal cannot see refuses the read part-way; the
        // transaction still ends, so the next read on this connection is its
        // own snapshot and not a statement inside the refused one.
        let unseen = ["pbps_no_such_schema_688".to_owned()];
        scope_facts(&mut conn, &scope, &unseen)
            .await
            .expect_err("an unseen schema refuses the read");
        assert_eq!(
            isolation(&mut conn).await,
            outside,
            "the snapshot outlived a refused read"
        );
        scope_facts(&mut conn, &scope, &schemas)
            .await
            .expect("the connection reads again after a refusal");
    }
}

mod auth610 {
    use super::*;
    use pbps_pg::resolver::authorization::{AuthorizationContext, read};

    fn fingerprint(context: &AuthorizationContext) -> Vec<u8> {
        context.canonical()
    }

    #[tokio::test]
    #[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
    async fn the_deployers_authorization_is_the_engines_own_answers_not_the_admins() {
        let server = std::env::var("PBPS_TEST_PG_DB").unwrap();
        let pid = std::process::id();
        let database = format!("pbps_auth610_{pid}");
        let dep = format!("pbps_dep_{pid}");
        let owner = format!("pbps_owner_{pid}");
        let reader = format!("pbps_reader_{pid}");
        let mut admin = Conn::connect(Driver::Postgres, &server).await.unwrap();
        admin
            .execute(&format!(
                "CREATE DATABASE {database} TEMPLATE template0 LC_COLLATE 'C' LC_CTYPE 'C'"
            ))
            .await
            .unwrap();
        for role in [&owner, &reader] {
            admin
                .execute(&format!("CREATE ROLE {role} NOLOGIN"))
                .await
                .unwrap();
        }
        admin
            .execute(&format!(
                "CREATE ROLE {dep} LOGIN PASSWORD 'pbps-dep' IN ROLE {reader}; \
                 GRANT {owner} TO {dep} WITH INHERIT FALSE, SET TRUE"
            ))
            .await
            .unwrap();
        let mut setup = Conn::connect(Driver::Postgres, &format!("{server} dbname={database}"))
            .await
            .unwrap();
        setup
            .execute(&format!(
                "GRANT CONNECT ON DATABASE {database} TO {dep}; \
                 CREATE SCHEMA app AUTHORIZATION {owner}; \
                 CREATE TABLE app.t (x int); ALTER TABLE app.t OWNER TO {owner}; \
                 GRANT USAGE ON SCHEMA app TO {reader}; GRANT SELECT ON app.t TO {reader}; \
                 ALTER ROLE {dep} IN DATABASE {database} SET search_path = app, public"
            ))
            .await
            .unwrap();
        let schemas = ["app".to_owned()];

        let deployer_url = server
            .split_whitespace()
            .filter(|part| !part.starts_with("user=") && !part.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" ");
        let mut planning = Conn::connect(
            Driver::Postgres,
            &format!("{deployer_url} dbname={database} user={dep} password=pbps-dep"),
        )
        .await
        .unwrap();

        let context = read(&mut planning, &schemas).await.unwrap();
        // The deployer is the planning connection's user, not a superuser.
        assert_eq!(context.principal.effective, dep);
        assert_eq!(context.principal.login, dep);
        assert!(!context.principal.superuser);
        // The engine's own effective answers: USAGE via the inherited reader,
        // no CREATE. In-scope object privileges are #613, not captured here.
        let app = &context.schemas["app"];
        assert_eq!(app.owner, owner);
        assert!(app.privileges["USAGE"]);
        assert!(!(app.privileges["CREATE"]));
        // The owner is switchable (SET, no inherit); the reader is inherited.
        assert!(context.roles[&owner].can_set);
        assert!(!context.roles[&owner].inherits);
        assert!(context.roles[&reader].inherits);
        // The database-role search_path pin is part of the context.
        assert_eq!(
            context
                .settings
                .get("database-role:search_path")
                .map(String::as_str),
            Some("app, public")
        );

        // The setup administrator is a different context: it owns nothing here
        // but sees and may create everything, so its fingerprint differs and a
        // compilation run as it would not reproduce the deployer's binding.
        let admin_context = read(&mut setup, &schemas).await.unwrap();
        assert!(admin_context.principal.superuser);
        assert_ne!(fingerprint(&admin_context), fingerprint(&context));
        assert!(admin_context.schemas["app"].privileges["CREATE"]);

        // A granted CREATE changes the deployer's own fingerprint: authorization
        // is measured, not assumed stable.
        setup
            .execute(&format!("GRANT CREATE ON SCHEMA app TO {reader}"))
            .await
            .unwrap();
        let after = read(&mut planning, &schemas).await.unwrap();
        assert!(after.schemas["app"].privileges["CREATE"]);
        assert_ne!(fingerprint(&after), fingerprint(&context));

        drop(planning);
        drop(setup);
        admin
            .execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
            .await
            .unwrap();
        for role in [&dep, &owner, &reader] {
            admin.execute(&format!("DROP ROLE {role}")).await.unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
    async fn an_in_scope_schema_the_deployer_cannot_see_refuses_rather_than_reading_empty() {
        let server = std::env::var("PBPS_TEST_PG_DB").unwrap();
        let pid = std::process::id();
        let database = format!("pbps_auth610_hidden_{pid}");
        let dep = format!("pbps_dep_hidden_{pid}");
        let mut admin = Conn::connect(Driver::Postgres, &server).await.unwrap();
        admin
            .execute(&format!(
                "CREATE DATABASE {database} TEMPLATE template0 LC_COLLATE 'C' LC_CTYPE 'C'"
            ))
            .await
            .unwrap();
        admin
            .execute(&format!("CREATE ROLE {dep} LOGIN PASSWORD 'pbps-dep'"))
            .await
            .unwrap();
        let mut setup = Conn::connect(Driver::Postgres, &format!("{server} dbname={database}"))
            .await
            .unwrap();
        setup
            .execute(&format!("GRANT CONNECT ON DATABASE {database} TO {dep}"))
            .await
            .unwrap();
        let deployer_url = server
            .split_whitespace()
            .filter(|part| !part.starts_with("user=") && !part.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" ");
        let mut planning = Conn::connect(
            Driver::Postgres,
            &format!("{deployer_url} dbname={database} user={dep} password=pbps-dep"),
        )
        .await
        .unwrap();
        // A schema that does not exist is not visible; the read refuses instead
        // of returning a context with an empty schema map.
        assert!(
            read(&mut planning, &["nonexistent".to_owned()])
                .await
                .is_err()
        );
        drop(planning);
        drop(setup);
        admin
            .execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
            .await
            .unwrap();
        admin.execute(&format!("DROP ROLE {dep}")).await.unwrap();
    }
}

mod auth688 {
    use super::*;
    use pbps_pg::resolver::authorization::read;

    /// Role defaults apply at login, not on `SET ROLE`: after a switch the
    /// session's values are still the login's, and the effective role's own
    /// defaults apply nowhere (measured: `SHOW` agrees). The context records
    /// the login's, so a session whose two roles both pin one GUC is not
    /// reproduced with the wrong one and refused (finding on #688).
    #[tokio::test]
    #[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
    async fn role_defaults_are_the_logins_not_the_effective_roles_after_set_role() {
        let server = std::env::var("PBPS_TEST_PG_DB").unwrap();
        let pid = std::process::id();
        let database = format!("pbps_auth688_{pid}");
        let login = format!("pbps_alogin_{pid}");
        let effective = format!("pbps_aeff_{pid}");
        let mut admin = Conn::connect(Driver::Postgres, &server).await.unwrap();
        admin
            .execute(&format!(
                "CREATE DATABASE {database} TEMPLATE template0 LC_COLLATE 'C' LC_CTYPE 'C'"
            ))
            .await
            .unwrap();
        // The login's default first and the effective role's second, so a
        // read that took both under one key would keep the wrong one.
        admin
            .execute(&format!(
                "CREATE ROLE {login} LOGIN PASSWORD 'pbps-login'; \
                 CREATE ROLE {effective} NOLOGIN; GRANT {effective} TO {login}; \
                 ALTER ROLE {login} SET search_path = 'login_path'; \
                 ALTER ROLE {login} SET default_text_search_config = 'pg_catalog.simple'; \
                 ALTER ROLE {effective} SET search_path = 'effective_path'; \
                 GRANT CONNECT ON DATABASE {database} TO {login}"
            ))
            .await
            .unwrap();
        let login_url = server
            .split_whitespace()
            .filter(|part| !part.starts_with("user=") && !part.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" ");
        let mut planning = Conn::connect(
            Driver::Postgres,
            &format!("{login_url} dbname={database} user={login} password=pbps-login"),
        )
        .await
        .unwrap();
        planning
            .execute(&format!("SET ROLE {effective}"))
            .await
            .unwrap();
        let shown = planning
            .query("SELECT pg_catalog.current_setting('search_path') AS p")
            .await
            .unwrap()[0]
            .try_get::<&str>("p")
            .unwrap()
            .unwrap()
            .to_owned();
        assert_eq!(shown, "login_path", "the engine keeps the login's default");
        let context = read(&mut planning, &[]).await.unwrap();
        assert_eq!(context.principal.login, login);
        assert_eq!(context.principal.effective, effective);
        assert_eq!(
            context.settings.get("role:search_path").map(String::as_str),
            Some("login_path"),
            "{:?}",
            context.settings
        );
        // Every setting the compatibility rule compares is captured, not
        // only the loading-related few (finding on #688).
        assert_eq!(
            context
                .settings
                .get("role:default_text_search_config")
                .map(String::as_str),
            Some("pg_catalog.simple"),
            "{:?}",
            context.settings
        );
        drop(planning);
        admin
            .execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
            .await
            .unwrap();
        admin
            .execute(&format!("DROP ROLE {login}, {effective}"))
            .await
            .unwrap();
    }
}

mod recon610 {
    use super::*;
    use pbps_pg::resolver::authorization::{
        PlannedGrant, RoleMap, apply_planned, read, reconstruct, verify, with_planned,
    };

    #[tokio::test]
    #[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
    async fn scratch_reproduces_the_deployers_authorization_not_the_admins() {
        let server = std::env::var("PBPS_TEST_PG_DB").unwrap();
        let pid = std::process::id();
        let target_db = format!("pbps_recon_t_{pid}");
        let scratch_db = format!("pbps_recon_s_{pid}");
        // Distinct from auth610's role names: the suites share one process and
        // its pid, and two tests creating the same role race on "already exists".
        let dep = format!("pbps_rdep_{pid}");
        let owner = format!("pbps_rowner_{pid}");
        let reader = format!("pbps_rreader_{pid}");
        let run_login = format!("pbps_run_{pid}");
        let mut admin = Conn::connect(Driver::Postgres, &server).await.unwrap();
        for db in [&target_db, &scratch_db] {
            admin
                .execute(&format!(
                    "CREATE DATABASE {db} TEMPLATE template0 LC_COLLATE 'C' LC_CTYPE 'C'"
                ))
                .await
                .unwrap();
        }
        for role in [&owner, &reader] {
            admin
                .execute(&format!("CREATE ROLE {role} NOLOGIN"))
                .await
                .unwrap();
        }
        admin
            .execute(&format!(
                "CREATE ROLE {dep} LOGIN PASSWORD 'd' IN ROLE {reader}; \
                 GRANT {owner} TO {dep} WITH INHERIT FALSE, SET TRUE; \
                 GRANT pg_read_all_settings TO {dep}; \
                 ALTER ROLE {dep} SET search_path = \"$user\", public, \"odd name\"; \
                 ALTER ROLE {dep} SET default_text_search_config = 'pg_catalog.simple'; \
                 ALTER ROLE {dep} SET session_preload_libraries FROM CURRENT; \
                 CREATE ROLE {run_login} LOGIN PASSWORD 'r'"
            ))
            .await
            .unwrap();
        // The target: competing schemas, the deployer usable in `app` only.
        let mut setup = Conn::connect(Driver::Postgres, &format!("{server} dbname={target_db}"))
            .await
            .unwrap();
        setup
            .execute(&format!(
                "GRANT CONNECT ON DATABASE {target_db} TO {dep}; \
                 CREATE SCHEMA app AUTHORIZATION {owner}; CREATE SCHEMA secret; \
                 CREATE SCHEMA plain AUTHORIZATION {owner}; \
                 CREATE SCHEMA trimmed AUTHORIZATION {owner}; \
                 REVOKE CREATE ON SCHEMA trimmed FROM {owner}; \
                 CREATE SCHEMA mine AUTHORIZATION {dep}; \
                 REVOKE CREATE ON SCHEMA mine FROM {dep}; \
                 DROP SCHEMA public; CREATE SCHEMA public AUTHORIZATION {owner}; \
                 GRANT USAGE ON SCHEMA app TO {reader}"
            ))
            .await
            .unwrap();
        // `plain` has no grants at all: a NULL ACL, the commonest schema on
        // a real target, which reconstruction must not turn into one with
        // the owner's entry materialized (finding on #688). `trimmed` is the
        // other edge: the owner gave up part of its own default, so its ACL
        // holds less than the entry the engine materializes, and a replay
        // that only adds would leave the scratch owner able to CREATE
        // (finding on #688). `mine` is the deployer's own schema with CREATE
        // revoked from itself: the engine answers CREATE false for an owner
        // under such an ACL, and the expected context after the plan's grants
        // must say the same, not "owner, so everything" (finding on #688).
        // `public` was dropped and recreated on the target, so it has a NULL
        // ACL where the scratch database's template `public` has explicit
        // entries; the reproduction must not keep the template's (finding on
        // #688).
        let schemas = [
            "app".to_owned(),
            "secret".to_owned(),
            "plain".to_owned(),
            "trimmed".to_owned(),
            "mine".to_owned(),
            "public".to_owned(),
            // A write-path extra the engine provides: not droppable, owned
            // by the bootstrap superuser, so the reproduction leaves it
            // alone and compares the deployer's privileges on it alone
            // (finding on #688).
            "information_schema".to_owned(),
        ];
        // secret is unreadable to the deployer, so it is not an in-scope
        // schema for it; the reproduction covers only what the deployer sees.
        setup
            .execute(&format!("REVOKE ALL ON SCHEMA secret FROM PUBLIC; GRANT USAGE ON SCHEMA secret TO {dep} WITH GRANT OPTION"))
            .await
            .unwrap();

        let deployer_url = server
            .split_whitespace()
            .filter(|p| !p.starts_with("user=") && !p.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" ");
        let mut planning = Conn::connect(
            Driver::Postgres,
            &format!("{deployer_url} dbname={target_db} user={dep} password=d"),
        )
        .await
        .unwrap();
        // The deployer, holding USAGE on `secret` with the option, granted it
        // on to the reader itself: an entry whose grantor is the deployer,
        // which only the deployer's own revoke takes back (finding on #688).
        planning
            .execute(&format!("GRANT USAGE ON SCHEMA secret TO {reader}"))
            .await
            .unwrap();
        let target = read(&mut planning, &schemas).await.unwrap();
        // The deployer's login defaults: a quoted list, which must come back
        // from the reproduction as the same list and not as one element
        // spelled like it, and a compared setting outside the old shorter
        // whitelist (findings on #688).
        assert_eq!(
            target.settings.get("role:search_path").map(String::as_str),
            Some(r#""$user", public, "odd name""#),
            "{:?}",
            target.settings
        );
        assert_eq!(
            target
                .settings
                .get("role:default_text_search_config")
                .map(String::as_str),
            Some("pg_catalog.simple"),
            "{:?}",
            target.settings
        );
        // An empty list default is stored as empty text, which has no
        // spelling as an assignment; the reproduction must store the same
        // empty text, not `""` (finding on #688).
        assert_eq!(
            target
                .settings
                .get("role:session_preload_libraries")
                .map(String::as_str),
            Some(""),
            "{:?}",
            target.settings
        );
        assert!(
            target.schemas["plain"].acl.is_empty(),
            "{:?}",
            target.schemas["plain"]
        );
        assert!(
            target.schemas["public"].acl.is_empty(),
            "{:?}",
            target.schemas["public"]
        );
        assert_eq!(target.schemas["public"].owner, owner);
        assert!(target.schemas["information_schema"].privileges["USAGE"]);
        assert!(!target.schemas["information_schema"].privileges["CREATE"]);
        assert_eq!(target.schemas["mine"].owner, dep);
        assert!(target.schemas["mine"].privileges["USAGE"]);
        assert!(!target.schemas["mine"].privileges["CREATE"]);
        assert_eq!(
            target.schemas["trimmed"]
                .acl
                .get(&owner)
                .map(|grants| grants
                    .iter()
                    .map(|g| g.privilege.as_str())
                    .collect::<Vec<_>>()),
            Some(vec!["USAGE"]),
            "{:?}",
            target.schemas["trimmed"].acl
        );
        assert!(
            target.schemas["secret"].acl[&reader]
                .iter()
                .any(|g| g.privilege == "USAGE" && g.grantor == dep),
            "{:?}",
            target.schemas["secret"].acl
        );

        // Reconstruct on scratch as the admin.
        let map = RoleMap::generate(&target, &[], &run_login, &format!("t{pid}"));
        // Every run-local name is a fresh pbps_role_ identifier, never a
        // production name.
        assert!(
            map.run_local_names()
                .iter()
                .all(|n| n.starts_with("pbps_role_"))
        );
        assert!(
            !map.run_local_names()
                .iter()
                .any(|n| [&dep, &owner, &reader].contains(&n))
        );
        let mut scratch_admin =
            Conn::connect(Driver::Postgres, &format!("{server} dbname={scratch_db}"))
                .await
                .unwrap();
        reconstruct(&mut scratch_admin, &map, &target, &scratch_db)
            .await
            .unwrap();

        // The run login becomes the mapped deployer and reproduces the target.
        let mut run = Conn::connect(
            Driver::Postgres,
            &format!("{deployer_url} dbname={scratch_db} user={run_login} password=r"),
        )
        .await
        .unwrap();
        let mapped_deployer = {
            // The mapped deployer is the run-local name for the logical dep.
            let names = map.run_local_names();
            // Find it by asking the map through a reconstruction artefact:
            // the run login is a member of exactly the deployer, so SET ROLE
            // to each until current_user changes to a covered role.
            let mut found = None;
            for candidate in &names {
                if run.execute(&format!("SET ROLE {candidate}")).await.is_ok() {
                    let who = run.query("SELECT current_user AS u").await.unwrap()[0]
                        .try_get::<&str>("u")
                        .unwrap()
                        .unwrap()
                        .to_owned();
                    run.execute("RESET ROLE").await.unwrap();
                    if who == *candidate {
                        found = Some(candidate.clone());
                        break;
                    }
                }
            }
            found.expect("the run login can become the mapped deployer")
        };
        run.execute(&format!("SET ROLE {mapped_deployer}"))
            .await
            .unwrap();
        let differences = verify(&mut run, &map, &target, &schemas).await.unwrap();
        assert!(
            differences.is_empty(),
            "reproduction differed: {differences:?}"
        );
        // The deployer's membership in `pg_read_all_settings` is reproduced as
        // membership in the real predefined role, not a clone of it, so the
        // mapped deployer sees the superuser-only settings the way the target
        // deployer does (finding on #688).
        assert!(target.roles.contains_key("pg_read_all_settings"));
        let visible = run
            .query(
                "SELECT count(*)::text AS n FROM pg_catalog.pg_settings \
                 WHERE name IN ('shared_preload_libraries', 'session_preload_libraries', 'dynamic_library_path')",
            )
            .await
            .unwrap()[0]
            .try_get::<&str>("n")
            .unwrap()
            .unwrap()
            .to_owned();
        assert_eq!(
            visible, "3",
            "the superuser-only settings are visible to the mapped deployer"
        );

        // Case 16 negative control: reading as the setup administrator (no
        // SET ROLE) does not reproduce the deployer — the principal differs.
        let admin_differences = verify(&mut scratch_admin, &map, &target, &schemas)
            .await
            .unwrap();
        assert!(
            admin_differences.contains(&"principal".to_owned()),
            "{admin_differences:?}"
        );

        // The plan's grants run as the reproduced deployer, not the
        // administrator (finding on #688). A revoke of what the deployer
        // itself granted takes on scratch only if reconstruction replayed
        // that grant under the mapped deployer: the reader's USAGE on
        // `secret`, granted by the deployer above, goes, the owner-granted
        // entries stay, and the reproduction matches what the plan is meant
        // to leave behind.
        let deployer_role = map.deployer(&target).unwrap();
        let usable = [PlannedGrant {
            role: reader.clone(),
            schema: "secret".into(),
            privilege: "USAGE".into(),
            revoke: true,
        }];
        let expected = with_planned(target.clone(), &usable);
        assert!(!expected.schemas["secret"].acl.contains_key(&reader));
        assert!(expected.schemas["secret"].privileges["USAGE"]);
        apply_planned(&mut scratch_admin, &map, &deployer_role, &usable)
            .await
            .unwrap();
        let differences = verify(&mut run, &map, &expected, &schemas).await.unwrap();
        assert!(
            differences.is_empty(),
            "the deployer's own revoke did not reproduce: {differences:?}"
        );
        // One the deployer cannot make — CREATE on `app`, which it neither
        // owns nor holds the grant option for — is not an error on the
        // engine: holding some privilege on the schema, the grant is a
        // warning and a no-op. The reproduction then lacks what the plan
        // assumed, and verification refuses it, where the administrator
        // running the grant would have made it quietly succeed.
        assert!(!target.schemas["app"].privileges["CREATE"]);
        let unusable = [PlannedGrant {
            role: reader.clone(),
            schema: "app".into(),
            privilege: "CREATE".into(),
            revoke: false,
        }];
        apply_planned(&mut scratch_admin, &map, &deployer_role, &unusable)
            .await
            .unwrap();
        let after = read(&mut run, &schemas).await.unwrap();
        assert!(
            !after.schemas["app"].privileges["CREATE"],
            "a grant the deployer cannot make took effect"
        );
        let refused = verify(
            &mut run,
            &map,
            &with_planned(target.clone(), &unusable),
            &schemas,
        )
        .await
        .unwrap();
        assert!(
            refused.contains(&"schema:app:acl".to_owned()),
            "{refused:?}"
        );

        // A membership revoked on the target after qualification, one that
        // changes no schema answer (the deployer never inherited `owner`), is
        // still an authorization the reproduction has and the target no
        // longer does; requalification must name it (finding on #688).
        setup
            .execute(&format!("REVOKE {owner} FROM {dep}"))
            .await
            .unwrap();
        let narrowed = read(&mut planning, &schemas).await.unwrap();
        assert!(!narrowed.roles.contains_key(&owner));
        let stale = verify(&mut run, &map, &narrowed, &schemas).await.unwrap();
        assert!(stale.contains(&format!("role:{owner}:extra")), "{stale:?}");

        drop(planning);
        drop(setup);
        drop(run);
        drop(scratch_admin);
        for db in [&target_db, &scratch_db] {
            admin
                .execute(&format!("DROP DATABASE {db} WITH (FORCE)"))
                .await
                .unwrap();
        }
        for role in map.run_local_names() {
            admin.execute(&format!("DROP ROLE {role}")).await.unwrap();
        }
        for role in [&run_login, &dep, &owner, &reader] {
            admin.execute(&format!("DROP ROLE {role}")).await.unwrap();
        }
    }
}

mod recon610_public {
    use super::*;
    use pbps_pg::resolver::authorization::{
        PlannedGrant, RoleMap, apply_planned, read, reconstruct, verify, with_planned,
    };

    #[tokio::test]
    #[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
    async fn scratch_reproduces_the_template_public_and_refuses_an_impossible_grant() {
        let server = std::env::var("PBPS_TEST_PG_DB").unwrap();
        let pid = std::process::id();
        let target_db = format!("pbps_pub_t_{pid}");
        let scratch_db = format!("pbps_pub_s_{pid}");
        let dep = format!("pbps_pdep_{pid}");
        let run_login = format!("pbps_prun_{pid}");
        let mut admin = Conn::connect(Driver::Postgres, &server).await.unwrap();
        for db in [&target_db, &scratch_db] {
            admin
                .execute(&format!(
                    "CREATE DATABASE {db} TEMPLATE template0 LC_COLLATE 'C' LC_CTYPE 'C'"
                ))
                .await
                .unwrap();
        }
        admin
            .execute(&format!(
                "CREATE ROLE {dep} LOGIN PASSWORD 'd'; CREATE ROLE {run_login} LOGIN PASSWORD 'r'"
            ))
            .await
            .unwrap();
        admin
            .execute(&format!("GRANT CONNECT ON DATABASE {target_db} TO {dep}"))
            .await
            .unwrap();
        let schemas = ["public".to_owned()];

        let deployer_url = server
            .split_whitespace()
            .filter(|p| !p.starts_with("user=") && !p.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" ");
        let mut planning = Conn::connect(
            Driver::Postgres,
            &format!("{deployer_url} dbname={target_db} user={dep} password=d"),
        )
        .await
        .unwrap();
        // The default public schema: owned by pg_database_owner, USAGE to
        // PUBLIC, no CREATE for the deployer.
        let target = read(&mut planning, &schemas).await.unwrap();
        assert_eq!(target.schemas["public"].owner, "pg_database_owner");
        assert!(target.schemas["public"].privileges["USAGE"]);
        assert!(!target.schemas["public"].privileges["CREATE"]);
        assert!(target.schemas["public"].acl.contains_key("PUBLIC"));

        let map = RoleMap::generate(&target, &[], &run_login, &format!("p{pid}"));
        let mut scratch_admin =
            Conn::connect(Driver::Postgres, &format!("{server} dbname={scratch_db}"))
                .await
                .unwrap();
        // Reusing the existing public schema must not fail on "already exists".
        reconstruct(&mut scratch_admin, &map, &target, &scratch_db)
            .await
            .unwrap();

        let mut run = Conn::connect(
            Driver::Postgres,
            &format!("{deployer_url} dbname={scratch_db} user={run_login} password=r"),
        )
        .await
        .unwrap();
        let deployer_role = map.deployer(&target).unwrap();
        run.execute(&format!("SET ROLE \"{deployer_role}\""))
            .await
            .unwrap();
        assert!(
            verify(&mut run, &map, &target, &schemas)
                .await
                .unwrap()
                .is_empty(),
            "public schema not reproduced"
        );

        // A planned CREATE grant the deployer cannot make: it neither owns
        // `public` nor holds the grant option, so run as the reproduced
        // deployer (finding on #688) the grant is a warning and a no-op on
        // the engine, not the administrator's silent success. The expected
        // post-plan context has CREATE, the reproduction does not, and
        // verifying against the expectation refuses it.
        let planned = [PlannedGrant {
            role: dep.clone(),
            schema: "public".into(),
            privilege: "CREATE".into(),
            revoke: false,
        }];
        let expected = with_planned(target.clone(), &planned);
        assert!(expected.schemas["public"].privileges["CREATE"]);
        apply_planned(&mut scratch_admin, &map, &deployer_role, &planned)
            .await
            .unwrap();
        let after = read(&mut run, &schemas).await.unwrap();
        assert!(
            !after.schemas["public"].privileges["CREATE"],
            "a grant the deployer cannot make took effect"
        );
        let refused = verify(&mut run, &map, &expected, &schemas).await.unwrap();
        assert!(
            refused.contains(&"schema:public:acl".to_owned()),
            "{refused:?}"
        );

        drop(planning);
        drop(run);
        drop(scratch_admin);
        for db in [&target_db, &scratch_db] {
            admin
                .execute(&format!("DROP DATABASE {db} WITH (FORCE)"))
                .await
                .unwrap();
        }
        for role in map.run_local_names() {
            admin
                .execute(&format!("DROP ROLE IF EXISTS \"{role}\""))
                .await
                .unwrap();
        }
        for role in [&run_login, &dep] {
            admin.execute(&format!("DROP ROLE {role}")).await.unwrap();
        }
    }
}

mod recon610_super {
    use super::*;
    use pbps_pg::resolver::authorization::{
        PlannedGrant, RoleMap, apply_planned, read, reconstruct, verify, with_planned,
    };

    #[tokio::test]
    #[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
    async fn a_superuser_deployer_on_the_default_database_reproduces_public_owner() {
        // Mirrors the root-fixture qualify test: the deployer is a superuser
        // reading the default database's public schema, owned by
        // pg_database_owner.
        let server = std::env::var("PBPS_TEST_PG_DB").unwrap();
        let pid = std::process::id();
        let scratch_db = format!("pbps_super_s_{pid}");
        let run_login = format!("pbps_srun_{pid}");
        let mut admin = Conn::connect(Driver::Postgres, &server).await.unwrap();
        admin
            .execute(&format!(
                "CREATE DATABASE {scratch_db} TEMPLATE template0 LC_COLLATE 'C' LC_CTYPE 'C'"
            ))
            .await
            .unwrap();
        admin
            .execute(&format!("CREATE ROLE {run_login} LOGIN PASSWORD 'r'"))
            .await
            .unwrap();

        // Read as the superuser (the server's own admin user), against the
        // database the connection string names. A second schema nobody has
        // granted anything on: its ACL is NULL, and the plan's first grant on
        // it is the case where the engine materializes the owner's entries.
        let fresh = format!("pbps_fresh_{pid}");
        admin
            .execute(&format!("CREATE SCHEMA {fresh}"))
            .await
            .unwrap();
        let schemas = ["public".to_owned(), fresh.clone()];
        let mut planning = Conn::connect(Driver::Postgres, &server).await.unwrap();
        let target = read(&mut planning, &schemas).await.unwrap();
        assert!(target.principal.superuser);
        assert_eq!(target.schemas["public"].owner, "pg_database_owner");
        assert!(target.schemas[&fresh].acl.is_empty());

        let planned = [PlannedGrant {
            role: "PUBLIC".into(),
            schema: fresh.clone(),
            privilege: "USAGE".into(),
            revoke: false,
        }];
        let map = RoleMap::generate(&target, &planned, &run_login, &format!("s{pid}"));
        let mut scratch_admin =
            Conn::connect(Driver::Postgres, &format!("{server} dbname={scratch_db}"))
                .await
                .unwrap();
        reconstruct(&mut scratch_admin, &map, &target, &scratch_db)
            .await
            .unwrap();

        let mut run = Conn::connect(
            Driver::Postgres,
            &format!(
                "{} dbname={scratch_db} user={run_login} password=r",
                server
                    .split_whitespace()
                    .filter(|p| !p.starts_with("user=") && !p.starts_with("password="))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        )
        .await
        .unwrap();
        let deployer = map.deployer(&target).unwrap();
        run.execute(&format!("SET ROLE \"{deployer}\""))
            .await
            .unwrap();
        let differences = verify(&mut run, &map, &target, &schemas).await.unwrap();
        assert!(
            differences.is_empty(),
            "public owner not reproduced for a superuser deployer: {differences:?}"
        );
        // The plan's first grant on the fresh schema, run as the reproduced
        // deployer, materializes the owner's entries on scratch as it would on
        // the target; the expected context carries them too (finding on #688).
        apply_planned(&mut scratch_admin, &map, &deployer, &planned)
            .await
            .unwrap();
        let expected = with_planned(target.clone(), &planned);
        assert!(expected.schemas[&fresh].acl.contains_key("PUBLIC"));
        let after = verify(&mut run, &map, &expected, &schemas).await.unwrap();
        assert!(
            after.is_empty(),
            "a first grant on a default ACL did not reproduce: {after:?}"
        );

        drop(planning);
        drop(run);
        drop(scratch_admin);
        admin
            .execute(&format!("DROP SCHEMA {fresh}"))
            .await
            .unwrap();
        admin
            .execute(&format!("DROP DATABASE {scratch_db} WITH (FORCE)"))
            .await
            .unwrap();
        for role in map.run_local_names() {
            admin
                .execute(&format!("DROP ROLE IF EXISTS \"{role}\""))
                .await
                .unwrap();
        }
        admin
            .execute(&format!("DROP ROLE {run_login}"))
            .await
            .unwrap();
    }
}
