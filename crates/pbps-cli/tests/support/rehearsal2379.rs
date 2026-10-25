//! The connected rehearsal of a PostgreSQL plan (#2379, DEC-2379.2): its
//! statements run in order on a reproduction of the target's managed state,
//! on the scratch the selected resolver entry names, before the plan is
//! written. Each case runs on every kind of scratch the entry can be: a
//! server on another cluster (the run-owned layout), a confined account on
//! the target's own cluster (the supplied layout), and a Docker image.

use super::resolver_publication::Confined;
use super::*;

/// The environment variables the fixture's `pbps.yml` reads.
const TARGET: &str = "PBPS_REH_TARGET";
const SCRATCH: &str = "PBPS_REH_SCRATCH";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A superuser on a cluster of its own: the run makes and drops its
    /// login and database.
    Server,
    /// A confined account's own database on the target's cluster.
    Supplied,
    /// A container pbps starts and removes.
    Docker,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Server, Kind::Supplied, Kind::Docker];

    fn layout(self) -> &'static str {
        match self {
            Kind::Supplied => "supplied",
            Kind::Server | Kind::Docker => "run-owned",
        }
    }

    fn phrase(self) -> &'static str {
        match self {
            Kind::Docker => "docker scratch",
            Kind::Server | Kind::Supplied => "server scratch",
        }
    }
}

fn image() -> String {
    std::env::var("PBPS_TEST_PG_SCRATCH_IMAGE")
        .expect("PBPS_TEST_PG_SCRATCH_IMAGE is not set; see scripts/live-tests-pg.sh")
}

/// What differs from the default project.
#[derive(Default, Clone)]
pub struct Options<'a> {
    /// Run on the target and put in the entry's externals file.
    pub externals: Option<&'a str>,
    /// Run on the target only.
    pub target_only: Option<&'a str>,
    /// No `resolve_with`: no scratch entry is selected.
    pub unselected: bool,
    /// Extra lines under the `prod` environment.
    pub environment: &'a str,
    /// Written over `schema/app.v.view.yml` before the first plan.
    pub view: Option<&'a str>,
    /// A role already on the target's cluster, declared with USAGE on `app`.
    pub role: Option<&'a str>,
    /// The target holds a managed enum, which a table column uses.
    pub typed: bool,
}

pub struct Fixture {
    pub d: Demo,
    own: OwnDatabase,
    confined: Option<Confined>,
    pub kind: Kind,
    scratch_server: String,
}

const COUNTRY: &str = "table: app.country\ncolumns:\n  code: {type: varchar(2), nullable: false}\n  \
    label: {type: varchar(40), nullable: false}\nprimary_key: {name: pk_country, columns: [code]}\n\
    data:\n  mode: ensure\n  rows:\n";

fn country(taiwan: &str) -> String {
    format!("{COUNTRY}    TW: {{label: \"{taiwan}\"}}\n    JP: {{label: Japan}}\n")
}

const TABLE: &str = "table: app.t\ncolumns:\n  id: {type: integer, nullable: false}\n  \
    amount: {type: numeric, nullable: false, default: \"0\"}\n  note: {type: text}\n\
    primary_key: {name: pk_t, columns: [id]}\nchecks:\n  ck: amount >= 0\n\
    indexes:\n  ix:\n    columns: [note]\n    where: note IS NOT NULL\n";

const FUNCTION: &str = "function: app.f(integer)\npublic_execute: true\n\ndefinition: |-\n  \
    (integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1 + 1\n";

const VIEW: &str = "view: app.v\ndefinition: SELECT id, app.f(id) AS next, amount FROM app.t\n";

impl Fixture {
    pub fn new(server: &str, slug: &str, kind: Kind, options: &Options<'_>) -> Self {
        let own = OwnDatabase::new(server, slug);
        let connection = own.connection().to_owned();
        on_server(&connection, "CREATE SCHEMA app");
        let d = Demo::new(slug);
        let write = |name: &str, body: &str| {
            std::fs::write(d.dir.join("schema").join(name), body).unwrap();
        };
        write("app.t.yml", TABLE);
        write("app.country.yml", &country("Taiwan"));
        write("app.f%28integer%29.function.yml", FUNCTION);
        if let Some(sql) = options.externals {
            // On the target before it is bootstrapped, and in the entry's
            // externals file, which stages it on scratch (#1673).
            on_server(&connection, sql);
            std::fs::write(d.dir.join("externals.sql"), format!("{sql};\n")).unwrap();
        }
        write("app.v.view.yml", options.view.unwrap_or(VIEW));
        if options.typed {
            write("app.mood.yml", "enum: app.mood\nvalues: [sad, ok]\n");
            write(
                "app.t.yml",
                &TABLE.replace(
                    "  note: {type: text}\n",
                    "  note: {type: text}\n  mood: {type: app.mood, nullable: true}\n",
                ),
            );
        }
        if let Some(role) = options.role {
            write(
                "role.yml",
                &format!("role: {role}\ngrants:\n  schema::app: [usage]\n"),
            );
        }
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", &connection]));
        if let Some(sql) = options.target_only {
            on_server(&connection, sql);
        }
        let confined = (kind == Kind::Supplied).then(|| Confined::new(server));
        let scratch_server = std::env::var("PBPS_TEST_PG_SCRATCH_DB")
            .expect("PBPS_TEST_PG_SCRATCH_DB is not set; see scripts/live-tests-pg.sh");
        let fixture = Self {
            d,
            own,
            confined,
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

    fn configure(&self, options: &Options<'_>) {
        let externals = if options.externals.is_some() {
            "    externals: externals.sql\n"
        } else {
            ""
        };
        let entry = match self.kind {
            Kind::Docker => format!(
                "    kind: docker\n    image: '{}'\n    pull: never\n{externals}",
                image()
            ),
            Kind::Server | Kind::Supplied => {
                format!("    kind: server\n    url_env: {SCRATCH}\n{externals}")
            }
        };
        let selected = if options.unselected {
            ""
        } else {
            "    resolve_with: scratch\n"
        };
        std::fs::write(
            self.d.dir.join("pbps.yml"),
            format!(
                "dialect: postgres\nresolvers:\n  scratch:\n{entry}environments:\n  prod:\n    \
                 url_env: {TARGET}\n{selected}    fingerprint_key_file: key\n{}",
                options.environment
            ),
        )
        .unwrap();
    }

    pub fn target(&self) -> &str {
        self.own.connection()
    }

    /// The scratch connection a command is given, if its kind has one.
    pub fn scratch(&self) -> Option<String> {
        match self.kind {
            Kind::Server => Some(self.scratch_server.clone()),
            Kind::Supplied => Some(self.confined.as_ref().unwrap().connection.clone()),
            Kind::Docker => None,
        }
    }

    /// Rewrites a declared row, so the plan moves data and nothing a kept
    /// binding could reach: the resolver is not needed.
    pub fn revise_row(&self) {
        std::fs::write(
            self.d.dir.join("schema/app.country.yml"),
            country("Taiwan, ROC"),
        )
        .unwrap();
        self.d.commit();
    }

    /// Adds a column with a default, which a kept binding could reach: the
    /// resolver is needed.
    pub fn add_column(&self, default: &str) {
        let table = TABLE.replace(
            "  note: {type: text}\n",
            &format!("  note: {{type: text}}\n  extra: {{type: boolean, nullable: true, default: \"{default}\"}}\n"),
        );
        std::fs::write(self.d.dir.join("schema/app.t.yml"), table).unwrap();
        succeeds(self.d.run(&["plan"]));
        self.d.commit();
    }

    pub fn artifact(&self) -> std::path::PathBuf {
        self.d.dir.join("plan.json")
    }

    /// `plan --env prod --out plan.json` with `extra` arguments, the scratch
    /// variable set when the kind has one.
    pub fn plan(&self, extra: &[&str]) -> Output {
        self.command("plan", extra, true)
    }

    /// [`Self::plan`], and the process it ran as: a scratch container is
    /// labelled with its starter's pid, and containers other runs on this
    /// machine start are not this plan's.
    pub fn plan_as_process(&self, extra: &[&str]) -> (Output, u32) {
        let mut cmd = self.built("plan", extra, true);
        let child = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        (child.wait_with_output().unwrap(), pid)
    }

    fn command(&self, verb: &str, extra: &[&str], scratch: bool) -> Output {
        self.built(verb, extra, scratch).output().unwrap()
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
        if scratch && let Some(connection) = self.scratch() {
            cmd.env(SCRATCH, connection);
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
        let mut cmd = Command::new(BIN);
        cmd.arg("--project")
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
            .env(SCRATCH, "host=127.0.0.1 port=1 dbname=unused");
        cmd.output().unwrap()
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
}

/// Scratch containers the process `pid` created at or after `since`.
fn created_by(pid: u32, since: u64) -> Vec<String> {
    let now = unix_now() + 1;
    let out = Command::new("docker")
        .args([
            "events",
            "--since",
            &since.to_string(),
            "--until",
            &now.to_string(),
            "--filter",
            "type=container",
            "--filter",
            "event=create",
            "--format",
            "{{.Actor.Attributes.name}} {{index .Actor.Attributes \"pbps.resolver.process\"}}",
        ])
        .output()
        .expect("docker");
    let label = format!("{pid}-");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(name, process)| name.starts_with("pbps-scratch-") && process.starts_with(&label))
        .map(|(name, _)| name.to_owned())
        .collect()
}

fn remaining_containers() -> Vec<String> {
    let out = Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            "name=pbps-scratch-",
            "--format",
            "{{.Names}}",
        ])
        .output()
        .expect("docker");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn said(output: &Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

fn spellings(record: &serde_json::Value) -> Vec<String> {
    record["converged"]["spelling"]
        .as_array()
        .map(|lines| {
            lines
                .iter()
                .map(|line| line.as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// A converging plan seals `Converged`, with the spellings the engine stored,
/// and a plan that needs no resolver still rehearses. `explain` and `apply`
/// show the record, and `apply` acquires no scratch.
pub fn converges(server: &str, kind: Kind) {
    let f = Fixture::new(
        server,
        &format!("rehearse-ok-{kind:?}").to_lowercase(),
        kind,
        &Options::default(),
    );
    f.revise_row();
    let since = unix_now();
    let (out, pid) = f.plan_as_process(&[]);
    assert_eq!(code(&out), 0, "{kind:?}: {}", said(&out));
    let text = stdout(&out);
    assert!(text.contains("not needed for binding"), "{kind:?}: {text}");
    assert!(
        text.contains(&format!(
            "Rehearsal: converged on scratch ({}, {})",
            kind.phrase(),
            kind.layout()
        )),
        "{kind:?}: {text}"
    );
    let record = f.record();
    assert_eq!(record["converged"]["entry"], "scratch", "{record}");
    assert_eq!(record["converged"]["layout"], kind.layout(), "{record}");
    assert_eq!(
        record["converged"]["scratch"]["kind"],
        if kind == Kind::Docker {
            "docker"
        } else {
            "server"
        },
        "{record}"
    );
    // What the engine stored of a check and of an index predicate is not what
    // was declared, and the record keeps both.
    let spelt = spellings(&record).join("\n");
    assert!(
        spelt.contains("check ck on app.t: declared `amount >= 0`"),
        "{spelt}"
    );
    assert!(
        spelt.contains("index ix on app.t: declared where `note IS NOT NULL`"),
        "{spelt}"
    );
    assert!(text.contains("the engine stores `"), "{text}");

    // `explain` shows it from the file alone, in both formats.
    let explained = f.explain("text");
    assert!(
        stdout(&explained).contains(&format!(
            "rehearsal   converged on scratch ({}, {})",
            kind.phrase(),
            kind.layout()
        )),
        "{}",
        said(&explained)
    );
    let json: serde_json::Value = serde_json::from_slice(&f.explain("json").stdout).unwrap();
    assert_eq!(json["data"]["rehearsal"]["outcome"], "converged", "{json}");

    // `plan --format json` carries the same record.
    let json = json_output(f.plan(&["--format", "json"]));
    assert_eq!(json["data"]["rehearsal"]["outcome"], "converged", "{json}");

    // Negative: an unrehearsed plan is not read as one. Removing the record
    // by hand changes the plan, and the explanation stops claiming it.
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
    if kind == Kind::Docker {
        let made = created_by(pid, since);
        assert_eq!(made.len(), 1, "{made:?}");
        assert!(
            remaining_containers()
                .iter()
                .all(|name| !made.contains(name)),
            "the plan's scratch container is left"
        );
    }
}

/// The resolver and the rehearsal both ran, and on a Docker entry they shared
/// one container.
pub fn shares_one_scratch(server: &str, kind: Kind) {
    let f = Fixture::new(
        server,
        &format!("rehearse-one-{kind:?}").to_lowercase(),
        kind,
        &Options::default(),
    );
    f.add_column("true");
    let since = unix_now();
    let (out, pid) = f.plan_as_process(&[]);
    assert_eq!(code(&out), 0, "{kind:?}: {}", said(&out));
    let text = stdout(&out);
    assert!(
        text.contains("it answered the binding questions"),
        "{kind:?}: {text}"
    );
    assert!(
        text.contains("Rehearsal: converged on scratch"),
        "{kind:?}: {text}"
    );
    let plan: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(f.artifact()).unwrap()).unwrap();
    assert_eq!(plan["origin"], "resolved_database", "{plan}");
    assert!(plan.get("rehearsal").is_some(), "{plan}");
    if kind == Kind::Docker {
        let made = created_by(pid, since);
        assert_eq!(made.len(), 1, "one container for both uses: {made:?}");
        assert!(
            remaining_containers()
                .iter()
                .all(|name| !made.contains(name))
        );
        // Negative: the process is the right one to ask about; with the
        // rehearsal off it makes the one container the resolver needs.
        let (_, alone) = f.plan_as_process(&["--no-rehearse"]);
        assert_eq!(created_by(alone, since).len(), 1);
    }
}

/// #2312's shape: a default with `IN`, emitted ungrouped, fails at apply. The
/// rehearsal meets it at plan time and names the statement. EXPECTED REFUSAL
/// while #2312 is open: when the emitter groups the default, this plan
/// converges, and this case flips to assert `Converged`.
pub fn refuses_the_ungrouped_default_of_issue_2312(server: &str, kind: Kind) {
    let f = Fixture::new(
        server,
        &format!("rehearse-2312-{kind:?}").to_lowercase(),
        kind,
        &Options::default(),
    );
    f.add_column("'a' IN ('a','b')");
    let prior = "prior plan 2379";
    std::fs::write(f.artifact(), prior).unwrap();
    let out = f.plan(&[]);
    assert_eq!(code(&out), 2, "{kind:?}: {}", said(&out));
    let text = said(&out);
    assert!(
        text.contains("rejected statement 1 of the plan"),
        "{kind:?}: {text}"
    );
    assert!(
        text.contains("DEFAULT 'a' IN ('a','b')"),
        "{kind:?}: {text}"
    );
    assert!(
        text.contains("syntax error at or near \"IN\""),
        "{kind:?}: {text}"
    );
    assert!(text.contains("--no-rehearse"), "{kind:?}: {text}");
    // Nothing was written, and the file the plan would have replaced is as it was.
    assert_eq!(std::fs::read_to_string(f.artifact()).unwrap(), prior);

    // Negative: without the rehearsal the same plan is written, and fails
    // only at apply, which is what the rehearsal exists to prevent.
    let off = f.plan(&["--no-rehearse"]);
    assert_eq!(code(&off), 0, "{kind:?}: {}", said(&off));
    assert_eq!(f.record(), serde_json::json!("off"));
    let applied = f.apply("");
    assert_ne!(code(&applied), 0, "{}", said(&applied));
    assert!(
        said(&applied).contains("syntax error at or near \"IN\""),
        "{}",
        said(&applied)
    );
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
    let flagged = f.command("plan", &["--no-rehearse"], false);
    assert_eq!(code(&flagged), 0, "{kind:?}: {}", said(&flagged));
    assert_eq!(f.record(), serde_json::json!("off"));
    assert!(stdout(&flagged).contains("Rehearsal: off (this plan was not rehearsed)"));
    let explained = f.explain("text");
    assert!(
        stdout(&explained).contains("rehearsal   off (this plan was not rehearsed)"),
        "{}",
        said(&explained)
    );
    let json: serde_json::Value = serde_json::from_slice(&f.explain("json").stdout).unwrap();
    assert_eq!(
        json["data"]["rehearsal"],
        serde_json::json!({"outcome": "off"})
    );
    let applied = f.apply("data-update");
    assert!(
        stdout(&applied).contains("Rehearsal, recorded in the plan: off"),
        "{}",
        said(&applied)
    );

    // The environment's `rehearse: false`.
    let f = Fixture::new(
        server,
        &format!("rehearse-env-{kind:?}").to_lowercase(),
        kind,
        &Options {
            environment: "    rehearse: false\n",
            ..Options::default()
        },
    );
    f.revise_row();
    let configured = f.command("plan", &[], false);
    assert_eq!(code(&configured), 0, "{kind:?}: {}", said(&configured));
    assert_eq!(f.record(), serde_json::json!("off"));

    // Negative: `rehearse: true` is the default, not a switch.
    let f = Fixture::new(
        server,
        &format!("rehearse-on-{kind:?}").to_lowercase(),
        kind,
        &Options {
            environment: "    rehearse: true\n",
            ..Options::default()
        },
    );
    f.revise_row();
    let on = f.plan(&[]);
    assert_eq!(code(&on), 0, "{kind:?}: {}", said(&on));
    assert!(f.record()["converged"].is_object(), "{}", f.record());
}

/// With no scratch entry selected the plan carries no record, whatever the
/// switches say, and no scratch variable is read.
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
    for extra in [&[][..], &["--no-rehearse"][..]] {
        let out = f.command("plan", extra, false);
        assert_eq!(code(&out), 0, "{extra:?}: {}", said(&out));
        assert!(f.record().is_null(), "{extra:?}: {}", f.record());
        assert!(
            !stdout(&out).contains("Rehearsal"),
            "{extra:?}: {}",
            said(&out)
        );
        let explained = f.explain("text");
        assert!(
            !stdout(&explained).contains("rehearsal"),
            "{}",
            said(&explained)
        );
    }
}

/// The supplied layout leaves its database empty, and a second run still
/// finds it empty enough to use.
pub fn supplied_database_is_empty_after_each_use(server: &str) {
    let f = Fixture::new(
        server,
        "rehearse-supplied",
        Kind::Supplied,
        &Options::default(),
    );
    f.add_column("true");
    let confined = f.confined.as_ref().unwrap();
    for run in 0..2 {
        let out = f.plan(&[]);
        assert_eq!(code(&out), 0, "run {run}: {}", said(&out));
        let text = stdout(&out);
        assert!(text.contains("it answered the binding questions"), "{text}");
        assert!(
            text.contains("Rehearsal: converged on scratch (server scratch, supplied)"),
            "{text}"
        );
        assert_eq!(f.record()["converged"]["layout"], "supplied");
        for (what, sql) in [
            (
                "relations",
                "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast')",
            ),
            (
                "routines",
                "SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
                 WHERE n.nspname NOT IN ('pg_catalog', 'information_schema')",
            ),
            (
                "schemas",
                "SELECT count(*) FROM pg_namespace WHERE nspname NOT LIKE 'pg\\_%' \
                 AND nspname NOT IN ('information_schema', 'public')",
            ),
        ] {
            assert_eq!(
                scalar(&confined.connection, sql),
                0,
                "run {run}: {what} left in the supplied database"
            );
        }
    }
}

/// An external the base reads is staged from the entry's file and compared
/// with the target, as the resolver compares it: the same one converges, a
/// differing one refuses.
pub fn stages_and_compares_externals(server: &str) {
    const EXTERNAL: &str = "CREATE SCHEMA ext; CREATE FUNCTION ext.g() RETURNS integer \
        LANGUAGE sql IMMUTABLE RETURN 7; CREATE FUNCTION ext.h() RETURNS integer \
        LANGUAGE sql IMMUTABLE RETURN 8";
    let view = "view: app.v\ndefinition: SELECT id, ext.g() AS seven FROM app.t\n";
    let f = Fixture::new(
        server,
        "rehearse-ext",
        Kind::Server,
        &Options {
            externals: Some(EXTERNAL),
            view: Some(view),
            ..Options::default()
        },
    );
    f.revise_row();
    let out = f.plan(&[]);
    assert_eq!(code(&out), 0, "{}", said(&out));
    assert!(f.record()["converged"].is_object(), "{}", said(&out));

    // Negative: the file stages a function the target does not hold.
    on_server(f.target(), "DROP FUNCTION ext.h()");
    let prior = "prior plan 2379";
    std::fs::write(f.artifact(), prior).unwrap();
    let refused = f.plan(&[]);
    assert_eq!(code(&refused), 1, "{}", said(&refused));
    let text = said(&refused);
    assert!(text.contains("externals file was refused"), "{text}");
    assert!(text.contains("ext.h"), "{text}");
    assert_eq!(std::fs::read_to_string(f.artifact()).unwrap(), prior);

    // The switch gets past a scratch that is at fault, and says so.
    let off = f.plan(&["--no-rehearse"]);
    assert_eq!(code(&off), 0, "{}", said(&off));
}

/// Roles and their grants are cluster-level: the scratch is shared, so they
/// are neither created on it nor compared. A plan that grants to a role the
/// scratch lacks still converges, and leaves the scratch cluster as it found it.
pub fn leaves_roles_and_grants_out(server: &str) {
    let role = format!("pbps_reh_r_{}", std::process::id());
    let _roles = ClusterRoles {
        server: server.to_owned(),
        names: vec![role.clone()],
    };
    on_server(server, &format!("DROP ROLE IF EXISTS {role}"));
    on_server(server, &format!("CREATE ROLE {role} NOLOGIN"));
    let f = Fixture::new(
        server,
        "rehearse-roles",
        Kind::Server,
        &Options {
            role: Some(&role),
            ..Options::default()
        },
    );
    std::fs::write(
        f.d.dir.join("schema/role.yml"),
        format!("role: {role}\ngrants:\n  schema::app: [usage]\n  app.t: [select]\n"),
    )
    .unwrap();
    succeeds(f.d.run(&["plan"]));
    f.d.commit();
    let out = f.plan(&[]);
    assert_eq!(code(&out), 0, "{}", said(&out));
    let text = stdout(&out);
    assert!(
        text.contains("grant select on app.t"),
        "the plan grants: {text}"
    );
    assert!(f.record()["converged"].is_object(), "{}", f.record());
    // Negative: nothing of the target's cluster reached the scratch cluster.
    let scratch = f.scratch().unwrap();
    assert_eq!(
        scalar(
            &scratch,
            &format!("SELECT count(*) FROM pg_roles WHERE rolname = '{role}'")
        ),
        0
    );
}

/// A project that declares a managed type cannot be rehearsed yet: the
/// resolver's externals fill does not know types, and a rehearsal stages
/// externals through it. The refusal says so and names the switch, which gets
/// past it.
pub fn managed_types_name_the_switch(server: &str) {
    let f = Fixture::new(
        server,
        "rehearse-types",
        Kind::Server,
        &Options {
            typed: true,
            ..Options::default()
        },
    );
    f.revise_row();
    let prior = "prior plan 2379";
    std::fs::write(f.artifact(), prior).unwrap();
    let refused = f.plan(&[]);
    assert_ne!(code(&refused), 0, "{}", said(&refused));
    let text = said(&refused);
    assert!(
        text.contains("cannot answer for these declarations"),
        "{text}"
    );
    assert!(text.contains("--no-rehearse"), "{text}");
    assert_eq!(std::fs::read_to_string(f.artifact()).unwrap(), prior);
    // The switch gets past it, and the plan says it was not rehearsed.
    let off = f.plan(&["--no-rehearse"]);
    assert_eq!(code(&off), 0, "{}", said(&off));
    assert_eq!(f.record(), serde_json::json!("off"));
}
