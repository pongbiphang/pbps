//! The public command must compose the qualified owned services, not only list them.
#![cfg(target_os = "linux")]
use super::{BIN, Demo, code, stderr, stdout, succeeds};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Project(Demo);
impl Project {
    fn new() -> Self {
        let d = Demo::new(&format!("test1165-{:x}", rand::random::<u64>()));
        d.commit();
        let p = Self(d);
        p.table("amount", "integer", false);
        p.ids();
        p
    }
    fn state(&self) -> std::path::PathBuf {
        // Operational state normally lives outside the declarations' checkout.
        self.0.dir.with_extension("state")
    }
    fn write(&self, path: &str, text: impl AsRef<[u8]>) {
        let path = self.0.dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn table(&self, amount: &str, ty: &str, extra: bool) {
        self.0.table(&format!(
            concat!(
                "table: app.t\ncolumns:\n  id: {{type: integer, nullable: false}}\n",
                "  {amount}: {{type: {ty}, nullable: false}}\n",
                "{note}primary_key: {{name: t_pk, columns: [id]}}\n"
            ),
            amount = amount,
            ty = ty,
            note = if extra { "  note: {type: text}\n" } else { "" },
        ));
    }
    fn ids(&self) {
        succeeds(self.0.run(&["plan", "--no-dev"]));
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
    fn manifest(&self, image: &str, base: &str, fixtures: &[(&str, &str)], assertion: &str) {
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
                    "version: 1\ndialect: postgres\nbase: {base}\n",
                    "engine:\n  image: {image}\nfixtures: {groups}\n",
                    "assertions:\n  - {{name: a, file: a.sql}}\ntimeouts:\n",
                    "  startup_ms: 120000\n  step_ms: 120000\n  fixture_ms: 30000\n",
                    "  assertion_ms: 10000\n  run_ms: 600000\n  cleanup_ms: 30000\n"
                ),
                base = base,
                image = image,
                groups = groups,
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
    fn run(&self, expected: i32) -> Value {
        let before = self.bytes();
        let out = self.command("json").output().unwrap();
        assert_eq!(code(&out), expected, "{}{}", stdout(&out), stderr(&out));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["command"], "test");
        for raw in [
            include_str!("../../../../schemas/envelope.schema.json"),
            include_str!("../fixtures/published-schemas/84/envelope.schema.json"),
        ] {
            let schema = serde_json::from_str(raw).unwrap();
            let validator = jsonschema::validator_for(&schema).unwrap();
            validator.validate(&v).unwrap();
            let mut wrong = v.clone();
            wrong["schema_version"] = json!(2);
            assert!(!validator.is_valid(&wrong));
        }
        assert_eq!(
            self.bytes(),
            before,
            "the command published into the project"
        );
        assert!(v["data"].get("plan").is_none());
        v
    }
    fn bytes(&self) -> Vec<(String, Vec<u8>)> {
        fn visit(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let e = entry.unwrap();
                if dir == root && (e.file_name() == ".git") {
                    continue;
                }
                if e.file_type().unwrap().is_dir() {
                    visit(root, &e.path(), out);
                } else {
                    out.push((
                        e.path()
                            .strip_prefix(root)
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .into(),
                        std::fs::read(e.path()).unwrap(),
                    ));
                }
            }
        }
        let mut out = Vec::new();
        visit(&self.0.dir, &self.0.dir, &mut out);
        out.sort();
        out
    }
    fn settled(&self) {
        let dir = self.state().join("pbps/test-runs");
        if dir.exists() {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                assert_ne!(
                    p.extension().and_then(|s| s.to_str()),
                    Some("json"),
                    "unsettled record: {p:?}"
                );
                assert_ne!(p.extension().and_then(|s| s.to_str()), Some("key"));
            }
        }
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        let state = self.state();
        let unfinished = match std::fs::read_dir(state.join("pbps/test-runs")) {
            Ok(mut entries) => entries.any(|entry| {
                entry.map_or(true, |e| {
                    matches!(
                        e.path().extension().and_then(|s| s.to_str()),
                        Some("json" | "key")
                    )
                })
            }),
            Err(e) => e.kind() != std::io::ErrorKind::NotFound,
        };
        if unfinished {
            eprintln!("preserved unconfirmed run evidence at {}", state.display());
        } else {
            let _ = std::fs::remove_dir_all(state);
        }
    }
}
fn image(old: bool) -> String {
    std::env::var(if old {
        "PBPS_TEST_PG_OLD_SCRATCH_IMAGE"
    } else {
        "PBPS_TEST_PG_SCRATCH_IMAGE"
    })
    .unwrap()
}
fn engine(v: &Value, old: bool) {
    let version = if old {
        "PostgreSQL 16."
    } else {
        "PostgreSQL 18."
    };
    assert!(
        v["data"]["engine_build"]
            .as_str()
            .unwrap()
            .contains(version)
    );
    assert!(
        v["data"]["acquired_image"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
}
fn phase<'a>(v: &'a Value, name: &str) -> &'a Value {
    v["data"]["phases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["phase"] == name)
        .unwrap()
}
fn identities(v: &Value) {
    for o in std::iter::once(&v["data"]).chain(v["data"]["phases"].as_array().unwrap()) {
        for (key, val) in o.as_object().unwrap() {
            if val.is_null() {
                assert!(
                    !o["unavailable"][key].as_str().unwrap().is_empty(),
                    "{key}: {v}"
                );
            }
        }
    }
}

#[test]
fn invalid_inputs_keep_observed_hashes_and_never_acquire() {
    let p = Project::new();
    let base = p.commit();
    let image = "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280";
    p.manifest(image, &base, &[], "SELECT 1 WHERE false");
    p.write("schema/app.t.yml", "table: [invalid\n");
    let v = p.run(2);
    assert!(v["data"]["input"].is_null());
    assert_eq!(
        v["data"]["observed_input"]["resolved_commits"]["candidate"],
        base
    );
    assert!(
        v["data"]["observed_input"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["path"] == "tests/a.sql" && f["sha256"].as_str().unwrap().len() == 64)
    );
    assert!(!p.state().exists());
    identities(&v);
    p.table("renamed", "integer", false);
    let missing = p.run(2);
    assert!(missing["findings"].as_array().unwrap().iter().any(|f| {
        f["message"]
            .as_str()
            .unwrap()
            .contains("identity intent is missing")
    }));
    assert!(!p.state().exists());
    let help = p.command("human").arg("--help").output().unwrap();
    assert!(stdout(&help).contains("--cleanup"));
    for args in [
        ["--db", "unowned"],
        ["--env", "production"],
        ["--plan", "plan.json"],
        ["--out", "plan.json"],
        ["fixtures", "repin"],
    ] {
        let out = p.command("json").args(args).output().unwrap();
        assert_eq!(code(&out), 2);
        assert!(stdout(&out).is_empty());
    }
}

#[test]
fn unsupported_engine_refuses_before_runtime_discovery() {
    let p = Project::new();
    let base = p.commit();
    p.manifest(
        "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280",
        &base,
        &[],
        "SELECT 1 WHERE false",
    );
    // Both snapshots must agree with the manifest before engine admission.
    p.write("pbps.yml", "dialect: mssql\n");
    p.0.git(&["add", "pbps.yml"]);
    p.0.git(&["commit", "-qm", "mssql"]);
    let base = p.head();
    let manifest = p.0.dir.join("tests/pbps-tests.yml");
    let text = std::fs::read_to_string(&manifest)
        .unwrap()
        .replace("dialect: postgres", "dialect: mssql");
    let text = text
        .lines()
        .map(|l| {
            if l.starts_with("base:") {
                format!("base: {base}")
            } else {
                l.into()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(manifest, text).unwrap();
    let v = p.run(2);
    assert!(v["findings"].as_array().unwrap().iter().any(|f| {
        f["message"]
            .as_str()
            .unwrap()
            .contains("currently supports PostgreSQL")
    }));
    assert!(!p.state().exists());
}
fn replay(old: bool) {
    let p = Project::new();
    p.table("amount", "text", false);
    let path = p.0.dir.join("schema/app.t.yml");
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("primary_key:", "  obsolete: {type: text}\nprimary_key:");
    p.write("schema/app.t.yml", text);
    p.ids();
    let oldest = p.commit();
    let first = concat!(
        "DO $$ BEGIN IF to_regnamespace('later') IS NULL THEN ",
        "RAISE EXCEPTION 'future namespace missing'; END IF; END $$; ",
        "CREATE TEMP TABLE helper(n int); INSERT INTO helper VALUES(7); ",
        "INSERT INTO app.t(id,amount) VALUES(1,'7');"
    );
    p.write("tests/first.sql", first);
    p.write(
        "tests/second.sql",
        "INSERT INTO app.t(id,amount) SELECT 2,n::text FROM helper;",
    );
    p.table("total", "text", false);
    succeeds(p.0.run(&[
        "intent",
        "--rename",
        "app.t.amount",
        "total",
        "--drop",
        "app.t.obsolete",
        "obsolete sample",
    ]));
    let middle = p.commit();
    p.write("tests/later.sql", "INSERT INTO app.t VALUES(3,'9');");
    // A fixture on the merged side branch is valid even off the first-parent line.
    p.0.git(&["checkout", "-qb", "side"]);
    p.write("side.txt", "side");
    let side = p.commit();
    p.0.git(&["checkout", "-q", "-"]);
    p.write("main.txt", "main");
    p.commit();
    p.0.git(&["merge", "-q", "--no-ff", "-m", "merge", "side"]);
    p.table("total", "integer", false);
    succeeds(p.0.run(&[
        "convert",
        "app.t.total",
        "--from",
        "text",
        "--using",
        "total::integer",
    ]));
    p.ids();
    let base = p.commit();
    p.table("total", "integer", true);
    p.ids();
    p.write(
        "schema/later.yml",
        "table: later.added\ncolumns:\n  id: {type: integer}\n",
    );
    p.ids();
    let assertion = concat!(
        "SELECT 1 WHERE (SELECT sum(total) FROM app.t) IS DISTINCT FROM 23 ",
        "OR (SELECT count(*) FROM app.t) <> 3 ",
        "OR (SELECT count(*) FROM later.added) <> 0"
    );
    p.manifest(
        &image(old),
        &base,
        &[("later", &side), ("first", &oldest), ("second", &oldest)],
        assertion,
    );
    let v = p.run(0);
    engine(&v, old);
    assert_eq!(v["result"], "ok");
    identities(&v);
    assert_eq!(v["data"]["input"]["candidate"]["kind"], "working_tree");
    assert_eq!(v["data"]["input"]["candidate"]["uncommitted"], true);
    assert_eq!(v["data"]["input"]["candidate"]["commit"], base);
    assert_eq!(v["data"]["inputs_reverified"], true);
    assert_eq!(v["data"]["cleanup_confirmed"], true);
    let phases = v["data"]["phases"].as_array().unwrap();
    assert_eq!(
        phases
            .iter()
            .filter(|x| x["phase"] == "fixture")
            .map(|x| x["identity"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["first", "second", "later"]
    );
    assert!(
        phases
            .iter()
            .any(|x| x["from"] == oldest && x["to"] == side)
    );
    assert!(phases.iter().any(|x| x["from"] == side && x["to"] == base));
    assert_eq!(v["data"]["replay_hops"], 2);
    let checksum = &v["data"]["candidate_sealed_checksum"];
    assert_eq!(checksum.as_str().unwrap().len(), 64);
    assert_eq!(checksum, &v["data"]["candidate_accepted_checksum"]);
    assert_eq!(checksum, &v["data"]["candidate_executed_checksum"]);
    assert_eq!(phase(&v, "candidate_apply")["started"], true);
    p.settled();
    // Explicit candidate reads committed declarations, not a broken working tree.
    let candidate = p.commit();
    let manifest = p.0.dir.join("tests/pbps-tests.yml");
    let mut text = std::fs::read_to_string(&manifest).unwrap();
    text.push_str(&format!("candidate: {candidate}\n"));
    std::fs::write(&manifest, text).unwrap();
    p.write("schema/app.t.yml", "invalid: [\n");
    let v = p.run(0);
    assert_eq!(v["data"]["input"]["candidate"]["kind"], "commit");
    assert_eq!(v["data"]["input"]["candidate"]["uncommitted"], false);
    p.settled();
    assert_ne!(middle, oldest);
}
fn assertions(old: bool) {
    let p = Project::new();
    let base = p.commit();
    p.table("amount", "integer", true);
    p.ids();
    p.write("tests/f.sql", "INSERT INTO app.t(id,amount) VALUES(1,7)");
    p.manifest(&image(old), &base, &[("f", &base)], "SELECT 1 WHERE false");
    let out = p.command("human").output().unwrap();
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    for text in [
        "uncommitted working-tree",
        &base,
        "tests/f.sql",
        "cleanup confirmed: true",
        "candidate_apply",
    ] {
        assert!(stdout(&out).contains(text), "{text}: {}", stdout(&out));
    }
    assert!(stdout(&out).contains("acquired image sha256:"));
    assert!(stdout(&out).contains("checksum none"));
    for debug in ["run Some(", "checksum None", "risks ["] {
        assert!(!stdout(&out).contains(debug), "{}", stdout(&out));
    }
    for (sql, expected, outcome) in [
        ("SELECT NULL::text AS missing", 2, "violation"),
        (
            "SELECT repeat('x',70000) AS big FROM generate_series(1,25)",
            2,
            "violation",
        ),
        ("SELECT 1/0", 2, "sql_error"),
        ("SELECT FROM app.t WHERE false", 2, "invalid_result"),
        ("COMMIT", 2, "boundary_violation"),
        ("SELECT 1; SELECT 2", 2, "sql_error"),
        ("SELECT 1 FROM pg_sleep(2)", 1, "unanswerable"),
    ] {
        p.write("tests/a.sql", sql);
        let path = p.0.dir.join("tests/pbps-tests.yml");
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| {
                if line.trim_start().starts_with("assertion_ms:") {
                    format!(
                        "  assertion_ms: {}",
                        if sql.contains("pg_sleep") { 250 } else { 10000 }
                    )
                } else {
                    line.into()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(path, text).unwrap();
        let v = p.run(expected);
        engine(&v, old);
        assert_eq!(v["data"]["assertions"][0]["outcome"], outcome, "{v}");
        assert_eq!(v["data"]["cleanup_confirmed"], true);
        identities(&v);
        p.settled();
        if sql.starts_with("SELECT NULL") {
            assert_eq!(v["data"]["assertions"][0]["rows"], json!([[null]]));
            assert_eq!(
                v["data"]["assertions"][0]["columns"][0]["type"],
                "pg_catalog.text"
            );
        }
        if sql.contains("generate_series") {
            assert!(v["data"]["assertions"][0]["rows"].as_array().unwrap().len() <= 20);
            assert_eq!(v["data"]["assertions"][0]["truncated"], true);
        }
    }
    p.write("tests/a.sql", "SELECT 1 WHERE false");
    for fixture in ["CREATE TABLE app.forbidden(n int)", "CREATE ROLE forbidden"] {
        p.write("tests/f.sql", fixture);
        let v = p.run(2);
        assert_eq!(phase(&v, "fixture")["outcome"], "failed");
        assert_eq!(phase(&v, "candidate_plan")["outcome"], "skipped");
        p.settled();
    }
}
fn empty_candidate(old: bool) {
    let p = Project::new();
    let base = p.commit();
    p.manifest(&image(old), &base, &[], "SELECT 1 WHERE false");
    for (sql, expected, outcome) in [
        ("SELECT 1 WHERE false", 0, "pass"),
        ("SELECT FROM app.t WHERE false", 2, "invalid_result"),
    ] {
        p.write("tests/a.sql", sql);
        let v = p.run(expected);
        engine(&v, old);
        assert_eq!(phase(&v, "candidate_plan")["changes"], 0);
        assert_eq!(v["data"]["assertions"][0]["outcome"], outcome);
        assert_eq!(v["data"]["cleanup_confirmed"], true);
        identities(&v);
        p.settled();
    }
}

struct Running {
    child: Option<Child>,
    state: std::path::PathBuf,
}
impl Running {
    fn finish(mut self) -> std::process::Output {
        self.child.take().unwrap().wait_with_output().unwrap()
    }
}
impl Drop for Running {
    fn drop(&mut self) {
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
fn waiting(p: &Project) -> (Running, String) {
    let child = p
        .command("json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut running = Running {
        child: Some(child),
        state: p.state(),
    };
    let end = Instant::now() + Duration::from_secs(120);
    loop {
        assert!(
            running
                .child
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
                .is_none(),
            "child exited before the fixture wait"
        );
        let dir = p.state().join("pbps/test-runs");
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries {
                let e = e.unwrap();
                if e.path().extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                let v: Value = serde_json::from_slice(&std::fs::read(e.path()).unwrap()).unwrap();
                let Some(id) = v["container"]["id"].as_str() else {
                    continue;
                };
                let query = Command::new("docker")
                    .args([
                        "exec",
                        id,
                        "psql",
                        "-U",
                        "postgres",
                        "-d",
                        "pbps_test",
                        "-Atqc",
                        concat!(
                            "SELECT count(*) FROM pg_stat_activity ",
                            "WHERE usename LIKE 'pbps_test_fixture_%' AND wait_event='PgSleep'"
                        ),
                    ])
                    .output()
                    .unwrap();
                if query.status.success() && stdout(&query).trim() == "1" {
                    return (running, id.into());
                }
            }
        }
        assert!(Instant::now() < end, "fixture wait was not observed");
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn interrupted(old: bool) {
    let p = Project::new();
    let base = p.commit();
    p.write("tests/f.sql", "SELECT pg_sleep(30)");
    p.manifest(&image(old), &base, &[("f", &base)], "SELECT 1 WHERE false");
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
    let (run, id) = waiting(&p);
    p.write(
        "schema/new.yml",
        "table: app.untracked\ncolumns:\n  id: {type: integer}\n",
    );
    let changed = p.bytes();
    // Release the known fixture backend without changing the frozen SQL text.
    let out = Command::new("docker")
        .args([
            "exec",
            &id,
            "psql",
            "-U",
            "postgres",
            "-d",
            "pbps_test",
            "-Atqc",
            concat!(
                "SELECT pg_cancel_backend(pid) FROM pg_stat_activity ",
                "WHERE usename LIKE 'pbps_test_fixture_%' AND wait_event='PgSleep'"
            ),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
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
    std::fs::remove_file(p.0.dir.join("pbps.yml")).unwrap();
    let before = p.bytes();
    let cleanup = p.command("json").arg("--cleanup").output().unwrap();
    assert_eq!(p.bytes(), before, "cleanup published into the project");
    assert_eq!(
        code(&cleanup),
        0,
        "{}{}",
        stdout(&cleanup),
        stderr(&cleanup)
    );
    let v: Value = serde_json::from_slice(&cleanup.stdout).unwrap();
    assert_eq!(v["data"]["operation"], "cleanup");
    assert_eq!(v["data"]["cleanup_confirmed"], true);
}

#[test]
#[ignore = "needs docker and the pinned PostgreSQL 16 scratch image"]
fn d2b2_1165_pg16_replays_pins_and_keeps_the_candidate_seal() {
    replay(true);
}
#[test]
#[ignore = "needs docker and the pinned PostgreSQL 18 scratch image"]
fn d2b2_1165_pg18_replays_pins_and_keeps_the_candidate_seal() {
    replay(false);
}
#[test]
#[ignore = "needs docker and the pinned PostgreSQL 16 scratch image"]
fn d2b2_1165_pg16_fixture_and_assertion_boundaries_publish_one_report() {
    assertions(true);
}
#[test]
#[ignore = "needs docker and the pinned PostgreSQL 18 scratch image"]
fn d2b2_1165_pg18_fixture_and_assertion_boundaries_publish_one_report() {
    assertions(false);
}
#[test]
#[ignore = "needs docker and the pinned PostgreSQL 16 scratch image"]
fn d2b2_1165_pg16_cancellation_and_changed_inputs_cannot_qualify() {
    interrupted(true);
}
#[test]
#[ignore = "needs docker and the pinned PostgreSQL 18 scratch image"]
fn d2b2_1165_pg18_cancellation_and_changed_inputs_cannot_qualify() {
    interrupted(false);
}

#[test]
#[ignore = "needs docker and the pinned PostgreSQL 16 scratch image"]
fn d2b2_1165_pg16_an_empty_candidate_still_checks_assertion_result_shapes() {
    empty_candidate(true);
}
#[test]
#[ignore = "needs docker and the pinned PostgreSQL 18 scratch image"]
fn d2b2_1165_pg18_an_empty_candidate_still_checks_assertion_result_shapes() {
    empty_candidate(false);
}
