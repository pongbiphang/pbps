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
        "CREATE SCHEMA IF NOT EXISTS ext; CREATE SCHEMA IF NOT EXISTS other; CREATE EXTENSION citext WITH SCHEMA ext VERSION '1.4'; CREATE EXTENSION hstore WITH SCHEMA ext VERSION '1.4'; CREATE TABLE ext.ordinary(id integer);",
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

const SCRATCH_TARGET: &str = "PBPS_2447_TARGET";
const SCRATCH_SERVER: &str = "PBPS_2447_SCRATCH";
const EXACT_PACKAGES: &str = "CREATE SCHEMA IF NOT EXISTS ext; CREATE EXTENSION citext WITH SCHEMA ext VERSION '1.4'; CREATE EXTENSION hstore WITH SCHEMA ext VERSION '1.4';";

struct ScratchCase {
    _database: OwnDatabase,
    scratch: super::resolver_publication::Confined,
    demo: Demo,
    connection: String,
}

impl ScratchCase {
    fn new(server: &str, slug: &str, resolver: bool) -> Self {
        let database = OwnDatabase::new(server, slug);
        let connection = database.connection().to_owned();
        installed(&connection);
        if resolver {
            on_server(
                &connection,
                "CREATE VIEW ext.v AS SELECT id FROM ext.ordinary",
            );
        }
        let demo = adopted(&connection, slug);
        let scratch = super::resolver_publication::Confined::new(server);
        std::fs::write(demo.dir.join("pbps.yml"), format!("dialect: postgres\nresolvers:\n  scratch:\n    kind: server\n    url_env: {SCRATCH_SERVER}\n    externals: externals.sql\nenvironments:\n  prod:\n    url_env: {SCRATCH_TARGET}\n    resolve_with: scratch\n    fingerprint_key_file: key\n")).unwrap();
        std::fs::write(demo.dir.join("externals.sql"), EXACT_PACKAGES).unwrap();
        succeeds(demo.run(&[
            "key",
            "generate",
            "--out",
            demo.dir.join("key").to_str().unwrap(),
        ]));
        std::fs::write(
            demo.dir.join("schema/ext.ordinary.yml"),
            "table: ext.ordinary\ncolumns:\n  id: {type: integer}\n  added: {type: integer}\n",
        )
        .unwrap();
        succeeds(demo.run(&["plan", "--no-dev"]));
        demo.commit();
        Self {
            _database: database,
            scratch,
            demo,
            connection,
        }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.demo.run_with_env(
            args,
            &[
                (SCRATCH_TARGET, &self.connection),
                (SCRATCH_SERVER, &self.scratch.connection),
            ],
        )
    }

    fn plan(&self, resolver: bool) -> std::process::Output {
        let path = self.demo.dir.join("scratch.plan.json");
        let mut args = vec![
            "plan",
            "--env",
            "prod",
            "--out",
            path.to_str().unwrap(),
            "--format",
            "json",
        ];
        if resolver {
            args.push("--no-rehearse");
        }
        self.run(&args)
    }

    fn refused(&self, resolver: bool, required: &[&str]) {
        let out = self.plan(resolver);
        assert_ne!(code(&out), 0, "{}", stdout(&out));
        let message = format!("{}{}", stdout(&out), stderr(&out));
        for fact in required {
            assert!(message.contains(fact), "missing {fact}: {message}");
        }
        assert!(!self.demo.dir.join("scratch.plan.json").exists());
        assert_eq!(
            super::scalar(
                &self.connection,
                "SELECT count(*) FROM pg_catalog.pg_attribute WHERE attrelid = 'ext.ordinary'::regclass AND attname = 'added'"
            ),
            0
        );
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn adopted_extensions_allow_resolver_and_rehearsal_for_ordinary_changes() {
    servers(|server, variable| {
        for resolver in [true, false] {
            let case = ScratchCase::new(
                server,
                &format!("extensions2447-scratch-{variable}-{resolver}"),
                resolver,
            );
            let report = json_output(succeeds(case.plan(resolver)));
            if resolver {
                assert_eq!(
                    report["data"]["resolver_assessment"]["need"], "resolved",
                    "{report}"
                );
            } else {
                assert_eq!(
                    report["data"]["rehearsal"]["outcome"], "converged",
                    "{report}"
                );
            }
            let path = case.demo.dir.join("scratch.plan.json");
            let saved: pbps_model::SavedPlan =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(saved.ids.extensions.len(), 3);
            assert_eq!(saved.changes.changes.len(), 1);
            assert!(matches!(
                saved.changes.changes[0].change,
                pbps_model::Change::AddColumn { .. }
            ));
            let checksum = super::plan_checksum(&path);
            succeeds(case.run(&[
                "apply",
                "--env",
                "prod",
                "--plan",
                path.to_str().unwrap(),
                "--checksum",
                &checksum,
            ]));
            succeeds(case.run(&["verify", "--env", "prod"]));
            assert_eq!(
                super::scalar(
                    &case.connection,
                    "SELECT count(*) FROM pg_catalog.pg_attribute WHERE attrelid = 'ext.ordinary'::regclass AND attname = 'added'"
                ),
                1
            );
        }
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn scratch_extensions_require_presence_exact_version_and_schema() {
    servers(|server, variable| {
        for resolver in [true, false] {
            let case = ScratchCase::new(
                server,
                &format!("extensions2447-prereq-{variable}-{resolver}"),
                resolver,
            );
            for (sql, facts) in [
                (
                    "CREATE SCHEMA IF NOT EXISTS ext; CREATE EXTENSION hstore WITH SCHEMA ext VERSION '1.4';",
                    vec!["scratch extension", "citext", "1.4", "absent", "externals"],
                ),
                (
                    "CREATE SCHEMA IF NOT EXISTS ext; CREATE EXTENSION citext WITH SCHEMA ext VERSION '1.6'; CREATE EXTENSION hstore WITH SCHEMA ext VERSION '1.4';",
                    vec!["scratch extension", "citext", "1.4", "1.6"],
                ),
                (
                    "CREATE SCHEMA IF NOT EXISTS ext; CREATE SCHEMA IF NOT EXISTS other; CREATE EXTENSION citext WITH SCHEMA other VERSION '1.4'; CREATE EXTENSION hstore WITH SCHEMA ext VERSION '1.4';",
                    vec!["scratch extension", "citext", "ext", "other"],
                ),
            ] {
                std::fs::write(case.demo.dir.join("externals.sql"), sql).unwrap();
                case.demo.commit();
                case.refused(resolver, &facts);
            }
        }
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn scratch_bootstrap_does_not_authorize_real_extension_lifecycle_changes() {
    servers(|server, variable| {
        let case = ScratchCase::new(
            server,
            &format!("extensions2447-lifecycle-scratch-{variable}"),
            false,
        );
        for (name, text, action) in [
            (
                "pgcrypto",
                "extension: pgcrypto\nschema: ext\nversion: '1.3'\n",
                "CREATE EXTENSION",
            ),
            (
                "citext",
                "extension: citext\nschema: ext\nversion: '1.6'\n",
                "UPDATE",
            ),
            (
                "hstore",
                "extension: hstore\nschema: other\nversion: '1.4'\n",
                "SET SCHEMA",
            ),
        ] {
            let path = extension_file(&case.demo, name);
            let old = std::fs::read(&path).ok();
            std::fs::write(&path, text).unwrap();
            case.demo.commit();
            case.refused(false, &[name, action]);
            match old {
                Some(bytes) => std::fs::write(path, bytes).unwrap(),
                None => std::fs::remove_file(path).unwrap(),
            }
            case.demo.commit();
        }
    });
}

/// Temporarily hide only this fixture's package scripts, then restore even if
/// an assertion fails. This distinguishes installed target state from scratch
/// availability without substituting a different PostgreSQL major.
struct HiddenCitextScripts {
    container: String,
}
impl HiddenCitextScripts {
    fn new(container: String) -> Self {
        succeeds(std::process::Command::new("docker").args(["exec", &container, "sh", "-c", "set -eu; pbps_extension_dir=$(pg_config --sharedir)/extension; for pbps_package_file in \"$pbps_extension_dir\"/citext*.sql; do test -f \"$pbps_package_file\"; mv \"$pbps_package_file\" \"$pbps_package_file.pbps-2447-disabled\"; done"]).output().unwrap());
        Self { container }
    }
}
impl Drop for HiddenCitextScripts {
    fn drop(&mut self) {
        succeeds(std::process::Command::new("docker").args(["exec", &self.container, "sh", "-c", "set -eu; pbps_extension_dir=$(pg_config --sharedir)/extension; for pbps_package_file in \"$pbps_extension_dir\"/citext*.sql.pbps-2447-disabled; do test -f \"$pbps_package_file\"; mv \"$pbps_package_file\" \"${pbps_package_file%.pbps-2447-disabled}\"; done"]).output().unwrap());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn scratch_without_the_exact_installable_extension_version_is_named() {
    servers(|server, variable| {
        let case = ScratchCase::new(
            server,
            &format!("extensions2447-unavailable-{variable}"),
            false,
        );
        let container_variable = variable.replace("_DB", "_CONTAINER");
        let hidden = HiddenCitextScripts::new(std::env::var(container_variable).unwrap());
        assert_eq!(
            super::scalar(
                server,
                "SELECT count(*) FROM pg_catalog.pg_available_extension_versions WHERE name = 'citext' AND version = '1.4'"
            ),
            0
        );
        case.refused(false, &["citext", "1.4"]);
        drop(hidden);
        assert_eq!(
            super::scalar(
                server,
                "SELECT count(*) FROM pg_catalog.pg_available_extension_versions WHERE name = 'citext' AND version = '1.4'"
            ),
            1
        );
    });
}
