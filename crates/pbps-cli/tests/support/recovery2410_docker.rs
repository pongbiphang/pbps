//! A real `pbps plan` on a Docker scratch killed with SIGKILL, then the next
//! one (#2410, DEC-2410.3): the container the killed run left running is
//! removed by the next run, once it is provably the dead run's. The in-process
//! cases in `scratch/live_tests/docker2410_tests.rs` make each state by hand;
//! these prove that nothing between a hand-made state and a real kill was
//! missed. Only the containers of the processes a test started are looked at:
//! other sessions on the host run scratch containers meanwhile.

use super::*;
use std::process::Stdio;
use std::time::{Duration, Instant};

const TARGET: &str = "PBPS_REC2410D_TARGET";
const LIMIT: Duration = Duration::from_secs(120);

const TABLE: &str = "table: app.t\ncolumns:\n  id: {type: integer, nullable: false}\n  \
    amount: {type: numeric, nullable: false, default: \"0\"}\n  note: {type: text}\n\
    primary_key: {name: pk_t, columns: [id]}\n";

const FUNCTION: &str = "function: app.f(integer)\npublic_execute: true\n\ndefinition: |-\n  \
    (integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1 + 1\n";

const VIEW: &str = "view: app.v\ndefinition: SELECT id, app.f(id) AS next, amount FROM app.t\n";

pub struct Fixture {
    d: Demo,
    own: OwnDatabase,
}

impl Fixture {
    fn new(server: &str, slug: &str) -> Self {
        let image = std::env::var("PBPS_TEST_PG_SCRATCH_IMAGE")
            .expect("PBPS_TEST_PG_SCRATCH_IMAGE is not set; see scripts/live-tests-pg.sh");
        let own = OwnDatabase::new(server, slug);
        let connection = own.connection().to_owned();
        on_server(&connection, "CREATE SCHEMA app");
        let d = Demo::new(slug);
        let write = |name: &str, body: &str| {
            std::fs::write(d.dir.join("schema").join(name), body).unwrap();
        };
        write("app.t.yml", TABLE);
        write("app.f%28integer%29.function.yml", FUNCTION);
        write("app.v.view.yml", VIEW);
        std::fs::write(d.dir.join("externals.sql"), "SELECT 1;\n").unwrap();
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", &connection]));
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!(
                "dialect: postgres\nresolvers:\n  scratch:\n    kind: docker\n    image: '{image}'\n    \
                 pull: never\n    externals: externals.sql\nenvironments:\n  prod:\n    \
                 url_env: {TARGET}\n    resolve_with: scratch\n    fingerprint_key_file: key\n"
            ),
        )
        .unwrap();
        succeeds(d.run(&[
            "key",
            "generate",
            "--out",
            d.dir.join("key").to_str().unwrap(),
        ]));
        // A column with a default, which a kept binding (the view) could
        // reach: the resolver is needed, so a container is started.
        write(
            "app.t.yml",
            &TABLE.replace(
                "  note: {type: text}\n",
                "  note: {type: text}\n  extra: {type: boolean, nullable: true, default: \"true\"}\n",
            ),
        );
        succeeds(d.run(&["plan"]));
        d.commit();
        Self { d, own }
    }

    fn externals(&self, sql: &str) {
        std::fs::write(self.d.dir.join("externals.sql"), sql).unwrap();
    }

    fn command(&self, extra: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.arg("--project")
            .arg(&self.d.dir)
            .args(["plan", "--env", "prod", "--out"])
            .arg(self.d.dir.join("plan.json"))
            .args(extra)
            .env(TARGET, self.own.connection());
        cmd
    }

    fn start(&self) -> std::process::Child {
        self.command(&[])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn plan(&self, extra: &[&str]) -> Output {
        self.command(extra).output().unwrap()
    }
}

/// Runs a thing until it holds, or panics saying what it waited for.
fn until(what: &str, ready: impl Fn() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(started.elapsed() < LIMIT, "gave up waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The scratch containers the process `pid` started, by name.
fn containers_of(pid: u32) -> Vec<String> {
    let out = Command::new("docker")
        .args([
            "ps",
            "--all",
            "--no-trunc",
            "--filter",
            "label=pbps.resolver=scratch",
            "--format",
            "{{.Names}} {{.Label \"pbps.resolver.process\"}}",
        ])
        .output()
        .expect("docker");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(_, process)| process.starts_with(&format!("{pid}-")))
        .map(|(name, _)| name.to_owned())
        .collect()
}

fn exists(name: &str) -> bool {
    Command::new("docker")
        .args(["container", "inspect", name])
        .output()
        .expect("docker")
        .status
        .success()
}

fn remove(name: &str) {
    let _ = Command::new("docker")
        .args(["rm", "--force", "--volumes", name])
        .output();
}

/// Removes the containers a test is left with, however it ends.
struct Leftovers(Vec<String>);

impl Drop for Leftovers {
    fn drop(&mut self) {
        for name in &self.0 {
            remove(name);
        }
    }
}

/// How many sessions in the container's server are a run's keeper.
fn keepers(name: &str) -> usize {
    let out = Command::new("docker")
        .args([
            "exec",
            name,
            "psql",
            "-U",
            "postgres",
            "-Atc",
            "SELECT count(*) FROM pg_stat_activity WHERE application_name ~ '^pbps-[0-9a-f]{32}$'",
        ])
        .output()
        .expect("docker");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

fn sleeping(name: &str, marker: &str) -> bool {
    let out = Command::new("docker")
        .args([
            "exec",
            name,
            "psql",
            "-U",
            "postgres",
            "-Atc",
            &format!(
                "SELECT count(*) FROM pg_stat_activity WHERE query LIKE '%{marker}%' AND pid <> \
                 pg_backend_pid()"
            ),
        ])
        .output()
        .expect("docker");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<usize>()
        .unwrap_or(0)
        > 0
}

fn said(output: &Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

/// What the next run reports, in the plan's JSON findings.
fn recovered(output: &Output) -> Vec<String> {
    let report: serde_json::Value = serde_json::from_str(&stdout(output))
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", said(output)));
    report["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|finding| finding["id"] == "scratch.recovered")
        .map(|finding| finding["message"].as_str().unwrap().to_owned())
        .collect()
}

/// Killed right after `docker run`, before it connects: its container is kept
/// by the next run, which cannot tell it from a run that has not connected
/// yet, until the grace has passed.
pub fn before_the_first_connection(server: &str) {
    let f = Fixture::new(server, "rec-dk-early");
    let mut child = f.start();
    let pid = child.id();
    until("the run's container", || !containers_of(pid).is_empty());
    child.kill().unwrap();
    child.wait().unwrap();
    let name = containers_of(pid).remove(0);
    let _left = Leftovers(vec![name.clone()]);
    let next = f.plan(&["--format", "json"]);
    assert_eq!(code(&next), 0, "{}", said(&next));
    assert!(exists(&name), "inside the grace, with no keeper: kept");
    assert!(
        recovered(&next).iter().all(|m| !m.contains(&name)),
        "and not reported as removed: {}",
        said(&next)
    );
}

/// Killed once its keeper has connected: the container is the dead run's, and
/// the next run removes it, with no wait.
pub fn after_the_connection(server: &str) {
    let f = Fixture::new(server, "rec-dk-conn");
    let mut child = f.start();
    let pid = child.id();
    until("the run's container", || !containers_of(pid).is_empty());
    let name = containers_of(pid).remove(0);
    let _left = Leftovers(vec![name.clone()]);
    until("the keeper's session", || keepers(&name) > 0);
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(exists(&name), "a killed run leaves its container");
    let next = f.plan(&["--format", "json"]);
    assert_eq!(code(&next), 0, "{}", said(&next));
    assert!(!exists(&name), "the next run removed it");
    assert!(
        recovered(&next).iter().any(|m| m.contains(&name)),
        "and said so: {}",
        said(&next)
    );
}

/// Killed inside its externals file: the same.
pub fn during_use(server: &str) {
    let f = Fixture::new(server, "rec-dk-used");
    f.externals("SELECT pg_sleep(120);\n");
    let mut child = f.start();
    let pid = child.id();
    until("the run's container", || !containers_of(pid).is_empty());
    let name = containers_of(pid).remove(0);
    let _left = Leftovers(vec![name.clone()]);
    until("the run inside its externals file", || {
        sleeping(&name, "pg_sleep(120)")
    });
    child.kill().unwrap();
    child.wait().unwrap();
    f.externals("SELECT 1;\n");
    let next = f.plan(&["--format", "json"]);
    assert_eq!(code(&next), 0, "{}", said(&next));
    assert!(!exists(&name), "the next run removed it");
    assert!(recovered(&next).iter().any(|m| m.contains(&name)));
}

/// A live run's container is never touched by another run, and neither is a
/// container of another tool, or one with the scratch label and no token.
pub fn a_live_run_and_foreign_containers_are_left_alone(server: &str) {
    let live = Fixture::new(server, "rec-dk-live");
    let other = Fixture::new(server, "rec-dk-other");
    live.externals("SELECT pg_sleep(25);\n");
    let image = std::env::var("PBPS_TEST_PG_SCRATCH_IMAGE").unwrap();
    let suffix = std::process::id();
    let foreign = [
        (format!("rec2410-foreign-{suffix}"), Vec::<&str>::new()),
        (
            format!("pbps-scratch-0123456789abcdef-{suffix}"),
            vec!["--label", "pbps.resolver=scratch"],
        ),
    ];
    let mut leftovers = Leftovers(Vec::new());
    for (name, labels) in &foreign {
        let status = Command::new("docker")
            .args(["run", "--detach", "--pull", "never", "--name", name])
            .args(labels)
            .args(["--entrypoint", "sleep", &image, "600"])
            .output()
            .unwrap();
        assert!(status.status.success(), "{status:?}");
        leftovers.0.push(name.clone());
    }
    let mut running = live.start();
    let pid = running.id();
    until("the live run's container", || {
        !containers_of(pid).is_empty()
    });
    let name = containers_of(pid).remove(0);
    leftovers.0.push(name.clone());
    until("the live run inside its externals file", || {
        sleeping(&name, "pg_sleep(25)")
    });
    let next = other.plan(&["--format", "json"]);
    let still = exists(&name);
    let foreign_still: Vec<bool> = foreign.iter().map(|(n, _)| exists(n)).collect();
    let finished = running.wait().unwrap();
    assert_eq!(code(&next), 0, "{}", said(&next));
    assert!(
        still,
        "the live run's container stood while another run swept"
    );
    assert_eq!(foreign_still, [true, true], "and so did the others");
    assert!(recovered(&next).is_empty(), "{}", said(&next));
    assert!(finished.success(), "and the live run was not disturbed");
}
