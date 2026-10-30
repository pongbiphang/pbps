//! The target's readable catalog history is independent of endpoint equality.

use super::*;

const EXTENSION: &str = "CREATE SCHEMA pbps_history695; CREATE EXTENSION pbps_history695 VERSION '1.0' SCHEMA pbps_history695;";
const ACL: &[&str] = &[
    "GRANT USAGE ON SCHEMA pbps_evidence1274 TO public",
    "REVOKE USAGE ON SCHEMA pbps_evidence1274 FROM public",
];

struct Case {
    observer: Fixture,
    database: String,
    target: String,
    scratch: String,
    login: Option<String>,
    inputs: Inputs,
    key: ProjectKey,
}

impl Case {
    async fn opening(&self, externals: Option<&str>) -> super::super::super::Unsealed {
        let (result, _) = Box::pin(produce_unsealed(
            &self.scratch,
            &self.target,
            &self.inputs,
            &self.key,
            externals,
        ))
        .await;
        result.unwrap_or_else(|error| panic!("opening run: {error}"))
    }

    async fn facts(
        &self,
    ) -> (
        pbps_db::resolver::environment::CatalogFacts,
        crate::resolver::scope::Authorization,
    ) {
        let schemas = vec![cases::SCHEMA.to_owned()];
        let mut reader = Conn::connect(Driver::Postgres, &self.target).await.unwrap();
        crate::resolver::scope::read_target(
            &mut reader,
            Driver::Postgres,
            &schemas,
            &[],
            &schemas,
            &[],
        )
        .await
        .unwrap()
    }

    async fn witness(&self) -> pbps_pg::resolver::scope_witness::Witness {
        let mut reader = Conn::connect(Driver::Postgres, &self.target).await.unwrap();
        let schemas = vec![cases::SCHEMA.to_owned()];
        let scope = pbps_pg::resolver::environment::Scope {
            schemas: &schemas,
            write_path_extras: &[],
        };
        pbps_pg::resolver::witnessed_scope_facts(&mut reader, &scope, &schemas)
            .await
            .unwrap()
            .2
    }

    async fn run(&self, statements: &[&str]) {
        self.observer.run(&self.database, statements).await;
    }

    async fn restored(&self, statements: &[&str], named: &str) {
        let before = self.facts().await;
        let unsealed = self.opening(None).await;
        self.run(statements).await;
        let after = self.facts().await;
        assert_eq!(before.0, after.0, "reported endpoint facts were restored");
        assert!(
            before.1.same_context(&after.1),
            "endpoint authorization was restored"
        );
        let refused = match Box::pin(recheck(unsealed, &self.target)).await {
            Ok(_) => panic!("a restored readable catalog row must refuse publication: {named}"),
            Err((reason, _)) => reason,
        };
        assert!(refused.contains(named), "catalog row is named: {refused}");
        assert!(!refused.contains("not a recheck refusal"), "{refused}");
        println!("PASS readable surviving-row restoration: {named}");
    }

    async fn supported(&self, statements: &[&str]) {
        let unsealed = self.opening(None).await;
        self.run(statements).await;
        let rechecked = Box::pin(recheck(unsealed, &self.target))
            .await
            .unwrap_or_else(|(reason, _)| panic!("supported ordinary plan: {reason}"));
        let inputs = &self.inputs;
        let changes = rechecked.changes().clone();
        let plan = rechecked
            .seal(changes, inputs.base(), inputs.desired())
            .unwrap();
        let limitation = plan
            .evidence
            .qualification()
            .limitations
            .get("target.role-history");
        if self.login.is_some() {
            let limitation = limitation
                .expect("ordinary login names unreadable role history")
                .raw();
            assert!(limitation.contains("checked by value") && limitation.contains("DEC-695.1"));
        } else {
            assert!(
                limitation.is_none(),
                "readable role versions need no limitation"
            );
        }
        // The persisted evidence describes the limit, never the physical carrier.
        let saved = serde_json::to_string(&plan.evidence).unwrap();
        for private in ["xmin", "role_versions", "pg_snapshot", "scope_witness"] {
            assert!(
                !saved.contains(&format!("\"{private}\":")),
                "physical witness is not saved: {private}"
            );
        }
        println!("PASS supported plan and named role-history coverage");
    }

    async fn engine_restored(&self, statements: &[&str], named: &str) {
        let before = self.facts().await;
        let history = self.witness().await;
        self.run(statements).await;
        let after = self.facts().await;
        assert_eq!(
            before.0, after.0,
            "reported endpoint facts were restored: {named}"
        );
        assert!(
            before.1.same_context(&after.1),
            "authorization was restored: {named}"
        );
        let changed = history.differences(&self.witness().await).unwrap();
        assert!(
            changed.iter().any(|s| s.contains(named)),
            "surviving row is named: {changed:?}"
        );
        println!("PASS engine surviving-row restoration: {named}");
    }

    async fn extension(&self) {
        self.run(&[EXTENSION]).await;
        let before = self.facts().await;
        let unsealed = self.opening(Some(EXTENSION)).await;
        for (version, value) in [("2.0", 2), ("1.0", 1)] {
            self.run(&[&format!(
                "ALTER EXTENSION pbps_history695 UPDATE TO '{version}'"
            )])
            .await;
            let mut admin = Conn::connect(Driver::Postgres, &self.observer.on(&self.database))
                .await
                .unwrap();
            let rows = admin.query("SELECT extversion::text AS version, pbps_history695.pbps_history695_value()::text AS value FROM pg_catalog.pg_extension WHERE extname='pbps_history695'").await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].try_get::<&str>("version").unwrap(), Some(version));
            assert_eq!(
                rows[0].try_get::<&str>("value").unwrap(),
                Some(value.to_string().as_str())
            );
        }
        let after = self.facts().await;
        assert_eq!(
            before.0, after.0,
            "real UPDATE/back restores extension facts"
        );
        assert!(before.1.same_context(&after.1));
        let refused = match Box::pin(recheck(unsealed, &self.target)).await {
            Ok(_) => panic!("a real extension UPDATE/back must refuse publication"),
            Err((reason, _)) => reason,
        };
        assert!(
            refused.contains("pg_extension") && refused.contains("pbps_history695"),
            "{refused}"
        );
        println!("PASS real extension UPDATE 1.0 -> 2.0 -> 1.0");
    }

    // Bound nested test-future stack use on both normal and live-test profiles.
    async fn cases(&self) {
        Box::pin(self.supported(&[])).await;
        Box::pin(self.supported(&["BEGIN", ACL[0], ACL[1], "ROLLBACK"])).await;
        Box::pin(self.restored(ACL, "pg_namespace")).await;
        Box::pin(self.restored(&["BEGIN", ACL[0], ACL[1], "COMMIT"], "pg_namespace")).await;
        if let Some(login) = &self.login {
            // The measured inaccessible history remains the approved limitation;
            // a standing role change is still checked by public role values.
            Box::pin(self.supported(&[
                &format!("ALTER ROLE {login} NOINHERIT"),
                &format!("ALTER ROLE {login} INHERIT"),
            ]))
            .await;
            let unsealed = self.opening(None).await;
            self.run(&[&format!("ALTER ROLE {login} BYPASSRLS")]).await;
            let refused = match Box::pin(recheck(unsealed, &self.target)).await {
                Ok(_) => panic!("a standing public role change must refuse"),
                Err((reason, _)) => reason,
            };
            self.run(&[&format!("ALTER ROLE {login} NOBYPASSRLS")])
                .await;
            assert!(refused.contains("authorization"), "{refused}");
            println!("PASS ordinary public role value check");
        } else {
            // Use our own scratch login instead of touching postgres.
            let mut scratch = Conn::connect(Driver::Postgres, &self.scratch)
                .await
                .unwrap();
            let rows = scratch
                .query("SELECT current_user::text AS name")
                .await
                .unwrap();
            let role = rows[0].try_get::<&str>("name").unwrap().unwrap().to_owned();
            drop(scratch);
            Box::pin(self.restored(
                &[
                    &format!("ALTER ROLE {role} NOINHERIT"),
                    &format!("ALTER ROLE {role} INHERIT"),
                ],
                "pg_authid",
            ))
            .await;
            Box::pin(self.engine_restored(
                &[
                    &format!("ALTER DATABASE {} CONNECTION LIMIT 19", self.database),
                    &format!("ALTER DATABASE {} CONNECTION LIMIT -1", self.database),
                ],
                "pg_database",
            ))
            .await;
            self.run(&[&format!(
                "ALTER DATABASE {} SET array_nulls TO on",
                self.database
            )])
            .await;
            Box::pin(self.engine_restored(
                &[
                    &format!("ALTER DATABASE {} SET array_nulls TO off", self.database),
                    &format!("ALTER DATABASE {} SET array_nulls TO on", self.database),
                ],
                "pg_db_role_setting",
            ))
            .await;
            self.run(&[
                "CREATE SCHEMA pbps_history_extra",
                "CREATE COLLATION pbps_history_extra.c (provider=libc, locale='C')",
            ])
            .await;
            Box::pin(self.engine_restored(
                &[
                    "ALTER COLLATION pbps_history_extra.c RENAME TO renamed",
                    "ALTER COLLATION pbps_history_extra.renamed RENAME TO c",
                ],
                "pg_collation",
            ))
            .await;
            Box::pin(self.engine_restored(
                &[
                    "DROP COLLATION pbps_history_extra.c",
                    "CREATE COLLATION pbps_history_extra.c (provider=libc, locale='C')",
                ],
                "pg_collation",
            ))
            .await;
            Box::pin(self.engine_restored(
                &[
                    "ALTER SCHEMA pbps_history_extra RENAME TO pbps_history_renamed",
                    "ALTER SCHEMA pbps_history_renamed RENAME TO pbps_history_extra",
                ],
                "pg_namespace",
            ))
            .await;
            // This pre-existing edge is in a superuser's authorization closure.
            Box::pin(self.engine_restored(
                &[
                    &format!("GRANT pg_read_all_settings TO {role} WITH ADMIN OPTION"),
                    &format!("REVOKE ADMIN OPTION FOR pg_read_all_settings FROM {role}"),
                ],
                "pg_auth_members",
            ))
            .await;
            self.run(&["CREATE EXTENSION fuzzystrmatch SCHEMA pbps_history_extra"])
                .await;
            Box::pin(self.engine_restored(
                &[
                    "ALTER FUNCTION pbps_history_extra.soundex(text) COST 2",
                    "ALTER FUNCTION pbps_history_extra.soundex(text) COST 1",
                ],
                "pg_proc",
            ))
            .await;
            Box::pin(self.engine_restored(
                &[
                    "ALTER EXTENSION fuzzystrmatch DROP FUNCTION pbps_history_extra.soundex(text)",
                    "ALTER EXTENSION fuzzystrmatch ADD FUNCTION pbps_history_extra.soundex(text)",
                ],
                "pg_depend",
            ))
            .await;
            // Restore the declared environment before the full extension run.
            self.run(&[
                "DROP EXTENSION fuzzystrmatch",
                "DROP SCHEMA pbps_history_extra CASCADE",
                &format!("ALTER DATABASE {} RESET array_nulls", self.database),
            ])
            .await;
            Box::pin(self.extension()).await;
        }
    }
}

pub(super) async fn readable_catalog_history_is_not_endpoint_equality() {
    for server in SERVERS {
        for ordinary in [false, true] {
            let mut fixture = Fixture::new(server);
            let login = if ordinary {
                let login = fixture
                    .login("d695", "NOSUPERUSER NOCREATEDB NOCREATEROLE")
                    .await;
                fixture
                    .admin()
                    .await
                    .execute(&format!("GRANT pg_read_all_settings TO {}", login.0))
                    .await
                    .unwrap();
                Some(login)
            } else {
                None
            };
            let database = fixture
                .database("history695", login.as_ref().map(|l| l.0.as_str()))
                .await;
            let target = login
                .as_ref()
                .map(|l| fixture.as_login(&database, l))
                .unwrap_or_else(|| fixture.on(&database));
            let mut setup = Conn::connect(Driver::Postgres, &target).await.unwrap();
            for sql in cases::TARGET_SETUP {
                setup.execute(sql).await.unwrap();
            }
            drop(setup);
            // A NULL ACL and an explicit owner-only ACL are the same policy,
            // but need not have the same storage. Establish the latter first.
            fixture.run(&database, ACL).await;
            let (scratch_login, scratch_db) = fixture.confined().await;
            let case = Case {
                observer: Fixture {
                    server: fixture.server.clone(),
                    token: fixture.token.clone(),
                    databases: Vec::new(),
                    roles: Vec::new(),
                },
                database,
                target,
                scratch: fixture.as_login(&scratch_db, &scratch_login),
                login: login.map(|l| l.0),
                inputs: Inputs::overload(),
                key: ProjectKey::new(true),
            };
            // Only the outer fixture owns cleanup. Revert-witness assertion
            // failures in the observer must not leave databases or logins.
            let result = tokio::spawn(async move { Box::pin(case.cases()).await }).await;
            fixture.drop().await;
            result.unwrap();
            println!("PASS {server} ordinary={ordinary} history cases and cleanup");
        }
    }
}
