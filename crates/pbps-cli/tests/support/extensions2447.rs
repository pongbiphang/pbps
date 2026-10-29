//! Read-only extension adoption; every case runs against both pinned engines.
use super::{
    Demo, OwnDatabase, approved_apply, code, json_output, on_server, stderr, stdout, succeeds,
    text_of,
};
use std::path::Path;

fn servers(mut check: impl FnMut(&str, &str)) {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        check(&std::env::var(variable).unwrap(), variable);
    }
}

fn installed(connection: &str) {
    on_server(
        connection,
        "CREATE SCHEMA ext; CREATE SCHEMA other; CREATE EXTENSION citext WITH SCHEMA ext VERSION '1.4'; CREATE EXTENSION hstore WITH SCHEMA ext VERSION '1.4'; CREATE TABLE ext.ordinary(id integer);",
    );
}

fn extension_file(d: &Demo, name: &str) -> std::path::PathBuf {
    d.dir.join(format!("schema/extensions/{name}.yml"))
}

fn adopted(connection: &str, slug: &str) -> Demo {
    let d = Demo::new(slug);
    succeeds(d.run(&["pull", "--db", connection]));
    succeeds(d.run(&["fmt", "--check"]));
    d.commit();
    succeeds(d.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "explicit extension adoption",
    ]));
    d
}

fn findings(d: &Demo, connection: &str) -> serde_json::Value {
    json_output(succeeds(
        d.run(&["plan", "--db", connection, "--format", "json"]),
    ))
}

fn assert_refused(d: &Demo, connection: &str, action: &str, name: &str) {
    let path = d.dir.join("must-not-exist.plan.json");
    let before = text_of(connection, "SELECT count(*)::text FROM public.__pbps_state");
    let out = d.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()]);
    assert_ne!(code(&out), 0, "{}", stdout(&out));
    let message = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        message.contains(action) && message.contains(name),
        "{message}"
    );
    assert!(!path.exists(), "refused lifecycle wrote a partial plan");
    assert_eq!(
        text_of(connection, "SELECT count(*)::text FROM public.__pbps_state"),
        before
    );
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn exact_installed_packages_round_trip_and_native_changes_are_named_drift() {
    servers(|admin, slug| {
        let own = OwnDatabase::new(admin, &format!("extensions2447-read-{slug}"));
        let connection = own.connection();
        installed(connection);
        on_server(
            connection,
            "COMMENT ON EXTENSION citext IS 'still unsupported inventory'",
        );
        let d = adopted(connection, &format!("extensions2447-read-{slug}"));
        let loaded = pbps_load::load_schema_dir(&d.dir.join("schema")).unwrap();
        assert_eq!(loaded.schema.extensions.len(), 3);
        let duplicate = d.dir.join("schema/duplicate-extension.yml");
        std::fs::copy(extension_file(&d, "citext"), &duplicate).unwrap();
        let rejected = pbps_load::load_schema_dir(&d.dir.join("schema")).unwrap_err();
        assert!(rejected.iter().any(|e| e.to_string().contains("citext")));
        std::fs::remove_file(duplicate).unwrap();
        for (name, schema, version) in [
            ("plpgsql", "pg_catalog", "1.0"),
            ("citext", "ext", "1.4"),
            ("hstore", "ext", "1.4"),
        ] {
            let definition = &loaded.schema.extensions[name];
            assert_eq!(definition.schema, schema);
            assert_eq!(definition.version, version);
            assert!(
                std::fs::read_to_string(extension_file(&d, name))
                    .unwrap()
                    .contains(&format!("version: \"{version}\""))
            );
        }
        assert_eq!(loaded.schema.tables.len(), 1);
        assert!(loaded.schema.modules.is_empty());
        assert!(loaded.schema.types.is_empty());
        let ids: pbps_model::IdsFile =
            serde_json::from_slice(&std::fs::read(d.dir.join("schema.ids.json")).unwrap()).unwrap();
        assert_eq!(ids.extensions.len(), 3);
        assert!(
            ids.extensions
                .keys()
                .all(|uid| uid.kind() == pbps_model::UidKind::Extension)
        );
        succeeds(d.run(&["verify", "--db", connection]));
        let report = findings(&d, connection);
        assert_eq!(report["data"]["changes"], 0, "{report}");
        let messages: Vec<_> = report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["id"] == "plan.unheld-object")
            .map(|f| f["message"].as_str().unwrap())
            .collect();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("comment on extension") && m.contains("citext")),
            "{report}"
        );
        assert!(
            !messages.iter().any(|m| m.starts_with("extension `")),
            "{report}"
        );
        let saved = d.dir.join("noop.plan.json");
        succeeds(d.run(&["plan", "--db", connection, "--out", saved.to_str().unwrap()]));
        let plan: pbps_model::SavedPlan =
            serde_json::from_slice(&std::fs::read(&saved).unwrap()).unwrap();
        assert_eq!(plan.version, pbps_model::plan::CURRENT_VERSION);
        assert!(plan.changes.is_empty());
        succeeds(approved_apply(&d, connection, &saved, &[]));
        succeeds(d.run(&["verify", "--db", connection]));
        let first = std::fs::read(d.dir.join("schema.ids.json")).unwrap();
        succeeds(d.run(&["pull", "--force", "--db", connection]));
        assert_eq!(std::fs::read(d.dir.join("schema.ids.json")).unwrap(), first);
        // Read-only package support must not relax the provider-type gate.
        on_server(connection, "CREATE TABLE ext.user_provider(c ext.citext)");
        let provider = succeeds(d.run(&["pull", "--force", "--db", connection]));
        assert!(
            stderr(&provider).contains("citext"),
            "{}",
            stderr(&provider)
        );
        assert!(
            !pbps_load::load_schema_dir(&d.dir.join("schema"))
                .unwrap()
                .schema
                .tables
                .contains_key(&"ext.user_provider".parse().unwrap())
        );
        on_server(connection, "DROP TABLE ext.user_provider");
        on_server(connection, "ALTER EXTENSION citext UPDATE TO '1.6'");
        let out = d.run(&["verify", "--db", connection, "--format", "json"]);
        assert_ne!(code(&out), 0);
        let message = format!("{}{}", stdout(&out), stderr(&out));
        for fact in ["citext", "UPDATE", "1.4", "1.6"] {
            assert!(message.contains(fact), "{message}");
        }
        on_server(connection, "ALTER EXTENSION hstore SET SCHEMA other");
        let out = d.run(&["verify", "--db", connection]);
        assert_ne!(code(&out), 0);
        assert!(format!("{}{}", stdout(&out), stderr(&out)).contains("SET SCHEMA"));
        on_server(connection, "DROP EXTENSION hstore");
        let out = d.run(&["verify", "--db", connection]);
        assert_ne!(code(&out), 0);
        let message = format!("{}{}", stdout(&out), stderr(&out));
        assert!(
            message.contains("hstore") && message.contains("DROP EXTENSION"),
            "{message}"
        );
        assert_ne!(code(&d.run(&["pull", "--force", "--db", connection])), 0);
        assert_eq!(std::fs::read(d.dir.join("schema.ids.json")).unwrap(), first);
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn changed_lifecycles_never_publish_a_partial_executable_plan() {
    servers(|admin, slug| {
        let own = OwnDatabase::new(admin, &format!("extensions2447-refuse-{slug}"));
        let connection = own.connection();
        installed(connection);
        let d = adopted(connection, &format!("extensions2447-refuse-{slug}"));
        for (name, replacement, action) in [
            (
                "pgcrypto",
                Some("extension: pgcrypto\nschema: ext\nversion: '1.3'\n"),
                "CREATE EXTENSION",
            ),
            ("hstore", None, "DROP EXTENSION"),
            (
                "citext",
                Some("extension: citext\nschema: ext\nversion: '1.5'\n"),
                "UPDATE",
            ),
            (
                "hstore",
                Some("extension: hstore\nschema: other\nversion: '1.4'\n"),
                "SET SCHEMA",
            ),
        ] {
            let path = extension_file(&d, name);
            let original = std::fs::read(&path).ok();
            match replacement {
                Some(text) => std::fs::write(&path, text).unwrap(),
                None => std::fs::remove_file(&path).unwrap(),
            }
            let ordinary = d.dir.join("schema/ext.ordinary.yml");
            let table = std::fs::read(&ordinary).unwrap();
            if action == "UPDATE" {
                std::fs::write(&ordinary, "table: ext.ordinary\ncolumns:\n  id: {type: integer}\n  added: {type: integer}\n").unwrap();
            }
            d.commit();
            assert_refused(&d, connection, action, name);
            std::fs::write(ordinary, table).unwrap();
            match original {
                Some(text) => std::fs::write(&path, text).unwrap(),
                None => std::fs::remove_file(&path).unwrap(),
            }
            d.commit();
        }
        succeeds(d.run(&["verify", "--db", connection]));
        assert_eq!(
            text_of(
                connection,
                "SELECT extversion FROM pg_catalog.pg_extension WHERE extname='citext'"
            ),
            "1.4"
        );
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn legacy_scope_keeps_unheld_packages_out_until_explicit_adoption() {
    servers(|admin, slug| {
        let own = OwnDatabase::new(admin, &format!("extensions2447-legacy-{slug}"));
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA app; CREATE TABLE app.t(id integer)",
        );
        let d = Demo::new(&format!("extensions2447-legacy-{slug}"));
        d.table("table: app.t\ncolumns:\n  id: {type: integer}\n");
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            connection,
            "--reason",
            "legacy scope with no extension ownership",
        ]));
        on_server(
            connection,
            "CREATE EXTENSION citext WITH SCHEMA public VERSION '1.4'",
        );
        succeeds(d.run(&["verify", "--db", connection]));
        let report = findings(&d, connection);
        assert_eq!(report["data"]["changes"], 0);
        assert!(
            report["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["id"] == "plan.unheld-object"
                    && f["message"]
                        .as_str()
                        .unwrap()
                        .starts_with("extension `citext`")),
            "{report}"
        );
        assert!(
            !report.to_string().contains("extension `plpgsql`"),
            "{report}"
        );
        std::fs::create_dir_all(d.dir.join("schema/extensions")).unwrap();
        std::fs::write(
            extension_file(&d, "citext"),
            "extension: citext\nschema: public\nversion: '1.4'\n",
        )
        .unwrap();
        d.commit();
        assert_refused(&d, connection, "CREATE EXTENSION", "citext");
        succeeds(d.run(&["pull", "--force", "--db", connection]));
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            connection,
            "--reason",
            "explicit adoption of installed extensions",
        ]));
        succeeds(d.run(&["verify", "--db", connection]));
        assert_eq!(findings(&d, connection)["data"]["changes"], 0);
        assert!(Path::new(&extension_file(&d, "plpgsql")).is_file());
    });
}
