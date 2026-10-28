//! Unsupported properties use the ordinary managed-set limitation gate.
use super::{
    ClusterRoles, Demo, OwnDatabase, as_role, code, on_server, scalar, stderr, stdout, succeeds,
    try_on_server,
};
use std::process::Command;

fn servers(mut check: impl FnMut(&str, &str)) {
    for (database, container) in [
        ("PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_OLD_CONTAINER"),
        ("PBPS_TEST_PG_DB", "PBPS_TEST_PG_CONTAINER"),
    ] {
        check(
            &std::env::var(database).unwrap(),
            &std::env::var(container).unwrap(),
        );
    }
}

fn setup(connection: &str) {
    on_server(
        connection,
        "CREATE SCHEMA app; CREATE TABLE app.t (id integer, txt text, amount numeric); \
         CREATE VIEW app.v AS SELECT id FROM app.t",
    );
}

fn baseline(connection: &str, slug: &str) -> Demo {
    let d = Demo::new(&format!("properties2445-{slug}"));
    succeeds(d.run(&["pull", "--db", connection]));
    d.commit();
    succeeds(d.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "before changing the unsupported property",
    ]));
    silent(&d, connection);
    d
}

fn silent(d: &Demo, connection: &str) {
    let fresh = Demo::new("properties2445-default-pull");
    let pull = succeeds(fresh.run(&["pull", "--db", connection]));
    let plan = succeeds(d.run(&["plan", "--db", connection]));
    assert!(stdout(&plan).contains("No changes."), "{}", stdout(&plan));
    for output in [&pull, &plan] {
        for property in ["STORAGE", "COMPRESSION", "TABLESPACE", "column default"] {
            assert!(!stderr(output).contains(property), "{}", stderr(output));
        }
    }
}

fn named(d: &Demo, connection: &str, expected: &str) {
    let fresh = Demo::new("properties2445-named-pull");
    let pulled = succeeds(fresh.run(&["pull", "--db", connection]));
    assert!(stderr(&pulled).contains(expected), "{}", stderr(&pulled));
    let artifact = d.dir.join("property.plan.json");
    std::fs::write(&artifact, b"existing approved artifact").unwrap();
    let ids = std::fs::read(d.dir.join("schema.ids.json")).unwrap();
    let plan = d.run(&[
        "plan",
        "--db",
        connection,
        "--out",
        artifact.to_str().unwrap(),
    ]);
    assert_ne!(code(&plan), 0, "{}", stdout(&plan));
    assert!(stderr(&plan).contains(expected), "{}", stderr(&plan));
    assert!(
        stderr(&plan).contains("Refusing to compare, plan from, or record a partial schema"),
        "{}",
        stderr(&plan)
    );
    assert_eq!(
        std::fs::read(artifact).unwrap(),
        b"existing approved artifact"
    );
    assert_eq!(std::fs::read(d.dir.join("schema.ids.json")).unwrap(), ids);
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn column_storage_is_named_in_pull_and_refused_in_plan() {
    servers(|server, _| {
        let own = OwnDatabase::new(server, "properties2445-storage");
        let connection = own.connection();
        setup(connection);
        let d = baseline(connection, "storage");
        on_server(
            connection,
            "ALTER TABLE app.t ALTER COLUMN txt SET STORAGE EXTERNAL",
        );
        named(
            &d,
            connection,
            "column `app.t`.`txt` has STORAGE `EXTERNAL`",
        );
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn explicit_column_compression_is_named_even_when_it_matches_the_server_default() {
    servers(|server, _| {
        let own = OwnDatabase::new(server, "properties2445-compression");
        let connection = own.connection();
        setup(connection);
        let d = baseline(connection, "compression");
        for method in ["pglz", "lz4"] {
            on_server(
                connection,
                &format!(
                    "ALTER DATABASE \"{}\" SET default_toast_compression = '{method}'; \
                ALTER TABLE app.t ALTER COLUMN txt SET COMPRESSION {method}",
                    own.name
                ),
            );
            assert_eq!(
                scalar(
                    connection,
                    &format!(
                        "SELECT count(*) WHERE current_setting('default_toast_compression') = '{method}'"
                    )
                ),
                1
            );
            named(
                &d,
                connection,
                &format!("column `app.t`.`txt` has COMPRESSION `{method}`"),
            );
        }
    });
}

struct Tablespace {
    server: String,
    name: String,
}
impl Drop for Tablespace {
    fn drop(&mut self) {
        let _ = try_on_server(
            &self.server,
            &format!("DROP TABLESPACE IF EXISTS {}", self.name),
        );
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn table_tablespace_is_named_in_pull_and_refused_in_plan() {
    servers(|server, container| {
        let directory = "/var/lib/postgresql/pbps2445_space";
        succeeds(
            Command::new("docker")
                .args([
                    "exec", container, "install", "-d", "-o", "postgres", "-g", "postgres",
                    directory,
                ])
                .output()
                .unwrap(),
        );
        let space = Tablespace {
            server: server.into(),
            name: format!("pbps2445_space_{}", std::process::id()),
        };
        on_server(
            server,
            &format!("CREATE TABLESPACE {} LOCATION '{directory}'", space.name),
        );
        // This database must disappear before the tablespace guard runs.
        let own = OwnDatabase::new(server, "properties2445-tablespace");
        let connection = own.connection();
        setup(connection);
        let d = baseline(connection, "tablespace");
        on_server(
            connection,
            &format!("ALTER TABLE app.t SET TABLESPACE {}", space.name),
        );
        named(
            &d,
            connection,
            &format!("table `app.t` uses TABLESPACE `{}`", space.name),
        );
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_default_added_to_a_baselined_view_is_named_and_refused() {
    servers(|server, _| {
        let own = OwnDatabase::new(server, "properties2445-view-default");
        let connection = own.connection();
        setup(connection);
        let d = baseline(connection, "view-default");
        for expression in ["42", "43"] {
            on_server(
                connection,
                &format!("ALTER VIEW app.v ALTER COLUMN id SET DEFAULT {expression}"),
            );
            named(
                &d,
                connection,
                &format!("view `app.v` has column default `id DEFAULT {expression}"),
            );
        }
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn column_storage_equal_to_its_type_default_stays_silent() {
    servers(|server, _| {
        let own = OwnDatabase::new(server, "properties2445-default-storage");
        let connection = own.connection();
        setup(connection);
        let d = baseline(connection, "default-storage");
        on_server(
            connection,
            "ALTER TABLE app.t ALTER COLUMN txt SET STORAGE EXTENDED; \
            ALTER TABLE app.t ALTER COLUMN id SET STORAGE PLAIN; \
            ALTER TABLE app.t ALTER COLUMN amount SET STORAGE MAIN",
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid \
            WHERE a.attrelid = 'app.t'::regclass AND a.attnum > 0 AND a.attstorage <> t.typstorage"
            ),
            0
        );
        silent(&d, connection);
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn inherited_compression_stays_silent_under_both_server_defaults() {
    servers(|server, _| {
        let own = OwnDatabase::new(server, "properties2445-default-compression");
        let connection = own.connection();
        setup(connection);
        let d = baseline(connection, "default-compression");
        for method in ["pglz", "lz4"] {
            on_server(
                connection,
                &format!(
                    "ALTER DATABASE \"{}\" SET default_toast_compression = '{method}'; \
                ALTER TABLE app.t ALTER COLUMN txt SET COMPRESSION {method}; \
                ALTER TABLE app.t ALTER COLUMN txt SET COMPRESSION default",
                    own.name
                ),
            );
            assert_eq!(
                scalar(
                    connection,
                    &format!(
                        "SELECT count(*) WHERE current_setting('default_toast_compression') = '{method}'"
                    )
                ),
                1
            );
            assert_eq!(
                scalar(
                    connection,
                    "SELECT count(*) FROM pg_attribute WHERE attrelid = 'app.t'::regclass \
                AND attname = 'txt' AND attcompression = ''"
                ),
                1
            );
            silent(&d, connection);
        }
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn database_default_table_placement_stays_silent() {
    servers(|server, _| {
        let own = OwnDatabase::new(server, "properties2445-default-tablespace");
        let connection = own.connection();
        setup(connection);
        let d = baseline(connection, "default-tablespace");
        on_server(connection, "ALTER TABLE app.t SET TABLESPACE pg_default");
        assert_eq!(
            scalar(
                connection,
                "SELECT reltablespace::bigint FROM pg_class WHERE oid = 'app.t'::regclass"
            ),
            0
        );
        silent(&d, connection);
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_view_without_column_defaults_stays_silent() {
    servers(|server, _| {
        let own = OwnDatabase::new(server, "properties2445-default-view");
        let connection = own.connection();
        setup(connection);
        let d = baseline(connection, "default-view");
        on_server(
            connection,
            "ALTER VIEW app.v ALTER COLUMN id SET DEFAULT 42; \
            ALTER VIEW app.v ALTER COLUMN id DROP DEFAULT",
        );
        silent(&d, connection);
        on_server(
            connection,
            "ALTER VIEW app.v ALTER COLUMN id SET DEFAULT 42; \
            ALTER VIEW app.v ALTER COLUMN id SET DEFAULT NULL",
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_attrdef WHERE adrelid = 'app.v'::regclass"
            ),
            0
        );
        silent(&d, connection);
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn unreadable_property_catalog_columns_are_errors_in_pull_and_plan() {
    servers(|server, _| {
        let role = format!("pbps2445_reader_{}", std::process::id());
        let _roles = ClusterRoles {
            server: server.into(),
            names: vec![role.clone()],
        };
        on_server(
            server,
            &format!("CREATE ROLE {role} LOGIN PASSWORD 'trigger-test'"),
        );
        let own = OwnDatabase::new(server, "properties2445-unreadable");
        let admin = own.connection();
        setup(admin);
        let d = baseline(admin, "unreadable");
        on_server(
            admin,
            &format!(
                "GRANT USAGE ON SCHEMA app, public TO {role}; \
            GRANT SELECT ON ALL TABLES IN SCHEMA app TO {role}; \
            GRANT SELECT ON public.__pbps_state, public.__pbps_lock TO {role}"
            ),
        );
        let connection = as_role(admin, &role);
        for (catalog, column) in [
            ("pg_attribute", "attstorage"),
            ("pg_attribute", "attcompression"),
            ("pg_tablespace", "spcname"),
            ("pg_attrdef", "adbin"),
        ] {
            on_server(
                admin,
                &format!(
                    "REVOKE SELECT ON pg_catalog.{catalog} FROM PUBLIC; \
                DO $grant$ DECLARE names text; BEGIN \
                SELECT string_agg(quote_ident(attname), ',') INTO names FROM pg_attribute \
                WHERE attrelid = 'pg_catalog.{catalog}'::regclass AND attnum > 0 AND NOT attisdropped AND attname <> '{column}'; \
                EXECUTE format('GRANT SELECT (%s) ON pg_catalog.{catalog} TO {role}', names); END $grant$"
                ),
            );
            for command in ["pull", "plan"] {
                let fresh = Demo::new("properties2445-unreadable-pull");
                let project = if command == "pull" { &fresh } else { &d };
                let out = project.run(&[command, "--db", &connection]);
                assert_ne!(code(&out), 0, "{}", stdout(&out));
                assert!(
                    stderr(&out).contains("permission denied"),
                    "{}",
                    stderr(&out)
                );
                assert!(stderr(&out).contains(catalog), "{}", stderr(&out));
            }
            on_server(
                admin,
                &format!(
                    "GRANT SELECT ON pg_catalog.{catalog} TO PUBLIC; \
                DO $revoke$ DECLARE names text; BEGIN \
                SELECT string_agg(quote_ident(attname), ',') INTO names FROM pg_attribute \
                WHERE attrelid = 'pg_catalog.{catalog}'::regclass AND attnum > 0 AND NOT attisdropped; \
                EXECUTE format('REVOKE SELECT (%s) ON pg_catalog.{catalog} FROM {role}', names); END $revoke$"
                ),
            );
        }
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn partition_column_properties_are_named_and_refused() {
    servers(|server, _| {
        let own = OwnDatabase::new(server, "properties2445-partition");
        let connection = own.connection();
        setup(connection);
        on_server(
            connection,
            "CREATE TABLE app.p (k integer, txt text) PARTITION BY RANGE (k); \
            CREATE TABLE app.p0 PARTITION OF app.p FOR VALUES FROM (0) TO (10)",
        );
        let d = baseline(connection, "partition");
        on_server(
            connection,
            "ALTER TABLE app.p0 ALTER COLUMN txt SET STORAGE EXTERNAL",
        );
        named(
            &d,
            connection,
            "column `app.p0`.`txt` has STORAGE `EXTERNAL`",
        );
        on_server(
            connection,
            "ALTER TABLE app.p0 ALTER COLUMN txt SET STORAGE EXTENDED; \
            ALTER TABLE app.p0 ALTER COLUMN txt SET COMPRESSION lz4",
        );
        named(
            &d,
            connection,
            "column `app.p0`.`txt` has COMPRESSION `lz4`",
        );
    });
}
