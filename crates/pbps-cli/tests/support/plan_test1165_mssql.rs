//! `pbps test` on an owned SQL Server through the public command (#1165 F2).
//!
//! The engine's own boundaries (the seal, the identities, the classification)
//! are qualified natively by `plan_test::session::mssql_tests` and the report
//! tests. These tests pin what only the command composes: replay, the sealed
//! candidate, every assertion outcome's exit code, and confirmed cleanup.
//! Each one needs Docker and `PBPS_TEST_DEV_IMAGE`, and fails loudly without.
#![cfg(target_os = "linux")]
use super::{BIN, Demo, code, stderr, stdout};
use serde_json::Value;
use std::{
    path::Path,
    process::{Child, Command},
    time::{Duration, Instant},
};

struct Project(Demo);
impl Project {
    fn new() -> Self {
        let d = Demo::new(&format!("test1165m-{:x}", rand::random::<u64>()));
        d.commit();
        let p = Self(d);
        p.table("amount");
        p.ids();
        p
    }
    fn state(&self) -> std::path::PathBuf {
        self.0.dir.with_extension("state")
    }
    fn write(&self, path: &str, text: impl AsRef<[u8]>) {
        let path = self.0.dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn table(&self, amount: &str) {
        self.write(
            "schema/app.t.yml",
            format!(
                concat!(
                    "table: app.t\ncolumns:\n  id: {{type: int, nullable: false}}\n",
                    "  {amount}: {{type: int, nullable: false}}\n",
                    "primary_key: {{name: t_pk, columns: [id]}}\n"
                ),
                amount = amount
            ),
        );
    }
    fn ids(&self) {
        let out = self.0.run(&["plan", "--no-dev"]);
        assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    }
    fn head(&self) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.0.dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(out.status.success());
        stdout(&out).trim().to_owned()
    }
    fn commit(&self) -> String {
        self.0.commit();
        self.head()
    }
    fn manifest(&self, base: &str, fixtures: &[(&str, &str)], assertion: &str, assertion_ms: u32) {
        self.write("tests/a.sql", assertion);
        let groups = if fixtures.is_empty() {
            "[]".into()
        } else {
            fixtures
                .iter()
                .map(|(name, at)| format!("\n  - {{name: {name}, file: {name}.sql, at: {at}}}"))
                .collect::<String>()
        };
        self.write(
            "tests/pbps-tests.yml",
            format!(
                concat!(
                    "version: 1\ndialect: mssql\nbase: {base}\n",
                    "engine:\n  image: {image}\nfixtures: {groups}\n",
                    "assertions:\n  - {{name: a, file: a.sql}}\ntimeouts:\n",
                    "  startup_ms: 180000\n  step_ms: 120000\n  fixture_ms: 60000\n",
                    "  assertion_ms: {assertion_ms}\n  run_ms: 900000\n  cleanup_ms: 60000\n"
                ),
                base = base,
                image = std::env::var("PBPS_TEST_DEV_IMAGE").unwrap(),
                groups = groups,
                assertion_ms = assertion_ms,
            ),
        );
    }
    fn command(&self, format: &str) -> Command {
        let mut c = Command::new(BIN);
        c.arg("--project")
            .arg(&self.0.dir)
            .args(["test", "--format", format]);
        c.env("XDG_STATE_HOME", self.state());
        c
    }
    /// The command's report. It never writes into the project, so the tree is
    /// compared byte for byte before and after.
    fn run(&self, expected: i32) -> Value {
        let before = self.bytes();
        let out = self.command("json").output().unwrap();
        assert_eq!(code(&out), expected, "{}{}", stdout(&out), stderr(&out));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["command"], "test");
        assert_eq!(self.bytes(), before, "the command wrote into the project");
        assert_eq!(v["data"]["cleanup_confirmed"], true, "{v}");
        v
    }
    fn bytes(&self) -> Vec<(String, Vec<u8>)> {
        fn visit(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let e = entry.unwrap();
                if dir == root && e.file_name() == ".git" {
                    continue;
                }
                if e.file_type().unwrap().is_dir() {
                    visit(root, &e.path(), out);
                } else {
                    let name = e
                        .path()
                        .strip_prefix(root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .into();
                    out.push((name, std::fs::read(e.path()).unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        visit(&self.0.dir, &self.0.dir, &mut out);
        out.sort();
        out
    }
    /// No run record is left unconfirmed.
    fn settled(&self) {
        let dir = self.state().join("pbps/test-runs");
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries {
                let ext = e.unwrap().path().extension().map(|s| s.to_owned());
                assert!(
                    !matches!(
                        ext.as_deref().and_then(|s| s.to_str()),
                        Some("json" | "key")
                    ),
                    "unsettled record"
                );
            }
        }
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.state());
    }
}

fn phase<'a>(v: &'a Value, name: &str) -> &'a Value {
    v["data"]["phases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["phase"] == name)
        .unwrap()
}
fn findings(v: &Value) -> Vec<String> {
    v["data"]["phases"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["findings"].as_array().unwrap().iter())
        .map(|f| format!("{} {}", f["id"].as_str().unwrap_or(""), f["message"]))
        .collect()
}
fn assertion_outcome(v: &Value) -> &str {
    v["data"]["assertions"][0]["outcome"].as_str().unwrap()
}

/// The ADR §8 shape: a rename replayed from a pinned fixture, one assertion on
/// the renamed column. `total` was `amount` when the fixture inserted 7.
fn renamed() -> (Project, String) {
    let p = Project::new();
    let base = p.commit();
    p.write(
        "tests/first.sql",
        "INSERT INTO app.t(id, amount) VALUES (1, 7);",
    );
    p.table("total");
    let out = p.0.run(&["intent", "--rename", "app.t.amount", "total"]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    p.ids();
    (p, base)
}

#[test]
#[ignore = "needs docker and the pinned SQL Server image"]
fn plan_test1165_mssql_replays_a_pinned_fixture_and_the_sealed_candidate_decides_the_exit() {
    let (p, base) = renamed();
    p.manifest(
        &base,
        &[("first", &base)],
        "SELECT 1 AS x WHERE ISNULL((SELECT SUM(total) FROM app.t), -1) <> 7",
        10_000,
    );
    let v = p.run(0);
    assert_eq!(v["result"], "ok");
    let build = v["data"]["engine_build"].as_str().unwrap();
    assert!(
        build.contains("SQL Server") && build.contains("server collation"),
        "{build}"
    );
    assert!(
        v["data"]["acquired_image"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert_eq!(v["data"]["inputs_reverified"], true);
    let sealed = &v["data"]["candidate_sealed_checksum"];
    assert_eq!(sealed.as_str().unwrap().len(), 64);
    assert_eq!(sealed, &v["data"]["candidate_accepted_checksum"]);
    assert_eq!(sealed, &v["data"]["candidate_executed_checksum"]);
    assert_eq!(phase(&v, "candidate_apply")["started"], true);
    assert_eq!(assertion_outcome(&v), "pass");
    p.settled();

    // Negative: the same replay with an assertion that finds a row is a finding.
    p.write("tests/a.sql", "SELECT total FROM app.t WHERE id = 1");
    let v = p.run(2);
    assert_eq!(assertion_outcome(&v), "violation");
    assert_eq!(v["data"]["assertions"][0]["rows"][0][0]["value"], "7");
    p.settled();
}

#[test]
#[ignore = "needs docker and the pinned SQL Server image"]
fn plan_test1165_mssql_every_assertion_outcome_other_than_zero_rows_fails_with_its_own_kind() {
    let p = Project::new();
    let base = p.commit();
    p.table("amount");
    // (assertion, assertion_ms, exit, outcome)
    for (sql, ms, exit, outcome) in [
        // A persistent write is refused by the login and by the sealed database.
        (
            "INSERT INTO app.t(id, amount) VALUES (9, 9); SELECT 1 AS x WHERE 1 = 0",
            10_000,
            2,
            "sql_error",
        ),
        // No result set is not an empty one.
        ("DECLARE @n int = 1", 10_000, 2, "invalid_result"),
        // The batch separator is the engine's to refuse, not the runner's to split.
        (
            "SELECT 1 AS x WHERE 1 = 0\nGO\nSELECT 1 AS x",
            10_000,
            2,
            "sql_error",
        ),
        // A statement that outlives its deadline never answered: exit 1, not a pass.
        (
            "WAITFOR DELAY '00:00:30'; SELECT 1 AS x WHERE 1 = 0",
            3_000,
            1,
            "unanswerable",
        ),
    ] {
        p.manifest(&base, &[], sql, ms);
        let v = p.run(exit);
        assert_eq!(assertion_outcome(&v), outcome, "{sql}: {v}");
        p.settled();
    }
}

#[test]
#[ignore = "needs docker and the pinned SQL Server image"]
fn plan_test1165_mssql_a_fixture_that_leaves_a_transaction_open_fails_and_runs_nothing_after() {
    let p = Project::new();
    let base = p.commit();
    p.write(
        "tests/f.sql",
        "BEGIN TRANSACTION; INSERT INTO app.t(id, amount) VALUES (2, 2);",
    );
    p.manifest(&base, &[("f", &base)], "SELECT 1 AS x WHERE 1 = 0", 10_000);
    let v = p.run(2);
    assert!(
        findings(&v)
            .iter()
            .any(|f| f.starts_with("test.fixture") && f.contains("transaction")),
        "{:?}",
        findings(&v)
    );
    // The assertion never ran against a database a lost fixture left empty.
    assert_eq!(assertion_outcome(&v), "skipped");
    assert_eq!(phase(&v, "candidate_apply")["outcome"], "skipped");
    p.settled();
}

#[test]
#[ignore = "needs docker and the pinned SQL Server image"]
fn plan_test1165_mssql_fixture_repin_still_refuses_before_any_runtime_or_write() {
    let p = Project::new();
    let oldest = p.commit();
    p.write("later.txt", "later");
    let base = p.commit();
    p.write(
        "tests/f.sql",
        "INSERT INTO app.t(id, amount) VALUES (1, 1);",
    );
    p.manifest(
        &base,
        &[("f", &oldest)],
        "SELECT 1 AS x WHERE 1 = 0",
        10_000,
    );
    // A re-pin reads the committed manifest and fixture, and a newer pin to move to.
    p.commit();
    let before = p.bytes();
    let out = Command::new(BIN)
        .arg("--project")
        .arg(&p.0.dir)
        .args([
            "test", "fixtures", "repin", "f", "--at", &base, "--format", "json",
        ])
        .env("XDG_STATE_HOME", p.state())
        .output()
        .unwrap();
    assert_eq!(code(&out), 2, "{}{}", stdout(&out), stderr(&out));
    assert!(
        stdout(&out).contains("currently supports PostgreSQL"),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    assert_eq!(p.bytes(), before, "a refused re-pin wrote into the project");
    assert!(
        !p.state().join("pbps/test-runs").exists(),
        "a refusal started a run"
    );
}

struct Running {
    child: Option<Child>,
    state: std::path::PathBuf,
    output: [std::path::PathBuf; 2],
}
impl Running {
    fn finish(mut self) -> std::process::Output {
        let status = self.child.take().unwrap().wait().unwrap();
        let [out, err] = &self.output;
        std::process::Output {
            status,
            stdout: std::fs::read(out).unwrap(),
            stderr: std::fs::read(err).unwrap(),
        }
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        for file in &self.output {
            let _ = std::fs::remove_file(file);
        }
        if let Some(c) = &mut self.child {
            let _ = c.kill();
            let _ = c.wait();
            let cleanup = Command::new(BIN)
                .args(["test", "--cleanup", "--format", "json"])
                .env("XDG_STATE_HOME", &self.state)
                .output()
                .unwrap();
            if !cleanup.status.success() {
                eprintln!(
                    "interrupted fixture cleanup: {}{}",
                    stdout(&cleanup),
                    stderr(&cleanup)
                );
            }
        }
    }
}

/// One statement on the run's own container, as `sa`. Panics on failure: the
/// wait below is the test's precondition, not something to skip.
fn on_container(id: &str, sql: &str) -> String {
    let env = Command::new("docker")
        .args([
            "inspect",
            "-f",
            "{{range .Config.Env}}{{println .}}{{end}}",
            id,
        ])
        .output()
        .unwrap();
    let password = stdout(&env)
        .lines()
        .find_map(|l| l.strip_prefix("MSSQL_SA_PASSWORD=").map(str::to_owned))
        .expect("the scratch container names its administrator password");
    let out = Command::new("docker")
        .args([
            "exec",
            id,
            "/opt/mssql-tools18/bin/sqlcmd",
            "-C",
            "-S",
            "localhost",
            "-U",
            "sa",
            "-P",
        ])
        .arg(password)
        .args(["-h", "-1", "-W", "-Q", sql])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "sqlcmd {}: {}{}",
        out.status,
        stdout(&out),
        stderr(&out)
    );
    stdout(&out)
}

const WAITING: &str = "SET NOCOUNT ON; SELECT COUNT(*) FROM sys.dm_exec_requests r JOIN sys.dm_exec_sessions s \
    ON s.session_id = r.session_id WHERE s.login_name LIKE 'pbps_test_fixture_%' \
    AND r.wait_type = 'WAITFOR'";

/// Starts the run and returns once its fixture is observed waiting on the server.
fn waiting(p: &Project) -> (Running, String) {
    let output = ["out", "err"].map(|kind| p.0.dir.with_extension(kind));
    let [out, err] = output
        .clone()
        .map(|path| std::fs::File::create(path).unwrap());
    let child = p.command("json").stdout(out).stderr(err).spawn().unwrap();
    let mut running = Running {
        child: Some(child),
        state: p.state(),
        output,
    };
    let end = Instant::now() + Duration::from_secs(240);
    loop {
        if let Some(status) = running.child.as_mut().unwrap().try_wait().unwrap() {
            let [out, err] = &running.output;
            panic!(
                "child exited ({status}) before the fixture wait: {}{}",
                String::from_utf8_lossy(&std::fs::read(out).unwrap()),
                String::from_utf8_lossy(&std::fs::read(err).unwrap())
            );
        }
        if let Ok(entries) = std::fs::read_dir(p.state().join("pbps/test-runs")) {
            for e in entries {
                let e = e.unwrap();
                if e.path().extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                let v: Value = serde_json::from_slice(&std::fs::read(e.path()).unwrap()).unwrap();
                let Some(id) = v["container"]["id"].as_str() else {
                    continue;
                };
                // The server may still be starting; only a settled answer counts.
                let ready = Command::new("docker")
                    .args(["inspect", "-f", "{{.State.Running}}", id])
                    .output()
                    .unwrap();
                if stdout(&ready).trim() == "true"
                    && std::panic::catch_unwind(|| on_container(id, WAITING))
                        .is_ok_and(|n| n.trim() == "1")
                {
                    return (running, id.into());
                }
            }
        }
        assert!(Instant::now() < end, "fixture wait was not observed");
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[test]
#[ignore = "needs docker and the pinned SQL Server image"]
fn plan_test1165_mssql_cancellation_and_changed_inputs_cannot_qualify_and_cleanup_is_confirmed() {
    let p = Project::new();
    let base = p.commit();
    p.write("tests/f.sql", "WAITFOR DELAY '00:10:00'");
    p.manifest(&base, &[("f", &base)], "SELECT 1 AS x WHERE 1 = 0", 10_000);
    let before = p.bytes();
    let (run, _) = waiting(&p);
    let out = Command::new("kill")
        .args(["-INT", &run.child.as_ref().unwrap().id().to_string()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let out = run.finish();
    assert_eq!(p.bytes(), before, "cancellation published into the project");
    assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["cleanup_confirmed"], true);
    assert_eq!(phase(&v, "candidate_plan")["outcome"], "skipped");
    p.settled();

    // An input that changes while the run waits cannot qualify it, however the
    // fixture then ends: the server session is ended without touching its SQL.
    let (run, id) = waiting(&p);
    p.write(
        "schema/new.yml",
        "table: app.untracked\ncolumns:\n  id: {type: int}\n",
    );
    let changed = p.bytes();
    on_container(
        &id,
        "DECLARE @s int = (SELECT TOP 1 s.session_id FROM sys.dm_exec_sessions s \
         WHERE s.login_name LIKE 'pbps_test_fixture_%'); \
         DECLARE @k nvarchar(40) = N'KILL ' + CONVERT(nvarchar(10), @s); EXEC (@k)",
    );
    let out = run.finish();
    assert_eq!(
        p.bytes(),
        changed,
        "input refusal published into the project"
    );
    assert_ne!(code(&out), 0);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["inputs_reverified"], false);
    assert_eq!(v["data"]["cleanup_confirmed"], true);
    assert!(stdout(&out).contains("new.yml"));
    p.settled();
}
