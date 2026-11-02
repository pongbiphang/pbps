//! The connected rehearsal of a PostgreSQL plan (#2379, DEC-2379.2): its
//! statements run in order on a reproduction of the target's managed state,
//! on the scratch the selected resolver entry names, before the plan is
//! written. Each case runs on every kind of scratch the entry can be: a
//! server on another cluster (the run-owned layout), a confined account on
//! the target's own cluster (the supplied layout), and a Docker image.

use super::resolver_publication::Confined;
use super::*;
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(90);

#[path = "docker_events2616.rs"]
mod docker_events;
use docker_events::DockerEvents;

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
    /// An account that may create roles, on a cluster of its own: the
    /// supplied layout where the plan's roles can be mapped.
    SuppliedRoles,
    /// An account that may not, on a cluster other runs use: the one placement
    /// where authorization is not rehearsed.
    SuppliedPlain,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Server, Kind::Supplied, Kind::Docker];

    /// The kinds whose authorization is rehearsed: the target's own cluster
    /// (the roles are there), a container (made under their names), and a
    /// cluster of others' where the account can make a mapped role for each.
    pub const FULL: [Kind; 4] = [
        Kind::Server,
        Kind::Supplied,
        Kind::Docker,
        Kind::SuppliedRoles,
    ];

    fn layout(self) -> &'static str {
        match self {
            Kind::Supplied | Kind::SuppliedRoles | Kind::SuppliedPlain => "supplied",
            Kind::Server | Kind::Docker => "run-owned",
        }
    }

    fn phrase(self) -> &'static str {
        match self {
            Kind::Docker => "docker scratch",
            Kind::Server | Kind::Supplied | Kind::SuppliedRoles | Kind::SuppliedPlain => {
                "server scratch"
            }
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
    /// A row-security policy on `app.t` for `role`, with this `USING`.
    pub policy: Option<&'a str>,
    /// A domain `app.short`, which `app.t.note` uses.
    pub domain: bool,
    /// The entry lets its externals file write outside the scratch database
    /// (`outside_writes: allow`), as one that makes a role must.
    pub outside_writes: bool,
    /// `app.t` forces row security under a policy whose `USING` names this role
    /// as text: `pg_has_role(current_user, '<role>', 'member') IS NOT NULL`.
    pub forced_literal: Option<&'a str>,
    /// A volatile external function that a column default of `app.country` calls, so
    /// that the two declared rows the rehearsal's base inserts hold it open
    /// sixteen seconds, long enough to act on a run that is using its roles.
    pub slow: bool,
    /// `app.f()` calls the overload `app.f(integer)`, which it names in
    /// `depends_on:` because overloads are not ordered by the identifier scan.
    pub overload: bool,
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

const CALLER: &str = "function: app.f()\ndepends_on: [app.f(integer)]\n\ndefinition: |-\n  \
    () RETURNS integer LANGUAGE sql IMMUTABLE RETURN app.f(1)\n";

const VIEW: &str = "view: app.v\ndefinition: SELECT id, app.f(id) AS next, amount FROM app.t\n";

/// The external function that holds a statement calling it open for eight
/// seconds a call. An external, because a table default that names a managed
/// function is not ordered after it.
const SLOW_EXTERNAL: &str = "CREATE SCHEMA ext; CREATE FUNCTION ext.slow() RETURNS integer \
    LANGUAGE sql VOLATILE RETURN (SELECT 1 FROM pg_sleep(8))";

fn table_with_policy(role: &str, using: &str) -> String {
    format!(
        "{TABLE}row_security: {{enabled: true, forced: false}}\nrow_policies:\n  mine:\n    \
         command: all\n    restrictive: false\n    to: [{role}]\n    using: {using}\n"
    )
}

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
        if let Some(role) = options.forced_literal {
            // A table with a policy cannot declare rows, so nothing is read back
            // through it; the policy is still created and compared.
            write(
                "app.t.yml",
                &format!(
                    "{TABLE}row_security: {{enabled: true, forced: true}}\nrow_policies:\n  lit:\n    \
                     command: all\n    restrictive: false\n    using: \"pg_has_role(current_user, \
                     '{role}', 'member') IS NOT NULL\"\n"
                ),
            );
        }
        if options.domain {
            write(
                "app.short.type.yml",
                "domain: app.short\ntype: text\nchecks: {nonempty: \"VALUE <> ''\"}\n",
            );
            write(
                "app.t.yml",
                &TABLE.replace("  note: {type: text}\n", "  note: {type: app.short}\n"),
            );
        }
        if options.overload {
            write("app.f%28%29.function.yml", CALLER);
        }
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
            if let Some(using) = options.policy {
                write("app.t.yml", &table_with_policy(role, using));
            }
        }
        if options.slow {
            write(
                "app.country.yml",
                &country("Taiwan").replace(
                    "  label: {type: varchar(40), nullable: false}\n",
                    "  label: {type: varchar(40), nullable: false}\n  \
                     pause: {type: integer, nullable: true, default: \"ext.slow()\"}\n",
                ),
            );
        }
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", &connection]));
        if let Some(sql) = options.target_only {
            on_server(&connection, sql);
        }
        let scratch_server = std::env::var("PBPS_TEST_PG_SCRATCH_DB")
            .expect("PBPS_TEST_PG_SCRATCH_DB is not set; see scripts/live-tests-pg.sh");
        let confined = match kind {
            Kind::Supplied => Some(Confined::new(server)),
            Kind::SuppliedRoles => Some(Confined::creating_roles(&scratch_server)),
            Kind::SuppliedPlain => Some(Confined::new(&scratch_server)),
            Kind::Server | Kind::Docker => None,
        };
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
        let mut externals = String::new();
        if options.externals.is_some() {
            externals.push_str("    externals: externals.sql\n");
        }
        if options.outside_writes {
            externals.push_str("    outside_writes: allow\n");
        }
        let entry = match self.kind {
            Kind::Docker => format!(
                "    kind: docker\n    image: '{}'\n    pull: never\n{externals}",
                image()
            ),
            Kind::Server | Kind::Supplied | Kind::SuppliedRoles | Kind::SuppliedPlain => {
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

    /// A connection to the cluster the scratch is on, where it is a cluster
    /// to ask: a container's is gone with it.
    fn cluster(&self) -> Option<String> {
        match self.kind {
            Kind::Server | Kind::SuppliedRoles | Kind::SuppliedPlain => {
                Some(self.scratch_server.clone())
            }
            Kind::Supplied => Some(self.own.connection().to_owned()),
            Kind::Docker => None,
        }
    }

    /// The roles on the scratch cluster that a rehearsal made.
    fn rehearsal_roles(&self) -> BTreeSet<String> {
        let Some(cluster) = self.cluster() else {
            return BTreeSet::new();
        };
        text_of(
            &cluster,
            "SELECT coalesce(string_agg(rolname, ','), '') FROM pg_roles \
             WHERE rolname LIKE 'pbps\\_rehearsal\\_%'",
        )
        .split(',')
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
    }

    /// The supplied account's own database.
    fn confined_database(&self) -> String {
        self.confined
            .as_ref()
            .unwrap()
            .connection
            .split_whitespace()
            .find_map(|word| word.strip_prefix("dbname="))
            .unwrap()
            .to_owned()
    }

    /// Every role on the scratch cluster, whoever made it.
    fn all_roles(&self) -> BTreeSet<String> {
        text_of(
            &self.cluster().unwrap(),
            "SELECT coalesce(string_agg(rolname, ','), '') FROM pg_roles",
        )
        .split(',')
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
    }

    /// What names `role` in the supplied account's database and in the shared
    /// catalog: ACL entries, policies, default ACLs, and the dependencies that
    /// keep the role from being dropped. All zero once the run has cleaned up.
    fn traces_of(&self, role: &str) -> (i64, i64) {
        let database = with_dbname(&self.cluster().unwrap(), &self.confined_database());
        let acl = scalar(
            &database,
            &format!(
                "SELECT (SELECT count(*) FROM pg_class WHERE relacl::text ~ '{role}') \
                 + (SELECT count(*) FROM pg_namespace WHERE nspacl::text ~ '{role}') \
                 + (SELECT count(*) FROM pg_proc WHERE proacl::text ~ '{role}') \
                 + (SELECT count(*) FROM pg_type WHERE typacl::text ~ '{role}') \
                 + (SELECT count(*) FROM pg_attribute WHERE attacl::text ~ '{role}') \
                 + (SELECT count(*) FROM pg_default_acl WHERE defaclacl::text ~ '{role}') \
                 + (SELECT count(*) FROM pg_policy) \
                 + (SELECT count(*) FROM pg_default_acl)"
            ),
        );
        let shared = scalar(
            &self.cluster().unwrap(),
            &format!(
                "SELECT count(*) FROM pg_shdepend WHERE refobjid = \
                 (SELECT oid FROM pg_roles WHERE rolname = '{role}')"
            ),
        );
        (acl, shared)
    }

    /// Sessions of the run with `token` on the scratch cluster: its keeper,
    /// and whatever it had open in its database. Until the server has seen
    /// them go, a sweep is right to call the run alive.
    fn sessions_of(&self, token: &str) -> i64 {
        let database = match self.kind {
            Kind::Supplied | Kind::SuppliedRoles | Kind::SuppliedPlain => self.confined_database(),
            Kind::Server | Kind::Docker => format!("pbps_scratch_{token}"),
        };
        scalar(
            &self.cluster().unwrap(),
            &format!(
                "SELECT count(*) FROM pg_stat_activity WHERE pid <> pg_backend_pid() \
                 AND (application_name = 'pbps-{token}' OR datname = '{database}')"
            ),
        )
    }

    /// Gives `role` a grant on `app.t` and changes its policy's `USING`: the
    /// plan then carries a grant and a policy, which are what a rehearsal maps
    /// the role for.
    pub fn revise_authorization(&self, role: &str, using: &str) {
        std::fs::write(
            self.d.dir.join("schema/role.yml"),
            format!("role: {role}\ngrants:\n  schema::app: [usage]\n  app.t: [select]\n"),
        )
        .unwrap();
        std::fs::write(
            self.d.dir.join("schema/app.t.yml"),
            table_with_policy(role, using),
        )
        .unwrap();
        succeeds(self.d.run(&["plan"]));
        self.d.commit();
    }

    /// A plan, left running.
    fn start(&self) -> std::process::Child {
        self.built("plan", &[], true)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// [`Self::until`] for a plan left running: one that ends first has said
    /// why on its standard error, and that is what this panics with.
    fn until_running(
        &self,
        run: &mut std::process::Child,
        what: &str,
        ready: impl Fn(&Self) -> bool,
    ) {
        let started = Instant::now();
        while !ready(self) {
            if let Some(status) = run.try_wait().unwrap() {
                let mut said = String::new();
                if let Some(mut stderr) = run.stderr.take() {
                    std::io::Read::read_to_string(&mut stderr, &mut said).unwrap();
                }
                panic!("the plan ended ({status}) before {what}: {said}");
            }
            assert!(started.elapsed() < LIMIT, "gave up waiting for {what}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Waits for `ready` to hold, or panics saying what it waited for.
    fn until(&self, what: &str, ready: impl Fn(&Self) -> bool) {
        let started = Instant::now();
        while !ready(self) {
            assert!(started.elapsed() < LIMIT, "gave up waiting for {what}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The scratch connection a command is given, if its kind has one.
    pub fn scratch(&self) -> Option<String> {
        match self.kind {
            Kind::Server => Some(self.scratch_server.clone()),
            Kind::Supplied | Kind::SuppliedRoles | Kind::SuppliedPlain => {
                Some(self.confined.as_ref().unwrap().connection.clone())
            }
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

    /// Declares the two rows of `app.country` with one label and a unique
    /// constraint over it, which the rows break.
    pub fn make_labels_unique(&self) {
        let table = country("Japan").replace(
            "primary_key: {name: pk_country, columns: [code]}\n",
            "primary_key: {name: pk_country, columns: [code]}\nunique:\n  uq_label: [label]\n",
        );
        std::fs::write(self.d.dir.join("schema/app.country.yml"), table).unwrap();
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
    assert!(
        out.status.success(),
        "docker ps: {}: {}",
        out.status,
        stderr(&out)
    );
    String::from_utf8(out.stdout)
        .expect("docker ps stdout must be readable")
        .lines()
        .map(str::to_owned)
        .collect()
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
    let evidence = (kind == Kind::Docker).then(|| DockerEvents::start(&image()).unwrap());
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
        let made = evidence.unwrap().created_by(pid).unwrap();
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
    let evidence = (kind == Kind::Docker).then(|| DockerEvents::start(&image()).unwrap());
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
        let made = evidence.unwrap().created_by(pid).unwrap();
        assert_eq!(made.len(), 1, "one container for both uses: {made:?}");
        assert!(
            remaining_containers()
                .iter()
                .all(|name| !made.contains(name))
        );
        // Negative: the process is the right one to ask about; with the
        // rehearsal off it makes the one container the resolver needs.
        let evidence = DockerEvents::start(&image()).unwrap();
        let (out, alone) = f.plan_as_process(&["--no-rehearse"]);
        assert_eq!(code(&out), 0, "{}", said(&out));
        assert_eq!(f.record(), serde_json::json!("off"));
        let made = evidence.created_by(alone).unwrap();
        assert_eq!(made.len(), 1, "resolver without rehearsal: {made:?}");
        assert!(
            remaining_containers()
                .iter()
                .all(|name| !made.contains(name))
        );
    }
}

/// A statement the engine rejects at apply is rejected at plan time: the
/// rehearsal meets it, names it, and writes nothing. (This was #2312's
/// ungrouped `IN` default until the emitter grouped it.) Two declared rows
/// that share a label fail the unique constraint the plan adds over it.
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
    assert!(
        text.contains("rejected statement 2 of the plan"),
        "{kind:?}: {text}"
    );
    assert!(
        text.contains("ADD CONSTRAINT \"uq_label\" UNIQUE"),
        "{kind:?}: {text}"
    );
    assert!(
        text.contains("could not create unique index"),
        "{kind:?}: {text}"
    );
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
    assert!(
        said(&applied).contains("would collide under the new unique constraint"),
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

/// A role of this test on the target's cluster, which is dropped with it.
struct TargetRole {
    _guard: ClusterRoles,
    name: String,
}

impl TargetRole {
    fn new(server: &str, what: &str, kind: Kind) -> Self {
        let name = format!(
            "pbps_reh_{what}_{}_{}",
            std::process::id(),
            format!("{kind:?}").to_lowercase()
        );
        on_server(server, &format!("DROP ROLE IF EXISTS {name}"));
        on_server(server, &format!("CREATE ROLE {name} NOLOGIN"));
        Self {
            _guard: ClusterRoles {
                server: server.to_owned(),
                names: vec![name.clone()],
            },
            name,
        }
    }
}

/// A project whose role holds a grant and a policy on the target, and whose
/// next plan changes both.
fn authorized(server: &str, what: &str, kind: Kind, slow: bool) -> (TargetRole, Fixture) {
    let role = TargetRole::new(server, what, kind);
    let f = Fixture::new(
        server,
        &format!("rehearse-{what}-{kind:?}").to_lowercase(),
        kind,
        &Options {
            role: Some(&role.name),
            policy: Some("id > 0"),
            slow,
            externals: slow.then_some(SLOW_EXTERNAL),
            ..Options::default()
        },
    );
    (role, f)
}

/// Authorization is rehearsed, not left out: a plan that grants and changes a
/// policy runs both on roles made for the run, converges, says so, and leaves
/// neither the target's role nor the run's on the scratch cluster (#2379).
pub fn authorization_is_rehearsed(server: &str, kind: Kind) {
    let (role, f) = authorized(server, "ok", kind, false);
    f.revise_authorization(&role.name, "id > 1");
    let before = f.rehearsal_roles();
    let (roles_before, traces_before) = (
        f.cluster().map(|_| f.all_roles()),
        (kind == Kind::Supplied).then(|| f.traces_of(&role.name)),
    );
    let out = f.plan(&[]);
    assert_eq!(code(&out), 0, "{kind:?}: {}", said(&out));
    let text = stdout(&out);
    assert!(
        text.contains("grant select on app.t"),
        "{kind:?}: the plan grants: {text}"
    );
    assert!(
        text.contains(&format!(
            "Rehearsal: converged on scratch ({}, {})",
            kind.phrase(),
            kind.layout()
        )),
        "{kind:?}: {text}"
    );
    assert!(!text.contains("NOT rehearsed"), "{kind:?}: {text}");
    assert_eq!(
        f.record()["converged"]["authorization"],
        "rehearsed",
        "{kind:?}: {}",
        f.record()
    );
    // `explain` and the JSON say the same from the file.
    let explained = f.explain("text");
    assert!(!stdout(&explained).contains("NOT rehearsed"));
    let json: serde_json::Value = serde_json::from_slice(&f.explain("json").stdout).unwrap();
    assert_eq!(
        json["data"]["rehearsal"]["authorization"], "rehearsed",
        "{json}"
    );
    // Nothing of either kind of role stays on the scratch cluster.
    assert!(
        f.rehearsal_roles().is_subset(&before),
        "{kind:?}: {:?} left",
        f.rehearsal_roles().difference(&before).collect::<Vec<_>>()
    );
    if let Some(cluster) = f.cluster() {
        assert_eq!(
            scalar(
                &cluster,
                &format!(
                    "SELECT count(*) FROM pg_roles WHERE rolname = '{}'",
                    role.name
                )
            ),
            // Only the target's own cluster holds the target's role; a scratch
            // on a cluster of its own, or others', never gets its name.
            i64::from(kind == Kind::Supplied),
            "{kind:?}"
        );
    }
    // On the target's own cluster the roles are there and the run makes none: not
    // one role is added or dropped, and what the run granted them is gone.
    if kind == Kind::Supplied {
        assert_eq!(Some(f.all_roles()), roles_before, "{kind:?}: no role made");
        assert_eq!(
            Some(f.traces_of(&role.name)),
            traces_before,
            "{kind:?}: no ACL, policy or dependency names the role afterwards"
        );
    }
    // And the plan applies, as the rehearsal said it would.
    let applied = f.apply("revoke,row-security");
    assert_eq!(code(&applied), 0, "{kind:?}: {}", said(&applied));
    assert_eq!(
        scalar(
            f.target(),
            "SELECT count(*) FROM pg_policies WHERE tablename = 't' AND qual LIKE '%> 1%'"
        ),
        1,
        "{kind:?}"
    );
}

/// A policy whose `USING` the engine rejects is refused at plan time, naming
/// the statement, and leaves no role behind; with the rehearsal off the same
/// plan is written and fails only at apply.
pub fn a_wrong_policy_is_refused(server: &str, kind: Kind) {
    let (role, f) = authorized(server, "bad", kind, false);
    f.revise_authorization(&role.name, "no_such_column > 0");
    let before = f.rehearsal_roles();
    let prior = "prior plan 2379";
    std::fs::write(f.artifact(), prior).unwrap();
    let out = f.plan(&[]);
    assert_eq!(code(&out), 2, "{kind:?}: {}", said(&out));
    let text = said(&out);
    assert!(text.contains("rejected statement"), "{kind:?}: {text}");
    assert!(text.contains("CREATE POLICY"), "{kind:?}: {text}");
    assert!(text.contains("no_such_column"), "{kind:?}: {text}");
    assert!(text.contains("--no-rehearse"), "{kind:?}: {text}");
    assert_eq!(std::fs::read_to_string(f.artifact()).unwrap(), prior);
    assert!(
        f.rehearsal_roles().is_subset(&before),
        "{kind:?}: a refusal left {:?}",
        f.rehearsal_roles().difference(&before).collect::<Vec<_>>()
    );
    // Negative: without the rehearsal the plan is written, and apply is where
    // the engine refuses it.
    let off = f.plan(&["--no-rehearse"]);
    assert_eq!(code(&off), 0, "{kind:?}: {}", said(&off));
    assert_eq!(f.record(), serde_json::json!("off"));
    let applied = f.apply("revoke,row-security");
    assert_ne!(code(&applied), 0, "{kind:?}: {}", said(&applied));
    assert!(
        said(&applied).contains("no_such_column"),
        "{}",
        said(&applied)
    );
}

/// An account that may not create roles gets everything else rehearsed and a
/// record that says authorization was not: in the plan's output, in `explain`,
/// in the JSON and at `apply`. The error a rehearsal of authorization would
/// have found is not found, and is not claimed to be.
pub fn an_account_without_createrole_downgrades_and_says_so(server: &str) {
    let kind = Kind::SuppliedPlain;
    let (role, f) = authorized(server, "down", kind, false);
    // A policy the engine would reject: it is what the downgrade does not see.
    f.revise_authorization(&role.name, "no_such_column > 0");
    let before = f.rehearsal_roles();
    let out = f.plan(&[]);
    assert_eq!(code(&out), 0, "{}", said(&out));
    let text = stdout(&out);
    assert!(
        text.contains("Rehearsal: converged on scratch (server scratch, supplied)"),
        "{text}"
    );
    assert!(text.contains("; authorization NOT rehearsed"), "{text}");
    assert!(text.contains("Authorization was NOT rehearsed"), "{text}");
    assert!(text.contains("may not create roles"), "{text}");
    let record = f.record();
    assert_eq!(
        record["converged"]["authorization"]["not_rehearsed"]["reason"]
            .as_str()
            .map(|reason| reason.contains("may not create roles")),
        Some(true),
        "{record}"
    );
    let explained = f.explain("text");
    assert!(
        stdout(&explained).contains("authorization NOT rehearsed"),
        "{}",
        said(&explained)
    );
    let json: serde_json::Value = serde_json::from_slice(&f.explain("json").stdout).unwrap();
    assert_eq!(
        json["data"]["rehearsal"]["authorization"], "not-rehearsed",
        "{json}"
    );
    let applied = f.apply("revoke,row-security");
    assert!(
        stdout(&applied).contains("authorization NOT rehearsed"),
        "apply shows it: {}",
        said(&applied)
    );
    // The rest was rehearsed, and the cluster is as it was.
    assert!(f.rehearsal_roles().is_subset(&before));
    assert_eq!(
        scalar(
            &f.confined.as_ref().unwrap().connection,
            "SELECT count(*) FROM pg_namespace WHERE nspname = 'pbps_claim'"
        ),
        0,
        "the claim is gone"
    );
    // Negative: the same account on the target's own cluster is not downgraded
    // (the roles are there, so nothing has to be made), and a project that names
    // no role has nothing to skip.
    let (_role, same) = authorized(server, "down3", Kind::Supplied, false);
    same.revise_authorization(&_role.name, "id > 1");
    let full = same.plan(&[]);
    assert_eq!(code(&full), 0, "{}", said(&full));
    assert_eq!(
        same.record()["converged"]["authorization"],
        "rehearsed",
        "{}",
        same.record()
    );
    let plain = Fixture::new(server, "rehearse-down2-supplied", kind, &Options::default());
    plain.revise_row();
    let ok = plain.plan(&[]);
    assert_eq!(code(&ok), 0, "{}", said(&ok));
    assert_eq!(
        plain.record()["converged"]["authorization"],
        "rehearsed",
        "{}",
        plain.record()
    );
}

/// What the next run reports as removed, in its JSON findings.
fn recovered(output: &Output) -> Vec<String> {
    json_output_of(output)["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|finding| finding["id"] == "scratch.recovered")
        .map(|finding| finding["message"].as_str().unwrap().to_owned())
        .collect()
}

fn json_output_of(output: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(output))
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", said(output)))
}

/// The token a rehearsal's roles carry.
fn token_of(roles: &BTreeSet<String>) -> String {
    let tokens: BTreeSet<_> = roles
        .iter()
        .map(|name| name.rsplit('_').next().unwrap().to_owned())
        .collect();
    assert_eq!(tokens.len(), 1, "one run's roles: {roles:?}");
    tokens.into_iter().next().unwrap()
}

/// A rehearsal killed with SIGKILL leaves the roles it made, and the next run
/// removes exactly them: those of a run that is gone, and not those of one that
/// is still running (#2410, #2379).
pub fn a_killed_rehearsal_is_cleaned_up_by_the_next_run(server: &str, kind: Kind) {
    let (role, f) = authorized(server, "kill", kind, true);
    f.revise_authorization(&role.name, "id > 1");
    let before = f.rehearsal_roles();
    let mut dead = f.start();
    f.until_running(&mut dead, "the run's roles", |f| {
        f.rehearsal_roles().difference(&before).next().is_some()
    });
    let left: BTreeSet<_> = f.rehearsal_roles().difference(&before).cloned().collect();
    let token = token_of(&left);
    dead.kill().unwrap();
    dead.wait().unwrap();
    assert!(left.is_subset(&f.rehearsal_roles()), "a kill leaves them");
    // The server notices its client gone.
    f.until("the dead run's session to end", |f| {
        f.sessions_of(&token) == 0
    });
    // The next run removes them before it makes its own.
    let mut next = f.start();
    f.until_running(&mut next, "the dead run's roles to go", |f| {
        left.is_disjoint(&f.rehearsal_roles())
    });
    f.until_running(&mut next, "the next run's roles", |f| {
        f.rehearsal_roles().difference(&before).next().is_some()
    });
    let live: BTreeSet<_> = f.rehearsal_roles().difference(&before).cloned().collect();
    assert!(live.is_disjoint(&left), "{live:?} {left:?}");
    // While it runs, another run leaves its roles alone.
    if kind == Kind::Server {
        let (other_role, other) = authorized(server, "kill2", kind, false);
        other.revise_authorization(&other_role.name, "id > 1");
        let swept = other.plan(&["--format", "json"]);
        assert_eq!(code(&swept), 0, "{}", said(&swept));
        assert!(
            live.is_subset(&f.rehearsal_roles()),
            "a live run's roles stand"
        );
        let live_token = token_of(&live);
        assert!(
            recovered(&swept).iter().all(|m| !m.contains(&live_token)),
            "{:?}",
            recovered(&swept)
        );
    } else {
        // One supplied database, one run at a time: the claim says who has it.
        let refused = f.plan(&[]);
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        assert!(
            said(&refused).contains("still running"),
            "{}",
            said(&refused)
        );
        assert!(
            live.is_subset(&f.rehearsal_roles()),
            "a live run's roles stand"
        );
    }
    let finished = next.wait().unwrap();
    assert!(finished.success(), "the live run was not disturbed");
    assert!(
        f.rehearsal_roles().is_subset(&before),
        "nothing is left: {:?}",
        f.rehearsal_roles().difference(&before).collect::<Vec<_>>()
    );
}

/// A rehearsal on the target's own cluster uses the real role names and makes
/// none, so a kill leaves no role to remove, only what it granted the real one in
/// the supplied database. The next run cleans that by the claim, and afterwards no
/// ACL, policy or default ACL names the role and the shared catalog's
/// dependencies on it are what they were before the run (#2432).
pub fn a_killed_same_cluster_rehearsal_leaves_nothing_naming_the_real_role(server: &str) {
    let kind = Kind::Supplied;
    let (role, f) = authorized(server, "killsame", kind, true);
    f.revise_authorization(&role.name, "id > 1");
    let before = f.traces_of(&role.name);
    let roles = f.all_roles();
    let database = f.confined_database();
    let in_database = |sql: &str| scalar(&with_dbname(&f.cluster().unwrap(), &database), sql);
    let mut dead = f.start();
    // Inside the rehearsal: the base is building the declared rows, and the call
    // that holds it open is sleeping.
    f.until_running(&mut dead, "the rehearsal's rows", |f| {
        scalar(
            &f.cluster().unwrap(),
            &format!(
                "SELECT count(*) FROM pg_stat_activity WHERE datname = '{database}' \
                 AND wait_event = 'PgSleep'"
            ),
        ) > 0
    });
    dead.kill().unwrap();
    dead.wait().unwrap();
    // The run is dead and its claim is what says so.
    assert_eq!(
        in_database("SELECT count(*) FROM pg_namespace WHERE nspname = 'pbps_claim'"),
        1,
        "the dead run left its claim"
    );
    f.until("the dead run's sessions to end", |f| {
        scalar(
            &f.cluster().unwrap(),
            &format!(
                "SELECT count(*) FROM pg_stat_activity WHERE datname = '{database}' \
                 AND pid <> pg_backend_pid()"
            ),
        ) == 0
    });
    // A run makes no role here, so a kill leaves none.
    assert_eq!(f.all_roles(), roles, "no role was made");
    let next = f.plan(&["--format", "json"]);
    assert_eq!(code(&next), 0, "{}", said(&next));
    assert_eq!(
        in_database("SELECT count(*) FROM pg_namespace WHERE nspname = 'pbps_claim'"),
        0,
        "the next run cleaned the claim"
    );
    assert_eq!(
        f.traces_of(&role.name),
        before,
        "no ACL, policy or default ACL names the real role, and its dependencies are as they were"
    );
    assert_eq!(f.all_roles(), roles);
    assert_eq!(f.record()["converged"]["authorization"], "rehearsed");
}

/// A policy whose `USING` names a role as text, on a table that forces row
/// security, converges on every placement, though the text runs as written and
/// on a cluster others use the role is not there under that name. Nothing reads
/// through the policy: a table with policies cannot declare rows, so the rehearsal
/// reads none of it back, and the policy is created and compared as declared.
/// (Run directly, as the table's owner, the same text fails with `role "x" does
/// not exist`: that is what the rehearsal would meet if it read rows through it,
/// and why this case is pinned.)
pub fn a_policy_naming_a_role_in_text_converges_on_every_placement(server: &str, kind: Kind) {
    let role = TargetRole::new(server, "lit", kind);
    let f = Fixture::new(
        server,
        &format!("rehearse-literal-{kind:?}").to_lowercase(),
        kind,
        &Options {
            forced_literal: Some(&role.name),
            ..Options::default()
        },
    );
    f.revise_row();
    let before = f.rehearsal_roles();
    let out = f.plan(&[]);
    assert_eq!(code(&out), 0, "{kind:?}: {}", said(&out));
    let record = f.record();
    assert!(record["converged"].is_object(), "{kind:?}: {record}");
    // The text names a role no declaration or policy recipient does, so even the
    // account that cannot make a mapped role has none to skip.
    assert_eq!(
        record["converged"]["authorization"], "rehearsed",
        "{kind:?}: {record}"
    );
    assert!(
        f.rehearsal_roles().is_subset(&before),
        "{kind:?}: nothing is left"
    );
}

/// A container is the run's own cluster, so the plan's roles are made there under
/// their names, but only those it lacks: the bootstrap superuser, the engine's
/// `pg_*` roles and a role the externals file made are there already, and
/// creating them again would fail a valid plan (#2432).
pub fn a_container_gets_only_the_roles_it_lacks(server: &str) {
    let kind = Kind::Docker;
    let ours = TargetRole::new(server, "skip", kind);
    let made = format!("pbps_reh_ext_{}", std::process::id());
    on_server(server, &format!("DROP ROLE IF EXISTS {made}"));
    on_server(server, &format!("CREATE ROLE {made} NOLOGIN"));
    let _made = ClusterRoles {
        server: server.to_owned(),
        names: vec![made.clone()],
    };
    // The externals file makes `made` in the container (and finds it on the target).
    let external = format!(
        "DO $$ BEGIN IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '{made}') \
         THEN CREATE ROLE {made} NOLOGIN; END IF; END $$"
    );
    let f = Fixture::new(
        server,
        "rehearse-skip-docker",
        kind,
        &Options {
            role: Some(&ours.name),
            policy: Some("id > 0"),
            externals: Some(&external),
            outside_writes: true,
            ..Options::default()
        },
    );
    // A policy for the bootstrap superuser, an engine role, a role the file made
    // and one only the run makes: only the last is the run's to create.
    std::fs::write(
        f.d.dir.join("schema/app.t.yml"),
        format!(
            "{}row_security: {{enabled: true, forced: false}}\nrow_policies:\n  mine:\n    \
             command: all\n    restrictive: false\n    to: [postgres, pg_monitor, {made}, {}]\n    \
             using: id > 1\n",
            TABLE, ours.name
        ),
    )
    .unwrap();
    succeeds(f.d.run(&["plan"]));
    f.d.commit();
    let out = f.plan(&[]);
    assert_eq!(code(&out), 0, "{}", said(&out));
    assert_eq!(
        f.record()["converged"]["authorization"],
        "rehearsed",
        "{}",
        f.record()
    );
}

/// Two plans at once on one scratch cluster map the same role to roles of
/// their own: the names do not collide, and both finish and clean up.
pub fn concurrent_plans_do_not_collide(server: &str) {
    let kind = Kind::Server;
    let (role, one) = authorized(server, "one", kind, true);
    one.revise_authorization(&role.name, "id > 1");
    let (other, two) = authorized(server, "two", kind, true);
    two.revise_authorization(&other.name, "id > 1");
    let before = one.rehearsal_roles();
    let mut first = one.start();
    let mut second = two.start();
    one.until_running(&mut first, "both runs' roles", |f| {
        let now = f.rehearsal_roles();
        let tokens: BTreeSet<_> = now
            .difference(&before)
            .map(|name| name.rsplit('_').next().unwrap().to_owned())
            .collect();
        tokens.len() == 2
    });
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());
    assert!(one.rehearsal_roles().is_subset(&before));
    assert_eq!(one.record()["converged"]["authorization"], "rehearsed");
    assert_eq!(two.record()["converged"]["authorization"], "rehearsed");
}

/// A declared domain is created on the scratch with the rest of the base, and
/// what the engine stored of its check is a respelling, printed and sealed, not
/// a difference that fails the plan.
pub fn a_declared_domain_is_rehearsed(server: &str) {
    let f = Fixture::new(
        server,
        "rehearse-domain",
        Kind::Server,
        &Options {
            domain: true,
            ..Options::default()
        },
    );
    f.revise_row();
    let out = f.plan(&[]);
    assert_eq!(code(&out), 0, "{}", said(&out));
    let text = stdout(&out);
    assert!(text.contains("Rehearsal: converged on scratch"), "{text}");
    let spelt = spellings(&f.record()).join("\n");
    assert!(
        spelt.contains("domain app.short check nonempty: declared `VALUE <> ''`"),
        "{spelt}"
    );
    assert!(text.contains("domain app.short check nonempty"), "{text}");
}

/// A declared enum is created on the scratch with the rest of the base and
/// compared with the declarations: master records a declared type when it is
/// planned and builds it on the scratch, and the resolver's externals fill never
/// sees it (DEC-2407.1), so nothing refuses the project. (It was refused, with the
/// switch named, while the fill did not know types.)
pub fn a_declared_enum_is_rehearsed(server: &str) {
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
    let out = f.plan(&[]);
    assert_eq!(code(&out), 0, "{}", said(&out));
    assert!(
        stdout(&out).contains("Rehearsal: converged on scratch"),
        "{}",
        said(&out)
    );
    assert_eq!(
        f.record()["converged"]["authorization"],
        "rehearsed",
        "{}",
        f.record()
    );
}

/// A revision that drops a kept function's `depends_on:` is rehearsed on a base
/// rebuilt under the annotation the target was written with: the old caller of
/// an overload is created after its callee, and the plan converges.
pub fn a_kept_module_is_rebuilt_under_its_recorded_dependencies(server: &str) {
    let f = Fixture::new(
        server,
        "rehearse-hints",
        Kind::Server,
        &Options {
            overload: true,
            ..Options::default()
        },
    );
    std::fs::write(
        f.d.dir.join("schema/app.f%28%29.function.yml"),
        "function: app.f()\n\ndefinition: |-\n  () RETURNS integer LANGUAGE sql IMMUTABLE RETURN 2\n",
    )
    .unwrap();
    f.d.commit();
    let out = f.plan(&[]);
    assert_eq!(code(&out), 0, "{}", said(&out));
    assert!(
        stdout(&out).contains("Rehearsal: converged on scratch"),
        "{}",
        said(&out)
    );
}
