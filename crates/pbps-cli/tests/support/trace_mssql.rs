//! SQL Server's remaining T2 cells (#2488; DEC-952.1).
use super::*;
use std::path::Path;
use std::time::{Duration, Instant};

#[path = "trace/collector.rs"]
mod collector;
use collector::*;

const BASE: &str =
    "table: dbo.base\ncolumns:\n  id: {type: int, nullable: false}\nprimary_key: [id]\n";
const UNAVAILABLE_SCRATCH: &str =
    "Server=127.0.0.1,1;User Id=sa;Password=not-used;Database=master;TrustServerCertificate=true";
const EXTERNALS: &str = "CREATE SCHEMA ext;\nGO\n\
    CREATE TABLE ext.t(id int, computed int);\nGO\n\
    CREATE FUNCTION ext.f() RETURNS int AS BEGIN RETURN 1; END;\nGO\n\
    CREATE VIEW ext.v AS SELECT id, CAST(NULL AS nvarchar(100)) AS label, \
      ext.f() AS amount, computed FROM ext.t;\nGO\n";

struct Fixture {
    d: Demo,
    target: OwnDatabase,
    scratch: Option<OwnDatabase>,
    env: Vec<(String, String)>,
    trace: Trace,
    slowest: Duration,
}

impl Fixture {
    fn new(docker: bool) -> Self {
        let slug = if docker { "trace2488d" } else { "trace2488s" };
        let target = OwnDatabase::new(&std::env::var("PBPS_TEST_DB").unwrap(), slug);
        let scratch = (!docker)
            .then(|| OwnDatabase::new(&std::env::var("PBPS_TEST_MSSQL_SCRATCH_DB").unwrap(), slug));
        let d = Demo::new(slug);
        on_server(target.connection(), "CREATE SCHEMA ext;");
        on_server(
            target.connection(),
            &format!(
                "CREATE TABLE ext.t(id int CONSTRAINT private_default DEFAULT LEN(N'{}'), \
             computed AS (id + LEN(N'{}')), CONSTRAINT private_check CHECK (id <> LEN(N'{}')));",
                SOURCES[4], SOURCES[5], SOURCES[3]
            ),
        );
        on_server(target.connection(), &function());
        on_server(
            target.connection(),
            &format!(
                "CREATE TRIGGER ext.private_trigger ON ext.t AFTER INSERT AS \
             BEGIN IF LEN(N'{}') < 0 THROW 51088, N'unreachable fixture branch', 1; END;",
                SOURCES[2]
            ),
        );
        on_server(
            target.connection(),
            &format!(
                "CREATE VIEW ext.v AS SELECT id, CASE WHEN id IS NULL THEN CAST(NULL AS nvarchar(100)) ELSE CAST(N'{}' + N'{}' AS nvarchar(100)) END AS label, ext.f() AS amount, computed FROM ext.t;",
                SOURCES[0], DICTIONARY[2]
            ),
        );
        let profile = if docker {
            format!(
                "kind: docker\n    image: {}",
                std::env::var("PBPS_TEST_RESOLVER_MSSQL_IMAGE").unwrap()
            )
        } else {
            "kind: server\n    url_env: PBPS_TRACE_SCRATCH".into()
        };
        std::fs::write(d.dir.join("pbps.yml"), format!(
            "dialect: mssql\nresolvers:\n  scratch:\n    {profile}\n    externals: externals.sql\n\
             environments:\n  prod:\n    url_env: PBPS_TRACE_TARGET\n    fingerprint_key_env: PBPS_TRACE_KEY\n    resolve_with: scratch\n")).unwrap();
        std::fs::write(d.dir.join("externals.sql"), EXTERNALS).unwrap();
        std::fs::write(d.dir.join("schema/dbo.base.yml"), BASE).unwrap();
        let temporary = d.dir.join("child-tmp");
        std::fs::create_dir(&temporary).unwrap();
        let env = vec![
            ("PBPS_TRACE_TARGET".into(), target.connection().into()),
            (
                "PBPS_TRACE_SCRATCH".into(),
                scratch
                    .as_ref()
                    .map_or_else(String::new, |s| s.connection().into()),
            ),
            ("PBPS_TRACE_KEY".into(), key(9)),
            ("TMPDIR".into(), temporary.display().to_string()),
        ];
        let mut fixture = Self {
            d,
            target,
            scratch,
            env,
            trace: Trace::new(),
            slowest: Duration::ZERO,
        };
        // Resolution asks about retained bindings (DEC-1515.1). A newly
        // created view alone is rebuilt by definition and needs no resolver.
        std::fs::write(
            fixture.d.dir.join("schema/dbo.anchor.view.yml"),
            "view: dbo.anchor\ndefinition: SELECT id, label, amount, computed FROM ext.v\n",
        )
        .unwrap();
        fixture.remember_definitions();
        fixture.ok(&["plan", "--no-dev", "--out", "preview.json"]);
        fixture.d.commit();
        fixture.ok(&["bootstrap", "--env", "prod"]);
        std::fs::write(
            fixture.view(),
            "view: dbo.managed\ndefinition: SELECT id, label, amount, computed FROM ext.v\n",
        )
        .unwrap();
        fixture.ok(&["plan", "--no-dev"]);
        fixture.d.commit();
        fixture
    }

    fn view(&self) -> PathBuf {
        self.d.dir.join("schema/dbo.managed.view.yml")
    }

    fn command(&self, directory: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        command
            .current_dir(directory)
            .arg("--project")
            .arg(directory)
            .args(args)
            .env_remove("PBPS_TRACE_KEY")
            .env_remove("PBPS_TRACE_SCRATCH")
            .envs(self.env.iter().map(|(k, v)| (k, v)));
        command
    }

    fn run(&mut self, args: &[&str]) -> Output {
        let started = Instant::now();
        let result = self.command(&self.d.dir, args).output().unwrap();
        self.slowest = self.slowest.max(started.elapsed());
        self.trace.output(&result);
        self.trace.files(&self.d.dir);
        result
    }

    fn ok(&mut self, args: &[&str]) -> Output {
        let result = self.run(args);
        assert_eq!(code(&result), 0, "{args:?}: {}", stderr(&result));
        result
    }

    fn plan(&mut self, file: &str) -> serde_json::Value {
        self.ok(&[
            "plan", "--env", "prod", "--out", file, "--sql", "plan.sql", "--format", "json",
        ]);
        let plan = self.trace.artifact(Sink::Plan, &self.d.dir.join(file));
        self.trace.artifact(Sink::Sql, &self.d.dir.join("plan.sql"));
        let plan: serde_json::Value = serde_json::from_slice(&plan).unwrap();
        assert_eq!(plan["origin"], "resolved_database");
        let prerequisites = plan["analysis"]["evidence"]["before"]["prerequisites"]
            .as_array()
            .unwrap();
        for name in [
            "v",
            "f",
            "private_trigger",
            "private_check",
            "private_default",
            "computed",
        ] {
            assert!(
                prerequisites.iter().any(|r| {
                    let object = &r["object"];
                    object["name"].as_array().unwrap().iter().any(|v| v == name)
                        && (has_namespace(object, "ext")
                            || object["signature"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|s| has_namespace(s, "ext")))
                }),
                "definition class {name} was not captured"
            );
        }
        plan
    }

    fn remember_definitions(&mut self) {
        // Register the engine's complete spelling as well as literal sentinels:
        // a bare digest of the whole definition must not escape either.
        for query in [
            "SELECT definition FROM sys.sql_modules WHERE object_id=OBJECT_ID(N'ext.v');",
            "SELECT definition FROM sys.sql_modules WHERE object_id=OBJECT_ID(N'ext.f');",
            "SELECT definition FROM sys.sql_modules WHERE object_id=OBJECT_ID(N'ext.private_trigger');",
            "SELECT definition FROM sys.check_constraints WHERE name=N'private_check';",
            "SELECT definition FROM sys.default_constraints WHERE name=N'private_default';",
            "SELECT definition FROM sys.computed_columns WHERE object_id=OBJECT_ID(N'ext.t') AND name=N'computed';",
        ] {
            let definition = text(self.target.connection(), query);
            assert!(
                SOURCES.iter().any(|source| definition.contains(source)),
                "fixture definition was absent"
            );
            self.trace.candidate(definition.as_bytes());
            if definition.contains(DICTIONARY[2]) {
                for candidate in DICTIONARY {
                    self.trace
                        .candidate(definition.replace(DICTIONARY[2], candidate).as_bytes());
                }
            }
        }
    }

    fn draft_refusal(&mut self) {
        // Drafting still reads PostgreSQL bindings; the SQL Server refusal is
        // itself an output surface whose bytes must pass the trace collector.
        let result = self.run(&[
            "resolver",
            "externals",
            "--env",
            "prod",
            "--schema",
            "ext",
            "--out",
            "draft.sql",
        ]);
        assert_ne!(code(&result), 0);
        assert!(
            stderr(&result).contains("reads a PostgreSQL target")
                && stderr(&result).contains("SQL Server has no binding adapter"),
            "{}",
            stderr(&result)
        );
        assert!(!self.d.dir.join("draft.sql").try_exists().unwrap());
    }

    fn offline(&mut self, file: &str) {
        let export = OfflineExport(std::env::temp_dir().join(format!(
            "pbps-trace2488-offline-{:016x}",
            rand::random::<u64>()
        )));
        std::fs::create_dir(&export.0).unwrap();
        std::fs::copy(self.d.dir.join(file), export.0.join("plan.json")).unwrap();
        let plan: serde_json::Value =
            serde_json::from_slice(&std::fs::read(export.0.join("plan.json")).unwrap()).unwrap();
        for format in ["human", "json"] {
            let result = Command::new(BIN)
                .current_dir(&export.0)
                .args(["explain", "--plan", "plan.json", "--format", format])
                .env_remove("PBPS_TRACE_TARGET")
                .env_remove("PBPS_TRACE_SCRATCH")
                .env_remove("PBPS_TRACE_KEY")
                .env_remove("PBPS_TEST_DB")
                .env_remove("PBPS_TEST_MSSQL_SCRATCH_DB")
                .env("DOCKER_HOST", "unix:///pbps-trace2488-no-socket")
                .output()
                .unwrap();
            self.trace.output(&result);
            assert_eq!(code(&result), 0);
            for record in plan["analysis"]["evidence"]["before"]["prerequisites"]
                .as_array()
                .unwrap()
            {
                assert!(
                    !stdout(&result).contains(record["properties"].as_str().unwrap()),
                    "offline explanation exposed a fingerprint value"
                );
            }
            if format == "json" {
                let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
                assert_eq!(report["data"]["applyable"], true);
                assert!(
                    report["data"]["approve_with"]
                        .as_str()
                        .unwrap()
                        .contains("apply")
                );
                let detail = &report["data"]["resolver"];
                assert_eq!(detail["entry"], "scratch");
                assert!(!detail["prerequisites"].as_array().unwrap().is_empty());
                assert!(
                    detail["prerequisites"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|p| p.get("properties").is_none())
                );
            }
        }
        self.trace.files(&export.0);
    }

    fn refused(&mut self, extra: &[&str], expected: &str) {
        let before = std::fs::read(self.d.dir.join("second.json")).unwrap();
        let mut args = vec![
            "plan",
            "--env",
            "prod",
            "--out",
            "second.json",
            "--format",
            "json",
        ];
        args.extend_from_slice(extra);
        let result = self.run(&args);
        assert_ne!(code(&result), 0);
        assert!(
            stdout(&result).contains(expected),
            "expected {expected}: {}",
            stdout(&result)
        );
        assert_eq!(
            std::fs::read(self.d.dir.join("second.json")).unwrap(),
            before
        );
    }

    fn refusals(&mut self) {
        self.refused(&["--resolve-with", "absent"], "resolver.selection");
        self.refused(&["--staged"], "resolver.unresolved");
        let configured = self.env.clone();
        self.env.retain(|(name, _)| name != "PBPS_TRACE_KEY");
        self.refused(&[], "resolver.key");
        self.env = configured;
        // The target itself is never an acceptable scratch, even with an alias.
        let before_alias = self.env.clone();
        self.env
            .push(("PBPS_TRACE_ALIAS".into(), self.target.connection().into()));
        let configured = std::fs::read(self.d.dir.join("pbps.yml")).unwrap();
        std::fs::write(self.d.dir.join("pbps.yml"),
            "dialect: mssql\nresolvers:\n  scratch: {kind: server, url_env: PBPS_TRACE_ALIAS, externals: externals.sql}\nenvironments:\n  prod: {url_env: PBPS_TRACE_TARGET, fingerprint_key_env: PBPS_TRACE_KEY, resolve_with: scratch}\n").unwrap();
        self.refused(&[], "target database");
        self.env = before_alias;
        std::fs::write(self.d.dir.join("pbps.yml"), configured).unwrap();
        std::fs::write(
            self.d.dir.join("externals.sql"),
            format!("{EXTERNALS}\nTHROW 51088, N'trace2488 compilation refused', 1;\n"),
        )
        .unwrap();
        self.refused(&[], "trace2488 compilation refused");
        std::fs::write(self.d.dir.join("externals.sql"), EXTERNALS).unwrap();
    }

    fn publication_refusal(&mut self, docker: bool) {
        let before = std::fs::read(self.d.dir.join("second.json")).unwrap();
        std::fs::write(
            self.d.dir.join("externals.sql"),
            format!("{EXTERNALS}\n/*trace2488_publication_wait*/ WAITFOR DELAY '00:00:15';\n"),
        )
        .unwrap();
        let prior = if docker {
            docker_subjects()
        } else {
            Vec::new()
        };
        let mut child = self
            .command(
                &self.d.dir,
                &[
                    "plan",
                    "--env",
                    "prod",
                    "--out",
                    "second.json",
                    "--format",
                    "json",
                ],
            )
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        let observed = loop {
            if child.try_wait().unwrap().is_some() {
                break false;
            }
            let scratch = if docker {
                docker_connection(child.id(), &prior)
            } else {
                Some(self.scratch.as_ref().unwrap().connection().to_owned())
            };
            if let Some(scratch) = scratch {
                let waiting = try_text(
                    &scratch,
                    "SELECT CONVERT(nvarchar(10),COUNT(*)) FROM sys.dm_exec_requests r CROSS APPLY sys.dm_exec_sql_text(r.sql_handle) t WHERE r.session_id<>@@SPID AND r.command=N'WAITFOR' AND t.text LIKE N'%trace2488_publication_wait%';",
                );
                if waiting.is_ok_and(|n| n != "0") {
                    break true;
                }
            }
            if started.elapsed() >= Duration::from_secs(120) {
                break false;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        if observed {
            on_server(
                self.target.connection(),
                &function().replace("RETURN LEN(", "RETURN 1 + LEN("),
            );
            self.remember_definitions();
        } else if child.try_wait().unwrap().is_none() {
            child.kill().unwrap();
        }
        let result = child.wait_with_output().unwrap();
        // Restore the engine state before inspecting a refusal or private bytes.
        on_server(self.target.connection(), &function());
        std::fs::write(self.d.dir.join("externals.sql"), EXTERNALS).unwrap();
        self.trace.output(&result);
        self.trace.files(&self.d.dir);
        assert!(
            observed,
            "publication barrier was never observed: {}",
            stderr(&result)
        );
        assert_ne!(code(&result), 0);
        assert!(
            stdout(&result).contains("resolver.recheck"),
            "{}",
            stdout(&result)
        );
        assert_eq!(
            std::fs::read(self.d.dir.join("second.json")).unwrap(),
            before
        );
    }

    fn apply_plan(&mut self, file: &str) -> Output {
        let checksum = plan_checksum(&self.d.dir.join(file));
        self.run(&[
            "apply",
            "--env",
            "prod",
            "--plan",
            file,
            "--checksum",
            &checksum,
            "--allow",
            "rename,destructive,constraint",
            "--format",
            "json",
        ])
    }

    fn latest_state_id(&self) -> String {
        let id = text(
            self.target.connection(),
            "SELECT CONVERT(nvarchar(20), MAX(id)) FROM dbo.__pbps_state;",
        );
        id.parse::<u64>().unwrap();
        id
    }

    fn resolved_outputs(&mut self) -> (String, String) {
        self.d.commit();
        self.plan("resolved-cli.json");
        let checksum = plan_checksum(&self.d.dir.join("resolved-cli.json"));
        let plan_bytes = std::fs::read(self.d.dir.join("resolved-cli.json")).unwrap();
        let before = self.ledger_rows();
        let configured = self.env.clone();
        self.env
            .iter_mut()
            .find(|(name, _)| name == "PBPS_TRACE_KEY")
            .unwrap()
            .1 = key(11);
        let wrong_key = self.apply_plan("resolved-cli.json");
        let ui_wrong_key = self.ui_result(
            "apply",
            serde_json::json!({
                "environment":"prod", "plan":"resolved-cli.json", "checksum":checksum,
                "allow":["rename","destructive","constraint"], "staged":false,"resume":false
            }),
        );
        self.env = configured;
        assert_ne!(code(&wrong_key), 0);
        assert!(stdout(&wrong_key).contains("fingerprint key"));
        assert_ne!(ui_wrong_key["code"].as_i64().unwrap(), 0);
        assert!(
            serde_json::to_string(&ui_wrong_key)
                .unwrap()
                .contains("fingerprint key")
        );
        assert_eq!(self.ledger_rows(), before);
        self.ledger();

        on_server(
            self.target.connection(),
            &function().replace("RETURN LEN(", "RETURN 1 + LEN("),
        );
        self.remember_definitions();
        let stale = self.apply_plan("resolved-cli.json");
        on_server(self.target.connection(), &function());
        assert_ne!(code(&stale), 0);
        assert!(stdout(&stale).contains("before its first statement"));
        assert_eq!(self.ledger_rows(), before);
        assert_eq!(
            text(
                self.target.connection(),
                "SELECT CONVERT(nvarchar(10), COUNT(*)) FROM sys.views WHERE object_id=OBJECT_ID(N'dbo.managed');"
            ),
            "0"
        );
        assert_eq!(
            std::fs::read(self.d.dir.join("resolved-cli.json")).unwrap(),
            plan_bytes
        );
        self.ledger();

        // Sealed apply reads the target alone (DEC-2144.1). An unavailable
        // scratch URL and Docker daemon must not be consulted to record it.
        let configured = self.env.clone();
        self.env
            .iter_mut()
            .find(|(name, _)| name == "PBPS_TRACE_SCRATCH")
            .unwrap()
            .1 = UNAVAILABLE_SCRATCH.into();
        self.env.push((
            "DOCKER_HOST".into(),
            "unix:///pbps-trace2488-no-socket".into(),
        ));
        let applied = self.apply_plan("resolved-cli.json");
        self.env = configured;
        assert_eq!(code(&applied), 0, "{}", stderr(&applied));
        assert_eq!(self.hook("on_apply_attempt")["outcome"], "success");
        assert!(self.hook("on_apply")["analysis"]["evidence"].is_object());
        let cli_id = self.latest_state_id();
        self.ledger();
        on_server(
            self.target.connection(),
            "INSERT INTO ext.t(id) VALUES (7);",
        );
        assert_eq!(
            text(
                self.target.connection(),
                "SELECT CONVERT(nvarchar(20), id) FROM dbo.managed;"
            ),
            "7"
        );

        std::fs::write(self.view(), "view: dbo.managed\ndefinition: SELECT id + 1 AS id, label, amount, computed FROM ext.v\n").unwrap();
        self.d.commit();
        self.ui(
            "plan",
            serde_json::json!({"environment":"prod", "out":"resolved-ui.json"}),
        );
        let bytes = self
            .trace
            .artifact(Sink::Plan, &self.d.dir.join("resolved-ui.json"));
        let plan: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(plan["origin"], "resolved_database");
        assert!(!plan["changes"]["changes"].as_array().unwrap().is_empty());
        let checksum = plan_checksum(&self.d.dir.join("resolved-ui.json"));
        let configured = self.env.clone();
        self.env
            .iter_mut()
            .find(|(name, _)| name == "PBPS_TRACE_SCRATCH")
            .unwrap()
            .1 = UNAVAILABLE_SCRATCH.into();
        self.env.push((
            "DOCKER_HOST".into(),
            "unix:///pbps-trace2488-no-socket".into(),
        ));
        self.ui(
            "apply",
            serde_json::json!({
                "environment":"prod", "plan":"resolved-ui.json", "checksum":checksum,
                "allow":["rename","destructive","constraint"], "staged":false,"resume":false
            }),
        );
        self.env = configured;
        assert_eq!(
            text(
                self.target.connection(),
                "SELECT CONVERT(nvarchar(20), id) FROM dbo.managed;"
            ),
            "8"
        );
        assert_eq!(self.hook("on_apply_attempt")["outcome"], "success");
        assert!(self.hook("on_apply")["analysis"]["evidence"].is_object());
        let ui_id = self.latest_state_id();
        assert_ne!(cli_id, ui_id);
        self.ledger();
        (cli_id, ui_id)
    }

    fn render_failure(&mut self) {
        let mut preview: serde_json::Value =
            serde_json::from_slice(&std::fs::read(self.d.dir.join("preview.json")).unwrap())
                .unwrap();
        let table = preview["changes"]["changes"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|c| c["op"] == "create_table")
            .unwrap();
        table["table"]["columns"] = serde_json::json!({});
        std::fs::write(
            self.d.dir.join("unrenderable.json"),
            serde_json::to_vec(&preview).unwrap(),
        )
        .unwrap();
        let result = self.run(&["explain", "--plan", "unrenderable.json", "--format", "json"]);
        assert_ne!(code(&result), 0);
        assert!(stdout(&result).contains("plan.unexplainable"));
    }

    fn server_cleanup_failure(&mut self) {
        let scratch = self.scratch.as_ref().unwrap().connection().to_owned();
        std::fs::write(
            self.d.dir.join("externals.sql"),
            format!(
                "{EXTERNALS}\n\
            CREATE TABLE ext.cleanup_refusal(i int); CREATE INDEX ix ON ext.cleanup_refusal(i);\n\
            EXEC sys.sp_addextendedproperty @name=N'cleanup_property', @value=N'held', \
            @level0type=N'SCHEMA',@level0name=N'ext',@level1type=N'TABLE', \
            @level1name=N'cleanup_refusal',@level2type=N'INDEX',@level2name=N'ix';\n"
            ),
        )
        .unwrap();
        let result = self
            .command(&self.d.dir, &["plan", "--env", "prod", "--format", "json"])
            .output()
            .unwrap();
        // Restore fixture-owned objects before an assertion can unwind. The
        // original report must still retain the unconfirmed cleanup.
        after_test_on_server(
            &scratch,
            "IF OBJECT_ID(N'ext.cleanup_refusal') IS NOT NULL DROP TABLE ext.cleanup_refusal;",
        );
        std::fs::write(self.d.dir.join("externals.sql"), EXTERNALS).unwrap();
        self.trace.output(&result);
        self.trace.files(&self.d.dir);
        assert_ne!(code(&result), 0);
        assert!(stdout(&result).contains("cleanup"), "{}", stdout(&result));
        assert!(
            stdout(&result).contains("cleanup_property"),
            "{}",
            stdout(&result)
        );
    }

    fn docker_cleanup_failure(&mut self) {
        let real = Command::new("sh")
            .args(["-c", "command -v docker"])
            .output()
            .unwrap();
        assert!(real.status.success());
        let real = String::from_utf8(real.stdout).unwrap().trim().to_owned();
        let bin = self.d.dir.join("disposal-bin");
        std::fs::create_dir(&bin).unwrap();
        executable(&bin.join("docker"), DISPOSAL_SHIM);
        let created = self.d.dir.join("disposal.created");
        let attempted = self.d.dir.join("disposal.attempted");
        let mut command = self.command(&self.d.dir, &["plan", "--env", "prod", "--format", "json"]);
        command
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .env("PBPS_TRACE_DOCKER_REAL", &real)
            .env("PBPS_TRACE_CREATED", &created)
            .env("PBPS_TRACE_ATTEMPTED", &attempted);
        let result = command.output().unwrap();
        let id = std::fs::read_to_string(created).unwrap();
        let id = id.trim();
        assert!(id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()));
        let removed = Command::new(&real)
            .args(["rm", "--force", "--volumes", id])
            .output()
            .unwrap();
        assert!(removed.status.success());
        assert_disposal_attempts(&std::fs::read_to_string(attempted).unwrap(), id);
        self.trace.output(&result);
        self.trace.files(&self.d.dir);
        assert_ne!(code(&result), 0);
        assert!(stdout(&result).contains("cleanup was not confirmed"));
    }

    fn ledger_rows(&self) -> String {
        text(
            self.target.connection(),
            "SELECT * FROM dbo.__pbps_state ORDER BY id FOR JSON PATH;",
        )
    }

    fn ledger(&mut self) {
        let rows = self.ledger_rows();
        self.trace.rows(&rows);
    }

    fn hooks(&self) {
        executable(
            &self.d.dir.join("record-hook.sh"),
            "#!/bin/sh\nset -eu\nmkdir -p hook-records\ncat /proc/$$/cmdline /proc/$PPID/cmdline > \"hook-records/$PBPS_HOOK.argv\"\nenv > \"hook-records/$PBPS_HOOK.environment\"\ncat > \"hook-records/$PBPS_HOOK.stdin\"\n",
        );
        let path = self.d.dir.join("pbps.yml");
        let mut config = std::fs::read_to_string(&path).unwrap();
        config.push_str("hooks:\n  on_apply: ./record-hook.sh\n  on_apply_attempt: ./record-hook.sh\n  on_drift: ./record-hook.sh\n");
        std::fs::write(path, config).unwrap();
    }

    fn hook(&mut self, name: &str) -> serde_json::Value {
        let stem = self.d.dir.join(format!("hook-records/{name}"));
        for (sink, extension) in [
            (Sink::HookArgv, "argv"),
            (Sink::HookEnvironment, "environment"),
            (Sink::HookStdin, "stdin"),
        ] {
            self.trace.collect(
                sink,
                &std::fs::read(stem.with_extension(extension)).unwrap(),
            );
        }
        let payload: serde_json::Value =
            serde_json::from_slice(&std::fs::read(stem.with_extension("stdin")).unwrap()).unwrap();
        assert!(payload.is_object());
        payload
    }

    fn launcher(&self) -> PathBuf {
        capture_launcher(&self.d.dir, Path::new(BIN), &self.env)
    }

    fn ui(&mut self, action: &str, body: serde_json::Value) {
        assert_eq!(self.ui_result(action, body)["code"].as_i64().unwrap(), 0);
    }

    fn ui_result(&mut self, action: &str, body: serde_json::Value) -> serde_json::Value {
        let mut ui = Ui::new(self.launcher(), self.d.dir.clone());
        self.trace
            .ui(&ui.answer(action, &serde_json::to_vec(&body).unwrap()));
        let started = Instant::now();
        loop {
            let response = ui.answer("runs", b"{}");
            self.trace.ui(&response);
            let mut runs: Vec<serde_json::Value> = serde_json::from_slice(&response).unwrap();
            assert_eq!(runs.len(), 1);
            if runs[0]["ended"] == true {
                self.trace.files(&self.d.dir);
                return runs.remove(0);
            }
            ui_child_wait(self.slowest, started.elapsed(), action, &runs[0]);
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn readers(&mut self, cli_id: &str, ui_id: &str) {
        for format in ["human", "json"] {
            for args in [
                vec!["status", "--format", format],
                vec!["state", "list", "--env", "prod", "--format", format],
                vec!["state", "show", cli_id, "--env", "prod", "--format", format],
                vec!["state", "show", ui_id, "--env", "prod", "--format", format],
                vec![
                    "state", "diff", cli_id, ui_id, "--env", "prod", "--format", format,
                ],
            ] {
                self.ok(&args);
            }
        }
        use pbps_ui::client::{Client, View};
        let client = Client {
            executable: self.launcher(),
            project: self.d.dir.clone(),
        };
        for view in [
            View::Status,
            View::Drift("prod".into()),
            View::Plan("resolved-ui.json".into()),
            View::Timeline("prod".into()),
            View::Docs,
        ] {
            read_ui(&mut self.trace, &client, &self.d.dir, &view);
        }
        let exported = self.d.dir.join("exported");
        std::fs::create_dir(&exported).unwrap();
        std::fs::write(
            exported.join("pbps.yml"),
            "dialect: mssql\nenvironments:\n  prod: {url_env: PBPS_TRACE_TARGET}\n",
        )
        .unwrap();
        let result = self
            .command(
                &exported,
                &[
                    "state", "export", ui_id, "--env", "prod", "--format", "json",
                ],
            )
            .output()
            .unwrap();
        self.trace.output(&result);
        self.trace.files(&exported);
        assert_eq!(code(&result), 0);
        self.ledger();
    }

    // Run ordinary changes before installing the resolved view: removing an
    // applied managed view later would need separate reviewed drop intent.
    fn ordinary_outputs(&mut self) {
        self.hooks();
        // Explicitly re-declare the only retained module in these ordinary
        // plans, so assessment answers Rebuild rather than Resolve.
        let anchor = self.d.dir.join("schema/dbo.anchor.view.yml");
        let definition = "view: dbo.anchor\ndefinition: SELECT id + 0 AS id, label, amount, computed FROM ext.v\n";
        std::fs::write(&anchor, definition).unwrap();
        let view = std::fs::read(self.view()).unwrap();
        std::fs::remove_file(self.view()).unwrap();
        let base = "table: dbo.base\ncolumns:\n  id: {type: int, nullable: false}\n  note: {type: int, nullable: true}\nprimary_key: [id]\n";
        std::fs::write(self.d.dir.join("schema/dbo.base.yml"), base).unwrap();
        self.ok(&["plan", "--env", "prod", "--out", "ordinary.json"]);
        let ordinary: serde_json::Value = serde_json::from_slice(
            &self
                .trace
                .artifact(Sink::Plan, &self.d.dir.join("ordinary.json")),
        )
        .unwrap();
        assert_eq!(ordinary["origin"], "database");
        let checksum = plan_checksum(&self.d.dir.join("ordinary.json"));
        self.ok(&[
            "apply",
            "--env",
            "prod",
            "--plan",
            "ordinary.json",
            "--checksum",
            &checksum,
        ]);
        self.hook("on_apply");
        self.hook("on_apply_attempt");
        on_server(
            self.target.connection(),
            "ALTER TABLE dbo.base ALTER COLUMN note bigint NULL;",
        );
        let drift = self.run(&["verify", "--env", "prod", "--format", "json"]);
        on_server(
            self.target.connection(),
            "ALTER TABLE dbo.base ALTER COLUMN note int NULL;",
        );
        assert_eq!(code(&drift), FINDING);
        self.hook("on_drift");
        std::fs::write(
            self.d.dir.join("schema/dbo.base.yml"),
            base.replace("  note:", "  extra: {type: int, nullable: true}\n  note:"),
        )
        .unwrap();
        std::fs::write(&anchor, definition.replace("id + 0", "id + 0 + 0")).unwrap();
        self.ui(
            "plan",
            serde_json::json!({"environment":"prod", "out":"ui.json"}),
        );
        let ordinary: serde_json::Value =
            serde_json::from_slice(&self.trace.artifact(Sink::Plan, &self.d.dir.join("ui.json")))
                .unwrap();
        assert_eq!(ordinary["origin"], "database");
        let checksum = plan_checksum(&self.d.dir.join("ui.json"));
        self.ui("apply", serde_json::json!({"environment":"prod", "plan":"ui.json", "checksum":checksum, "allow":[], "staged":false,"resume":false}));
        std::fs::write(self.view(), view).unwrap();
    }
}

fn key(byte: u8) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode([byte; 32])
}

fn function() -> String {
    format!(
        "CREATE OR ALTER FUNCTION ext.f() RETURNS int AS BEGIN RETURN LEN(N'{}'); END;",
        SOURCES[1]
    )
}

fn text(connection: &str, sql: &str) -> String {
    try_text(connection, sql).unwrap()
}

fn try_text(connection: &str, sql: &str) -> Result<String, pbps_db::DbError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut connection = connect_live(connection).await?;
            connection
                .query(sql)
                .await?
                .iter()
                .map(|row| {
                    row.try_get_at::<&str>(0)
                        .map(|value| value.unwrap_or_default().to_owned())
                })
                .collect()
        })
}

/// Only the live planner child's process label authorizes inspecting its
/// disposable engine. Never guess another run's endpoint or credentials.
fn docker_subjects() -> Vec<String> {
    let listed = Command::new("docker")
        .args([
            "ps",
            "--all",
            "--no-trunc",
            "--filter",
            "label=pbps.resolver.process",
            "--format",
            r#"{{.ID}} {{.Label "pbps.resolver.process"}}"#,
        ])
        .output()
        .unwrap();
    assert!(listed.status.success());
    String::from_utf8(listed.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn docker_connection(pid: u32, prior: &[String]) -> Option<String> {
    let mut found = None;
    for subject in docker_subjects() {
        if prior.contains(&subject) {
            continue;
        }
        let (id, label) = subject.split_once(' ').unwrap();
        if label
            .split_once('-')
            .and_then(|(owner, _)| owner.parse::<u32>().ok())
            != Some(pid)
        {
            continue;
        }
        let inspected = Command::new("docker")
            .args(["inspect", id])
            .output()
            .unwrap();
        assert!(
            inspected.status.success(),
            "owned runtime inspection failed"
        );
        let values: Vec<serde_json::Value> = serde_json::from_slice(&inspected.stdout).unwrap();
        let value = &values[0];
        assert_eq!(value["Id"], id);
        assert_eq!(value["Config"]["Labels"]["pbps.resolver.process"], label);
        let Some(port) = value["NetworkSettings"]["Ports"]["1433/tcp"][0]["HostPort"].as_str()
        else {
            continue;
        };
        let environment = value["Config"]["Env"].as_array().unwrap();
        let password = environment
            .iter()
            .filter_map(|v| v.as_str())
            .find_map(|v| v.strip_prefix("MSSQL_SA_PASSWORD="))
            .unwrap();
        assert!(found.is_none(), "planner acquired two scratch engines");
        found = Some(format!(
            "Server=127.0.0.1,{port};User Id=sa;Password={password};Database=master;TrustServerCertificate=true"
        ));
    }
    found
}

fn check(docker: bool) {
    let mut fixture = Fixture::new(docker);
    fixture.draft_refusal();
    let first = fixture.plan("first.json");
    fixture.offline("first.json");
    fixture
        .env
        .iter_mut()
        .find(|(key, _)| key == "PBPS_TRACE_KEY")
        .unwrap()
        .1 = key(10);
    let second = fixture.plan("second.json");
    keyed_records_differ(&first, &second);
    fixture.publication_refusal(docker);
    fixture.render_failure();
    fixture.refusals();
    if docker {
        fixture.docker_cleanup_failure();
    } else {
        fixture.server_cleanup_failure();
    }
    fixture.ordinary_outputs();
    let (cli_id, ui_id) = fixture.resolved_outputs();
    fixture.readers(&cli_id, &ui_id);
    fixture.trace.complete();
}

#[test]
#[ignore = "needs two pinned SQL Server instances"]
fn mssql_server_keeps_definition_sources_out_of_every_sink() {
    check(false);
}

#[test]
#[ignore = "needs two pinned SQL Server instances and Docker"]
fn mssql_docker_keeps_definition_sources_out_of_every_sink() {
    check(true);
}
