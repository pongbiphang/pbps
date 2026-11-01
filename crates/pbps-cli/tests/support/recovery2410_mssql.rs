//! A real `pbps plan --dev` killed with SIGKILL against SQL Server, then the
//! next one (#2410, DEC-2410.2): what the killed run left on the scratch is
//! removed by the next run, and the report says so. The in-process cases in
//! `scratch/recovery2410_mssql_tests.rs` cover each stage by construction;
//! these prove that nothing between a simulated death and a real one was
//! missed, with the stage chosen by what the engine shows.

use super::*;
use std::collections::BTreeSet;
use std::process::Stdio;
use std::time::{Duration, Instant};

const SCRATCH: &str = "PBPS_REC2410_SCRATCH";
const LIMIT: Duration = Duration::from_secs(90);

/// A database a login owns on the scratch instance: the supplied layout.
struct Account {
    server: String,
    database: OwnDatabase,
    login: String,
    connection: String,
}

impl Account {
    fn new(server: &str, slug: &str) -> Self {
        let database = OwnDatabase::new(server, slug);
        let login = format!(
            "pbps_rec2410_{}_{}",
            slug.replace('-', "_"),
            std::process::id()
        );
        let password = "Pbps!Rec2410Supplied";
        on_server(
            server,
            &format!("CREATE LOGIN [{login}] WITH PASSWORD='{password}', CHECK_POLICY=OFF;"),
        );
        on_server(
            database.connection(),
            &format!(
                "ALTER AUTHORIZATION ON DATABASE::[{}] TO [{login}];",
                database.name()
            ),
        );
        let connection = format!(
            "{};User Id={login};Password={password}",
            database.connection().trim_end_matches(';')
        );
        Self {
            server: server.to_owned(),
            database,
            login,
            connection,
        }
    }
}

impl Drop for Account {
    fn drop(&mut self) {
        // OwnDatabase drops afterwards; its sessions go first, and the login
        // cannot be dropped while it owns the database.
        on_server(
            &self.server,
            &format!(
                "USE master; ALTER DATABASE [{0}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; \
                 DECLARE @sql nvarchar(max)=N'ALTER AUTHORIZATION ON DATABASE::[{0}] TO '+\
                 QUOTENAME(SUSER_SNAME()); EXEC(@sql);",
                self.database.name()
            ),
        );
        super::drop_test_login(&self.database.server, &self.login);
    }
}

pub struct Fixture {
    d: Demo,
    scratch: String,
    /// The instance's superuser, to look at it.
    server: String,
    account: Option<Account>,
    rt: tokio::runtime::Runtime,
}

impl Fixture {
    fn new(slug: &str, supplied: bool) -> Self {
        let server = std::env::var("PBPS_TEST_MSSQL_SCRATCH_DB")
            .expect("PBPS_TEST_MSSQL_SCRATCH_DB is not set; see scripts/live-tests.sh");
        let account = supplied.then(|| Account::new(&server, slug));
        let scratch = account
            .as_ref()
            .map_or_else(|| server.clone(), |a| a.connection.clone());
        let d = Demo::new(slug);
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!(
                "dialect: mssql\nresolvers:\n  scratch: {{kind: server, url_env: {SCRATCH}, \
                 externals: externals.sql}}\nresolve_with: scratch\ndev: {{resolver: scratch}}\n"
            ),
        )
        .unwrap();
        std::fs::write(d.dir.join("externals.sql"), "SELECT 1;\n").unwrap();
        d.table(ONE_COLUMN);
        assert_eq!(code(&d.run(&["plan", "--no-dev"])), 0);
        d.commit();
        // A change to rehearse against the committed baseline.
        d.table(
            "table: dbo.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  \
             extra: {type: int, nullable: true}\n",
        );
        Self {
            d,
            scratch,
            server,
            account,
            rt: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        }
    }

    fn externals(&self, sql: &str) {
        std::fs::write(self.d.dir.join("externals.sql"), sql).unwrap();
    }

    fn command(&self, extra: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.arg("--project")
            .arg(&self.d.dir)
            .arg("plan")
            .args(extra)
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

    /// The first column of `sql` on `connection`, as text.
    fn rows(&self, connection: &str, sql: &str) -> Vec<String> {
        self.rt.block_on(async {
            let mut conn = connect_live(connection).await.unwrap();
            conn.query(sql)
                .await
                .unwrap()
                .iter()
                .map(|row| {
                    row.try_get_at::<&str>(0)
                        .unwrap()
                        .unwrap_or_default()
                        .to_owned()
                })
                .collect()
        })
    }

    /// The run-owned databases and logins on the instance.
    fn names(&self) -> BTreeSet<String> {
        self.rows(
            &self.server,
            "SELECT name FROM sys.databases WHERE name LIKE N'pbps[_]scratch[_]%' OR name LIKE \
             N'pbps[_]reference[_]%' UNION ALL SELECT name FROM sys.server_principals WHERE \
             name LIKE N'pbps[_]run[_]%';",
        )
        .into_iter()
        .collect()
    }

    /// The sandbox users in the supplied database.
    fn sandboxes(&self) -> Vec<String> {
        let account = self.account.as_ref().unwrap();
        self.rows(
            account.database.connection(),
            "SELECT name FROM sys.database_principals WHERE name LIKE N'pbps[_]sandbox[_]%';",
        )
    }

    fn tables(&self) -> Vec<String> {
        let account = self.account.as_ref().unwrap();
        self.rows(
            account.database.connection(),
            "SELECT name FROM sys.tables;",
        )
    }

    fn marker(&self, token: &str) -> bool {
        self.rows(
            &self.server,
            &format!(
                "SELECT CONVERT(nvarchar(1), CASE WHEN OBJECT_ID(N'tempdb..##pbps_{token}') IS \
                 NULL THEN 0 ELSE 1 END);"
            ),
        ) == ["1"]
    }

    /// Waits for `ready` to hold, or panics saying what it waited for.
    fn until(&self, what: &str, ready: impl Fn(&Self) -> bool) {
        let started = Instant::now();
        while !ready(self) {
            assert!(started.elapsed() < LIMIT, "gave up waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn waiting(&self) -> bool {
        self.rows(
            &self.server,
            "SELECT CONVERT(nvarchar(10), COUNT(*)) FROM sys.dm_exec_requests WHERE wait_type = \
             N'WAITFOR';",
        ) != ["0"]
    }

    /// Kills `child` with SIGKILL and reaps it, then waits for the instance
    /// to show the run's marker gone.
    fn kill(&self, child: &mut std::process::Child, token: &str) {
        child.kill().unwrap();
        child.wait().unwrap();
        self.until("the dead run's marker to go", |f| !f.marker(token));
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

fn token_of(name: &str) -> String {
    name[name.len() - 32..].to_owned()
}

/// Killed once its database exists: the next run removes the database and the
/// login, and names them.
pub fn run_owned_after_creation() {
    let f = Fixture::new("rec-ms-owned", false);
    let before = f.names();
    let mut child = f.start();
    f.until("the run's database", |f| {
        f.names()
            .difference(&before)
            .any(|n| n.starts_with("pbps_scratch_"))
    });
    let left: BTreeSet<_> = f.names().difference(&before).cloned().collect();
    let database = left
        .iter()
        .find(|n| n.starts_with("pbps_scratch_"))
        .unwrap()
        .clone();
    let token = token_of(&database);
    f.kill(&mut child, &token);
    let left: BTreeSet<_> = f.names().difference(&before).cloned().collect();
    assert!(
        left.contains(&format!("pbps_run_{token}")),
        "its record is there: {left:?}"
    );
    let next = f.plan(&["--format", "json"]);
    assert_eq!(code(&next), 0, "{}", said(&next));
    assert_eq!(f.names(), before, "exactly the dead run's resources went");
    let messages = recovered(&next);
    assert!(
        messages.iter().any(|m| m.contains(&database)),
        "{messages:?}"
    );
}

/// Killed while it reads a fresh database's options from the reference
/// database it makes for the purpose: the window is short, so the run is
/// started again until a kill lands in it.
pub fn run_owned_in_the_reference_window() {
    let f = Fixture::new("rec-ms-reference", false);
    for attempt in 1..=12 {
        let before = f.names();
        let mut child = f.start();
        let reference = loop {
            if let Some(name) = f
                .names()
                .difference(&before)
                .find(|n| n.starts_with("pbps_reference_"))
            {
                break Some(name.clone());
            }
            if child.try_wait().unwrap().is_some() {
                break None;
            }
        };
        let Some(reference) = reference else {
            // It finished before the kill could land: nothing was left.
            assert_eq!(
                f.names(),
                before,
                "attempt {attempt}: a finished run leaves nothing"
            );
            continue;
        };
        let token = token_of(&reference);
        f.kill(&mut child, &token);
        assert!(
            f.names().contains(&reference),
            "the reference database is left"
        );
        let next = f.plan(&["--format", "json"]);
        assert_eq!(code(&next), 0, "{}", said(&next));
        assert_eq!(
            f.names(),
            before,
            "the reference window: exactly the dead run's resources went"
        );
        assert!(recovered(&next).iter().any(|m| m.contains(&reference)));
        return;
    }
    panic!("no kill landed in the reference window in 12 attempts");
}

/// Killed inside the externals file (the run is using its scratch): the same.
pub fn run_owned_during_use() {
    let f = Fixture::new("rec-ms-owned-used", false);
    f.externals("WAITFOR DELAY '00:00:30';\n");
    let before = f.names();
    let mut child = f.start();
    f.until("the run inside its externals file", |f| f.waiting());
    let left: BTreeSet<_> = f.names().difference(&before).cloned().collect();
    let database = left
        .iter()
        .find(|n| n.starts_with("pbps_scratch_"))
        .unwrap()
        .clone();
    f.kill(&mut child, &token_of(&database));
    f.externals("SELECT 1;\n");
    let next = f.plan(&["--format", "json"]);
    assert_eq!(code(&next), 0, "{}", said(&next));
    assert_eq!(f.names(), before, "exactly the dead run's resources went");
    assert!(!recovered(&next).is_empty(), "{}", said(&next));
}

/// A supplied run killed once its sandbox user is the only thing it made.
pub fn supplied_after_the_sandbox() {
    let f = Fixture::new("rec-ms-sup", true);
    let mut child = f.start();
    f.until("the run's sandbox user", |f| !f.sandboxes().is_empty());
    let token = f.sandboxes()[0]["pbps_sandbox_".len()..].to_owned();
    f.kill(&mut child, &token);
    assert!(!f.sandboxes().is_empty(), "the sandbox is left");
    let next = f.plan(&["--format", "json"]);
    assert_eq!(code(&next), 0, "{}", said(&next));
    assert!(f.sandboxes().is_empty(), "the database is empty again");
    assert!(
        recovered(&next)
            .iter()
            .any(|m| m.contains("database contents")),
        "{}",
        said(&next)
    );
}

/// A supplied run killed inside its externals file, with a table of its own
/// made: the next run cleans the database by the sandbox, where it used to
/// refuse it as not empty.
pub fn supplied_during_use() {
    let f = Fixture::new("rec-ms-sup-used", true);
    f.externals("CREATE TABLE dbo.left_over(i int);\nGO\nWAITFOR DELAY '00:00:30';\n");
    let mut child = f.start();
    f.until("the run inside its externals file", |f| f.waiting());
    let token = f.sandboxes()[0]["pbps_sandbox_".len()..].to_owned();
    f.kill(&mut child, &token);
    assert_eq!(f.tables(), ["left_over"], "its table is left");
    f.externals("SELECT 1;\n");
    let next = f.plan(&["--format", "json"]);
    assert_eq!(code(&next), 0, "{}", said(&next));
    assert!(
        f.tables().is_empty(),
        "the database is empty again: {:?}",
        f.tables()
    );
    assert!(!recovered(&next).is_empty(), "{}", said(&next));
}

/// A supplied database a live run holds is refused, and left alone.
pub fn supplied_held_by_a_live_run() {
    let f = Fixture::new("rec-ms-sup-live", true);
    f.externals("CREATE TABLE dbo.live_work(i int);\nGO\nWAITFOR DELAY '00:00:12';\n");
    let mut live = f.start();
    f.until("the run inside its externals file", |f| f.waiting());
    let other = f.plan(&[]);
    let tables = f.tables();
    let finished = live.wait().unwrap();
    assert_ne!(code(&other), 0, "{}", said(&other));
    assert!(
        said(&other).contains("in use by a run that is still running"),
        "{}",
        said(&other)
    );
    assert_eq!(
        tables,
        ["live_work"],
        "the live run's table stood while the other was refused"
    );
    assert!(
        finished.success(),
        "and the live run was not disturbed by it"
    );
}
