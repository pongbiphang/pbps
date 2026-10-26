//! A real `pbps plan` killed with SIGKILL, then the next one (#2410,
//! DEC-2410.1): what the killed run left on the scratch is removed by the
//! next run, and the report says so. The in-process cases in
//! `scratch/live_tests/recovery2410_tests.rs` cover each stage by construction;
//! these prove that nothing between a simulated death and a real one was
//! missed, with the stage chosen by what the engine shows.

use super::*;
use std::collections::BTreeSet;
use std::process::Stdio;
use std::time::{Duration, Instant};

const TARGET: &str = "PBPS_REC_TARGET";
const SCRATCH: &str = "PBPS_REC_SCRATCH";
const LIMIT: Duration = Duration::from_secs(60);

const TABLE: &str = "table: app.t\ncolumns:\n  id: {type: integer, nullable: false}\n  \
    amount: {type: numeric, nullable: false, default: \"0\"}\n  note: {type: text}\n\
    primary_key: {name: pk_t, columns: [id]}\n";

const FUNCTION: &str = "function: app.f(integer)\npublic_execute: true\n\ndefinition: |-\n  \
    (integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1 + 1\n";

const VIEW: &str = "view: app.v\ndefinition: SELECT id, app.f(id) AS next, amount FROM app.t\n";

/// A login confined to a database it owns, dropped with it: the supplied
/// layout. It shares the target's cluster, as such an account must.
struct Account {
    server: String,
    login: String,
    database: String,
    connection: String,
}

impl Account {
    fn new(server: &str, slug: &str) -> Self {
        let token = format!("{}_{:08x}", std::process::id(), rand::random::<u32>());
        let login = format!("pbps_rec_{}_{token}", slug.replace('-', "_"));
        let password = format!("p{token}");
        on_server(
            server,
            &format!(
                "CREATE ROLE {login} LOGIN PASSWORD '{password}' NOSUPERUSER NOCREATEDB \
                 NOCREATEROLE; GRANT pg_read_all_settings TO {login}"
            ),
        );
        on_server(
            server,
            &format!("CREATE DATABASE {login} OWNER {login} TEMPLATE template0"),
        );
        let kept: Vec<&str> = server
            .split_whitespace()
            .filter(|w| {
                !w.starts_with("dbname=") && !w.starts_with("user=") && !w.starts_with("password=")
            })
            .collect();
        Self {
            server: server.to_owned(),
            connection: format!(
                "{} dbname={login} user={login} password={password}",
                kept.join(" ")
            ),
            database: login.clone(),
            login,
        }
    }
}

impl Drop for Account {
    fn drop(&mut self) {
        let _ = try_on_server(
            &self.server,
            &format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.database),
        );
        let _ = try_on_server(&self.server, &format!("DROP ROLE IF EXISTS {}", self.login));
    }
}

pub struct Fixture {
    d: Demo,
    own: OwnDatabase,
    scratch: String,
    /// The cluster the scratch is on, and a connection to it as a superuser.
    cluster: String,
    _account: Option<Account>,
}

impl Fixture {
    /// `supplied` is the confined-account layout; otherwise a superuser on a
    /// cluster of its own, which gets the run-owned one.
    fn new(server: &str, slug: &str, supplied: bool) -> Self {
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
        let (account, scratch, cluster) = if supplied {
            let account = Account::new(server, slug);
            let scratch = account.connection.clone();
            (Some(account), scratch, server.to_owned())
        } else {
            let scratch = std::env::var("PBPS_TEST_PG_SCRATCH_DB")
                .expect("PBPS_TEST_PG_SCRATCH_DB is not set; see scripts/live-tests-pg.sh");
            (None, scratch.clone(), scratch)
        };
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!(
                "dialect: postgres\nresolvers:\n  scratch:\n    kind: server\n    url_env: {SCRATCH}\n    \
                 externals: externals.sql\nenvironments:\n  prod:\n    url_env: {TARGET}\n    \
                 resolve_with: scratch\n    fingerprint_key_file: key\n"
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
        // reach: the resolver is needed, so a scratch is acquired.
        write(
            "app.t.yml",
            &TABLE.replace(
                "  note: {type: text}\n",
                "  note: {type: text}\n  extra: {type: boolean, nullable: true, default: \"true\"}\n",
            ),
        );
        succeeds(d.run(&["plan"]));
        d.commit();
        Self {
            d,
            own,
            scratch,
            cluster,
            _account: account,
        }
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
            .env(TARGET, self.own.connection())
            .env(SCRATCH, &self.scratch);
        cmd
    }

    /// A plan, left running.
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

    fn text(&self, sql: &str) -> String {
        text_of(&self.cluster, sql)
    }

    /// Names of the run-owned databases and roles on the cluster.
    fn names(&self) -> BTreeSet<String> {
        self.text(
            "SELECT coalesce(string_agg(n, ','), '') FROM (\
               SELECT datname::text AS n FROM pg_database WHERE datname LIKE 'pbps\\_scratch\\_%' \
               UNION ALL SELECT rolname::text FROM pg_roles WHERE rolname LIKE 'pbps\\_run\\_%' \
                  OR rolname LIKE 'pbps\\_role\\_%') s",
        )
        .split(',')
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
    }

    /// Waits for `ready` to hold, or panics saying what it waited for.
    fn until(&self, what: &str, ready: impl Fn(&Self) -> bool) {
        let started = Instant::now();
        while !ready(self) {
            assert!(started.elapsed() < LIMIT, "gave up waiting for {what}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Kills `child` with SIGKILL and reaps it.
    fn kill(child: &mut std::process::Child) {
        child.kill().unwrap();
        child.wait().unwrap();
    }
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

/// Killed once its database exists: the next run removes the database, the
/// login and every run-local role, and names them.
pub fn run_owned_after_creation(server: &str) {
    let f = Fixture::new(server, "rec-owned-created", false);
    let before = f.names();
    let mut child = f.start();
    f.until("the run's database", |f| {
        f.names()
            .difference(&before)
            .any(|n| n.starts_with("pbps_scratch_"))
    });
    Fixture::kill(&mut child);
    let left: BTreeSet<_> = f.names().difference(&before).cloned().collect();
    let database = left
        .iter()
        .find(|n| n.starts_with("pbps_scratch_"))
        .unwrap();
    let token = database.trim_start_matches("pbps_scratch_").to_owned();
    assert!(
        left.contains(&format!("pbps_run_{token}")),
        "its record is there: {left:?}"
    );
    // Its sessions end with the process; the server notices at once.
    f.until("the dead run's sessions", |f| {
        f.text(&format!(
            "SELECT count(*)::text FROM pg_stat_activity WHERE datname = '{database}'"
        )) == "0"
    });
    let next = f.plan(&["--format", "json"]);
    assert_eq!(code(&next), 0, "{}", said(&next));
    assert_eq!(f.names(), before, "exactly the dead run's resources went");
    let messages = recovered(&next);
    assert!(
        messages.iter().any(|m| m.contains(database.as_str())),
        "{messages:?}"
    );
    assert!(messages.iter().all(|m| m.contains(&token)), "{messages:?}");
    let human = f.plan(&[]);
    assert!(
        !said(&human).contains("ended without cleaning up"),
        "a clean cluster says nothing"
    );
}

/// Killed inside the externals file (the run is using its scratch): the same,
/// once the server has finished the statement the dead client started.
pub fn run_owned_during_use(server: &str) {
    let f = Fixture::new(server, "rec-owned-used", false);
    f.externals("SELECT pg_sleep(12);\n");
    let before = f.names();
    let mut child = f.start();
    f.until("the run inside its externals file", |f| {
        f.text("SELECT count(*)::text FROM pg_stat_activity WHERE query LIKE '%pg_sleep(12)%' AND pid <> pg_backend_pid()") != "0"
    });
    Fixture::kill(&mut child);
    let left: BTreeSet<_> = f.names().difference(&before).cloned().collect();
    assert!(
        left.iter().any(|n| n.starts_with("pbps_role_"))
            || left.iter().any(|n| n.starts_with("pbps_scratch_")),
        "{left:?}"
    );
    let database = left
        .iter()
        .find(|n| n.starts_with("pbps_scratch_"))
        .unwrap()
        .clone();
    // The sleeping backend finishes its statement, then sees its client gone.
    f.until("the dead run's sessions", |f| {
        f.text(&format!(
            "SELECT count(*)::text FROM pg_stat_activity WHERE datname = '{database}'"
        )) == "0"
    });
    f.externals("SELECT 1;\n");
    let next = f.plan(&["--format", "json"]);
    assert_eq!(code(&next), 0, "{}", said(&next));
    assert_eq!(f.names(), before, "exactly the dead run's resources went");
    assert!(!recovered(&next).is_empty(), "{}", said(&next));
}

/// A supplied run killed inside its externals file: the next run cleans the
/// database by its claim, where it used to refuse it as not empty.
pub fn supplied_during_use(server: &str) {
    let f = Fixture::new(server, "rec-supplied-used", true);
    f.externals("CREATE TABLE app.left_over (id integer);\nSELECT pg_sleep(12);\n");
    on_server(
        f.own.connection(),
        "CREATE TABLE app.left_over (id integer)",
    );
    let database = f._account.as_ref().unwrap().database.clone();
    let mut child = f.start();
    f.until("the run inside its externals file", |f| {
        f.text("SELECT count(*)::text FROM pg_stat_activity WHERE query LIKE '%pg_sleep(12)%' AND pid <> pg_backend_pid()") != "0"
    });
    Fixture::kill(&mut child);
    let in_database = |f: &Fixture, sql: &str| text_of(&with_dbname(&f.cluster, &database), sql);
    assert_eq!(
        in_database(
            &f,
            "SELECT count(*)::text FROM pg_namespace WHERE nspname = 'pbps_claim'"
        ),
        "1",
        "the claim is the record"
    );
    // The keeper notices within the check interval, the backend finishes its
    // statement: then nothing is connected for the run.
    f.until("the dead run's sessions", |f| {
        f.text(&format!(
            "SELECT count(*)::text FROM pg_stat_activity WHERE datname = '{database}'"
        )) == "0"
    });
    f.externals("CREATE TABLE app.left_over (id integer);\n");
    let next = f.plan(&["--format", "json"]);
    assert_eq!(code(&next), 0, "{}", said(&next));
    assert_eq!(
        in_database(
            &f,
            "SELECT count(*)::text FROM pg_namespace WHERE nspname !~ '^(pg_|information_schema$|public$)'"
        ),
        "0",
        "the database is empty again"
    );
    let messages = recovered(&next);
    assert!(
        messages.iter().any(|m| m.contains(&database)),
        "the cleaning is reported: {messages:?}"
    );
}

/// A supplied database a live run holds is refused, and left alone.
pub fn supplied_held_by_a_live_run(server: &str) {
    let f = Fixture::new(server, "rec-supplied-live", true);
    f.externals("SELECT pg_sleep(12);\n");
    let database = f._account.as_ref().unwrap().database.clone();
    let mut live = f.start();
    f.until("the run inside its externals file", |f| {
        f.text("SELECT count(*)::text FROM pg_stat_activity WHERE query LIKE '%pg_sleep(12)%' AND pid <> pg_backend_pid()") != "0"
    });
    let other = f.plan(&[]);
    let claims = text_of(
        &with_dbname(&f.cluster, &database),
        "SELECT count(*)::text FROM pg_namespace WHERE nspname = 'pbps_claim'",
    );
    let finished = live.wait().unwrap();
    assert_ne!(code(&other), 0, "{}", said(&other));
    assert!(
        said(&other).contains("in use by a run that is still running"),
        "{}",
        said(&other)
    );
    assert_eq!(
        claims, "1",
        "the live run's claim stood while the other was refused"
    );
    assert!(
        finished.success(),
        "and the live run was not disturbed by it"
    );
}
