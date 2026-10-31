//! T2's single output collector (DEC-952.1; #621 B / #2488).
//!
//! The fixture puts source only in the target database. The externals file
//! deliberately supplies shapes and harmless routine stand-ins, never that
//! source. A scanner that only sees a draft or an unused routine proves little.
use super::*;
use base64::Engine as _;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
use std::path::Path;

const SOURCES: [&str; 5] = [
    "TRACE2488_PRIVATE_VIEW_s5C8",
    "TRACE2488_PRIVATE_ROUTINE_d9E2",
    "TRACE2488_PRIVATE_TRIGGER_k7H4",
    "TRACE2488_PRIVATE_CHECK_m3J6",
    "TRACE2488_PRIVATE_DEFAULT_p8L1",
];
const DICTIONARY: [&str; 3] = [
    "TRACE2488_PRIVATE_CHOICE_red",
    "TRACE2488_PRIVATE_CHOICE_amber",
    "TRACE2488_PRIVATE_CHOICE_green",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Sink {
    Stdout,
    Stderr,
    Artifact,
    Plan,
    Sql,
    Ledger,
    Snapshot,
    HookArgv,
    HookEnvironment,
    HookStdin,
    Ui,
}

const SINKS: [Sink; 11] = [
    Sink::Stdout,
    Sink::Stderr,
    Sink::Artifact,
    Sink::Plan,
    Sink::Sql,
    Sink::Ledger,
    Sink::Snapshot,
    Sink::HookArgv,
    Sink::HookEnvironment,
    Sink::HookStdin,
    Sink::Ui,
];

#[derive(Default)]
struct Trace {
    forbidden: BTreeSet<Vec<u8>>,
    seen: BTreeSet<Sink>,
}

impl Trace {
    fn new() -> Self {
        let mut trace = Self::default();
        for value in SOURCES.into_iter().chain(DICTIONARY) {
            trace.candidate(value.as_bytes());
        }
        trace
    }

    fn candidate(&mut self, value: &[u8]) {
        assert!(!value.is_empty());
        let digest = Sha256::digest(value);
        self.forbidden.insert(value.to_vec());
        self.forbidden.insert(format!("{digest:x}").into_bytes());
        self.forbidden.insert(format!("{digest:X}").into_bytes());
        self.forbidden.insert(
            base64::engine::general_purpose::STANDARD
                .encode(digest)
                .into_bytes(),
        );
    }

    fn check(&mut self, sink: Sink, bytes: &[u8]) -> Result<(), String> {
        self.seen.insert(sink);
        // Patterns are UTF-8 fixture definitions or ASCII verifiers. Lossy
        // decoding preserves each pattern even in an otherwise binary artifact.
        let text = String::from_utf8_lossy(bytes);
        if self
            .forbidden
            .iter()
            .any(|value| text.contains(std::str::from_utf8(value).unwrap()))
        {
            // Even an assertion failure must not echo a captured definition.
            Err(format!(
                "private definition or unkeyed verifier reached {sink:?}"
            ))
        } else {
            Ok(())
        }
    }

    fn collect(&mut self, sink: Sink, bytes: &[u8]) {
        self.check(sink, bytes).unwrap();
    }

    fn output(&mut self, output: &Output) {
        self.collect(Sink::Stdout, &output.stdout);
        self.collect(Sink::Stderr, &output.stderr);
    }

    fn artifact(&mut self, sink: Sink, path: &Path) -> Vec<u8> {
        let bytes = std::fs::read(path).unwrap();
        self.collect(Sink::Artifact, &bytes);
        if sink != Sink::Artifact {
            self.collect(sink, &bytes);
        }
        bytes
    }

    fn rows(&mut self, raw: &str) {
        self.collect(Sink::Ledger, raw.as_bytes());
        let rows: Vec<serde_json::Value> = serde_json::from_str(raw).unwrap();
        assert!(!rows.is_empty());
        for row in rows {
            self.collect(
                Sink::Snapshot,
                row["state_json"].as_str().unwrap().as_bytes(),
            );
        }
    }

    fn ui(&mut self, bytes: &[u8]) {
        self.collect(Sink::Ui, bytes);
    }

    fn files(&mut self, root: &Path) {
        for entry in std::fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                self.files(&path);
            } else if kind.is_file() {
                let bytes = self.artifact(Sink::Artifact, &path);
                match path.extension().and_then(|s| s.to_str()) {
                    Some("stdout") => self.collect(Sink::Stdout, &bytes),
                    Some("stderr") => self.collect(Sink::Stderr, &bytes),
                    Some("argv") => self.collect(Sink::HookArgv, &bytes),
                    Some("environment") => self.collect(Sink::HookEnvironment, &bytes),
                    Some("stdin") => self.collect(Sink::HookStdin, &bytes),
                    _ => {}
                }
            } else {
                panic!(
                    "trace cannot inspect a non-regular artifact at {}",
                    path.display()
                );
            }
        }
    }

    fn complete(&self) {
        assert_eq!(
            self.seen,
            SINKS.into_iter().collect(),
            "unobserved output sink"
        );
    }
}

#[test]
fn every_sink_rejects_planted_plaintext_and_all_dictionary_verifiers() {
    let d = Demo::new("trace2488-planted");
    let directory = d.dir.join("planted");
    std::fs::create_dir(&directory).unwrap();
    let status = Command::new(BIN).arg("--version").output().unwrap().status;
    assert!(status.success());
    for sink in SINKS {
        // Generate controls independently of the scanner's pattern set. A
        // forgotten encoding must still be planted and fail this assertion.
        for literal in SOURCES.into_iter().chain(DICTIONARY) {
            let digest = Sha256::digest(literal.as_bytes());
            for value in [
                literal.to_owned(),
                format!("{digest:x}"),
                format!("{digest:X}"),
                base64::engine::general_purpose::STANDARD.encode(digest),
            ] {
                let planted = format!("surrounding output\n{value}\nend of output");
                assert!(
                    Trace::new().check(sink, planted.as_bytes()).is_err(),
                    "{sink:?} missed an encoding"
                );
            }
        }
        // Pin the actual collection adapters as well as the scanner. Snapshot
        // uses a JSON escape so only decoding the row exposes the planted value.
        let extension = match sink {
            Sink::Artifact => Some("bin"),
            Sink::Plan => Some("json"),
            Sink::Sql => Some("sql"),
            Sink::HookArgv => Some("argv"),
            Sink::HookEnvironment => Some("environment"),
            Sink::HookStdin => Some("stdin"),
            Sink::Stdout | Sink::Stderr | Sink::Ledger | Sink::Snapshot | Sink::Ui => None,
        };
        let path = directory.join(format!("leak.{}", extension.unwrap_or("unused")));
        if extension.is_some() {
            std::fs::write(&path, SOURCES[0]).unwrap();
        }
        let rejected = std::panic::catch_unwind(|| {
            let mut trace = Trace::new();
            match sink {
                Sink::Stdout | Sink::Stderr => trace.output(&Output {
                    status,
                    stdout: if sink == Sink::Stdout {
                        SOURCES[0].as_bytes().to_vec()
                    } else {
                        Vec::new()
                    },
                    stderr: if sink == Sink::Stderr {
                        SOURCES[0].as_bytes().to_vec()
                    } else {
                        Vec::new()
                    },
                }),
                Sink::Artifact | Sink::HookArgv | Sink::HookEnvironment | Sink::HookStdin => {
                    trace.files(&directory)
                }
                Sink::Plan | Sink::Sql => {
                    trace.artifact(sink, &path);
                }
                Sink::Ledger => trace.rows(
                    &serde_json::json!([{ "state_json": "{}", "operator": SOURCES[0] }])
                        .to_string(),
                ),
                Sink::Snapshot => {
                    let raw = serde_json::json!([{ "state_json": SOURCES[0] }]).to_string();
                    let escaped = format!("\\u0054{}", &SOURCES[0][1..]);
                    trace.rows(&raw.replace(SOURCES[0], &escaped));
                }
                Sink::Ui => trace.ui(SOURCES[0].as_bytes()),
            }
        });
        if extension.is_some() {
            std::fs::remove_file(path).unwrap();
        }
        assert!(
            rejected.is_err(),
            "{sink:?} collector missed the planted sink"
        );
    }
}

#[test]
fn a_missing_sink_and_an_unreadable_artifact_cannot_pass_as_empty() {
    assert!(std::panic::catch_unwind(|| Trace::new().complete()).is_err());
    let d = Demo::new("trace2488-missing");
    let absent = d.dir.join("absent");
    assert!(std::panic::catch_unwind(|| Trace::new().files(&absent)).is_err());
    // A directory cannot be read as artifact bytes, even with DAC override.
    assert!(std::panic::catch_unwind(|| Trace::new().artifact(Sink::Artifact, &d.dir)).is_err());
    #[cfg(unix)]
    if !rustix::process::geteuid().is_root() {
        // Root bypasses mode bits; the I/O failure above covers every uid.
        use std::os::unix::fs::PermissionsExt as _;
        let denied = d.dir.join("unreadable");
        std::fs::write(&denied, b"public bytes").unwrap();
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o000)).unwrap();
        let rejected = std::panic::catch_unwind(|| Trace::new().files(&d.dir));
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(rejected.is_err());
    }
    let mut trace = Trace::new();
    for sink in SINKS {
        trace.collect(sink, b"public identity, type and checksum");
    }
    trace.complete();
}

const EXTERNALS: &str = "CREATE SCHEMA IF NOT EXISTS ext;\n\
    CREATE TABLE ext.t (id integer);\n\
    CREATE VIEW ext.v AS SELECT NULL::integer AS id, NULL::text AS label WHERE false;\n\
    CREATE FUNCTION ext.f(numeric) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT 1';\n";

struct Fixture {
    slug: String,
    d: Demo,
    own: OwnDatabase,
    _scratch: resolver_publication::Confined,
    env: Vec<(String, String)>,
    trace: Trace,
}

impl Fixture {
    fn new(server: &str, slug: &str, docker: bool, old: bool) -> Self {
        let own = OwnDatabase::new(server, slug);
        let scratch = resolver_publication::Confined::new(server);
        let d = Demo::new(slug);
        on_server(
            own.connection(),
            &format!(
                "CREATE SCHEMA app; CREATE SCHEMA ext;\n\
                 CREATE TABLE ext.t (id integer DEFAULT length('{}'), \
                   CONSTRAINT private_check CHECK (id <> length('{}')));\n\
                 CREATE VIEW ext.v AS SELECT id, '{}'::text || '{}'::text AS label FROM ext.t;\n\
                 CREATE FUNCTION ext.f(numeric) RETURNS integer LANGUAGE sql IMMUTABLE \
                   AS $$SELECT length('{}')$$;\n\
                 CREATE FUNCTION ext.fire() RETURNS trigger LANGUAGE plpgsql AS \
                   $$BEGIN PERFORM '{}'; RETURN NEW; END$$;\n\
                 CREATE TRIGGER private_trigger BEFORE INSERT ON ext.t FOR EACH ROW \
                   EXECUTE FUNCTION ext.fire();",
                SOURCES[4], SOURCES[3], SOURCES[0], DICTIONARY[2], SOURCES[1], SOURCES[2]
            ),
        );
        std::fs::write(
            d.dir.join("schema/app.t.yml"),
            "table: app.t\ncolumns:\n  k: {type: int, nullable: false}\n  \
             c: {type: int, default: 'ext.f(1)'}\nprimary_key: [k]\n",
        )
        .unwrap();
        std::fs::write(
            d.dir.join("schema/app.v.view.yml"),
            "view: app.v\ndefinition: SELECT id, label FROM ext.v\n",
        )
        .unwrap();
        let profile = if docker {
            let image = std::env::var(if old {
                "PBPS_TEST_PG_OLD_SCRATCH_IMAGE"
            } else {
                "PBPS_TEST_PG_SCRATCH_IMAGE"
            })
            .expect("export both pinned PG scratch images before flow_pg");
            format!("kind: docker\n    image: {image}\n    externals: externals.sql")
        } else {
            "kind: server\n    url_env: PBPS_TRACE_SCRATCH\n    externals: externals.sql".into()
        };
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!(
                "dialect: postgres\nresolvers:\n  scratch:\n    {profile}\n\
                 environments:\n  prod:\n    url_env: PBPS_TRACE_TARGET\n    \
                 fingerprint_key_env: PBPS_TRACE_KEY\n    resolve_with: scratch\n"
            ),
        )
        .unwrap();
        std::fs::write(d.dir.join("externals.sql"), EXTERNALS).unwrap();
        std::fs::create_dir(d.dir.join("child-tmp")).unwrap();
        let env = vec![
            ("PBPS_TRACE_TARGET".into(), own.connection().into()),
            ("PBPS_TRACE_SCRATCH".into(), scratch.connection.clone()),
            ("PBPS_TRACE_KEY".into(), fingerprint_key(9)),
            (
                "TMPDIR".into(),
                d.dir.join("child-tmp").display().to_string(),
            ),
        ];
        let mut fixture = Self {
            slug: slug.into(),
            d,
            own,
            _scratch: scratch,
            env,
            trace: Trace::new(),
        };
        let preview = fixture.d.dir.join("preview.json");
        fixture.ok(&["plan", "--no-dev", "--out", preview.to_str().unwrap()]);
        fixture.d.commit();
        fixture.ok(&["bootstrap", "--env", "prod"]);
        std::fs::write(
            fixture.d.dir.join("schema/ext.f%28integer%29.function.yml"),
            "function: ext.f(integer)\npublic_execute: true\ndefinition: |-\n  \
             (integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN 1\n",
        )
        .unwrap();
        fixture.ok(&["plan", "--no-dev"]);
        fixture.d.commit();
        fixture
    }

    fn run(&mut self, args: &[&str]) -> Output {
        self.run_at(&self.d.dir.clone(), args)
    }

    fn run_at(&mut self, project: &Path, args: &[&str]) -> Output {
        let output = Command::new(BIN)
            .current_dir(project)
            .env_remove("PBPS_TRACE_KEY")
            .arg("--project")
            .arg(project)
            .args(args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .output()
            .unwrap();
        self.trace.output(&output);
        self.trace.files(&self.d.dir);
        output
    }

    fn ok(&mut self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert_eq!(code(&output), 0, "{args:?}: {}", stderr(&output));
        output
    }

    fn plan(&mut self, file: &str) -> serde_json::Value {
        let path = self.d.dir.join(file);
        let sql = self.d.dir.join("plan.sql");
        self.ok(&[
            "plan",
            "--env",
            "prod",
            "--resolve-with",
            "scratch",
            "--out",
            path.to_str().unwrap(),
            "--sql",
            sql.to_str().unwrap(),
            "--format",
            "json",
        ]);
        let bytes = self.trace.artifact(Sink::Plan, &path);
        self.trace.artifact(Sink::Sql, &sql);
        let plan: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(plan["origin"], "resolved_database");
        plan
    }

    fn ledger(&mut self) {
        let raw = text_of(
            self.own.connection(),
            "SELECT COALESCE(json_agg(row_to_json(s))::text, '[]') FROM public.__pbps_state s",
        );
        self.trace.rows(&raw);
    }

    fn draft(&mut self) {
        let path = self.d.dir.join("draft.sql");
        self.ok(&[
            "resolver",
            "externals",
            "--env",
            "prod",
            "--schema",
            "ext",
            "--out",
            path.to_str().unwrap(),
        ]);
        let draft = std::fs::read_to_string(path).unwrap();
        assert!(draft.contains("CREATE VIEW ext.v AS SELECT NULL::integer AS id"));
        assert!(!draft.contains("private_trigger") && !draft.contains("private_check"));
        // Read the engine's complete definitions, not guessed spelling. The
        // public shape is excluded; definitions and dictionary alternatives aren't.
        for query in [
            "SELECT pg_get_viewdef('ext.v'::regclass, true)",
            "SELECT pg_get_functiondef('ext.f(numeric)'::regprocedure)",
            "SELECT pg_get_functiondef('ext.fire()'::regprocedure)",
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'private_check'",
            "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef WHERE adrelid = 'ext.t'::regclass",
        ] {
            let definition = text_of(self.own.connection(), query);
            assert!(SOURCES.iter().any(|s| definition.contains(s)));
            self.trace.candidate(definition.as_bytes());
            if definition.contains(DICTIONARY[2]) {
                for candidate in DICTIONARY {
                    self.trace
                        .candidate(definition.replace(DICTIONARY[2], candidate).as_bytes());
                }
            }
        }
    }

    fn hooks(&mut self) {
        let hook = self.d.dir.join("record-hook.sh");
        executable(
            &hook,
            "#!/bin/sh\nset -eu\nmkdir -p hook-records\n\
            cat /proc/$$/cmdline /proc/$PPID/cmdline > \"hook-records/$PBPS_HOOK.argv\"\n\
            env > \"hook-records/$PBPS_HOOK.environment\"\n\
            cat > \"hook-records/$PBPS_HOOK.stdin\"\n",
        );
        let path = self.d.dir.join("pbps.yml");
        let mut config = std::fs::read_to_string(&path).unwrap();
        config.push_str(
            "hooks:\n  on_apply: ./record-hook.sh\n  \
            on_apply_attempt: ./record-hook.sh\n  on_drift: ./record-hook.sh\n",
        );
        std::fs::write(path, config).unwrap();
    }

    fn hook_record(&mut self, name: &str) -> serde_json::Value {
        let path = self.d.dir.join(format!("hook-records/{name}"));
        let argv = std::fs::read(path.with_extension("argv")).unwrap();
        let environment = std::fs::read(path.with_extension("environment")).unwrap();
        let stdin = std::fs::read(path.with_extension("stdin")).unwrap();
        assert!(String::from_utf8_lossy(&argv).contains("record-hook.sh"));
        assert!(
            String::from_utf8_lossy(&environment)
                .lines()
                .any(|v| v == format!("PBPS_HOOK={name}"))
        );
        self.trace.collect(Sink::HookArgv, &argv);
        self.trace.collect(Sink::HookEnvironment, &environment);
        self.trace.collect(Sink::HookStdin, &stdin);
        let payload: serde_json::Value = serde_json::from_slice(&stdin).unwrap();
        assert!(payload.is_object());
        payload
    }

    fn drift(&mut self) {
        // verify compares the recorded state with the database, independently
        // of pending declarations. Arrange real drift and restore it first.
        on_server(
            self.own.connection(),
            "ALTER TABLE app.t ALTER COLUMN k TYPE bigint",
        );
        let output = self.run(&["verify", "--env", "prod", "--format", "json"]);
        on_server(
            self.own.connection(),
            "ALTER TABLE app.t ALTER COLUMN k TYPE integer",
        );
        assert_eq!(code(&output), 2, "the managed column type must be drift");
        self.hook_record("on_drift");
        self.plan("second.json");
    }

    fn apply(&mut self, file: &str, success: bool) {
        let path = self.d.dir.join(file);
        let checksum = plan_checksum(&path);
        let output = self.run(&[
            "apply",
            "--env",
            "prod",
            "--plan",
            path.to_str().unwrap(),
            "--checksum",
            &checksum,
            "--allow",
            "rename,destructive,constraint",
        ]);
        assert_eq!(code(&output) == 0, success, "{}", stderr(&output));
        self.ledger();
        let payload = self.hook_record("on_apply_attempt");
        assert_eq!(
            payload["outcome"],
            if success { "success" } else { "failure" }
        );
        if success {
            assert!(self.hook_record("on_apply")["analysis"]["evidence"].is_object());
        }
    }

    fn readers(&mut self) {
        for format in ["human", "json"] {
            for args in [
                vec!["status", "--format", format],
                vec!["state", "list", "--env", "prod", "--format", format],
                vec!["state", "show", "1", "--env", "prod", "--format", format],
                vec![
                    "state", "diff", "1", "2", "--env", "prod", "--format", format,
                ],
            ] {
                self.ok(&args);
            }
        }
        let exported = self.d.dir.join("exported");
        std::fs::create_dir(&exported).unwrap();
        std::fs::write(
            exported.join("pbps.yml"),
            "dialect: postgres\nenvironments:\n  prod:\n    url_env: PBPS_TRACE_TARGET\n",
        )
        .unwrap();
        let output = self.run_at(
            &exported,
            &["state", "export", "2", "--env", "prod", "--format", "json"],
        );
        assert_eq!(code(&output), 0, "{}", stderr(&output));
        self.ledger();
    }

    fn refusal(&mut self, extra: &[&str], expected: &str, message: &str) {
        let plan = self.d.dir.join("first.json");
        let sql = self.d.dir.join("plan.sql");
        let prior = (std::fs::read(&plan).unwrap(), std::fs::read(&sql).unwrap());
        let mut args = vec![
            "plan",
            "--env",
            "prod",
            "--out",
            plan.to_str().unwrap(),
            "--sql",
            sql.to_str().unwrap(),
            "--format",
            "json",
        ];
        args.extend(extra);
        let output = self.run(&args);
        assert_ne!(code(&output), 0, "refusal unexpectedly published");
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(
            report["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["id"] == expected),
            "{report}"
        );
        assert!(stdout(&output).contains(message), "{report}");
        assert_eq!(std::fs::read(&plan).unwrap(), prior.0);
        assert_eq!(std::fs::read(&sql).unwrap(), prior.1);
    }

    fn refusals(&mut self, docker: bool) {
        self.refusal(
            &["--resolve-with", "absent"],
            "resolver.selection",
            "absent",
        );
        self.refusal(&["--staged"], "resolver.unresolved", "--staged");
        let keyed = self.env.clone();
        self.env.retain(|(key, _)| key != "PBPS_TRACE_KEY");
        self.refusal(&[], "resolver.key", "fingerprint");
        self.env = keyed;

        let reader = format!(
            "trace2488_reader_{}_{:08x}",
            std::process::id(),
            rand::random::<u32>()
        );
        let roles = ClusterRoles {
            server: self.own.connection().into(),
            names: vec![reader.clone()],
        };
        on_server(
            self.own.connection(),
            &format!(
                "CREATE ROLE {reader} LOGIN NOSUPERUSER PASSWORD 'trigger-test'; GRANT pg_read_all_data, pg_read_all_settings TO {reader}"
            ),
        );
        let revoke = ControlSystemExecuteRevoked::new(self.own.connection());
        let before = self.env.clone();
        self.env
            .iter_mut()
            .find(|(key, _)| key == "PBPS_TRACE_TARGET")
            .unwrap()
            .1 = as_role(self.own.connection(), &reader);
        self.refusal(&[], "resolver.identity", "identity");
        self.env = before;
        drop(revoke);
        drop(roles);

        let externals = self.d.dir.join("externals.sql");
        std::fs::write(
            &externals,
            format!("{EXTERNALS}\nSELECT no_such_trace2488_function();\n"),
        )
        .unwrap();
        self.refusal(&[], "plan.failed", "no_such_trace2488_function");
        std::fs::write(
            &externals,
            format!("{EXTERNALS}\nSELECT pg_terminate_backend(pg_backend_pid());\n"),
        )
        .unwrap();
        self.refusal(
            &[],
            "plan.failed",
            if docker {
                "connection"
            } else {
                "cleanup was not confirmed"
            },
        );
        std::fs::write(&externals, EXTERNALS).unwrap();
        // The next real run must recover its own abandoned claim. It cannot
        // silently classify unreadable cleanup state as an empty scratch.
        self.plan("second.json");
        self.ledger();
    }

    fn docker_disposal_failure(&mut self) {
        let real = Command::new("sh")
            .args(["-c", "command -v docker"])
            .output()
            .unwrap();
        assert!(real.status.success());
        let real = String::from_utf8(real.stdout).unwrap().trim().to_owned();
        let bin = self.d.dir.join("disposal-bin");
        std::fs::create_dir(&bin).unwrap();
        let created = self.d.dir.join("disposal.created");
        let attempted = self.d.dir.join("disposal.attempted");
        executable(&bin.join("docker"), DISPOSAL_SHIM);
        let before = self.env.clone();
        self.env.extend([
            (
                "PATH".into(),
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            ),
            ("PBPS_TRACE_DOCKER_REAL".into(), real.clone()),
            ("PBPS_TRACE_CREATED".into(), created.display().to_string()),
            (
                "PBPS_TRACE_ATTEMPTED".into(),
                attempted.display().to_string(),
            ),
        ]);
        // Hold the output until the exact run-created ID has been cleaned up,
        // even if a privacy assertion is the reason this control fails.
        let output = Command::new(BIN)
            .current_dir(&self.d.dir)
            .arg("--project")
            .arg(&self.d.dir)
            .args(["plan", "--env", "prod", "--format", "json"])
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .output()
            .unwrap();
        self.env = before;
        let id = std::fs::read_to_string(created).unwrap();
        let id = id.trim();
        assert!(id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_disposal_attempts(&std::fs::read_to_string(attempted).unwrap(), id);
        assert!(
            Command::new(&real)
                .args(["rm", "--force", "--volumes", id])
                .output()
                .unwrap()
                .status
                .success()
        );
        self.trace.output(&output);
        self.trace.files(&self.d.dir);
        assert_ne!(code(&output), 0);
        assert!(stdout(&output).contains("cleanup was not confirmed"));
    }

    fn publication_refusal(&mut self, old: bool, docker: bool) {
        let alternate = if old { server() } else { old_server() };
        let identity = "SELECT system_identifier::text FROM pg_catalog.pg_control_system()";
        assert_ne!(
            text_of(self.own.connection(), identity),
            text_of(&alternate, identity)
        );
        // The alternate needs only the same database name and fixture login:
        // lineage is checked before any catalog or ledger is trusted there.
        let other = OwnDatabase::new(&alternate, &self.slug);
        let primary_sessions = if docker { 3 } else { 2 };
        let proxy = RecheckTarget::new(self.own.connection(), other.connection(), primary_sessions);
        let before = self.env.clone();
        self.env
            .iter_mut()
            .find(|(key, _)| key == "PBPS_TRACE_TARGET")
            .unwrap()
            .1 = proxy.connection(self.own.connection());
        self.refusal(&[], "resolver.recheck", "another cluster");
        self.env = before;
        assert_eq!(
            proxy.count.load(std::sync::atomic::Ordering::Acquire),
            primary_sessions + 1
        );
        drop(proxy);
        drop(other);
    }

    fn stale_input_refusal(&mut self) {
        let original = text_of(
            self.own.connection(),
            "SELECT pg_get_functiondef('ext.f(numeric)'::regprocedure)",
        );
        on_server(
            self.own.connection(),
            "CREATE OR REPLACE FUNCTION ext.f(numeric) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT 2'",
        );
        self.apply("second.json", false);
        on_server(self.own.connection(), &original);
        assert!(
            text_of(
                self.own.connection(),
                "SELECT pg_get_functiondef('ext.f(numeric)'::regprocedure)"
            ) == original,
            "fixture definition was not restored"
        );
    }

    fn render_failure(&mut self) {
        let mut plan: serde_json::Value =
            serde_json::from_slice(&std::fs::read(self.d.dir.join("preview.json")).unwrap())
                .unwrap();
        let table = plan["changes"]["changes"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|c| c["op"] == "create_table")
            .unwrap();
        table["table"]["columns"] = serde_json::json!({});
        let path = self.d.dir.join("unrenderable.json");
        std::fs::write(&path, serde_json::to_vec(&plan).unwrap()).unwrap();
        let output = self.run(&[
            "explain",
            "--plan",
            path.to_str().unwrap(),
            "--format",
            "json",
        ]);
        assert_ne!(code(&output), 0);
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["findings"][0]["id"], "plan.unexplainable");
    }

    fn launcher(&self) -> PathBuf {
        let path = self.d.dir.join("ui-launcher.sh");
        let mut script = String::from("#!/bin/sh\nset -eu\n");
        for (key, value) in &self.env {
            script.push_str(&format!("export {key}={}\n", quoted(value)));
        }
        // Client deliberately drops stderr; preserve both real streams before
        // it does. Trigger's bounded browser payload cannot hide a late leak.
        script.push_str(&format!("set +e\n{} \"$@\" > ui-$$.stdout 2> ui-$$.stderr\nresult=$?\ncat ui-$$.stdout\ncat ui-$$.stderr >&2\nexit \"$result\"\n", quoted(BIN)));
        executable(&path, &script);
        path
    }

    fn ui_plan(&mut self, file: &str, apply: bool) {
        let mut trigger = Ui::new(self.launcher(), self.d.dir.clone());
        self.ui_action(
            &mut trigger,
            "plan",
            serde_json::json!({"environment":"prod", "out":file}),
        );
        let path = self.d.dir.join(file);
        self.trace
            .collect(Sink::Plan, &std::fs::read(&path).unwrap());
        if apply {
            self.ui_action(&mut trigger, "apply", serde_json::json!({"environment":"prod", "plan":file, "checksum":plan_checksum(&path), "allow":["rename","destructive","constraint"], "staged":false,"resume":false}));
        }
    }

    fn ui_action(&mut self, trigger: &mut Ui, action: &str, body: serde_json::Value) {
        let started = trigger.answer(action, &serde_json::to_vec(&body).unwrap());
        self.trace.ui(&started);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        loop {
            let response = trigger.answer("runs", b"{}");
            self.trace.ui(&response);
            let runs: Vec<serde_json::Value> = serde_json::from_slice(&response).unwrap();
            assert_eq!(runs.len(), 1);
            if runs[0]["ended"] == true {
                assert_eq!(runs[0]["code"], 0, "{action}: {}", runs[0]["stderr"]);
                self.trace.files(&self.d.dir);
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "UI child never completed"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn ui_readers(&mut self) {
        use pbps_ui::client::{Client, View};
        let client = Client {
            executable: self.launcher(),
            project: self.d.dir.clone(),
        };
        for view in [
            View::Status,
            View::Drift("prod".into()),
            View::Plan("second.json".into()),
            View::Timeline("prod".into()),
            View::Docs,
        ] {
            let payload = client.read(&view).unwrap();
            self.trace.ui(&payload);
            self.trace.files(&self.d.dir);
        }
    }

    fn offline(&mut self, file: &str, plan: &serde_json::Value) {
        // A directory outside the project prevents ancestor/path discovery
        // from satisfying an accidental new dependency on project files.
        let export = OfflineExport(std::env::temp_dir().join(format!(
            "trace2488-offline-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        )));
        let empty = &export.0;
        std::fs::create_dir(empty).unwrap();
        let path = empty.join("plan.json");
        std::fs::copy(self.d.dir.join(file), &path).unwrap();
        assert_eq!(std::fs::read_dir(empty).unwrap().count(), 1);
        for json in [false, true] {
            let mut command = Command::new(BIN);
            command
                .current_dir(empty)
                .arg("explain")
                .arg("--plan")
                .arg(&path);
            for (key, _) in &self.env {
                command.env_remove(key);
            }
            command.env("TMPDIR", empty).env(
                "DOCKER_HOST",
                format!("unix://{}/unavailable.sock", empty.display()),
            );
            if json {
                command.args(["--format", "json"]);
            }
            let output = command.output().unwrap();
            self.trace.output(&output);
            self.trace.files(&self.d.dir);
            self.trace.files(empty);
            assert_eq!(code(&output), 0, "{}", stderr(&output));
            let text = stdout(&output);
            let records = plan["analysis"]["evidence"]["before"]["prerequisites"]
                .as_array()
                .unwrap();
            for record in records {
                assert!(
                    !text.contains(record["properties"].as_str().unwrap()),
                    "explain exported a verifier"
                );
            }
            if json {
                let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                for shown in value["data"]["resolver"]["prerequisites"]
                    .as_array()
                    .unwrap()
                {
                    assert_eq!(
                        shown
                            .as_object()
                            .unwrap()
                            .keys()
                            .map(String::as_str)
                            .collect::<BTreeSet<_>>(),
                        BTreeSet::from(["object", "fingerprinted"])
                    );
                }
            }
        }
    }
}

struct OfflineExport(PathBuf);

impl Drop for OfflineExport {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Route only the publication session to another real cluster (DEC-1685.1).
/// The planner and resolver each open a target session first; Docker acquisition
/// also binds its target before the resolver runs. The following session is
/// the fresh publication recheck. Bytes are forwarded unchanged in both
/// directions: a manufactured protocol reply would not test this boundary.
struct RecheckTarget {
    address: std::net::SocketAddr,
    count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl RecheckTarget {
    fn new(primary: &str, alternate: &str, primary_sessions: usize) -> Self {
        use std::net::{Shutdown, TcpListener, TcpStream};
        use std::sync::atomic::Ordering;
        let endpoint = |connection: &str| {
            let field = |prefix: &str| {
                connection
                    .split_whitespace()
                    .find_map(|s| s.strip_prefix(prefix))
                    .unwrap()
                    .to_owned()
            };
            (field("host="), field("port=").parse::<u16>().unwrap())
        };
        let primary = endpoint(primary);
        let alternate = endpoint(alternate);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = count.clone();
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut copies = Vec::new();
            for client in listener.incoming() {
                let mut client = client.unwrap();
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                let index = seen.fetch_add(1, Ordering::AcqRel);
                let destination = if index < primary_sessions {
                    &primary
                } else {
                    &alternate
                };
                let mut server =
                    TcpStream::connect((destination.0.as_str(), destination.1)).unwrap();
                let mut client_read = client.try_clone().unwrap();
                let mut server_write = server.try_clone().unwrap();
                copies.push(std::thread::spawn(move || {
                    let _ = std::io::copy(&mut client_read, &mut server_write);
                    let _ = server_write.shutdown(Shutdown::Write);
                }));
                copies.push(std::thread::spawn(move || {
                    let _ = std::io::copy(&mut server, &mut client);
                    let _ = client.shutdown(Shutdown::Write);
                }));
            }
            for copy in copies {
                copy.join().unwrap();
            }
        });
        Self {
            address,
            count,
            stop,
            thread: Some(thread),
        }
    }

    fn connection(&self, original: &str) -> String {
        let retained = original
            .split_whitespace()
            .filter(|s| !s.starts_with("host=") && !s.starts_with("port="))
            .collect::<Vec<_>>()
            .join(" ");
        format!("host=127.0.0.1 port={} {retained}", self.address.port())
    }
}

impl Drop for RecheckTarget {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        let _ = std::net::TcpStream::connect(self.address);
        let result = self.thread.take().unwrap().join();
        if !std::thread::panicking() {
            result.unwrap();
        }
    }
}

// Use the public HTTP boundary: Trigger is intentionally private to the UI.
// The idle listener lives only until this integration-test process exits.
struct Ui {
    address: String,
    token: String,
}

impl Ui {
    fn new(executable: PathBuf, project: PathBuf) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let viewer = pbps_ui::Viewer::bind(pbps_ui::Config {
                executable,
                project,
                token: [0x48; 32],
                docs_style_hash: "A".repeat(44),
            })
            .unwrap();
            sender.send(viewer.url()).unwrap();
            viewer.serve().unwrap();
        });
        let url = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let (address, token) = url
            .strip_prefix("http://")
            .unwrap()
            .split_once("/#")
            .unwrap();
        Self {
            address: address.into(),
            token: token.into(),
        }
    }

    fn answer(&mut self, action: &str, body: &[u8]) -> Vec<u8> {
        use std::io::{Read as _, Write as _};
        let mut stream = std::net::TcpStream::connect(&self.address).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(90)))
            .unwrap();
        write!(stream, "POST /api/trigger/{action} HTTP/1.1\r\nHost: {}\r\nOrigin: http://{}\r\nX-Pbps-Token: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", self.address, self.address, self.token, body.len()).unwrap();
        stream.write_all(body).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        assert!(
            response.starts_with(b"HTTP/1.1 200 "),
            "UI refused the test request"
        );
        let split = response.windows(4).position(|v| v == b"\r\n\r\n").unwrap() + 4;
        response[split..].to_vec()
    }
}

const DISPOSAL_SHIM: &str = r#"#!/bin/sh
set -eu
if [ "$1" = run ]; then
  set +e
  "$PBPS_TRACE_DOCKER_REAL" "$@" > "$PBPS_TRACE_CREATED"
  pbps_trace_status=$?
  set -e
  cat "$PBPS_TRACE_CREATED"
  exit "$pbps_trace_status"
fi
if [ "$1" = rm ]; then
  # Recovery precedes this run opening its capture (DEC-2410.3). Once opened,
  # even an empty capture keeps every target subject to the disposal assertion.
  # Recorder-only controls omit CREATED and always exercise recording.
  if [ -n "${PBPS_TRACE_CREATED:-}" ] && [ ! -e "$PBPS_TRACE_CREATED" ]; then
    exec "$PBPS_TRACE_DOCKER_REAL" "$@"
  fi
  # Drop may record another attempt while the parent reads this append-only log.
  printf '%s\n' "$@" "" >> "$PBPS_TRACE_ATTEMPTED"
  echo 'forced trace Docker disposal failure' >&2
  exit 1
fi
exec "$PBPS_TRACE_DOCKER_REAL" "$@"
"#;

fn assert_disposal_attempts(recorded: &str, id: &str) {
    // A blank line commits one argv record; a late writer may still be appending.
    let (complete, _) = recorded
        .rsplit_once("\n\n")
        .expect("at least one disposal attempt must be recorded");
    assert!(
        !complete.is_empty(),
        "at least one disposal attempt must be recorded"
    );
    for attempt in complete.split("\n\n") {
        let mut args = attempt.split('\n');
        assert_eq!(args.next(), Some("rm"));
        let removed = args.filter(|v| !v.starts_with('-')).collect::<Vec<_>>();
        assert_eq!(
            removed,
            [id],
            "cleanup must target exactly this run's new container"
        );
    }
}

#[test]
fn disposal_records_require_at_least_one_attempt_and_only_the_owned_container() {
    let owned = "rm\n--force\n--volumes\nowned\n\n";
    assert_disposal_attempts(&format!("{owned}{owned}rm\n--force"), "owned");
    for recorded in [
        String::new(),
        "\n\n".into(),
        "rm\n--force\nowned\n".into(),
        "rm\n--force\nowned\nforeign\n\n".into(),
        "rm\n--force\nowned \n\n".into(),
        "rm\n--force\nowned\n\n\n".into(),
        format!("{owned}rm\n--force\nforeign\n\n"),
        format!("rm\n--force\nforeign\n\n{owned}"),
        format!("{owned}rm\n--force\n\n"),
    ] {
        assert!(std::panic::catch_unwind(|| assert_disposal_attempts(&recorded, "owned")).is_err());
    }
}

#[cfg(unix)]
#[test]
fn disposal_control_separates_prior_recovery_from_the_new_runs_cleanup() {
    let dir = Demo::new("trace2560-recovery");
    let shim = dir.dir.join("docker");
    let created = dir.dir.join("disposal.created");
    let attempted = dir.dir.join("disposal.attempted");
    let forwarded = dir.dir.join("forwarded");
    let id = "a".repeat(64);
    let prior = "b".repeat(64);
    std::fs::write(&shim, DISPOSAL_SHIM).unwrap();
    // The real-runtime stand-in is an interpreter reading these files. No
    // daemon-visible orphan or freshly written executable is involved.
    std::fs::write(
        dir.dir.join("run"),
        "printf '%s\\n' run \"$@\" \"\" >> \"$PBPS_TRACE_FORWARDED\"\n\
         printf '%s\\n' \"$PBPS_TRACE_CONTROL_ID\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.dir.join("rm"),
        "printf '%s\\n' rm \"$@\" \"\" >> \"$PBPS_TRACE_FORWARDED\"\n\
         exit \"${PBPS_TRACE_CONTROL_STATUS:-0}\"\n",
    )
    .unwrap();
    let command = |args: &[&str]| {
        let mut command = Command::new("sh");
        command
            .arg(&shim)
            .args(args)
            .current_dir(&dir.dir)
            .env("PBPS_TRACE_DOCKER_REAL", "sh")
            .env("PBPS_TRACE_CREATED", &created)
            .env("PBPS_TRACE_ATTEMPTED", &attempted)
            .env("PBPS_TRACE_FORWARDED", &forwarded)
            .env("PBPS_TRACE_CONTROL_ID", &id)
            .env("PBPS_TRACE_CONTROL_STATUS", "0");
        command
    };
    let prior_rm = ["rm", "--force", "--volumes", &prior];
    // An opened but empty capture is already the current run's phase. It
    // must not turn a foreign cleanup into unobserved recovery.
    std::fs::write(&created, "").unwrap();
    assert_eq!(command(&prior_rm).output().unwrap().status.code(), Some(1));
    let record = std::fs::read_to_string(&attempted).unwrap();
    assert!(std::panic::catch_unwind(|| assert_disposal_attempts(&record, &id)).is_err());
    assert_eq!(
        std::fs::read_to_string(&forwarded).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    std::fs::remove_file(&created).unwrap();
    std::fs::remove_file(&attempted).unwrap();

    assert!(command(&prior_rm).output().unwrap().status.success());
    assert_eq!(
        command(&prior_rm)
            .env("PBPS_TRACE_CONTROL_STATUS", "37")
            .output()
            .unwrap()
            .status
            .code(),
        Some(37),
        "recovery preserves the real runtime's failure status"
    );
    assert_eq!(
        std::fs::read_to_string(&attempted).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    let run = command(&["run", "--detach", "fixture-image"])
        .output()
        .unwrap();
    assert!(run.status.success());
    assert_eq!(stdout(&run), format!("{id}\n"));
    assert_eq!(
        std::fs::read_to_string(&created).unwrap(),
        format!("{id}\n")
    );
    let recovery = format!("rm\n--force\n--volumes\n{prior}\n\n");
    let before = std::fs::read_to_string(&forwarded).unwrap();
    assert_eq!(
        before,
        format!("{recovery}{recovery}run\n--detach\nfixture-image\n\n")
    );

    let own_rm = ["rm", "--force", "--volumes", &id];
    assert_eq!(command(&own_rm).output().unwrap().status.code(), Some(1));
    assert_disposal_attempts(&std::fs::read_to_string(&attempted).unwrap(), &id);
    assert_eq!(command(&prior_rm).output().unwrap().status.code(), Some(1));
    let record = std::fs::read_to_string(&attempted).unwrap();
    assert!(std::panic::catch_unwind(|| assert_disposal_attempts(&record, &id)).is_err());
    assert_eq!(std::fs::read_to_string(&forwarded).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn disposal_records_survive_a_blocked_late_writer() {
    use std::io::Write as _;
    use std::process::Stdio;

    struct Reap(std::process::Child);
    impl Drop for Reap {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let dir = Demo::new("trace2533-blocked");
    let attempted = dir.dir.join("disposal.attempted");
    let shim = dir.dir.join("docker");
    let ready = dir.dir.join("ready");
    let id = "a".repeat(64);
    executable(&shim, DISPOSAL_SHIM);
    let command = || {
        let mut command = Command::new(&shim);
        command
            .args(["rm", "--force", "--volumes", &id])
            .env("PBPS_TRACE_ATTEMPTED", &attempted);
        command
    };
    assert_eq!(
        stand_in::retry_exec(|| command().output())
            .unwrap()
            .status
            .code(),
        Some(1)
    );
    assert!(std::fs::metadata(&attempted).unwrap().len() > 0);
    // Redirection opens before this function runs. With the old `>` recorder,
    // ready therefore proves that the previous record has been truncated.
    executable(
        &shim,
        &DISPOSAL_SHIM.replacen(
            "set -eu\n",
            "set -eu\nprintf() {\n  : > \"$PBPS_TRACE_BLOCKED\"\n  IFS= read -r release\n  command printf \"$@\"\n}\n",
            1,
        ),
    );
    let mut late = Reap(
        stand_in::retry_exec(|| {
            command()
                .env("PBPS_TRACE_BLOCKED", &ready)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
        })
        .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !ready.exists() {
        assert!(
            late.0.try_wait().unwrap().is_none(),
            "late shim exited early"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "late shim never blocked"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let during = std::fs::read_to_string(&attempted).unwrap();
    assert!(late.0.try_wait().unwrap().is_none());
    late.0
        .stdin
        .take()
        .unwrap()
        .write_all(b"release\n")
        .unwrap();
    assert_eq!(late.0.wait().unwrap().code(), Some(1));
    assert_disposal_attempts(&during, &id);
    let after = std::fs::read_to_string(&attempted).unwrap();
    assert_disposal_attempts(&after, &id);
    assert_eq!(after.matches("\n\n").count(), 2);
}

fn quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn executable(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

fn has_namespace(object: &serde_json::Value, namespace: &str) -> bool {
    object["name"]
        .as_array()
        .is_some_and(|names| names.first().and_then(|v| v.as_str()) == Some(namespace))
        || object["signature"]
            .as_array()
            .is_some_and(|s| s.iter().any(|o| has_namespace(o, namespace)))
}

fn retained_classes(plan: &serde_json::Value) {
    let records = plan["analysis"]["evidence"]["before"]["prerequisites"]
        .as_array()
        .unwrap();
    for class in [
        "pg_rewrite",
        "pg_proc",
        "pg_trigger",
        "pg_constraint",
        "pg_attrdef",
    ] {
        assert!(
            records
                .iter()
                .any(|r| r["object"]["class"] == class && has_namespace(&r["object"], "ext")),
            "the fixture did not capture {class}"
        );
    }
}

fn keyed_records_differ(first: &serde_json::Value, second: &serde_json::Value) {
    let before = |plan: &serde_json::Value| plan["analysis"]["evidence"]["before"].clone();
    let a = before(first);
    let b = before(second);
    assert_ne!(a["key_id"], b["key_id"]);
    let records = b["prerequisites"].as_array().unwrap();
    for record in a["prerequisites"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| has_namespace(&r["object"], "ext"))
    {
        let other = records
            .iter()
            .find(|r| r["object"] == record["object"])
            .unwrap();
        assert_ne!(
            record["properties"], other["properties"],
            "a retained property became a key-independent guessing oracle"
        );
    }
}

fn check(server: &str, slug: &str, docker: bool, old: bool) {
    let mut fixture = Fixture::new(server, slug, docker, old);
    fixture.draft();
    let first = fixture.plan("first.json");
    retained_classes(&first);
    fixture.offline("first.json", &first);
    fixture
        .env
        .iter_mut()
        .find(|(k, _)| k == "PBPS_TRACE_KEY")
        .unwrap()
        .1 = fingerprint_key(10);
    let second = fixture.plan("second.json");
    keyed_records_differ(&first, &second);
    fixture.render_failure();
    fixture.refusals(docker);
    if docker {
        fixture.docker_disposal_failure();
    }
    fixture.publication_refusal(old, docker);
    fixture.hooks();
    fixture.drift();
    fixture.ui_plan("ui-before.json", false);
    fixture.apply("first.json", false);
    fixture.stale_input_refusal();
    fixture.apply("second.json", true);
    fixture.readers();
    fixture.ui_readers();
    fixture.ui_plan("ui-after.json", true);
    fixture.ledger();
    fixture.trace.complete();
}

#[test]
#[ignore = "needs live pg18"]
fn pg18_server_keeps_definition_sources_out_of_every_sink() {
    check(&server(), "trace2488-18s", false, false);
}

#[test]
#[ignore = "needs live pg16"]
fn pg16_server_keeps_definition_sources_out_of_every_sink() {
    check(&old_server(), "trace2488-16s", false, true);
}

#[test]
#[ignore = "needs live pg18 and Docker"]
fn pg18_docker_keeps_definition_sources_out_of_every_sink() {
    check(&server(), "trace2488-18d", true, false);
}

#[test]
#[ignore = "needs live pg16 and Docker"]
fn pg16_docker_keeps_definition_sources_out_of_every_sink() {
    check(&old_server(), "trace2488-16d", true, true);
}
