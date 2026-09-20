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

/// The `PBPS_TEST_DB` server as another login, in one database.
fn login_url(user: &str, password: &str, database: &str) -> String {
    let server: Vec<String> = conn_str()
        .split(';')
        .filter(|part| {
            let key = part
                .split('=')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            !matches!(
                key.as_str(),
                "user id" | "uid" | "user" | "password" | "pwd" | "database" | "initial catalog"
            ) && !part.trim().is_empty()
        })
        .map(str::to_owned)
        .collect();
    format!(
        "{};User Id={user};Password={password};Database={database}",
        server.join(";")
    )
}

mod contained611 {
    use super::*;
    use pbps_db::resolver::ScratchNames;
    use pbps_db::resolver::environment::DatabaseRecipe;
    use pbps_mssql::resolver::environment::{Scope, read};
    use pbps_mssql::resolver::{contained_databases_are_allowed, scratch_database_ddl};

    /// A partially contained target needs a scratch server that allows
    /// contained databases, which a fresh one does not. The premise is asked
    /// before anything is created and refused by the option's name; with the
    /// option on, the recipe read from the real target creates a database
    /// contained the way the target's is (finding on #611).
    ///
    /// The option is server-wide, so this test is the only one that may touch
    /// it, and it puts it back.
    #[tokio::test]
    #[ignore = "needs live SQL Server"]
    async fn a_partially_contained_target_needs_a_server_that_allows_contained_databases() {
        let mut admin = connect_live(&conn_str()).await.unwrap();
        let option = |value: u8| {
            format!("EXEC sp_configure 'contained database authentication', {value}; RECONFIGURE;")
        };
        admin.execute(&option(1)).await.unwrap();
        let pid = std::process::id();
        let target = format!("pbps_contained611_t_{pid}");
        let scratch = format!("pbps_contained611_s_{pid}");
        admin
            .execute(&format!(
                "CREATE DATABASE [{target}] CONTAINMENT = PARTIAL;"
            ))
            .await
            .unwrap();
        let mut planning = connect_live(&format!("{};Database={target}", conn_str()))
            .await
            .unwrap();
        let schemas = ["dbo".to_owned()];
        let catalog = read(&mut planning, &Scope { schemas: &schemas })
            .await
            .unwrap();
        let recipe = DatabaseRecipe::from_sql_server_catalog(&catalog).unwrap();
        assert_eq!(recipe.sql_server.as_ref().unwrap().containment, "PARTIAL");

        // Off: refused by name, before any statement could fail halfway. The
        // engine will not turn the option off while a contained database
        // exists (12818), so the target goes first; its recipe is what is kept.
        drop(planning);
        let drop_database = |database: &str| {
            format!(
                "ALTER DATABASE [{database}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; \
                 DROP DATABASE [{database}];"
            )
        };
        admin.execute(&drop_database(&target)).await.unwrap();
        admin.execute(&option(0)).await.unwrap();
        let refused = contained_databases_are_allowed(&mut admin, &recipe)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("contained database authentication"),
            "{refused}"
        );
        // A target that is not contained asks nothing of the server.
        contained_databases_are_allowed(&mut admin, &DatabaseRecipe::neutral())
            .await
            .unwrap();

        // On: allowed, and the recipe's own statements create it contained.
        admin.execute(&option(1)).await.unwrap();
        contained_databases_are_allowed(&mut admin, &recipe)
            .await
            .unwrap();
        let names = ScratchNames::new(
            scratch.clone(),
            format!("pbps_contained611_l_{pid}"),
            "0123456789abcdef0123456789abcdef".to_owned(),
        )
        .unwrap();
        for statement in scratch_database_ddl(&names, &recipe).unwrap() {
            admin.execute(&statement).await.unwrap();
        }
        let rows = admin
            .query(&format!(
                "SELECT CONVERT(nvarchar(16), containment_desc) AS containment FROM sys.databases \
                 WHERE name = N'{scratch}';"
            ))
            .await
            .unwrap();
        assert_eq!(
            rows[0].try_get::<&str>("containment").unwrap(),
            Some("PARTIAL")
        );

        admin.execute(&drop_database(&scratch)).await.unwrap();
        admin.execute(&option(0)).await.unwrap();
    }
}

mod scope611 {
    use super::*;
    use pbps_db::resolver::environment::{
        CatalogFacts, EnvironmentFacts, ExecutableIdentity, ExecutableRole, ExecutableSet,
        FactStatus, Provenance, Verdict,
    };
    use pbps_mssql::resolver::compatibility::compare;
    use pbps_mssql::resolver::environment::{Scope, read};
    use pbps_mssql::resolver::scope_facts;

    /// The catalog says nothing about processes; the rule needs an engine
    /// identity on both sides, so the same one is attached to each.
    fn with_executables(catalog: CatalogFacts) -> EnvironmentFacts {
        EnvironmentFacts {
            catalog,
            executables: ExecutableSet {
                engine: ExecutableIdentity {
                    role: ExecutableRole::Engine,
                    path: "/opt/mssql/bin/sqlservr".into(),
                    digest: Some("e1".into()),
                    provenance: Provenance::LoadedContent,
                    disk_differs_from_loaded: Some(false),
                },
                libraries: Vec::new(),
            },
        }
    }

    /// The facts are the reading session's own: a deployment login's default
    /// language decides the language, the date format and the first day of
    /// the week its statements run under, where the administrator's session
    /// reads the server's. The rule names each difference, and the database
    /// facts a scratch database must be created to match.
    #[tokio::test]
    #[ignore = "needs live SQL Server"]
    async fn scope_facts_are_the_deployment_sessions_own_and_the_rule_names_each_difference() {
        let mut db = TestDb::create("scope611").await;
        let pid = std::process::id();
        let login = format!("pbps_scope611_{pid}");
        db.conn
            .execute(&format!(
                "ALTER DATABASE [{0}] COLLATE Latin1_General_100_CS_AS; \
                 ALTER DATABASE [{0}] SET COMPATIBILITY_LEVEL = 160; \
                 ALTER DATABASE [{0}] SET ANSI_PADDING ON;",
                db.name
            ))
            .await
            .unwrap();
        db.conn
            .execute(&format!(
                "CREATE LOGIN [{login}] WITH PASSWORD = 'Pbps!Scope611', CHECK_POLICY = OFF, \
                 DEFAULT_LANGUAGE = [Deutsch]; CREATE USER [{login}] FOR LOGIN [{login}];"
            ))
            .await
            .unwrap();
        db.conn.execute("CREATE SCHEMA app;").await.unwrap();
        let mut deployer = connect_live(&login_url(&login, "Pbps!Scope611", &db.name))
            .await
            .unwrap();
        let schemas = ["app".to_owned(), "absent611".to_owned()];
        let scope = Scope { schemas: &schemas };
        let seen = read(&mut deployer, &scope).await.unwrap();
        for (key, value) in [
            ("engine_edition", "3"),
            ("database_collation", "Latin1_General_100_CS_AS"),
            ("database_compatibility_level", "160"),
            ("database_containment", "NONE"),
            ("database_ansi_padding", "1"),
            ("host_platform", "Linux"),
        ] {
            assert_eq!(
                seen.observations[key].value(),
                Some(value),
                "{key}: {seen:?}"
            );
        }
        assert!(
            seen.observations["product_version"]
                .value()
                .is_some_and(|version| version.starts_with("17.")),
            "{seen:?}"
        );
        for (name, value) in [
            ("language", "Deutsch"),
            ("dateformat", "dmy"),
            ("datefirst", "1"),
            ("quoted_identifier", "1"),
            ("ansi_nulls", "1"),
        ] {
            assert_eq!(seen.settings[name].value, value, "{name}: {seen:?}");
        }
        // A bare name binds in its own schema and then in dbo; a schema that
        // is not there is not part of the order.
        assert_eq!(seen.visibility["app"].value(), Some(r#"["app","dbo"]"#));
        assert_eq!(seen.visibility["absent611"].value(), Some(r#"["dbo"]"#));
        assert!(seen.extensions.is_empty());

        // The administrator's session in the same database speaks the
        // server's language: a resolver compiled through it would read date
        // literals differently, and the rule says so by name.
        let admin = read(&mut db.conn, &scope).await.unwrap();
        let report = compare(
            &with_executables(seen.clone()),
            &with_executables(admin),
            &[],
        );
        assert_eq!(
            report.verdict(),
            Verdict::Mismatch(vec![
                "setting:datefirst".into(),
                "setting:dateformat".into(),
                "setting:language".into(),
            ]),
            "{report:?}"
        );
        assert_eq!(
            report.facts["setting:language"],
            FactStatus::Mismatch {
                target: "Deutsch".into(),
                resolver: "us_english".into()
            }
        );
        let same = compare(
            &with_executables(seen.clone()),
            &with_executables(seen.clone()),
            &[],
        );
        assert_eq!(same.verdict(), Verdict::Verified, "{same:?}");

        // Another database on the same server, created the ordinary way: its
        // collation, compatibility level and padding default are its own.
        let mut other = TestDb::create("scope611_other").await;
        other
            .conn
            .execute(&format!("CREATE USER [{login}] FOR LOGIN [{login}];"))
            .await
            .unwrap();
        other.conn.execute("CREATE SCHEMA app;").await.unwrap();
        let mut elsewhere = connect_live(&login_url(&login, "Pbps!Scope611", &other.name))
            .await
            .unwrap();
        let plain = read(&mut elsewhere, &scope).await.unwrap();
        let report = compare(
            &with_executables(seen.clone()),
            &with_executables(plain),
            &[],
        );
        assert_eq!(
            report.verdict(),
            Verdict::Mismatch(vec![
                "database_ansi_padding".into(),
                "database_collation".into(),
                "database_compatibility_level".into(),
            ]),
            "{report:?}"
        );

        // A session that would persist QUOTED_IDENTIFIER OFF into a module is
        // not one the rule verifies, even against itself.
        deployer
            .execute("SET QUOTED_IDENTIFIER OFF;")
            .await
            .unwrap();
        let off = read(&mut deployer, &scope).await.unwrap();
        let report = compare(&with_executables(off.clone()), &with_executables(off), &[]);
        assert_eq!(
            report.verdict(),
            Verdict::Mismatch(vec!["persisted-module-settings".into()]),
            "{report:?}"
        );
        deployer.execute("SET QUOTED_IDENTIFIER ON;").await.unwrap();

        // The combined read refuses an in-scope schema that is not there
        // rather than sealing a scope with a hole in it, and reads the same
        // facts when the scope is whole.
        let refused = scope_facts(&mut deployer, &scope, &schemas)
            .await
            .expect_err("an absent in-scope schema refuses the read");
        assert!(refused.to_string().contains("absent611"), "{refused}");
        let whole = ["app".to_owned()];
        let (catalog, authorization) =
            scope_facts(&mut deployer, &Scope { schemas: &whole }, &whole)
                .await
                .unwrap();
        assert_eq!(catalog.settings["language"].value, "Deutsch");
        assert_eq!(authorization.principal.effective, login);
        assert_eq!(authorization.language, "Deutsch");

        drop(deployer);
        drop(elsewhere);
        other.drop().await;
        let mut db = db;
        db.conn
            .execute(&format!("USE master; DROP LOGIN [{login}];"))
            .await
            .ok();
        db.drop().await;
    }
}

mod editions611 {
    use super::*;
    use pbps_db::resolver::environment::{
        EnvironmentFacts, ExecutableIdentity, ExecutableRole, ExecutableSet, FactStatus,
        Provenance, Verdict,
    };
    use pbps_mssql::resolver::compatibility::compare;
    use pbps_mssql::resolver::environment::{Scope, read};

    /// Two real servers of one build: an Express instance installed under
    /// another server collation, and the suite's Developer instance. The
    /// edition is a named limitation, because binding is the same and
    /// capability is the target's to check; the server collation, which no
    /// scratch database can be created to change, is a mismatch.
    #[tokio::test]
    #[ignore = "needs a second live SQL Server; set PBPS_TEST_EXPRESS_DB (see scripts/live-tests.sh)"]
    async fn a_real_express_target_is_an_edition_limitation_and_its_server_collation_a_mismatch() {
        let express = std::env::var("PBPS_TEST_EXPRESS_DB").expect(
            "PBPS_TEST_EXPRESS_DB is not set; this test needs the Express server scripts/live-tests.sh starts",
        );
        let mut target = connect_live(&express).await.unwrap();
        let mut resolver = connect_live(&conn_str()).await.unwrap();
        let schemas = ["dbo".to_owned()];
        let scope = Scope { schemas: &schemas };
        let side = |catalog| EnvironmentFacts {
            catalog,
            executables: ExecutableSet {
                engine: ExecutableIdentity {
                    role: ExecutableRole::Engine,
                    path: "/opt/mssql/bin/sqlservr".into(),
                    digest: Some("e1".into()),
                    provenance: Provenance::LoadedContent,
                    disk_differs_from_loaded: Some(false),
                },
                libraries: Vec::new(),
            },
        };
        let target_facts = read(&mut target, &scope).await.unwrap();
        let resolver_facts = read(&mut resolver, &scope).await.unwrap();
        assert_eq!(
            target_facts.observations["engine_edition"].value(),
            Some("4")
        );
        assert_eq!(
            resolver_facts.observations["engine_edition"].value(),
            Some("3")
        );
        assert_eq!(
            target_facts.observations["product_version"],
            resolver_facts.observations["product_version"],
            "the two servers are one build"
        );
        let report = compare(&side(target_facts), &side(resolver_facts), &[]);
        assert_eq!(report.facts["engine_edition"], FactStatus::Match);
        assert_eq!(report.facts["edition"], FactStatus::Match);
        let limitation = &report.limitations["edition"];
        assert!(
            limitation.contains("Express Edition") && limitation.contains("Developer Edition"),
            "{limitation}"
        );
        // `master` takes the server's collation, so both move together.
        assert_eq!(
            report.verdict(),
            Verdict::Mismatch(vec!["database_collation".into(), "server_collation".into()]),
            "{report:?}"
        );
    }
}

mod recon611 {
    use super::*;
    use pbps_mssql::resolver::authorization::{
        PlannedGrant, PrincipalMap, apply_planned, enter, read, reconstruct, verify,
    };

    /// What the deployer holds through `public` is part of its authorization:
    /// every user is in `public` without being a member of it, so a
    /// database permission or an impersonation right granted there is in the
    /// engine's effective answer. Both are read, reproduced and verified —
    /// not left out of the rows replayed, which refused the target (finding
    /// on #611).
    #[tokio::test]
    #[ignore = "needs live SQL Server"]
    async fn what_the_deployer_holds_through_public_is_reproduced() {
        let mut target = TestDb::create("recon611p_t").await;
        let mut scratch = TestDb::create("recon611p_s").await;
        let pid = std::process::id();
        let dep = format!("pbps_pdep611_{pid}");
        let run_login = format!("pbps_prun611_{pid}");
        target
            .conn
            .execute(&format!(
                "CREATE LOGIN [{dep}] WITH PASSWORD = 'Pbps!Recon611', CHECK_POLICY = OFF; \
                 CREATE USER [{dep}] FOR LOGIN [{dep}]; CREATE USER other WITHOUT LOGIN; \
                 GRANT CREATE VIEW TO public; GRANT IMPERSONATE ON USER::other TO public;"
            ))
            .await
            .unwrap();
        let schemas = ["dbo".to_owned()];
        let mut planning = connect_live(&login_url(&dep, "Pbps!Recon611", &target.name))
            .await
            .unwrap();
        let context = read(&mut planning, &schemas).await.unwrap();
        assert!(context.database_permissions.contains("CREATE VIEW"));
        assert!(
            context
                .database_grants
                .iter()
                .any(|g| g.grantee == "public" && g.permission == "CREATE VIEW"),
            "{:?}",
            context.database_grants
        );
        assert!(context.impersonation.contains("other"));
        // `public` is a holder, not a membership to reproduce.
        assert!(!context.roles.contains_key("public"));

        scratch
            .conn
            .execute(&format!(
                "CREATE LOGIN [{run_login}] WITH PASSWORD = 'Pbps!Run611', CHECK_POLICY = OFF; \
                 ALTER AUTHORIZATION ON DATABASE::[{}] TO [{run_login}];",
                scratch.name
            ))
            .await
            .unwrap();
        let map = PrincipalMap::generate(&context, &[], &format!("p{pid}"));
        reconstruct(&mut scratch.conn, &map, &context, &run_login)
            .await
            .unwrap();
        let mut run = connect_live(&login_url(&run_login, "Pbps!Run611", &scratch.name))
            .await
            .unwrap();
        enter(&mut run, map.deployer(&context).as_deref())
            .await
            .unwrap();
        let differences = verify(&mut run, &map, &context, &schemas).await.unwrap();
        assert!(
            differences.is_empty(),
            "reproduction differed: {differences:?}"
        );
        // And a scratch side that loses it is named.
        scratch
            .conn
            .execute("REVOKE CREATE VIEW FROM public;")
            .await
            .unwrap();
        let drifted = verify(&mut run, &map, &context, &schemas).await.unwrap();
        assert!(
            drifted.contains(&"database:permissions".to_owned())
                && drifted.contains(&"database:grants".to_owned()),
            "{drifted:?}"
        );

        drop(run);
        drop(planning);
        let mut master = connect_live(&conn_str()).await.unwrap();
        target.drop().await;
        scratch.drop().await;
        for login in [&dep, &run_login] {
            master
                .execute(&format!("DROP LOGIN [{login}];"))
                .await
                .unwrap();
        }
    }

    /// A deployer granted a permission by a grant-option holder sees its own
    /// row and not the holder's: the session that reads the context cannot
    /// see why the grantor could grant. That is an ordinary target, and it is
    /// reconstructed — the grantor recorded on scratch is the mapped holder,
    /// at the schema level and at the database level — rather than refused
    /// for a row nobody could have shown it (finding on #611).
    #[tokio::test]
    #[ignore = "needs live SQL Server"]
    async fn a_grant_from_a_holder_whose_own_grant_the_deployer_cannot_see_is_reproduced() {
        let mut target = TestDb::create("recon611w_t").await;
        let mut scratch = TestDb::create("recon611w_s").await;
        let pid = std::process::id();
        let dep = format!("pbps_wdep611_{pid}");
        let run_login = format!("pbps_wrun611_{pid}");
        for statement in [
            format!(
                "CREATE LOGIN [{dep}] WITH PASSWORD = 'Pbps!Recon611', CHECK_POLICY = OFF; \
                 CREATE USER [{dep}] FOR LOGIN [{dep}]; \
                 CREATE USER app_owner WITHOUT LOGIN; CREATE USER wgo WITHOUT LOGIN; \
                 CREATE USER dbg WITHOUT LOGIN;"
            ),
            "CREATE SCHEMA app AUTHORIZATION app_owner;".to_owned(),
            "GRANT EXECUTE ON SCHEMA::app TO wgo WITH GRANT OPTION; \
             GRANT CREATE TABLE TO dbg WITH GRANT OPTION;"
                .to_owned(),
            format!(
                "EXECUTE AS USER = 'wgo'; GRANT EXECUTE ON SCHEMA::app TO [{dep}]; REVERT; \
                 EXECUTE AS USER = 'dbg'; GRANT CREATE TABLE TO [{dep}]; REVERT;"
            ),
        ] {
            target.conn.execute(&statement).await.unwrap();
        }
        let schemas = ["app".to_owned()];
        let mut planning = connect_live(&login_url(&dep, "Pbps!Recon611", &target.name))
            .await
            .unwrap();
        let context = read(&mut planning, &schemas).await.unwrap();
        // The premise, on this engine: the deployer's rows name the holders,
        // and the holders' own rows are not among them.
        let seen = |grants: &[pbps_mssql::resolver::authorization::Grant]| {
            grants
                .iter()
                .map(|g| format!("{} {} by {}", g.grantee, g.permission, g.grantor))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            seen(&context.schemas["app"].grants),
            [format!("{dep} EXECUTE by wgo")]
        );
        assert!(
            seen(&context.database_grants).contains(&format!("{dep} CREATE TABLE by dbg")),
            "{:?}",
            context.database_grants
        );
        assert!(
            context
                .database_grants
                .iter()
                .all(|g| g.grantee != "dbg" && g.grantee != "wgo")
        );

        scratch
            .conn
            .execute(&format!(
                "CREATE LOGIN [{run_login}] WITH PASSWORD = 'Pbps!Run611', CHECK_POLICY = OFF; \
                 ALTER AUTHORIZATION ON DATABASE::[{}] TO [{run_login}];",
                scratch.name
            ))
            .await
            .unwrap();
        let map = PrincipalMap::generate(&context, &[], &format!("w{pid}"));
        reconstruct(&mut scratch.conn, &map, &context, &run_login)
            .await
            .unwrap();
        let mut run = connect_live(&login_url(&run_login, "Pbps!Run611", &scratch.name))
            .await
            .unwrap();
        enter(&mut run, map.deployer(&context).as_deref())
            .await
            .unwrap();
        let differences = verify(&mut run, &map, &context, &schemas).await.unwrap();
        assert!(
            differences.is_empty(),
            "reproduction differed: {differences:?}"
        );
        // Recorded under the mapped holder, not under the owner that enabled it.
        let grantors = scratch
            .conn
            .query(
                "SELECT USER_NAME(grantor_principal_id) AS grantor FROM sys.database_permissions \
                 WHERE state = 'G' AND permission_name IN (N'EXECUTE', N'CREATE TABLE') \
                   AND USER_NAME(grantee_principal_id) LIKE N'pbps[_]principal[_]%';",
            )
            .await
            .unwrap();
        let grantors: Vec<String> = grantors
            .iter()
            .map(|row| {
                row.try_get::<&str>("grantor")
                    .unwrap()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        assert_eq!(grantors.len(), 2, "{grantors:?}");
        assert!(
            grantors
                .iter()
                .all(|name| name.starts_with("pbps_principal_")),
            "{grantors:?}"
        );

        drop(run);
        drop(planning);
        let mut master = connect_live(&conn_str()).await.unwrap();
        target.drop().await;
        scratch.drop().await;
        for login in [&dep, &run_login] {
            master
                .execute(&format!("DROP LOGIN [{login}];"))
                .await
                .unwrap();
        }
    }

    /// The deployer's authorization — default schema, roles, a DENY, an
    /// impersonation right, the grant option it holds — is reproduced in a
    /// scratch database with run-local principals and verified by reading it
    /// back as the reproduced deployer; the scratch owner's own view is not
    /// that, and a change to the reproduction is named.
    #[tokio::test]
    #[ignore = "needs live SQL Server"]
    async fn scratch_reproduces_the_deployers_authorization_not_the_scratch_owners() {
        let mut target = TestDb::create("recon611_t").await;
        let mut scratch = TestDb::create("recon611_s").await;
        let pid = std::process::id();
        let dep = format!("pbps_rdep611_{pid}");
        let run_login = format!("pbps_rrun611_{pid}");
        target
            .conn
            .execute(&format!(
                "CREATE LOGIN [{dep}] WITH PASSWORD = 'Pbps!Recon611', CHECK_POLICY = OFF, \
                 DEFAULT_LANGUAGE = [Deutsch]; \
                 CREATE USER [{dep}] FOR LOGIN [{dep}] WITH DEFAULT_SCHEMA = app; \
                 CREATE USER app_owner WITHOUT LOGIN; CREATE USER other WITHOUT LOGIN; \
                 CREATE USER auditors WITHOUT LOGIN; \
                 CREATE ROLE readers; ALTER ROLE readers ADD MEMBER [{dep}]; \
                 ALTER ROLE db_ddladmin ADD MEMBER [{dep}];"
            ))
            .await
            .unwrap();
        for schema in ["app", "denied"] {
            target
                .conn
                .execute(&format!("CREATE SCHEMA {schema} AUTHORIZATION app_owner;"))
                .await
                .unwrap();
        }
        target
            .conn
            .execute(&format!(
                "GRANT SELECT ON SCHEMA::app TO readers; \
                 GRANT REFERENCES ON SCHEMA::app TO [{dep}] WITH GRANT OPTION; \
                 GRANT SELECT ON SCHEMA::denied TO readers; \
                 DENY SELECT ON SCHEMA::denied TO [{dep}]; \
                 GRANT ALTER ON SCHEMA::dbo TO [{dep}]; \
                 GRANT CREATE TABLE TO [{dep}]; \
                 GRANT IMPERSONATE ON USER::other TO [{dep}];"
            ))
            .await
            .unwrap();
        let schemas = ["app".to_owned(), "denied".to_owned(), "dbo".to_owned()];
        let mut planning = connect_live(&login_url(&dep, "Pbps!Recon611", &target.name))
            .await
            .unwrap();
        let context = read(&mut planning, &schemas).await.unwrap();
        assert_eq!(context.principal.effective, dep);
        assert!(!context.principal.superuser);
        assert_eq!(context.default_schema, "app");
        assert_eq!(context.language, "Deutsch");
        assert_eq!(
            context
                .roles
                .iter()
                .map(|(n, f)| (n.as_str(), *f))
                .collect::<Vec<_>>(),
            [("db_ddladmin", true), ("readers", false)]
        );
        assert!(context.impersonation.contains("other"));
        assert_eq!(context.schemas["app"].owner, "app_owner");
        assert!(context.schemas["app"].permissions.contains("SELECT"));
        assert!(context.schemas["app"].permissions.contains("REFERENCES"));
        // The DENY wins over the role's grant in the engine's own answer.
        assert!(!context.schemas["denied"].permissions.contains("SELECT"));
        assert!(
            context.schemas["denied"]
                .grants
                .iter()
                .any(|g| g.grantee == dep && g.state == "DENY"),
            "{:?}",
            context.schemas["denied"].grants
        );
        assert!(context.database_permissions.contains("CREATE TABLE"));

        // The scratch database belongs to a run login of its own, as the
        // dedicated-server path creates it.
        scratch
            .conn
            .execute(&format!(
                "CREATE LOGIN [{run_login}] WITH PASSWORD = 'Pbps!Run611', CHECK_POLICY = OFF; \
                 ALTER AUTHORIZATION ON DATABASE::[{}] TO [{run_login}];",
                scratch.name
            ))
            .await
            .unwrap();
        let usable = [PlannedGrant {
            principal: "auditors".into(),
            schema: "app".into(),
            permission: "REFERENCES".into(),
            revoke: false,
        }];
        let map = PrincipalMap::generate(&context, &usable, &format!("t{pid}"));
        assert!(
            map.run_local_names()
                .iter()
                .all(|name| name.starts_with("pbps_principal_"))
        );
        reconstruct(&mut scratch.conn, &map, &context, &run_login)
            .await
            .unwrap();
        // No production principal exists on scratch.
        let leaked = scratch
            .conn
            .query(&format!(
                "SELECT name FROM sys.database_principals WHERE name IN (N'{dep}', N'app_owner', N'readers', N'other', N'auditors');"
            ))
            .await
            .unwrap();
        assert!(leaked.is_empty(), "production names reached scratch");

        let mut run = connect_live(&login_url(&run_login, "Pbps!Run611", &scratch.name))
            .await
            .unwrap();
        // The scratch owner's own view is dbo's, which is not the deployer's.
        let as_owner = verify(&mut run, &map, &context, &schemas).await.unwrap();
        assert!(
            as_owner.contains(&"principal".to_owned())
                && as_owner.contains(&"database:permissions".to_owned()),
            "{as_owner:?}"
        );
        let deployer = map.deployer(&context);
        assert!(deployer.is_some());
        enter(&mut run, deployer.as_deref()).await.unwrap();
        // Every later check enters again. On a session already running as
        // the deployer that is a no-op, not one more impersonation context on
        // the stack: the engine refuses the 33rd.
        for _ in 0..40 {
            enter(&mut run, deployer.as_deref()).await.unwrap();
        }
        let differences = verify(&mut run, &map, &context, &schemas).await.unwrap();
        assert!(
            differences.is_empty(),
            "reproduction differed: {differences:?}"
        );

        // A grant the deployer holds the option for takes on scratch, run as
        // the deployer on the session that entered it; one it holds no right
        // to grant is the engine's refusal, not the scratch owner's success.
        apply_planned(&mut run, &map, &usable).await.unwrap();
        let granted = scratch
            .conn
            .query(
                "SELECT USER_NAME(p.grantor_principal_id) AS grantor FROM sys.database_permissions p \
                 WHERE p.class = 3 AND p.permission_name = N'REFERENCES' AND p.state = 'G' \
                   AND p.major_id = SCHEMA_ID(N'app');",
            )
            .await
            .unwrap();
        assert_eq!(
            granted[0]
                .try_get::<&str>("grantor")
                .unwrap()
                .map(str::to_owned),
            deployer,
            "the planned grant is recorded under the reproduced deployer"
        );
        let unusable = [PlannedGrant {
            principal: "auditors".into(),
            schema: "app".into(),
            permission: "CONTROL".into(),
            revoke: false,
        }];
        let refused = apply_planned(&mut run, &map, &unusable)
            .await
            .expect_err("a grant the deployer cannot make is refused");
        // By number, not by text: the scratch session speaks the deployer's
        // language, so the engine's sentence is in German here.
        assert!(refused.to_string().contains("15151"), "{refused}");

        // A reproduction that drifts is named: a DENY the target does not
        // have takes SELECT away from the deployer, and both the engine's
        // effective answer and the grant rows say so.
        scratch
            .conn
            .execute(&format!(
                "DENY SELECT ON SCHEMA::app TO [{}];",
                deployer.clone().unwrap()
            ))
            .await
            .unwrap();
        let drifted = verify(&mut run, &map, &context, &schemas).await.unwrap();
        assert!(
            drifted.contains(&"schema:app:permissions".to_owned())
                && drifted.contains(&"schema:app:grants".to_owned()),
            "{drifted:?}"
        );

        drop(run);
        drop(planning);
        let mut master = connect_live(&conn_str()).await.unwrap();
        target.drop().await;
        scratch.drop().await;
        for login in [&dep, &run_login] {
            master
                .execute(&format!("DROP LOGIN [{login}];"))
                .await
                .unwrap();
        }
    }

    /// A deployer that is the database owner — which a member of `sysadmin`
    /// is in every database — is reproduced by the run login itself, which
    /// owns its scratch database: no impersonation, and nothing to clone.
    #[tokio::test]
    #[ignore = "needs live SQL Server"]
    async fn a_dbo_deployer_is_reproduced_by_the_scratch_owner_itself() {
        let mut target = TestDb::create("recon611_dbo_t").await;
        let mut scratch = TestDb::create("recon611_dbo_s").await;
        let pid = std::process::id();
        let run_login = format!("pbps_rdbo611_{pid}");
        target.conn.execute("CREATE SCHEMA app;").await.unwrap();
        let schemas = ["app".to_owned(), "dbo".to_owned()];
        let context = read(&mut target.conn, &schemas).await.unwrap();
        assert!(context.principal.superuser);
        assert_eq!(context.principal.effective, "dbo");
        assert!(context.roles.is_empty() && context.database_grants.is_empty());
        assert!(context.schemas["app"].permissions.contains("CONTROL"));

        scratch
            .conn
            .execute(&format!(
                "CREATE LOGIN [{run_login}] WITH PASSWORD = 'Pbps!Run611', CHECK_POLICY = OFF; \
                 ALTER AUTHORIZATION ON DATABASE::[{}] TO [{run_login}];",
                scratch.name
            ))
            .await
            .unwrap();
        let map = PrincipalMap::generate(&context, &[], &format!("t{pid}"));
        assert_eq!(map.deployer(&context), None);
        assert!(map.run_local_names().is_empty());
        reconstruct(&mut scratch.conn, &map, &context, &run_login)
            .await
            .unwrap();
        let mut run = connect_live(&login_url(&run_login, "Pbps!Run611", &scratch.name))
            .await
            .unwrap();
        enter(&mut run, None).await.unwrap();
        let differences = verify(&mut run, &map, &context, &schemas).await.unwrap();
        assert!(
            differences.is_empty(),
            "reproduction differed: {differences:?}"
        );
        drop(run);
        let mut master = connect_live(&conn_str()).await.unwrap();
        target.drop().await;
        scratch.drop().await;
        master
            .execute(&format!("DROP LOGIN [{run_login}];"))
            .await
            .unwrap();
    }
}
