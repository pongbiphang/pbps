//! Plan-time advisory vetting, with a confined server scratch on both engines.
use super::*;

struct Scratch {
    database: OwnDatabase,
    role: String,
    connection: String,
    before: String,
}

impl Scratch {
    fn new(server: &str) -> Self {
        let database = OwnDatabase::new(server, "domain2309scratch");
        let role = format!("pbps_vet2309_{}", std::process::id());
        on_server(
            server,
            &format!(
                "CREATE ROLE {role} LOGIN PASSWORD 'vet2309' NOSUPERUSER NOCREATEDB NOCREATEROLE; GRANT pg_read_all_settings TO {role}; ALTER DATABASE {} OWNER TO {role}",
                database.name
            ),
        );
        let connection = format!("{} user={role} password=vet2309", database.connection());
        let before = inventory(server);
        Self {
            database,
            role,
            connection,
            before,
        }
    }

    fn released(&self) {
        assert_eq!(
            inventory(&self.database.server),
            self.before,
            "scratch database/login/roles leaked"
        );
        assert_eq!(
            scalar(
                self.database.connection(),
                "SELECT count(*)::bigint FROM pg_namespace WHERE nspname NOT IN ('public','information_schema') AND nspname NOT LIKE 'pg_%'"
            ),
            0,
            "scratch schema leaked"
        );
        assert_eq!(
            scalar(
                self.database.connection(),
                "SELECT count(*)::bigint FROM pg_type t JOIN pg_namespace n ON n.oid=t.typnamespace WHERE n.nspname='public' AND t.oid>=16384"
            ),
            0,
            "scratch type leaked"
        );
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = try_on_server(
            &self.database.server,
            &format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE);",
                self.database.name
            ),
        );
        let _ = try_on_server(
            &self.database.server,
            &format!("DROP ROLE IF EXISTS {}", self.role),
        );
    }
}

fn inventory(server: &str) -> String {
    text_of(
        server,
        "SELECT (SELECT string_agg(datname, ',' ORDER BY datname) FROM pg_database) || '/' || (SELECT string_agg(rolname, ',' ORDER BY rolname) FROM pg_roles) AS names",
    )
}

fn config(d: &Demo, extra: &str) {
    std::fs::write(d.dir.join("pbps.yml"), format!("dialect: postgres\nresolvers:\n  scratch:\n    kind: server\n    url_env: PBPS_VET2309_SCRATCH\n    externals: externals.sql\n{extra}")).unwrap();
}

fn declaration(d: &Demo, body: &str) {
    std::fs::write(d.dir.join("schema/app.d.type.yml"), body).unwrap();
    let root = Command::new("git")
        .arg("-C")
        .arg(&d.dir)
        .args(["rev-list", "--max-parents=0", "HEAD"])
        .output()
        .unwrap();
    let root = String::from_utf8(root.stdout).unwrap();
    succeeds(d.run(&["plan", "--no-dev", "--since", root.trim()]));
    d.commit();
}

fn plan(d: &Demo, db: &str, scratch: &str, extra: &[&str]) -> Output {
    let path = d.dir.join("vet.plan.json");
    let mut args = vec![
        "plan",
        "--db",
        db,
        "--resolve-with",
        "scratch",
        "--no-rehearse",
        "--out",
        path.to_str().unwrap(),
        "--format",
        "json",
    ];
    args.extend_from_slice(extra);
    d.run_with_env(&args, &[("PBPS_VET2309_SCRATCH", scratch)])
}

// `ext.eqf` is a string body, which no proof reads: a `RETURN` or `BEGIN ATOMIC`
// body that reaches only built-ins is admitted instead (#2563).
const EXTERNALS: &str = "CREATE SCHEMA ext; CREATE FUNCTION ext.bad(text) RETURNS boolean LANGUAGE plpgsql IMMUTABLE AS $$BEGIN RAISE EXCEPTION 'code ran'; END$$; CREATE FUNCTION ext.dflt() RETURNS text LANGUAGE plpgsql AS $$BEGIN RAISE EXCEPTION 'default ran'; END$$; CREATE FUNCTION ext.eqf(text,text) RETURNS boolean LANGUAGE sql IMMUTABLE AS $$SELECT $1=$2$$; CREATE OPERATOR ext.=== (LEFTARG=text, RIGHTARG=text, FUNCTION=ext.eqf);";

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn domain_vet2309_refuses_check_default_operator_and_nested_types_before_writing() {
    for var in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let server = std::env::var(var).unwrap();
        let own = OwnDatabase::new(&server, "domain2309target");
        on_server(own.connection(), "CREATE SCHEMA app");
        on_server(own.connection(), EXTERNALS);
        let scratch = Scratch::new(&server);
        let d = Demo::new("domain2309refusals");
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            own.connection(),
            "--reason",
            "empty managed state",
        ]));
        config(&d, "");
        std::fs::write(d.dir.join("externals.sql"), EXTERNALS).unwrap();
        let cases = [
            (
                "domain: app.d\ntype: text\nchecks: {user_check: 'ext.bad(VALUE)'}\n",
                "CHECK `user_check`",
                "ext.bad",
            ),
            (
                "domain: app.d\ntype: text\ndefault: ext.dflt()\n",
                "DEFAULT",
                "ext.dflt",
            ),
            (
                "domain: app.d\ntype: text\nchecks: {user_operator: \"VALUE OPERATOR(ext.===) 'x'\"}\n",
                "CHECK `user_operator`",
                "ext.eqf",
            ),
        ];
        for (body, part, routine) in cases {
            declaration(&d, body);
            let out = plan(&d, own.connection(), &scratch.connection, &[]);
            scratch.released();
            let text = format!("{}{}", stdout(&out), stderr(&out));
            assert_ne!(code(&out), 0, "{text}");
            for expected in ["app.d", part, routine] {
                assert!(text.contains(expected), "{expected}: {text}");
            }
            assert!(!d.dir.join("vet.plan.json").exists());
        }
        std::fs::write(
            d.dir.join("schema/app.e.type.yml"),
            "enum: app.e\nvalues: [ok]\n",
        )
        .unwrap();
        declaration(
            &d,
            "domain: app.d\ntype: app.e\nchecks: {enum_check: 'ext.bad(VALUE::text)'}\n",
        );
        let out = plan(&d, own.connection(), &scratch.connection, &[]);
        scratch.released();
        assert_ne!(code(&out), 0);
        assert!(
            stdout(&out).contains("enum_check"),
            "{}{}",
            stdout(&out),
            stderr(&out)
        );
        std::fs::write(
            d.dir.join("schema/app.base.type.yml"),
            "domain: app.base\ntype: text\nchecks: {base_check: 'ext.bad(VALUE)'}\n",
        )
        .unwrap();
        declaration(
            &d,
            "domain: app.d\ntype: app.base\nchecks: {outer_check: 'VALUE IS NOT NULL'}\n",
        );
        let out = plan(&d, own.connection(), &scratch.connection, &[]);
        scratch.released();
        assert_ne!(code(&out), 0);
        assert!(
            stdout(&out).contains("app.base") && stdout(&out).contains("base_check"),
            "{}{}",
            stdout(&out),
            stderr(&out)
        );
        assert!(!d.dir.join("vet.plan.json").exists());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn domain_vet2309_compliant_staged_and_no_resolver_plans_keep_apply_vetting() {
    for var in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let server = std::env::var(var).unwrap();
        let own = OwnDatabase::new(&server, "domain2309good");
        on_server(own.connection(), "CREATE SCHEMA app");
        on_server(own.connection(), EXTERNALS);
        let scratch = Scratch::new(&server);
        let d = Demo::new("domain2309good");
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            own.connection(),
            "--reason",
            "empty database",
        ]));
        config(&d, "");
        std::fs::write(d.dir.join("externals.sql"), "").unwrap();
        declaration(
            &d,
            "domain: app.d\ntype: text\ndefault: \"'x'\"\nchecks: {positive: 'length(VALUE) > 0'}\n",
        );
        for extra in [vec![], vec!["--staged"]] {
            succeeds(plan(&d, own.connection(), &scratch.connection, &extra));
            scratch.released();
            let saved: pbps_model::SavedPlan =
                serde_json::from_slice(&std::fs::read(d.dir.join("vet.plan.json")).unwrap())
                    .unwrap();
            assert!(matches!(
                saved.analysis,
                pbps_model::resolver::PlanAnalysis::Ordinary
            ));
        }
        let path = d.dir.join("without.plan.json");
        succeeds(d.run(&[
            "plan",
            "--db",
            own.connection(),
            "--out",
            path.to_str().unwrap(),
        ]));
        scratch.released();
        // The saved no-resolver plan still meets apply's independent vet.
        declaration(
            &d,
            "domain: app.d\ntype: text\nchecks: {user_check: 'ext.bad(VALUE)'}\n",
        );
        succeeds(d.run(&[
            "plan",
            "--db",
            own.connection(),
            "--out",
            path.to_str().unwrap(),
        ]));
        let applied = apply_plan(&d, own.connection(), &path, false);
        scratch.released();
        assert_ne!(code(&applied), 0);
        let text = format!("{}{}", stdout(&applied), stderr(&applied));
        assert!(text.contains("app.d") && text.contains("ext.bad"), "{text}");
        assert_eq!(
            scalar(
                own.connection(),
                "SELECT count(*)::bigint FROM pg_type t JOIN pg_namespace n ON n.oid=t.typnamespace WHERE n.nspname='app' AND t.typname='d'"
            ),
            0
        );
        declaration(
            &d,
            "domain: app.d\ntype: text\nchecks: {positive: 'length(VALUE) > 0'}\n",
        );
        succeeds(d.run(&[
            "plan",
            "--db",
            own.connection(),
            "--out",
            path.to_str().unwrap(),
        ]));
        succeeds(apply_plan(&d, own.connection(), &path, false));
        declaration(
            &d,
            "domain: app.d\ntype: text\nchecks: {user_check: 'ext.bad(VALUE)'}\n",
        );
        std::fs::write(d.dir.join("externals.sql"), EXTERNALS).unwrap();
        std::fs::remove_file(d.dir.join("vet.plan.json")).unwrap();
        let out = plan(&d, own.connection(), &scratch.connection, &[]);
        scratch.released();
        assert_ne!(code(&out), 0);
        assert!(
            stdout(&out).contains("user_check"),
            "{}{}",
            stdout(&out),
            stderr(&out)
        );
        assert!(!d.dir.join("vet.plan.json").exists());
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn domain_vet2309_unavailable_warns_policy_escalates_and_unrelated_plans_do_not_acquire() {
    for var in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let server = std::env::var(var).unwrap();
        let own = OwnDatabase::new(&server, "domain2309warning");
        on_server(own.connection(), "CREATE SCHEMA app");
        let scratch = Scratch::new(&server);
        let d = Demo::new("domain2309warning");
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            own.connection(),
            "--reason",
            "empty database",
        ]));
        config(&d, "");
        std::fs::write(d.dir.join("externals.sql"), "").unwrap();
        let down = "host=127.0.0.1 port=1 user=none dbname=none sslmode=disable";
        declaration(&d, "enum: app.d\nvalues: [ok]\n");
        let out = succeeds(plan(&d, own.connection(), down, &[]));
        scratch.released();
        assert!(!stdout(&out).contains("plan.domain-vet-unavailable"));
        let explain = d.dir.join("enum.plan.json");
        std::fs::copy(d.dir.join("vet.plan.json"), &explain).unwrap();
        declaration(
            &d,
            "domain: app.d\ntype: text\nchecks: {positive: 'length(VALUE) > 0'}\n",
        );
        let path = d.dir.join("vet.plan.json");
        std::fs::remove_file(&path).unwrap();
        // The advisory warning also permits output with the default rehearsal
        // setting: an unavailable scratch must not be retried as a fatal rehearsal.
        let out = succeeds(d.run_with_env(
            &[
                "plan",
                "--db",
                own.connection(),
                "--resolve-with",
                "scratch",
                "--out",
                path.to_str().unwrap(),
                "--format",
                "json",
            ],
            &[("PBPS_VET2309_SCRATCH", down)],
        ));
        scratch.released();
        let saved: pbps_model::SavedPlan =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(
            saved
                .changes
                .changes
                .iter()
                .flat_map(|p| &p.findings)
                .any(|f| f.id == "plan.domain-vet-unavailable")
        );
        let report = json_output(out);
        assert!(
            report["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["id"] == "plan.domain-vet-unavailable" && f["severity"] == "warning"),
            "{report}"
        );
        assert!(path.exists());
        std::fs::remove_file(&path).unwrap();
        config(
            &d,
            "policies:\n  rules:\n    plan.domain-vet-unavailable: error\n",
        );
        let out = plan(&d, own.connection(), down, &[]);
        scratch.released();
        assert_ne!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
        assert!(stdout(&out).contains("plan.domain-vet-unavailable"));
        assert!(!path.exists());
        config(&d, "resolve_with: scratch\n");
        d.commit();
        for args in [
            vec!["plan", "--check"],
            vec!["plan", "--no-dev"],
            vec!["explain", "--plan", explain.to_str().unwrap()],
        ] {
            let out = succeeds(d.run_with_env(&args, &[("PBPS_VET2309_SCRATCH", down)]));
            scratch.released();
            assert!(
                !stdout(&out).contains("plan.domain-vet-unavailable"),
                "{}{}",
                stdout(&out),
                stderr(&out)
            );
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn domain_vet2309_managed_missing_functions_are_named_and_other_compile_errors_warn() {
    for var in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let server = std::env::var(var).unwrap();
        let own = OwnDatabase::new(&server, "domain2309managed");
        on_server(own.connection(), "CREATE SCHEMA app");
        let scratch = Scratch::new(&server);
        let d = Demo::new("domain2309managed");
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            own.connection(),
            "--reason",
            "empty database",
        ]));
        config(&d, "");
        std::fs::write(d.dir.join("externals.sql"), "").unwrap();
        std::fs::write(d.dir.join("schema/app.bad.yml"), "function: app.bad(text)\ndefinition: '(text) RETURNS boolean LANGUAGE sql IMMUTABLE RETURN true'\n").unwrap();
        declaration(
            &d,
            "domain: app.d\ntype: text\nchecks: {managed_check: 'app.bad(VALUE)'}\n",
        );
        let out = plan(&d, own.connection(), &scratch.connection, &[]);
        scratch.released();
        assert_ne!(code(&out), 0);
        let text = format!("{}{}", stdout(&out), stderr(&out));
        for expected in ["app.d", "app.bad", "managed", "does not exist"] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
        assert!(!d.dir.join("vet.plan.json").exists());
        declaration(
            &d,
            "domain: app.d\ntype: text\nchecks: {missing_check: 'ext.absent(VALUE)'}\n",
        );
        let out = succeeds(plan(&d, own.connection(), &scratch.connection, &[]));
        scratch.released();
        assert!(stdout(&out).contains("plan.domain-vet-unavailable"));
        assert!(d.dir.join("vet.plan.json").exists());
        // A valid --dev request follows the independent preview path. Its
        // unreachable explicit dev server must not acquire the selected vet entry.
        config(&d, "resolve_with: scratch\n");
        let out = d.run_with_env(
            &[
                "plan",
                "--dev",
                "host=127.0.0.1 port=1 user=none dbname=none sslmode=disable",
                "--format",
                "json",
            ],
            &[("PBPS_VET2309_SCRATCH", &scratch.connection)],
        );
        scratch.released();
        assert_ne!(code(&out), 0);
        assert!(!stdout(&out).contains("plan.domain-vet-unavailable"));
        assert!(!stdout(&out).contains("plan.domain-code"));
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn domain_vet2309_xml_fragments_use_deployment_pins_with_database_and_role_defaults() {
    for var in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let server = std::env::var(var).unwrap();
        for role_default in [false, true] {
            let own = OwnDatabase::new(&server, "domain2309xml");
            let db = own.connection();
            on_server(
                &server,
                &format!(
                    "ALTER DATABASE {} SET xmloption = document; ALTER DATABASE {} SET TimeZone = 'America/New_York'",
                    own.name, own.name
                ),
            );
            if role_default {
                on_server(
                    &server,
                    &format!(
                        "ALTER ROLE postgres IN DATABASE {} SET xmloption = document",
                        own.name
                    ),
                );
            }
            // The premise must hold on a fresh target session, including when
            // a database-specific role default supplies the ambient setting.
            assert_eq!(
                text_of(db, "SELECT current_setting('xmloption')"),
                "document"
            );
            assert_eq!(
                text_of(db, "SELECT current_setting('TimeZone')"),
                "America/New_York"
            );
            assert!(try_on_server(db, "SELECT '<a/><b/>'::xml").is_err());
            on_server(db, "CREATE SCHEMA app");
            on_server(db, EXTERNALS);
            let scratch = Scratch::new(&server);
            // Pinned settings are excluded from authorization reproduction.
            // Give supplied scratch an ambient document parser too, so its
            // clean server default cannot hide a missing deployment pin.
            let scratch_connection =
                format!("{} options='-c xmloption=document'", scratch.connection);
            assert_eq!(
                text_of(&scratch_connection, "SELECT current_setting('xmloption')"),
                "document"
            );
            let d = Demo::new("domain2309xml");
            d.commit();
            succeeds(d.run(&["baseline", "--db", db, "--reason", "empty database"]));
            std::fs::write(d.dir.join("externals.sql"), "").unwrap();
            declaration(
                &d,
                "domain: app.d\ntype: xml\ndefault: \"'<a/><b/>'::xml\"\nchecks: {compliant: 'VALUE IS NOT NULL'}\n",
            );
            let path = d.dir.join("vet.plan.json");
            for policy in [
                "",
                "policies:\n  rules:\n    plan.domain-vet-unavailable: error\n",
            ] {
                config(&d, policy);
                let report = json_output(succeeds(plan(&d, db, &scratch_connection, &[])));
                scratch.released();
                assert!(
                    !report["findings"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|finding| finding["id"] == "plan.domain-vet-unavailable"),
                    "{var}, role default {role_default}: {report}"
                );
                assert!(path.exists());
                std::fs::remove_file(&path).unwrap();
            }
            // Pinning the parser must not admit user-written code. Keep the
            // XML default so a missing pin would mask the named refusal with
            // an advisory compile failure instead.
            std::fs::write(d.dir.join("externals.sql"), EXTERNALS).unwrap();
            declaration(
                &d,
                "domain: app.d\ntype: xml\ndefault: \"'<a/><b/>'::xml\"\nchecks: {user_check: 'ext.bad(VALUE::text)'}\n",
            );
            let refused = plan(&d, db, &scratch_connection, &[]);
            scratch.released();
            let said = format!("{}{}", stdout(&refused), stderr(&refused));
            assert_ne!(code(&refused), 0, "{said}");
            for expected in ["app.d", "CHECK `user_check`", "ext.bad"] {
                assert!(said.contains(expected), "{expected}: {said}");
            }
            assert!(!said.contains("plan.domain-vet-unavailable"), "{said}");
            assert!(!path.exists());
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn domain2520_statement_tail_refuses_and_literal_semicolons_stay_valid() {
    for variable in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        let server = std::env::var(variable).unwrap();
        let own = OwnDatabase::new(&server, "domain2520target");
        on_server(own.connection(), "CREATE SCHEMA app");
        let scratch = Scratch::new(&server);
        let d = Demo::new("domain2520");
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            own.connection(),
            "--reason",
            "empty database",
        ]));
        config(&d, "");
        std::fs::write(d.dir.join("externals.sql"), "").unwrap();
        declaration(
            &d,
            "domain: app.d\ntype: integer\nchecks:\n  allowed: 'VALUE > 0); CREATE TABLE public.marker2520(id integer); SELECT (true'\n",
        );
        let out = plan(&d, own.connection(), &scratch.connection, &[]);
        for db in [own.connection(), scratch.database.connection()] {
            assert_eq!(
                scalar(
                    db,
                    "SELECT (pg_catalog.to_regclass('public.marker2520') IS NOT NULL)::integer::bigint"
                ),
                0,
                "domain vet executed a trailing marker2520 statement"
            );
        }
        assert_ne!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
        let text = format!("{}{}", stdout(&out), stderr(&out));
        assert!(
            text.contains("app.d") && text.contains("one statement"),
            "{text}"
        );
        scratch.released();
        assert!(!d.dir.join("vet.plan.json").exists());
        declaration(
            &d,
            "domain: app.d\ntype: text\ndefault: \"';'\"\nchecks: {allowed: \"VALUE = ';'\"}\n",
        );
        succeeds(plan(&d, own.connection(), &scratch.connection, &[]));
        scratch.released();
    }
}
