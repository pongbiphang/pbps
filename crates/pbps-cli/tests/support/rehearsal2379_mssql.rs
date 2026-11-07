//! The connected rehearsal of a SQL Server plan (#2379 PR 3, DEC-2379.3): its
//! statements run in order on a reproduction of the target's managed state, on
//! the scratch the selected resolver entry names, before the plan is written.
//! Each case runs on both kinds of scratch the entry can be: a server on an
//! instance of its own (the run-owned layout) and a Docker image.

use super::*;

/// The environment variables the fixture's `pbps.yml` reads.
const TARGET: &str = "PBPS_REH_TARGET";
const SCRATCH: &str = "PBPS_REH_SCRATCH";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A login that may create databases, on an instance of its own: the run
    /// makes and drops its login and database.
    Server,
    /// A container pbps starts and removes.
    Docker,
}

impl Kind {
    fn phrase(self) -> &'static str {
        match self {
            Kind::Docker => "docker scratch",
            Kind::Server => "server scratch",
        }
    }
}

fn image() -> String {
    std::env::var("PBPS_TEST_RESOLVER_MSSQL_IMAGE")
        .or_else(|_| std::env::var("PBPS_TEST_DEV_IMAGE"))
        .expect("PBPS_TEST_RESOLVER_MSSQL_IMAGE (or PBPS_TEST_DEV_IMAGE) must name the SQL Server image")
}

/// What differs from the default project.
#[derive(Default, Clone)]
pub struct Options<'a> {
    /// No `resolve_with`: no scratch entry is selected.
    pub unselected: bool,
    /// Extra lines under the `prod` environment.
    pub environment: &'a str,
}

pub struct Fixture {
    pub d: Demo,
    own: OwnDatabase,
    pub kind: Kind,
    scratch_server: String,
}

const COUNTRY: &str = "table: dbo.country\ncolumns:\n  code: {type: varchar(2), nullable: false}\n  \
    label: {type: varchar(40), nullable: false}\nprimary_key: {name: pk_country, columns: [code]}\n\
    data:\n  mode: ensure\n  rows:\n";

fn country(taiwan: &str) -> String {
    format!("{COUNTRY}    TW: {{label: \"{taiwan}\"}}\n    JP: {{label: Japan}}\n")
}

const TABLE: &str = "table: dbo.t\ncolumns:\n  id: {type: int, nullable: false}\n  \
    amount: {type: int, nullable: false, default: \"0\"}\n  note: {type: nvarchar(40)}\n\
    primary_key: {name: pk_t, columns: [id]}\nchecks:\n  ck: amount >= 0\n";

impl Fixture {
    pub fn new(server: &str, slug: &str, kind: Kind, options: &Options<'_>) -> Self {
        let own = OwnDatabase::new(server, slug);
        let connection = own.connection().to_owned();
        let d = Demo::new(slug);
        let write = |name: &str, body: &str| {
            std::fs::write(d.dir.join("schema").join(name), body).unwrap();
        };
        write("dbo.t.yml", TABLE);
        write("dbo.country.yml", &country("Taiwan"));
        write(
            "view.yml",
            "view: dbo.v\ndefinition: SELECT id, amount FROM dbo.t\n",
        );
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", &connection]));
        let scratch_server = std::env::var("PBPS_TEST_MSSQL_SCRATCH_DB")
            .expect("PBPS_TEST_MSSQL_SCRATCH_DB is not set; see scripts/live-tests.sh");
        let fixture = Self {
            d,
            own,
            kind,
            scratch_server,
        };
        fixture.configure(options);
        succeeds(fixture.d.run(&[
            "key",
            "generate",
            "--out",
            fixture.d.dir.join("key").to_str().unwrap(),
        ]));
        fixture
    }

    /// A fixture whose target pbps did not build: its table default and check
    /// call a function the target also holds, which the plain bootstrap cannot
    /// write (it creates the table first, error 342), so the target is made by
    /// hand, its declarations are pulled from it and it is baselined.
    pub fn from_a_target_that_calls_its_own_function(server: &str, slug: &str, kind: Kind) -> Self {
        let own = OwnDatabase::new(server, slug);
        let connection = own.connection().to_owned();
        for sql in [
            "CREATE FUNCTION dbo.f (@x int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN @x + 1 END",
            "CREATE TABLE dbo.t (id int NOT NULL CONSTRAINT pk_t PRIMARY KEY, \
             amount int NOT NULL CONSTRAINT df_t_amount DEFAULT (dbo.f(1)), \
             note nvarchar(40) NULL, CONSTRAINT ck_t CHECK (dbo.f(amount) >= 0))",
            "CREATE TABLE dbo.country (code varchar(2) NOT NULL CONSTRAINT pk_country PRIMARY KEY, \
             label varchar(40) NOT NULL); \
             INSERT INTO dbo.country VALUES ('TW', 'Taiwan'), ('JP', 'Japan')",
        ] {
            on_server(&connection, sql);
        }
        let d = Demo::new(slug);
        succeeds(d.run(&["pull", "--db", &connection]));
        std::fs::write(d.dir.join("schema/dbo.country.yml"), country("Taiwan")).unwrap();
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        succeeds(d.run(&["baseline", "--db", &connection, "--reason", "adopt 2379"]));
        let fixture = Self {
            d,
            own,
            kind,
            scratch_server: std::env::var("PBPS_TEST_MSSQL_SCRATCH_DB")
                .expect("PBPS_TEST_MSSQL_SCRATCH_DB is not set; see scripts/live-tests.sh"),
        };
        fixture.configure(&Options::default());
        succeeds(fixture.d.run(&[
            "key",
            "generate",
            "--out",
            fixture.d.dir.join("key").to_str().unwrap(),
        ]));
        fixture
    }

    fn configure(&self, options: &Options<'_>) {
        let entry = match self.kind {
            Kind::Docker => format!(
                "    kind: docker\n    image: '{}'\n    pull: never\n",
                image()
            ),
            Kind::Server => format!("    kind: server\n    url_env: {SCRATCH}\n"),
        };
        let selected = if options.unselected {
            ""
        } else {
            "    resolve_with: scratch\n"
        };
        std::fs::write(
            self.d.dir.join("pbps.yml"),
            format!(
                "dialect: mssql\nresolvers:\n  scratch:\n{entry}environments:\n  prod:\n    \
                 url_env: {TARGET}\n{selected}    fingerprint_key_file: key\n{}",
                options.environment
            ),
        )
        .unwrap();
    }

    pub fn target(&self) -> &str {
        self.own.connection()
    }

    /// Rewrites a declared row, so the plan moves data and nothing a kept
    /// binding could reach: the resolver is not needed.
    pub fn revise_row(&self) {
        std::fs::write(
            self.d.dir.join("schema/dbo.country.yml"),
            country("Taiwan, ROC"),
        )
        .unwrap();
        self.d.commit();
    }

    /// Declares the two rows of `dbo.country` with one label and a unique
    /// constraint over it, which the rows break.
    pub fn make_labels_unique(&self) {
        let table = country("Japan").replace(
            "primary_key: {name: pk_country, columns: [code]}\n",
            "primary_key: {name: pk_country, columns: [code]}\nunique:\n  uq_label: [label]\n",
        );
        std::fs::write(self.d.dir.join("schema/dbo.country.yml"), table).unwrap();
        self.d.commit();
    }

    pub fn artifact(&self) -> std::path::PathBuf {
        self.d.dir.join("plan.json")
    }

    /// `plan --env prod --out plan.json` with `extra` arguments, the scratch
    /// variable set when the kind has one.
    pub fn plan(&self, extra: &[&str]) -> Output {
        self.built("plan", extra, true).output().unwrap()
    }

    fn built(&self, verb: &str, extra: &[&str], scratch: bool) -> Command {
        let artifact = self.artifact();
        let mut args: Vec<&str> = vec![verb, "--env", "prod"];
        if verb == "plan" {
            args.extend(["--out", artifact.to_str().unwrap()]);
        }
        args.extend(extra);
        let mut cmd = Command::new(BIN);
        cmd.arg("--project")
            .arg(&self.d.dir)
            .args(&args)
            .env(TARGET, self.target())
            .env_remove(SCRATCH);
        if scratch && self.kind == Kind::Server {
            cmd.env(SCRATCH, &self.scratch_server);
        }
        cmd
    }

    /// The plan file's `rehearsal` record, or `Null` where it has none.
    pub fn record(&self) -> serde_json::Value {
        let plan: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(self.artifact()).unwrap()).unwrap();
        plan.get("rehearsal").cloned().unwrap_or_default()
    }

    /// `apply` of the plan file with the scratch out of reach.
    pub fn apply(&self, allow: &str) -> Output {
        let checksum = plan_checksum(&self.artifact());
        Command::new(BIN)
            .arg("--project")
            .arg(&self.d.dir)
            .args([
                "apply",
                "--env",
                "prod",
                "--plan",
                self.artifact().to_str().unwrap(),
                "--checksum",
                &checksum,
                "--allow",
                allow,
            ])
            .env(TARGET, self.target())
            .env(SCRATCH, "Server=127.0.0.1,1;User Id=u;Password=p")
            .output()
            .unwrap()
    }

    /// `explain --plan plan.json`.
    pub fn explain(&self, format: &str) -> Output {
        Command::new(BIN)
            .arg("--project")
            .arg(&self.d.dir)
            .args([
                "explain",
                "--plan",
                self.artifact().to_str().unwrap(),
                "--format",
                format,
            ])
            .output()
            .unwrap()
    }

    /// What a rehearsal leaves on the scratch instance: the run's databases
    /// and logins. Nothing, once it is done.
    pub fn leftovers(&self) -> Vec<String> {
        match self.kind {
            Kind::Docker => Vec::new(),
            Kind::Server => resources(&self.scratch_server),
        }
    }
}

fn resources(server: &str) -> Vec<String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut conn = connect_live(server).await.unwrap();
            conn.query(
                "SELECT name AS resource FROM sys.databases WHERE name LIKE 'pbps_scratch_%' \
                 UNION ALL SELECT name FROM sys.server_principals WHERE name LIKE 'pbps_run_%' \
                 ORDER BY resource;",
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get::<&str>("resource").unwrap().unwrap().to_owned())
            .collect()
        })
}

fn said(output: &Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

fn succeeds(output: Output) -> Output {
    assert_eq!(code(&output), 0, "{}", said(&output));
    output
}

/// The number of server principals on the instance that `connection` reaches.
fn server_principals(connection: &str) -> i64 {
    text_on_server(
        connection,
        "SELECT CAST(COUNT(*) AS varchar(20)) FROM sys.server_principals;",
    )
    .parse()
    .unwrap()
}

/// A converging plan seals `Converged`, a plan that needs no resolver still
/// rehearses, `explain` and `apply` show the record, `apply` acquires no
/// scratch, and the scratch leaves nothing on its instance.
pub fn converges(server: &str, kind: Kind) {
    let f = Fixture::new(
        server,
        &format!("rehearse-ok-{kind:?}").to_lowercase(),
        kind,
        &Options::default(),
    );
    f.revise_row();
    let before = f.leftovers();
    let out = f.plan(&[]);
    assert_eq!(code(&out), 0, "{kind:?}: {}", said(&out));
    let text = stdout(&out);
    assert!(
        text.contains(&format!(
            "Rehearsal: converged on scratch ({}, run-owned)",
            kind.phrase()
        )),
        "{kind:?}: {text}"
    );
    let record = f.record();
    assert_eq!(record["converged"]["entry"], "scratch", "{record}");
    assert_eq!(record["converged"]["layout"], "run-owned", "{record}");
    assert_eq!(
        record["converged"]["authorization"], "rehearsed",
        "{record}"
    );
    assert_eq!(
        record["converged"]["scratch"]["kind"],
        if kind == Kind::Docker {
            "docker"
        } else {
            "server"
        },
        "{record}"
    );
    assert_eq!(
        f.leftovers(),
        before,
        "{kind:?}: the run left resources behind"
    );

    // `explain` shows it from the file alone, in both formats.
    let explained = f.explain("text");
    assert!(
        stdout(&explained).contains(&format!(
            "rehearsal   converged on scratch ({}, run-owned)",
            kind.phrase()
        )),
        "{}",
        said(&explained)
    );
    let json: serde_json::Value = serde_json::from_slice(&f.explain("json").stdout).unwrap();
    assert_eq!(json["data"]["rehearsal"]["outcome"], "converged", "{json}");

    // Negative: an unrehearsed plan is not read as one. Removing the record
    // by hand changes the plan, so the checksum no longer matches.
    let mut stripped: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(f.artifact()).unwrap()).unwrap();
    stripped.as_object_mut().unwrap().remove("rehearsal");
    let bare = f.d.dir.join("bare.json");
    std::fs::write(&bare, serde_json::to_string(&stripped).unwrap()).unwrap();
    assert_ne!(plan_checksum(&bare), plan_checksum(&f.artifact()));

    // `apply` only displays it, and reaches no scratch.
    let applied = f.apply("data-update");
    assert_eq!(code(&applied), 0, "{}", said(&applied));
    assert!(
        stdout(&applied).contains("Rehearsal, recorded in the plan: converged on scratch"),
        "{}",
        said(&applied)
    );
}

/// A statement the engine rejects at the plan's step is found at plan time,
/// names the statement, and writes nothing; without the rehearsal the same
/// plan is written and apply is where the data refuses it.
pub fn refuses_a_statement_the_engine_rejects(server: &str, kind: Kind) {
    let f = Fixture::new(
        server,
        &format!("rehearse-reject-{kind:?}").to_lowercase(),
        kind,
        &Options::default(),
    );
    f.make_labels_unique();
    let prior = "prior plan 2379";
    std::fs::write(f.artifact(), prior).unwrap();
    let out = f.plan(&[]);
    assert_eq!(code(&out), 2, "{kind:?}: {}", said(&out));
    let text = said(&out);
    assert!(text.contains("rejected statement"), "{kind:?}: {text}");
    assert!(text.contains("uq_label"), "{kind:?}: {text}");
    assert!(text.contains("--no-rehearse"), "{kind:?}: {text}");
    // Nothing was written, and the file the plan would have replaced is as it was.
    assert_eq!(std::fs::read_to_string(f.artifact()).unwrap(), prior);

    // Negative: without the rehearsal the same plan is written, and apply is
    // where the data refuses it, which is what the rehearsal moves to plan time.
    let off = f.plan(&["--no-rehearse"]);
    assert_eq!(code(&off), 0, "{kind:?}: {}", said(&off));
    assert_eq!(f.record(), serde_json::json!("off"));
    let applied = f.apply("constraint,data-update");
    assert_ne!(code(&applied), 0, "{kind:?}: {}", said(&applied));
}

/// Both switches seal `Off`, reach no scratch, and are shown as what they are.
pub fn off_switches_seal_off(server: &str, kind: Kind) {
    let f = Fixture::new(
        server,
        &format!("rehearse-off-{kind:?}").to_lowercase(),
        kind,
        &Options::default(),
    );
    f.revise_row();
    // The per-run flag, with the scratch out of reach: nothing is acquired.
    let off = f.built("plan", &["--no-rehearse"], false).output().unwrap();
    assert_eq!(code(&off), 0, "{kind:?}: {}", said(&off));
    assert_eq!(f.record(), serde_json::json!("off"));
    assert!(
        stdout(&off).contains("Rehearsal: off"),
        "{kind:?}: {}",
        said(&off)
    );
    let explained = f.explain("text");
    assert!(
        stdout(&explained).contains("not rehearsed"),
        "{}",
        said(&explained)
    );
    // The environment's setting says the same, and a run does not need to ask.
    let configured = std::fs::read_to_string(f.d.dir.join("pbps.yml")).unwrap();
    std::fs::write(
        f.d.dir.join("pbps.yml"),
        configured.replace(
            "    fingerprint_key_file: key\n",
            "    fingerprint_key_file: key\n    rehearse: false\n",
        ),
    )
    .unwrap();
    let off = f.built("plan", &[], false).output().unwrap();
    assert_eq!(code(&off), 0, "{kind:?}: {}", said(&off));
    assert_eq!(f.record(), serde_json::json!("off"));
}

/// No selected entry: nothing to rehearse on or to turn off, so no record.
pub fn no_entry_no_record(server: &str) {
    let f = Fixture::new(
        server,
        "rehearse-none",
        Kind::Server,
        &Options {
            unselected: true,
            ..Options::default()
        },
    );
    f.revise_row();
    let out = f.plan(&[]);
    assert_eq!(code(&out), 0, "{}", said(&out));
    assert!(f.record().is_null(), "{}", f.record());
    assert!(!stdout(&out).contains("Rehearsal"), "{}", said(&out));
}

/// A target whose table default and check call a function it also holds has
/// no base the plain bootstrap can write (it creates the table before the
/// function, error 342). The rehearsal builds the base through the engine's
/// creation trials, as the resolver builds the declarations, and converges.
pub fn a_base_that_calls_its_own_function_rehearses(server: &str, kind: Kind) {
    let f = Fixture::from_a_target_that_calls_its_own_function(
        server,
        &format!("rehearse-calls-{kind:?}").to_lowercase(),
        kind,
    );
    f.revise_row();
    let out = f.plan(&[]);
    assert_eq!(code(&out), 0, "{kind:?}: {}", said(&out));
    assert_eq!(
        f.record()["converged"]["layout"],
        "run-owned",
        "{kind:?}: {}",
        said(&out)
    );
}

/// Roles and the privileges they hold are database-scoped on SQL Server, so a
/// rehearsal runs a plan's role and grant changes under their own names on the
/// scratch and makes no server principal anywhere (DEC-2379.3): the target's
/// instance and the scratch's hold the same principals afterwards.
pub fn authorization_is_rehearsed_and_makes_no_server_principal(server: &str, kind: Kind) {
    let f = Fixture::new(
        server,
        &format!("rehearse-auth-{kind:?}").to_lowercase(),
        kind,
        &Options::default(),
    );
    std::fs::write(
        f.d.dir.join("schema/reader.yml"),
        "role: reader2379\ngrants:\n  dbo.t: [select]\n",
    )
    .unwrap();
    succeeds(f.d.run(&["plan", "--no-dev"]));
    f.d.commit();
    let target_before = server_principals(f.target());
    let scratch_before = server_principals(&f.scratch_server);
    let out = f.plan(&[]);
    assert_eq!(code(&out), 0, "{kind:?}: {}", said(&out));
    let record = f.record();
    assert_eq!(
        record["converged"]["authorization"], "rehearsed",
        "{record}"
    );
    assert!(
        stdout(&out).contains("1 change"),
        "{kind:?}: {}",
        said(&out)
    );
    assert_eq!(server_principals(f.target()), target_before);
    assert_eq!(server_principals(&f.scratch_server), scratch_before);
    // Negative: the target itself was not touched by planning.
    assert_eq!(
        text_on_server(
            f.target(),
            "SELECT CAST(COUNT(*) AS varchar(20)) FROM sys.database_principals WHERE name = 'reader2379';"
        ),
        "0"
    );
}

/// The sessions the dead run's login still has on the scratch instance: until
/// the server has seen them go, a sweep is right to call the run alive.
fn sessions_of_run_logins(connection: &str) -> i64 {
    text_on_server(
        connection,
        "SELECT CAST(COUNT(*) AS varchar(20)) FROM sys.dm_exec_sessions \
         WHERE login_name LIKE 'pbps_run_%';",
    )
    .parse()
    .unwrap()
}

/// A rehearsal killed with SIGKILL leaves the database and login it made on the
/// scratch instance, and the next run removes exactly them (#2410, #2379).
pub fn a_killed_rehearsal_is_cleaned_up_by_the_next_run(server: &str) {
    use std::time::{Duration, Instant};
    let f = Fixture::new(server, "rehearse-kill", Kind::Server, &Options::default());
    f.revise_row();
    let before = f.leftovers();
    let mut dead = f
        .built("plan", &["--format", "json"], true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let left = loop {
        let now = f.leftovers();
        if now.iter().any(|name| !before.contains(name)) {
            break now;
        }
        if let Some(status) = dead.try_wait().unwrap() {
            panic!("the plan ended ({status}) before it made anything to leave behind");
        }
        assert!(
            started.elapsed() < Duration::from_secs(90),
            "gave up waiting for the run's database"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    dead.kill().unwrap();
    dead.wait().unwrap();
    // Negative: a kill leaves them, which is why the next run has work to do.
    assert!(
        f.leftovers().iter().any(|name| !before.contains(name)),
        "{left:?}"
    );
    // The server notices its client gone.
    let started = Instant::now();
    while sessions_of_run_logins(&f.scratch_server) != 0 {
        assert!(
            started.elapsed() < Duration::from_secs(90),
            "the dead run's session did not end"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // The next run removes them, says so, and leaves nothing of its own.
    let next = f.plan(&["--format", "json"]);
    assert_eq!(code(&next), 0, "{}", said(&next));
    let report: serde_json::Value = serde_json::from_str(&stdout(&next))
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", said(&next)));
    let recovered = report["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|finding| finding["id"] == "scratch.recovered");
    assert!(recovered, "{}", said(&next));
    assert_eq!(f.leftovers(), before);
}
