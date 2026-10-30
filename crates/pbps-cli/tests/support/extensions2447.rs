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
    succeeds(d.run(&[
        "pull",
        "--db",
        connection,
        "--extension",
        "citext",
        "--extension",
        "hstore",
        "--extension",
        "plpgsql",
    ]));
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
        succeeds(d.run(&[
            "pull",
            "--force",
            "--db",
            connection,
            "--extension",
            "plpgsql",
        ]));
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

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn connected_bootstrap_adopts_exact_preinstalled_packages_without_lifecycle_ddl() {
    servers(|admin, slug| {
        let source = OwnDatabase::new(admin, &format!("extensions2447-bootstrap-source-{slug}"));
        let target = OwnDatabase::new(admin, &format!("extensions2447-bootstrap-target-{slug}"));
        for connection in [source.connection(), target.connection()] {
            // The server-selected defaults differ across majors. Pull pins what
            // was installed; neither the name nor pg_catalog grants an exemption.
            on_server(
                connection,
                "CREATE SCHEMA ext; CREATE SCHEMA app; CREATE EXTENSION citext SCHEMA pg_catalog; CREATE EXTENSION hstore SCHEMA ext;",
            );
        }
        on_server(source.connection(), "CREATE TABLE app.ordinary(id integer)");
        on_server(
            target.connection(),
            "CREATE EXTENSION pgcrypto SCHEMA ext VERSION '1.3'",
        );
        let d = Demo::new(&format!("extensions2447-bootstrap-{slug}"));
        succeeds(d.run(&[
            "pull",
            "--db",
            source.connection(),
            "--extension",
            "citext",
            "--extension",
            "hstore",
            "--extension",
            "plpgsql",
        ]));
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let loaded = pbps_load::load_schema_dir(&d.dir.join("schema")).unwrap();
        assert!(loaded.schema.extensions.contains_key("citext"));
        assert!(!loaded.schema.extensions.contains_key("pgcrypto"));
        let inventory = "SELECT jsonb_agg(jsonb_build_array(extname,extnamespace,extversion,oid) ORDER BY extname)::text FROM pg_extension";
        let before = text_of(target.connection(), inventory);
        succeeds(d.run(&["bootstrap", "--db", target.connection()]));
        assert_eq!(text_of(target.connection(), inventory), before);
        succeeds(d.run(&["verify", "--db", target.connection()]));
        let report = findings(&d, target.connection());
        assert_eq!(report["data"]["changes"], 0, "{report}");
        assert!(
            report["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["id"] == "plan.unheld-object"
                    && f["message"]
                        .as_str()
                        .unwrap()
                        .contains("extension `pgcrypto`")),
            "{report}"
        );
        // Recorded ownership must still make an explicit removal a refused DROP.
        std::fs::remove_file(extension_file(&d, "hstore")).unwrap();
        d.commit();
        assert_refused(&d, target.connection(), "DROP EXTENSION", "hstore");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn bootstrap_requires_exact_packages_before_creation_and_recording() {
    servers(|admin, slug| {
        let source = OwnDatabase::new(admin, &format!("extensions2447-required-source-{slug}"));
        on_server(
            source.connection(),
            "CREATE SCHEMA ext; CREATE SCHEMA app; CREATE EXTENSION hstore SCHEMA ext VERSION '1.4'; CREATE TABLE app.ordinary(id integer)",
        );
        let d = Demo::new(&format!("extensions2447-required-{slug}"));
        succeeds(d.run(&[
            "pull",
            "--db",
            source.connection(),
            "--extension",
            "hstore",
            "--extension",
            "plpgsql",
        ]));
        d.commit();
        for (case, install, expected) in [
            ("absent", "", "CREATE EXTENSION"),
            (
                "version",
                "CREATE EXTENSION hstore SCHEMA ext VERSION '1.8'",
                "UPDATE",
            ),
            (
                "schema",
                "CREATE EXTENSION hstore SCHEMA other VERSION '1.4'",
                "SET SCHEMA",
            ),
            (
                "occupied",
                "CREATE EXTENSION hstore SCHEMA ext VERSION '1.4'; CREATE TABLE app.ordinary(id integer)",
                "already has",
            ),
        ] {
            let target = OwnDatabase::new(admin, &format!("extensions2447-required-{case}-{slug}"));
            on_server(
                target.connection(),
                "CREATE SCHEMA ext; CREATE SCHEMA other; CREATE SCHEMA app",
            );
            if !install.is_empty() {
                on_server(target.connection(), install);
            }
            let out = d.run(&["bootstrap", "--db", target.connection()]);
            assert_ne!(code(&out), 0);
            let message = format!("{}{}", stdout(&out), stderr(&out));
            assert!(message.contains(expected), "{case}: {message}");
            if case != "occupied" {
                assert!(message.contains("hstore"), "{message}");
                assert_eq!(
                    text_of(
                        target.connection(),
                        "SELECT (to_regclass('app.ordinary') IS NULL)::text"
                    ),
                    "true"
                );
            }
            if text_of(
                target.connection(),
                "SELECT (to_regclass('public.__pbps_state') IS NOT NULL)::text",
            ) == "true"
            {
                assert_eq!(
                    text_of(
                        target.connection(),
                        "SELECT count(*)::text FROM public.__pbps_state"
                    ),
                    "0"
                );
            }
        }
        let target = OwnDatabase::new(admin, &format!("extensions2447-closing-{slug}"));
        on_server(
            target.connection(),
            "CREATE SCHEMA ext; CREATE SCHEMA other; CREATE SCHEMA app; CREATE EXTENSION hstore SCHEMA ext VERSION '1.4'; CREATE FUNCTION public.move_package() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN IF tg_tag = 'CREATE TABLE' AND to_regclass('app.ordinary') IS NOT NULL THEN ALTER EXTENSION hstore SET SCHEMA other; END IF; END $$; CREATE EVENT TRIGGER move_package ON ddl_command_end EXECUTE FUNCTION public.move_package()",
        );
        let out = d.run(&["bootstrap", "--db", target.connection()]);
        assert_ne!(
            code(&out),
            0,
            "closing drift was recorded: {}",
            stdout(&out)
        );
        assert!(stderr(&out).contains("SET SCHEMA"), "{}", stderr(&out));
        assert_eq!(
            text_of(
                target.connection(),
                "SELECT (to_regclass('app.ordinary') IS NULL)::text"
            ),
            "true"
        );
        assert_eq!(
            text_of(
                target.connection(),
                "SELECT n.nspname FROM pg_extension e JOIN pg_namespace n ON n.oid=e.extnamespace WHERE e.extname='hstore'"
            ),
            "ext"
        );
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn bootstrap_sql_checks_exact_packages_on_the_replay_target_before_ddl() {
    servers(|admin, slug| {
        let source = OwnDatabase::new(admin, &format!("extensions2447-sql-source-{slug}"));
        on_server(
            source.connection(),
            "CREATE SCHEMA ext; CREATE SCHEMA app; CREATE EXTENSION hstore SCHEMA ext VERSION '1.4'; CREATE TABLE app.ordinary(id integer)",
        );
        let d = Demo::new(&format!("extensions2447-sql-{slug}"));
        succeeds(d.run(&[
            "pull",
            "--db",
            source.connection(),
            "--extension",
            "hstore",
            "--extension",
            "plpgsql",
        ]));
        d.commit();
        let path = d.dir.join("bootstrap.sql");
        succeeds(d.run(&["bootstrap", "--sql", path.to_str().unwrap()]));
        let sql = std::fs::read_to_string(&path).unwrap();
        assert!(!sql.contains("CREATE EXTENSION"));
        for (case, install) in [
            ("exact", "CREATE EXTENSION hstore SCHEMA ext VERSION '1.4'"),
            ("absent", ""),
            (
                "version",
                "CREATE EXTENSION hstore SCHEMA ext VERSION '1.8'",
            ),
            (
                "schema",
                "CREATE EXTENSION hstore SCHEMA other VERSION '1.4'",
            ),
        ] {
            let target = OwnDatabase::new(admin, &format!("extensions2447-sql-{case}-{slug}"));
            on_server(
                target.connection(),
                "CREATE SCHEMA ext; CREATE SCHEMA other; CREATE SCHEMA app",
            );
            if !install.is_empty() {
                on_server(target.connection(), install);
            }
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let mut conn =
                        pbps_db::Conn::connect(pbps_db::Driver::Postgres, target.connection())
                            .await
                            .unwrap();
                    conn.execute(&sql).await.map_err(|e| e.to_string())
                });
            if case == "exact" {
                result.unwrap();
                assert_eq!(
                    text_of(
                        target.connection(),
                        "SELECT (to_regclass('app.ordinary') IS NOT NULL)::text"
                    ),
                    "true"
                );
            } else {
                let error = result.unwrap_err();
                assert!(
                    error.contains("required extension `hstore`"),
                    "{case}: {error}"
                );
                assert_eq!(
                    text_of(
                        target.connection(),
                        "SELECT (to_regclass('app.ordinary') IS NULL)::text"
                    ),
                    "true"
                );
            }
        }
    });
}

fn extension_ids(
    d: &Demo,
) -> std::collections::BTreeMap<pbps_model::Uid, pbps_model::ObjectLocalName> {
    serde_json::from_slice::<pbps_model::IdsFile>(
        &std::fs::read(d.dir.join("schema.ids.json")).unwrap(),
    )
    .unwrap()
    .extensions
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn pull_selects_only_requested_extensions_and_preserves_their_identities() {
    servers(|admin, slug| {
        let own = OwnDatabase::new(admin, &format!("extensions2447-selection-{slug}"));
        let connection = own.connection();
        installed(connection);
        let d = Demo::new(&format!("extensions2447-selection-{slug}"));
        succeeds(d.run(&["pull", "--db", connection]));
        assert!(extension_ids(&d).is_empty());
        assert!(
            pbps_load::load_schema_dir(&d.dir.join("schema"))
                .unwrap()
                .schema
                .extensions
                .is_empty()
        );
        assert_eq!(
            text_of(connection, "SELECT count(*)::text FROM pg_extension"),
            "3"
        );
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            connection,
            "--reason",
            "ordinary objects only",
        ]));
        succeeds(d.run(&[
            "pull",
            "--db",
            connection,
            "--force",
            "--extension",
            "citext",
        ]));
        let selected = extension_ids(&d);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected.values().next().unwrap().raw(), "citext");
        // Local selection is not permission to take ownership in an ordinary plan.
        d.commit();
        assert_refused(&d, connection, "CREATE EXTENSION", "citext");
        let package_path = extension_file(&d, "citext");
        let package = std::fs::read(&package_path).unwrap();
        let entries = text_of(connection, "SELECT count(*)::text FROM public.__pbps_state");
        for (schema, version, action) in [("ext", "1.8", "UPDATE"), ("other", "1.4", "SET SCHEMA")]
        {
            std::fs::write(
                &package_path,
                format!("extension: citext\nschema: {schema}\nversion: '{version}'\n"),
            )
            .unwrap();
            d.commit();
            let out = d.run(&[
                "baseline",
                "--db",
                connection,
                "--reason",
                "must verify exact package",
            ]);
            assert_ne!(code(&out), 0, "baseline adopted a mismatching package");
            assert!(
                stderr(&out).contains(action) && stderr(&out).contains("citext"),
                "{}",
                stderr(&out)
            );
            assert_eq!(
                text_of(connection, "SELECT count(*)::text FROM public.__pbps_state"),
                entries
            );
        }
        std::fs::write(&package_path, package).unwrap();
        d.commit();
        on_server(connection, "DROP EXTENSION citext RESTRICT");
        let absent = d.run(&[
            "baseline",
            "--db",
            connection,
            "--reason",
            "missing package cannot be adopted",
        ]);
        assert_ne!(code(&absent), 0);
        assert!(stderr(&absent).contains("citext"), "{}", stderr(&absent));
        assert_eq!(
            text_of(connection, "SELECT count(*)::text FROM public.__pbps_state"),
            entries
        );
        on_server(
            connection,
            "CREATE EXTENSION citext SCHEMA ext VERSION '1.4'",
        );
        succeeds(d.run(&[
            "baseline",
            "--db",
            connection,
            "--reason",
            "explicitly manage citext",
        ]));
        assert_eq!(findings(&d, connection)["data"]["changes"], 0);
        for args in [
            vec!["pull", "--db", connection, "--force"],
            vec![
                "pull",
                "--db",
                connection,
                "--force",
                "--extension",
                "citext",
                "--extension",
                "citext",
            ],
        ] {
            succeeds(d.run(&args));
            assert_eq!(extension_ids(&d), selected);
        }
        // A held identity also preserves selection when its local file is absent.
        std::fs::remove_file(extension_file(&d, "citext")).unwrap();
        succeeds(d.run(&["pull", "--db", connection, "--force"]));
        assert_eq!(extension_ids(&d), selected);
        assert!(extension_file(&d, "citext").exists());
        let before_ids = std::fs::read(d.dir.join("schema.ids.json")).unwrap();
        let before = pbps_load::load_schema_dir(&d.dir.join("schema"))
            .unwrap()
            .schema;
        for name in ["not_installed_2447", "pgcrypto", ""] {
            let out = d.run(&["pull", "--db", connection, "--force", "--extension", name]);
            assert_ne!(code(&out), 0);
            assert!(stderr(&out).contains("not installed"), "{}", stderr(&out));
            assert!(stderr(&out).contains(name), "{}", stderr(&out));
            assert_eq!(
                std::fs::read(d.dir.join("schema.ids.json")).unwrap(),
                before_ids
            );
            assert_eq!(
                pbps_load::load_schema_dir(&d.dir.join("schema"))
                    .unwrap()
                    .schema,
                before
            );
        }
        // An explicit declaration selects a package before it has an identity.
        let authored = Demo::new(&format!("extensions2447-authored-{slug}"));
        std::fs::create_dir_all(authored.dir.join("schema/extensions")).unwrap();
        std::fs::write(
            extension_file(&authored, "hstore"),
            "extension: hstore\nschema: other\nversion: '1.8'\n",
        )
        .unwrap();
        succeeds(authored.run(&["pull", "--db", connection, "--force"]));
        assert_eq!(extension_ids(&authored).len(), 1);
        let loaded = pbps_load::load_schema_dir(&authored.dir.join("schema")).unwrap();
        assert_eq!(loaded.schema.extensions["hstore"].schema, "ext");
        assert_eq!(loaded.schema.extensions["hstore"].version, "1.4");
        // Bad declaration history cannot become an empty selection under force.
        let path = extension_file(&authored, "hstore");
        std::fs::write(&path, "extension: hstore\nversion: [broken\n").unwrap();
        let ids = std::fs::read(authored.dir.join("schema.ids.json")).unwrap();
        let out = authored.run(&["pull", "--db", connection, "--force"]);
        assert_ne!(code(&out), 0);
        assert!(
            stderr(&out).contains("extension selection"),
            "{}",
            stderr(&out)
        );
        assert_eq!(
            std::fs::read(authored.dir.join("schema.ids.json")).unwrap(),
            ids
        );
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "extension: hstore\nversion: [broken\n"
        );
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn pull_names_unmanaged_packages_and_hides_the_hint_when_all_are_selected() {
    servers(|admin, slug| {
        let own = OwnDatabase::new(admin, &format!("extensions2447-hint-{slug}"));
        let connection = own.connection();
        installed(connection);
        let d = Demo::new(&format!("extensions2447-hint-{slug}"));
        let out = succeeds(d.run(&["pull", "--db", connection]));
        let text = stdout(&out);
        assert!(
            text.contains("Installed but not managed: citext 1.4, hstore 1.4, plpgsql 1.0"),
            "{text}"
        );
        assert_eq!(text.matches("Installed but not managed:").count(), 1);
        assert_eq!(
            text.matches("To manage one: pbps pull --extension citext")
                .count(),
            1
        );
        let out = succeeds(d.run(&[
            "pull",
            "--db",
            connection,
            "--force",
            "--extension",
            "citext",
        ]));
        let text = stdout(&out);
        assert!(
            text.contains("Installed but not managed: hstore 1.4, plpgsql 1.0"),
            "{text}"
        );
        assert!(
            text.contains("To manage one: pbps pull --extension hstore"),
            "{text}"
        );
        let out = succeeds(d.run(&[
            "pull",
            "--db",
            connection,
            "--force",
            "--extension",
            "hstore",
            "--extension",
            "plpgsql",
        ]));
        let text = stdout(&out);
        assert!(!text.contains("Installed but not managed:"), "{text}");
        assert!(!text.contains("To manage one:"), "{text}");
        assert_eq!(extension_ids(&d).len(), 3);
    });
}
