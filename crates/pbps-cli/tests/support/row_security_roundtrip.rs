//! The remaining #1172 acceptance cases compare the engine's source with a
//! bootstrap. Catalog-only equality cannot prove that a policy enforces access.

use crate::{
    Demo, OwnDatabase, approved_apply, code, connected_artifact, latest_snapshot, on_server,
    psql_script_in, scalar, stderr, stdout, succeeds, try_on_server,
};
use pbps_db::{Conn, DbError, Driver};
use pbps_model::{PolicyCommand, PolicyRoles, RowSecurity, TableName};
use std::time::Instant;

pub(super) fn versions(mut check: impl FnMut(&str, &str)) {
    let mut failed = Vec::new();
    for (variable, container, expected) in [
        ("PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_OLD_CONTAINER", 16),
        ("PBPS_TEST_PG_DB", "PBPS_TEST_PG_CONTAINER", 18),
    ] {
        let base = std::env::var(variable).expect("both live PostgreSQL fixtures are required");
        let version = scalar(
            &base,
            "SELECT current_setting('server_version_num')::bigint",
        );
        assert_eq!(version / 10000, expected);
        let started = Instant::now();
        // Keep the other engine observable when an acceptance case fails;
        // unwinding the closure first also exercises fixture cleanup.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            check(&base, container);
        }));
        eprintln!("row-security PostgreSQL {version}: {:?}", started.elapsed());
        if result.is_err() {
            failed.push(version);
        }
    }
    assert!(
        failed.is_empty(),
        "row-security acceptance failed on {failed:?}"
    );
}

pub(super) struct Roles {
    server: String,
    names: Vec<String>,
}

impl Roles {
    pub(super) fn new(server: &str) -> Self {
        Self {
            server: server.into(),
            names: Vec::new(),
        }
    }

    pub(super) fn create(&mut self, suffix: &str, options: &str) -> String {
        let name = format!("pbps_1172d_{suffix}_{}", std::process::id());
        on_server(
            &self.server,
            &format!("CREATE ROLE \"{name}\" NOSUPERUSER NOBYPASSRLS {options}"),
        );
        self.names.push(name.clone());
        name
    }
}

impl Drop for Roles {
    fn drop(&mut self) {
        // Construct this guard before the databases: their ACLs, policies and
        // ownership must be gone before a cluster-scoped recipient can go away.
        let mut errors = Vec::new();
        for name in self.names.iter().rev() {
            if let Err(error) = try_on_server(&self.server, &format!("DROP ROLE \"{name}\"")) {
                errors.push(error);
            }
        }
        if std::thread::panicking() {
            for error in errors {
                eprintln!("role cleanup: {error}");
            }
        } else {
            assert!(errors.is_empty(), "role cleanup: {errors:?}");
        }
    }
}

fn loaded(demo: &Demo) -> pbps_model::Schema {
    pbps_load::load_schema_dir(&demo.dir.join("schema"))
        .unwrap()
        .schema
}

pub(super) fn assert_noop(demo: &Demo, connection: &str) {
    succeeds(demo.run(&["verify", "--db", connection]));
    let out = succeeds(demo.run(&["plan", "--db", connection, "--format", "json"]));
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(json["data"]["changes"], 0, "{json}");
}

#[test]
#[ignore = "needs both live PostgreSQL versions"]
fn row_security_d_pull_bootstrap_pull_preserves_every_policy_shape() {
    versions(|base, _| {
        let mut roles = Roles::new(base);
        let first = roles.create("roundtrip_a", "");
        let second = roles.create("roundtrip_B", "");
        let source = OwnDatabase::new(base, "rls_d_roundtrip_source");
        let fresh = OwnDatabase::new(base, "rls_d_roundtrip_fresh");
        let mut setup = String::from("CREATE SCHEMA app;");
        let commands = [
            PolicyCommand::All,
            PolicyCommand::Select,
            PolicyCommand::Insert,
            PolicyCommand::Update,
            PolicyCommand::Delete,
        ];
        let mut expected = Vec::new();
        for command in commands {
            for restrictive in [false, true] {
                for named in [false, true] {
                    for bare in [false, true] {
                        let name = format!(
                            "{}_{}_{}_{}",
                            command.as_str(),
                            u8::from(restrictive),
                            u8::from(named),
                            u8::from(bare)
                        );
                        let target = if named {
                            format!("\"{second}\", \"{first}\"")
                        } else {
                            "PUBLIC".into()
                        };
                        let using = !bare && command != PolicyCommand::Insert;
                        let with_check = !bare
                            && matches!(
                                command,
                                PolicyCommand::All | PolicyCommand::Insert | PolicyCommand::Update
                            );
                        setup.push_str(&format!(
                            "CREATE TABLE app.{name} (id integer PRIMARY KEY, tenant integer);
                            ALTER TABLE app.{name} ENABLE ROW LEVEL SECURITY;
                            CREATE POLICY gate ON app.{name} AS {} FOR {} TO {target}{}{};",
                            if restrictive {
                                "RESTRICTIVE"
                            } else {
                                "PERMISSIVE"
                            },
                            command.as_str(),
                            if using { " USING (tenant > 0)" } else { "" },
                            if with_check {
                                " WITH CHECK (tenant > 1)"
                            } else {
                                ""
                            }
                        ));
                        expected.push((name, command, restrictive, named, using, with_check));
                    }
                }
            }
        }
        setup.push_str("CREATE TABLE app.ordinary (id integer);
            CREATE TABLE app.default_deny (id integer); ALTER TABLE app.default_deny ENABLE ROW LEVEL SECURITY;
            CREATE TABLE app.dormant (id integer); CREATE POLICY bare ON app.dormant;
            CREATE TABLE app.force_only (id integer); ALTER TABLE app.force_only FORCE ROW LEVEL SECURITY;
            CREATE TABLE app.enabled_forced (id integer); ALTER TABLE app.enabled_forced ENABLE ROW LEVEL SECURITY;
            ALTER TABLE app.enabled_forced FORCE ROW LEVEL SECURITY;");
        on_server(source.connection(), &setup);
        let demo = Demo::new("rls-d-roundtrip-source");
        succeeds(demo.run(&["pull", "--db", source.connection()]));
        let before = loaded(&demo);
        assert_eq!(before.tables.len(), 45);
        for (name, command, restrictive, named, using, with_check) in expected {
            let table = &before.tables[&TableName::new("app", name)];
            assert_eq!(
                table.row_security,
                RowSecurity {
                    enabled: true,
                    forced: false
                }
            );
            let policy = &table.row_policies["gate"];
            assert_eq!(policy.command, command);
            assert_eq!(policy.restrictive, restrictive);
            assert_eq!(policy.using.is_some(), using);
            assert_eq!(policy.with_check.is_some(), with_check);
            assert_eq!(
                policy.to,
                if named {
                    PolicyRoles::Roles([first.clone().into(), second.clone().into()].into())
                } else {
                    PolicyRoles::Public
                }
            );
        }
        for (name, enabled, forced, policies) in [
            ("ordinary", false, false, 0),
            ("default_deny", true, false, 0),
            ("dormant", false, false, 1),
            ("force_only", false, true, 0),
            ("enabled_forced", true, true, 0),
        ] {
            let table = &before.tables[&TableName::new("app", name)];
            assert_eq!(table.row_security, RowSecurity { enabled, forced });
            assert_eq!(table.row_policies.len(), policies);
        }
        demo.commit();
        on_server(fresh.connection(), "CREATE SCHEMA app");
        succeeds(demo.run(&["bootstrap", "--db", fresh.connection()]));
        assert_noop(&demo, fresh.connection());
        let back = Demo::new("rls-d-roundtrip-back");
        succeeds(back.run(&["pull", "--db", fresh.connection()]));
        assert_eq!(loaded(&back), before);
    });
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Rows(Vec<i64>),
    Denied,
}

pub(super) async fn attempt(conn: &mut Conn, sql: &str) -> Outcome {
    conn.execute("SAVEPOINT operation").await.unwrap();
    let result = conn.query(sql).await;
    conn.execute("ROLLBACK TO SAVEPOINT operation; RELEASE SAVEPOINT operation")
        .await
        .unwrap();
    match result {
        Ok(rows) => Outcome::Rows(
            rows.iter()
                .map(|row| row.try_get_at::<i64>(0).unwrap().unwrap())
                .collect(),
        ),
        Err(DbError::Driver {
            code: Some(code), ..
        }) if code == "42501" => Outcome::Denied,
        Err(error) => panic!("unexpected SQL failure for {sql}: {error:?}"),
    }
}

fn rows(ids: &[i64]) -> Outcome {
    Outcome::Rows(ids.to_vec())
}

fn enforcement(
    connection: &str,
    role: &str,
    owner: bool,
    tenant: Option<i32>,
    table: &str,
) -> Vec<Outcome> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let mut conn = Conn::connect(Driver::Postgres, connection).await.unwrap();
        conn.execute(&format!("BEGIN; SET LOCAL ROLE {role}")).await.unwrap();
        let role_flags = conn.query("SELECT rolsuper OR rolbypassrls FROM pg_catalog.pg_roles WHERE rolname=current_user").await.unwrap();
        assert_eq!(role_flags[0].try_get_at::<bool>(0).unwrap(), Some(false));
        if let Some(tenant) = tenant {
            conn.execute(&format!("SET LOCAL app.tenant = '{tenant}'")).await.unwrap();
        } else {
            let setting = conn.query("SELECT current_setting('app.tenant',true) IS NULL").await.unwrap();
            assert_eq!(setting[0].try_get_at::<bool>(0).unwrap(), Some(true));
        }
        let bypass = owner && table == "unforced_rows";
        let sees = |row_tenant| bypass || tenant == Some(row_tenant);
        let mut cases = vec![(
            format!("SELECT id::bigint FROM app.{table} ORDER BY id"),
            rows(if bypass { &[1, 2, 3] } else if tenant == Some(1) { &[1] } else if tenant == Some(2) { &[2] } else { &[] }),
        )];
        for t in [1,2] {
            cases.push((format!("INSERT INTO app.{table} VALUES (101,{t},10) RETURNING id::bigint"),
                if sees(t) { rows(&[101]) } else { Outcome::Denied }));
            cases.push((format!("UPDATE app.{table} SET payload=11 WHERE id={t} RETURNING id::bigint"),
                if sees(t) { rows(&[i64::from(t)]) } else { rows(&[]) }));
            cases.push((format!("UPDATE app.{table} SET tenant={} WHERE id={t} RETURNING id::bigint", 3-t),
                if bypass { rows(&[i64::from(t)]) } else if sees(t) { Outcome::Denied } else { rows(&[]) }));
            cases.push((format!("UPDATE app.{table} SET payload=-1 WHERE id={t} RETURNING id::bigint"),
                if bypass { rows(&[i64::from(t)]) } else if sees(t) { Outcome::Denied } else { rows(&[]) }));
            cases.push((format!("INSERT INTO app.{table} VALUES (102,{t},-1) RETURNING id::bigint"),
                if bypass { rows(&[102]) } else { Outcome::Denied }));
        }
        let mut outcomes = Vec::new();
        for (sql, expected) in cases {
            let actual = attempt(&mut conn, &sql).await;
            assert_eq!(actual, expected, "role={role}, owner={owner}, tenant={tenant:?}, table={table}, SQL={sql}");
            outcomes.push(actual);
        }
        conn.execute("ROLLBACK").await.unwrap();
        outcomes
    })
}

#[test]
#[ignore = "needs both live PostgreSQL versions"]
fn row_security_d_bootstrap_preserves_enforcement_for_tenants_members_and_forced_owner() {
    versions(|base, container| {
        let mut roles = Roles::new(base);
        let owner = roles.create("owner", "");
        let user = roles.create("user", "");
        let member = roles.create("member", "NOINHERIT");
        on_server(base, &format!("GRANT {owner} TO {member}"));
        assert_eq!(
            scalar(
                base,
                &format!("SELECT pg_has_role('{member}','{owner}','MEMBER')::int::bigint")
            ),
            1
        );
        assert_eq!(
            scalar(
                base,
                &format!("SELECT pg_has_role('{member}','{owner}','USAGE')::int::bigint")
            ),
            0
        );
        let source = OwnDatabase::new(base, "rls_d_enforcement_source");
        let fresh = OwnDatabase::new(base, "rls_d_enforcement_fresh");
        let mut setup = String::from("CREATE SCHEMA app;");
        for table in ["public_rows", "named_rows", "unforced_rows"] {
            let targets = if table == "named_rows" {
                format!("{owner},{user},{member}")
            } else {
                "PUBLIC".into()
            };
            setup.push_str(&format!("CREATE TABLE app.{table} (id integer PRIMARY KEY, tenant integer, payload integer);
                CREATE POLICY tenant_select ON app.{table} FOR SELECT TO {targets}
                    USING (tenant = current_setting('app.tenant',true)::integer);
                CREATE POLICY tenant_insert ON app.{table} FOR INSERT TO {targets}
                    WITH CHECK (tenant = current_setting('app.tenant',true)::integer);
                CREATE POLICY tenant_update ON app.{table} FOR UPDATE TO {targets}
                    USING (tenant = current_setting('app.tenant',true)::integer)
                    WITH CHECK (tenant = current_setting('app.tenant',true)::integer);
                CREATE POLICY positive_payload ON app.{table} AS RESTRICTIVE FOR ALL TO {targets}
                    USING (payload >= 0) WITH CHECK (payload >= 0);
                ALTER TABLE app.{table} ENABLE ROW LEVEL SECURITY;"));
            if table != "unforced_rows" {
                setup.push_str(&format!(
                    "ALTER TABLE app.{table} FORCE ROW LEVEL SECURITY;"
                ));
            }
        }
        on_server(source.connection(), &setup);
        let demo = Demo::new("rls-d-enforcement");
        succeeds(demo.run(&["pull", "--db", source.connection()]));
        demo.commit();
        let script = demo.dir.join("bootstrap.sql");
        succeeds(demo.run(&["bootstrap", "--sql", script.to_str().unwrap()]));
        on_server(fresh.connection(), "CREATE SCHEMA app");
        // Exercise the exported bootstrap as well as the connected bootstrap in
        // the matrix test. Behavior must catch a bad predicate even when its
        // presence satisfies the structural closing comparison.
        succeeds(psql_script_in(
            container,
            &fresh,
            &std::fs::read_to_string(script).unwrap(),
        ));
        for db in [&source, &fresh] {
            on_server(
                db.connection(),
                &format!("GRANT USAGE ON SCHEMA app TO {owner},{user},{member};"),
            );
            for table in ["public_rows", "named_rows", "unforced_rows"] {
                on_server(
                    db.connection(),
                    &format!(
                        "INSERT INTO app.{table} VALUES (1,1,10),(2,2,20),(3,1,-1);
                    ALTER TABLE app.{table} OWNER TO {owner};
                    GRANT SELECT,INSERT,UPDATE ON app.{table} TO {user},{member};"
                    ),
                );
            }
        }
        // Owner goes first so removing FORCE fails this exact acceptance case,
        // rather than being hidden by an earlier recipient or privilege error.
        for (role, is_owner) in [(&owner, true), (&user, false), (&member, false)] {
            for tenant in [Some(1), None, Some(2), Some(99)] {
                for table in ["public_rows", "named_rows", "unforced_rows"] {
                    let expected = enforcement(source.connection(), role, is_owner, tenant, table);
                    let actual = enforcement(fresh.connection(), role, is_owner, tenant, table);
                    assert_eq!(
                        actual, expected,
                        "source/bootstrap enforcement: {role}, {tenant:?}, {table}"
                    );
                }
            }
        }
    });
}

#[test]
#[ignore = "needs both live PostgreSQL versions"]
fn row_security_d_predicate_drift_refuses_planning_and_a_saved_plan() {
    versions(|base, _| {
        let own = OwnDatabase::new(base, "rls_d_drift");
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA app; CREATE TABLE app.docs (id integer PRIMARY KEY, tenant integer);
            CREATE POLICY tenant_gate ON app.docs USING (tenant=1);
            ALTER TABLE app.docs ENABLE ROW LEVEL SECURITY;",
        );
        let demo = Demo::new("rls-d-drift");
        succeeds(demo.run(&["pull", "--db", connection]));
        demo.commit();
        succeeds(demo.run(&[
            "baseline",
            "--db",
            connection,
            "--reason",
            "adopt RLS drift fixture",
        ]));
        let table = demo.dir.join("schema/app.docs.yml");
        let old = std::fs::read_to_string(&table).unwrap();
        let new = old.replacen("columns:\n", "columns:\n  label: {type: text}\n", 1);
        assert_ne!(old, new);
        std::fs::write(table, new).unwrap();
        let plan = connected_artifact(&demo, connection, false);
        let before = latest_snapshot(connection);
        on_server(
            connection,
            "ALTER POLICY tenant_gate ON app.docs USING (tenant=2)",
        );
        let verification = demo.run(&["verify", "--db", connection]);
        assert_ne!(code(&verification), 0);
        assert!(format!("{}{}", stdout(&verification), stderr(&verification)).contains("app.docs"));
        let output = demo.dir.join("keep-plan.json");
        std::fs::write(&output, "previous artifact").unwrap();
        let planning = demo.run(&[
            "plan",
            "--db",
            connection,
            "--out",
            output.to_str().unwrap(),
        ]);
        assert_ne!(code(&planning), 0);
        assert_eq!(
            std::fs::read_to_string(output).unwrap(),
            "previous artifact"
        );
        assert_eq!(latest_snapshot(connection), before);
        let apply = approved_apply(&demo, connection, &plan, &[]);
        assert_ne!(code(&apply), 0, "{}{}", stdout(&apply), stderr(&apply));
        let failed = latest_snapshot(connection);
        // SPEC 7.5 requires an audit entry for a refusal under the lock. The
        // rejected catalog must not replace the recorded schema or identities.
        assert_eq!(failed.kind, pbps_model::StateKind::Failed);
        assert_eq!(failed.schema, before.schema);
        assert_eq!(failed.ids, before.ids);
        assert_eq!(failed.declared, before.declared);
        let reason = failed.reason.as_ref().unwrap().raw();
        assert!(
            reason.contains("no longer the database this plan was computed against"),
            "{reason}"
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_attribute WHERE attrelid='app.docs'::regclass AND attname='label' AND NOT attisdropped"
            ),
            0
        );
        on_server(
            connection,
            "ALTER POLICY tenant_gate ON app.docs USING (tenant=1)",
        );
        succeeds(approved_apply(&demo, connection, &plan, &[]));
        assert_noop(&demo, connection);
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_attribute WHERE attrelid='app.docs'::regclass AND attname='label' AND NOT attisdropped"
            ),
            1
        );
    });
}

#[test]
#[ignore = "needs both live PostgreSQL versions"]
fn row_security_d_reference_data_refuses_every_nondefault_security_shape() {
    versions(|base, _| {
        let own = OwnDatabase::new(base, "rls_d_data");
        let connection = own.connection();
        on_server(connection, "CREATE SCHEMA app");
        let demo = Demo::new("rls-d-data");
        let declaration = "table: app.t\ncolumns:\n  id: {type: integer, nullable: false}\nprimary_key: [id]\ndata: {mode: ensure, rows: {1: {}}}\n";
        // Keep HEAD valid: each refusal must come from the current invalid
        // declaration, not from a previously committed invalid baseline.
        demo.table(declaration);
        succeeds(demo.run(&["plan"]));
        demo.commit();
        let artifact = demo.dir.join("keep.json");
        let script = demo.dir.join("keep.sql");
        for security in [
            "row_security: {enabled: true}\n",
            "row_security: {forced: true}\n",
            "row_policies: {dormant: {}}\n",
        ] {
            demo.table(&format!("{declaration}{security}"));
            std::fs::write(&artifact, "previous plan").unwrap();
            std::fs::write(&script, "previous script").unwrap();
            for args in [
                vec!["validate"],
                vec![
                    "plan",
                    "--db",
                    connection,
                    "--out",
                    artifact.to_str().unwrap(),
                ],
                vec![
                    "bootstrap",
                    "--db",
                    connection,
                    "--sql",
                    script.to_str().unwrap(),
                ],
            ] {
                let refused = demo.run(&args);
                assert_ne!(code(&refused), 0);
                let message = format!("{}{}", stdout(&refused), stderr(&refused));
                assert!(message.contains("`data:` cannot"), "{message}");
            }
            assert_eq!(std::fs::read_to_string(&artifact).unwrap(), "previous plan");
            assert_eq!(std::fs::read_to_string(&script).unwrap(), "previous script");
            assert_eq!(
                scalar(
                    connection,
                    "SELECT count(*) FROM pg_tables WHERE schemaname='app'"
                ),
                0
            );
            assert_eq!(
                scalar(
                    connection,
                    "SELECT (to_regclass('public.__pbps_state') IS NULL)::int::bigint"
                ),
                1
            );
        }
        demo.table(&format!(
            "{declaration}row_security: {{enabled: false, forced: false}}\nrow_policies: {{}}\n"
        ));
        succeeds(demo.run(&["plan"]));
        demo.commit();
        succeeds(demo.run(&["bootstrap", "--db", connection]));
        assert_eq!(
            scalar(connection, "SELECT count(*) FROM app.t WHERE id=1"),
            1
        );
        assert_noop(&demo, connection);
    });
}
