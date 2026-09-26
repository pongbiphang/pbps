//! The end-to-end flow on PostgreSQL, through the binary.
//!
//! `tests/flow.rs` is the SQL Server suite; this file is the same shape on the
//! other engine, for the path a user actually types: `bootstrap`, `verify`,
//! `plan --db`, `apply`, `state list`, `doctor`. Each connected command reaches
//! the engine through `engine.rs`, and only a run against a real server can
//! say that every arm it routes to answers — the dialect's own live suite
//! (`pbps-pg/tests/live.rs`) proves each function, not that the CLI reaches
//! it (issue #85, DECISIONS 417).
//!
//! Run with `scripts/live-tests-pg.sh`, or by hand:
//! `PBPS_TEST_PG_DB=... cargo test -p pbps-cli --test flow_pg -- --ignored`.
//! Script tests use `PBPS_TEST_PSQL`, the psql inside `PBPS_TEST_PG_CONTAINER`
//! (set by the local script and CI), or local `psql`, in that order.

use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_pbps");

#[path = "support/envelope_archives.rs"]
mod envelope_archives;

#[path = "support/resolver_selection.rs"]
mod resolver_selection;

#[test]
fn resolver_configuration_keeps_offline_commands_offline() {
    resolver_selection::offline("postgres");
}

#[test]
#[ignore = "needs live postgres"]
fn resolver_selection_is_lazy_and_obeys_cli_environment_project_precedence() {
    resolver_selection::connected(&server(), "postgres");
}

#[path = "support/resolver_discovery.rs"]
mod resolver_discovery;

#[path = "support/rename_order_pg.rs"]
mod rename_order_pg;

#[test]
#[ignore = "needs live PostgreSQL; set PBPS_TEST_PG_DB"]
fn doctor_resolver_discovery_is_advisory_and_never_acquires_an_engine() {
    resolver_discovery::check(&server(), "postgres");
}

fn server() -> String {
    std::env::var("PBPS_TEST_PG_DB")
        .expect("PBPS_TEST_PG_DB is not set; see scripts/live-tests-pg.sh")
}

/// Runs one statement against the server under test, through the same
/// connection type the tool uses. Panics on failure: a test that cannot
/// arrange its state has not passed (DECISIONS 198).
fn on_server(connection: &str, sql: &str) {
    if let Err(e) = try_on_server(connection, sql) {
        panic!("{e}");
    }
}

fn try_on_server(connection: &str, sql: &str) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("no runtime: {e}"))?;
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
            .await
            .map_err(|e| format!("cannot reach the server under test: {e}"))?;
        conn.execute(sql)
            .await
            .map_err(|e| format!("`{sql}` failed: {e}"))?;
        Ok(())
    })
}

/// A database of this test's own, dropped when the guard goes out of scope,
/// for the reason `tests/flow.rs` gives: the ledger is per database, and two
/// tests writing one ledger fail each other in an order that depends on the
/// scheduler.
struct OwnDatabase {
    server: String,
    name: String,
    connection: String,
}

impl OwnDatabase {
    fn new(server: &str, slug: &str) -> Self {
        let name = format!("pbps_cli_{slug}_{}", std::process::id());
        on_server(
            server,
            &format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"),
        );
        on_server(server, &format!("CREATE DATABASE \"{name}\""));
        Self {
            server: server.to_owned(),
            connection: with_dbname(server, &name),
            name,
        }
    }

    fn connection(&self) -> &str {
        &self.connection
    }
}

impl Drop for OwnDatabase {
    fn drop(&mut self) {
        // Tolerant: a panic while unwinding aborts the process and takes the
        // rest of the suite with it. `FORCE` because a CLI process that
        // panicked mid-command may still hold its session for a moment.
        let _ = try_on_server(
            &self.server,
            &format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", self.name),
        );
    }
}

/// The libpq keyword form `PBPS_TEST_PG_DB` is written in, with its `dbname`
/// replaced: `key=value` words separated by spaces, and a `dbname` that is
/// already there has to go, or the driver takes whichever it reads last.
fn with_dbname(connection: &str, name: &str) -> String {
    let kept: Vec<&str> = connection
        .split_whitespace()
        .filter(|word| !word.starts_with("dbname="))
        .collect();
    format!("{} dbname={name}", kept.join(" "))
}

struct Demo {
    dir: PathBuf,
    /// A connection string this project reaches through a keyed environment
    /// instead of `--db` (see [`Demo::keyed`]).
    keyed: std::cell::RefCell<Option<String>>,
}

/// The environment [`Demo::keyed`] writes, and the variables it reads.
const KEYED_ENV: &str = "keyed";
const KEYED_URL: &str = "PBPS_FLOW_PG_KEYED_URL";
const KEYED_KEY: &str = "PBPS_FLOW_PG_KEYED_KEY";

impl Demo {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("pbps-flow-pg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("schema")).unwrap();
        std::fs::write(dir.join("pbps.yml"), "dialect: postgres\n").unwrap();
        let d = Self {
            dir,
            keyed: std::cell::RefCell::new(None),
        };
        d.git(&["init", "-q"]);
        d.git(&["config", "user.email", "d@e.f"]);
        d.git(&["config", "user.name", "demo"]);
        d
    }

    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
    }

    fn commit(&self) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", "wip"]);
    }

    fn table(&self, body: &str) {
        std::fs::write(self.dir.join("schema/app.t.yml"), body).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_env(args, &[])
    }

    fn run_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let keyed = self.keyed.borrow().clone();
        let mut args: Vec<&str> = args.to_vec();
        let mut extra: Vec<(&str, String)> = Vec::new();
        if let Some(connection) = &keyed
            && let Some(at) = args
                .windows(2)
                .position(|w| w[0] == "--db" && w[1] == connection.as_str())
        {
            args.splice(at..at + 2, ["--env", KEYED_ENV]);
            extra.push((KEYED_URL, connection.clone()));
            extra.push((KEYED_KEY, fingerprint_key(9)));
        }
        Command::new(BIN)
            .arg("--project")
            .arg(&self.dir)
            .args(&args)
            .envs(env.iter().copied())
            .envs(extra.iter().map(|(k, v)| (*k, v.as_str())))
            .output()
            .unwrap()
    }

    /// Routes every later `--db connection` through an environment with a
    /// fingerprint key, for a test whose database holds a routine a role short
    /// of a superuser can replace: a plan against it pins that routine, and a
    /// bare `--db` target has no key to pin it under (DEC-319.1). Appends to
    /// whatever `pbps.yml` already says.
    fn keyed(&self, connection: &str) {
        let path = self.dir.join("pbps.yml");
        let mut config = std::fs::read_to_string(&path).unwrap();
        assert!(
            !config.contains("environments:"),
            "a keyed demo writes its own environments"
        );
        config.push_str(&format!(
            "environments:\n  {KEYED_ENV}:\n    url_env: {KEYED_URL}\n    \
             fingerprint_key_env: {KEYED_KEY}\n"
        ));
        std::fs::write(&path, config).unwrap();
        *self.keyed.borrow_mut() = Some(connection.to_owned());
    }
}

impl Drop for Demo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

fn plan_checksum(path: &std::path::Path) -> String {
    let raw = std::fs::read_to_string(path).unwrap();
    serde_json::from_str::<pbps_model::SavedPlan>(&raw)
        .unwrap()
        .checksum()
}

const ONE_COLUMN: &str = "table: app.t\ncolumns:\n  id: {type: bigint, nullable: false}\n\
                          primary_key: {name: pk_t, columns: [id]}\n";
const TWO_COLUMNS: &str = "table: app.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  \
                           label: {type: varchar(50)}\n\
                           primary_key: {name: pk_t, columns: [id]}\n";

fn succeeds(o: Output) -> Output {
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    o
}

fn json_output(o: Output) -> serde_json::Value {
    let text = stdout(&o);
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}\n{}", stderr(&o)))
}

fn scalar(connection: &str, sql: &str) -> i64 {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut c = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
                .await
                .unwrap();
            c.query(sql).await.unwrap()[0]
                .try_get_at::<i64>(0)
                .unwrap()
                .unwrap()
        })
}

/// One text cell, for the catalog questions a test asks in passing.
fn text_of(connection: &str, sql: &str) -> String {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut c = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
                .await
                .unwrap();
            c.query(sql).await.unwrap()[0]
                .try_get_at::<&str>(0)
                .unwrap()
                .unwrap()
                .to_owned()
        })
}

fn latest_snapshot(connection: &str) -> pbps_model::StateSnapshot {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut c = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
                .await
                .unwrap();
            pbps_pg::state::latest(&mut c)
                .await
                .unwrap()
                .unwrap()
                .snapshot
        })
}

fn connected_artifact(d: &Demo, connection: &str, staged: bool) -> PathBuf {
    succeeds(d.run(&["plan"]));
    d.commit();
    let path = d.dir.join("invariant-plan.json");
    let mut args = vec!["plan", "--db", connection, "--out", path.to_str().unwrap()];
    if staged {
        args.push("--staged");
    }
    succeeds(d.run(&args));
    path
}

fn approved_apply(d: &Demo, connection: &str, plan: &std::path::Path, extra: &[&str]) -> Output {
    let checksum = plan_checksum(plan);
    let mut args = vec![
        "apply",
        "--db",
        connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &checksum,
    ];
    args.extend_from_slice(extra);
    d.run(&args)
}

/// Use psql's actual statement scanner: sending the whole file in one simple
/// query would parse the DDL before the leading SETs take effect (decision 458).
fn psql_script(db: &OwnDatabase, script: &str) -> Output {
    use std::io::Write;
    use std::process::Stdio;

    let options = "-c standard_conforming_strings=off -c check_function_bodies=off \
        -c DateStyle=German,DMY -c TimeZone=America/New_York -c IntervalStyle=sql_standard \
        -c timezone_abbreviations=Australia -c transform_null_equals=on \
        -c bytea_output=escape -c extra_float_digits=-3";
    let mut command = if let Some(psql) = std::env::var_os("PBPS_TEST_PSQL") {
        let mut command = Command::new(psql);
        command
            .arg("--dbname")
            .arg(db.connection())
            .env("PGOPTIONS", options);
        command
    } else if let Some(container) = std::env::var_os("PBPS_TEST_PG_CONTAINER") {
        let mut command = Command::new("docker");
        command.args(["exec", "-i", "-e", &format!("PGOPTIONS={options}")]);
        command
            .arg(container)
            .args(["psql", "-U", "postgres", "-d", &db.name]);
        command
    } else {
        let mut command = Command::new("psql");
        command
            .arg("--dbname")
            .arg(db.connection())
            .env("PGOPTIONS", options);
        command
    };
    let mut child = command
        .args([
            "-X",
            "-Atq",
            "--set=ON_ERROR_STOP=1",
            "--set=VERBOSITY=verbose",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("script regression needs psql or PBPS_TEST_PG_CONTAINER");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
#[ignore = "needs a live PostgreSQL and psql; see scripts/live-tests-pg.sh"]
fn every_sql_output_pins_the_parser_before_the_generated_definitions() {
    let planning = OwnDatabase::new(&server(), "script_planning");
    on_server(planning.connection(), "CREATE SCHEMA app");
    let empty = Demo::new("script-empty");
    succeeds(empty.run(&["plan"]));
    empty.commit();
    succeeds(empty.run(&["bootstrap", "--db", planning.connection()]));

    let d = Demo::new("script-pins");
    d.table(
        r#"table: app.t
columns:
  id: {type: integer, nullable: false}
  d: {type: date}
  label: {type: text}
primary_key: [id]
checks:
  date_pin: "d >= '01/02/2026'"
  string_pin: "label <> 'a\\n'"
"#,
    );
    let offline = d.dir.join("offline.sql");
    let bootstrap = d.dir.join("bootstrap.sql");
    let connected = d.dir.join("connected.sql");
    succeeds(d.run(&["plan", "--sql", offline.to_str().unwrap()]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--sql", bootstrap.to_str().unwrap()]));
    succeeds(d.run(&[
        "plan",
        "--db",
        planning.connection(),
        "--sql",
        connected.to_str().unwrap(),
    ]));

    for (name, path) in [
        ("offline", offline),
        ("bootstrap", bootstrap),
        ("connected", connected),
    ] {
        let db = OwnDatabase::new(&server(), &format!("script_{name}"));
        on_server(db.connection(), "CREATE SCHEMA app");
        let generated = std::fs::read_to_string(path).unwrap();
        let script = format!(
            r#"
SELECT 'initial=' || current_setting('standard_conforming_strings') || ',' || current_setting('DateStyle');
{generated}
SELECT 'pins=' || current_setting('standard_conforming_strings') || ',' || current_setting('check_function_bodies') || ',' || current_setting('DateStyle') || ',' || current_setting('TimeZone') || ',' || current_setting('IntervalStyle') || ',' || current_setting('timezone_abbreviations') || ',' || current_setting('transform_null_equals') || ',' || current_setting('bytea_output') || ',' || current_setting('extra_float_digits');
SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid='app.t'::regclass AND conname IN ('date_pin', 'string_pin') ORDER BY conname;
"#
        );
        let out = succeeds(psql_script(&db, &script));
        let output = stdout(&out);
        assert!(
            output.contains("initial=off,German, DMY"),
            "{name}: {output}"
        );
        assert!(
            output.contains("pins=on,on,ISO, MDY,UTC,postgres,Default,off,hex,1"),
            "{name}: {output}"
        );
        assert!(output.contains("'2026-01-02'::date"), "{name}: {output}");
        assert!(output.contains(r"'a\n'::text"), "{name}: {output}");

        // A spelling accepted only with the operator's old string parser must
        // fail, rather than reaching the database under a different meaning.
        on_server(db.connection(), "DROP TABLE app.t");
        let invalid = generated.replace(r"'a\n'", r"'it\'s  here'");
        assert_ne!(
            invalid, generated,
            "the negative case must change a literal"
        );
        let refused = psql_script(&db, &invalid);
        assert_eq!(
            code(&refused),
            3,
            "{name}: {}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(
            stderr(&refused).contains("42601"),
            "{name}: {}",
            stderr(&refused)
        );
        assert_eq!(
            scalar(
                db.connection(),
                "SELECT count(*) FROM pg_constraint WHERE conrelid=to_regclass('app.t') AND conname='string_pin'"
            ),
            0
        );
    }
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn staged_and_resumed_writes_pin_the_session_before_the_next_statement() {
    for resume in [false, true] {
        let slug = if resume { "pins_resume" } else { "pins_staged" };
        let own = OwnDatabase::new(&server(), slug);
        let connection = own.connection();
        let base = "table: app.t\ncolumns:\n  id: {type: integer, nullable: false}\n  d: {type: date}\n  label: {type: text}\nprimary_key: [id]\n";
        let d = bootstrapped_demo(connection, slug, ONE_COLUMN);
        // Database-local defaults affect fresh deployment connections without
        // changing the shared test login or another test's session.
        on_server(
            connection,
            &format!(
                "ALTER DATABASE {} SET standard_conforming_strings = off; ALTER DATABASE {} SET DateStyle = 'German, DMY'",
                own.name, own.name
            ),
        );
        std::fs::write(
            d.dir.join("schema/app.u.yml"),
            format!(
                "{}checks:\n  b_date: \"d >= '01/02/2026'\"\n  c_string: \"label <> 'a\\\\n'\"\n",
                base.replace("table: app.t", "table: app.u")
            ),
        )
        .unwrap();
        let plan = connected_artifact(&d, connection, true);
        if resume {
            on_server(
                connection,
                r#"
CREATE SCHEMA witness;
CREATE FUNCTION witness.stop_after_first() RETURNS event_trigger LANGUAGE plpgsql AS $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_constraint WHERE conrelid='app.u'::regclass AND conname='b_date') THEN
    RAISE EXCEPTION 'stop before the date constraint commits';
  END IF;
END $$;
CREATE EVENT TRIGGER stop_after_first ON ddl_command_end WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION witness.stop_after_first();
"#,
            );
            let stopped = approved_apply(&d, connection, &plan, &["--staged"]);
            assert_eq!(
                code(&stopped),
                1,
                "{}{}",
                stdout(&stopped),
                stderr(&stopped)
            );
            let checkpoint = latest_snapshot(connection);
            assert_eq!(checkpoint.staged.as_ref().unwrap().completed, 1);
            on_server(connection, "DROP EVENT TRIGGER stop_after_first");
        }
        let flags: &[&str] = if resume {
            &["--staged", "--resume"]
        } else {
            &["--staged"]
        };
        succeeds(approved_apply(&d, connection, &plan, flags));
        on_server(
            connection,
            "INSERT INTO app.u (id, d, label) VALUES (1, DATE '2026-01-15', 'present')",
        );
        assert!(
            try_on_server(
                connection,
                "INSERT INTO app.u (id, d, label) VALUES (2, DATE '2026-01-01', 'present')"
            )
            .is_err()
        );
        assert!(try_on_server(connection, "INSERT INTO app.u (id, d, label) VALUES (3, DATE '2026-01-15', 'a' || chr(92) || 'n')").is_err());
        on_server(
            connection,
            "INSERT INTO app.u (id, d, label) VALUES (4, DATE '2026-01-15', 'a' || chr(10))",
        );
        let snapshot = latest_snapshot(connection);
        assert!(snapshot.staged.is_none());
        assert_eq!(snapshot.kind, pbps_model::StateKind::Apply);
        succeeds(d.run(&["verify", "--db", connection]));
    }
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_staged_create_never_closes_over_a_replaced_pending_part() {
    for index in [false, true] {
        let slug = if index {
            "pending_index"
        } else {
            "pending_unique"
        };
        let own = OwnDatabase::new(&server(), slug);
        let connection = own.connection();
        let d = bootstrapped_demo(connection, slug, ONE_COLUMN);
        let part = if index {
            "indexes:\n  ix: {columns: [id]}\n"
        } else {
            "unique:\n  uq: [id]\n"
        };
        std::fs::write(d.dir.join("schema/app.u.yml"), format!("table: app.u\ncolumns:\n  id: {{type: integer}}\n  other: {{type: integer}}\n{part}")).unwrap();
        let plan = connected_artifact(&d, connection, true);
        let (tag, present, replace) = if index {
            (
                "CREATE INDEX",
                "to_regclass('app.ix') IS NOT NULL",
                "DROP INDEX app.ix; CREATE INDEX ix ON app.u(other);",
            )
        } else {
            (
                "ALTER TABLE",
                "EXISTS (SELECT 1 FROM pg_constraint WHERE conrelid=to_regclass('app.u') AND conname='uq')",
                "ALTER TABLE app.u DROP CONSTRAINT uq; ALTER TABLE app.u ADD CONSTRAINT uq UNIQUE(other);",
            )
        };
        on_server(
            connection,
            &format!(
                r#"
CREATE SCHEMA witness;
CREATE TABLE witness.fired (id integer PRIMARY KEY);
CREATE FUNCTION witness.replace_part() RETURNS event_trigger LANGUAGE plpgsql AS $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM witness.fired) AND {present} THEN
    INSERT INTO witness.fired VALUES (1);
    {replace}
  END IF;
END $$;
CREATE EVENT TRIGGER replace_part ON ddl_command_end WHEN TAG IN ('{tag}') EXECUTE FUNCTION witness.replace_part();
"#
            ),
        );
        let refused = approved_apply(&d, connection, &plan, &["--staged"]);
        assert_eq!(
            code(&refused),
            1,
            "a replaced pending part was accepted: {}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(
            stderr(&refused).contains("not the one this plan's `CREATE TABLE` declares"),
            "{}",
            stderr(&refused)
        );
        assert_eq!(scalar(connection, "SELECT count(*) FROM witness.fired"), 1);
        let checkpoint = latest_snapshot(connection);
        assert!(
            checkpoint.staged.is_some(),
            "the bad definition must not close as an apply"
        );
        assert_ne!(checkpoint.kind, pbps_model::StateKind::Apply);
        let recorded = &checkpoint.schema.tables[&"app.u".parse().unwrap()];
        if index {
            assert_eq!(recorded.indexes["ix"].columns[0].name, "other");
        } else {
            assert_eq!(recorded.unique["uq"].columns, ["other"]);
        }
        // An unchanged bad checkpoint is still not the CREATE's declaration.
        let still_bad = approved_apply(&d, connection, &plan, &["--staged", "--resume"]);
        assert_eq!(
            code(&still_bad),
            1,
            "{}{}",
            stdout(&still_bad),
            stderr(&still_bad)
        );
        assert!(latest_snapshot(connection).staged.is_some());
        let clean_slug = format!("{slug}_clean");
        let clean = OwnDatabase::new(&server(), &clean_slug);
        let control = bootstrapped_demo(clean.connection(), &clean_slug, ONE_COLUMN);
        std::fs::write(
            control.dir.join("schema/app.u.yml"),
            std::fs::read(d.dir.join("schema/app.u.yml")).unwrap(),
        )
        .unwrap();
        let clean_plan = connected_artifact(&control, clean.connection(), true);
        succeeds(approved_apply(
            &control,
            clean.connection(),
            &clean_plan,
            &["--staged"],
        ));
        succeeds(control.run(&["verify", "--db", clean.connection()]));
    }
}

fn bootstrapped_demo(connection: &str, slug: &str, table: &str) -> Demo {
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new(slug);
    d.table(table);
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    d
}

#[test]
#[ignore = "needs live PostgreSQL"]
fn narrowing_projection_refuses_constraint_failures_before_the_first_statement() {
    for fk in [false, true] {
        let slug = if fk { "projection424" } else { "projection281" };
        let own = OwnDatabase::new(&server(), slug);
        let connection = own.connection();
        on_server(connection, "CREATE SCHEMA app");
        let d = Demo::new(slug);
        let (from, to) = if fk {
            ("numeric(20,0)", "integer")
        } else {
            ("numeric(5,2)", "numeric(5,1)")
        };
        let before = format!(
            "table: app.t\ncolumns:\n  id: {{type: integer, nullable: false}}\n  first: {{type: integer}}\n  v: {{type: '{from}'}}\nprimary_key: {{name: pk_t, columns: [id]}}\n"
        );
        d.table(&before);
        if fk {
            std::fs::write(d.dir.join("schema/app.parent.yml"), "table: app.parent\ncolumns:\n  v: {type: integer, nullable: false}\nprimary_key: {name: pk_parent, columns: [v]}\n").unwrap();
        }
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", connection]));
        if fk {
            on_server(connection, "INSERT INTO app.parent VALUES (1)");
        }
        on_server(
            connection,
            &format!(
                "INSERT INTO app.t VALUES (1,NULL,{})",
                if fk { "1" } else { "1.04" }
            ),
        );
        let added = if fk {
            "foreign_keys:\n  fk_projection:\n    columns: [v]\n    references: app.parent(v)\n"
        } else {
            "unique:\n  uq_projection: [v]\n"
        };
        d.table(&format!(
            "{}{added}",
            before.replace(from, to).replace(
                "first: {type: integer}",
                "preflight_marker: {type: integer}\n  first: {type: integer}"
            )
        ));
        let artifact = connected_artifact(&d, connection, false);
        let saved: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(&artifact).unwrap()).unwrap();
        let addition = saved
            .changes
            .changes
            .iter()
            .position(|p| matches!(&p.change, pbps_model::Change::AddColumn { .. }))
            .unwrap();
        let retype = saved
            .changes
            .changes
            .iter()
            .position(|p| matches!(&p.change, pbps_model::Change::AlterColumnType { .. }))
            .unwrap();
        assert!(
            addition < retype,
            "the benign statement really precedes the retype"
        );
        on_server(
            connection,
            &format!(
                "INSERT INTO app.t VALUES (2,NULL,{})",
                if fk { "2" } else { "1.00" }
            ),
        );
        // Sequence calls survive rollback: zero proves preflight stopped DDL,
        // and the successful retry below proves this witness was active.
        on_server(
            connection,
            r#"
            CREATE SCHEMA witness;
            CREATE SEQUENCE witness.executed;
            CREATE FUNCTION witness.count_ddl() RETURNS event_trigger LANGUAGE plpgsql AS $$
            BEGIN
              IF EXISTS (SELECT FROM pg_event_trigger_ddl_commands() WHERE objid = 'app.t'::regclass) THEN
                PERFORM nextval('witness.executed');
              END IF;
            END $$;
            CREATE EVENT TRIGGER count_projection_ddl ON ddl_command_end WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION witness.count_ddl();
        "#,
        );
        let expected = if fk {
            "1 rows with no matching parent for the new foreign key fk_projection"
        } else {
            "2 rows that would collide under the new unique constraint uq_projection"
        };
        // Planning records the approved change; data probes run at apply.
        succeeds(d.run(&["plan", "--db", connection]));
        let refused = approved_apply(
            &d,
            connection,
            &artifact,
            &["--allow", "narrowing,constraint"],
        );
        assert_eq!(
            code(&refused),
            1,
            "{}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(
            stderr(&refused).contains(expected)
                && stderr(&refused).contains("nothing has been changed"),
            "{}",
            stderr(&refused)
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM witness.executed"
            ),
            0
        );
        on_server(connection, "DELETE FROM app.t WHERE id = 2");
        succeeds(approved_apply(
            &d,
            connection,
            &artifact,
            &["--allow", "narrowing,constraint"],
        ));
        succeeds(d.run(&["verify", "--db", connection]));
        assert!(
            scalar(
                connection,
                "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM witness.executed"
            ) > 0
        );
    }
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn connected_cost_distinguishes_rewrites_scans_unknowns_and_risk() {
    let own = OwnDatabase::new(&server(), "cost");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("cost");
    let before = "table: app.t\ncolumns:\n  i: {type: integer}\n  w: {type: varchar(10)}\n  j: {type: integer}\n  n: {type: integer}\nindexes:\n  ix_j: {columns: [j]}\nchecks:\n  ck_n: n > 0\n";
    d.table(before);
    let other = d.dir.join("schema/u.yml");
    std::fs::write(&other, "table: app.u\ncolumns:\n  v: {type: integer}\n").unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    on_server(
        connection,
        "INSERT INTO app.t SELECT v, 'ok', v, v FROM generate_series(1,3) AS v; ANALYZE app.t",
    );
    d.table(
        &before
            .replace("i: {type: integer}", "i: {type: bigint}")
            .replace("varchar(10)", "varchar(20)")
            .replace("j: {type: integer}", "j: {type: bigint}")
            .replace("n: {type: integer}", "n: {type: integer, nullable: false}"),
    );
    std::fs::write(&other, "table: app.u\ncolumns:\n  v: {type: bigint}\n").unwrap();
    std::fs::write(
        d.dir.join("schema/new.yml"),
        "table: app.new_table\ncolumns:\n  id: {type: integer}\n",
    )
    .unwrap();
    let offline = succeeds(d.run(&["plan", "--format", "json"]));
    let offline: serde_json::Value = serde_json::from_str(&stdout(&offline)).unwrap();
    assert!(offline["data"].get("cost").is_none());
    d.commit();
    let artifact = d.dir.join("deployment.json");
    let out = succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--format",
        "json",
        "--out",
        artifact.to_str().unwrap(),
    ]));
    let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let schema: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/envelope.schema.json"),
        )
        .unwrap(),
    )
    .unwrap();
    jsonschema::validator_for(&schema)
        .unwrap()
        .validate(&report)
        .unwrap();
    envelope_archives::assert_accepted_by_archives(&report, "connected plan");
    let cost = &report["data"]["cost"];
    assert_eq!(cost["engine"], "postgres");
    assert_eq!(cost["status"], "available");
    let changes = cost["changes"].as_array().unwrap();
    let saved_text = std::fs::read_to_string(&artifact).unwrap();
    let saved: pbps_model::SavedPlan = serde_json::from_str(&saved_text).unwrap();
    let saved_json: serde_json::Value = serde_json::from_str(&saved_text).unwrap();
    assert!(saved_json.get("cost").is_none());
    assert_eq!(changes.len(), saved.changes.changes.len());
    let mut checked = 0;
    for (index, p) in saved.changes.changes.iter().enumerate() {
        let c = &changes[index];
        assert_eq!(c["change_index"], index);
        match &p.change {
            pbps_model::Change::CreateTable { .. } => {
                assert_eq!(c["status"], "unavailable");
                assert!(c["reason"].as_str().unwrap().contains("no measurement"));
                checked += 1;
            }
            pbps_model::Change::AlterColumnType { column, .. } => {
                assert_eq!(c["lock"], "AccessExclusiveLock");
                assert_eq!(c["blocks"], "reads and writes");
                match column.name.as_str() {
                    "i" => {
                        assert_eq!(c["rewrite"]["value"], "yes");
                        assert_eq!(c["reads"]["value"], "every_row");
                    }
                    "w" => {
                        assert_eq!(c["rewrite"]["value"], "no");
                        assert_eq!(c["reads"]["value"], "nothing");
                    }
                    "j" => {
                        assert_eq!(c["rewrite"]["value"], "unknown");
                        assert!(c["rewrite"]["reason"].as_str().unwrap().contains("index"));
                    }
                    "v" => assert_eq!(c["rows"]["status"], "never_analyzed"),
                    name => panic!("unexpected column {name}"),
                }
                if column.name != "v" {
                    assert_eq!(c["rows"]["status"], "estimated");
                    assert_eq!(c["rows"]["count"], 3);
                }
                checked += 1;
            }
            pbps_model::Change::AlterColumnNullability { .. } => {
                assert_eq!(c["rewrite"]["value"], "no");
                assert_eq!(c["reads"]["value"], "unknown");
                assert!(c["reads"]["reason"].as_str().unwrap().contains("ck_n"));
                checked += 1;
            }
            change @ pbps_model::Change::DropTable { .. }
            | change @ pbps_model::Change::RenameTable { .. }
            | change @ pbps_model::Change::AddColumn { .. }
            | change @ pbps_model::Change::DropColumn { .. }
            | change @ pbps_model::Change::RenameColumn { .. }
            | change @ pbps_model::Change::AlterColumnDefault { .. }
            | change @ pbps_model::Change::SetColumnDeprecated { .. }
            | change @ pbps_model::Change::SetPrimaryKey { .. }
            | change @ pbps_model::Change::AddUnique { .. }
            | change @ pbps_model::Change::DropUnique { .. }
            | change @ pbps_model::Change::AddForeignKey { .. }
            | change @ pbps_model::Change::DropForeignKey { .. }
            | change @ pbps_model::Change::AddCheck { .. }
            | change @ pbps_model::Change::DropCheck { .. }
            | change @ pbps_model::Change::AddIndex { .. }
            | change @ pbps_model::Change::DropIndex { .. }
            | change @ pbps_model::Change::InsertRow { .. }
            | change @ pbps_model::Change::UpdateRow { .. }
            | change @ pbps_model::Change::DeleteRow { .. }
            | change @ pbps_model::Change::SetDataMode { .. }
            | change @ pbps_model::Change::CreateModule { .. }
            | change @ pbps_model::Change::AlterModule { .. }
            | change @ pbps_model::Change::DropModule { .. }
            | change @ pbps_model::Change::CreateRole { .. }
            | change @ pbps_model::Change::DropRole { .. }
            | change @ pbps_model::Change::RenameRole { .. }
            | change @ pbps_model::Change::Grant { .. }
            | change @ pbps_model::Change::Revoke { .. }
            | change @ pbps_model::Change::PublicExecution { .. } => {
                panic!("unexpected change {change:?}")
            }
        }
    }
    assert_eq!(checked, 6);
    let human = stdout(&succeeds(d.run(&["plan", "--db", connection])));
    for text in [
        "Operational cost estimate",
        "Rewrite: yes",
        "Rewrite: no",
        "unknown",
        "approximately 3",
        "never analyzed",
        "ck_n",
        "AccessExclusiveLock",
        "no measurement",
    ] {
        assert!(human.contains(text), "missing {text}: {human}");
    }
    let risks: Vec<_> = saved.changes.risks().iter().map(|r| r.as_str()).collect();
    assert_eq!(report["data"]["risks"], serde_json::json!(risks));
    // Tightening nullability still requires its ordinary approval; cost never
    // grants it. The widening-only changes keep their existing risk classes.
    let applied = apply_plan(&d, connection, &artifact, false);
    assert_ne!(
        code(&applied),
        0,
        "{}{}",
        stdout(&applied),
        stderr(&applied)
    );
    assert!(stderr(&applied).contains("--allow"), "{}", stderr(&applied));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn connected_cost_keeps_catalog_identity_through_column_and_table_renames() {
    use pbps_model::Change;
    for move_table in [false, true] {
        let slug = if move_table {
            "cost_renamed_table347"
        } else {
            "cost_renamed_column347"
        };
        let own = OwnDatabase::new(&server(), slug);
        let connection = own.connection();
        let before = "table: app.t\ncolumns:\n  v: {type: integer}\n  n: {type: integer}\n  plain: {type: integer}\nindexes:\n  ix_v: {columns: [v]}\nchecks:\n  ck_n: n > 0\n";
        let d = bootstrapped_demo(connection, slug, before);
        on_server(
            connection,
            "INSERT INTO app.t SELECT g,g,g FROM generate_series(1,20) g; ANALYZE app.t",
        );
        let after = before
            .replace(
                "  v: {type: integer}",
                "  amount: {type: bigint, renamed_from: v}",
            )
            .replace(
                "  n: {type: integer}",
                "  required: {type: integer, nullable: false, renamed_from: n}",
            )
            .replace("plain: {type: integer}", "plain: {type: bigint}")
            .replace("columns: [v]", "columns: [amount]")
            .replace("ck_n: n > 0", "ck_n: required > 0");
        let after = if move_table {
            after.replace("table: app.t", "table: app.renamed\nrenamed_from: app.t")
        } else {
            after
        };
        d.table(&after);
        succeeds(d.run(&["plan"]));
        d.commit();
        let path = d.dir.join("rename-cost.json");
        let out = succeeds(d.run(&[
            "plan",
            "--db",
            connection,
            "--format",
            "json",
            "--out",
            path.to_str().unwrap(),
        ]));
        let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
        let saved_text = std::fs::read_to_string(&path).unwrap();
        let saved: pbps_model::SavedPlan = serde_json::from_str(&saved_text).unwrap();
        let saved_json: serde_json::Value = serde_json::from_str(&saved_text).unwrap();
        assert!(saved_json.get("cost").is_none());
        let costs = report["data"]["cost"]["changes"].as_array().unwrap();
        assert_eq!(costs.len(), saved.changes.changes.len());
        let mut verified = 0;
        for (i, p) in saved.changes.changes.iter().enumerate() {
            let c = &costs[i];
            assert_eq!(c["change_index"], i);
            if let Change::AlterColumnType { column, .. } = &p.change
                && column.name == "amount"
            {
                assert_eq!(c["rewrite"]["value"], "unknown", "{report:#}");
                assert!(c["rewrite"]["reason"].as_str().unwrap().contains("index"));
                assert_eq!(c["rows"]["count"], 20);
                verified += 1;
            }
            if let Change::AlterColumnNullability { column, .. } = &p.change
                && column.name == "required"
            {
                // The renamed CHECK is replaced after tightening. Its old
                // catalog definition is gone before the statement (479).
                assert_eq!(c["reads"]["value"], "every_row", "{report:#}");
                assert_eq!(c["rewrite"]["value"], "no");
                verified += 1;
            }
            if let Change::AlterColumnType { column, .. } = &p.change
                && column.name == "plain"
            {
                assert_eq!(c["rewrite"]["value"], "yes");
                assert_eq!(c["reads"]["value"], "every_row");
                verified += 1;
            }
        }
        assert_eq!(verified, 3);
        let human = stdout(&succeeds(d.run(&["plan", "--db", connection])));
        let cost_text = human.split_once("Operational cost estimate").unwrap().1;
        for text in [
            "index is built over the column",
            "approximately 20",
            "amount",
            "required",
        ] {
            assert!(cost_text.contains(text), "{cost_text}");
        }
        let risks: Vec<_> = saved.changes.risks().iter().map(|r| r.as_str()).collect();
        assert_eq!(report["data"]["risks"], serde_json::json!(risks));
        on_server(
            connection,
            "INSERT INTO app.t VALUES (21,21,21); ANALYZE app.t",
        );
        let again = d.dir.join("rename-cost-again.json");
        let second = succeeds(d.run(&[
            "plan",
            "--db",
            connection,
            "--format",
            "json",
            "--out",
            again.to_str().unwrap(),
        ]));
        let second: serde_json::Value = serde_json::from_str(&stdout(&second)).unwrap();
        assert!(
            second["data"]["cost"]["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["rows"]["count"] == 21)
        );
        let mut next: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(&again).unwrap()).unwrap();
        next.created_at = saved.created_at.clone();
        assert_eq!(
            next.checksum(),
            saved.checksum(),
            "changed advisory statistics cannot alter the saved plan apart from its timestamp"
        );
        let denied = apply_plan(&d, connection, &path, false);
        assert_ne!(code(&denied), 0);
        assert!(stderr(&denied).contains("--allow"));
    }
}

fn apply_plan(d: &Demo, connection: &str, plan: &std::path::Path, staged: bool) -> Output {
    let checksum = plan_checksum(plan);
    let mut args = vec![
        "apply",
        "--db",
        connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &checksum,
    ];
    if staged {
        args.push("--staged");
    }
    d.run(&args)
}

fn refuses_connected_check_json(d: &Demo, connection: &str, name: &str, detail: &str) {
    let out = d.run(&["plan", "--db", connection, "--format", "json"]);
    assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
    let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(report["result"], "unanswerable", "{report}");
    assert!(
        report["findings"].as_array().unwrap().iter().any(|f| {
            let message = f["message"].as_str().unwrap_or_default();
            message.contains(name) && message.contains(detail)
        }),
        "{report}"
    );
}

fn passed_connected_check_json(d: &Demo, connection: &str, name: &str) -> serde_json::Value {
    let out = succeeds(d.run(&["plan", "--db", connection, "--format", "json"]));
    let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let check = report["data"]["connected_checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("missing {name}: {report}"));
    assert_eq!(check["status"], "passed", "{report}");
    assert_eq!(check["engine"], "PostgreSQL", "{report}");
    check.clone()
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn module_rebuilds_refuse_carried_state_before_planning_and_before_recording() {
    let server = server();
    let own = OwnDatabase::new(&server, "module-rebuild");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("module-rebuild");
    d.table(ONE_COLUMN);
    let view = d.dir.join("schema/v.yml");
    std::fs::write(&view, "view: app.v\ndefinition: SELECT id FROM app.t\n").unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    std::fs::write(
        &view,
        "view: app.v\ndefinition: SELECT id FROM app.t WHERE id > 0\n",
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let planning = ["plan", "--db", connection, "--out", plan.to_str().unwrap()];

    on_server(connection, "ALTER VIEW app.v SET (security_invoker = true)");
    let o = d.run(&planning);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("security_invoker"), "{}", stderr(&o));
    assert!(!plan.exists());
    on_server(connection, "ALTER VIEW app.v RESET (security_invoker)");
    // A comment is carried state the module precondition itself checks, not
    // an introspection limitation that an earlier guard could refuse instead.
    on_server(
        connection,
        "COMMENT ON VIEW app.v IS 'preserve this review note'",
    );
    refuses_connected_check_json(
        &d,
        connection,
        "before_a_rebuild (PostgreSQL)",
        "preserve this review note",
    );
    assert!(!plan.exists());
    on_server(connection, "COMMENT ON VIEW app.v IS NULL");
    let check = passed_connected_check_json(&d, connection, "before_a_rebuild");
    assert!(
        check["message"]
            .as_str()
            .unwrap()
            .starts_with("1 module rebuild"),
        "{check}"
    );
    succeeds(d.run(&planning));

    let staged = d.run(&["plan", "--db", connection, "--staged"]);
    assert_eq!(code(&staged), 1, "{}{}", stdout(&staged), stderr(&staged));
    assert!(stderr(&staged).contains("module rebuilds require a transaction"));

    // This attribute is outside the baseline checksum. Apply must ask again.
    on_server(
        connection,
        "COMMENT ON VIEW app.v IS 'keep this operator note'",
    );
    let o = apply_plan(&d, connection, &plan, false);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(
        stderr(&o).contains("keep this operator note"),
        "{}",
        stderr(&o)
    );
    on_server(
        connection,
        "DO $$ BEGIN IF obj_description('app.v'::regclass) IS DISTINCT FROM 'keep this operator note' THEN RAISE EXCEPTION 'note was lost'; END IF; END $$",
    );
    on_server(connection, "COMMENT ON VIEW app.v IS NULL");

    // The replacement itself can acquire state that the precheck never saw.
    on_server(
        connection,
        "CREATE FUNCTION public.note_rebuilt_view() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN COMMENT ON VIEW app.v IS 'injected after create'; END $$",
    );
    on_server(
        connection,
        "CREATE EVENT TRIGGER note_rebuilt_view ON ddl_command_end WHEN TAG IN ('CREATE VIEW') EXECUTE FUNCTION public.note_rebuilt_view()",
    );
    let o = apply_plan(&d, connection, &plan, false);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(
        stderr(&o).contains("injected after create"),
        "{}",
        stderr(&o)
    );
    on_server(
        connection,
        "DO $$ BEGIN IF obj_description('app.v'::regclass) IS NOT NULL OR pg_get_viewdef('app.v'::regclass) LIKE '%WHERE%' THEN RAISE EXCEPTION 'failed rebuild committed'; END IF; END $$",
    );
    on_server(connection, "DROP EVENT TRIGGER note_rebuilt_view");
    on_server(connection, "DROP FUNCTION public.note_rebuilt_view()");
    succeeds(apply_plan(&d, connection, &plan, false));
    succeeds(d.run(&["verify", "--db", connection]));
    let next = succeeds(d.run(&["plan", "--db", connection]));
    assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
}

/// #314. Every PostgreSQL module change is a drop and a create, and the
/// engine refuses the drop while anything depends on the module. The plan now
/// accounts for each dependent where the approver sees it: a check, a default
/// and a chain of views over a rebuilt module are dropped before it and put
/// back after it, an undeclared dependent refuses the plan by name, and a
/// dependent created after planning refuses the apply.
///
/// The objects are created on the server and adopted with `pull`, because a
/// table whose check calls a function cannot be bootstrapped: tables are built
/// before modules.
/// A function that becomes a procedure, a new function `g` beside it, and a
/// check calling `g`: the check still follows `CREATE FUNCTION` for `g`
/// (#1047). Judging the rebuild by the created kind alone stopped moving it.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_check_calling_a_new_function_follows_it_beside_a_function_turned_procedure() {
    let server = server();
    let own = OwnDatabase::new(&server, "function-to-procedure");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; \
         CREATE FUNCTION app.f(x integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT x $$; \
         CREATE TABLE app.t (id integer PRIMARY KEY)",
    );
    let d = Demo::new("function-to-procedure");
    succeeds(d.run(&["pull", "--db", connection]));
    d.commit();
    succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
    std::fs::remove_file(d.dir.join("schema/app.f%28integer%29.function.yml")).unwrap();
    std::fs::write(
        d.dir.join("schema/app.f%28integer%29.procedure.yml"),
        "procedure: app.f(integer)\npublic_execute: true\n\ndefinition: |-\n  \
         (IN x integer) LANGUAGE sql AS $$ SELECT 1 $$\n",
    )
    .unwrap();
    std::fs::write(
        d.dir.join("schema/app.g%28integer%29.function.yml"),
        "function: app.g(integer)\npublic_execute: true\n\ndefinition: |-\n  \
         (x integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT x $$\n",
    )
    .unwrap();
    let table = d.dir.join("schema/app.t.yml");
    let text = std::fs::read_to_string(&table).unwrap();
    std::fs::write(&table, format!("{text}checks:\n  ck_g: 'app.g(id) >= 0'\n")).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let sql = d.dir.join("plan.sql");
    succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--out",
        plan.to_str().unwrap(),
        "--sql",
        sql.to_str().unwrap(),
    ]));
    let script = std::fs::read_to_string(&sql).unwrap();
    let at = |needle: &str| {
        script
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` missing from:\n{script}"))
    };
    assert!(
        at("CREATE FUNCTION \"app\".\"g\"") < at("ADD CONSTRAINT \"ck_g\""),
        "{script}"
    );
    let allow = [
        "--allow",
        "constraint",
        "--allow",
        "destructive",
        "--allow",
        "grant-widen",
    ];
    succeeds(approved_apply(&d, connection, &plan, &allow));
    succeeds(d.run(&["verify", "--db", connection]));
    let next = succeeds(d.run(&["plan", "--db", connection]));
    assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
}

/// A procedure that becomes a function, and a check added in the same revision
/// that calls the new function: the check follows `CREATE FUNCTION` and the
/// plan applies (#1024). The kind change is a `DropModule` of the procedure and
/// a `CreateModule` of the function, so "is a function rebuilt" has to be read
/// from the create.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_check_calling_a_procedure_turned_function_follows_its_create() {
    let server = server();
    let own = OwnDatabase::new(&server, "procedure-to-function");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; \
         CREATE PROCEDURE app.f(x integer) LANGUAGE sql AS $$ SELECT 1 $$; \
         CREATE TABLE app.t (id integer PRIMARY KEY)",
    );
    let d = Demo::new("procedure-to-function");
    succeeds(d.run(&["pull", "--db", connection]));
    d.commit();
    succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
    std::fs::remove_file(d.dir.join("schema/app.f%28integer%29.procedure.yml")).unwrap();
    std::fs::write(
        d.dir.join("schema/app.f%28integer%29.function.yml"),
        "function: app.f(integer)\npublic_execute: true\n\ndefinition: |-\n  \
         (x integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT x $$\n",
    )
    .unwrap();
    let table = d.dir.join("schema/app.t.yml");
    let text = std::fs::read_to_string(&table).unwrap();
    std::fs::write(&table, format!("{text}checks:\n  ck_f: 'app.f(id) >= 0'\n")).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let sql = d.dir.join("plan.sql");
    succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--out",
        plan.to_str().unwrap(),
        "--sql",
        sql.to_str().unwrap(),
    ]));
    let script = std::fs::read_to_string(&sql).unwrap();
    let at = |needle: &str| {
        script
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` missing from:\n{script}"))
    };
    assert!(at("DROP PROCEDURE") < at("CREATE FUNCTION"), "{script}");
    assert!(
        at("CREATE FUNCTION") < at("ADD CONSTRAINT \"ck_f\""),
        "{script}"
    );
    let allow = [
        "--allow",
        "constraint",
        "--allow",
        "destructive",
        "--allow",
        "grant-widen",
    ];
    succeeds(approved_apply(&d, connection, &plan, &allow));
    succeeds(d.run(&["verify", "--db", connection]));
    let next = succeeds(d.run(&["plan", "--db", connection]));
    assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
}

/// A table the same revision creates, whose check, filtered index and default
/// call a function that revision rebuilds: they are created after the rebuild,
/// and the plan applies (#1027). They ride inside one `CreateTable` in the
/// differ's output, so the positional move of DEC-942.1 cannot reach them until
/// they are split out. Controls: the same new table with no rebuild stays one
/// `CREATE TABLE` with its check inside the plan's table creation, and a new
/// table whose rows the plan writes keeps its default.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_new_tables_expressions_calling_a_rebuilt_function_follow_it() {
    let server = server();
    let own = OwnDatabase::new(&server, "new-table-after-rebuild");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; \
         CREATE FUNCTION app.f(x integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT x $$",
    );
    let d = Demo::new("new-table-after-rebuild");
    succeeds(d.run(&["pull", "--db", connection]));
    d.commit();
    succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
    let function = d.dir.join("schema/app.f%28integer%29.function.yml");
    let text = std::fs::read_to_string(&function).unwrap();
    std::fs::write(&function, text.replace("SELECT x", "SELECT x + 0")).unwrap();
    std::fs::write(
        d.dir.join("schema/app.n.yml"),
        "table: app.n\ncolumns:\n  id: {type: integer, nullable: false}\n  \
         v: {type: integer, default: 'app.f(1)'}\n\
         primary_key: {name: n_pkey, columns: [id]}\n\
         checks:\n  ck_n: 'app.f(id) >= 0'\n\
         indexes:\n  ix_n: {columns: [id], where: 'app.f(id) > 0'}\n",
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let sql = d.dir.join("plan.sql");
    succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--out",
        plan.to_str().unwrap(),
        "--sql",
        sql.to_str().unwrap(),
    ]));
    let script = std::fs::read_to_string(&sql).unwrap();
    let at = |needle: &str| {
        script
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` missing from:\n{script}"))
    };
    let create = at("CREATE FUNCTION");
    assert!(
        at("CREATE TABLE \"app\".\"n\"") < at("DROP FUNCTION"),
        "{script}"
    );
    assert!(create < at("ADD CONSTRAINT \"ck_n\""), "{script}");
    assert!(create < at("CREATE INDEX \"ix_n\""), "{script}");
    assert!(
        create < at("ALTER TABLE \"app\".\"n\" ALTER COLUMN \"v\" SET DEFAULT"),
        "{script}"
    );
    let allow = ["--allow", "constraint", "--allow", "grant-widen"];
    succeeds(approved_apply(&d, connection, &plan, &allow));
    succeeds(d.run(&["verify", "--db", connection]));
    let next = succeeds(d.run(&["plan", "--db", connection]));
    assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
    std::fs::remove_file(&plan).unwrap();

    // Control: a new table beside no rebuild keeps its expressions in the
    // table's own creation, as the differ writes it.
    std::fs::write(
        d.dir.join("schema/app.m.yml"),
        "table: app.m\ncolumns:\n  id: {type: integer, nullable: false, default: 'app.f(2)'}\n\
         primary_key: {name: m_pkey, columns: [id]}\nchecks:\n  ck_m: 'app.f(id) >= 0'\n",
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--out",
        plan.to_str().unwrap(),
        "--sql",
        sql.to_str().unwrap(),
    ]));
    let script = std::fs::read_to_string(&sql).unwrap();
    assert!(!script.contains("DROP FUNCTION"), "{script}");
    assert!(
        !script.contains("ALTER COLUMN \"id\" SET DEFAULT"),
        "{script}"
    );
    succeeds(approved_apply(&d, connection, &plan, &allow));
    succeeds(d.run(&["verify", "--db", connection]));
}

/// A new default calling a function the revision rebuilds, on a table whose
/// row the same plan updates in another column: the update takes nothing from
/// the default, so the default follows `CREATE FUNCTION` and the plan applies
/// (#1030). Only a row that takes the default holds it back.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_default_follows_a_rebuilt_function_past_an_update_that_does_not_take_it() {
    let server = server();
    let own = OwnDatabase::new(&server, "default-past-update");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; \
         CREATE FUNCTION app.f(x integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT x $$; \
         CREATE TABLE app.u (id integer PRIMARY KEY, n integer, other integer); \
         INSERT INTO app.u VALUES (1, 7, 5)",
    );
    let d = Demo::new("default-past-update");
    succeeds(d.run(&["pull", "--db", connection]));
    d.commit();
    succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
    let function = d.dir.join("schema/app.f%28integer%29.function.yml");
    let text = std::fs::read_to_string(&function).unwrap();
    std::fs::write(&function, text.replace("SELECT x", "SELECT x + 0")).unwrap();
    std::fs::write(
        d.dir.join("schema/app.u.yml"),
        "table: app.u\ncolumns:\n  id: {type: integer, nullable: false}\n  \
         \"n\": {type: integer, default: 'app.f(1)'}\n  other: {type: integer}\n\
         primary_key: {name: u_pkey, columns: [id]}\n\
         data:\n  mode: ensure\n  rows:\n    1: {\"n\": 7, other: 6}\n",
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let sql = d.dir.join("plan.sql");
    succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--out",
        plan.to_str().unwrap(),
        "--sql",
        sql.to_str().unwrap(),
    ]));
    let script = std::fs::read_to_string(&sql).unwrap();
    let at = |needle: &str| {
        script
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` missing from:\n{script}"))
    };
    assert!(at("UPDATE \"app\".\"u\"") < at("DROP FUNCTION"), "{script}");
    assert!(
        at("CREATE FUNCTION") < at("ALTER TABLE \"app\".\"u\" ALTER COLUMN \"n\" SET DEFAULT"),
        "{script}"
    );
    let allow = ["--allow", "data-update", "--allow", "grant-widen"];
    succeeds(approved_apply(&d, connection, &plan, &allow));
    succeeds(d.run(&["verify", "--db", connection]));
    let next = succeeds(d.run(&["plan", "--db", connection]));
    assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
    assert_eq!(
        scalar(connection, "SELECT other::bigint FROM app.u WHERE id = 1"),
        6
    );
}

/// A function dropped for good, a view edited to stop calling it, and an
/// unchanged view over that view: the plan is valid, and the second view is
/// rebuilt around the first (#1069 review). Its path to the function goes
/// through the edited view, so it is not refused as a dependent of the drop.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_view_over_a_view_edited_off_a_dropped_function_is_rebuilt() {
    let server = server();
    let own = OwnDatabase::new(&server, "cut-path-to-dropped-function");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; \
         CREATE FUNCTION app.f(x integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT x $$; \
         CREATE VIEW app.v1 AS SELECT app.f(1) AS one; \
         CREATE VIEW app.v2 AS SELECT one FROM app.v1",
    );
    let d = Demo::new("cut-path-to-dropped-function");
    succeeds(d.run(&["pull", "--db", connection]));
    d.commit();
    succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
    std::fs::remove_file(d.dir.join("schema/app.f%28integer%29.function.yml")).unwrap();
    let v1 = d.dir.join("schema/app.v1.view.yml");
    let text = std::fs::read_to_string(&v1).unwrap();
    assert!(text.contains("app.f(1)"), "{text}");
    std::fs::write(&v1, text.replace("app.f(1)", "1")).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "destructive"],
    ));
    succeeds(d.run(&["verify", "--db", connection]));
    let next = succeeds(d.run(&["plan", "--db", connection]));
    assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
}

/// Declarations that drop a function for good while keeping a view that
/// calls it are refused by name at `plan --db`, and no plan is written
/// (#947). The view used to be rebuilt around the drop, and the plan failed at
/// apply on a `CREATE VIEW` against a function that was gone. Control: the
/// same drop with the view removed too plans and applies.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_view_kept_on_a_function_dropped_for_good_refuses_the_plan_by_name() {
    let server = server();
    let own = OwnDatabase::new(&server, "view-on-dropped-function");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; \
         CREATE FUNCTION app.f(x integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT x $$; \
         CREATE FUNCTION app.g(x integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT x $$; \
         CREATE VIEW app.v AS SELECT app.f(1) AS one, app.g(1) AS two",
    );
    let d = Demo::new("view-on-dropped-function");
    succeeds(d.run(&["pull", "--db", connection]));
    d.commit();
    succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
    std::fs::remove_file(d.dir.join("schema/app.f%28integer%29.function.yml")).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let o = d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    let err = stderr(&o);
    assert!(err.contains("app.v"), "{err}");
    assert!(err.contains("app.f(integer)"), "{err}");
    assert!(!plan.exists(), "a refused plan wrote {}", plan.display());

    // The view also calls `g`, and this revision edits `g`: the view is on a
    // rebuilt function too, and still refused for the dropped one rather than
    // rebuilt through the other (#1069 review).
    let g = d.dir.join("schema/app.g%28integer%29.function.yml");
    let text = std::fs::read_to_string(&g).unwrap();
    std::fs::write(&g, text.replace("SELECT x", "SELECT x + 0")).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let o = d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    let err = stderr(&o);
    assert!(
        err.contains("app.v") && err.contains("app.f(integer)"),
        "{err}"
    );
    assert!(!plan.exists(), "a refused plan wrote {}", plan.display());

    std::fs::remove_file(d.dir.join("schema/app.v.view.yml")).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "destructive"],
    ));
    succeeds(d.run(&["verify", "--db", connection]));
}

/// A sequence or an index already at the name of a table this plan creates
/// refuses the plan by name, and no plan is written (#951). Both share
/// PostgreSQL's relation namespace with tables, and neither is in the catalog
/// inventory the other occupied-name checks read, so the `CREATE TABLE` used
/// to reach apply and fail there. Control: the same table at a free name.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_sequence_or_index_at_a_new_tables_name_refuses_the_plan() {
    let server = server();
    let own = OwnDatabase::new(&server, "relation-namespace-occupant");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; \
         CREATE SEQUENCE app.s",
    );
    // Each case from a fresh project, so one case's ids file names nothing
    // the next one has to decide about.
    let adopted = |case: &str| {
        let d = Demo::new(&format!("relation-namespace-occupant-{case}"));
        succeeds(d.run(&["pull", "--db", connection]));
        d.commit();
        succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
        d
    };
    let declare = |d: &Demo, name: &str| {
        std::fs::write(
            d.dir.join(format!("schema/app.{name}.yml")),
            format!("table: app.{name}\ncolumns:\n  id: {{type: integer}}\n"),
        )
        .unwrap();
        succeeds(d.run(&["plan"]));
        d.commit();
        d.dir.join("plan.json")
    };
    for (name, kind) in [
        ("s", "sequence `app.s`"),
        ("i", "index `app.i` on `app.other`"),
    ] {
        let d = adopted(name);
        if name == "i" {
            // Created after adoption: a table this project does not record,
            // with an index at the name the declaration takes.
            on_server(
                connection,
                "CREATE TABLE app.other (id integer); CREATE INDEX i ON app.other (id)",
            );
        }
        let plan = declare(&d, name);
        let o = d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]);
        assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
        let err = stderr(&o);
        assert!(err.contains(&format!("already has {kind}")), "{err}");
        assert!(!plan.exists(), "a refused plan wrote {}", plan.display());
    }

    // A unique constraint the same revision drops frees its index's name
    // for a new table there (#1082 review): a valid plan, and applied.
    on_server(
        connection,
        "CREATE TABLE app.u (id integer, CONSTRAINT uq UNIQUE (id))",
    );
    let d = adopted("unique");
    std::fs::write(
        d.dir.join("schema/app.u.yml"),
        "table: app.u\ncolumns:\n  id: {type: integer}\n",
    )
    .unwrap();
    let plan = declare(&d, "uq");
    succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "destructive"],
    ));

    let d = adopted("free");
    let plan = declare(&d, "free");
    succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
    succeeds(approved_apply(&d, connection, &plan, &[]));
}

/// A check, a filtered index and a default the same revision adds, each calling
/// a function that revision rebuilds, are created after the rebuild (#942,
/// DEC-942.1). `modules::dependents` reads the catalog, where none of them
/// exists yet, so #941's weaving never saw them; the differ's own order put
/// them before `AlterModule`, and the rebuild's `DROP FUNCTION` was refused by
/// the object just created against it. Controls: the same additions with no
/// rebuild, and a default on a table whose rows the plan writes, which stays
/// ahead of the rows it has to fill.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn additions_calling_a_rebuilt_function_are_created_after_it() {
    let server = server();
    let own = OwnDatabase::new(&server, "additions-after-rebuild");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; \
         CREATE FUNCTION app.f(x integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT x $$; \
         CREATE TABLE app.t (id integer PRIMARY KEY, n integer); \
         CREATE TABLE app.u (id integer PRIMARY KEY, n integer)",
    );
    let d = Demo::new("additions-after-rebuild");
    succeeds(d.run(&["pull", "--db", connection]));
    d.commit();
    succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
    let function = d.dir.join("schema/app.f%28integer%29.function.yml");
    let table = |name: &str, extra: &str| {
        std::fs::write(
            d.dir.join(format!("schema/app.{name}.yml")),
            format!(
                "table: app.{name}\ncolumns:\n  id: {{type: integer, nullable: false}}\n  \
                 \"n\": {{type: integer, default: 'app.f(1)'}}\n\
                 primary_key: {{name: {name}_pkey, columns: [id]}}\n{extra}"
            ),
        )
        .unwrap();
    };
    let plan = d.dir.join("plan.json");
    let sql = d.dir.join("plan.sql");
    let planned = |d: &Demo| {
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&[
            "plan",
            "--db",
            connection,
            "--out",
            plan.to_str().unwrap(),
            "--sql",
            sql.to_str().unwrap(),
        ]));
        std::fs::read_to_string(&sql).unwrap()
    };
    let at = |script: &str, needle: &str| {
        script
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` missing from:\n{script}"))
    };
    let allow = ["--allow", "constraint", "--allow", "grant-widen"];

    // The additions and the rebuild in one revision. `u` also takes a row the
    // plan writes, so its default has to be in place before that insert.
    let text = std::fs::read_to_string(&function).unwrap();
    std::fs::write(&function, text.replace("SELECT x", "SELECT x + 0")).unwrap();
    table(
        "t",
        "checks:\n  ck_f: 'app.f(id) >= 0'\nindexes:\n  ix_f: {columns: [id], where: 'app.f(id) > 0'}\n",
    );
    table("u", "data:\n  mode: ensure\n  rows:\n    1: {}\n");
    let script = planned(&d);
    let create = at(&script, "CREATE FUNCTION");
    assert!(create < at(&script, "ADD CONSTRAINT \"ck_f\""), "{script}");
    assert!(create < at(&script, "CREATE INDEX \"ix_f\""), "{script}");
    let t_default = at(
        &script,
        "ALTER TABLE \"app\".\"t\" ALTER COLUMN \"n\" SET DEFAULT",
    );
    assert!(create < t_default, "{script}");
    // `u`'s default stays ahead of the row that needs it, so the rebuild's
    // `DROP` meets it: the one shape the positional rule leaves to the engine.
    let u_default = at(
        &script,
        "ALTER TABLE \"app\".\"u\" ALTER COLUMN \"n\" SET DEFAULT",
    );
    assert!(
        u_default < at(&script, "INSERT INTO \"app\".\"u\""),
        "{script}"
    );
    std::fs::remove_file(&plan).unwrap();

    // Without `u`'s row the revision applies whole and converges.
    table("u", "");
    let _ = planned(&d);
    succeeds(approved_apply(&d, connection, &plan, &allow));
    succeeds(d.run(&["verify", "--db", connection]));
    let next = succeeds(d.run(&["plan", "--db", connection]));
    assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_constraint WHERE conrelid = 'app.t'::regclass AND conname = 'ck_f'"
        ),
        1
    );
    std::fs::remove_file(&plan).unwrap();

    // Control: the same kind of addition with no rebuild keeps the differ's
    // order, class 13 ahead of the modules, and applies.
    table("u", "checks:\n  ck_u: 'app.f(id) >= 0'\n");
    let script = planned(&d);
    assert!(!script.contains("DROP FUNCTION"), "{script}");
    assert!(script.contains("ADD CONSTRAINT \"ck_u\""), "{script}");
    succeeds(approved_apply(&d, connection, &plan, &allow));
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn module_dependents_are_dropped_and_restored_around_the_rebuild_or_refused_by_name() {
    struct Role(String, String);
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let server = server();
    let reader = Role(
        server.clone(),
        format!("pbps_dep_reader_{}", std::process::id()),
    );
    let own = OwnDatabase::new(&server, "module-dependents");
    let connection = own.connection();
    on_server(connection, &format!("CREATE ROLE {} NOSUPERUSER", reader.1));
    on_server(
        connection,
        "CREATE SCHEMA app; \
         CREATE FUNCTION app.f(x integer) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT x $$; \
         CREATE TABLE app.t (id integer PRIMARY KEY CONSTRAINT ck CHECK (app.f(id) >= 0), \
                             n integer DEFAULT app.f(1)); \
         CREATE VIEW app.v0 AS SELECT id FROM app.t; \
         CREATE VIEW app.v1 AS SELECT id FROM app.v0; \
         CREATE VIEW app.v2 AS SELECT id FROM app.v1; \
         INSERT INTO app.t (id) VALUES (1), (2)",
    );
    // A declared grant on a view the rebuild of `v0` has to drop and create:
    // the plan restates it only if the differ rebuilt the view, not if the
    // pair were added after its permission passes had run.
    on_server(
        connection,
        &format!(
            "GRANT USAGE ON SCHEMA app TO {r}; GRANT SELECT ON app.v1 TO {r}",
            r = reader.1
        ),
    );
    let d = Demo::new("module-dependents");
    succeeds(d.run(&["pull", "--db", connection]));
    d.commit();
    succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));

    // A function edit: the check and the default depend on it, and neither is
    // in the diff.
    let function = d.dir.join("schema/app.f%28integer%29.function.yml");
    let text = std::fs::read_to_string(&function).unwrap();
    std::fs::write(&function, text.replace("SELECT x", "SELECT x + 0")).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let sql = d.dir.join("plan.sql");
    let o = succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--out",
        plan.to_str().unwrap(),
        "--sql",
        sql.to_str().unwrap(),
    ]));
    let script = std::fs::read_to_string(&sql).unwrap();
    let at = |needle: &str| {
        script
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` missing from:\n{script}"))
    };
    // Removed before the function goes, restored after it is back.
    assert!(
        at("DROP CONSTRAINT \"ck\"") < at("DROP FUNCTION"),
        "{script}"
    );
    assert!(at("DROP DEFAULT") < at("DROP FUNCTION"), "{script}");
    assert!(
        at("CREATE FUNCTION") < at("ADD CONSTRAINT \"ck\""),
        "{script}"
    );
    assert!(at("CREATE FUNCTION") < at("SET DEFAULT"), "{script}");
    let check = passed_connected_check_json(&d, connection, "module_dependents");
    assert!(
        check["message"]
            .as_str()
            .unwrap()
            .contains("2 dependent(s)"),
        "{check}"
    );
    assert!(stdout(&o).contains("ck"), "{}", stdout(&o));
    // Restoring the check revalidates the table, which is the approver's to
    // accept; the rebuild restates the engine's own `PUBLIC` execute.
    let allow = ["--allow", "constraint", "--allow", "grant-widen"];
    succeeds(approved_apply(&d, connection, &plan, &allow));
    succeeds(d.run(&["verify", "--db", connection]));
    let next = succeeds(d.run(&["plan", "--db", connection]));
    assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
    assert_eq!(
        text_of(
            connection,
            "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef WHERE adrelid = 'app.t'::regclass"
        ),
        "app.f(1)"
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_constraint WHERE conrelid = 'app.t'::regclass AND conname = 'ck'"
        ),
        1
    );
    std::fs::remove_file(&plan).unwrap();

    // A view edit under a chain of views: both are dropped deepest first and
    // created back in reverse, each through `before_a_rebuild`.
    let v0 = d.dir.join("schema/app.v0.view.yml");
    let text = std::fs::read_to_string(&v0).unwrap();
    std::fs::write(&v0, format!("{} WHERE id > 0\n", text.trim_end())).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--out",
        plan.to_str().unwrap(),
        "--sql",
        sql.to_str().unwrap(),
    ]));
    let script = std::fs::read_to_string(&sql).unwrap();
    let at = |needle: &str| {
        script
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` missing from:\n{script}"))
    };
    assert!(
        at("DROP VIEW \"app\".\"v2\"") < at("DROP VIEW \"app\".\"v1\""),
        "{script}"
    );
    assert!(
        at("DROP VIEW \"app\".\"v1\"") < at("DROP VIEW \"app\".\"v0\""),
        "{script}"
    );
    assert!(
        at("CREATE VIEW \"app\".\"v0\"") < at("CREATE VIEW \"app\".\"v1\""),
        "{script}"
    );
    assert!(
        at("CREATE VIEW \"app\".\"v1\"") < at("CREATE VIEW \"app\".\"v2\""),
        "{script}"
    );

    // Created after the plan was saved: the approver never saw it, so the
    // apply refuses rather than extends the plan, and nothing changes.
    on_server(
        connection,
        "CREATE VIEW public.late AS SELECT id FROM app.v0",
    );
    let approve = ["--allow", "destructive", "--allow", "grant-widen"];
    let o = approved_apply(&d, connection, &plan, &approve);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("module_dependents"), "{}", stderr(&o));
    assert!(stderr(&o).contains("public.late"), "{}", stderr(&o));
    on_server(
        connection,
        "DO $$ BEGIN IF pg_get_viewdef('app.v0'::regclass) LIKE '%>%' THEN RAISE EXCEPTION 'refused apply changed v0'; END IF; END $$",
    );
    // And planning again refuses it by name: this project does not declare
    // it, so it cannot be put back, and `CASCADE` is not offered.
    let o = d.run(&["plan", "--db", connection]);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(
        stderr(&o).contains("public.late") && stderr(&o).contains("does not declare it"),
        "{}",
        stderr(&o)
    );
    on_server(connection, "DROP VIEW public.late");
    std::fs::remove_file(&plan).unwrap();
    succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
    // The views the plan drops to rebuild them carry a drop's own risk: the
    // saved plan's risks are the dialect's answer for each change, and the
    // approver is approving `DROP VIEW` statements.
    let o = approved_apply(&d, connection, &plan, &allow);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("--allow destructive"), "{}", stderr(&o));
    succeeds(approved_apply(&d, connection, &plan, &approve));
    succeeds(d.run(&["verify", "--db", connection]));
    let next = succeeds(d.run(&["plan", "--db", connection]));
    assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
    // The rebuilt view still grants what its declaration grants.
    assert_eq!(
        scalar(
            connection,
            &format!(
                "SELECT count(*) FROM information_schema.role_table_grants \
                 WHERE grantee = '{}' AND table_schema = 'app' AND table_name = 'v1' \
                 AND privilege_type = 'SELECT'",
                reader.1
            )
        ),
        1
    );
}

/// #316, #320, #323. A name the declarations newly create that the
/// database already uses, outside what this project records, is a `CREATE`
/// this engine refuses at apply. `plan --db` now refuses it first, naming
/// each occupant by kind — a readable table, a view, a routine, and a
/// partitioned table pbps cannot read — with nothing written and no ledger
/// entry. A name nobody uses plans as before; adopting the readable
/// occupants with `baseline` clears the refusal; and a name taken after the
/// plan was saved fails the apply inside its transaction, recording nothing.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_newly_declared_name_the_database_already_uses_is_refused_before_the_plan() {
    let server = server();
    let own = OwnDatabase::new(&server, "occupied-names");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("occupied-names");
    d.table(ONE_COLUMN);
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    let entries = || scalar(connection, "SELECT count(*) FROM public.__pbps_state");
    let recorded = entries();

    on_server(
        connection,
        "CREATE TABLE app.x (id integer PRIMARY KEY); \
         CREATE VIEW app.v AS SELECT 1 AS n; \
         CREATE FUNCTION app.g(integer) RETURNS integer LANGUAGE sql AS $$ SELECT $1 $$; \
         CREATE TABLE app.p (id integer NOT NULL) PARTITION BY RANGE (id)",
    );
    let table = |name: &str| {
        std::fs::write(
            d.dir.join(format!("schema/{name}.yml")),
            format!(
                "table: {name}\ncolumns:\n  id: {{type: integer, nullable: false}}\n\
                 primary_key: [id]\n"
            ),
        )
        .unwrap();
    };
    for name in ["app.x", "app.p", "app.fresh"] {
        table(name);
    }
    std::fs::write(
        d.dir.join("schema/app.v.view.yml"),
        "view: app.v\ndefinition: SELECT 1 AS n\n",
    )
    .unwrap();
    std::fs::write(
        d.dir.join("schema/app.g.function.yml"),
        "function: app.g(integer)\ndefinition: |-\n  (integer) RETURNS integer LANGUAGE sql AS $$ SELECT $1 $$\n",
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();

    let plan = d.dir.join("plan.json");
    let o = d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    let err = stderr(&o);
    for named in [
        "table `app.x`: the database already has table `app.x`",
        "view `app.v`: the database already has view `app.v`",
        "function `app.g(integer)`: the database already has function `app.g(integer)`",
        "table `app.p`: the database already has `app.p`",
    ] {
        assert!(err.contains(named), "`{named}` missing from: {err}");
    }
    // The name nobody uses is not among them.
    assert!(!err.contains("app.fresh"), "{err}");
    assert!(!plan.exists());
    assert_eq!(entries(), recorded, "a refused plan touched the ledger");

    // The partitioned table cannot be adopted; its declaration goes. The
    // readable ones are adopted as they stand, and the refusal is gone.
    // Its uid was only ever minted locally, never recorded, so the identity
    // file is minted again rather than asked to drop a table no ledger has.
    std::fs::remove_file(d.dir.join("schema/app.p.yml")).unwrap();
    std::fs::remove_file(d.dir.join("schema.ids.json")).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt-existing"]));
    succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));

    // Taken after the plan was saved: the `CREATE` fails at apply, inside the
    // transaction, and the ledger records nothing.
    let recorded = entries();
    on_server(connection, "CREATE TABLE app.fresh (other text)");
    let o = apply_plan(&d, connection, &plan, false);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert_eq!(entries(), recorded, "a failed apply recorded an entry");
}

/// #248, end to end: a rebuild forced by an ordinary view edit still lets a
/// declared, least-privilege role read the view afterwards.
///
/// Read as the granted role itself, never as `postgres` — a superuser passes
/// every privilege check without consulting an ACL at all, so a superuser
/// read would pass whether or not the rebuild restored anything (DECISIONS
/// 375, the live suite's own correction of exactly that mistake). Only a
/// query that can fail on a missing grant is evidence the grant came back.
///
/// The refusal itself reads the plan: `read_acl` calls a grant restorable only
/// when the plan's own `ChangeSet` carries a matching `Grant`, so a plan that
/// does not restate this grant is exactly the plan that still refuses to
/// rebuild — there is no reachable path where the apply proceeds and the
/// grant is silently lost. That coupling is why reverting the differ's
/// restatement fails this test at the `plan --db` step, not at the final
/// read below — the refusal proves the plan *contains* the `Grant`; only the
/// role's own read after the apply proves the emitted statement lands on the
/// rebuilt object, with the declared permission, *after* the `CREATE` rather
/// than before it. Two different facts, and this test is both of them.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_view_rebuild_restates_a_declared_roles_grant_and_the_role_still_reads_it() {
    struct Role(String, String);
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let server = server();
    let role = Role(
        server.clone(),
        format!("pbps_view_reader_{}", std::process::id()),
    );
    let own = OwnDatabase::new(&server, "view-rebuild-grant");
    let connection = own.connection();
    on_server(
        connection,
        &format!(
            "CREATE ROLE {} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION \
             PASSWORD 'view-rebuild-grant'; CREATE SCHEMA app",
            role.1
        ),
    );
    // The same connection, reissued under the least-privilege role's own
    // credentials rather than the admin's (`doctor_reports_data_and_role_grant_gaps_from_the_declarations`'s
    // idiom for the same reason).
    let login = format!(
        "{} user={} password=view-rebuild-grant",
        connection
            .split_whitespace()
            .filter(|word| !word.starts_with("user=") && !word.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" "),
        role.1
    );

    let d = Demo::new("view-rebuild-grant");
    d.table(ONE_COLUMN);
    let view = d.dir.join("schema/v.yml");
    std::fs::write(&view, "view: app.v\ndefinition: SELECT id FROM app.t\n").unwrap();
    std::fs::write(
        d.dir.join("schema/reader.yml"),
        format!(
            "role: {}\ngrants:\n  schema::app: [usage]\n  app.v: [select]\n",
            role.1
        ),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));

    // Granted from the start: the ordinary path, unaffected by #248.
    on_server(&login, "SELECT id FROM app.v");

    // The view's return shape changes — every edit is a rebuild on this
    // engine, with no `CREATE OR REPLACE` fallback (ADR-0009 §3) — and the
    // role's declared grant is unchanged by this revision, so before #248 the
    // connected plan would have refused rather than silently drop it.
    std::fs::write(
        &view,
        "view: app.v\ndefinition: SELECT id, id * 2 AS doubled FROM app.t\n",
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
    succeeds(apply_plan(&d, connection, &plan, false));

    // The rebuilt object, read by the role the plan declared — not by the
    // admin connection that ran the apply.
    on_server(&login, "SELECT doubled FROM app.v");
    succeeds(d.run(&["verify", "--db", connection]));
}

/// A routine `PUBLIC` can execute, by the engine's own reckoning: the ACL
/// expanded from `acldefault` where the catalog holds none, which is the read
/// ADR-0010 §5 insists on — a NULL `proacl` is the default applying, not
/// nobody being granted anything.
const PUBLIC_EXECUTES: &str = "SELECT count(*) FROM pg_proc p, LATERAL \
     aclexplode(coalesce(p.proacl, acldefault('f', p.proowner))) a \
     WHERE p.oid = $ROUTINE$::regprocedure AND a.grantee = 0 AND a.privilege_type = 'EXECUTE'";

fn public_executes(connection: &str, routine: &str) -> bool {
    scalar(
        connection,
        &PUBLIC_EXECUTES.replace("$ROUTINE$", &format!("'{routine}'")),
    ) == 1
}

/// Issue #318. A `SECURITY DEFINER` routine runs as its owner, so this
/// engine's default — `EXECUTE` to `PUBLIC` on every function created — means
/// any principal that can reach the schema may act as the owner through it.
/// The plan takes that default away as part of creating the routine, and this
/// is the property measured from the outside: a real low-privilege login
/// attempting the privileged action the definer routine performs.
///
/// Every control the fix needs is in the one fixture, because each is only
/// meaningful beside the others: a routine that opted back in, a routine a
/// declared role was granted, and an `ALTER DEFAULT PRIVILEGES` that hands
/// `PUBLIC` the privilege explicitly rather than by default.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_created_routine_refuses_a_low_privilege_caller_unless_the_declaration_opens_it() {
    struct Role(String, String);
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let server = server();
    let role = Role(
        server.clone(),
        format!("pbps_pe_caller_{}", std::process::id()),
    );
    let own = OwnDatabase::new(&server, "public-execute");
    let connection = own.connection();
    on_server(
        connection,
        &format!(
            "CREATE ROLE {} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION \
             PASSWORD 'public-execute'; CREATE SCHEMA app",
            role.1
        ),
    );
    // The custom default the issue names. Measured on 18.6: with this in
    // place a function this account creates in `app` arrives with an
    // *explicit* `{=X/postgres,postgres=X/postgres}` rather than a NULL
    // `proacl` — the same access by a different route, and a fix that only
    // looked at "is the ACL NULL" would miss it. The plan's revoke names the
    // grantee, so it closes both.
    on_server(
        connection,
        "ALTER DEFAULT PRIVILEGES IN SCHEMA app GRANT EXECUTE ON FUNCTIONS TO PUBLIC",
    );
    let login = format!(
        "{} user={} password=public-execute",
        connection
            .split_whitespace()
            .filter(|word| !word.starts_with("user=") && !word.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" "),
        role.1
    );

    let d = Demo::new("public-execute");
    d.table(ONE_COLUMN);
    // The privileged marker action: the caller holds nothing on `app.t`, so
    // the only way a row reaches it is through the definer routine's owner.
    std::fs::write(
        d.dir.join("schema/mark.yml"),
        "function: app.mark()\ndefinition: |-\n  () RETURNS bigint LANGUAGE sql SECURITY DEFINER \
         SET search_path = app, pg_temp AS $$ INSERT INTO app.t (id) VALUES (1) RETURNING id $$\n",
    )
    .unwrap();
    // An ordinary invoker routine. Closed too, under the policy this issue
    // settled: which routines are definer routines is inside a body this tool
    // never parses, so the safe answer is the default and the declaration is
    // where a project says otherwise.
    std::fs::write(
        d.dir.join("schema/plain.yml"),
        "function: app.plain()\ndefinition: |-\n  () RETURNS integer LANGUAGE sql AS $$ SELECT 7 $$\n",
    )
    .unwrap();
    // A *procedure*, and it is here for the statement rather than the policy:
    // one word has to take a function and a procedure alike, and measured,
    // `ON FUNCTION` on a procedure is `is not a function` (DECISIONS 372).
    // Nothing but a live procedure proves the emitted `ON ROUTINE` is that
    // word on a revoke as well as on a grant.
    std::fs::write(
        d.dir.join("schema/touch.yml"),
        "procedure: app.touch()\ndefinition: |-\n  () LANGUAGE sql SECURITY DEFINER \
         SET search_path = app, pg_temp AS $$ INSERT INTO app.t (id) VALUES (2) $$\n",
    )
    .unwrap();
    // The one line that says otherwise.
    std::fs::write(
        d.dir.join("schema/open.yml"),
        "function: app.open()\npublic_execute: true\ndefinition: |-\n  () RETURNS integer LANGUAGE sql AS $$ SELECT 8 $$\n",
    )
    .unwrap();
    // The same line, on a signature the loader and the engine spell
    // differently. The loader is dialect-free, so this file says `int`; the
    // module key becomes `app.alias(integer)` before the differ runs, and an
    // opt-in that kept the file's spelling would match nothing — which the
    // differ reads as the closed answer, applying the declaration as its own
    // opposite.
    std::fs::write(
        d.dir.join("schema/alias.yml"),
        "function: app.alias(int)\npublic_execute: true\ndefinition: |-\n  (int) RETURNS integer LANGUAGE sql AS $$ SELECT $1 $$\n",
    )
    .unwrap();
    // The positive control: access granted the ordinary way, to a role the
    // declarations name. Closing `PUBLIC` must not close this.
    std::fs::write(
        d.dir.join("schema/granted.yml"),
        "function: app.granted()\ndefinition: |-\n  () RETURNS integer LANGUAGE sql AS $$ SELECT 9 $$\n",
    )
    .unwrap();
    std::fs::write(
        d.dir.join("schema/caller.yml"),
        format!(
            "role: {}\ngrants:\n  schema::app: [usage]\n  app.granted(): [execute]\n",
            role.1
        ),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    // No `--allow revoke`: a routine that did not exist a statement earlier
    // takes nothing away from anybody, and demanding the flag for every plan
    // that declares a function would be friction without safety.
    succeeds(d.run(&["bootstrap", "--db", connection]));

    // The exposure, attempted for real. Not "the ACL looks right" — a login
    // holding `USAGE` on the schema and nothing else, calling the definer
    // routine that writes as its owner.
    let denied = try_on_server(&login, "SELECT app.mark()").expect_err("PUBLIC must not execute");
    assert!(denied.contains("permission denied"), "{denied}");
    assert_eq!(
        scalar(connection, "SELECT count(*) FROM app.t"),
        0,
        "the marker action ran"
    );
    let denied =
        try_on_server(&login, "SELECT app.plain()").expect_err("an invoker routine is closed too");
    assert!(denied.contains("permission denied"), "{denied}");
    let denied = try_on_server(&login, "CALL app.touch()").expect_err("a procedure is closed too");
    assert!(denied.contains("permission denied"), "{denied}");

    // The three that stay open, each for its own reason.
    on_server(&login, "SELECT app.open()");
    on_server(&login, "SELECT app.alias(1)");
    on_server(&login, "SELECT app.granted()");

    // And the catalog agrees with the engine's own answer, `ALTER DEFAULT
    // PRIVILEGES` included.
    for closed in ["app.mark()", "app.plain()", "app.granted()", "app.touch()"] {
        assert!(!public_executes(connection, closed), "{closed} is open");
    }
    assert!(public_executes(connection, "app.open()"));
    assert!(public_executes(connection, "app.alias(integer)"));
    succeeds(d.run(&["verify", "--db", connection]));

    // A pull of this database writes the key back, or the project it hands
    // over would close `app.open()` on its first apply. And it writes it only
    // where the database has the routine open: the closed ones come back with
    // no key, which is what the absent key means.
    let puller = Demo::new("public-execute-pull");
    succeeds(puller.run(&["pull", "--db", connection]));
    // Found by its leading key rather than by filename: how a routine's
    // signature is spelled on disk is `declaration_file`'s business.
    let pulled = |routine: &str| {
        let wanted = [
            format!("function: {routine}"),
            format!("procedure: {routine}"),
        ];
        std::fs::read_dir(puller.dir.join("schema"))
            .unwrap()
            .filter_map(|e| std::fs::read_to_string(e.unwrap().path()).ok())
            .find(|text| wanted.iter().any(|w| text.starts_with(w)))
            .unwrap_or_else(|| panic!("no pulled declaration for {routine}"))
    };
    for open in ["app.open()", "app.alias(integer)"] {
        assert!(
            pulled(open).contains("public_execute: true"),
            "{open}: {}",
            pulled(open)
        );
    }
    for closed in ["app.mark()", "app.plain()", "app.granted()", "app.touch()"] {
        assert!(
            !pulled(closed).contains("public_execute"),
            "{closed}: {}",
            pulled(closed)
        );
    }
}

/// Every module edit on this engine is a drop and a create (ADR-0009 §3), and
/// the create restores the engine's default `EXECUTE` to `PUBLIC`. Before
/// issue #318 nothing in the model could take it away again, so a routine
/// somebody had closed by hand made every later plan *refuse* — hardening a
/// managed routine and managing it were mutually exclusive (ADR-0010 §5).
///
/// Both halves are measured here. The rebuild does not reopen the routine,
/// which is what the test has always been for; and it no longer deadlocks,
/// because the revoke that closes it again is a change in the approved plan.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn routine_rebuilds_do_not_restore_revoked_public_execute() {
    let server = server();
    let own = OwnDatabase::new(&server, "routine-rebuild");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("routine-rebuild");
    let file = d.dir.join("schema/secret.yml");
    let declaration = |value| {
        format!(
            "function: app.secret()\ndefinition: () RETURNS integer LANGUAGE sql SECURITY DEFINER SET search_path = app, pg_temp AS $$ SELECT {value} $$\n"
        )
    };
    std::fs::write(&file, declaration(1)).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    // Closed from the moment it was created, without anyone typing a REVOKE.
    assert!(!public_executes(connection, "app.secret()"));

    std::fs::write(&file, declaration(2)).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));

    // A hand revoke, on a routine the plan has already closed: the state the
    // old code refused to plan over. It is the plan's own state now, so the
    // rebuild goes ahead.
    on_server(
        connection,
        "REVOKE EXECUTE ON FUNCTION app.secret() FROM PUBLIC",
    );
    // The rebuild's revoke *is* gated, and this is the difference from a
    // fresh create: the object was there, so what the declarations cannot say
    // is whether anybody was relying on the default the `CREATE` restores.
    let unapproved = apply_plan(&d, connection, &plan, false);
    assert_eq!(
        code(&unapproved),
        1,
        "{}{}",
        stdout(&unapproved),
        stderr(&unapproved)
    );
    assert!(
        stderr(&unapproved).contains("--allow revoke"),
        "{}",
        stderr(&unapproved)
    );
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "revoke"],
    ));

    // The edit landed, and the routine is still closed to PUBLIC.
    assert_eq!(scalar(connection, "SELECT app.secret()::bigint"), 2);
    assert!(!public_executes(connection, "app.secret()"));
    succeeds(d.run(&["verify", "--db", connection]));
}

/// The other direction of the same deadlock. A routine this tool closed on
/// creation, whose declaration later adds `public_execute: true` *and* edits
/// the definition, asks for a rebuild whose whole effect is the `CREATE`
/// giving the default back.
///
/// The plan records that decision, and the rebuild guard reads it as the
/// recorded intent it is. A plan that instead stayed silent about the routine
/// looked to that guard exactly like a plan with no opinion, and the missing
/// default refused the rebuild: a valid plan refused for asking for the state
/// it was asked to reach.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_declaration_may_reopen_a_closed_routine_on_its_next_rebuild() {
    let server = server();
    let own = OwnDatabase::new(&server, "routine-reopen");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("routine-reopen");
    let file = d.dir.join("schema/reopen.yml");
    let declaration = |value: u8, open: bool| {
        format!(
            "function: app.reopen()\ndefinition: () RETURNS integer LANGUAGE sql AS $$ SELECT {value} $$\n{}",
            if open { "public_execute: true\n" } else { "" }
        )
    };
    std::fs::write(&file, declaration(1, false)).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    assert!(!public_executes(connection, "app.reopen()"));

    // Both edits at once, which is the only way the key can take effect: the
    // annotation is never compared, so it is the rebuild the definition edit
    // forces that acts on it.
    std::fs::write(&file, declaration(2, true)).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));

    // Not gated: keeping the engine's default is a widening, and a widening
    // is labelled rather than gated.
    succeeds(apply_plan(&d, connection, &plan, false));
    assert_eq!(scalar(connection, "SELECT app.reopen()::bigint"), 2);
    assert!(public_executes(connection, "app.reopen()"));
    succeeds(d.run(&["verify", "--db", connection]));

    // And the door closes again the same way it opened, on the next rebuild
    // that carries the other decision.
    std::fs::write(&file, declaration(3, false)).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "revoke"],
    ));
    assert!(!public_executes(connection, "app.reopen()"));
    succeeds(d.run(&["verify", "--db", connection]));
}

/// #691: EXECUTE that a third role granted to PUBLIC is something no
/// `REVOKE ... FROM PUBLIC` from this connection takes away. It never reaches
/// one. Closing a routine is a rebuild on this engine, and that role can only
/// have granted to PUBLIC while holding the grant option on the routine,
/// which the declarations have no flag for. So `before_a_rebuild` refuses the
/// plan, naming the role, before a statement runs, and the ACL is untouched.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn closing_a_routine_a_third_role_opened_to_public_is_refused_before_it_runs() {
    let admin = server();
    let mid = format!("pbps_pub_mid_{}", std::process::id());
    let _roles = ClusterRoles {
        server: admin.clone(),
        names: vec![mid.clone()],
    };
    on_server(&admin, &format!("CREATE ROLE {mid} NOSUPERUSER"));
    let own = OwnDatabase::new(&admin, "third-public");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("third-public");
    let file = d.dir.join("schema/shut.yml");
    let declaration = |value: u8, open: bool| {
        format!(
            "function: app.shut()\ndefinition: () RETURNS integer LANGUAGE sql AS $$ SELECT {value} $$\n{}",
            if open { "public_execute: true\n" } else { "" }
        )
    };
    std::fs::write(&file, declaration(1, true)).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    on_server(
        connection,
        &format!(
            "GRANT USAGE ON SCHEMA app TO {mid}; \
             GRANT EXECUTE ON FUNCTION app.shut() TO {mid} WITH GRANT OPTION; \
             SET ROLE {mid}; GRANT EXECUTE ON FUNCTION app.shut() TO PUBLIC; RESET ROLE"
        ),
    );
    let by_mid = || {
        scalar(
            connection,
            &format!(
                "SELECT count(*)::bigint FROM pg_proc p, aclexplode(p.proacl) a \
                  WHERE p.oid = 'app.shut()'::regprocedure AND a.grantee = 0 \
                    AND a.grantor = '{mid}'::regrole::oid"
            ),
        )
    };
    assert_eq!(
        by_mid(),
        1,
        "PUBLIC's entry has to carry the third role's grantor"
    );

    std::fs::write(&file, declaration(2, false)).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let refused = d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("before_a_rebuild") && stderr(&refused).contains(&mid),
        "{}",
        stderr(&refused)
    );
    assert!(!plan.exists(), "a refused plan writes no artifact");
    assert_eq!(by_mid(), 1, "nothing ran");
}

/// `--staged` applies one logical change (ADR-0003), and the decision the
/// differ appends to a routine it creates is not a second one. Creating one
/// routine in a staged revision was legal before issue #318, and the routine
/// that declares `public_execute: true` is the case where it has to stay so:
/// the other decision is refused for its own reason, by its own message.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_staged_revision_may_still_create_one_routine() {
    let server = server();
    let own = OwnDatabase::new(&server, "routine-staged");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("routine-staged");
    d.table(ONE_COLUMN);
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));

    let open = d.dir.join("schema/open.yml");
    std::fs::write(
        &open,
        "function: app.open()\npublic_execute: true\ndefinition: () RETURNS integer LANGUAGE sql AS $$ SELECT 1 $$\n",
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--staged",
        "--out",
        plan.to_str().unwrap(),
    ]));
    succeeds(approved_apply(&d, connection, &plan, &["--staged"]));
    assert!(public_executes(connection, "app.open()"));
    succeeds(d.run(&["verify", "--db", connection]));

    // The closed answer is refused, and by the rule that is actually about
    // it: the window between the `CREATE` and the revoke, not the count.
    std::fs::write(
        d.dir.join("schema/shut.yml"),
        "function: app.shut()\ndefinition: () RETURNS integer LANGUAGE sql AS $$ SELECT 2 $$\n",
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let refused = d.run(&["plan", "--db", connection, "--staged"]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("a staged run commits each statement on its own"),
        "{}",
        stderr(&refused)
    );
}

/// The same postcondition at the read `bootstrap` records, and the shape that
/// needs no other session at all: a `ddl_command_end` trigger already in the
/// database reverses what the build just settled, inside the deployer'"'"'s own
/// transaction.
///
/// The comment beside `refuse_unexpressible` there has said for a long time
/// that a database-side trigger can *add* a privilege during the build and
/// that a snapshot must never silently omit it (DECISIONS 110, 147). It can
/// take one back too, and what `PUBLIC` holds is in neither the schema nor
/// the unexpressible list (DECISIONS 371), so nothing else here would look.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn bootstrap_refuses_to_record_a_routine_a_trigger_reopened_to_public() {
    let server = server();
    let own = OwnDatabase::new(&server, "routine-bootstrap-snatch");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    // Already there when bootstrap starts, and it re-opens whatever the
    // build closes. Keyed on the command tag so its own `GRANT` does not
    // fire it again.
    on_server(
        connection,
        "CREATE FUNCTION public.reopen() RETURNS event_trigger LANGUAGE plpgsql AS $$          BEGIN IF EXISTS (SELECT 1 FROM pg_event_trigger_ddl_commands() WHERE command_tag = 'REVOKE')          AND to_regprocedure('app.shut()') IS NOT NULL          THEN GRANT EXECUTE ON ROUTINE app.shut() TO PUBLIC; END IF; END $$;          CREATE EVENT TRIGGER reopen_revoke ON ddl_command_end EXECUTE FUNCTION public.reopen()",
    );

    let d = Demo::new("routine-bootstrap-snatch");
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("schema/shut.yml"),
        "function: app.shut()\ndefinition: () RETURNS integer LANGUAGE sql SECURITY DEFINER SET search_path = app, pg_temp AS $$ SELECT 1 $$\n",
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();

    let refused = d.run(&["bootstrap", "--db", connection]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("app.shut()"),
        "{}",
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("this plan closed"),
        "{}",
        stderr(&refused)
    );
    // The read-back and the ledger row are in the build'"'"'s own transaction, so
    // the refusal leaves an empty database rather than a recorded lie.
    on_server(
        connection,
        "DO $$ BEGIN IF to_regprocedure('app.shut()') IS NOT NULL THEN RAISE EXCEPTION 'the build was not rolled back'; END IF; END $$",
    );

    // With the trigger gone the same declarations bootstrap, closed.
    on_server(connection, "DROP EVENT TRIGGER reopen_revoke");
    succeeds(d.run(&["bootstrap", "--db", connection]));
    assert!(!public_executes(connection, "app.shut()"));
    succeeds(d.run(&["verify", "--db", connection]));
}

/// Issue #308. `init --from` is the adoption workflow (SPEC §14.1), and it
/// kept the introspector's schema, warnings and unmanaged inventory while
/// dropping its `unexpressible` entries. A column-level grant or a
/// `WITH GRANT OPTION` could therefore be left out of the generated
/// declarations without being said out loud — and the command then printed a
/// success and suggested `baseline`, which refuses that database on exactly
/// those facts (DECISIONS 95, 97, 110).
///
/// The supported-grants case is the other half: a database with nothing
/// inexpressible in it adopts with the plain suggestion, so this is a report
/// and not a new refusal.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn init_from_a_database_says_which_permissions_it_could_not_take_with_it() {
    struct Role(String, String);
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let server = server();
    let role = Role(
        server.clone(),
        format!("pbps_init_ux_{}", std::process::id()),
    );
    let own = OwnDatabase::new(&server, "init-unexpressible");
    let connection = own.connection();
    on_server(
        connection,
        &format!(
            "CREATE ROLE {} NOSUPERUSER; CREATE SCHEMA app;              CREATE TABLE app.t (id integer PRIMARY KEY, email text);              CREATE TABLE app.plain (id integer PRIMARY KEY);              GRANT USAGE ON SCHEMA app TO {};              GRANT SELECT (email) ON app.t TO {};              GRANT SELECT ON app.plain TO {} WITH GRANT OPTION",
            role.1, role.1, role.1, role.1
        ),
    );

    let adopt = |tag: &str, connection: &str| {
        let dir = std::env::temp_dir().join(format!("pbps-init-ux-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let var = format!("PBPS_INIT_UX_{tag}_{}", std::process::id());
        let o = Command::new(BIN)
            .env(&var, connection)
            .arg("--project")
            .arg(&dir)
            .args([
                "init",
                "--from",
                "source",
                "--url-env",
                &var,
                "--dialect",
                "postgres",
            ])
            .output()
            .unwrap();
        (dir, o)
    };

    let (dir, o) = adopt("held", connection);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let all = format!("{}{}", stdout(&o), stderr(&o));
    // The role, the target and the privilege — each named, for both shapes.
    for expected in [
        role.1.as_str(),
        "app.t",
        "column `email`",
        "app.plain",
        "WITH GRANT OPTION",
    ] {
        assert!(all.contains(expected), "{expected} missing from: {all}");
    }
    // And the next step says so rather than sending the operator to a
    // `baseline` that refuses on the same facts.
    assert!(
        stdout(&o).contains("`baseline` refuses a database holding one"),
        "{}",
        stdout(&o)
    );
    // Adoption writes declarations and no ledger entry: `init` never touches
    // the database it read.
    assert!(dir.join("schema.ids.json").is_file());
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace              WHERE n.nspname = 'public' AND c.relname = '__pbps_state'"
        ),
        0,
        "init wrote a ledger"
    );
    let _ = std::fs::remove_dir_all(&dir);

    // #705: a schema literally named `$user` is one no plan can scope a
    // statement to. The adoption leaves its table, its view and the grants on
    // them out, says so, and writes a project the next command accepts —
    // where it used to refuse the whole adoption over them.
    let dollar = OwnDatabase::new(&server, "init-dollar-user");
    on_server(
        dollar.connection(),
        &format!(
            "CREATE SCHEMA app; CREATE TABLE app.t (id integer PRIMARY KEY); \
             CREATE SCHEMA \"$user\"; CREATE TABLE \"$user\".mine (id integer PRIMARY KEY); \
             CREATE VIEW \"$user\".v AS SELECT 1 AS x; \
             GRANT USAGE ON SCHEMA \"$user\" TO {r}; GRANT SELECT ON \"$user\".mine TO {r}",
            r = role.1
        ),
    );
    let (dir, o) = adopt("dollar", dollar.connection());
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    for named in [
        "table `$user.mine` was left out",
        "view $user.v",
        "on `$user.mine` was left out",
        "on `schema::$user` was left out",
    ] {
        assert!(
            stderr(&o).contains(named),
            "`{named}` missing from: {}",
            stderr(&o)
        );
    }
    let schema_dir = dir.join("schema");
    let mut dirs = vec![schema_dir.clone()];
    while let Some(at) = dirs.pop() {
        for entry in std::fs::read_dir(at).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(
                !text.contains("$user") && !path.to_string_lossy().contains("%24user"),
                "{} carries `$user`:\n{text}",
                path.display()
            );
        }
    }
    assert!(schema_dir.join("app.t.yml").is_file());
    // And the command after it accepts the project: `validate`, and a plan
    // that builds it — the `$user` view was the one `validate` used to pass
    // and the emitter refuse.
    let validated = Command::new(BIN)
        .arg("--project")
        .arg(&dir)
        .arg("validate")
        .output()
        .unwrap();
    assert_eq!(
        code(&validated),
        0,
        "{}{}",
        stdout(&validated),
        stderr(&validated)
    );
    let empty = OwnDatabase::new(&server, "init-dollar-empty");
    // `bootstrap` builds the declared objects, not the schemas they live in.
    on_server(empty.connection(), "CREATE SCHEMA app");
    let script = dir.join("bootstrap.sql");
    let built = Command::new(BIN)
        .arg("--project")
        .arg(&dir)
        .args(["bootstrap", "--db", empty.connection(), "--sql"])
        .arg(&script)
        .output()
        .unwrap();
    assert_eq!(code(&built), 0, "{}{}", stdout(&built), stderr(&built));
    let _ = std::fs::remove_dir_all(&dir);

    // The control: the same adoption against a database whose grants the
    // model can all hold.
    let plain = OwnDatabase::new(&server, "init-expressible");
    on_server(
        plain.connection(),
        "CREATE SCHEMA app; CREATE TABLE app.t (id integer PRIMARY KEY)",
    );
    let (dir, o) = adopt("plain", plain.connection());
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(
        stdout(&o).contains("pbps baseline --env source --reason initial-adoption"),
        "{}",
        stdout(&o)
    );
    assert!(
        !stdout(&o).contains("`baseline` refuses a database holding one"),
        "{}",
        stdout(&o)
    );
    let _ = std::fs::remove_dir_all(&dir);

    // The third case, and the one that decides which list the suggestion is
    // counted from: a permission the model cannot express on a securable
    // nothing manages. It is still reported — the operator asked what this
    // read left behind — but `baseline` cuts it out (DECISIONS 176), so
    // sending them to revoke it would be sending them to undo a privilege
    // that never reaches a plan.
    let outside = Role(
        server.clone(),
        format!("pbps_init_outside_{}", std::process::id()),
    );
    let aside = OwnDatabase::new(&server, "init-outside");
    on_server(
        aside.connection(),
        &format!(
            "CREATE ROLE {} NOSUPERUSER; CREATE SCHEMA app; \
             CREATE TABLE app.t (id integer PRIMARY KEY); \
             CREATE MATERIALIZED VIEW app.mv AS SELECT 1 AS n; \
             GRANT USAGE ON SCHEMA app TO {}; \
             GRANT SELECT ON app.mv TO {} WITH GRANT OPTION",
            outside.1, outside.1, outside.1
        ),
    );
    let (dir, o) = adopt("outside", aside.connection());
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(
        stderr(&o).contains("`app.mv` is on a materialized view"),
        "the finding is still reported: {}",
        stderr(&o)
    );
    assert!(
        !stdout(&o).contains("`baseline` refuses a database holding one"),
        "{}",
        stdout(&o)
    );
    // And the claim the suggestion no longer makes is the one the next
    // command settles.
    let var = format!("PBPS_INIT_UX_outside_{}", std::process::id());
    let baselined = Command::new(BIN)
        .env(&var, aside.connection())
        .arg("--project")
        .arg(&dir)
        .args([
            "baseline",
            "--env",
            "source",
            "--reason",
            "initial-adoption",
        ])
        .output()
        .unwrap();
    assert_eq!(
        code(&baselined),
        0,
        "{}{}",
        stdout(&baselined),
        stderr(&baselined)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Issue #902. The reader and the validator are two answers to "can a
/// declaration say this?", and wherever they disagreed `pull` wrote a project
/// the very next `validate` refused: an identity whose increment outruns its
/// type (#504), a table, a view or a grant in a schema named `$user` (#705),
/// a grant in a schema its role cannot use. Each is now left out of the files
/// and named, and what is written passes `validate`.
///
/// The controls are the neighbours of each: an identity at exactly its span,
/// a table and a grant in an ordinary schema.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn pull_leaves_out_and_names_what_validate_would_refuse() {
    struct Role(String, String);
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let server = server();
    let role = Role(
        server.clone(),
        format!("pbps_pull_902_{}", std::process::id()),
    );
    let own = OwnDatabase::new(&server, "pull-refused");
    let connection = own.connection().to_owned();
    on_server(
        &connection,
        &format!(
            "CREATE ROLE {r} NOSUPERUSER; CREATE SCHEMA app; CREATE SCHEMA other; \
             CREATE TABLE app.t (id integer PRIMARY KEY); \
             CREATE TABLE app.big (id smallint GENERATED ALWAYS AS IDENTITY \
                 (START WITH 1 INCREMENT BY 40000) PRIMARY KEY); \
             CREATE TABLE app.edge (id smallint GENERATED ALWAYS AS IDENTITY \
                 (START WITH 1 INCREMENT BY 32766) PRIMARY KEY); \
             CREATE TABLE other.u (id integer PRIMARY KEY); \
             CREATE SCHEMA \"$user\"; CREATE TABLE \"$user\".mine (id integer PRIMARY KEY); \
             CREATE VIEW \"$user\".v AS SELECT 1 AS x; \
             GRANT USAGE ON SCHEMA app TO {r}; GRANT SELECT ON app.t TO {r}; \
             GRANT SELECT ON other.u TO {r}; \
             GRANT USAGE ON SCHEMA \"$user\" TO {r}; GRANT SELECT ON \"$user\".mine TO {r}",
            r = role.1
        ),
    );

    let d = Demo::new("pull-refused");
    let o = d.run(&["pull", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let err = stderr(&o);
    for named in [
        "table `$user.mine` was left out",
        "table `app.big` was left out",
        // The identity the engine holds, not a corrected one.
        "increment of 40000",
        "view $user.v",
        "on `$user.mine` was left out",
        "on `schema::$user` was left out",
        "on `other.u` was left out",
    ] {
        assert!(err.contains(named), "`{named}` missing from: {err}");
    }
    assert!(
        stdout(&o).contains("could not be expressed and were left out"),
        "{}",
        stdout(&o)
    );

    // What was written is what `validate` accepts, and nothing of `$user` is
    // in it.
    let o = d.run(&["validate"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let mut files = Vec::new();
    let mut dirs = vec![d.dir.join("schema")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                files.push(path);
            }
        }
    }
    for path in &files {
        let text = std::fs::read_to_string(path).unwrap();
        assert!(
            !text.contains("$user") && !path.to_string_lossy().contains("%24user"),
            "{} carries `$user`:\n{text}",
            path.display()
        );
    }
    // The neighbours stayed: the exact-span identity, and the grant the
    // role's `usage` makes valid.
    let edge = std::fs::read_to_string(d.dir.join("schema/app.edge.yml")).unwrap();
    assert!(edge.contains("32766"), "{edge}");
    assert!(!d.dir.join("schema/app.big.yml").exists());
    let granted =
        std::fs::read_to_string(d.dir.join(format!("schema/roles/{}.yml", role.1))).unwrap();
    assert!(granted.contains("app.t"), "{granted}");
    assert!(!granted.contains("other.u"), "{granted}");

    // A pull only reads: the identity the engine holds is the one it had.
    assert_eq!(
        scalar(
            &connection,
            "SELECT increment_by FROM pg_sequences \
             WHERE schemaname = 'app' AND sequencename = 'big_id_seq'"
        ),
        40000
    );
}

/// Issue #919. A pull into a project whose `pbps.yml` sets a declaration rule
/// to `error` evaluated only `data.max-rows`, so it wrote a table the next
/// `validate` refused for its name. Every declaration rule is evaluated now,
/// as `validate` evaluates it: at `error` nothing is written; at `warning`
/// the pull succeeds and says so; excused by name, it succeeds quietly.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn pull_draws_the_line_the_projects_own_rules_draw() {
    let server = server();
    let own = OwnDatabase::new(&server, "pull-policies");
    let connection = own.connection().to_owned();
    on_server(
        &connection,
        "CREATE SCHEMA app; CREATE TABLE app.\"BadName\" (id integer PRIMARY KEY); \
         CREATE TABLE app.good (id integer PRIMARY KEY)",
    );
    let rule = "dialect: postgres\npolicies:\n  rules:\n    naming.table: \
                {severity: SEVERITY, pattern: \"^[a-z][a-z0-9_]*$\"}\n";

    let d = Demo::new("pull-policy-error");
    std::fs::write(d.dir.join("pbps.yml"), rule.replace("SEVERITY", "error")).unwrap();
    let o = d.run(&["pull", "--db", &connection]);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(
        stderr(&o).contains("naming.table") && stderr(&o).contains("app.BadName"),
        "{}",
        stderr(&o)
    );
    assert!(stderr(&o).contains("Nothing was written"), "{}", stderr(&o));
    assert!(
        std::fs::read_dir(d.dir.join("schema"))
            .unwrap()
            .next()
            .is_none(),
        "a refused pull wrote a file"
    );
    assert!(!d.dir.join("schema.ids.json").exists());

    // At `warning` the pull writes, reports the finding, and `validate`
    // accepts what it wrote.
    let d = Demo::new("pull-policy-warning");
    std::fs::write(d.dir.join("pbps.yml"), rule.replace("SEVERITY", "warning")).unwrap();
    let o = d.run(&["pull", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("app.BadName"), "{}", stderr(&o));
    let o = d.run(&["validate"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    // Excused by name at `error`: the pull writes, and so does `validate` pass.
    let d = Demo::new("pull-policy-suppressed");
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!(
            "{}  suppress:\n    - rule: naming.table\n      on: app.BadName\n      reason: inherited\n",
            rule.replace("SEVERITY", "error")
        ),
    )
    .unwrap();
    let o = d.run(&["pull", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert!(d.dir.join("schema/app.BadName.yml").is_file());
    let o = d.run(&["validate"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
}

/// Issue #921. `pull --data` validated the requested table before the
/// omission filter ran, so a table `validate` refuses stopped the whole pull
/// when its rows were asked for, and was merely left out when they were not.
/// Now it is left out either way: named, with a line saying its rows were not
/// read, while the other `--data` table in the same command gets its block.
/// A table the database does not have is still refused by name.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn pull_data_on_a_table_validate_refuses_leaves_it_out_and_reads_the_rest() {
    let server = server();
    let own = OwnDatabase::new(&server, "pull-data-refused");
    let connection = own.connection().to_owned();
    on_server(
        &connection,
        "CREATE SCHEMA app; \
         CREATE TABLE app.big (id smallint GENERATED BY DEFAULT AS IDENTITY \
             (START WITH 1 INCREMENT BY 40000) PRIMARY KEY); \
         CREATE TABLE app.good (id integer PRIMARY KEY, label text NOT NULL); \
         INSERT INTO app.big DEFAULT VALUES; \
         INSERT INTO app.good VALUES (1, 'one'), (2, 'two')",
    );

    let d = Demo::new("pull-data-refused");
    let o = d.run(&[
        "pull",
        "--db",
        &connection,
        "--data",
        "app.big",
        "--data",
        "app.good",
    ]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let err = stderr(&o);
    assert!(err.contains("table `app.big` was left out"), "{err}");
    assert!(
        err.contains("--data app.big: its rows were not read"),
        "{err}"
    );
    assert!(!d.dir.join("schema/app.big.yml").exists());
    let good = std::fs::read_to_string(d.dir.join("schema/app.good.yml")).unwrap();
    assert!(
        good.contains("mode: exact") && good.contains("two"),
        "{good}"
    );
    let o = d.run(&["validate"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    // The line between "left out" and "not there" holds: a name the database
    // does not have is refused, and nothing is written.
    let d = Demo::new("pull-data-missing");
    let o = d.run(&["pull", "--db", &connection, "--data", "app.nope"]);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("app.nope"), "{}", stderr(&o));
    assert!(
        std::fs::read_dir(d.dir.join("schema"))
            .unwrap()
            .next()
            .is_none(),
        "a refused pull wrote a file"
    );
}

/// Issue #261. A managed role that owns a managed object already holds every
/// privilege on it, with no ACL entry at all; the pull reads that entry — when
/// a `GRANT` forces the engine to write one — as the zero point rather than as
/// grants (DECISIONS 371). So a declaration granting the owner something reads
/// back as unsatisfied, the differ emits the same `GRANT` on every plan, and
/// the apply's closing read refuses it for not having achieved its own
/// postcondition.
///
/// Refused at `plan --db` instead, with the line to delete. The second half is
/// what keeps the rule scoped to the declaration: the same role owning the
/// same table, granted nothing on it, is an ordinary project.
/// Issue #261, at the one command that creates the objects it grants on.
///
/// `bootstrap` builds the declared schema, so the role that runs it owns
/// everything it built. A declaration granting that role a permission on what
/// it just created is the same impossible line a connected plan is refused
/// for — and there is nothing downstream to catch it here: the engine writes
/// only the owner's default ACL entry, the pull reads that entry as the zero
/// point (DECISIONS 371), and `bootstrap` has no declared-against-built
/// comparison of grants. Unrefused, it reports success and records a snapshot
/// that does not hold the declared grant.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn bootstrap_refuses_a_grant_to_the_role_that_will_own_what_it_builds() {
    let server = server();
    // The schema belongs to somebody else, so that the `usage` line the
    // grant needs is not itself the impossible one.
    let holder = format!("pbps_bootstrap_holder_{}", std::process::id());
    let other = format!("pbps_bootstrap_reader_{}", std::process::id());
    let _roles = ClusterRoles {
        server: server.clone(),
        names: vec![holder.clone(), other.clone()],
    };
    on_server(
        &server,
        &format!("CREATE ROLE {holder} NOSUPERUSER; CREATE ROLE {other} NOSUPERUSER"),
    );
    let own = OwnDatabase::new(&server, "bootstrap-owned-target");
    let connection = own.connection();
    on_server(
        connection,
        &format!("CREATE SCHEMA app AUTHORIZATION {holder}"),
    );
    let deployer = text_of(connection, "SELECT current_user");

    let d = Demo::new("bootstrap-owned-target");
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("schema/deployer.yml"),
        format!("role: {deployer}\ngrants:\n  schema::app: [usage]\n  app.t: [select]\n"),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let refused = d.run(&["bootstrap", "--db", connection]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("owned_targets") && stderr(&refused).contains("will own"),
        "{}",
        stderr(&refused)
    );
    assert!(stderr(&refused).contains("app.t"), "{}", stderr(&refused));
    // Refused before a statement runs: nothing built, nothing recorded.
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*)::bigint FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'app'"
        ),
        0
    );

    // The same build, granted to somebody who will not own it. The deployer's
    // own file stays: a role file that vanishes while another appears is a
    // rename to the resolver, and this test is not about identity.
    std::fs::write(
        d.dir.join("schema/deployer.yml"),
        format!("role: {deployer}\ngrants:\n  schema::app: [usage]\n"),
    )
    .unwrap();
    std::fs::write(
        d.dir.join("schema/other.yml"),
        format!("role: {other}\ngrants:\n  schema::app: [usage]\n  app.t: [select]\n"),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    succeeds(d.run(&["verify", "--db", connection]));
}

/// Issue #261. The owner a grant has to be checked against is the one the
/// object will have when the statement runs, not the one the baseline read
/// carries — and for an object the plan creates, the baseline has no owner
/// for it at all.
///
/// **Measured** on 18.6: a table created in somebody else's schema is owned
/// by its creator, not by the schema's owner. So the role that runs the plan
/// owns what the plan creates, and a declaration granting that role something
/// on it is the same impossible line as on a table that already stands.
///
/// The other half of this — a target the plan *replaces* — cannot reach this
/// check: `before_a_rebuild` refuses a rebuild whose object somebody else
/// owns, precisely because the rebuild would hand it to the deploying
/// account. The created case is the one that gets here.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_grant_on_a_table_this_plan_creates_is_checked_against_its_coming_owner() {
    let server = server();
    let other = format!("pbps_other_reader_{}", std::process::id());
    // The schema belongs to neither the deployment role nor the grantee, so
    // that the `usage` line every grant here needs is not itself the
    // impossible one this test is about.
    let holder = format!("pbps_schema_holder_{}", std::process::id());
    let mover = format!("pbps_table_mover_{}", std::process::id());
    let _roles = ClusterRoles {
        server: server.clone(),
        names: vec![other.clone(), holder.clone(), mover.clone()],
    };
    on_server(
        &server,
        &format!(
            "CREATE ROLE {other} NOSUPERUSER; CREATE ROLE {holder} NOSUPERUSER; \
             CREATE ROLE {mover} NOSUPERUSER"
        ),
    );
    let own = OwnDatabase::new(&server, "owner-after-plan");
    let connection = own.connection();
    on_server(
        connection,
        &format!("CREATE SCHEMA app AUTHORIZATION {holder}"),
    );

    let d = Demo::new("owner-after-plan");
    d.table(ONE_COLUMN);
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));

    // A table that does not exist yet, granted to the role that will own it
    // the moment the plan creates it.
    let deployer = text_of(connection, "SELECT current_user");
    let new_table = "table: app.n\ncolumns:\n  id: {type: bigint, nullable: false}\n\
                     primary_key: {name: pk_n, columns: [id]}\n";
    std::fs::write(d.dir.join("schema/n.yml"), new_table).unwrap();
    std::fs::write(
        d.dir.join("schema/deployer.yml"),
        format!("role: {deployer}\ngrants:\n  schema::app: [usage]\n  app.n: [select]\n"),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let refused = d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("owned_targets") && stderr(&refused).contains("will own"),
        "{}",
        stderr(&refused)
    );
    assert!(stderr(&refused).contains("app.n"), "{}", stderr(&refused));
    assert!(!plan.exists(), "a refused plan writes no artifact");

    // The negative case: the same new table, granted to somebody who will not
    // own it. A plan that creates a table and grants on it is ordinary work.
    // Kept, not deleted: a role file that vanishes while another appears is
    // a rename to the resolver, and this test is not about identity.
    std::fs::write(
        d.dir.join("schema/deployer.yml"),
        format!("role: {deployer}\ngrants:\n  schema::app: [usage]\n"),
    )
    .unwrap();
    std::fs::write(
        d.dir.join("schema/other.yml"),
        format!("role: {other}\ngrants:\n  schema::app: [usage]\n  app.n: [select]\n"),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
    succeeds(approved_apply(&d, connection, &plan, &[]));
    succeeds(d.run(&["verify", "--db", connection]));

    // And the third way a target can move under the check: a rename carries
    // the object's owner with it, while the map is keyed by what the read
    // saw. The grant names the table as the plan leaves it.
    // Handed to a role with no recorded grants of its own, so that moving the
    // owner does not itself read as drift: an owner's entry is the zero point
    // and leaves the comparison when it arrives.
    on_server(connection, &format!("ALTER TABLE app.n OWNER TO {mover}"));
    std::fs::write(
        d.dir.join("schema/n.yml"),
        new_table.replace("table: app.n", "table: app.m\nrenamed_from: app.n"),
    )
    .unwrap();
    std::fs::write(
        d.dir.join("schema/other.yml"),
        format!("role: {other}\ngrants:\n  schema::app: [usage]\n  app.m: [select]\n"),
    )
    .unwrap();
    std::fs::write(
        d.dir.join("schema/mover.yml"),
        format!("role: {mover}\ngrants:\n  schema::app: [usage]\n  app.m: [select]\n"),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let renamed = d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(
        code(&renamed),
        1,
        "{}{}",
        stdout(&renamed),
        stderr(&renamed)
    );
    assert!(
        stderr(&renamed).contains("owned_targets")
            && stderr(&renamed).contains("app.m")
            && stderr(&renamed).contains(&mover),
        "{}",
        stderr(&renamed)
    );
}

/// Issue #251, and the reason the grantor rule asks the engine rather than
/// comparing names.
///
/// A `REVOKE` carries the grantor PostgreSQL selects for it, and an inherited
/// one counts when nothing competes with it (DECISIONS 483). **Measured** on
/// 18.6: `ih_deploy`, an inheriting member of `ih_mid` and holding no direct
/// option of its own, removed `ih_reader=r/ih_mid`; with a competing direct
/// option in place the same statement removed nothing and reported success.
///
/// So the deployer here can narrow a role whose grants a role it inherits
/// made, and a name comparison would refuse that valid plan.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_grant_from_a_role_the_deployer_inherits_is_one_it_can_still_narrow() {
    let admin = server();
    let pid = std::process::id();
    let owner = format!("pbps_ih_owner_{pid}");
    let mid = format!("pbps_ih_mid_{pid}");
    let deployer = format!("pbps_ih_deploy_{pid}");
    let reader = format!("pbps_ih_read_{pid}");
    let _roles = ClusterRoles {
        server: admin.clone(),
        names: vec![owner.clone(), mid.clone(), deployer.clone(), reader.clone()],
    };
    on_server(
        &admin,
        &format!(
            "CREATE ROLE {owner} NOSUPERUSER; CREATE ROLE {mid} LOGIN NOSUPERUSER \
             PASSWORD 'trigger-test'; CREATE ROLE {deployer} LOGIN NOSUPERUSER INHERIT \
             PASSWORD 'trigger-test'; CREATE ROLE {reader} NOSUPERUSER; \
             GRANT {mid} TO {deployer}"
        ),
    );
    let own = OwnDatabase::new(&admin, "inherited-grantor");
    let connection = own.connection();
    on_server(
        connection,
        &format!(
            "CREATE SCHEMA app AUTHORIZATION {deployer}; \
             GRANT CREATE ON SCHEMA public TO {deployer}; \
             CREATE TABLE app.t(id bigint NOT NULL, CONSTRAINT pk_t PRIMARY KEY (id)); \
             ALTER TABLE app.t OWNER TO {owner}; \
             GRANT SELECT, INSERT ON app.t TO {mid} WITH GRANT OPTION; \
             GRANT USAGE ON SCHEMA app TO {reader}, {mid}"
        ),
    );
    // The entry this test is about: made by `mid`, which the deployer
    // inherits and holds no competing direct option against.
    on_server(
        &as_role(connection, &mid),
        &format!("GRANT SELECT, INSERT ON app.t TO {reader}"),
    );
    let deployment = as_role(connection, &deployer);
    let granted = |permission: &str| {
        scalar(
            connection,
            &format!(
                "SELECT CASE WHEN has_table_privilege('{reader}', 'app.t', '{permission}') \
                 THEN 1 ELSE 0 END::bigint"
            ),
        )
    };
    assert_eq!(granted("INSERT"), 1);

    let d = Demo::new("inherited-grantor");
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("schema/reader.yml"),
        format!("role: {reader}\ngrants:\n  schema::app: [usage]\n  app.t: [select]\n"),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&[
        "baseline",
        "--db",
        &deployment,
        "--reason",
        "adopting a database a role this deployer inherits granted into",
    ]));
    let plan = d.dir.join("plan.json");
    let planned = succeeds(d.run(&["plan", "--db", &deployment, "--out", plan.to_str().unwrap()]));
    assert!(
        stdout(&planned).contains("revoke insert on app.t"),
        "{}",
        stdout(&planned)
    );
    succeeds(d.run(&[
        "apply",
        "--db",
        &deployment,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
        "--allow",
        "revoke",
    ]));
    assert_eq!(granted("SELECT"), 1);
    assert_eq!(granted("INSERT"), 0);
    succeeds(d.run(&["verify", "--db", &deployment]));
}

/// Issue #251, the other half of the grantor rule. A `REVOKE` matches on the
/// grantor, and a least-privilege deployer holding `WITH GRANT OPTION` is the
/// second grantor whose entries this connection can match: it granted them
/// under its own name, not the owner's.
///
/// Reported as unexpressible, those entries would refuse the narrowing plan
/// the deployer is entitled to run — and refuse `baseline` before that, since
/// the database would be holding a permission the declarations cannot carry.
///
/// Run as the deployer, not as a superuser, because a superuser's grant on
/// somebody else's object records the **owner** as grantor (measured): the
/// case only exists where the connection is the grantor and the owner is
/// somebody else.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_deployers_own_onward_grant_is_one_it_can_still_narrow() {
    let admin = server();
    let pid = std::process::id();
    let owner = format!("pbps_gr_owner_{pid}");
    let deployer = format!("pbps_gr_deploy_{pid}");
    let reader = format!("pbps_gr_read_{pid}");
    // Declared before the database so that it drops after it: the roles own
    // objects inside it, and locals drop in reverse declaration order.
    let _roles = ClusterRoles {
        server: admin.clone(),
        names: vec![owner.clone(), deployer.clone(), reader.clone()],
    };
    on_server(
        &admin,
        &format!(
            "CREATE ROLE {owner} NOSUPERUSER; \
             CREATE ROLE {deployer} LOGIN NOSUPERUSER PASSWORD 'trigger-test'; \
             CREATE ROLE {reader} NOSUPERUSER"
        ),
    );
    let own = OwnDatabase::new(&admin, "deployer-grantor");
    let connection = own.connection();
    on_server(
        connection,
        &format!(
            "CREATE SCHEMA app AUTHORIZATION {deployer}; \
             GRANT CREATE ON SCHEMA public TO {deployer}; \
             CREATE TABLE app.t(id bigint NOT NULL, CONSTRAINT pk_t PRIMARY KEY (id)); \
             ALTER TABLE app.t OWNER TO {owner}; \
             GRANT SELECT, INSERT ON app.t TO {deployer} WITH GRANT OPTION; \
             GRANT USAGE ON SCHEMA app TO {reader}"
        ),
    );
    let deployment = as_role(connection, &deployer);
    // The grant this test is about: made by the deployer, under its own name.
    on_server(
        &deployment,
        &format!("GRANT SELECT, INSERT ON app.t TO {reader}"),
    );
    let granted = |permission: &str| {
        scalar(
            connection,
            &format!(
                "SELECT CASE WHEN has_table_privilege('{reader}', 'app.t', '{permission}') \
                 THEN 1 ELSE 0 END::bigint"
            ),
        )
    };
    assert_eq!(granted("SELECT"), 1);
    assert_eq!(granted("INSERT"), 1);

    let d = Demo::new("deployer-grantor");
    d.table(ONE_COLUMN);
    // Narrower than the database: the `INSERT` is the change this plan makes.
    std::fs::write(
        d.dir.join("schema/reader.yml"),
        format!("role: {reader}\ngrants:\n  schema::app: [usage]\n  app.t: [select]\n"),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&[
        "baseline",
        "--db",
        &deployment,
        "--reason",
        "adopting a database this deployer granted into",
    ]));

    let plan = d.dir.join("plan.json");
    let planned = succeeds(d.run(&["plan", "--db", &deployment, "--out", plan.to_str().unwrap()]));
    assert!(
        stdout(&planned).contains("revoke insert on app.t"),
        "{}",
        stdout(&planned)
    );
    succeeds(d.run(&[
        "apply",
        "--db",
        &deployment,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
        "--allow",
        "revoke",
    ]));
    // The statement the deployer was entitled to run, and it landed.
    assert_eq!(granted("SELECT"), 1);
    assert_eq!(granted("INSERT"), 0);
    succeeds(d.run(&["verify", "--db", &deployment]));
}

/// Issue #251, the grain of the grantor rule. A `REVOKE` removes only the
/// entries the one grantor it selects put there, so two grantors on **one**
/// privilege of one grantee defeat it. Two grantors on two *different*
/// privileges do not: measured on 18.6, `REVOKE SELECT` run by the deployer
/// over `reader=a/owner,reader=r/deployer` took the `SELECT` away and left
/// the owner's `INSERT` standing.
///
/// Reading that rule per target rather than per privilege refuses a narrowing
/// the deployer is entitled to run, and refuses `baseline` before it.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_grant_the_deployer_made_is_narrowable_beside_one_the_owner_made() {
    let admin = server();
    let pid = std::process::id();
    let owner = format!("pbps_mix_owner_{pid}");
    let deployer = format!("pbps_mix_deploy_{pid}");
    let reader = format!("pbps_mix_read_{pid}");
    let _roles = ClusterRoles {
        server: admin.clone(),
        names: vec![owner.clone(), deployer.clone(), reader.clone()],
    };
    on_server(
        &admin,
        &format!(
            "CREATE ROLE {owner} NOSUPERUSER; \
             CREATE ROLE {deployer} LOGIN NOSUPERUSER PASSWORD 'trigger-test'; \
             CREATE ROLE {reader} NOSUPERUSER"
        ),
    );
    let own = OwnDatabase::new(&admin, "mixed-grantors");
    let connection = own.connection();
    on_server(
        connection,
        &format!(
            "CREATE SCHEMA app AUTHORIZATION {deployer}; \
             GRANT CREATE ON SCHEMA public TO {deployer}; \
             CREATE TABLE app.t(id bigint NOT NULL, CONSTRAINT pk_t PRIMARY KEY (id)); \
             ALTER TABLE app.t OWNER TO {owner}; \
             GRANT SELECT ON app.t TO {deployer} WITH GRANT OPTION; \
             GRANT INSERT ON app.t TO {reader}; \
             GRANT USAGE ON SCHEMA app TO {reader}"
        ),
    );
    let deployment = as_role(connection, &deployer);
    // The second grantor on the same target, and the whole point of the case:
    // the owner granted the `INSERT`, this connection granted the `SELECT`.
    on_server(&deployment, &format!("GRANT SELECT ON app.t TO {reader}"));
    assert_eq!(
        scalar(
            connection,
            &format!(
                "SELECT count(DISTINCT grantor)::bigint \
                   FROM pg_class c, aclexplode(c.relacl) a \
                  WHERE c.oid = 'app.t'::regclass \
                    AND a.grantee = '{reader}'::regrole::oid"
            ),
        ),
        2,
        "the grantee has to hold entries from two grantors for this case to exist"
    );
    let granted = |permission: &str| {
        scalar(
            connection,
            &format!(
                "SELECT CASE WHEN has_table_privilege('{reader}', 'app.t', '{permission}') \
                 THEN 1 ELSE 0 END::bigint"
            ),
        )
    };

    let d = Demo::new("mixed-grantors");
    d.table(ONE_COLUMN);
    // The owner's `INSERT` is kept; the deployer's own `SELECT` is the one
    // this plan takes away.
    std::fs::write(
        d.dir.join("schema/reader.yml"),
        format!("role: {reader}\ngrants:\n  schema::app: [usage]\n  app.t: [insert]\n"),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&[
        "baseline",
        "--db",
        &deployment,
        "--reason",
        "adopting a database two roles granted into",
    ]));
    let plan = d.dir.join("plan.json");
    let planned = succeeds(d.run(&["plan", "--db", &deployment, "--out", plan.to_str().unwrap()]));
    assert!(
        stdout(&planned).contains("revoke select on app.t"),
        "{}",
        stdout(&planned)
    );
    succeeds(d.run(&[
        "apply",
        "--db",
        &deployment,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
        "--allow",
        "revoke",
    ]));
    assert_eq!(granted("SELECT"), 0);
    assert_eq!(
        granted("INSERT"),
        1,
        "the owner's grant was not this plan's"
    );
    succeeds(d.run(&["verify", "--db", &deployment]));

    // The negative the rule is for: two grantors on **one** privilege. The
    // deployer's `REVOKE INSERT` would leave the owner's entry behind, so the
    // permission is unexpressible and the declaration that drops it is
    // refused before a statement runs.
    on_server(
        connection,
        &format!("GRANT INSERT ON app.t TO {deployer} WITH GRANT OPTION"),
    );
    on_server(&deployment, &format!("GRANT INSERT ON app.t TO {reader}"));
    std::fs::write(
        d.dir.join("schema/reader.yml"),
        format!("role: {reader}\ngrants:\n  schema::app: [usage]\n"),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let refused = d.run(&["plan", "--db", &deployment, "--out", plan.to_str().unwrap()]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("INSERT") && stderr(&refused).contains(&reader),
        "{}",
        stderr(&refused)
    );
    assert_eq!(granted("INSERT"), 1, "nothing ran");
}

/// Issue #251, why one `REVOKE` carries one privilege. PostgreSQL selects the
/// grantor once for the whole statement, so a combined `REVOKE SELECT, INSERT`
/// over entries from two grantors takes only one of them and warns `not all
/// privileges could be revoked` — measured on 18.6, where the same two
/// privileges revoked one statement each took both.
///
/// Both grantors are roles this deployer inherits, so each privilege is
/// revocable on its own; only the width of the statement could lose one.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn two_grantors_on_two_privileges_are_revoked_one_statement_each() {
    let admin = server();
    let pid = std::process::id();
    let owner = format!("pbps_two_owner_{pid}");
    let deployer = format!("pbps_two_deploy_{pid}");
    let first = format!("pbps_two_first_{pid}");
    let second = format!("pbps_two_second_{pid}");
    let reader = format!("pbps_two_read_{pid}");
    let _roles = ClusterRoles {
        server: admin.clone(),
        names: vec![
            owner.clone(),
            deployer.clone(),
            first.clone(),
            second.clone(),
            reader.clone(),
        ],
    };
    on_server(
        &admin,
        &format!(
            "CREATE ROLE {owner} NOSUPERUSER; \
             CREATE ROLE {deployer} LOGIN NOSUPERUSER PASSWORD 'trigger-test'; \
             CREATE ROLE {first} NOSUPERUSER; \
             CREATE ROLE {second} NOSUPERUSER; \
             CREATE ROLE {reader} NOSUPERUSER; \
             GRANT {first} TO {deployer}; \
             GRANT {second} TO {deployer}"
        ),
    );
    let own = OwnDatabase::new(&admin, "two-grantors");
    let connection = own.connection();
    on_server(
        connection,
        &format!(
            "CREATE SCHEMA app AUTHORIZATION {deployer}; \
             GRANT CREATE ON SCHEMA public TO {deployer}; \
             CREATE TABLE app.t(id bigint NOT NULL, CONSTRAINT pk_t PRIMARY KEY (id)); \
             ALTER TABLE app.t OWNER TO {owner}; \
             GRANT USAGE ON SCHEMA app TO {first}, {second}, {reader}; \
             GRANT SELECT ON app.t TO {first} WITH GRANT OPTION; \
             GRANT INSERT ON app.t TO {second} WITH GRANT OPTION"
        ),
    );
    // One privilege from each intermediate role, which is what makes a single
    // statement unable to carry both. `SET ROLE` rather than a connection of
    // their own: the grantor a `GRANT` records is the current role, and these
    // two need no login for that.
    on_server(
        connection,
        &format!(
            "SET ROLE {first}; GRANT SELECT ON app.t TO {reader}; RESET ROLE; \
             SET ROLE {second}; GRANT INSERT ON app.t TO {reader}; RESET ROLE"
        ),
    );
    assert_eq!(
        scalar(
            connection,
            &format!(
                "SELECT count(DISTINCT grantor)::bigint \
                   FROM pg_class c, aclexplode(c.relacl) a \
                  WHERE c.oid = 'app.t'::regclass \
                    AND a.grantee = '{reader}'::regrole::oid"
            ),
        ),
        2,
        "the two privileges have to come from two grantors for this case to exist"
    );
    let deployment = as_role(connection, &deployer);
    let granted = |permission: &str| {
        scalar(
            connection,
            &format!(
                "SELECT CASE WHEN has_table_privilege('{reader}', 'app.t', '{permission}') \
                 THEN 1 ELSE 0 END::bigint"
            ),
        )
    };
    assert_eq!(granted("SELECT"), 1);
    assert_eq!(granted("INSERT"), 1);

    let d = Demo::new("two-grantors");
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("schema/reader.yml"),
        format!("role: {reader}\ngrants:\n  schema::app: [usage]\n  app.t: [select, insert]\n"),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&[
        "baseline",
        "--db",
        &deployment,
        "--reason",
        "adopting a database two intermediate roles granted into",
    ]));
    // Both permissions go, in one plan.
    std::fs::write(
        d.dir.join("schema/reader.yml"),
        format!("role: {reader}\ngrants:\n  schema::app: [usage]\n"),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    succeeds(d.run(&["plan", "--db", &deployment, "--out", plan.to_str().unwrap()]));
    succeeds(d.run(&[
        "apply",
        "--db",
        &deployment,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
        "--allow",
        "revoke",
    ]));
    // One combined statement would have left one of these standing.
    assert_eq!(granted("SELECT"), 0);
    assert_eq!(granted("INSERT"), 0);
    succeeds(d.run(&["verify", "--db", &deployment]));
}

/// Issue #251, the direction the limitation is *not* in. An entry a third
/// role granted is an ordinary grant: a declaration that keeps it is
/// satisfied by the database exactly as it stands, and no statement is
/// needed. Left out of the pull instead, it made every connected command over
/// that database fail — `refuse_unexpressible` runs before the changes are
/// looked at — so a no-op plan and the `baseline` before it were both refused.
///
/// Only the removal is impossible, and that is what is refused, before a
/// statement runs and with the grantor named.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_grant_a_third_role_made_is_kept_and_only_its_removal_is_refused() {
    let admin = server();
    let pid = std::process::id();
    let owner = format!("pbps_thd_owner_{pid}");
    let deployer = format!("pbps_thd_deploy_{pid}");
    let third = format!("pbps_thd_third_{pid}");
    let reader = format!("pbps_thd_read_{pid}");
    let _roles = ClusterRoles {
        server: admin.clone(),
        names: vec![
            owner.clone(),
            deployer.clone(),
            third.clone(),
            reader.clone(),
        ],
    };
    on_server(
        &admin,
        &format!(
            "CREATE ROLE {owner} NOSUPERUSER; \
             CREATE ROLE {deployer} LOGIN NOSUPERUSER PASSWORD 'trigger-test'; \
             CREATE ROLE {third} NOSUPERUSER; \
             CREATE ROLE {reader} NOSUPERUSER"
        ),
    );
    let own = OwnDatabase::new(&admin, "third-grantor");
    let connection = own.connection();
    on_server(
        connection,
        &format!(
            "CREATE SCHEMA app AUTHORIZATION {deployer}; \
             GRANT CREATE ON SCHEMA public TO {deployer}; \
             CREATE TABLE app.t(id bigint NOT NULL, CONSTRAINT pk_t PRIMARY KEY (id)); \
             ALTER TABLE app.t OWNER TO {owner}; \
             GRANT USAGE ON SCHEMA app TO {third}, {reader}; \
             GRANT SELECT ON app.t TO {third} WITH GRANT OPTION; \
             SET ROLE {third}; GRANT SELECT ON app.t TO {reader}; RESET ROLE"
        ),
    );
    let deployment = as_role(connection, &deployer);
    // The deployer is not the owner, is no member of the grantor, and holds no
    // option of its own on this privilege: nothing it runs takes this away.
    assert_eq!(
        scalar(
            connection,
            &format!(
                "SELECT count(*)::bigint FROM pg_class c, aclexplode(c.relacl) a \
                  WHERE c.oid = 'app.t'::regclass \
                    AND a.grantee = '{reader}'::regrole::oid \
                    AND a.grantor = '{third}'::regrole::oid"
            ),
        ),
        1,
        "the entry has to carry the third role's grantor for this case to exist"
    );

    let d = Demo::new("third-grantor");
    d.table(ONE_COLUMN);
    // The declaration that keeps it. Nothing has to run for this to be true.
    std::fs::write(
        d.dir.join("schema/reader.yml"),
        format!("role: {reader}\ngrants:\n  schema::app: [usage]\n  app.t: [select]\n"),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&[
        "baseline",
        "--db",
        &deployment,
        "--reason",
        "adopting a database a third role granted into",
    ]));
    let plan = d.dir.join("plan.json");
    let planned = succeeds(d.run(&["plan", "--db", &deployment, "--out", plan.to_str().unwrap()]));
    assert!(
        stdout(&planned).contains("No changes."),
        "the declaration matches the database: {}",
        stdout(&planned)
    );
    succeeds(d.run(&["verify", "--db", &deployment]));

    // The one direction that is impossible, refused before a statement runs.
    std::fs::write(
        d.dir.join("schema/reader.yml"),
        format!("role: {reader}\ngrants:\n  schema::app: [usage]\n"),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let _ = std::fs::remove_file(&plan);
    let refused = d.run(&["plan", "--db", &deployment, "--out", plan.to_str().unwrap()]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("unrevocable_grants"),
        "{}",
        stderr(&refused)
    );
    assert!(stderr(&refused).contains(&third), "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("nothing this tool can run takes it away"),
        "{}",
        stderr(&refused)
    );
    // #707: the remedy sends the operator to the grantor, never to the owner.
    assert!(
        stderr(&refused).contains(&format!("Have `{third}` revoke it"))
            && stderr(&refused).contains(&format!("SET ROLE \"{third}\"")),
        "{}",
        stderr(&refused)
    );
    assert!(!stderr(&refused).contains(&owner), "{}", stderr(&refused));
    assert!(!plan.exists(), "a refused plan writes no artifact");
    let reads = || {
        scalar(
            connection,
            &format!(
                "SELECT CASE WHEN has_table_privilege('{reader}', 'app.t', 'SELECT') \
                 THEN 1 ELSE 0 END::bigint"
            ),
        )
    };
    assert_eq!(reads(), 1, "nothing ran");

    // And the advice agrees with the engine. The owner's `REVOKE`, which the
    // message used to offer, reports success and leaves the entry standing;
    // the grantor's takes it away (#707).
    on_server(
        connection,
        &format!(
            "GRANT USAGE ON SCHEMA app TO {owner}; \
             SET ROLE {owner}; REVOKE SELECT ON app.t FROM {reader}; RESET ROLE"
        ),
    );
    assert_eq!(reads(), 1, "the owner's revoke left the third role's entry");
    on_server(
        connection,
        &format!("SET ROLE {third}; REVOKE SELECT ON app.t FROM {reader}; RESET ROLE"),
    );
    assert_eq!(reads(), 0, "the grantor's revoke took it away");
}

/// #696: a grant the table's owner made is revocable only by a connection
/// that can act as the owner. A least-privilege deployer that is neither the
/// owner, a superuser nor a member of the owner cannot take it back: its
/// `REVOKE` fails with `permission denied` (measured here). So a plan that
/// narrows the role is refused before a statement runs, naming the owner as
/// the one who can revoke it, while the read and a declaration that keeps the
/// grant are unaffected. The same plan made over a superuser connection is an
/// ordinary one.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn an_owners_grant_a_least_privilege_deployer_cannot_revoke_is_refused_before_it_runs() {
    let admin = server();
    let pid = std::process::id();
    let owner = format!("pbps_ogr_owner_{pid}");
    let deployer = format!("pbps_ogr_deploy_{pid}");
    let reader = format!("pbps_ogr_read_{pid}");
    let _roles = ClusterRoles {
        server: admin.clone(),
        names: vec![owner.clone(), deployer.clone(), reader.clone()],
    };
    on_server(
        &admin,
        &format!(
            "CREATE ROLE {owner} NOSUPERUSER; \
             CREATE ROLE {deployer} LOGIN NOSUPERUSER PASSWORD 'trigger-test'; \
             CREATE ROLE {reader} NOSUPERUSER"
        ),
    );
    let own = OwnDatabase::new(&admin, "owner-grant");
    let connection = own.connection();
    on_server(
        connection,
        &format!(
            "CREATE SCHEMA app AUTHORIZATION {deployer}; \
             GRANT CREATE ON SCHEMA public TO {deployer}; \
             CREATE TABLE app.t(id bigint NOT NULL, CONSTRAINT pk_t PRIMARY KEY (id)); \
             ALTER TABLE app.t OWNER TO {owner}; \
             GRANT USAGE ON SCHEMA app TO {owner}, {reader}; \
             SET ROLE {owner}; GRANT SELECT, INSERT ON app.t TO {reader}; RESET ROLE"
        ),
    );
    let deployment = as_role(connection, &deployer);
    // The premise, measured: this deployer's `REVOKE` cannot take the owner's
    // entry back.
    let denied = try_on_server(
        connection,
        &format!("SET ROLE {deployer}; REVOKE INSERT ON app.t FROM {reader}"),
    )
    .expect_err("a non-owner's revoke of the owner's grant");
    assert!(denied.contains("permission denied"), "{denied}");
    on_server(connection, "RESET ROLE");

    let d = Demo::new("owner-grant");
    d.table(ONE_COLUMN);
    let file = d.dir.join("schema/reader.yml");
    let declaration = |permissions: &str| {
        format!("role: {reader}\ngrants:\n  schema::app: [usage]\n  app.t: [{permissions}]\n")
    };
    // The declaration that keeps the grant reads and baselines as before.
    std::fs::write(&file, declaration("select, insert")).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&[
        "baseline",
        "--db",
        &deployment,
        "--reason",
        "adopting a database whose owner granted into it",
    ]));

    // Narrowing it is what this deployer cannot do.
    std::fs::write(&file, declaration("select")).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let refused = d.run(&["plan", "--db", &deployment, "--out", plan.to_str().unwrap()]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("unrevocable_grants")
            && stderr(&refused).contains(&format!("granted by `{owner}`"))
            && stderr(&refused).contains(&format!("Have `{owner}` revoke it")),
        "{}",
        stderr(&refused)
    );
    assert!(!plan.exists(), "a refused plan writes no artifact");

    // A superuser can take it back, so the same plan is an ordinary one there.
    succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
}

/// Issue #251, the rename the check has to see through. A plan that renames a
/// table and narrows a role in the same step spells the `Revoke` with the name
/// the plan leaves behind, while the read that found the grant knows the
/// object under the name it had. Keyed by the planned name alone, the check
/// finds nothing and lets through a `REVOKE` that removes no ACL entry — and
/// nothing downstream catches that (#700).
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_grant_no_revoke_could_remove_is_found_under_the_name_the_read_saw() {
    let admin = server();
    let pid = std::process::id();
    let owner = format!("pbps_rn_owner_{pid}");
    let deployer = format!("pbps_rn_deploy_{pid}");
    let third = format!("pbps_rn_third_{pid}");
    let reader = format!("pbps_rn_read_{pid}");
    let _roles = ClusterRoles {
        server: admin.clone(),
        names: vec![
            owner.clone(),
            deployer.clone(),
            third.clone(),
            reader.clone(),
        ],
    };
    on_server(
        &admin,
        &format!(
            "CREATE ROLE {owner} NOSUPERUSER; \
             CREATE ROLE {deployer} LOGIN NOSUPERUSER PASSWORD 'trigger-test'; \
             CREATE ROLE {third} NOSUPERUSER; \
             CREATE ROLE {reader} NOSUPERUSER"
        ),
    );
    let own = OwnDatabase::new(&admin, "renamed-grantor");
    let connection = own.connection();
    on_server(
        connection,
        &format!(
            "CREATE SCHEMA app AUTHORIZATION {deployer}; \
             GRANT CREATE ON SCHEMA public TO {deployer}; \
             CREATE TABLE app.t(id bigint NOT NULL, CONSTRAINT pk_t PRIMARY KEY (id)); \
             ALTER TABLE app.t OWNER TO {owner}; \
             GRANT USAGE ON SCHEMA app TO {third}, {reader}; \
             GRANT SELECT ON app.t TO {third} WITH GRANT OPTION; \
             SET ROLE {third}; GRANT SELECT ON app.t TO {reader}; RESET ROLE"
        ),
    );
    let deployment = as_role(connection, &deployer);

    let d = Demo::new("renamed-grantor");
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("schema/reader.yml"),
        format!("role: {reader}\ngrants:\n  schema::app: [usage]\n  app.t: [select]\n"),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&[
        "baseline",
        "--db",
        &deployment,
        "--reason",
        "adopting a database a third role granted into",
    ]));

    // The rename and the narrowing in one plan: the `Revoke` names `app.moved`
    // and the read knows the grant on `app.t`.
    d.table(&ONE_COLUMN.replace("table: app.t", "table: app.moved\nrenamed_from: app.t"));
    std::fs::write(
        d.dir.join("schema/reader.yml"),
        format!("role: {reader}\ngrants:\n  schema::app: [usage]\n"),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let refused = d.run(&["plan", "--db", &deployment, "--out", plan.to_str().unwrap()]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("unrevocable_grants") && stderr(&refused).contains(&third),
        "{}",
        stderr(&refused)
    );
    assert!(!plan.exists(), "a refused plan writes no artifact");
    assert_eq!(
        scalar(
            connection,
            &format!(
                "SELECT CASE WHEN has_table_privilege('{reader}', 'app.t', 'SELECT') \
                 THEN 1 ELSE 0 END::bigint"
            ),
        ),
        1,
        "nothing ran"
    );
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_declared_grant_to_the_targets_own_owner_is_refused_before_a_statement_runs() {
    struct Role(String, String);
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let server = server();
    let role = Role(server.clone(), format!("pbps_owner_{}", std::process::id()));
    let own = OwnDatabase::new(&server, "owned-target");
    let connection = own.connection();
    on_server(
        connection,
        &format!("CREATE ROLE {} NOSUPERUSER; CREATE SCHEMA app", role.1),
    );

    let d = Demo::new("owned-target");
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("schema/owner.yml"),
        format!("role: {}\n", role.1),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    // Ownership handed over out of band, which is the only way it moves: this
    // tool does not own `ALTER TABLE ... OWNER TO` (ADR-0010 §2).
    on_server(
        connection,
        &format!("ALTER TABLE app.t OWNER TO {}", role.1),
    );

    // The declaration that cannot converge.
    std::fs::write(
        d.dir.join("schema/owner.yml"),
        format!(
            "role: {}\ngrants:\n  schema::app: [usage]\n  app.t: [select]\n",
            role.1
        ),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let refused = d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("owned_targets"),
        "{}",
        stderr(&refused)
    );
    assert!(stderr(&refused).contains("app.t"), "{}", stderr(&refused));
    assert!(stderr(&refused).contains(&role.1), "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("delete it from role"),
        "{}",
        stderr(&refused)
    );
    assert!(!plan.exists(), "a refused plan writes no artifact");

    // The same declaration against an object whose ACL holds nothing at all.
    // `aclexplode` returns no rows for `{}`, so a reader that learned owners
    // from ACL rows would have this object owned by nobody and would accept
    // the very declaration refused above.
    on_server(connection, &format!("REVOKE ALL ON app.t FROM {}", role.1));
    assert_eq!(
        scalar(
            connection,
            "SELECT CASE WHEN relacl IS NOT NULL AND cardinality(relacl) = 0 \
             THEN 1 ELSE 0 END::bigint FROM pg_class WHERE relname = 't'"
        ),
        1,
        "the ACL has to be explicitly empty for this case to exist"
    );
    // The declaration is the one already committed above; only the database
    // has moved.
    let still = d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&still), 1, "{}{}", stdout(&still), stderr(&still));
    assert!(
        stderr(&still).contains("owned_targets") && stderr(&still).contains("app.t"),
        "{}",
        stderr(&still)
    );
    assert!(!plan.exists(), "a refused plan writes no artifact");

    // The same ownership, granted nothing: an ordinary project.
    std::fs::write(
        d.dir.join("schema/owner.yml"),
        format!("role: {}\n", role.1),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
    succeeds(d.run(&["verify", "--db", connection]));
}

/// #692: ownership is beside the checksum (DECISIONS 371), so a saved plan
/// whose target is handed to its grantee after planning still passes the
/// baseline. `apply` asks `owned_targets` again of the database as it stands,
/// on the transactional and on the staged path, and refuses before a
/// statement runs: no grant made, and no apply entry or checkpoint written.
/// The one entry the ledger gains is the failed-apply record every refusal
/// under the deployment lock leaves.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_target_handed_to_its_grantee_after_planning_is_refused_at_apply() {
    for staged in [false, true] {
        let slug = if staged {
            "owned-staged"
        } else {
            "owned-apply"
        };
        let server = server();
        let grantee = format!("pbps_own_late_{}_{}", std::process::id(), u8::from(staged));
        let _roles = ClusterRoles {
            server: server.clone(),
            names: vec![grantee.clone()],
        };
        on_server(&server, &format!("CREATE ROLE {grantee} NOSUPERUSER"));
        let own = OwnDatabase::new(&server, slug);
        let connection = own.connection();
        on_server(connection, "CREATE SCHEMA app");

        let d = Demo::new(slug);
        d.table(ONE_COLUMN);
        let file = d.dir.join("schema/grantee.yml");
        // Usage from the start, so the plan below is one logical change, which
        // is all a staged apply takes.
        std::fs::write(
            &file,
            format!("role: {grantee}\ngrants:\n  schema::app: [usage]\n"),
        )
        .unwrap();
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", connection]));

        std::fs::write(
            &file,
            format!("role: {grantee}\ngrants:\n  schema::app: [usage]\n  app.t: [select]\n"),
        )
        .unwrap();
        succeeds(d.run(&["plan"]));
        d.commit();
        let plan = d.dir.join("plan.json");
        let mut args = vec!["plan", "--db", connection, "--out", plan.to_str().unwrap()];
        if staged {
            args.push("--staged");
        }
        succeeds(d.run(&args));

        // After the plan was made, out of band: the only way ownership moves.
        on_server(connection, &format!("ALTER TABLE app.t OWNER TO {grantee}"));
        let entries = |kind: &str| {
            scalar(
                connection,
                &format!("SELECT count(*) FROM public.__pbps_state WHERE kind {kind}"),
            )
        };
        let (recorded, failed) = (entries("<> 'failed'"), entries("= 'failed'"));

        let refused = apply_plan(&d, connection, &plan, staged);
        assert_eq!(
            code(&refused),
            1,
            "{}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(
            stderr(&refused).contains("owned_targets")
                && stderr(&refused).contains("app.t")
                && stderr(&refused).contains(&grantee),
            "{}",
            stderr(&refused)
        );
        assert_eq!(
            entries("<> 'failed'"),
            recorded,
            "no apply entry or checkpoint was written"
        );
        assert_eq!(entries("= 'failed'"), failed + 1, "the refusal is audited");
        // Any `GRANT`, to the owner too, would have materialized the ACL.
        assert_eq!(
            scalar(
                connection,
                "SELECT CASE WHEN relacl IS NULL THEN 1 ELSE 0 END::bigint \
                 FROM pg_class WHERE oid = 'app.t'::regclass"
            ),
            1,
            "no statement ran"
        );
    }
}

/// SPEC §7.6, the case it names and the read it names it at.
///
/// A staged run commits each statement on its own, so another session can
/// reverse what a statement just wrote before the checkpoint reads it back.
/// §7.6 does not promise the checkpoint catches that — the field is expected
/// to move there, and the checkpoint cannot tell the plan'"'"'s statement from the
/// other session'"'"'s — it promises the **closing** read does, by holding the
/// field to the value the plan promised.
///
/// The other session is a DDL event trigger, which is how this suite stands
/// one in: measured, `ddl_command_end` fires on `GRANT`, so the revoke lands
/// after the plan'"'"'s own grant and before anything reads the catalog back.
/// What `PUBLIC` holds is in no `Schema` (DECISIONS 371), so nothing in the
/// movement comparison could have caught it.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_routine_reopened_or_closed_behind_the_plan_refuses_to_close_the_run() {
    let server = server();
    let own = OwnDatabase::new(&server, "routine-public-snatch");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("routine-public-snatch");
    d.table(ONE_COLUMN);
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));

    std::fs::write(
        d.dir.join("schema/open.yml"),
        "function: app.open()\npublic_execute: true\ndefinition: () RETURNS integer LANGUAGE sql AS $$ SELECT 1 $$\n",
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--staged",
        "--out",
        plan.to_str().unwrap(),
    ]));

    // The concurrent writer: it takes back exactly what the plan'"'"'s `GRANT`
    // just wrote, in the moment after it commits.
    on_server(
        connection,
        "CREATE FUNCTION public.snatch() RETURNS event_trigger LANGUAGE plpgsql AS $$          BEGIN IF EXISTS (SELECT 1 FROM pg_event_trigger_ddl_commands() WHERE command_tag = 'GRANT')          AND to_regprocedure('app.open()') IS NOT NULL          THEN REVOKE EXECUTE ON ROUTINE app.open() FROM PUBLIC; END IF; END $$;          CREATE EVENT TRIGGER snatch_grant ON ddl_command_end EXECUTE FUNCTION public.snatch()",
    );

    let refused = approved_apply(&d, connection, &plan, &["--staged"]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("app.open()"),
        "{}",
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("declared it would stay"),
        "{}",
        stderr(&refused)
    );
    // The routine is there — the statements ran and committed — and it is the
    // *closing* that refused, which is the honest answer: the ledger keeps
    // the last checkpoint rather than recording a success it cannot vouch for.
    assert_eq!(scalar(connection, "SELECT app.open()::bigint"), 1);
    assert!(!public_executes(connection, "app.open()"));

    // A resume alone does not mend it, and should not pretend to: every
    // statement has already run, so there is no `GRANT` left to re-issue and
    // the read finds the same thing again. The refusal is not a retryable
    // hiccup, it is a report that somebody else moved the field.
    on_server(connection, "DROP EVENT TRIGGER snatch_grant");
    let again = approved_apply(&d, connection, &plan, &["--staged", "--resume"]);
    assert_eq!(code(&again), 1, "{}{}", stdout(&again), stderr(&again));
    assert!(
        stderr(&again).contains("declared it would stay"),
        "{}",
        stderr(&again)
    );

    // Settling what moved is the operator's step, and then it closes — which
    // is what the refusal told them to do.
    on_server(connection, "GRANT EXECUTE ON ROUTINE app.open() TO PUBLIC");
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--staged", "--resume"],
    ));
    assert!(public_executes(connection, "app.open()"));
    succeeds(d.run(&["verify", "--db", connection]));
}

/// The engine'"'"'s default is not the last word on what a new routine arrives
/// holding. A cluster whose deployment role has run `ALTER DEFAULT PRIVILEGES
/// REVOKE EXECUTE ON ROUTINES FROM PUBLIC` creates routines with an explicit
/// ACL that `PUBLIC` is not in — so an opt-in that emitted nothing, trusting
/// the `CREATE`, would apply a declaration and leave the declared state
/// unreached, with `verify` unable to report it because what `PUBLIC` holds is
/// never compared (DECISIONS 371).
///
/// Both directions are measured against that cluster: the declaration that
/// asks for `PUBLIC` gets it, and the one that does not stays closed.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_declaration_opens_a_routine_even_where_default_privileges_close_it() {
    let server = server();
    let own = OwnDatabase::new(&server, "routine-default-acl");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    // Per-database, and the deployer is the role creating the routines, so
    // this is the state every `CREATE` below starts from.
    on_server(
        connection,
        "ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON ROUTINES FROM PUBLIC",
    );
    let d = Demo::new("routine-default-acl");
    std::fs::write(
        d.dir.join("schema/open.yml"),
        "function: app.open()\ndefinition: () RETURNS integer LANGUAGE sql AS $$ SELECT 1 $$\npublic_execute: true\n",
    )
    .unwrap();
    std::fs::write(
        d.dir.join("schema/shut.yml"),
        "function: app.shut()\ndefinition: () RETURNS integer LANGUAGE sql AS $$ SELECT 2 $$\n",
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));

    // The declared state, reached against a cluster that would not have
    // handed it over on its own.
    assert!(public_executes(connection, "app.open()"));
    assert!(!public_executes(connection, "app.shut()"));
    succeeds(d.run(&["verify", "--db", connection]));

    // The *rebuild* direction on such a cluster never gets this far, and that
    // is a different guard doing its job: `pg_default_acl` is one of the
    // things ADR-0010 §2 refuses to plan over, because who creates an object
    // decides what it arrives with. Asserted here so that the division of
    // labour is on the record — the opt-in's `GRANT` is what covers the
    // `CREATE`, and this refusal is what covers everything else the default
    // privileges would hand the replacement.
    std::fs::write(
        d.dir.join("schema/open.yml"),
        "function: app.open()\ndefinition: () RETURNS integer LANGUAGE sql AS $$ SELECT 3 $$\npublic_execute: true\n",
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let refused = d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("ALTER DEFAULT PRIVILEGES"),
        "{}",
        stderr(&refused)
    );
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn bootstrap_requires_cluster_roles_before_building_and_before_recording() {
    struct Role(String, String);
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let server = server();
    let role = Role(
        server.clone(),
        format!("pbps_bootstrap_empty_{}", std::process::id()),
    );
    let own = OwnDatabase::new(&server, "bootstrap-missing-role");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("bootstrap-missing-role");
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("schema/reader.yml"),
        format!("role: {}\n", role.1),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let refused = |o: Output| {
        assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
        assert!(
            stderr(&o).contains(&format!("CREATE ROLE \"{}\"", role.1)),
            "{}",
            stderr(&o)
        );
    };
    refused(d.run(&["bootstrap", "--db", connection]));
    on_server(
        connection,
        "DO $$ BEGIN IF to_regclass('app.t') IS NOT NULL OR EXISTS (SELECT 1 FROM public.__pbps_state) THEN RAISE EXCEPTION 'missing-role bootstrap built or recorded state'; END IF; END $$",
    );

    on_server(&server, &format!("CREATE ROLE {}", role.1));
    on_server(
        connection,
        &format!(
            "CREATE FUNCTION public.remove_bootstrap_role() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN IF EXISTS (SELECT 1 FROM pg_event_trigger_ddl_commands() WHERE object_identity = 'app.t') THEN DROP ROLE {}; END IF; END $$",
            role.1
        ),
    );
    on_server(
        connection,
        "CREATE EVENT TRIGGER remove_bootstrap_role ON ddl_command_end WHEN TAG IN ('CREATE TABLE') EXECUTE FUNCTION public.remove_bootstrap_role()",
    );
    refused(d.run(&["bootstrap", "--db", connection]));
    on_server(
        connection,
        &format!(
            "DO $$ BEGIN IF to_regclass('app.t') IS NOT NULL OR NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{}') THEN RAISE EXCEPTION 'missing-role read-back did not roll back the build'; END IF; END $$",
            role.1
        ),
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
            .await
            .unwrap();
        let last = pbps_pg::state::latest(&mut conn).await.unwrap().unwrap();
        assert_eq!(last.snapshot.kind, pbps_model::StateKind::Failed);
        assert!(last.snapshot.schema.roles.is_empty());
        assert!(last.snapshot.ids.roles.is_empty());
    });
    on_server(connection, "DROP EVENT TRIGGER remove_bootstrap_role");
    on_server(connection, "DROP FUNCTION public.remove_bootstrap_role()");
    succeeds(d.run(&["bootstrap", "--db", connection]));
    succeeds(d.run(&["verify", "--db", connection]));
    on_server(&server, &format!("DROP ROLE {}", role.1));
    let missing = d.run(&["verify", "--db", connection]);
    assert_eq!(
        code(&missing),
        2,
        "{}{}",
        stdout(&missing),
        stderr(&missing)
    );
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn grantless_cluster_role_additions_and_removals_update_the_recorded_scope() {
    struct Role(String, String);
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let server = server();
    let role = Role(
        server.clone(),
        format!("pbps_grantless_{}", std::process::id()),
    );
    let own = OwnDatabase::new(&server, "grantless-role");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("grantless-role");
    d.table(ONE_COLUMN);
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    let file = d.dir.join("schema/reader.yml");
    std::fs::write(&file, format!("role: {}\n", role.1)).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let planning = ["plan", "--db", connection, "--out", plan.to_str().unwrap()];
    let missing = d.run(&planning);
    assert_eq!(
        code(&missing),
        1,
        "{}{}",
        stdout(&missing),
        stderr(&missing)
    );
    assert!(
        stderr(&missing).contains("is missing"),
        "{}",
        stderr(&missing)
    );
    refuses_connected_check_json(&d, connection, "missing_roles (PostgreSQL)", &role.1);
    assert!(!plan.exists());
    on_server(&server, &format!("CREATE ROLE {}", role.1));
    let check = passed_connected_check_json(&d, connection, "missing_roles");
    assert!(
        check["message"]
            .as_str()
            .unwrap()
            .starts_with("1 incoming or renamed"),
        "{check}"
    );
    succeeds(d.run(&planning));
    let saved: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&plan).unwrap()).unwrap();
    assert!(saved.changes.is_empty());
    // The incoming role's grants are pinned even before its UID is recorded.
    on_server(
        connection,
        &format!("GRANT USAGE ON SCHEMA app TO {}", role.1),
    );
    let changed = apply_plan(&d, connection, &plan, false);
    assert_eq!(
        code(&changed),
        1,
        "{}{}",
        stdout(&changed),
        stderr(&changed)
    );
    assert!(
        stderr(&changed).contains("no longer the database"),
        "{}",
        stderr(&changed)
    );
    on_server(
        connection,
        &format!("REVOKE USAGE ON SCHEMA app FROM {}", role.1),
    );
    succeeds(apply_plan(&d, connection, &plan, false));
    succeeds(d.run(&["verify", "--db", connection]));
    on_server(
        connection,
        &format!("GRANT USAGE ON SCHEMA app TO {}", role.1),
    );
    let drift = d.run(&["verify", "--db", connection]);
    assert_eq!(code(&drift), 2, "{}{}", stdout(&drift), stderr(&drift));
    on_server(
        connection,
        &format!("REVOKE USAGE ON SCHEMA app FROM {}", role.1),
    );
    on_server(&server, &format!("DROP ROLE {}", role.1));
    let missing = d.run(&["verify", "--db", connection]);
    assert_eq!(
        code(&missing),
        2,
        "{}{}",
        stdout(&missing),
        stderr(&missing)
    );
    on_server(&server, &format!("CREATE ROLE {}", role.1));
    succeeds(d.run(&["verify", "--db", connection]));

    std::fs::remove_file(file).unwrap();
    succeeds(d.run(&[
        "drop-role",
        &role.1,
        "--reason",
        "stop managing this cluster role",
    ]));
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&planning));
    let saved: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&plan).unwrap()).unwrap();
    assert!(saved.changes.is_empty());
    assert!(saved.ids.roles.is_empty());
    succeeds(apply_plan(&d, connection, &plan, false));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
            .await
            .unwrap();
        let last = pbps_pg::state::latest(&mut conn).await.unwrap().unwrap();
        assert_eq!(last.snapshot.kind, pbps_model::StateKind::Apply);
        assert!(last.snapshot.ids.roles.is_empty());
        assert!(last.snapshot.schema.roles.is_empty());
    });
    on_server(&server, &format!("DROP ROLE {}", role.1));
    succeeds(d.run(&["verify", "--db", connection]));

    // A nonempty staged plan must adopt an incoming grantless role in its
    // first checkpoint too, not only at the final identity recording.
    on_server(&server, &format!("CREATE ROLE {}", role.1));
    std::fs::write(
        d.dir.join("schema/reader.yml"),
        format!("role: {}\n", role.1),
    )
    .unwrap();
    d.table(TWO_COLUMNS);
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--staged",
        "--out",
        plan.to_str().unwrap(),
    ]));
    succeeds(apply_plan(&d, connection, &plan, true));
    succeeds(d.run(&["verify", "--db", connection]));
    on_server(&server, &format!("DROP ROLE {}", role.1));
    let missing = d.run(&["verify", "--db", connection]);
    assert_eq!(
        code(&missing),
        2,
        "{}{}",
        stdout(&missing),
        stderr(&missing)
    );
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn completed_cluster_role_renames_are_pinned_and_recorded_without_role_sql() {
    struct Roles(String, Vec<String>);
    impl Drop for Roles {
        fn drop(&mut self) {
            for name in &self.1 {
                let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {name}"));
            }
        }
    }
    let server = server();
    let old = format!("pbps_old_{}", std::process::id());
    let new = format!("pbps_new_{}", std::process::id());
    let parked = format!("pbps_parked_{}", std::process::id());
    let _roles = Roles(
        server.clone(),
        vec![old.clone(), new.clone(), parked.clone()],
    );
    on_server(&server, &format!("CREATE ROLE {old}"));
    let own = OwnDatabase::new(&server, "role-rename");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("role-rename");
    d.table(ONE_COLUMN);
    let role_file = d.dir.join("schema/reader.yml");
    let declaration =
        |name: &str| format!("role: {name}\ngrants:\n  app.t: [select]\n  schema::app: [usage]\n");
    std::fs::write(&role_file, declaration(&old)).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    std::fs::write(&role_file, declaration(&new)).unwrap();
    succeeds(d.run(&["rename-role", &old, &new]));
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let planning = ["plan", "--db", connection, "--out", plan.to_str().unwrap()];
    let refused = |o: Output, remedy: &str, has_sql: bool| {
        assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
        assert!(stderr(&o).contains(remedy), "{}", stderr(&o));
        assert_eq!(stderr(&o).contains("ALTER ROLE"), has_sql, "{}", stderr(&o));
    };
    refused(d.run(&planning), "has not been run", true); // NotRunYet
    refuses_connected_check_json(&d, connection, "rename_evidence (PostgreSQL)", "ALTER ROLE");
    on_server(&server, &format!("CREATE ROLE {new}"));
    refused(d.run(&planning), "Resolve the name collision", false); // BothPresent
    on_server(&server, &format!("DROP ROLE {new}"));
    on_server(&server, &format!("ALTER ROLE {old} RENAME TO {parked}"));
    refused(d.run(&planning), "Restore the intended principal", false); // NeitherPresent
    on_server(&server, &format!("ALTER ROLE {parked} RENAME TO {new}"));
    let check = passed_connected_check_json(&d, connection, "rename_evidence");
    assert!(
        check["message"]
            .as_str()
            .unwrap()
            .starts_with("1 external cluster-role rename"),
        "{check}"
    );
    succeeds(d.run(&planning)); // Done; no SQL is needed to move the grants.
    let saved: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&plan).unwrap()).unwrap();
    assert!(saved.changes.is_empty());
    on_server(&server, &format!("CREATE ROLE {old}"));
    refused(
        apply_plan(&d, connection, &plan, false),
        "Resolve the name collision",
        false,
    );
    on_server(&server, &format!("DROP ROLE {old}"));
    succeeds(apply_plan(&d, connection, &plan, false));
    succeeds(d.run(&["verify", "--db", connection]));
    let listing = succeeds(d.run(&["state", "list", "--db", connection, "--format", "json"]));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stdout(&listing)).unwrap()["data"]["entries"][0]
            ["kind"],
        "apply"
    );

    // Done also covers drop-and-create: plan its missing grants (377), while
    // unrelated drift still refuses. Exercise the staged baseline as well.
    std::fs::write(&role_file, declaration(&old)).unwrap();
    succeeds(d.run(&["rename-role", &new, &old]));
    succeeds(d.run(&["plan"]));
    d.commit();
    on_server(connection, &format!("REVOKE SELECT ON app.t FROM {new}"));
    on_server(
        connection,
        &format!("REVOKE USAGE ON SCHEMA app FROM {new}"),
    );
    on_server(&server, &format!("DROP ROLE {new}"));
    on_server(&server, &format!("CREATE ROLE {old}"));
    on_server(connection, "ALTER TABLE app.t ADD COLUMN stray integer");
    let o = d.run(&planning);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("drifted"), "{}", stderr(&o));
    on_server(connection, "ALTER TABLE app.t DROP COLUMN stray");
    on_server(connection, &format!("GRANT USAGE ON SCHEMA app TO {old}"));
    succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--staged",
        "--out",
        plan.to_str().unwrap(),
    ]));
    succeeds(apply_plan(&d, connection, &plan, true));
    succeeds(d.run(&["verify", "--db", connection]));
    on_server(
        connection,
        &format!(
            "DO $$ BEGIN IF NOT has_table_privilege('{old}', 'app.t', 'SELECT') THEN RAISE EXCEPTION 'grant was not restored'; END IF; END $$"
        ),
    );
}

/// An unavailable rehearsal is not evidence that the PostgreSQL plan is
/// invalid, and must be refused before SQL Server setup sees either backend.
#[test]
fn postgres_rehearsals_refuse_before_connecting_or_starting_a_container() {
    let d = Demo::new("unsupported-dev");
    d.table(ONE_COLUMN);
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    d.commit();

    for backend in ["not-a-connection", "docker://not-an-image"] {
        let o = d.run(&["plan", "--dev", backend, "--format", "json"]);
        assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
        let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
        assert_eq!(v["findings"][0]["id"], "rehearsal.unavailable", "{v}");
        assert!(
            stdout(&o).contains("dev rehearsal is not supported for dialect `postgres`"),
            "{v}"
        );
    }

    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: postgres\ndev: {url_env: PBPS_UNSUPPORTED_DEV_TEST}\n",
    )
    .unwrap();
    let o = d.run_with_env(
        &["plan"],
        &[("PBPS_UNSUPPORTED_DEV_TEST", "not-a-connection")],
    );
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(
        stderr(&o).contains("dev rehearsal is not supported for dialect `postgres`"),
        "{}",
        stderr(&o)
    );

    // The configured backend must not prevent the existing read-only check.
    let o = d.run(&["plan", "--check"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
}

/// PostgreSQL keeps a check or foreign-key name per table (issue #496,
/// measured on 18.6), so the two tables SQL Server refuses in `flow.rs`'s
/// `two_tables_with_one_constraint_name_are_refused` validate here.
#[test]
fn two_tables_with_one_check_name_are_accepted() {
    let d = Demo::new("check-namespace");
    d.table("table: app.t1\ncolumns:\n  n: {type: integer}\nchecks:\n  c: n > 0\n");
    std::fs::write(
        d.dir.join("schema/app.t2.yml"),
        "table: app.t2\ncolumns:\n  n: {type: integer}\nchecks:\n  c: n > 0\n",
    )
    .unwrap();
    let o = d.run(&["validate"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
}

/// An index occupies the schema's relation namespace on PostgreSQL, alongside
/// tables and views — not a per-table namespace as on SQL Server — so two
/// tables in one schema declaring an index of the same name is refused before
/// it ever reaches the engine, the same shape as
/// `a_module_named_after_a_table_is_refused` in `flow.rs` but for
/// `Dialect::indexes_share_namespace_with_tables` (issue #176). This needs no
/// live server: `validate` is offline.
#[test]
fn two_tables_with_one_index_name_are_refused() {
    let d = Demo::new("index-namespace");
    d.table("table: app.t1\ncolumns:\n  n: {type: integer}\nindexes:\n  ix_n: {columns: [n]}\n");
    std::fs::write(
        d.dir.join("schema/app.t2.yml"),
        "table: app.t2\ncolumns:\n  n: {type: integer}\nindexes:\n  ix_n: {columns: [n]}\n",
    )
    .unwrap();
    let o = d.run(&["validate"]);
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(stderr(&o).contains("app.t1.ix_n"), "{}", stderr(&o));
    assert!(stderr(&o).contains("app.t2.ix_n"), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("one namespace per schema"),
        "{}",
        stderr(&o)
    );
    let o = d.run(&["validate", "--format", "json"]);
    assert!(
        stdout(&o).contains("schema.name-collision"),
        "{}",
        stdout(&o)
    );

    // A named unique constraint is backed by an index, so it collides too —
    // the review-widened half of issue #176 — while a check constraint of the
    // same name is not backed by one and is not in this namespace at all.
    std::fs::write(
        d.dir.join("schema/app.t3.yml"),
        "table: app.t3\ncolumns:\n  n: {type: integer}\nunique:\n  ix_n: [n]\n",
    )
    .unwrap();
    let o = d.run(&["validate", "--format", "json"]);
    let findings = json_output(o);
    let collisions = findings["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["id"] == "schema.name-collision")
        .count();
    assert!(collisions >= 2, "{findings}");

    std::fs::remove_file(d.dir.join("schema/app.t2.yml")).unwrap();
    std::fs::remove_file(d.dir.join("schema/app.t3.yml")).unwrap();
    std::fs::write(
        d.dir.join("schema/app.t2.yml"),
        "table: app.t2\ncolumns:\n  n: {type: integer}\nchecks:\n  t1: n > 0\n",
    )
    .unwrap();
    let o = d.run(&["validate", "--format", "json"]);
    assert!(
        !stdout(&o).contains("schema.name-collision"),
        "a check constraint has no backing index and is not in this namespace: {}",
        stdout(&o)
    );
}

/// One defect, one finding — through every gate that reads the declarations.
///
/// A named primary key and a unique constraint on one table sharing a name is
/// refused twice over: the table-local constraint-name rule calls it
/// `dialect.rejected`, and the schema's relation-namespace check, which holds
/// both of their backing indexes, called it `schema.name-collision` as well.
/// The declaration was correctly refused and the operator was handed the same
/// mistake twice, on `validate` and again on the connected gates that run the
/// same list (issue #498, DECISIONS 141).
///
/// The negative controls beside it are what the deduplication must not reach:
/// a check constraint sharing the name (no backing index, table-local only),
/// an index sharing it (not a constraint, namespace check only), and the same
/// two key constraints on two different tables, which no table-local rule can
/// see. Offline, like the collision tests above.
#[test]
fn a_primary_key_and_unique_constraint_sharing_a_name_are_one_finding_not_two() {
    let collisions = |o: std::process::Output| -> (usize, usize) {
        let findings = json_output(o);
        let of = |id: &str| {
            findings["findings"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|f| f["id"] == id)
                .count()
        };
        (of("dialect.rejected"), of("schema.name-collision"))
    };

    let d = Demo::new("key-name-dedup");
    d.table(
        "table: app.t\ncolumns:\n  num: {type: integer, nullable: false}\n\
         primary_key: {name: shared, columns: [num]}\nunique:\n  shared: [num]\n",
    );
    let o = d.run(&["validate", "--format", "json"]);
    assert_ne!(code(&o), 0, "the declaration is still refused");
    assert_eq!(
        collisions(o),
        (1, 0),
        "one defect must be one actionable finding"
    );

    // The connected gate reads the same list *before* it connects
    // (DECISIONS 141), so an unreachable database still reaches it — and it
    // counts the problems it would carry. One, not two.
    let o = d.run(&[
        "plan",
        "--db",
        "host=127.0.0.1 port=1 user=nobody dbname=nope",
        "--format",
        "json",
    ]);
    let reported = json_output(o);
    let message = reported["findings"][0]["message"].as_str().unwrap_or("");
    assert!(
        message.starts_with("the declarations have 1 problem(s)"),
        "the connected gate counts it once too: {reported}"
    );
    assert_eq!(
        message.matches("both named `shared`").count(),
        1,
        "{reported}"
    );

    // A check constraint of that name is not backed by an index, so it was
    // never the namespace check's and is unaffected.
    d.table(
        "table: app.t\ncolumns:\n  num: {type: integer, nullable: false}\n\
         primary_key: {name: shared, columns: [num]}\nchecks:\n  shared: num > 0\n",
    );
    assert_eq!(collisions(d.run(&["validate", "--format", "json"])), (1, 0));

    // An index of that name is not a constraint, so it was never the
    // table-local rule's — and it is still reported.
    d.table(
        "table: app.t\ncolumns:\n  num: {type: integer, nullable: false}\n\
         primary_key: {name: shared, columns: [num]}\nindexes:\n  shared: {columns: [num]}\n",
    );
    assert_eq!(collisions(d.run(&["validate", "--format", "json"])), (0, 1));

    // And the same two key constraints on two tables: no table-local rule can
    // see across tables, so this one belongs to the namespace check alone.
    d.table(
        "table: app.t\ncolumns:\n  num: {type: integer, nullable: false}\n\
         primary_key: {name: shared, columns: [num]}\n",
    );
    std::fs::write(
        d.dir.join("schema/app.t2.yml"),
        "table: app.t2\ncolumns:\n  num: {type: integer, nullable: false}\nunique:\n  shared: [num]\n",
    )
    .unwrap();
    assert_eq!(collisions(d.run(&["validate", "--format", "json"])), (0, 1));
}

/// The principal belongs to the cluster; bootstrap creates its managed grants
/// in this database, not the role itself (DECISIONS 211).
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
fn bootstrap_grants_to_an_existing_cluster_role_without_adopting_existing_grants() {
    struct Role {
        server: String,
        name: String,
    }
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.server, &format!("DROP ROLE {}", self.name));
        }
    }

    let server = server();
    let role = Role {
        server: server.clone(),
        name: format!("pbps_cli_bootstrap_reader_{}", std::process::id()),
    };
    on_server(&server, &format!("CREATE ROLE {}", role.name));
    // Declared after the role so the database (and its grants) drops first.
    let own = OwnDatabase::new(&server, "role-bootstrap");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");

    let d = Demo::new("role-bootstrap");
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("schema/reader.yml"),
        format!(
            "role: {}\ngrants:\n  app.t: [select]\n  schema::app: [usage]\n",
            role.name
        ),
    )
    .unwrap();
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    d.commit();

    // WITH GRANT OPTION is outside role.grants, but must still refuse before
    // the build and must not produce any ledger snapshot.
    on_server(
        connection,
        &format!(
            "GRANT USAGE ON SCHEMA app TO {} WITH GRANT OPTION",
            role.name
        ),
    );
    let o = d.run(&["bootstrap", "--db", connection]);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("WITH GRANT OPTION"), "{}", stderr(&o));
    on_server(
        connection,
        "DO $$ BEGIN IF to_regclass('app.t') IS NOT NULL OR EXISTS (SELECT 1 FROM public.__pbps_state) THEN RAISE EXCEPTION 'refused bootstrap wrote state'; END IF; END $$",
    );
    on_server(
        connection,
        &format!("REVOKE USAGE ON SCHEMA app FROM {}", role.name),
    );

    // Grants already in the managed set still make bootstrap refuse.
    on_server(
        connection,
        &format!("GRANT USAGE ON SCHEMA app TO {}", role.name),
    );
    let o = d.run(&["bootstrap", "--db", connection]);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("already has"), "{}", stderr(&o));
    on_server(
        connection,
        &format!("REVOKE USAGE ON SCHEMA app FROM {}", role.name),
    );

    // A single deployer's CREATE TABLE can trigger an unsupported grant.
    // The read-back must refuse and roll the whole build back, too.
    on_server(
        connection,
        &format!(
            "CREATE FUNCTION public.inject_grant() RETURNS event_trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF to_regclass('app.t') IS NOT NULL THEN \
         GRANT SELECT ON app.t TO {} WITH GRANT OPTION; END IF; END $$; \
         CREATE EVENT TRIGGER inject_grant ON ddl_command_end WHEN TAG IN ('CREATE TABLE') \
         EXECUTE FUNCTION public.inject_grant()",
            role.name
        ),
    );
    let o = d.run(&["bootstrap", "--db", connection]);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("WITH GRANT OPTION"), "{}", stderr(&o));
    on_server(
        connection,
        "DO $$ BEGIN IF to_regclass('app.t') IS NOT NULL THEN RAISE EXCEPTION 'refused bootstrap committed its table'; END IF; END $$",
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
            .await
            .unwrap();
        let latest = pbps_pg::state::latest(&mut conn).await.unwrap().unwrap();
        assert_eq!(latest.snapshot.kind, pbps_model::StateKind::Failed);
        assert!(latest.snapshot.schema.tables.is_empty());
        assert!(latest.snapshot.schema.roles.is_empty());
    });
    on_server(
        connection,
        "DROP EVENT TRIGGER inject_grant; DROP FUNCTION public.inject_grant()",
    );

    let o = d.run(&["bootstrap", "--db", connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    on_server(
        connection,
        &format!(
            "DO $$ BEGIN IF NOT has_table_privilege('{}', 'app.t', 'SELECT') OR \
         NOT has_schema_privilege('{}', 'app', 'USAGE') THEN \
         RAISE EXCEPTION 'bootstrap did not apply the declared grants'; END IF; END $$",
            role.name, role.name
        ),
    );
    let o = d.run(&["verify", "--db", connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    // The existing table must remain protected by the ordinary empty check.
    let o = d.run(&["bootstrap", "--db", connection]);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("already has"), "{}", stderr(&o));
}

/// The deployment loop, end to end, on PostgreSQL: an empty database is
/// bootstrapped, verified clean, planned against, applied to, verified clean
/// again, and its ledger listed. Every connected command the loop touches
/// routes through the seam, so a PostgreSQL arm that refused, or answered a
/// SQL Server shape, fails here by exit code.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
fn the_deployment_loop_runs_end_to_end_on_postgres() {
    deployment_loop(&server(), "loop");
}

#[test]
#[ignore = "needs PostgreSQL 16; set PBPS_TEST_PG_OLD_DB"]
fn the_deployment_loop_reads_constraints_before_postgres_18() {
    let server = std::env::var("PBPS_TEST_PG_OLD_DB").expect("PBPS_TEST_PG_OLD_DB");
    on_server(
        &server,
        "DO $$ BEGIN IF current_setting('server_version_num')::integer >= 180000 THEN RAISE EXCEPTION 'this regression needs a pre-18 server'; END IF; END $$",
    );
    deployment_loop(&server, "old-loop");
}

fn deployment_loop(server: &str, slug: &str) {
    let own = OwnDatabase::new(server, slug);
    let connection = own.connection().to_owned();
    // The tool never creates a schema (`doctor` says so); the environment has
    // to bring it, exactly as on the other engine.
    on_server(&connection, "CREATE SCHEMA app");

    let d = Demo::new(slug);
    d.table(ONE_COLUMN);
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    d.commit();

    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let o = d.run(&["verify", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert!(stdout(&o).contains("No drift"), "{}", stdout(&o));

    // One revision, planned against the database and applied through the
    // gate: a column added, and the plan's `--out` artifact is what `apply`
    // takes, checksum-pinned.
    d.table(TWO_COLUMNS);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let plan = d.dir.join("plan.json");
    let o = d.run(&["plan", "--db", &connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert!(stdout(&o).contains("label"), "{}", stdout(&o));
    let o = d.run(&[
        "apply",
        "--db",
        &connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
    ]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    let o = d.run(&["verify", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let o = d.run(&["plan", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert!(stdout(&o).contains("No changes"), "{}", stdout(&o));

    // The ledger, read back: the bootstrap and the apply, newest first.
    let o = d.run(&["state", "list", "--db", &connection, "--format", "json"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    let entries = v["data"]["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("{}", stdout(&o)));
    assert_eq!(entries.len(), 2, "{v}");
    assert_eq!(entries[0]["kind"], "apply", "{v}");
    assert_eq!(entries[1]["kind"], "bootstrap", "{v}");
    // Older servers lack the new flags, not constraints themselves. The
    // fallback must still observe an independently added ordinary CHECK.
    on_server(
        &connection,
        "ALTER TABLE app.t ADD CONSTRAINT positive_id CHECK (id > 0)",
    );
    let drift = d.run(&["verify", "--db", &connection]);
    assert_ne!(code(&drift), 0, "{}{}", stdout(&drift), stderr(&drift));
    assert!(
        stdout(&drift).contains("positive_id"),
        "{}{}",
        stdout(&drift),
        stderr(&drift)
    );
}

/// Drift is drift on this engine too: a column added by hand is reported,
/// with the exit code a scheduled drift-watch keys on (DECISIONS 25), and a
/// `pull` reads the same database back into declarations that validate.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
fn drift_and_pull_are_read_from_the_postgres_catalog() {
    let server = server();
    let own = OwnDatabase::new(&server, "drift");
    let connection = own.connection().to_owned();
    on_server(&connection, "CREATE SCHEMA app");

    let d = Demo::new("drift");
    d.table(ONE_COLUMN);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let o = d.run(&["bootstrap", "--db", &connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));

    on_server(&connection, "ALTER TABLE app.t ADD COLUMN stray integer");
    let o = d.run(&["verify", "--db", &connection, "--format", "json"]);
    assert_eq!(code(&o), 2, "{}{}", stdout(&o), stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    assert_eq!(v["result"], "findings", "{v}");
    assert!(stdout(&o).contains("stray"), "{}", stdout(&o));

    // `pull --force` overwrites the declarations with what the catalog holds,
    // and what it writes must validate under the same dialect.
    let o = d.run(&["pull", "--db", &connection, "--force"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let pulled = std::fs::read_to_string(d.dir.join("schema/app.t.yml")).unwrap();
    assert!(pulled.contains("stray"), "{pulled}");
    let o = d.run(&["validate"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
}

/// `doctor` examines a PostgreSQL environment through the seam: the version
/// is read, the engine has one edition so none is reported, the ledger is
/// found, and a schema the declarations use but the database lacks is the
/// error it is on the other engine — with the remedy quoted this engine's
/// way.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
fn doctor_examines_a_postgres_environment() {
    let server = server();
    let own = OwnDatabase::new(&server, "doctor");
    let connection = own.connection().to_owned();

    let d = Demo::new("doctor");
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: postgres\nenvironments:\n  dev:\n    url_env: PBPS_FLOW_PG_DEV\n",
    )
    .unwrap();
    d.table(ONE_COLUMN);
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    let o = d.run_with_env(
        &["doctor", "--format", "json"],
        &[("PBPS_FLOW_PG_DEV", connection.as_str())],
    );
    let v: serde_json::Value = serde_json::from_str(&stdout(&o))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&o)));
    let env = &v["data"]["environments"][0];
    assert_eq!(env["state"], "uninitialized", "{v}");
    assert!(
        env["server_version"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "{v}"
    );
    assert!(env["edition"].is_null(), "one edition, no line: {v}");
    let ids: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["id"].as_str())
        .collect();
    assert!(ids.contains(&"schema.absent"), "{v}");
    let absent = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "schema.absent")
        .unwrap();
    assert_eq!(absent["remedy"], "CREATE SCHEMA \"app\";", "{v}");
    assert_eq!(code(&o), 2, "{}{}", stdout(&o), stderr(&o));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
fn ledger_migration_guidance_keeps_the_explicit_or_configured_target() {
    struct Role(String, String);
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE {}", self.1));
        }
    }
    let server = server();
    let role = Role(
        server.clone(),
        format!("pbps_target444_{}", std::process::id()),
    );
    let password = "pbpsTarget444!1";
    let own = OwnDatabase::new(&server, "migration_target444");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, own.connection()).await.unwrap();
        pbps_pg::state::ensure_tables(&mut conn).await.unwrap();
        conn.execute("ALTER TABLE public.__pbps_state DROP COLUMN state_version, DROP COLUMN tables_count, DROP COLUMN modules_count, DROP COLUMN staged_completed, DROP COLUMN staged_total").await.unwrap();
    });
    on_server(
        own.connection(),
        &format!(
            "CREATE ROLE {0} LOGIN PASSWORD '{password}'; GRANT USAGE ON SCHEMA public TO {0}; GRANT SELECT, INSERT, DELETE ON public.__pbps_state, public.__pbps_lock TO {0};",
            role.1
        ),
    );
    let connection = format!(
        "{} user={} password={password}",
        own.connection()
            .split_whitespace()
            .filter(|w| !w.starts_with("user=") && !w.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" "),
        role.1
    );
    let d = Demo::new("migration-target444");
    succeeds(d.run(&["plan"]));
    d.commit();
    // The first refusal has no configured environment that bare doctor could inspect.
    for configured in [false, true] {
        let output = if configured {
            std::fs::write(d.dir.join("pbps.yml"), "dialect: postgres\nenvironments:\n  target:\n    url_env: PBPS_MIGRATION_TARGET444\n").unwrap();
            d.commit();
            d.run_with_env(
                &["bootstrap", "--env", "target"],
                &[("PBPS_MIGRATION_TARGET444", &connection)],
            )
        } else {
            d.run(&["bootstrap", "--db", &connection])
        };
        let message = format!("{}{}", stdout(&output), stderr(&output));
        assert_eq!(code(&output), 1, "{message}");
        assert!(
            message.contains("public.__pbps_state is missing the timeline columns"),
            "{message}"
        );
        assert!(
            message.contains("state_version") && message.contains("staged_total"),
            "{message}"
        );
        assert!(
            message.contains("needs ownership of public.__pbps_state"),
            "{message}"
        );
        // Before issue #167 this seam rendered the ownership refusal as the
        // literal `db error`; the server's own sentence is what a reader
        // needs beside the guidance the assertion above already checks.
        assert!(
            message.contains("this role could not add them.")
                && message.contains("must be owner of table __pbps_state"),
            "{message}"
        );
        assert!(
            message.contains(
                "Run `pbps doctor` with the same `--db` or `--env` target as the failed command"
            ),
            "{message}"
        );
        assert!(
            !message.contains(password) && !message.contains(&connection),
            "credentials must stay out of guidance"
        );
    }
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn doctor_reports_data_and_role_grant_gaps_from_the_declarations() {
    struct Roles(String, Vec<String>);
    impl Drop for Roles {
        fn drop(&mut self) {
            for role in &self.1 {
                let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {role}"));
            }
        }
    }
    let server = server();
    let roles = Roles(
        server.clone(),
        vec![
            format!("pbps_doctor_deployer_{}", std::process::id()),
            format!("pbps_doctor_reader_{}", std::process::id()),
        ],
    );
    let deployer = &roles.1[0];
    let reader = &roles.1[1];
    let own = OwnDatabase::new(&server, "doctor-demand");
    let connection = own.connection();
    on_server(
        connection,
        &format!(
            "CREATE ROLE {deployer} LOGIN PASSWORD 'doctor-test'; CREATE ROLE {reader}; \
         CREATE SCHEMA app AUTHORIZATION {deployer}; \
         CREATE TABLE app.t (id bigint NOT NULL PRIMARY KEY); \
         ALTER TABLE app.t OWNER TO {deployer}; \
         REVOKE INSERT ON app.t FROM {deployer}; \
         CREATE SCHEMA shared; CREATE VIEW shared.v AS SELECT 1 AS id; \
         CREATE FUNCTION shared.f(integer) RETURNS integer LANGUAGE sql AS 'SELECT $1'; \
         CREATE FUNCTION shared.f(text) RETURNS text LANGUAGE sql AS 'SELECT $1'; \
         GRANT USAGE ON SCHEMA shared TO {deployer}; \
         GRANT SELECT ON shared.v TO {deployer}; \
         GRANT EXECUTE ON FUNCTION shared.f(text) TO {deployer} WITH GRANT OPTION; \
         GRANT CREATE ON SCHEMA public TO {deployer}"
        ),
    );
    let login = format!(
        "{} user={deployer} password=doctor-test",
        connection
            .split_whitespace()
            .filter(|word| !word.starts_with("user=") && !word.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let d = Demo::new("doctor-demand");
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: postgres\nenvironments:\n  dev:\n    url_env: PBPS_FLOW_PG_DEV\n",
    )
    .unwrap();
    d.table(&format!(
        "{ONE_COLUMN}data:\n  mode: ensure\n  rows:\n    1: {{}}\n"
    ));
    std::fs::write(d.dir.join("schema/reader.yml"), format!(
        "role: {reader}\ngrants:\n  schema::shared: [usage]\n  shared.v: [select]\n  shared.f(integer): [execute]\n"
    )).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let diagnose = || {
        let output = d.run_with_env(
            &["doctor", "--format", "json"],
            &[("PBPS_FLOW_PG_DEV", login.as_str())],
        );
        let json: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
        (output, json)
    };
    let (output, json) = diagnose();
    assert_eq!(code(&output), 2, "{json}");
    let gaps = json["data"]["environments"][0]["missing_permissions"]
        .as_array()
        .unwrap();
    for expected in [
        "INSERT on TABLE \"app\".\"t\"",
        "USAGE WITH GRANT OPTION on SCHEMA \"shared\"",
        "SELECT WITH GRANT OPTION on TABLE \"shared\".\"v\"",
        "EXECUTE WITH GRANT OPTION on ROUTINE \"shared\".\"f\"(integer)",
    ] {
        assert!(
            gaps.iter()
                .any(|g| g.as_str().unwrap().starts_with(expected)),
            "missing {expected}: {json}"
        );
    }
    assert!(
        !gaps
            .iter()
            .any(|g| g.as_str().unwrap().starts_with("DELETE")
                || g.as_str().unwrap().starts_with("UPDATE")),
        "a key-only ensure block never updates or deletes: {json}"
    );
    on_server(
        connection,
        &format!(
            "GRANT INSERT (id) ON app.t TO {deployer}; \
         GRANT USAGE ON SCHEMA shared TO {deployer} WITH GRANT OPTION; \
         GRANT SELECT ON shared.v TO {deployer} WITH GRANT OPTION; \
         GRANT EXECUTE ON FUNCTION shared.f(integer) TO {deployer} WITH GRANT OPTION"
        ),
    );
    let (_, json) = diagnose();
    assert_eq!(
        json["data"]["environments"][0]["missing_permissions"],
        serde_json::json!([]),
        "the named remedies remove every permission gap: {json}"
    );
}

/// #566: doctor keys a declared routine the way the dialect spells it, as
/// the planner does. A managed `app.f(integer)` whose ACL grants `EXECUTE` to
/// a declared role, granted by somebody this deployer cannot act for, needs
/// the grant option to be managed. Declared as `app.f(int)`, the file's
/// spelling matched nothing the catalog reads back, the adopted ACL was set
/// aside as unmanaged, and doctor reported no gap at all. Both spellings now
/// report the same one.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn doctor_reports_an_adopted_routine_grant_whatever_the_argument_spelling() {
    for spelling in ["int", "integer"] {
        let server = server();
        let deployer = format!("pbps_d566_dep_{}_{spelling}", std::process::id());
        let reader = format!("pbps_d566_read_{}_{spelling}", std::process::id());
        let _roles = ClusterRoles {
            server: server.clone(),
            names: vec![deployer.clone(), reader.clone()],
        };
        let own = OwnDatabase::new(&server, &format!("doctor-566-{spelling}"));
        let connection = own.connection();
        on_server(
            connection,
            &format!(
                "CREATE ROLE {deployer} LOGIN PASSWORD 'doctor-test'; CREATE ROLE {reader}; \
                 CREATE SCHEMA app AUTHORIZATION {deployer}; \
                 GRANT CREATE ON SCHEMA public TO {deployer}; \
                 CREATE FUNCTION app.f(integer) RETURNS integer LANGUAGE sql AS 'SELECT $1'; \
                 GRANT EXECUTE ON FUNCTION app.f(integer) TO {reader}"
            ),
        );
        let login = format!(
            "{} user={deployer} password=doctor-test",
            connection
                .split_whitespace()
                .filter(|word| !word.starts_with("user=") && !word.starts_with("password="))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let d = Demo::new(&format!("doctor-566-{spelling}"));
        std::fs::write(
            d.dir.join("pbps.yml"),
            "dialect: postgres\nenvironments:\n  dev:\n    url_env: PBPS_FLOW_PG_DEV\n",
        )
        .unwrap();
        std::fs::write(
            d.dir.join("schema/f.yml"),
            format!(
                "function: app.f({spelling})\n\
                 definition: (n {spelling}) RETURNS integer LANGUAGE sql AS 'SELECT n'\n"
            ),
        )
        .unwrap();
        std::fs::write(d.dir.join("schema/reader.yml"), format!("role: {reader}\n")).unwrap();
        succeeds(d.run(&["plan"]));
        d.commit();
        let output = d.run_with_env(
            &["doctor", "--format", "json"],
            &[("PBPS_FLOW_PG_DEV", login.as_str())],
        );
        let json: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
        assert_eq!(code(&output), 2, "{spelling}: {json}");
        let gaps = json["data"]["environments"][0]["missing_permissions"]
            .as_array()
            .unwrap();
        assert!(
            gaps.iter().any(|g| g
                .as_str()
                .unwrap()
                .starts_with("EXECUTE WITH GRANT OPTION on ROUTINE \"app\".\"f\"(integer)")),
            "{spelling}: {json}"
        );
    }
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn arriving_overloads_rebind_unchanged_callers_in_the_approved_plan() {
    let server = server();
    let own = OwnDatabase::new(&server, "overload-rebind");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("overload-rebind");
    std::fs::write(d.dir.join("schema/f-bigint.yml"),
        "function: app.f(bigint)\ndefinition: (n bigint) RETURNS text LANGUAGE sql AS $$ SELECT 'old' $$\n").unwrap();
    std::fs::write(d.dir.join("schema/caller.yml"),
        "function: app.caller()\ndefinition: () RETURNS text LANGUAGE sql BEGIN ATOMIC SELECT f(1); END\n").unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    on_server(
        connection,
        "DO $$ BEGIN IF app.caller() <> 'old' THEN RAISE EXCEPTION 'wrong initial binding'; END IF; END $$",
    );

    std::fs::write(d.dir.join("schema/f-integer.yml"),
        "function: app.f(integer)\ndefinition: (n integer) RETURNS text LANGUAGE sql AS $$ SELECT 'new' $$\n").unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    let planning = ["plan", "--db", connection, "--out", plan.to_str().unwrap()];
    succeeds(d.run(&planning));
    // Run the real plan before inspecting its shape: an omitted rebuild must
    // fail on the engine's observed binding, not merely a mirrored assertion.
    //
    // `--allow revoke` because the synthesized rebuild of `app.caller()` is a
    // drop and a create on this engine, and the create restores the default
    // `EXECUTE` to `PUBLIC` that the plan then takes away again (issue #318):
    // the routine was there before, so the gate is asked.
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "revoke"],
    ));
    on_server(
        connection,
        "DO $$ BEGIN IF app.caller() <> 'new' THEN RAISE EXCEPTION 'successful apply retained the old overload binding'; END IF; END $$",
    );
    let saved: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&plan).unwrap()).unwrap();
    let rebuilt: Vec<_> = saved
        .changes
        .changes
        .iter()
        .filter_map(|p| {
            if let pbps_model::Change::AlterModule { id, .. } = &p.change {
                Some(id.to_string())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(rebuilt, ["app.caller()"]);
    succeeds(d.run(&["verify", "--db", connection]));
    let next = succeeds(d.run(&planning));
    assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));

    // A second new signature still reaches the ordinary carried-state guard.
    // The scanner is intentionally conservative about overload applicability.
    std::fs::write(d.dir.join("schema/f-smallint.yml"),
        "function: app.f(smallint)\ndefinition: (n smallint) RETURNS text LANGUAGE sql AS $$ SELECT 'small' $$\n").unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    on_server(connection, "COMMENT ON FUNCTION app.caller() IS 'keep me'");
    let refused = d.run(&planning);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(stderr(&refused).contains("comment"), "{}", stderr(&refused));
    on_server(connection, "COMMENT ON FUNCTION app.caller() IS NULL");
    succeeds(d.run(&planning));
    let staged = d.run(&[
        "plan",
        "--db",
        connection,
        "--staged",
        "--out",
        plan.to_str().unwrap(),
    ]);
    assert_eq!(code(&staged), 1, "{}{}", stdout(&staged), stderr(&staged));
    assert!(
        stderr(&staged).contains("require a transaction"),
        "{}",
        stderr(&staged)
    );
    on_server(
        connection,
        "COMMENT ON FUNCTION app.caller() IS 'arrived after planning'",
    );
    // Approved, so that the refusal under test is the comment and not the
    // rebuild's own `revoke` risk (issue #318) — an unapproved plan stops at
    // the gate before it ever asks the catalog.
    let refused = approved_apply(&d, connection, &plan, &["--allow", "revoke"]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(stderr(&refused).contains("comment"), "{}", stderr(&refused));
    on_server(
        connection,
        "DO $$ BEGIN IF to_regprocedure('app.f(smallint)') IS NOT NULL OR app.caller() <> 'new' THEN RAISE EXCEPTION 'refused rebuild changed the database'; END IF; END $$",
    );
}

fn omission_policy(d: &Demo, policy: &str) {
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!("dialect: postgres\nunmanaged: {policy}\nenvironments:\n  dev:\n    url_env: PBPS_OMISSION_DB\n"),
    )
    .unwrap();
    d.commit();
}

fn omission_report(d: &Demo, connection: &str, command: &str) -> serde_json::Value {
    let args = if command == "verify" {
        vec!["verify", "--db", connection, "--format", "json"]
    } else {
        vec!["status", "--format", "json"]
    };
    let result = d.run_with_env(&args, &[("PBPS_OMISSION_DB", connection)]);
    let report: serde_json::Value = serde_json::from_str(&stdout(&result)).unwrap();
    assert_eq!(
        code(&result),
        if command == "verify" { 2 } else { 0 },
        "{report}: {}",
        stderr(&result)
    );
    if command == "verify" {
        let facts = report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["id"] == "state.drift-unexpressible")
            .collect::<Vec<_>>();
        assert_eq!(facts.len(), 1, "{report}");
        assert!(
            facts[0]["message"]
                .as_str()
                .unwrap()
                .contains("which this model does not hold"),
            "{report}"
        );
    } else {
        let row = &report["data"][0];
        assert_eq!(row["state"], "drift", "{report}");
        let detail = row["detail"].as_str().unwrap();
        assert!(
            detail.contains("1 fact(s) inside the managed set"),
            "{report}"
        );
        let managed = detail
            .split(" — ")
            .find(|part| part.contains("fact(s) inside the managed set"))
            .unwrap();
        assert_eq!(
            managed.matches("which this model does not hold").count(),
            1,
            "{report}"
        );
    }
    report
}

fn omitted_state_cannot_be_recorded(d: &Demo, connection: &str) {
    // Ignore unrelated cluster roles here so only the managed omission can
    // explain the refusal. No command may replace the previous good snapshot.
    omission_policy(d, "ignore");
    let before = scalar(connection, "SELECT count(*) FROM public.__pbps_state");
    for args in [
        vec!["plan", "--db", connection],
        vec![
            "baseline",
            "--db",
            connection,
            "--reason",
            "must refuse omission",
        ],
        vec!["snapshot", "--db", connection, "--force"],
    ] {
        let result = d.run(&args);
        assert_eq!(code(&result), 1, "{}{}", stdout(&result), stderr(&result));
        assert!(
            stderr(&result).contains("1 fact(s) inside the managed set cannot be represented"),
            "{}",
            stderr(&result)
        );
        assert_eq!(
            scalar(connection, "SELECT count(*) FROM public.__pbps_state"),
            before
        );
    }
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn managed_omission_reports_each_catalog_fact_once_and_keeps_recording_refusals() {
    let own = OwnDatabase::new(&server(), "omission-facts");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("omission-facts");
    d.table(ONE_COLUMN);
    std::fs::write(d.dir.join("schema/f.yml"), "function: app.f(integer)\ndefinition: (n integer) RETURNS integer LANGUAGE sql AS $$ SELECT n $$\n").unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    succeeds(d.run(&["verify", "--db", connection]));
    on_server(
        connection,
        "DROP FUNCTION app.f(integer); CREATE AGGREGATE app.f(integer) (SFUNC = int4pl, STYPE = integer, INITCOND = '0')",
    );
    for policy in ["warn", "error"] {
        omission_policy(&d, policy);
        for command in ["verify", "status"] {
            let report = omission_report(&d, connection, command);
            assert!(
                !report.to_string().contains("aggregate app.f(integer) ("),
                "{report}"
            );
        }
    }
    omitted_state_cannot_be_recorded(&d, connection);
    on_server(
        connection,
        "DROP AGGREGATE app.f(integer); CREATE FUNCTION app.f(n integer) RETURNS integer LANGUAGE sql AS $$ SELECT n $$",
    );
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn managed_omission_relations_stay_out_of_unmanaged_policy_with_absent_tables() {
    let own = OwnDatabase::new(&server(), "omission-relations");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("omission-relations");
    d.table(ONE_COLUMN);
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    succeeds(d.run(&["verify", "--db", connection]));
    on_server(
        connection,
        "DROP TABLE app.t; CREATE MATERIALIZED VIEW app.t AS SELECT 1::bigint AS id",
    );
    for has_unmanaged in [false, true] {
        if has_unmanaged {
            on_server(
                connection,
                "CREATE MATERIALIZED VIEW app.other AS SELECT 1 AS id; CREATE AGGREGATE app.t(integer) (SFUNC = int4pl, STYPE = integer, INITCOND = '0')",
            );
        }
        for policy in ["warn", "error"] {
            omission_policy(&d, policy);
            for command in ["verify", "status"] {
                let report = omission_report(&d, connection, command);
                let rendered = report.to_string();
                assert!(!rendered.contains("materialized view app.t ("), "{report}");
                // Table identity must not hide an unrelated relation or the
                // same-named routine, even when their kind is also omitted.
                for name in [
                    "materialized view app.other (",
                    "aggregate app.t(integer) (",
                ] {
                    assert_eq!(rendered.contains(name), has_unmanaged, "{report}");
                }
            }
        }
    }
    omitted_state_cannot_be_recorded(&d, connection);
    on_server(
        connection,
        "DROP MATERIALIZED VIEW app.t; DROP MATERIALIZED VIEW app.other; DROP AGGREGATE app.t(integer); CREATE TABLE app.t (id bigint NOT NULL, CONSTRAINT pk_t PRIMARY KEY (id))",
    );
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn unrelated_routine_limitations_do_not_refuse_a_managed_table_or_overload() {
    let own = OwnDatabase::new(&server(), "limitation-scope");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("limitation-scope");
    d.table(ONE_COLUMN);
    std::fs::write(d.dir.join("schema/f.yml"), "function: app.t(integer)\ndefinition: (n integer) RETURNS integer LANGUAGE sql AS $$ SELECT n $$\n").unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    on_server(
        connection,
        "CREATE AGGREGATE app.t(bigint) (SFUNC = int8pl, STYPE = bigint, INITCOND = '0')",
    );
    succeeds(d.run(&["plan", "--db", connection]));
    succeeds(d.run(&["snapshot", "--db", connection, "--force"]));
    succeeds(d.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "accept managed state",
    ]));
    succeeds(d.run(&["verify", "--db", connection]));

    // A real table limitation must still refuse the same managed scope.
    on_server(connection, "ALTER TABLE app.t SET UNLOGGED");
    let refused = d.run(&["plan", "--db", connection]);
    assert_ne!(code(&refused), 0);
    assert!(
        stderr(&refused).contains("UNLOGGED"),
        "{}",
        stderr(&refused)
    );
    on_server(connection, "ALTER TABLE app.t SET LOGGED");

    // The exact managed signature becoming unsupported is also a limitation.
    on_server(
        connection,
        "DROP FUNCTION app.t(integer); CREATE AGGREGATE app.t(integer) (SFUNC = int4pl, STYPE = integer, INITCOND = '0')",
    );
    let refused = d.run(&["baseline", "--db", connection, "--reason", "must refuse"]);
    assert_ne!(code(&refused), 0);
    assert!(
        stderr(&refused).contains("aggregate"),
        "{}",
        stderr(&refused)
    );
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn omitted_modules_obey_unmanaged_policy_even_beside_a_managed_overload() {
    struct Role(String, String);
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let assert_omitted_modules = |result: &Output, present: bool| {
        let message = stderr(result);
        for object in ["aggregate app.t(bigint)", "view app.bad(int)"] {
            assert_eq!(message.contains(object), present, "{message}");
        }
        assert!(!message.contains("function app.t(integer)"), "{message}");
        // An unrelated cluster role may independently refuse the plan, but
        // connection, snapshot, or planning errors must never count as clean.
        if message.contains("`unmanaged: error`") {
            assert_ne!(code(result), 0, "{message}");
        } else {
            assert_eq!(code(result), 0, "{message}");
        }
    };
    let own = OwnDatabase::new(&server(), "unmanaged-modules");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("unmanaged-modules");
    d.table(ONE_COLUMN);
    std::fs::write(d.dir.join("schema/f.yml"), "function: app.t(integer)\ndefinition: (n integer) RETURNS integer LANGUAGE sql AS $$ SELECT n $$\n").unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    // Roles are cluster-wide, so compare only this test's objects rather than
    // assuming an own database means a stable unmanaged inventory.
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: postgres\nunmanaged: error\n",
    )
    .unwrap();
    d.commit();
    let control = d.run(&["plan", "--db", connection]);
    assert_omitted_modules(&control, false);
    let role = Role(
        connection.to_owned(),
        format!("pbps_unmanaged_churn_{}", std::process::id()),
    );
    on_server(connection, &format!("CREATE ROLE {}", role.1));

    on_server(
        connection,
        "CREATE AGGREGATE app.t(bigint) (SFUNC = int8pl, STYPE = bigint, INITCOND = '0'); CREATE VIEW app.\"bad(int)\" AS SELECT id FROM app.t",
    );

    for policy in ["ignore", "warn", "error"] {
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!("dialect: postgres\nunmanaged: {policy}\n"),
        )
        .unwrap();
        d.commit();
        let result = d.run(&["plan", "--db", connection]);
        if policy == "error" {
            assert_omitted_modules(&result, true);
            assert_ne!(code(&result), 0, "{}", stdout(&result));
            let message = stderr(&result);
            assert!(message.contains("`unmanaged: error`"), "{message}");
            assert!(message.contains("aggregate app.t(bigint)"), "{message}");
            assert!(message.contains("view app.bad(int)"), "{message}");
            let verified = d.run(&["verify", "--db", connection, "--format", "json"]);
            let report: serde_json::Value = serde_json::from_str(&stdout(&verified)).unwrap();
            assert_eq!(code(&verified), 2, "{report}: {}", stderr(&verified));
            assert!(
                report["findings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|finding| {
                        finding["id"] == "state.unmanaged-refused"
                            && finding["message"].as_str().is_some_and(|message| {
                                message.contains("aggregate app.t(bigint)")
                                    && message.contains("view app.bad(int)")
                            })
                    }),
                "{report}"
            );
        } else {
            let message = stderr(&result);
            assert_eq!(
                message.contains("are not declared and are left alone"),
                policy == "warn",
                "{message}"
            );
            succeeds(result);
        }
    }
    // Removing only our omitted modules restores the original policy answer;
    // the same-name managed function must not become an unmanaged object.
    on_server(
        connection,
        "DROP AGGREGATE app.t(bigint); DROP VIEW app.\"bad(int)\"",
    );
    let restored = d.run(&["plan", "--db", connection]);
    assert!(stderr(&restored).contains(&role.1), "{}", stderr(&restored));
    assert_omitted_modules(&restored, false);
    let role_name = role.1.clone();
    drop(role);
    let after_role_drop = d.run(&["plan", "--db", connection]);
    assert!(
        !stderr(&after_role_drop).contains(&role_name),
        "{}",
        stderr(&after_role_drop)
    );
    assert_omitted_modules(&after_role_drop, false);
}

#[test]
#[ignore = "needs PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn permission_versions_are_checked_before_planning_bootstrap_apply_and_resume() {
    struct Roles(Vec<(String, String)>);
    impl Drop for Roles {
        fn drop(&mut self) {
            for (server, name) in &self.0 {
                let _ = try_on_server(server, &format!("DROP ROLE IF EXISTS {name}"));
            }
        }
    }
    let old_server = std::env::var("PBPS_TEST_PG_OLD_DB").expect("PBPS_TEST_PG_OLD_DB");
    let new_server = server();
    let role = format!("pbps_version_{}", std::process::id());
    let _roles = Roles(vec![
        (old_server.clone(), role.clone()),
        (new_server.clone(), role.clone()),
    ]);
    for server in [&old_server, &new_server] {
        on_server(server, &format!("CREATE ROLE {role}"));
    }
    on_server(
        &old_server,
        "DO $$ BEGIN IF current_setting('server_version_num')::integer >= 170000 THEN RAISE EXCEPTION 'needs pre-17'; END IF; END $$",
    );
    on_server(
        &new_server,
        "DO $$ BEGIN IF current_setting('server_version_num')::integer < 170000 THEN RAISE EXCEPTION 'needs 17+'; END IF; END $$",
    );
    let old = OwnDatabase::new(&old_server, "permission_version");
    let new = OwnDatabase::new(&new_server, "permission_version");
    let bootstrap_target = OwnDatabase::new(&old_server, "permission_bootstrap");
    for db in [&old, &new, &bootstrap_target] {
        on_server(db.connection(), "CREATE SCHEMA app");
    }
    let d = Demo::new("permission-version");
    d.table(ONE_COLUMN);
    let role_file = d.dir.join("schema/reader.yml");
    let declare = |permissions: &str| {
        std::fs::write(
            &role_file,
            format!("role: {role}\ngrants:\n  app.t: [{permissions}]\n  schema::app: [usage]\n"),
        )
        .unwrap();
        succeeds(d.run(&["plan"]));
        d.commit();
    };
    declare("select");
    for db in [&old, &new] {
        succeeds(d.run(&["bootstrap", "--db", db.connection()]));
    }
    declare("select, maintain");
    let refuses_version = |o: Output| {
        assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
        assert!(
            stderr(&o).contains("permission_support (PostgreSQL)"),
            "{}",
            stderr(&o)
        );
        assert!(
            stderr(&o).contains("PostgreSQL 17 or later"),
            "{}",
            stderr(&o)
        );
    };
    refuses_version(d.run(&["bootstrap", "--db", bootstrap_target.connection()]));
    on_server(
        bootstrap_target.connection(),
        "DO $$ BEGIN IF to_regclass('app.t') IS NOT NULL OR to_regclass('public.__pbps_state') IS NOT NULL THEN RAISE EXCEPTION 'bootstrap wrote before refusing'; END IF; END $$",
    );

    let plan = d.dir.join("version-plan.json");
    let o = d.run(&[
        "plan",
        "--db",
        old.connection(),
        "--out",
        plan.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert_eq!(code(&o), 1, "{}{}", stdout(&o), stderr(&o));
    let report: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(report["result"], "unanswerable", "{report}");
    assert!(
        report["findings"][0]["message"]
            .as_str()
            .unwrap()
            .contains("permission_support (PostgreSQL)"),
        "{report}"
    );
    assert!(
        !plan.exists(),
        "refused planning must not write an artifact"
    );
    let o = succeeds(d.run(&[
        "plan",
        "--db",
        new.connection(),
        "--out",
        plan.to_str().unwrap(),
        "--format",
        "json",
    ]));
    let report: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(
        report["data"]["connected_checks"][0]["name"],
        "permission_support"
    );
    assert_eq!(report["data"]["connected_checks"][0]["status"], "passed");
    assert_eq!(
        report["data"]["connected_checks"][0]["engine"],
        "PostgreSQL"
    );
    refuses_version(apply_plan(&d, old.connection(), &plan, false));
    let staged = d.dir.join("version-staged.json");
    succeeds(d.run(&[
        "plan",
        "--db",
        new.connection(),
        "--out",
        staged.to_str().unwrap(),
        "--staged",
        "--format",
        "json",
    ]));
    refuses_version(apply_plan(&d, old.connection(), &staged, true));
    refuses_version(d.run(&[
        "apply",
        "--db",
        old.connection(),
        "--plan",
        staged.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&staged),
        "--staged",
        "--resume",
    ]));
    let listing = succeeds(d.run(&[
        "state",
        "list",
        "--db",
        old.connection(),
        "--format",
        "json",
    ]));
    let listing: serde_json::Value = serde_json::from_str(&stdout(&listing)).unwrap();
    assert_eq!(
        listing["data"]["entries"].as_array().unwrap().len(),
        1,
        "a capability refusal must not write a ledger entry: {listing}"
    );
    succeeds(apply_plan(&d, new.connection(), &plan, false));
    on_server(
        new.connection(),
        &format!(
            "DO $$ BEGIN IF NOT has_table_privilege('{role}', 'app.t', 'MAINTAIN') THEN RAISE EXCEPTION 'grant missing'; END IF; END $$"
        ),
    );

    // Reference-data adoption also remains a single JSON document, carrying
    // the notice that the human plan prints before its summary.
    d.table(&format!(
        "{ONE_COLUMN}data:\n  mode: ensure\n  rows:\n    1: {{}}\n"
    ));
    succeeds(d.run(&["plan"]));
    d.commit();
    let adoption = succeeds(d.run(&["plan", "--db", new.connection(), "--format", "json"]));
    let adoption: serde_json::Value = serde_json::from_str(&stdout(&adoption)).unwrap();
    assert!(
        adoption["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "data.adoption"),
        "{adoption}"
    );
    d.table(ONE_COLUMN);

    // A revoke-only plan still works on the old server. The guard is about
    // permissions the plan grants, not permissions mentioned in a declaration.
    std::fs::write(&role_file, format!("role: {role}\n")).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&[
        "plan",
        "--db",
        old.connection(),
        "--out",
        plan.to_str().unwrap(),
        "--format",
        "json",
    ]));
    let saved: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&plan).unwrap()).unwrap();
    assert!(
        saved
            .changes
            .changes
            .iter()
            .all(|p| matches!(p.change, pbps_model::Change::Revoke { .. }))
    );
    succeeds(d.run(&[
        "apply",
        "--db",
        old.connection(),
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&plan),
        "--allow",
        "revoke",
    ]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn drop_blockers_are_named_in_planning_and_rechecked_before_apply_writes() {
    for (target, slug) in [("app.t.label", "drop_column"), ("app.t", "drop_table")] {
        let db = OwnDatabase::new(&server(), slug);
        let connection = db.connection();
        on_server(connection, "CREATE SCHEMA app");
        let d = Demo::new(slug);
        d.table(TWO_COLUMNS);
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", connection]));
        if target == "app.t" {
            std::fs::remove_file(d.dir.join("schema/app.t.yml")).unwrap();
        } else {
            d.table(ONE_COLUMN);
        }
        let command = if target == "app.t" {
            "drop-table"
        } else {
            "drop"
        };
        succeeds(d.run(&[command, target, "--reason", "no longer retained"]));
        d.commit();
        on_server(
            connection,
            "CREATE VIEW app.external_v AS SELECT label FROM app.t",
        );
        let plan = d.dir.join("drop.json");
        let out = d.run(&[
            "plan",
            "--db",
            connection,
            "--format",
            "json",
            "--out",
            plan.to_str().unwrap(),
        ]);
        assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
        let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
        assert_eq!(report["result"], "unanswerable", "{report}");
        let message = report["findings"][0]["message"].as_str().unwrap();
        assert!(
            message.contains("drop_blockers (PostgreSQL)") && message.contains("external_v"),
            "{report}"
        );
        assert!(!plan.exists(), "refused plans must not be saved");
        on_server(connection, "DROP VIEW app.external_v");
        let out = succeeds(d.run(&[
            "plan",
            "--db",
            connection,
            "--format",
            "json",
            "--out",
            plan.to_str().unwrap(),
        ]));
        let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
        let checks = report["data"]["connected_checks"].as_array().unwrap();
        assert!(
            checks
                .iter()
                .any(|c| c["name"] == "drop_blockers" && c["status"] == "passed"),
            "{report}"
        );
        let human = stdout(&succeeds(d.run(&["plan", "--db", connection])));
        assert!(
            human.contains("drop_blockers") && human.contains("catalog dependencies"),
            "{human}"
        );
        let staged = d.dir.join("drop-staged.json");
        succeeds(d.run(&[
            "plan",
            "--db",
            connection,
            "--staged",
            "--out",
            staged.to_str().unwrap(),
        ]));
        on_server(
            connection,
            "CREATE VIEW app.external_v AS SELECT label FROM app.t",
        );
        for (artifact, is_staged) in [(&plan, false), (&staged, true)] {
            let checksum = plan_checksum(artifact);
            let mut args = vec![
                "apply",
                "--db",
                connection,
                "--plan",
                artifact.to_str().unwrap(),
                "--checksum",
                &checksum,
                "--allow",
                "destructive",
            ];
            if is_staged {
                args.push("--staged");
            }
            let out = d.run(&args);
            assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
            assert!(
                stderr(&out).contains("drop_blockers (PostgreSQL)")
                    && stderr(&out).contains("external_v"),
                "{}",
                stderr(&out)
            );
            let listing =
                succeeds(d.run(&["state", "list", "--db", connection, "--format", "json"]));
            let listing: serde_json::Value = serde_json::from_str(&stdout(&listing)).unwrap();
            let entries = listing["data"]["entries"].as_array().unwrap();
            assert_eq!(
                entries.iter().filter(|e| e["kind"] != "failed").count(),
                1,
                "{listing}"
            );
            assert_eq!(
                entries[0]["kind"], "failed",
                "failed attempts retain their audit entry"
            );
            on_server(connection, "SELECT label FROM app.t");
        }
        on_server(connection, "DROP VIEW app.external_v");
        succeeds(d.run(&[
            "apply",
            "--db",
            connection,
            "--plan",
            plan.to_str().unwrap(),
            "--checksum",
            &plan_checksum(&plan),
            "--allow",
            "destructive",
        ]));
        succeeds(d.run(&["verify", "--db", connection]));
    }
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn drop_blockers_do_not_refuse_a_closing_resume_after_the_drop_completed() {
    use pbps_dialect::Dialect;
    let db = OwnDatabase::new(&server(), "drop_resume");
    let connection = db.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("drop-resume");
    d.table(TWO_COLUMNS);
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    d.table(ONE_COLUMN);
    succeeds(d.run(&["drop", "app.t.label", "--reason", "no longer retained"]));
    d.commit();
    let artifact = d.dir.join("staged.json");
    succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--staged",
        "--out",
        artifact.to_str().unwrap(),
    ]));
    let plan: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&artifact).unwrap()).unwrap();
    assert_eq!(plan.changes.changes.len(), 1);
    let pg = pbps_pg::Postgres::new();
    let p = &plan.changes.changes[0];
    let statements = pg.emit(&p.change, p.strategy).unwrap();
    assert_eq!(
        statements.len(),
        1,
        "a pending DROP cannot follow a prior statement of this change"
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
            .await
            .unwrap();
        let mut checkpoint = pbps_pg::state::latest(&mut conn)
            .await
            .unwrap()
            .unwrap()
            .snapshot;
        conn.execute(&statements[0].sql).await.unwrap();
        checkpoint
            .schema
            .tables
            .get_mut(&pbps_model::TableName::new("app", "t"))
            .unwrap()
            .columns
            .shift_remove("label");
        checkpoint.ids = plan.ids.clone();
        checkpoint.kind = pbps_model::StateKind::Staged;
        checkpoint.plan_checksum = Some(plan.checksum());
        checkpoint.staged = Some(pbps_model::StagedProgress {
            completed: 1,
            total: 1,
            last_statement: statements[0].sql.clone(),
        });
        pbps_pg::state::record(&mut conn, &checkpoint)
            .await
            .unwrap();
    });
    succeeds(d.run(&[
        "apply",
        "--db",
        connection,
        "--plan",
        artifact.to_str().unwrap(),
        "--checksum",
        &plan.checksum(),
        "--allow",
        "destructive",
        "--staged",
        "--resume",
    ]));
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn the_cli_ledger_preserves_history_and_distinguishes_absent_empty_and_unreadable() {
    let own = OwnDatabase::new(&server(), "invariant_ledger");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("invariant-ledger");
    d.table(ONE_COLUMN);
    succeeds(d.run(&["plan"]));
    d.commit();
    let history = || {
        json_output(succeeds(
            d.run(&["state", "list", "--db", connection, "--format", "json"]),
        ))
    };
    let absent = history();
    assert_eq!(absent["data"]["initialized"], false, "{absent}");
    assert_eq!(absent["data"]["entries"], serde_json::json!([]));
    succeeds(d.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "adopting an empty schema",
    ]));
    let recorded = latest_snapshot(connection);
    assert!(recorded.schema.tables.is_empty());
    assert_eq!(
        latest_snapshot(connection),
        recorded,
        "separate connections read the same snapshot"
    );
    succeeds(d.run(&["bootstrap", "--db", connection]));
    let v = history();
    let entries = v["data"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "{v}");
    assert_eq!(entries[0]["kind"], "bootstrap");
    assert_eq!(entries[0]["tables"], 1);
    assert_eq!(entries[1]["kind"], "baseline");
    assert_eq!(entries[1]["tables"], 0);
    assert_eq!(entries[1]["reason"], "adopting an empty schema");
    assert!(entries[0]["id"].as_i64().unwrap() > entries[1]["id"].as_i64().unwrap());
    let limited = json_output(succeeds(d.run(&[
        "state", "list", "--db", connection, "--format", "json", "--limit", "1",
    ])));
    assert_eq!(limited["data"]["entries"], serde_json::json!([entries[0]]));
    // Missing query columns are unreadable, never an empty successful history.
    on_server(
        connection,
        "ALTER TABLE public.__pbps_state RENAME COLUMN kind TO hidden_kind",
    );
    let broken = d.run(&["state", "list", "--db", connection, "--format", "json"]);
    assert_eq!(code(&broken), 1, "{}", stderr(&broken));
    let broken = json_output(broken);
    assert_eq!(broken["result"], "unanswerable");
    assert!(broken.get("data").is_none(), "{broken}");
    on_server(
        connection,
        "ALTER TABLE public.__pbps_state RENAME COLUMN hidden_kind TO kind; DELETE FROM public.__pbps_state",
    );
    let empty = history();
    assert_eq!(empty["data"]["initialized"], true);
    assert_eq!(empty["data"]["entries"], serde_json::json!([]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn the_cli_refuses_the_second_lock_holder_and_unlock_releases_only_the_gate() {
    let own = OwnDatabase::new(&server(), "invariant_lock");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-lock", ONE_COLUMN);
    on_server(
        connection,
        "INSERT INTO public.__pbps_lock (id, locked_by) VALUES (1, 'invariant-first-holder')",
    );
    let before = latest_snapshot(connection);
    for args in [
        vec!["baseline", "--db", connection, "--reason", "second holder"],
        vec!["snapshot", "--db", connection],
    ] {
        let denied = d.run(&args);
        assert_eq!(code(&denied), 1, "{}{}", stdout(&denied), stderr(&denied));
        assert!(
            stderr(&denied).contains("invariant-first-holder"),
            "{}",
            stderr(&denied)
        );
    }
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM public.__pbps_lock WHERE locked_by = 'invariant-first-holder'"
        ),
        1
    );
    assert_eq!(latest_snapshot(connection), before);
    let diagnosis = d.run(&["doctor", "--db", connection, "--format", "json"]);
    assert_eq!(code(&diagnosis), 2);
    let diagnosis = json_output(diagnosis);
    assert!(
        diagnosis["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "state.locked")
    );
    succeeds(d.run(&["unlock", "--db", connection]));
    assert_eq!(
        scalar(connection, "SELECT count(*) FROM public.__pbps_lock"),
        0
    );
    assert_eq!(latest_snapshot(connection), before);
    succeeds(d.run(&["unlock", "--db", connection]));
    succeeds(d.run(&["baseline", "--db", connection, "--reason", "after unlock"]));
    assert_eq!(
        latest_snapshot(connection).reason.as_deref(),
        Some("after unlock")
    );
}

/// A trigger's own `RAISE EXCEPTION` naming a value nobody declared to this
/// tool keeps that value on the operator's own screen and off the ledger.
///
/// The ready-phase finding on PR #464 named two shapes that can put row data
/// into `db.message()` rather than the enrichment fields DECISIONS 454
/// already strips: an engine conversion error, and a "PL/pgSQL/check/trigger
/// exception whose primary MESSAGE contains row data." The first was tried
/// here first and **measured** not to be reachable through this tool's own
/// emitted DDL: `ALTER COLUMN ... TYPE` without an explicit `USING` — which is
/// all pbps ever emits (ADR-0012 §5 refuses a retype no automatic cast
/// covers) — fails a real out-of-range or over-length row with a *generic*
/// message (`integer out of range`, `value too long for type character
/// varying(10)`, `numeric field overflow`), none of which named the value on
/// 18.6; only a bare `CAST('text' AS type)` from an untyped literal does that,
/// and this tool never emits one against existing data. The second shape,
/// below, is reachable exactly as the finding named it, through a data
/// trigger this tool's own guard (`crates/pbps-pg/src/data_triggers.rs`)
/// already treats as approved because it exists in the very first recorded
/// state.
///
/// The secret lives in a table this plan never declares and this trigger
/// reads on its own — not in the row the plan writes. A first draft of this
/// fixture put the literal in the *declared* row instead, and its failure was
/// the useful measurement: pbps's own emitted `INSERT` necessarily restates
/// what was declared, so a secret placed there also appears in this tool's
/// own `.context()` framing around the statement, which is not the driver
/// frame and is not supposed to be redacted (it is the plan's own checksummed
/// text, already in git — DECISIONS 455). Reading a genuinely undeclared
/// value out of a side table, the way `unapproved_data_triggers_
/// cannot_use_the_deployers_privileges` above already reads `public.secret`,
/// isolates the one frame this test means to pin: the driver's.
///
/// The trigger is adopted the way an operator's own would be — pulled after
/// it exists live, declared from that pulled text, and folded into a fresh
/// `baseline` — rather than written by hand, for the same reason the
/// `trigger:`-pulling tests elsewhere in this file give: only a pulled
/// definition is the engine's own deparsed text, and this is what makes the
/// reference-data write below a *recorded managed* use of the trigger, not
/// the refusal `unapproved_data_triggers_cannot_use_the_deployers_privileges`
/// covers.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_triggers_own_exception_keeps_the_row_value_off_the_ledger_but_not_off_the_operator() {
    let own = OwnDatabase::new(&server(), "invariant_trigger_message");
    let connection = own.connection();
    let declared = "table: app.t\ncolumns:\n  code: {type: varchar(20), nullable: false}\n  \
         label: {type: text, nullable: false}\nprimary_key: {name: pk_t, columns: [code]}\n";
    let d = bootstrapped_demo(connection, "invariant-trigger-message", declared);
    on_server(
        connection,
        "CREATE TABLE public.secret(value text); \
         INSERT INTO public.secret VALUES ('super-secret-value-167'); \
         CREATE FUNCTION app.reject() RETURNS trigger LANGUAGE plpgsql AS \
         $$DECLARE v text; BEGIN SELECT value INTO v FROM public.secret; \
           RAISE EXCEPTION 'rejected: %', v; END$$; \
         CREATE TRIGGER reject BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.reject()",
    );
    let puller = Demo::new("invariant-trigger-message-pull");
    succeeds(puller.run(&["pull", "--db", connection]));
    let module = std::fs::read_dir(puller.dir.join("schema"))
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            let body = std::fs::read_to_string(&path).ok()?;
            body.lines()
                .any(|line| line.starts_with("trigger:"))
                .then_some(body)
        })
        .next()
        .expect("the engine's own spelling of the trigger");
    std::fs::write(d.dir.join("schema/reject.yml"), module).unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "adopt the reject trigger",
    ]));
    succeeds(d.run(&["verify", "--db", connection]));
    d.table(&format!(
        "{declared}data:\n  mode: exact\n  rows:\n    x: {{label: New}}\n"
    ));
    let plan = connected_artifact(&d, connection, false);
    let failed = approved_apply(&d, connection, &plan, &[]);
    assert_eq!(code(&failed), 1, "{}{}", stdout(&failed), stderr(&failed));
    assert!(
        stderr(&failed).contains("super-secret-value-167"),
        "the operator, who already holds credentials to this database, still \
         gets the server's own sentence: {}",
        stderr(&failed)
    );
    assert!(stderr(&failed).contains("rejected:"), "{}", stderr(&failed));

    let after = latest_snapshot(connection);
    assert_eq!(after.kind, pbps_model::StateKind::Failed);
    let reason = after.reason.clone().unwrap_or_default();
    assert!(
        !reason.contains("super-secret-value-167"),
        "a value nobody declared to this tool must never reach the durable \
         ledger: {reason}"
    );
    assert!(
        reason.contains("P0001"),
        "the SQLSTATE for a bare RAISE EXCEPTION is not data and should still \
         help an operator reading the ledger later: {reason}"
    );
    assert!(
        reason.contains("the database rejected this statement"),
        "this tool's own framing is not data either, and should survive \
         beside the redacted driver frame: {reason}"
    );
    assert!(
        reason.contains("New"),
        "the plan's own declared value is not the driver's data and must \
         survive in this tool's own emitted-statement text: {reason}"
    );
}

/// A tool-composed refusal — this tool's own guard naming an unapproved
/// trigger, never a server value — keeps naming the trigger in the durable
/// ledger, unlike the driver's own sentence the test above pins.
///
/// A second ready-phase round on PR #464 found `ledger_safe_reason` treating
/// this refusal the same way as a real driver frame: `data_triggers.rs`'s
/// `refused()` used to build a `DbError::Driver` with no code, exactly the
/// shape `ledger_safe_reason` could not tell apart from a server's, and it
/// redacted the one thing an operator reading the ledger later needs — which
/// trigger to remove. `DbError::Refused` (DECISIONS 455) makes this
/// unrepresentable as `Driver`: this test pins that the redaction can never
/// happen again, whatever call site builds the refusal.
///
/// The trigger is created live *after* the plan is computed against the
/// clean baseline, so `plan` never sees it and only `apply`'s own re-check
/// (`crate::engine::prepare_data_writes`, run fresh inside the transaction)
/// catches it — the same shape a trigger installed between approval and
/// deployment would take outside a test.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn an_unapproved_triggers_own_refusal_keeps_naming_it_on_the_ledger() {
    let own = OwnDatabase::new(&server(), "invariant_refused_trigger");
    let connection = own.connection();
    let declared = "table: app.t\ncolumns:\n  code: {type: varchar(20), nullable: false}\n  \
         label: {type: text, nullable: false}\nprimary_key: {name: pk_t, columns: [code]}\n";
    let d = bootstrapped_demo(connection, "invariant-refused-trigger", declared);
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: postgres\nunmanaged: ignore\n",
    )
    .unwrap();
    d.table(&format!(
        "{declared}data:\n  mode: exact\n  rows:\n    x: {{label: New}}\n"
    ));
    let plan = connected_artifact(&d, connection, false);
    on_server(
        connection,
        "CREATE FUNCTION app.audit() RETURNS trigger LANGUAGE plpgsql AS \
         $$BEGIN RETURN NEW; END$$; \
         CREATE TRIGGER audit BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.audit()",
    );
    let failed = approved_apply(&d, connection, &plan, &[]);
    assert_eq!(code(&failed), 1, "{}{}", stdout(&failed), stderr(&failed));
    assert!(
        stderr(&failed).contains("unsafe data trigger") && stderr(&failed).contains("audit"),
        "{}",
        stderr(&failed)
    );

    let after = latest_snapshot(connection);
    assert_eq!(after.kind, pbps_model::StateKind::Failed);
    let reason = after.reason.clone().unwrap_or_default();
    assert!(
        reason.contains("unsafe data trigger") && reason.contains("audit"),
        "a tool-composed refusal names its own trigger, and must survive on \
         the ledger unredacted: {reason}"
    );
    assert!(
        !reason.contains("its message is not recorded here"),
        "this is not a driver frame, and must never be run through the \
         driver-only redaction marker: {reason}"
    );
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_second_statement_failure_rolls_back_the_first_and_records_only_a_failed_attempt() {
    let own = OwnDatabase::new(&server(), "invariant_atomic");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-atomic", ONE_COLUMN);
    d.table(&ONE_COLUMN.replace(
        "  id:",
        "  first: {type: text}\n  second: {type: text}\n  id:",
    ));
    let plan = connected_artifact(&d, connection, false);
    let saved: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&plan).unwrap()).unwrap();
    assert_eq!(saved.changes.changes.len(), 2);
    // An event trigger fails the second DDL after the first really executed.
    // Sequence increments survive rollback, so this cannot pass by refusing
    // in preflight before either statement ran.
    on_server(
        connection,
        r#"
        CREATE SCHEMA witness;
        CREATE SEQUENCE witness.executed;
        CREATE FUNCTION witness.fail_second() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN
          IF EXISTS (SELECT FROM pg_attribute WHERE attrelid = 'app.t'::regclass AND attname IN ('first', 'second') AND NOT attisdropped) THEN
            PERFORM nextval('witness.executed');
          END IF;
          IF (SELECT count(*) FROM pg_attribute WHERE attrelid = 'app.t'::regclass AND attname IN ('first', 'second') AND NOT attisdropped) = 2 THEN
            RAISE EXCEPTION 'invariant second statement failed';
          END IF;
        END $$;
        CREATE EVENT TRIGGER fail_second ON ddl_command_end WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION witness.fail_second();
    "#,
    );
    let failed = approved_apply(&d, connection, &plan, &[]);
    assert_eq!(code(&failed), 1, "{}{}", stdout(&failed), stderr(&failed));
    assert_eq!(
        scalar(
            connection,
            "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM witness.executed"
        ),
        2,
        "{}{}",
        stdout(&failed),
        stderr(&failed)
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_attribute WHERE attrelid = 'app.t'::regclass AND attname IN ('first', 'second') AND NOT attisdropped"
        ),
        0
    );
    let after = latest_snapshot(connection);
    assert_eq!(after.kind, pbps_model::StateKind::Failed);
    assert_eq!(
        after.plan_checksum.as_deref(),
        Some(plan_checksum(&plan).as_str())
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM public.__pbps_state WHERE kind = 'apply'"
        ),
        0
    );
    succeeds(d.run(&["verify", "--db", connection]));
    on_server(connection, "DROP EVENT TRIGGER fail_second");
    succeeds(approved_apply(&d, connection, &plan, &[]));
    assert_eq!(
        latest_snapshot(connection).kind,
        pbps_model::StateKind::Apply
    );
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn the_cli_probe_counts_real_null_rows_before_any_statement_and_then_accepts_clean_data() {
    let own = OwnDatabase::new(&server(), "invariant_probe");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-probe", TWO_COLUMNS);
    on_server(
        connection,
        "INSERT INTO app.t (id, label) VALUES (1, NULL), (2, NULL), (3, 'present')",
    );
    d.table(&TWO_COLUMNS.replace(
        "label: {type: varchar(50)}",
        "label: {type: varchar(50), nullable: false}",
    ));
    let plan = connected_artifact(&d, connection, false);
    let refused = approved_apply(&d, connection, &plan, &["--allow", "not-null"]);
    assert_eq!(code(&refused), 1);
    assert!(
        stderr(&refused).contains("2 existing NULLs in"),
        "{}",
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("nothing has been changed"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_attribute WHERE attrelid = 'app.t'::regclass AND attname = 'label' AND attnotnull"
        ),
        0
    );
    assert_eq!(
        scalar(connection, "SELECT count(*) FROM app.t WHERE label IS NULL"),
        2
    );
    on_server(
        connection,
        "UPDATE app.t SET label = 'filled' WHERE label IS NULL",
    );
    let applied = succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "not-null"],
    ));
    assert!(
        stdout(&applied).contains("probe(s) passed"),
        "{}",
        stdout(&applied)
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_attribute WHERE attrelid = 'app.t'::regclass AND attname = 'label' AND attnotnull"
        ),
        1
    );
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn constraint_triggers_pull_bootstrap_and_plan_without_duplicate_constraints() {
    use pbps_model::{Change, ModuleKind, SavedPlan};
    let own = OwnDatabase::new(&server(), "constraint_trigger229");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app;
        CREATE TABLE app.t (id integer PRIMARY KEY);
        CREATE TABLE app.child (id integer REFERENCES app.t(id));
        CREATE TABLE app.calls (id integer);
        CREATE FUNCTION app.fire() RETURNS trigger LANGUAGE plpgsql AS
            $$BEGIN INSERT INTO app.calls VALUES (NEW.id); RETURN NEW; END$$;
        CREATE CONSTRAINT TRIGGER \"constraint name\" AFTER INSERT ON app.t
            DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION app.fire()",
    );
    let d = Demo::new("constraint-trigger229-pull");
    succeeds(d.run(&["pull", "--db", connection]));
    let loaded = pbps_load::load_schema_dir(&d.dir.join("schema")).unwrap();
    let trigger = "app.t.constraint name".parse().unwrap();
    assert_eq!(
        loaded.schema.modules.len(),
        2,
        "{:?}",
        loaded.schema.modules
    );
    let module = &loaded.schema.modules[&trigger];
    assert_eq!(module.kind, ModuleKind::Trigger);
    assert!(
        module
            .definition
            .starts_with("CONSTRAINT AFTER INSERT ON app.t ")
    );
    assert!(module.definition.contains("DEFERRABLE INITIALLY DEFERRED"));
    // The four internal RI triggers stay out while their FK stays represented.
    assert_eq!(
        loaded.schema.tables[&"app.child".parse().unwrap()]
            .foreign_keys
            .len(),
        1
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_trigger WHERE tgisinternal
        AND tgrelid IN ('app.t'::regclass, 'app.child'::regclass)"
        ),
        4
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
                .await
                .unwrap();
            let pulled = pbps_pg::catalog::introspect(&mut conn).await.unwrap();
            assert!(pulled.limitations.is_empty(), "{:?}", pulled.limitations);
        });
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "adopt a user constraint trigger",
    ]));
    let plan = d.dir.join("adoption-plan.json");
    succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
    let saved: SavedPlan = serde_json::from_str(&std::fs::read_to_string(plan).unwrap()).unwrap();
    assert!(saved.changes.is_empty(), "{:?}", saved.changes);

    // Bootstrap the actual serialized pull, then measure the promised timing.
    let rebuilt = OwnDatabase::new(&server(), "constraint_trigger229_rebuilt");
    let rebuilt = rebuilt.connection();
    on_server(rebuilt, "CREATE SCHEMA app");
    succeeds(d.run(&["bootstrap", "--db", rebuilt]));
    assert_eq!(
        latest_snapshot(rebuilt).schema.modules,
        loaded.schema.modules
    );
    assert_eq!(
        scalar(
            rebuilt,
            "SELECT count(*) FROM pg_trigger WHERE tgname='constraint name'
        AND NOT tgisinternal AND tgconstraint <> 0 AND tgdeferrable AND tginitdeferred"
        ),
        1
    );
    on_server(
        rebuilt,
        "BEGIN; INSERT INTO app.t VALUES (1);
        DO $$BEGIN ASSERT (SELECT count(*) FROM app.calls) = 0; END$$;
        SET CONSTRAINTS ALL IMMEDIATE;
        DO $$BEGIN ASSERT (SELECT count(*) FROM app.calls) = 1; END$$; ROLLBACK",
    );
    assert!(try_on_server(rebuilt, "INSERT INTO app.child VALUES (99)").is_err());

    // An edit takes the ordinary module drop/create path and retains CONSTRAINT.
    let mut paths = vec![d.dir.join("schema")];
    let path = loop {
        let path = paths.pop().expect("the pulled trigger declaration");
        if path.is_dir() {
            paths.extend(std::fs::read_dir(path).unwrap().map(|e| e.unwrap().path()));
        } else if std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .any(|l| l.starts_with("trigger:"))
        {
            break path;
        }
    };
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        path,
        text.replace("INITIALLY DEFERRED", "INITIALLY IMMEDIATE"),
    )
    .unwrap();
    let plan = connected_artifact(&d, rebuilt, false);
    let saved: SavedPlan = serde_json::from_str(&std::fs::read_to_string(&plan).unwrap()).unwrap();
    assert_eq!(saved.changes.changes.len(), 1, "{:?}", saved.changes);
    assert!(
        matches!(&saved.changes.changes[0].change, Change::AlterModule { id, .. } if id == &trigger)
    );
    succeeds(approved_apply(&d, rebuilt, &plan, &[]));
    assert_eq!(
        scalar(
            rebuilt,
            "SELECT count(*) FROM pg_trigger WHERE tgname='constraint name'
        AND tgconstraint <> 0 AND tgdeferrable AND NOT tginitdeferred"
        ),
        1
    );
    succeeds(d.run(&["verify", "--db", rebuilt]));
}

const CONSTRAINT_TRIGGER_TABLE: &str = "table: app.t\ncolumns:\n  code: {type: varchar(20), nullable: false}\n  label: {type: text, nullable: false}\nprimary_key: {name: pk_t, columns: [code]}\n";

fn adopted_constraint_trigger(connection: &str, slug: &str, function_body: &str) -> Demo {
    let d = bootstrapped_demo(connection, slug, CONSTRAINT_TRIGGER_TABLE);
    on_server(
        connection,
        &format!(
            "CREATE FUNCTION app.late() RETURNS trigger LANGUAGE plpgsql AS $$ {function_body} $$;
        CREATE CONSTRAINT TRIGGER late AFTER INSERT ON app.t
            DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION app.late()"
        ),
    );
    adopt_constraint_trigger_modules(connection, &d);
    d
}

fn adopt_constraint_trigger_modules(connection: &str, d: &Demo) {
    let modules = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
                .await
                .unwrap();
            pbps_pg::catalog::introspect(&mut conn)
                .await
                .unwrap()
                .schema
                .modules
        });
    for (index, (id, module)) in modules.into_iter().enumerate() {
        let body = module
            .definition
            .lines()
            .map(|line| format!("  {line}\n"))
            .collect::<String>();
        let kind = module.kind.as_str();
        let identity = match &id {
            pbps_model::ModuleId::Trigger { on, name } => format!("{}.{name}\non: {on}", on.schema),
            pbps_model::ModuleId::Named(_) | pbps_model::ModuleId::Routine(_) => id.to_string(),
        };
        std::fs::write(
            d.dir.join(format!("schema/adopted-{index}.yml")),
            format!("{kind}: {identity}\ndefinition: |-\n{body}"),
        )
        .unwrap();
    }
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "adopt a deferred trigger",
    ]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn constraint_trigger_checks_wait_for_every_planned_row() {
    let own = OwnDatabase::new(&server(), "constraint_trigger229_complete");
    let connection = own.connection();
    let d = adopted_constraint_trigger(
        connection,
        "constraint-trigger229-complete",
        "BEGIN IF (SELECT count(*) FROM app.t) <> 2 THEN RAISE EXCEPTION 'both rows are required'; END IF; RETURN NEW; END",
    );
    d.table(&format!("{CONSTRAINT_TRIGGER_TABLE}data:\n  mode: exact\n  rows:\n    first: {{label: First}}\n    second: {{label: Second}}\n"));
    let plan = connected_artifact(&d, connection, false);
    succeeds(approved_apply(&d, connection, &plan, &[]));
    assert_eq!(scalar(connection, "SELECT count(*) FROM app.t"), 2);
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn constraint_trigger_effects_cannot_arrive_after_the_recorded_row_check() {
    let own = OwnDatabase::new(&server(), "constraint_trigger229_late");
    let connection = own.connection();
    let d = adopted_constraint_trigger(
        connection,
        "constraint-trigger229-late",
        "BEGIN UPDATE app.t SET label='rewritten' WHERE code=NEW.code; RETURN NEW; END",
    );
    d.table(&format!(
        "{CONSTRAINT_TRIGGER_TABLE}data:\n  mode: exact\n  rows:\n    first: {{label: Expected}}\n"
    ));
    let plan = connected_artifact(&d, connection, false);
    let outcome = approved_apply(&d, connection, &plan, &[]);
    assert_eq!(
        code(&outcome),
        1,
        "{}{}",
        stdout(&outcome),
        stderr(&outcome)
    );
    assert!(
        stderr(&outcome).contains("deferred constraint triggers changed"),
        "{}",
        stderr(&outcome)
    );
    assert_eq!(scalar(connection, "SELECT count(*) FROM app.t"), 0);
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn indirect_constraint_trigger_effects_are_settled_before_recording() {
    let own = OwnDatabase::new(&server(), "constraint_trigger229_indirect");
    let connection = own.connection();
    let d = bootstrapped_demo(
        connection,
        "constraint-trigger229-indirect",
        CONSTRAINT_TRIGGER_TABLE,
    );
    on_server(
        connection,
        "CREATE TABLE app.side (code varchar(20));
        CREATE FUNCTION app.forward() RETURNS trigger LANGUAGE plpgsql AS
            $$BEGIN INSERT INTO app.side VALUES (NEW.code); RETURN NEW; END$$;
        CREATE TRIGGER forward AFTER INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.forward();
        CREATE FUNCTION app.late() RETURNS trigger LANGUAGE plpgsql AS
            $$BEGIN UPDATE app.t SET label='rewritten' WHERE code=NEW.code; RETURN NEW; END$$;
        CREATE CONSTRAINT TRIGGER late AFTER INSERT ON app.side
            DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION app.late()",
    );
    std::fs::write(
        d.dir.join("schema/side.yml"),
        "table: app.side\ncolumns:\n  code: {type: varchar(20)}\n",
    )
    .unwrap();
    adopt_constraint_trigger_modules(connection, &d);
    d.table(&format!(
        "{CONSTRAINT_TRIGGER_TABLE}data:\n  mode: exact\n  rows:\n    first: {{label: Expected}}\n"
    ));
    let plan = connected_artifact(&d, connection, false);
    let outcome = approved_apply(&d, connection, &plan, &[]);
    let verification = d.run(&["verify", "--db", connection]);
    assert_eq!(
        code(&outcome),
        1,
        "{}{}; verify: {}{}",
        stdout(&outcome),
        stderr(&outcome),
        stdout(&verification),
        stderr(&verification)
    );
    assert!(
        stderr(&outcome).contains("deferred constraint triggers changed"),
        "{}",
        stderr(&outcome)
    );
    assert_eq!(scalar(connection, "SELECT count(*) FROM app.t"), 0);
    assert_eq!(scalar(connection, "SELECT count(*) FROM app.side"), 0);
    succeeds(verification);
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn nonordinary_trigger_modes_are_reported_instead_of_recorded() {
    use pbps_db::catalog::LimitationTarget;
    let own = OwnDatabase::new(&server(), "constraint_trigger229_modes");
    let connection = own.connection();
    let d = bootstrapped_demo(
        connection,
        "constraint-trigger229-modes",
        CONSTRAINT_TRIGGER_TABLE,
    );
    on_server(
        connection,
        "CREATE FUNCTION app.fire() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$;
        CREATE TRIGGER ordinary BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.fire();
        CREATE CONSTRAINT TRIGGER deferred AFTER INSERT ON app.t
            DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION app.fire()",
    );
    adopt_constraint_trigger_modules(connection, &d);
    let before = latest_snapshot(connection);
    for (mode, flag, reason) in [
        ("DISABLE", "D", "disabled"),
        ("ENABLE REPLICA", "R", "replica"),
        ("ENABLE ALWAYS", "A", "always"),
    ] {
        on_server(
            connection,
            &format!(
                "ALTER TABLE app.t {mode} TRIGGER ordinary; ALTER TABLE app.t {mode} TRIGGER deferred"
            ),
        );
        assert_eq!(
            scalar(
                connection,
                &format!(
                    "SELECT count(*) FROM pg_trigger WHERE tgrelid='app.t'::regclass AND NOT tgisinternal AND tgenabled='{flag}'"
                )
            ),
            2
        );
        let pulled = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
                    .await
                    .unwrap();
                pbps_pg::catalog::introspect(&mut conn).await.unwrap()
            });
        assert!(pulled.schema.tables.contains_key(&"app.t".parse().unwrap()));
        assert!(
            !pulled
                .limitations
                .iter()
                .any(|l| l.target == LimitationTarget::Relation("app.t".parse().unwrap())),
            "{:?}",
            pulled.limitations
        );
        for name in ["deferred", "ordinary"] {
            let id = format!("app.t.{name}").parse().unwrap();
            assert!(
                !pulled.schema.modules.contains_key(&id),
                "{mode}: {name} was recorded as an ordinary trigger"
            );
            let target = LimitationTarget::Module(id);
            let notes: Vec<_> = pulled
                .limitations
                .iter()
                .filter(|l| l.target == target)
                .collect();
            assert_eq!(notes.len(), 1, "{mode}: {notes:?}");
            assert!(notes[0].detail.contains(reason), "{}", notes[0].detail);
            assert!(pulled.unmanaged_modules.iter().any(|m| m.target == target));
        }
        let verification = d.run(&["verify", "--db", connection]);
        assert_eq!(
            code(&verification),
            2,
            "{}{}",
            stdout(&verification),
            stderr(&verification)
        );
        let refused = d.run(&[
            "baseline",
            "--db",
            connection,
            "--reason",
            "do not erase an enable mode",
        ]);
        assert_eq!(
            code(&refused),
            1,
            "{}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert_eq!(latest_snapshot(connection).schema, before.schema);
        let fresh = Demo::new(&format!("constraint-trigger229-pull-{flag}"));
        let output = succeeds(fresh.run(&["pull", "--db", connection]));
        assert!(stderr(&output).contains(reason), "{}", stderr(&output));
        let loaded = pbps_load::load_schema_dir(&fresh.dir.join("schema")).unwrap();
        assert_eq!(
            loaded.schema.modules.len(),
            1,
            "{:?}",
            loaded.schema.modules
        );
    }
    // Restoring ordinary mode restores the same module definitions and clean state.
    on_server(
        connection,
        "ALTER TABLE app.t ENABLE TRIGGER ordinary; ALTER TABLE app.t ENABLE TRIGGER deferred",
    );
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn constraint_trigger_guards_accept_only_the_recorded_managed_definition() {
    use pbps_dialect::{Dialect, RowOperation, RowWrite};
    let own = OwnDatabase::new(&server(), "constraint_trigger229_guard");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; CREATE TABLE app.t (id integer);
        CREATE FUNCTION app.fire() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$;
        CREATE CONSTRAINT TRIGGER guarded AFTER INSERT ON app.t
            DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION app.fire()",
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
                .await
                .unwrap();
            let baseline = pbps_pg::catalog::introspect(&mut conn)
                .await
                .unwrap()
                .schema;
            let pg = pbps_pg::Postgres::new();
            let write = RowWrite {
                table: "app.t".parse().unwrap(),
                operation: RowOperation::Insert,
            };
            conn.begin(pg.transaction_framing()).await.unwrap();
            let guard = pbps_pg::data_triggers::prepare(
                &mut conn,
                std::slice::from_ref(&write),
                &baseline,
                &Default::default(),
            )
            .await
            .unwrap();
            pbps_pg::data_triggers::check(&mut conn, &write, &guard)
                .await
                .unwrap();
            conn.execute("INSERT INTO app.t VALUES (1)").await.unwrap();
            conn.commit(pg.transaction_framing()).await.unwrap();
            // A matching body without the marker must not authenticate another kind.
            let mut ordinary = baseline.clone();
            ordinary
                .modules
                .get_mut(&"app.t.guarded".parse().unwrap())
                .unwrap()
                .definition = baseline.modules[&"app.t.guarded".parse().unwrap()]
                .definition
                .strip_prefix("CONSTRAINT ")
                .unwrap()
                .to_owned();
            for record in [ordinary, Default::default()] {
                conn.begin(pg.transaction_framing()).await.unwrap();
                assert!(
                    pbps_pg::data_triggers::prepare(
                        &mut conn,
                        std::slice::from_ref(&write),
                        &record,
                        &Default::default()
                    )
                    .await
                    .is_err()
                );
                conn.rollback(pg.transaction_framing()).await.unwrap();
            }
        });
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn modules_apply_then_pull_round_trip_and_changed_bodies_are_drift() {
    let own = OwnDatabase::new(&server(), "invariant_modules");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-modules", ONE_COLUMN);
    std::fs::write(
        d.dir.join("schema/v.yml"),
        "view: app.v\ndefinition: SELECT id FROM app.t WHERE id > 0\n",
    )
    .unwrap();
    std::fs::write(d.dir.join("schema/f.yml"), "function: app.f(integer)\ndefinition: (n integer) RETURNS integer LANGUAGE sql AS $$ SELECT n + 1 $$\n").unwrap();
    let plan = connected_artifact(&d, connection, false);
    succeeds(approved_apply(&d, connection, &plan, &[]));
    succeeds(d.run(&["verify", "--db", connection]));
    let fresh = Demo::new("invariant-modules-pull");
    succeeds(fresh.run(&["pull", "--db", connection]));
    let loaded = pbps_load::load_schema_dir(&fresh.dir.join("schema")).unwrap();
    assert_eq!(loaded.schema.modules.len(), 2);
    assert!(
        loaded
            .schema
            .modules
            .contains_key(&"app.v".parse().unwrap())
    );
    assert!(
        loaded
            .schema
            .modules
            .contains_key(&"app.f(integer)".parse().unwrap())
    );
    // Pull serializes the engine's canonical body, which may differ from
    // the original SQL spelling tracked in declared-state metadata.
    assert_eq!(
        loaded.schema.modules,
        latest_snapshot(connection).schema.modules
    );
    on_server(
        connection,
        "CREATE OR REPLACE VIEW app.v AS SELECT id FROM app.t WHERE id > 1",
    );
    let drift = d.run(&["verify", "--db", connection]);
    assert_eq!(code(&drift), 2, "{}{}", stdout(&drift), stderr(&drift));
    assert!(stdout(&drift).contains("app.v"), "{}", stdout(&drift));
    let overwrite = fresh.run(&["pull", "--db", connection]);
    assert_eq!(code(&overwrite), 1);
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_staged_cli_checkpoint_survives_state_json_and_resume_checks_its_intermediate_name() {
    let own = OwnDatabase::new(&server(), "invariant_checkpoint");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-checkpoint", ONE_COLUMN);
    on_server(connection, "CREATE SCHEMA moved; CREATE SCHEMA witness");
    d.table(&ONE_COLUMN.replace("table: app.t", "table: moved.u\nrenamed_from: app.t"));
    let plan = connected_artifact(&d, connection, true);
    on_server(
        connection,
        r#"
        CREATE FUNCTION witness.stop_rename() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN
          IF to_regclass('moved.u') IS NOT NULL THEN RAISE EXCEPTION 'invariant stop after schema transfer'; END IF;
        END $$;
        CREATE EVENT TRIGGER stop_rename ON ddl_command_end WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION witness.stop_rename();
    "#,
    );
    let failed = approved_apply(&d, connection, &plan, &["--allow", "rename", "--staged"]);
    assert_eq!(code(&failed), 1, "{}{}", stdout(&failed), stderr(&failed));
    let checkpoint = latest_snapshot(connection);
    let progress = checkpoint
        .staged
        .as_ref()
        .expect("a durable checkpoint, not a transactional rollback");
    assert_eq!((progress.completed, progress.total), (1, 2));
    assert!(
        progress.last_statement.contains("SET SCHEMA"),
        "{progress:?}"
    );
    assert!(
        checkpoint
            .schema
            .tables
            .contains_key(&"moved.t".parse().unwrap()),
        "{checkpoint:?}"
    );
    assert!(
        !checkpoint
            .schema
            .tables
            .contains_key(&"moved.u".parse().unwrap())
    );
    assert_eq!(
        latest_snapshot(connection),
        checkpoint,
        "the checkpoint round-trips on a fresh connection"
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_class WHERE oid = to_regclass('moved.t')"
        ),
        1
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_class WHERE oid = to_regclass('moved.u')"
        ),
        0
    );
    let listed = json_output(succeeds(
        d.run(&["state", "list", "--db", connection, "--format", "json"]),
    ));
    assert_eq!(
        listed["data"]["entries"][0]["staged"]["completed"], 1,
        "{listed}"
    );
    // Movement after the saved checkpoint is not silently adopted by resume.
    on_server(
        connection,
        "DROP EVENT TRIGGER stop_rename; ALTER TABLE moved.t ADD COLUMN rogue text",
    );
    let refused = approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "rename", "--staged", "--resume"],
    );
    assert_eq!(code(&refused), 1);
    assert!(
        stderr(&refused).contains("has moved since the checkpoint"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(
        latest_snapshot(connection)
            .staged
            .as_ref()
            .unwrap()
            .completed,
        1
    );
    on_server(connection, "ALTER TABLE moved.t DROP COLUMN rogue");
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "rename", "--staged", "--resume"],
    ));
    let closed = latest_snapshot(connection);
    assert_eq!(closed.kind, pbps_model::StateKind::Apply);
    assert!(closed.staged.is_none());
    assert!(
        closed
            .schema
            .tables
            .contains_key(&"moved.u".parse().unwrap())
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_class WHERE oid = to_regclass('moved.t')"
        ),
        0
    );
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_cross_schema_cli_rename_requires_approval_and_records_only_its_declared_destination() {
    let own = OwnDatabase::new(&server(), "invariant_cross_schema");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-cross-schema", ONE_COLUMN);
    on_server(
        connection,
        "CREATE SCHEMA moved; INSERT INTO app.t VALUES (42)",
    );
    d.table(&ONE_COLUMN.replace("table: app.t", "table: moved.u\nrenamed_from: app.t"));
    let plan = connected_artifact(&d, connection, false);
    let denied = approved_apply(&d, connection, &plan, &[]);
    assert_eq!(code(&denied), 1);
    assert!(stderr(&denied).contains("--allow"), "{}", stderr(&denied));
    assert_eq!(
        scalar(connection, "SELECT count(*) FROM app.t WHERE id = 42"),
        1
    );
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "rename"],
    ));
    assert_eq!(
        scalar(connection, "SELECT count(*) FROM moved.u WHERE id = 42"),
        1
    );
    for name in ["app.t", "app.u", "moved.t"] {
        assert_eq!(
            scalar(
                connection,
                &format!("SELECT count(*) FROM pg_class WHERE oid = to_regclass('{name}')")
            ),
            0,
            "no intermediate alias {name}"
        );
    }
    let recorded = latest_snapshot(connection);
    assert_eq!(
        recorded.schema.tables.keys().collect::<Vec<_>>(),
        vec![&"moved.u".parse::<pbps_model::TableName>().unwrap()]
    );
    assert_eq!(
        recorded.ids.tables.values().collect::<Vec<_>>(),
        vec![&"moved.u".parse::<pbps_model::TableName>().unwrap()]
    );
    succeeds(d.run(&["verify", "--db", connection]));
    assert!(stdout(&succeeds(d.run(&["plan", "--db", connection]))).contains("No changes"));
}

/// #307: a rename whose only dependant is a view the engine carries printed
/// an "affects:" heading with nothing under it. The view is listed, as
/// carried, with the note that it keeps its old output column name.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_carried_only_rename_impact_lists_what_the_rename_carries() {
    let own = OwnDatabase::new(&server(), "carried_only");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "carried-only", TWO_COLUMNS);
    on_server(
        connection,
        "CREATE SCHEMA outside; CREATE VIEW outside.follows AS SELECT label FROM app.t;",
    );
    d.table(&TWO_COLUMNS.replace(
        "  label: {type: varchar(50)}",
        "  note: {type: varchar(50), renamed_from: label}",
    ));
    let plan = connected_artifact(&d, connection, false);
    let applied = succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "rename"],
    ));
    let out = stdout(&applied);
    let heading = out
        .lines()
        .position(|l| l.ends_with("affects:"))
        .unwrap_or_else(|| panic!("no impact heading: {out}"));
    let body: Vec<&str> = out
        .lines()
        .skip(heading + 1)
        .take_while(|l| l.starts_with("  "))
        .collect();
    assert!(
        body.iter().any(|l| l.contains("outside.follows")
            && l.contains("carried into the new name, keeps working")),
        "{out}"
    );
    assert!(
        body.iter()
            .any(|l| l.contains("keeps its old output column name")),
        "{out}"
    );
    assert_eq!(
        scalar(connection, "SELECT count(*) FROM outside.follows"),
        0
    );
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn postgres_rename_impact_reaches_the_cli_as_advisory_and_requires_explicit_approval() {
    let own = OwnDatabase::new(&server(), "invariant_impact");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-impact", TWO_COLUMNS);
    on_server(
        connection,
        r#"
        CREATE SCHEMA outside;
        CREATE VIEW outside.carried AS SELECT label FROM app.t;
        CREATE FUNCTION outside.text_body() RETURNS text LANGUAGE plpgsql AS $$
          BEGIN RETURN (SELECT label FROM app.t LIMIT 1); END $$;
        INSERT INTO app.t VALUES (1, 'kept');
    "#,
    );
    d.table(&TWO_COLUMNS.replace(
        "  label: {type: varchar(50)}",
        "  note: {type: varchar(50), renamed_from: label}",
    ));
    let plan = connected_artifact(&d, connection, false);
    let denied = approved_apply(&d, connection, &plan, &[]);
    assert_eq!(code(&denied), 1);
    assert!(stderr(&denied).contains("--allow"));
    // PostgreSQL carries catalog dependencies instead of refusing them as
    // SCHEMABINDING. A text body is advisory, printed by apply's preflight.
    let applied = succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "rename"],
    ));
    assert!(
        stdout(&applied).contains("outside.text_body"),
        "{}",
        stdout(&applied)
    );
    assert!(
        stdout(&applied).contains("Nothing outside the database is visible"),
        "{}",
        stdout(&applied)
    );
    // #307: the view follows the rename and says so, as carried — never as
    // something that breaks.
    let carried = stdout(&applied)
        .lines()
        .filter(|l| l.contains("outside.carried"))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    assert!(
        !carried.is_empty()
            && carried
                .iter()
                .all(|l| l.contains("carried into the new name, keeps working")),
        "{}",
        stdout(&applied)
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM outside.carried WHERE label = 'kept'"
        ),
        1
    );
    let broken = try_on_server(connection, "SELECT outside.text_body()");
    assert!(
        broken.is_err(),
        "the advisory names a text body the rename really breaks"
    );
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn doctor_exercises_a_real_non_superuser_and_names_the_permission_removed_from_it() {
    struct Role(String, String);
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let server = server();
    let role = Role(
        server.clone(),
        format!("pbps_invariant_doctor_{}", std::process::id()),
    );
    let own = OwnDatabase::new(&server, "invariant_doctor");
    let connection = own.connection();
    on_server(
        connection,
        &format!(
            "CREATE ROLE {} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD 'invariant-test'; CREATE SCHEMA app AUTHORIZATION {}; GRANT CREATE ON SCHEMA public TO {}",
            role.1, role.1, role.1
        ),
    );
    let login = format!(
        "{} user={} password=invariant-test",
        connection
            .split_whitespace()
            .filter(|w| !w.starts_with("user=") && !w.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" "),
        role.1
    );
    let d = Demo::new("invariant-doctor");
    d.table(ONE_COLUMN);
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", &login]));
    assert_eq!(
        scalar(
            &login,
            "SELECT count(*) FROM pg_roles WHERE rolname = current_user AND (rolsuper OR rolcreatedb OR rolcreaterole OR rolreplication)"
        ),
        0
    );
    let ready = json_output(succeeds(
        d.run(&["doctor", "--db", &login, "--format", "json"]),
    ));
    assert_eq!(
        ready["data"]["environments"][0]["missing_permissions"],
        serde_json::json!([]),
        "{ready}"
    );
    // The role owns `app.t`, and an owner can still lose ordinary `SELECT`:
    // ownership keeps the grant option, not the privilege (DECISIONS 428,
    // measured on 18.6, #368). The setup asserts both halves of that, so the
    // exit 2 below is `doctor` reading a real missing privilege.
    let privilege = |which: &str| {
        scalar(
            connection,
            &format!(
                "SELECT pg_catalog.has_table_privilege('{}', 'app.t', '{which}')::int::int8",
                role.1
            ),
        )
    };
    assert_eq!(privilege("SELECT"), 1, "the owner starts with SELECT");
    on_server(
        connection,
        &format!("REVOKE SELECT ON app.t FROM {}", role.1),
    );
    assert_eq!(
        privilege("SELECT"),
        0,
        "the REVOKE took SELECT from the owner"
    );
    assert_eq!(
        privilege("SELECT WITH GRANT OPTION"),
        1,
        "and left the owner's grant option standing"
    );
    let refused = d.run(&["doctor", "--db", &login, "--format", "json"]);
    assert_eq!(
        code(&refused),
        2,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    let refused = json_output(refused);
    let missing = refused["data"]["environments"][0]["missing_permissions"]
        .as_array()
        .unwrap();
    assert!(
        missing.iter().any(|v| v
            .as_str()
            .is_some_and(|s| s.contains("SELECT") && s.contains("app"))),
        "{refused}"
    );
    assert!(!refused.to_string().contains("invariant-test"));
    on_server(connection, &format!("GRANT SELECT ON app.t TO {}", role.1));
    succeeds(d.run(&["doctor", "--db", &login]));
}

/// #306: a role that cannot read the deployment lock is told what to grant in
/// PostgreSQL's words — `public.__pbps_lock` and `USAGE` on its schema — and
/// not SQL Server's `dbo.__pbps_lock` and `VIEW DEFINITION`. A real restricted
/// role against a ledger another role created, as a deployment account meets
/// it.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn doctor_tells_a_role_that_cannot_read_the_lock_what_to_grant_in_postgres_terms() {
    struct Role(String, String);
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let server = server();
    let role = Role(
        server.clone(),
        format!("pbps_lock_reader_{}", std::process::id()),
    );
    let own = OwnDatabase::new(&server, "lock_remedy");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "lock-remedy", ONE_COLUMN);
    on_server(
        connection,
        &format!(
            "CREATE ROLE {} LOGIN NOSUPERUSER PASSWORD 'pw-306-secret'; \
             REVOKE SELECT ON public.__pbps_lock FROM PUBLIC",
            role.1
        ),
    );
    let login = format!(
        "{} user={} password=pw-306-secret",
        connection
            .split_whitespace()
            .filter(|w| !w.starts_with("user=") && !w.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" "),
        role.1
    );
    let refused = d.run(&["doctor", "--db", &login, "--format", "json"]);
    assert_ne!(
        code(&refused),
        0,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    let report = json_output(refused);
    let lock = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "state.lock-unknown")
        .unwrap_or_else(|| panic!("no lock finding: {report}"));
    let remedy = lock["remedy"].as_str().unwrap_or_default();
    assert!(remedy.contains("public.__pbps_lock"), "{report}");
    assert!(remedy.contains("USAGE on schema public"), "{report}");
    let text = report.to_string();
    for foreign in ["dbo.__pbps_lock", "VIEW DEFINITION"] {
        assert!(!text.contains(foreign), "{foreign:?} in {report}");
    }
    assert!(
        !text.contains("pw-306-secret"),
        "the password leaked: {report}"
    );
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn reference_data_applies_pulls_defaults_and_refuses_rows_arriving_after_the_plan() {
    let own = OwnDatabase::new(&server(), "invariant_data");
    let connection = own.connection();
    let declared = "table: app.t\ncolumns:\n  code: {type: varchar(20), nullable: false}\n  label: {type: text, nullable: false, default: \"'Unlabelled'\"}\nprimary_key: {name: pk_t, columns: [code]}\n";
    let d = bootstrapped_demo(connection, "invariant-data", declared);
    d.table(&format!(
        "{declared}data:\n  mode: exact\n  rows:\n    new: {{label: New}}\n    old: {{}}\n"
    ));
    let plan = connected_artifact(&d, connection, false);
    on_server(connection, "INSERT INTO app.t (code) VALUES ('rogue')");
    let refused = approved_apply(&d, connection, &plan, &[]);
    assert_eq!(code(&refused), 1);
    assert!(
        stderr(&refused).contains("no longer the database this plan was computed against"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM app.t WHERE code IN ('new', 'old')"
        ),
        0
    );
    on_server(connection, "DELETE FROM app.t WHERE code = 'rogue'");
    succeeds(approved_apply(&d, connection, &plan, &[]));
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM app.t WHERE (code = 'new' AND label = 'New') OR (code = 'old' AND label = 'Unlabelled')"
        ),
        2
    );
    succeeds(d.run(&["verify", "--db", connection]));
    assert!(stdout(&succeeds(d.run(&["plan", "--db", connection]))).contains("No changes"));
    on_server(
        connection,
        "UPDATE app.t SET label = 'changed' WHERE code = 'old'",
    );
    let drift = d.run(&["verify", "--db", connection]);
    assert_eq!(code(&drift), 2);
    assert!(stdout(&drift).contains("row old"), "{}", stdout(&drift));
    on_server(
        connection,
        "UPDATE app.t SET label = 'Unlabelled' WHERE code = 'old'",
    );
    let fresh = Demo::new("invariant-data-pull");
    succeeds(fresh.run(&["pull", "--db", connection, "--data", "app.t"]));
    let file = std::fs::read_to_string(fresh.dir.join("schema/app.t.yml")).unwrap();
    assert!(file.contains("mode: exact"), "{file}");
    assert!(file.contains("new: {label: New}"), "{file}");
    assert!(file.contains("old: {}"), "{file}");
    assert!(!file.contains("rogue"), "{file}");
    let missing = fresh.run(&["pull", "--db", connection, "--data", "app.nope", "--force"]);
    assert_eq!(code(&missing), 1);
    assert!(stderr(&missing).contains("app.nope"));
    assert_eq!(
        std::fs::read_to_string(fresh.dir.join("schema/app.t.yml")).unwrap(),
        file
    );
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_staged_schema_transfer_does_not_hide_an_unplanned_column_at_the_intermediate_name() {
    let own = OwnDatabase::new(&server(), "invariant_checkpoint_movement");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-checkpoint-movement", ONE_COLUMN);
    on_server(connection, "CREATE SCHEMA moved; CREATE SCHEMA witness");
    d.table(&ONE_COLUMN.replace("table: app.t", "table: moved.u\nrenamed_from: app.t"));
    let plan = connected_artifact(&d, connection, true);
    // This lands inside the first emitted statement. The next checkpoint
    // must catch it even though that statement also moved the table's name.
    on_server(
        connection,
        r#"
        CREATE FUNCTION witness.add_rogue() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN
          IF to_regclass('moved.t') IS NOT NULL AND NOT EXISTS (
            SELECT FROM pg_attribute WHERE attrelid = 'moved.t'::regclass AND attname = 'rogue' AND NOT attisdropped
          ) THEN ALTER TABLE moved.t ADD COLUMN rogue text; END IF;
        END $$;
        CREATE EVENT TRIGGER add_rogue ON ddl_command_end WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION witness.add_rogue();
    "#,
    );
    let refused = approved_apply(&d, connection, &plan, &["--allow", "rename", "--staged"]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(stderr(&refused).contains("rogue"), "{}", stderr(&refused));
    let checkpoint = latest_snapshot(connection);
    assert_eq!(checkpoint.staged.as_ref().unwrap().completed, 1);
    assert!(
        checkpoint.schema.tables[&"moved.t".parse().unwrap()]
            .columns
            .contains_key("rogue")
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_class WHERE oid = to_regclass('moved.u')"
        ),
        0
    );
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_staged_rename_refuses_a_recreated_source_even_when_resuming_only_the_closing_read() {
    let own = OwnDatabase::new(&server(), "invariant_recreated_source");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-recreated-source", ONE_COLUMN);
    on_server(connection, "CREATE SCHEMA moved; CREATE SCHEMA witness");
    d.table(&ONE_COLUMN.replace("table: app.t", "table: moved.u\nrenamed_from: app.t"));
    let plan = connected_artifact(&d, connection, true);
    on_server(
        connection,
        r#"
        CREATE FUNCTION witness.recreate_source() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN
          IF to_regclass('moved.t') IS NOT NULL AND to_regclass('app.t') IS NULL THEN
            CREATE TABLE app.t (impostor text);
          END IF;
        END $$;
        CREATE EVENT TRIGGER recreate_source ON ddl_command_end WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION witness.recreate_source();
    "#,
    );
    for resume in [false, true] {
        let mut flags = vec!["--allow", "rename", "--staged"];
        if resume {
            flags.push("--resume");
        }
        let refused = approved_apply(&d, connection, &plan, &flags);
        assert_eq!(
            code(&refused),
            1,
            "{}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(stderr(&refused).contains("app.t"), "{}", stderr(&refused));
        let checkpoint = latest_snapshot(connection);
        assert_eq!(checkpoint.staged.as_ref().unwrap().completed, 2);
        assert!(
            checkpoint
                .schema
                .tables
                .contains_key(&"moved.u".parse().unwrap())
        );
        assert!(
            !checkpoint
                .schema
                .tables
                .contains_key(&"app.t".parse().unwrap())
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM public.__pbps_state WHERE kind = 'apply'"
            ),
            0
        );
    }
    // An unrelated unmanaged table does not become drift when the source is repaired.
    on_server(
        connection,
        "DROP EVENT TRIGGER recreate_source; DROP TABLE app.t; CREATE TABLE witness.unrelated (id bigint)",
    );
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "rename", "--staged", "--resume"],
    ));
    assert_eq!(
        latest_snapshot(connection).kind,
        pbps_model::StateKind::Apply
    );
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_transactional_rename_rolls_back_when_a_trigger_recreates_its_source() {
    let own = OwnDatabase::new(&server(), "invariant_recreated_transaction");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-recreated-transaction", ONE_COLUMN);
    on_server(
        connection,
        "CREATE SCHEMA moved; CREATE SCHEMA witness; INSERT INTO app.t VALUES (42)",
    );
    d.table(&ONE_COLUMN.replace("table: app.t", "table: moved.u\nrenamed_from: app.t"));
    let plan = connected_artifact(&d, connection, false);
    on_server(
        connection,
        r#"
        CREATE FUNCTION witness.recreate_source() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN
          IF to_regclass('moved.t') IS NOT NULL AND to_regclass('app.t') IS NULL THEN
            CREATE TABLE app.t (impostor text);
          END IF;
        END $$;
        CREATE EVENT TRIGGER recreate_source ON ddl_command_end WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION witness.recreate_source();
    "#,
    );
    let refused = approved_apply(&d, connection, &plan, &["--allow", "rename"]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(stderr(&refused).contains("app.t"), "{}", stderr(&refused));
    assert_eq!(
        scalar(connection, "SELECT count(*) FROM app.t WHERE id = 42"),
        1
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_class WHERE oid IN (to_regclass('moved.t'), to_regclass('moved.u'))"
        ),
        0
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM public.__pbps_state WHERE kind = 'apply'"
        ),
        0
    );
    assert_eq!(
        latest_snapshot(connection).kind,
        pbps_model::StateKind::Failed
    );
    on_server(connection, "DROP EVENT TRIGGER recreate_source");
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "rename"],
    ));
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_staged_rename_refuses_a_recreated_intermediate_name_at_closing_and_resume() {
    let own = OwnDatabase::new(&server(), "invariant_recreated_intermediate");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "invariant-recreated-intermediate", ONE_COLUMN);
    on_server(connection, "CREATE SCHEMA moved; CREATE SCHEMA witness");
    d.table(&ONE_COLUMN.replace("table: app.t", "table: moved.u\nrenamed_from: app.t"));
    let plan = connected_artifact(&d, connection, true);
    on_server(
        connection,
        r#"
        CREATE FUNCTION witness.recreate_intermediate() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN
          IF to_regclass('moved.u') IS NOT NULL AND to_regclass('moved.t') IS NULL THEN
            CREATE TABLE moved.t (impostor text);
          END IF;
        END $$;
        CREATE EVENT TRIGGER recreate_intermediate ON ddl_command_end WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION witness.recreate_intermediate();
    "#,
    );
    for resume in [false, true] {
        let mut flags = vec!["--allow", "rename", "--staged"];
        if resume {
            flags.push("--resume");
        }
        let refused = approved_apply(&d, connection, &plan, &flags);
        assert_eq!(
            code(&refused),
            1,
            "{}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(stderr(&refused).contains("moved.t"), "{}", stderr(&refused));
        let checkpoint = latest_snapshot(connection);
        assert_eq!(checkpoint.staged.as_ref().unwrap().completed, 2);
        assert!(
            checkpoint
                .schema
                .tables
                .contains_key(&"moved.u".parse().unwrap())
        );
        assert!(
            !checkpoint
                .schema
                .tables
                .contains_key(&"moved.t".parse().unwrap())
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM public.__pbps_state WHERE kind = 'apply'"
            ),
            0
        );
    }
    on_server(
        connection,
        "DROP EVENT TRIGGER recreate_intermediate; DROP TABLE moved.t",
    );
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--allow", "rename", "--staged", "--resume"],
    ));
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn unapproved_data_triggers_cannot_use_the_deployers_privileges() {
    struct Roles(String, Vec<String>);
    impl Drop for Roles {
        fn drop(&mut self) {
            for role in &self.1 {
                let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {role}"));
            }
        }
    }
    let admin = server();
    let deployer = format!("pbps_trigger_deployer_{}", std::process::id());
    let attacker = format!("pbps_trigger_attacker_{}", std::process::id());
    let _roles = Roles(admin.clone(), vec![deployer.clone(), attacker.clone()]);
    on_server(
        &admin,
        &format!(
            "CREATE ROLE {deployer} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD 'trigger-test'; \
         CREATE ROLE {attacker} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD 'trigger-test'"
        ),
    );
    for (operation, before, after, allow) in [
        (
            "INSERT",
            "",
            "data:\n  mode: exact\n  rows:\n    new: {label: New}\n",
            "",
        ),
        (
            "UPDATE",
            "data:\n  mode: exact\n  rows:\n    old: {label: Old}\n",
            "data:\n  mode: exact\n  rows:\n    old: {label: New}\n",
            "data-update",
        ),
        (
            "DELETE",
            "data:\n  mode: exact\n  rows:\n    old: {label: Old}\n",
            "data:\n  mode: exact\n  rows: {}\n",
            "data-delete",
        ),
    ] {
        for staged in [false, true] {
            let slug = format!("trigger_{}_{}", operation.to_lowercase(), staged);
            let own = OwnDatabase::new(&admin, &slug);
            let connection = own.connection();
            on_server(
                connection,
                &format!(
                    "CREATE SCHEMA app AUTHORIZATION {deployer}; CREATE SCHEMA attacker AUTHORIZATION {attacker}; \
                 GRANT USAGE ON SCHEMA app TO {attacker}; GRANT USAGE ON SCHEMA attacker TO {deployer}; \
                 GRANT CREATE ON SCHEMA public TO {deployer}; \
                 CREATE TABLE public.secret(value text); INSERT INTO public.secret VALUES ('test-only-secret'); \
                 GRANT SELECT ON public.secret TO {deployer}; \
                 CREATE TABLE attacker.leaked(value text); ALTER TABLE attacker.leaked OWNER TO {attacker}; \
                 GRANT INSERT ON attacker.leaked TO {deployer}"
                ),
            );
            let login = |role: &str| {
                format!(
                    "{} user={role} password=trigger-test",
                    connection
                        .split_whitespace()
                        .filter(|w| !w.starts_with("user=") && !w.starts_with("password="))
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            };
            let deployment = login(&deployer);
            let attack = login(&attacker);
            assert_eq!(
                scalar(
                    &deployment,
                    "SELECT count(*) FROM pg_roles WHERE rolname = current_user AND (rolsuper OR rolcreaterole OR rolcreatedb OR rolreplication)"
                ),
                0
            );
            assert!(try_on_server(&attack, "SELECT * FROM public.secret").is_err());
            let d = Demo::new(&slug);
            std::fs::write(
                d.dir.join("pbps.yml"),
                format!(
                    "dialect: postgres\nunmanaged: {}\n",
                    if staged { "warn" } else { "ignore" }
                ),
            )
            .unwrap();
            d.keyed(&deployment);
            let declared = "table: app.t\ncolumns:\n  code: {type: varchar(20), nullable: false}\n  label: {type: text, nullable: false}\nprimary_key: {name: pk_t, columns: [code]}\n";
            d.table(&format!("{declared}{before}"));
            succeeds(d.run(&["plan"]));
            d.commit();
            succeeds(d.run(&["bootstrap", "--db", &deployment]));
            // The function exists before approval and is pinned with it
            // (DEC-319.1). What arrives afterwards is only the trigger that
            // calls it: a new call site on an unchanged routine, which is this
            // guard's to refuse (DECISIONS 445), not the pins'.
            on_server(
                &attack,
                "CREATE FUNCTION attacker.steal() RETURNS trigger LANGUAGE plpgsql SECURITY INVOKER AS \
                 $$BEGIN INSERT INTO attacker.leaked SELECT value FROM public.secret; \
                 IF TG_OP = 'DELETE' THEN RETURN OLD; ELSE RETURN NEW; END IF; END$$",
            );
            d.table(&format!("{declared}{after}"));
            let plan = connected_artifact(&d, &deployment, staged);
            // Installed after approval by a role that cannot read the secret.
            on_server(connection, &format!("GRANT TRIGGER ON app.t TO {attacker}"));
            let level = if staged { "STATEMENT" } else { "ROW" };
            on_server(
                &attack,
                &format!(
                    "CREATE TRIGGER steal BEFORE {operation} ON app.t FOR EACH {level} EXECUTE FUNCTION attacker.steal()"
                ),
            );
            if operation == "INSERT" && !staged {
                use pbps_dialect::Dialect;
                // Merely recording this trigger cannot make its independently
                // replaceable invoker function safe to run as the deployer.
                tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                    let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &deployment).await.unwrap();
                    let recorded = pbps_pg::catalog::introspect(&mut conn).await.unwrap().schema;
                    let dialect = pbps_pg::Postgres::new();
                    conn.begin(dialect.transaction_framing()).await.unwrap();
                    let refused = pbps_pg::data_triggers::prepare(&mut conn, &[pbps_dialect::RowWrite {
                        table: pbps_model::TableName::new("app", "t"), operation: pbps_dialect::RowOperation::Insert,
                    }], &recorded, &Default::default()).await;
                    assert!(refused.is_err(), "recording a lower-privileged owner's invoker trigger must not authorize it");
                    conn.rollback(dialect.transaction_framing()).await.unwrap();
                });
            }
            let blocked_plan = d.run(&["plan", "--db", &deployment]);
            assert_eq!(code(&blocked_plan), 1);
            assert!(
                stderr(&blocked_plan).contains("unsafe data trigger"),
                "{}",
                stderr(&blocked_plan)
            );
            let mut extra = Vec::new();
            if staged {
                extra.push("--staged");
            }
            if !allow.is_empty() {
                extra.extend(["--allow", allow]);
            }
            let refused = approved_apply(&d, &deployment, &plan, &extra);
            assert_eq!(
                scalar(connection, "SELECT count(*) FROM attacker.leaked"),
                0,
                "{operation} staged={staged} leaked despite approval boundary: {}{}",
                stdout(&refused),
                stderr(&refused)
            );
            assert_eq!(
                code(&refused),
                1,
                "{}{}",
                stdout(&refused),
                stderr(&refused)
            );
            assert!(
                stderr(&refused).contains("trigger") && stderr(&refused).contains("steal"),
                "{}",
                stderr(&refused)
            );
            assert_eq!(
                scalar(
                    connection,
                    "SELECT count(*) FROM public.__pbps_state WHERE kind = 'apply'"
                ),
                0
            );
            on_server(&deployment, "DROP TRIGGER steal ON app.t");
            let other_event = if operation == "INSERT" {
                "UPDATE"
            } else {
                "INSERT"
            };
            on_server(
                &deployment,
                &format!(
                    "CREATE TRIGGER disabled BEFORE {operation} ON app.t FOR EACH ROW EXECUTE FUNCTION attacker.steal();                  ALTER TABLE app.t DISABLE TRIGGER disabled;                  CREATE TRIGGER another_event BEFORE {other_event} ON app.t FOR EACH ROW EXECUTE FUNCTION attacker.steal();                  CREATE TABLE app.unrelated (id integer);                  CREATE TRIGGER unrelated BEFORE {operation} ON app.unrelated FOR EACH ROW EXECUTE FUNCTION attacker.steal()"
                ),
            );
            succeeds(approved_apply(&d, &deployment, &plan, &extra));
            assert_eq!(
                scalar(connection, "SELECT count(*) FROM attacker.leaked"),
                0
            );
            succeeds(d.run(&["verify", "--db", &deployment]));
        }
    }
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn recorded_triggers_run_but_their_names_cannot_authorize_replacements() {
    let own = OwnDatabase::new(&server(), "recorded_triggers");
    let connection = own.connection();
    let declared = "table: app.t\ncolumns:\n  code: {type: varchar(20), nullable: false}\n  label: {type: text, nullable: false}\nprimary_key: {name: pk_t, columns: [code]}\n";
    let d = bootstrapped_demo(connection, "recorded-triggers", declared);
    on_server(
        connection,
        "CREATE TABLE app.calls (value integer); \
        CREATE FUNCTION app.record_call() RETURNS trigger LANGUAGE plpgsql AS \
        $$BEGIN INSERT INTO app.calls VALUES (1); RETURN NEW; END$$; \
        CREATE TRIGGER record_call BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.record_call()",
    );
    succeeds(d.run(&["pull", "--db", connection, "--force"]));
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "adopt the intended trigger",
    ]));
    d.table(&format!(
        "{declared}data:\n  mode: exact\n  rows:\n    first: {{label: First}}\n"
    ));
    let plan = connected_artifact(&d, connection, false);
    succeeds(approved_apply(&d, connection, &plan, &[]));
    assert_eq!(scalar(connection, "SELECT count(*) FROM app.calls"), 1);
    succeeds(d.run(&["verify", "--db", connection]));
    d.table(&format!("{declared}data:\n  mode: exact\n  rows:\n    first: {{label: First}}\n    second: {{label: Second}}\n"));
    let plan = connected_artifact(&d, connection, false);
    on_server(
        connection,
        "CREATE FUNCTION app.replacement() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$; \
        CREATE OR REPLACE TRIGGER record_call BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.replacement()",
    );
    let refused = approved_apply(&d, connection, &plan, &[]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM app.t WHERE code = 'second'"
        ),
        0
    );
    assert_eq!(scalar(connection, "SELECT count(*) FROM app.calls"), 1);
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_trigger_guard_holds_off_replacement_until_the_write_finishes() {
    use pbps_dialect::{Dialect, RowOperation, RowWrite};
    let own = OwnDatabase::new(&server(), "trigger_lock");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; CREATE TABLE app.t (id integer); \
        CREATE FUNCTION app.trusted() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$; \
        CREATE FUNCTION app.replacement() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$; \
        CREATE TRIGGER guarded BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.trusted()",
    );
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let mut writer = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection).await.unwrap();
        let mut other = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection).await.unwrap();
        let baseline = pbps_pg::catalog::introspect(&mut writer).await.unwrap().schema;
        let dialect = pbps_pg::Postgres::new();
        let mut write = RowWrite { table: pbps_model::TableName::new("app", "t"), operation: RowOperation::Insert };
        writer.begin(dialect.transaction_framing()).await.unwrap();
        let guard = pbps_pg::data_triggers::prepare(&mut writer, std::slice::from_ref(&write), &baseline, &Default::default()).await.unwrap();
        other.execute("SET lock_timeout = '100ms'").await.unwrap();
        let replace = "CREATE OR REPLACE TRIGGER guarded BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.replacement()";
        let blocked = other.execute(replace).await.unwrap_err();
        assert!(matches!(blocked, pbps_db::DbError::Driver { ref code, .. } if code.as_deref() == Some("55P03")), "{blocked}");
            writer.execute("ALTER TABLE app.t RENAME TO renamed").await.unwrap();
            write.table = pbps_model::TableName::new("app", "renamed");
        pbps_pg::data_triggers::check(&mut writer, &write, &guard).await.unwrap();
        writer.execute("INSERT INTO app.renamed VALUES (1)").await.unwrap();
        writer.commit(dialect.transaction_framing()).await.unwrap();
        // After release the same replacement succeeds, proving the first
        // failure came from the guard's lock, not invalid DDL or permissions.
        let baseline = pbps_pg::catalog::introspect(&mut writer).await.unwrap().schema;
            other.execute(&replace.replace("ON app.t", "ON app.renamed")).await.unwrap();
        writer.begin(dialect.transaction_framing()).await.unwrap();
        let refused = pbps_pg::data_triggers::prepare(&mut writer, &[write], &baseline, &Default::default()).await;
        assert!(refused.is_err());
        writer.rollback(dialect.transaction_framing()).await.unwrap();
    });
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn bootstrap_checks_triggers_that_arrive_with_a_created_table() {
    let own = OwnDatabase::new(&server(), "bootstrap_trigger");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; CREATE TABLE public.marker (value integer); \
        CREATE FUNCTION public.row_hook() RETURNS trigger LANGUAGE plpgsql AS \
        $$BEGIN INSERT INTO public.marker VALUES (1); RETURN NEW; END$$; \
        CREATE FUNCTION public.attach_hook() RETURNS event_trigger LANGUAGE plpgsql AS $$ \
        BEGIN \
          IF EXISTS (SELECT 1 FROM pg_event_trigger_ddl_commands() d JOIN pg_class c ON c.oid = d.objid \
                     JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'app' AND c.relname = 't') THEN \
            EXECUTE 'CREATE TRIGGER unexpected BEFORE INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION public.row_hook()'; \
          END IF; \
        END$$; \
        CREATE EVENT TRIGGER attach_hook ON ddl_command_end WHEN TAG IN ('CREATE TABLE') EXECUTE FUNCTION public.attach_hook()",
    );
    let d = Demo::new("bootstrap-trigger");
    d.table("table: app.t\ncolumns:\n  code: {type: text, nullable: false}\nprimary_key: {name: pk_t, columns: [code]}\ndata:\n  mode: exact\n  rows:\n    first: {}\n");
    succeeds(d.run(&["plan"]));
    d.commit();
    let refused = d.run(&["bootstrap", "--db", connection]);
    assert_eq!(
        code(&refused),
        1,
        "{}{}",
        stdout(&refused),
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("unexpected"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(scalar(connection, "SELECT count(*) FROM public.marker"), 0);
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_class WHERE oid = to_regclass('app.t')"
        ),
        0
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM public.__pbps_state WHERE kind = 'bootstrap'"
        ),
        0
    );
    on_server(connection, "DROP EVENT TRIGGER attach_hook");
    succeeds(d.run(&["bootstrap", "--db", connection]));
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn update_of_triggers_only_guard_columns_the_statement_can_fire() {
    for staged in [false, true] {
        for level in ["ROW", "STATEMENT"] {
            let own = OwnDatabase::new(&server(), &format!("update_of_{staged}_{level}"));
            let connection = own.connection();
            let declared = "table: app.t\ncolumns:\n  code: {type: text, nullable: false}\n  label: {type: text, nullable: false}\n  audit: {type: text, nullable: true}\nprimary_key: {name: pk_t, columns: [code]}\ndata:\n  mode: exact\n  rows:\n";
            let d = bootstrapped_demo(
                connection,
                "update-of",
                &format!("{declared}    first: {{label: Old}}\n"),
            );
            on_server(
                connection,
                &format!(
                    "CREATE TABLE public.fired (value integer); \
                 CREATE FUNCTION public.mark() RETURNS trigger LANGUAGE plpgsql AS \
                 $$BEGIN INSERT INTO public.fired VALUES (1); RETURN NEW; END$$; \
                 CREATE TRIGGER untouched BEFORE UPDATE OF audit ON app.t FOR EACH {level} EXECUTE FUNCTION public.mark(); \
                 BEGIN; UPDATE app.t SET label = 'Measured'; SELECT 1; COMMIT;"
                ),
            );
            assert_eq!(scalar(connection, "SELECT count(*) FROM public.fired"), 0);
            on_server(
                connection,
                "UPDATE app.t SET label = 'Old'; UPDATE app.t SET audit = NULL",
            );
            assert_eq!(scalar(connection, "SELECT count(*) FROM public.fired"), 1);
            on_server(connection, "TRUNCATE public.fired");
            d.table(&format!("{declared}    first: {{label: New}}\n"));
            let plan = connected_artifact(&d, connection, staged);
            let extra = if staged {
                vec!["--staged", "--allow", "data-update"]
            } else {
                vec!["--allow", "data-update"]
            };
            succeeds(approved_apply(&d, connection, &plan, &extra));
            assert_eq!(scalar(connection, "SELECT count(*) FROM public.fired"), 0);
            succeeds(d.run(&["verify", "--db", connection]));
            d.table(&format!("{declared}    first: {{label: Next}}\n"));
            let plan = connected_artifact(&d, connection, staged);
            on_server(
                connection,
                &format!(
                    "CREATE OR REPLACE TRIGGER untouched BEFORE UPDATE OF audit, label ON app.t FOR EACH {level} EXECUTE FUNCTION public.mark()"
                ),
            );
            let refused = approved_apply(&d, connection, &plan, &extra);
            assert_eq!(
                code(&refused),
                1,
                "{}{}",
                stdout(&refused),
                stderr(&refused)
            );
            assert!(
                stderr(&refused).contains("unsafe data trigger"),
                "{}",
                stderr(&refused)
            );
            assert_eq!(scalar(connection, "SELECT count(*) FROM public.fired"), 0);
            assert_eq!(
                scalar(connection, "SELECT count(*) FROM app.t WHERE label = 'New'"),
                1
            );
        }
    }
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn update_of_generated_columns_follows_their_source_columns() {
    use pbps_dialect::{Dialect, RowOperation, RowWrite};
    let own = OwnDatabase::new(&server(), "update_of_generated");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; CREATE TABLE app.t (label text, audit text, size integer GENERATED ALWAYS AS (length(label)) STORED); \
         INSERT INTO app.t (label) VALUES ('Old'); CREATE TABLE public.fired (value integer); \
         CREATE FUNCTION public.mark() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN INSERT INTO public.fired VALUES (1); RETURN NEW; END$$; \
         CREATE TRIGGER derived AFTER UPDATE OF size ON app.t FOR EACH ROW EXECUTE FUNCTION public.mark(); \
         UPDATE app.t SET audit = 'untouched'",
    );
    assert_eq!(scalar(connection, "SELECT count(*) FROM public.fired"), 0);
    on_server(connection, "UPDATE app.t SET label = 'Measured'");
    assert_eq!(scalar(connection, "SELECT count(*) FROM public.fired"), 1);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
            .await
            .unwrap();
        let dialect = pbps_pg::Postgres::new();
        for (column, allowed) in [("audit", true), ("label", false)] {
            let write = RowWrite {
                table: pbps_model::TableName::new("app", "t"),
                operation: RowOperation::Update {
                    columns: [column.to_owned()].into(),
                },
            };
            conn.begin(dialect.transaction_framing()).await.unwrap();
            let checked = pbps_pg::data_triggers::prepare(
                &mut conn,
                &[write],
                &Default::default(),
                &Default::default(),
            )
            .await;
            assert_eq!(
                checked.is_ok(),
                allowed,
                "column {column}: {:?}",
                checked.err()
            );
            conn.rollback(dialect.transaction_framing()).await.unwrap();
        }
        conn.execute("CREATE FUNCTION public.noop() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$; CREATE TRIGGER before_update BEFORE UPDATE ON app.t FOR EACH ROW EXECUTE FUNCTION public.noop(); ALTER TABLE app.t DISABLE TRIGGER before_update; TRUNCATE public.fired; UPDATE app.t SET audit = 'again'").await.unwrap();
        let rows = conn.query("SELECT count(*)::int8 AS n FROM public.fired").await.unwrap();
        assert_eq!(rows[0].try_get::<i64>("n").unwrap(), Some(1));
        let write = RowWrite {
            table: pbps_model::TableName::new("app", "t"),
            operation: RowOperation::Update { columns: ["audit".to_owned()].into() },
        };
        conn.begin(dialect.transaction_framing()).await.unwrap();
        let checked = pbps_pg::data_triggers::prepare(&mut conn, &[write], &Default::default(), &Default::default()).await;
        assert!(checked.is_err(), "a disabled BEFORE ROW trigger still widens generated-column updates");
        conn.rollback(dialect.transaction_framing()).await.unwrap();
        // Unless the plan drops it first: `DropModule` is `order_key` 0, so by
        // the row statement the widener is gone and `size` is not touched.
        let write = RowWrite {
            table: pbps_model::TableName::new("app", "t"),
            operation: RowOperation::Update { columns: ["audit".to_owned()].into() },
        };
        let removed = pbps_pg::data_triggers::Dropped {
            modules: [pbps_model::ModuleId::Trigger {
                on: pbps_model::TableName::new("app", "t"),
                name: "before_update".to_owned(),
            }]
            .into(),
            ..Default::default()
        };
        conn.begin(dialect.transaction_framing()).await.unwrap();
        let checked = pbps_pg::data_triggers::prepare(&mut conn, &[write], &Default::default(), &removed).await;
        assert!(checked.is_ok(), "a widener the plan drops widens nothing: {:?}", checked.err().map(|e| e.to_string()));
        conn.rollback(dialect.transaction_framing()).await.unwrap();
    });
}

/// Cluster-wide roles outlive the database that uses them, so a test holding
/// both drops the database first: `DROP ROLE` fails while the role still owns
/// an object anywhere in the cluster.
struct ClusterRoles {
    server: String,
    names: Vec<String>,
}

impl Drop for ClusterRoles {
    fn drop(&mut self) {
        for name in &self.names {
            let _ = try_on_server(&self.server, &format!("DROP ROLE IF EXISTS {name}"));
        }
    }
}

/// The membership topology DECISIONS 450 is about: `owner` owns the trigger
/// function and can act as the deployer, and `inheritor` and `distant` are the
/// roles each case hands the owner's rights to. Fields drop in declaration
/// order, so the database goes before the roles that own objects inside it.
struct Topology {
    own: OwnDatabase,
    _roles: ClusterRoles,
    deployer: String,
    owner: String,
    inheritor: String,
    distant: String,
    deployment: String,
}

impl Topology {
    fn connection(&self) -> &str {
        self.own.connection()
    }
}

/// A non-superuser deployer, a separate function owner that can `SET ROLE` to
/// it, and two roles that hold no membership yet. The trigger function is
/// `hook.audit`, owned by `owner` and invoker-rights like any plpgsql trigger
/// function, so its body runs with whatever privileges the deployer brings.
fn inherited_owner_topology(admin: &str, slug: &str) -> Topology {
    let pid = std::process::id();
    let deployer = format!("pbps_{slug}_deployer_{pid}");
    let owner = format!("pbps_{slug}_owner_{pid}");
    let inheritor = format!("pbps_{slug}_inheritor_{pid}");
    let distant = format!("pbps_{slug}_distant_{pid}");
    let roles = ClusterRoles {
        server: admin.to_owned(),
        names: vec![
            distant.clone(),
            inheritor.clone(),
            owner.clone(),
            deployer.clone(),
        ],
    };
    on_server(
        admin,
        &format!(
            "CREATE ROLE {deployer} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD 'trigger-test'; \
             CREATE ROLE {owner} NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION; \
             CREATE ROLE {inheritor} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD 'trigger-test'; \
             CREATE ROLE {distant} NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION; \
             GRANT {deployer} TO {owner} WITH INHERIT FALSE, SET TRUE"
        ),
    );
    let own = OwnDatabase::new(admin, slug);
    on_server(
        own.connection(),
        &format!(
            "CREATE SCHEMA app AUTHORIZATION {deployer}; CREATE SCHEMA hook AUTHORIZATION {owner}; \
             GRANT USAGE ON SCHEMA hook TO {deployer}; GRANT CREATE ON SCHEMA public TO {deployer}; \
             CREATE TABLE hook.calls (value integer); ALTER TABLE hook.calls OWNER TO {owner}; \
             GRANT INSERT, SELECT ON hook.calls TO {deployer}; \
             CREATE FUNCTION hook.audit() RETURNS trigger LANGUAGE plpgsql AS \
             $$BEGIN INSERT INTO hook.calls VALUES (1); \
             IF TG_OP = 'DELETE' THEN RETURN OLD; ELSE RETURN NEW; END IF; END$$; \
             ALTER FUNCTION hook.audit() OWNER TO {owner}"
        ),
    );
    let deployment = as_role(own.connection(), &deployer);
    assert_eq!(
        scalar(
            &deployment,
            "SELECT count(*) FROM pg_roles WHERE rolname = current_user AND (rolsuper OR rolcreaterole OR rolcreatedb OR rolreplication)"
        ),
        0
    );
    Topology {
        own,
        _roles: roles,
        deployer,
        owner,
        inheritor,
        distant,
        deployment,
    }
}

/// The same server, reached as another role: the libpq keyword form the suite
/// is configured with, with its `user` and `password` words replaced.
fn as_role(connection: &str, role: &str) -> String {
    let kept: Vec<&str> = connection
        .split_whitespace()
        .filter(|w| !w.starts_with("user=") && !w.starts_with("password="))
        .collect();
    format!("{} user={role} password=trigger-test", kept.join(" "))
}

/// A trigger on `app.t` whose function the deployer only executes, recorded as
/// the tool would record it, so nothing but ownership rights can refuse it.
fn recorded_audit_trigger(topology: &Topology, event: &str) -> pbps_model::Schema {
    on_server(
        &topology.deployment,
        &format!(
            "CREATE TABLE app.t (id integer); \
             CREATE TRIGGER audit BEFORE {event} ON app.t FOR EACH ROW EXECUTE FUNCTION hook.audit()"
        ),
    );
    let recorded = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &topology.deployment)
                .await
                .unwrap();
            pbps_pg::catalog::introspect(&mut conn)
                .await
                .unwrap()
                .schema
        });
    let id = pbps_model::ModuleId::Trigger {
        on: pbps_model::TableName::new("app", "t"),
        name: "audit".to_owned(),
    };
    assert!(
        recorded.modules.contains_key(&id),
        "the trigger has to be recorded, or the recording check is what refuses"
    );
    recorded
}

/// DECISIONS 450. A role that inherits the function owner's rights can replace
/// the body — the OID and the recorded trigger definition do not move — so the
/// direct owner's `SET` path is not the question the guard has to answer.
///
/// The cases are ordered so that each one differs from the last in one grant,
/// and the trusted ones are the control: a predicate that simply refused every
/// membership would pass the refusals and fail here.
fn effective_ownership_decides_trust(admin: &str, slug: &str) {
    let topology = inherited_owner_topology(admin, slug);
    let recorded = recorded_audit_trigger(&topology, "INSERT");
    let Topology {
        deployer,
        owner,
        inheritor,
        distant,
        ..
    } = &topology;
    let cases: [(&str, String, bool); 6] = [
        (
            "a direct owner that can act as the deployer is trusted",
            String::new(),
            true,
        ),
        (
            "a role inheriting the owner's rights without a SET path is not",
            format!("GRANT {owner} TO {inheritor} WITH INHERIT TRUE, SET FALSE"),
            false,
        ),
        (
            "an inheritor that can itself act as the deployer is trusted",
            format!("GRANT {deployer} TO {inheritor} WITH INHERIT FALSE, SET TRUE"),
            true,
        ),
        (
            "disabled inheritance confers no ownership rights",
            format!(
                "REVOKE {deployer} FROM {inheritor}; REVOKE {owner} FROM {inheritor}; \
                 GRANT {owner} TO {inheritor} WITH INHERIT FALSE, SET FALSE"
            ),
            true,
        ),
        (
            "inheritance reaches through a trusted inheritor to an untrusted one",
            format!(
                "REVOKE {owner} FROM {inheritor}; \
                 GRANT {owner} TO {inheritor} WITH INHERIT TRUE, SET FALSE; \
                 GRANT {deployer} TO {inheritor} WITH INHERIT FALSE, SET TRUE; \
                 GRANT {inheritor} TO {distant} WITH INHERIT TRUE, SET FALSE"
            ),
            false,
        ),
        (
            "the direct owner still needs a SET path of its own",
            format!(
                "REVOKE {inheritor} FROM {distant}; REVOKE {owner} FROM {inheritor}; \
                 REVOKE {deployer} FROM {inheritor}; REVOKE {deployer} FROM {owner}"
            ),
            false,
        ),
    ];
    let connection = topology.connection().to_owned();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            use pbps_dialect::Dialect;
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &topology.deployment)
                .await
                .unwrap();
            // Inside this runtime rather than through `on_server`: a nested
            // `block_on` panics, and the grants have to land between the checks.
            let mut admin = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &connection)
                .await
                .unwrap();
            let dialect = pbps_pg::Postgres::new();
            for (property, grant, trusted) in cases {
                if !grant.is_empty() {
                    admin.execute(&grant).await.unwrap();
                }
                conn.begin(dialect.transaction_framing()).await.unwrap();
                let checked = pbps_pg::data_triggers::prepare(
                    &mut conn,
                    &[pbps_dialect::RowWrite {
                        table: pbps_model::TableName::new("app", "t"),
                        operation: pbps_dialect::RowOperation::Insert,
                    }],
                    &recorded,
                    &Default::default(),
                )
                .await;
                let refusal = checked.as_ref().err().map(|e| e.to_string());
                conn.rollback(dialect.transaction_framing()).await.unwrap();
                assert_eq!(checked.is_ok(), trusted, "{property}: {refusal:?}");
                if let Some(message) = refusal {
                    assert!(message.contains("unsafe data trigger"), "{message}");
                }
            }
        });
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn effective_function_ownership_not_the_direct_owner_decides_trigger_trust() {
    effective_ownership_decides_trust(&server(), "effective_owner");
}

/// The `INHERIT`/`SET` grant options arrived together in PostgreSQL 16, and the
/// predicate has to mean the same thing on the oldest engine that has them: a
/// release where it silently read as "always trusted" would leave the hole open
/// on every supported server but one.
#[test]
#[ignore = "needs PostgreSQL 16; set PBPS_TEST_PG_OLD_DB"]
fn effective_function_ownership_decides_trigger_trust_before_postgres_17() {
    let server = std::env::var("PBPS_TEST_PG_OLD_DB").expect("PBPS_TEST_PG_OLD_DB");
    on_server(
        &server,
        "DO $$ BEGIN IF current_setting('server_version_num')::integer >= 170000 THEN RAISE EXCEPTION 'this regression needs a pre-17 server'; END IF; END $$",
    );
    effective_ownership_decides_trust(&server, "effective_owner_old");
}

/// Planning is not the last word: a membership granted after the plan was
/// approved is caught by the re-read the writer does under its own lock. The
/// lock cannot help here — `GRANT` does not touch the guarded table — which is
/// why the check is repeated rather than trusted from planning time.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_membership_granted_after_planning_is_refused_before_the_row_write() {
    let topology = inherited_owner_topology(&server(), "late_membership");
    let recorded = recorded_audit_trigger(&topology, "INSERT");
    let connection = topology.connection().to_owned();
    let grant = format!(
        "GRANT {} TO {} WITH INHERIT TRUE, SET FALSE",
        topology.owner, topology.inheritor
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            use pbps_dialect::Dialect;
            let mut writer =
                pbps_db::Conn::connect(pbps_db::Driver::Postgres, &topology.deployment)
                    .await
                    .unwrap();
            let dialect = pbps_pg::Postgres::new();
            let write = pbps_dialect::RowWrite {
                table: pbps_model::TableName::new("app", "t"),
                operation: pbps_dialect::RowOperation::Insert,
            };
            writer.begin(dialect.transaction_framing()).await.unwrap();
            let guard = pbps_pg::data_triggers::prepare(
                &mut writer,
                std::slice::from_ref(&write),
                &recorded,
                &Default::default(),
            )
            .await
            .expect("the topology is trusted while planning");
            // From another session, so the guarded table's lock is the one thing
            // that could have stopped it, and it does not.
            pbps_db::Conn::connect(pbps_db::Driver::Postgres, &connection)
                .await
                .unwrap()
                .execute(&grant)
                .await
                .unwrap();
            let refused = pbps_pg::data_triggers::check(&mut writer, &write, &guard).await;
            writer
                .rollback(dialect.transaction_framing())
                .await
                .unwrap();
            let message = refused.expect_err("the write must not run").to_string();
            assert!(message.contains("unsafe data trigger"), "{message}");
        });
}

/// The path a user types, on the topology of DECISIONS 450. The trigger is
/// installed and adopted while nothing can replace its function, so the
/// recording check and the direct owner's `SET` path both pass; only then does a
/// role that inherits the owner's rights take the body over. Every row DML kind
/// has to refuse, and refuse before the body copies a secret that only the
/// deployment role can read.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn an_inherited_trigger_function_owner_cannot_use_the_deployers_privileges() {
    let admin = server();
    let declared = "table: app.t\ncolumns:\n  code: {type: varchar(20), nullable: false}\n  label: {type: text, nullable: false}\nprimary_key: {name: pk_t, columns: [code]}\n";
    for (operation, before, after, allow) in [
        (
            "INSERT",
            "",
            "data:\n  mode: exact\n  rows:\n    new: {label: New}\n",
            "",
        ),
        (
            "UPDATE",
            "data:\n  mode: exact\n  rows:\n    old: {label: Old}\n",
            "data:\n  mode: exact\n  rows:\n    old: {label: New}\n",
            "data-update",
        ),
        (
            "DELETE",
            "data:\n  mode: exact\n  rows:\n    old: {label: Old}\n",
            "data:\n  mode: exact\n  rows: {}\n",
            "data-delete",
        ),
    ] {
        let slug = format!("inherited_{}", operation.to_lowercase());
        let topology = inherited_owner_topology(&admin, &slug);
        let connection = topology.connection().to_owned();
        let deployment = topology.deployment.clone();
        let d = Demo::new(&slug);
        std::fs::write(
            d.dir.join("pbps.yml"),
            "dialect: postgres\nunmanaged: ignore\n",
        )
        .unwrap();
        d.keyed(&deployment);
        d.table(declared);
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", &deployment]));
        // The trigger arrives the way an operator installs one, and is adopted
        // from the engine rather than written by hand: the guard compares the
        // deparsed definition, and only a pulled one is that text exactly.
        on_server(
            &deployment,
            &format!(
                "CREATE TRIGGER audit BEFORE {operation} ON app.t FOR EACH ROW EXECUTE FUNCTION hook.audit()"
            ),
        );
        // Pulled into a project of its own, and only the trigger is carried
        // across. The function stays unmanaged: a managed body would make the
        // replacement read as ordinary drift, and the refusal under test would
        // never be the thing that stopped the write. Deleting the pulled files
        // instead would ask for drop intent, which is a different test.
        let pulled = Demo::new(&format!("{slug}-pull"));
        succeeds(pulled.run(&["pull", "--db", &deployment]));
        let module = std::fs::read_dir(pulled.dir.join("schema"))
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                let body = std::fs::read_to_string(&path).ok()?;
                body.lines()
                    .any(|line| line.starts_with("trigger:"))
                    .then_some(body)
            })
            .next()
            .expect("the engine's own spelling of the trigger");
        std::fs::write(d.dir.join("schema/audit.yml"), module).unwrap();
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            &deployment,
            "--reason",
            "adopt the audit trigger",
        ]));
        succeeds(d.run(&["verify", "--db", &deployment]));
        // Built after the adoption, so none of it is managed or recorded: a
        // secret the deployer alone can read, and a table only the inheritor
        // owns to read it back out of.
        let Topology {
            deployer,
            owner,
            inheritor,
            ..
        } = &topology;
        on_server(
            &connection,
            &format!(
                "CREATE SCHEMA secrets; CREATE TABLE secrets.vault (value text); \
                 INSERT INTO secrets.vault VALUES ('test-only-secret'); \
                 GRANT USAGE ON SCHEMA secrets TO {deployer}; GRANT SELECT ON secrets.vault TO {deployer}; \
                 CREATE SCHEMA leak AUTHORIZATION {inheritor}; CREATE TABLE leak.copied (value text); \
                 ALTER TABLE leak.copied OWNER TO {inheritor}; \
                 GRANT USAGE ON SCHEMA leak TO {deployer}; GRANT INSERT ON leak.copied TO {deployer}"
            ),
        );
        let attack = as_role(&connection, inheritor);
        assert!(try_on_server(&attack, "SELECT * FROM secrets.vault").is_err());
        if !before.is_empty() {
            d.table(&format!("{declared}{before}"));
            let seed = connected_artifact(&d, &deployment, false);
            succeeds(approved_apply(&d, &deployment, &seed, &[]));
        }
        d.table(&format!("{declared}{after}"));
        let plan = connected_artifact(&d, &deployment, false);
        let applies = scalar(
            &connection,
            "SELECT count(*) FROM public.__pbps_state WHERE kind = 'apply'",
        );
        // Approved, recorded, unchanged — and now owned in effect by a role
        // that cannot act as the deployer.
        on_server(
            &connection,
            &format!("GRANT {owner} TO {inheritor} WITH INHERIT TRUE, SET FALSE"),
        );
        on_server(
            &attack,
            "CREATE OR REPLACE FUNCTION hook.audit() RETURNS trigger LANGUAGE plpgsql AS \
             $$BEGIN INSERT INTO leak.copied SELECT value FROM secrets.vault; \
             IF TG_OP = 'DELETE' THEN RETURN OLD; ELSE RETURN NEW; END IF; END$$",
        );
        let blocked = d.run(&["plan", "--db", &deployment]);
        assert_eq!(
            code(&blocked),
            1,
            "{}{}",
            stdout(&blocked),
            stderr(&blocked)
        );
        assert!(
            stderr(&blocked).contains("unsafe data trigger"),
            "{}",
            stderr(&blocked)
        );
        let mut extra = Vec::new();
        if !allow.is_empty() {
            extra.extend(["--allow", allow]);
        }
        let refused = approved_apply(&d, &deployment, &plan, &extra);
        assert_eq!(
            scalar(&connection, "SELECT count(*) FROM leak.copied"),
            0,
            "{operation} copied the secret: {}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert_eq!(
            code(&refused),
            1,
            "{}{}",
            stdout(&refused),
            stderr(&refused)
        );
        // The body was replaced after approval, so the routine pins refuse
        // before the probes, ahead of this guard (DEC-319.1). The guard's own
        // refusal is what stopped the plan above.
        assert!(
            stderr(&refused).contains("pinned routines changed in `hook`"),
            "{}",
            stderr(&refused)
        );
        assert_eq!(
            scalar(
                &connection,
                "SELECT count(*) FROM public.__pbps_state WHERE kind = 'apply'"
            ),
            applies,
            "a refused write must not record an apply"
        );
        // The control: the same plan, the same recorded trigger, once the
        // inherited rights are gone and the legitimate body is back. Replacing
        // a function does not move its ownership, so `owner` still owns it.
        on_server(&connection, &format!("REVOKE {owner} FROM {inheritor}"));
        on_server(
            &connection,
            "CREATE OR REPLACE FUNCTION hook.audit() RETURNS trigger LANGUAGE plpgsql AS \
             $$BEGIN INSERT INTO hook.calls VALUES (1); \
             IF TG_OP = 'DELETE' THEN RETURN OLD; ELSE RETURN NEW; END IF; END$$",
        );
        succeeds(approved_apply(&d, &deployment, &plan, &extra));
        assert_eq!(scalar(&connection, "SELECT count(*) FROM leak.copied"), 0);
        assert_eq!(
            scalar(&connection, "SELECT count(*) FROM hook.calls"),
            1,
            "the trusted trigger has to have run, or nothing was proved"
        );
        succeeds(d.run(&["verify", "--db", &deployment]));
    }
}

/// A row operation writes more tables than the one it names: PostgreSQL runs a
/// referential action on the referencing side, and a trigger there runs under
/// the deployment role just as one on the named table does (DECISIONS 451).
///
/// The delete cases carry a statement-level trigger and no referencing row on
/// purpose. A row that still points at the declared row makes the existing
/// delete preflight refuse the plan long before the guard, but the action's own
/// statement runs whether or not it matches anything — measured on 18.6 — so a
/// statement trigger on the referencing table is the delete's live reach.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn referential_actions_cannot_reach_an_unapproved_trigger() {
    struct Roles(String, Vec<String>);
    impl Drop for Roles {
        fn drop(&mut self) {
            for role in &self.1 {
                let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {role}"));
            }
        }
    }
    let admin = server();
    let deployer = format!("pbps_fk_deployer_{}", std::process::id());
    let attacker = format!("pbps_fk_attacker_{}", std::process::id());
    let _roles = Roles(admin.clone(), vec![deployer.clone(), attacker.clone()]);
    on_server(
        &admin,
        &format!(
            "CREATE ROLE {deployer} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD 'trigger-test'; \
         CREATE ROLE {attacker} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD 'trigger-test'"
        ),
    );
    // `chain` puts the trigger two foreign keys away: the first action's own
    // write is what produces the second, which only a recursive closure finds.
    for (operation, action, level, staged, chain) in [
        ("UPDATE", "CASCADE", "ROW", false, false),
        ("UPDATE", "SET NULL", "ROW", false, false),
        ("UPDATE", "SET DEFAULT", "ROW", false, false),
        ("UPDATE", "CASCADE", "ROW", false, true),
        ("DELETE", "CASCADE", "STATEMENT", false, false),
        ("DELETE", "SET NULL", "STATEMENT", false, false),
        ("UPDATE", "CASCADE", "STATEMENT", true, false),
        ("DELETE", "CASCADE", "STATEMENT", true, true),
    ] {
        let slug = format!(
            "fk_{}_{}_{level}_{staged}_{chain}",
            operation.to_lowercase(),
            action.to_lowercase().replace(' ', "_")
        )
        .to_lowercase();
        let own = OwnDatabase::new(&admin, &slug);
        let connection = own.connection();
        on_server(
            connection,
            &format!(
                "CREATE SCHEMA app AUTHORIZATION {deployer}; CREATE SCHEMA attacker AUTHORIZATION {attacker}; \
             GRANT USAGE ON SCHEMA app TO {attacker}; GRANT USAGE ON SCHEMA attacker TO {deployer}; \
             GRANT CREATE ON SCHEMA public TO {deployer}; \
             CREATE TABLE public.secret(value text); INSERT INTO public.secret VALUES ('test-only-secret'); \
             GRANT SELECT ON public.secret TO {deployer}; \
             CREATE TABLE attacker.leaked(value text); ALTER TABLE attacker.leaked OWNER TO {attacker}; \
             GRANT INSERT ON attacker.leaked TO {deployer}"
            ),
        );
        let login = |role: &str| {
            format!(
                "{} user={role} password=trigger-test",
                connection
                    .split_whitespace()
                    .filter(|w| !w.starts_with("user=") && !w.starts_with("password="))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        };
        let deployment = login(&deployer);
        let attack = login(&attacker);
        assert!(try_on_server(&attack, "SELECT * FROM public.secret").is_err());
        let d = Demo::new(&slug);
        std::fs::write(
            d.dir.join("pbps.yml"),
            format!(
                "dialect: postgres\nunmanaged: {}\n",
                if staged { "warn" } else { "ignore" }
            ),
        )
        .unwrap();
        d.keyed(&deployment);
        // The referenced key is a UNIQUE column and not the primary key: a
        // declared row is identified by its key, so only another column of it
        // can change under an UPDATE at all.
        let declared = "table: app.t\ncolumns:\n  code: {type: text, nullable: false}\n  ukey: {type: text, nullable: false}\n  label: {type: text, nullable: false}\nprimary_key: {name: pk_t, columns: [code]}\nunique:\n  uq_t_ukey: [ukey]\ndata:\n  mode: exact\n  rows:\n    fallback: {ukey: fallback, label: Fallback}\n";
        d.table(&format!(
            "{declared}    first: {{ukey: k1, label: First}}\n"
        ));
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", &deployment]));
        // Unmanaged and owned by the deployer, which is what makes an invoker
        // trigger on it run with the deployer's privileges.
        let (on_update, on_delete) = if operation == "UPDATE" {
            (action, "NO ACTION")
        } else {
            ("NO ACTION", action)
        };
        // A delete case leaves the declared row unreferenced: see the note on
        // this test. Every other row here belongs to the control below.
        let held = if operation == "UPDATE" {
            "k1"
        } else {
            "fallback"
        };
        on_server(
            &deployment,
            &format!(
                "CREATE TABLE app.child(id integer PRIMARY KEY, ukey text DEFAULT 'fallback' UNIQUE, note text, \
             CONSTRAINT fk_child FOREIGN KEY (ukey) REFERENCES app.t(ukey) ON UPDATE {on_update} ON DELETE {on_delete}); \
             CREATE TABLE app.grandchild(id integer PRIMARY KEY, ukey text, \
             CONSTRAINT fk_grandchild FOREIGN KEY (ukey) REFERENCES app.child(ukey) ON UPDATE CASCADE ON DELETE CASCADE); \
             INSERT INTO app.child VALUES (1, '{held}', 'note'); \
             INSERT INTO app.grandchild VALUES (1, '{held}')"
            ),
        );
        let reached = if chain { "app.grandchild" } else { "app.child" };
        let fires = if operation == "DELETE" && action == "CASCADE" {
            "DELETE"
        } else {
            "UPDATE"
        };
        // The control: the same action, on rows of its own, with a trigger the
        // deployer owns. It proves the engine really writes `reached` here, so
        // a later "no leak" cannot pass because nothing fired at all.
        on_server(
            &deployment,
            &format!(
                "CREATE TABLE public.fired(value integer); \
             CREATE FUNCTION public.mark() RETURNS trigger LANGUAGE plpgsql AS \
             $$BEGIN INSERT INTO public.fired VALUES (1); RETURN NULL; END$$; \
             CREATE TRIGGER control AFTER {fires} ON {reached} FOR EACH {level} EXECUTE FUNCTION public.mark(); \
             INSERT INTO app.t VALUES ('control', 'c1', 'Control'); \
             INSERT INTO app.child VALUES (99, 'c1', 'control'); \
             INSERT INTO app.grandchild VALUES (99, 'c1')"
            ),
        );
        on_server(
            &deployment,
            &if operation == "UPDATE" {
                "UPDATE app.t SET ukey = 'c2' WHERE code = 'control'".to_owned()
            } else {
                "DELETE FROM app.t WHERE code = 'control'".to_owned()
            },
        );
        assert!(
            scalar(connection, "SELECT count(*) FROM public.fired") > 0,
            "{slug}: the referential action never reached {reached}"
        );
        on_server(
            &deployment,
            &format!(
                "DROP TRIGGER control ON {reached}; TRUNCATE public.fired; \
             DELETE FROM app.grandchild WHERE id = 99; DELETE FROM app.child WHERE id = 99; \
             DELETE FROM app.t WHERE code = 'control'"
            ),
        );
        let after = if operation == "UPDATE" {
            format!("{declared}    first: {{ukey: k2, label: First}}\n")
        } else {
            declared.to_owned()
        };
        d.table(&after);
        // The function is there before approval and pinned with it
        // (DEC-319.1); only the trigger that calls it arrives afterwards.
        on_server(
            &attack,
            "CREATE FUNCTION attacker.steal() RETURNS trigger LANGUAGE plpgsql SECURITY INVOKER AS \
             $$BEGIN INSERT INTO attacker.leaked SELECT value FROM public.secret; \
             IF TG_OP = 'DELETE' THEN RETURN OLD; ELSE RETURN NEW; END IF; END$$",
        );
        let plan = connected_artifact(&d, &deployment, staged);
        // Installed after approval by a role that cannot read the secret and
        // owns neither the table it writes through nor the one it lands on.
        on_server(
            connection,
            &format!("GRANT TRIGGER ON {reached} TO {attacker}"),
        );
        on_server(
            &attack,
            &format!(
                "CREATE TRIGGER steal BEFORE {fires} ON {reached} FOR EACH {level} EXECUTE FUNCTION attacker.steal()"
            ),
        );
        let blocked = d.run(&["plan", "--db", &deployment]);
        assert_eq!(
            code(&blocked),
            1,
            "{}{}",
            stdout(&blocked),
            stderr(&blocked)
        );
        assert!(
            stderr(&blocked).contains("unsafe data trigger") && stderr(&blocked).contains(reached),
            "{slug}: {}",
            stderr(&blocked)
        );
        let mut extra = Vec::new();
        if staged {
            extra.push("--staged");
        }
        extra.extend([
            "--allow",
            if operation == "UPDATE" {
                "data-update"
            } else {
                "data-delete"
            },
        ]);
        let refused = approved_apply(&d, &deployment, &plan, &extra);
        assert_eq!(
            scalar(connection, "SELECT count(*) FROM attacker.leaked"),
            0,
            "{slug} leaked despite the approval boundary: {}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert_eq!(
            code(&refused),
            1,
            "{}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(
            stderr(&refused).contains("unsafe data trigger") && stderr(&refused).contains(reached),
            "{slug}: {}",
            stderr(&refused)
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM public.__pbps_state WHERE kind = 'apply'"
            ),
            0,
            "{slug}"
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM app.t WHERE code = 'first' AND ukey = 'k1'"
            ),
            1,
            "{slug}: the refused write must leave the named table alone"
        );
        // The same plan with the trigger gone: the row operation and every
        // action it produces are ordinary work the deployer is allowed to do.
        on_server(&deployment, &format!("DROP TRIGGER steal ON {reached}"));
        succeeds(approved_apply(&d, &deployment, &plan, &extra));
        assert_eq!(
            scalar(connection, "SELECT count(*) FROM attacker.leaked"),
            0,
            "{slug}"
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM app.t WHERE code = 'first'"
            ),
            i64::from(operation == "UPDATE"),
            "{slug}: the row operation itself went through"
        );
        if operation == "UPDATE" {
            let landed = match action {
                "CASCADE" => "SELECT count(*) FROM app.child WHERE ukey = 'k2'",
                "SET NULL" => "SELECT count(*) FROM app.child WHERE ukey IS NULL",
                _ => "SELECT count(*) FROM app.child WHERE ukey = 'fallback'",
            };
            assert_eq!(scalar(connection, landed), 1, "{slug}: the action ran");
        }
        succeeds(d.run(&["verify", "--db", &deployment]));
    }
}

/// The closure is the set of tables a row operation makes PostgreSQL write, and
/// no more: an action that writes nothing, an event the action does not raise
/// and a column it does not set all leave their triggers alone. A guard that
/// refused those would refuse valid plans, which is the other way to be wrong.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn the_write_closure_follows_writing_actions_and_stops_at_the_others() {
    use pbps_dialect::{Dialect, RowOperation, RowWrite};
    let own = OwnDatabase::new(&server(), "fk_closure");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE FUNCTION public.hook() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NULL; END$$",
    );
    let update = |columns: &[&str]| RowOperation::Update {
        columns: columns.iter().map(|c| (*c).to_owned()).collect(),
    };
    // Every case declares `{s}.p`, writes it, and puts one trigger somewhere a
    // referential action may or may not reach. `p` always keeps the same shape
    // so the write is the only thing that changes.
    let head = "CREATE TABLE {s}.p(code text PRIMARY KEY, ukey text UNIQUE, other text UNIQUE, label text, \
                UNIQUE (ukey, other)); ";
    let cases: Vec<(&str, String, RowOperation, bool)> = vec![
        (
            "no_action",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE NO ACTION); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            false,
        ),
        (
            "restrict",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE RESTRICT); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            false,
        ),
        (
            "cascade",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            true,
        ),
        (
            "set_default",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE SET DEFAULT); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            true,
        ),
        (
            "delete_action_under_an_update",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE NO ACTION ON DELETE CASCADE); \
                    CREATE TRIGGER hook AFTER DELETE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            false,
        ),
        (
            "update_action_under_a_delete",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE ON DELETE NO ACTION); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            RowOperation::Delete,
            false,
        ),
        (
            "insert_raises_no_action",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE ON DELETE CASCADE); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            RowOperation::Insert,
            false,
        ),
        (
            "untouched_referenced_column",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["label"]),
            false,
        ),
        (
            "another_key_of_the_same_table",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, other text REFERENCES {{s}}.p(other) ON UPDATE CASCADE); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            false,
        ),
        (
            // The delete side reaches row-level triggers too. The row-delete
            // preflight and the guard inside the emitted statement refuse a
            // plan whose declared row still has children, so the action's
            // statement usually matches nothing -- but measured on 18.6, a
            // concurrent child inserted while that guarded DELETE waits on the
            // parent row lock is cascaded anyway, and its row trigger, its
            // grandchild's row trigger and both statement triggers all fire
            // (DECISIONS 451).
            "a_row_trigger_on_a_delete_actions_child",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON DELETE CASCADE); \
                    CREATE TRIGGER hook AFTER DELETE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            RowOperation::Delete,
            true,
        ),
        (
            "a_grandchild_a_delete_action_cascades_into",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON DELETE CASCADE); \
                    CREATE TABLE {{s}}.g(id int PRIMARY KEY, cid int REFERENCES {{s}}.c(id) ON DELETE CASCADE); \
                    CREATE TRIGGER hook AFTER DELETE ON {{s}}.g FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            RowOperation::Delete,
            true,
        ),
        (
            "an_event_the_action_does_not_raise",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE); \
                    CREATE TRIGGER hook AFTER DELETE ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            false,
        ),
        (
            "a_child_column_the_action_does_not_set",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE, note text); \
                    CREATE TRIGGER hook AFTER UPDATE OF note ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            false,
        ),
        (
            "the_child_column_the_action_sets",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE, note text); \
                    CREATE TRIGGER hook AFTER UPDATE OF ukey ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            true,
        ),
        (
            "outside_an_on_delete_set_null_column_list",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text, other text, note text, \
                    FOREIGN KEY (ukey, other) REFERENCES {{s}}.p(ukey, other) ON DELETE SET NULL (other)); \
                    CREATE TRIGGER hook AFTER UPDATE OF ukey ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            RowOperation::Delete,
            false,
        ),
        (
            "inside_an_on_delete_set_null_column_list",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text, other text, note text, \
                    FOREIGN KEY (ukey, other) REFERENCES {{s}}.p(ukey, other) ON DELETE SET NULL (other)); \
                    CREATE TRIGGER hook AFTER UPDATE OF other ON {{s}}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            RowOperation::Delete,
            true,
        ),
        (
            "an_inheritance_descendant_of_the_child",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE); \
                    CREATE TABLE {{s}}.sub () INHERITS ({{s}}.c); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.sub FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            false,
        ),
        (
            "a_partition_of_the_child",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int, ukey text) PARTITION BY RANGE (id); \
                    CREATE TABLE {{s}}.part PARTITION OF {{s}}.c FOR VALUES FROM (0) TO (10); \
                    ALTER TABLE {{s}}.c ADD FOREIGN KEY (ukey) REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE; \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.part FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            true,
        ),
        (
            "a_statement_trigger_on_a_partition_of_the_child",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int, ukey text) PARTITION BY RANGE (id); \
                    CREATE TABLE {{s}}.part PARTITION OF {{s}}.c FOR VALUES FROM (0) TO (10); \
                    ALTER TABLE {{s}}.c ADD FOREIGN KEY (ukey) REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE; \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.part FOR EACH STATEMENT EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            false,
        ),
        (
            "a_statement_trigger_on_the_child_itself",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int, ukey text) PARTITION BY RANGE (id); \
                    CREATE TABLE {{s}}.part PARTITION OF {{s}}.c FOR VALUES FROM (0) TO (10); \
                    ALTER TABLE {{s}}.c ADD FOREIGN KEY (ukey) REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE; \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.c FOR EACH STATEMENT EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            true,
        ),
        (
            "two_foreign_keys_away",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text UNIQUE REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE); \
                    CREATE TABLE {{s}}.g(id int PRIMARY KEY, ukey text REFERENCES {{s}}.c(ukey) ON UPDATE CASCADE); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.g FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            true,
        ),
        (
            "a_cycle_the_walk_has_to_leave",
            format!(
                "{head}CREATE TABLE {{s}}.c(id int PRIMARY KEY, ukey text UNIQUE REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE, \
                    loop_ukey text REFERENCES {{s}}.c(ukey) ON UPDATE CASCADE); \
                    CREATE TABLE {{s}}.elsewhere(id int PRIMARY KEY); \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.elsewhere FOR EACH ROW EXECUTE FUNCTION public.hook()"
            ),
            update(&["ukey"]),
            false,
        ),
        (
            "a_partition_the_action_moves_a_row_out_of",
            format!("{head}CREATE TABLE {{s}}.c(id int, ukey text) PARTITION BY LIST (ukey); \
                    CREATE TABLE {{s}}.here PARTITION OF {{s}}.c FOR VALUES IN ('k1'); \
                    CREATE TABLE {{s}}.there PARTITION OF {{s}}.c FOR VALUES IN ('k2'); \
                    ALTER TABLE {{s}}.c ADD FOREIGN KEY (ukey) REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE; \
                    CREATE TRIGGER hook AFTER DELETE ON {{s}}.here FOR EACH ROW EXECUTE FUNCTION public.hook()"),
            update(&["ukey"]),
            true,
        ),
        (
            "a_partition_the_action_moves_a_row_into",
            format!("{head}CREATE TABLE {{s}}.c(id int, ukey text) PARTITION BY LIST (ukey); \
                    CREATE TABLE {{s}}.here PARTITION OF {{s}}.c FOR VALUES IN ('k1'); \
                    CREATE TABLE {{s}}.there PARTITION OF {{s}}.c FOR VALUES IN ('k2'); \
                    ALTER TABLE {{s}}.c ADD FOREIGN KEY (ukey) REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE; \
                    CREATE TRIGGER hook BEFORE INSERT ON {{s}}.there FOR EACH ROW EXECUTE FUNCTION public.hook()"),
            update(&["ukey"]),
            true,
        ),
        (
            // A movable statement is not a statement whose every row moves:
            // measured on 18.6, one that leaves a row where it is fires that
            // partition's BEFORE *and* AFTER UPDATE row triggers, while the
            // row that moves fires BEFORE UPDATE and the DELETE/INSERT halves.
            // So the movement events are added to the UPDATE triggers, never
            // swapped for them (DECISIONS 451).
            "an_after_update_row_trigger_on_a_partition_a_row_can_leave",
            format!("{head}CREATE TABLE {{s}}.c(id int, ukey text) PARTITION BY LIST (ukey); \
                    CREATE TABLE {{s}}.here PARTITION OF {{s}}.c FOR VALUES IN ('k1'); \
                    CREATE TABLE {{s}}.there PARTITION OF {{s}}.c FOR VALUES IN ('k2'); \
                    ALTER TABLE {{s}}.c ADD FOREIGN KEY (ukey) REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE; \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.here FOR EACH ROW EXECUTE FUNCTION public.hook()"),
            update(&["ukey"]),
            true,
        ),
        (
            "a_statement_level_delete_trigger_no_movement_fires",
            format!("{head}CREATE TABLE {{s}}.c(id int, ukey text) PARTITION BY LIST (ukey); \
                    CREATE TABLE {{s}}.here PARTITION OF {{s}}.c FOR VALUES IN ('k1'); \
                    ALTER TABLE {{s}}.c ADD FOREIGN KEY (ukey) REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE; \
                    CREATE TRIGGER hook AFTER DELETE ON {{s}}.c FOR EACH STATEMENT EXECUTE FUNCTION public.hook(); \
                    CREATE TRIGGER also AFTER DELETE ON {{s}}.here FOR EACH STATEMENT EXECUTE FUNCTION public.hook()"),
            update(&["ukey"]),
            false,
        ),
        (
            "a_partition_key_the_action_does_not_write",
            format!("{head}CREATE TABLE {{s}}.c(id int, ukey text, note text) PARTITION BY LIST (note); \
                    CREATE TABLE {{s}}.here PARTITION OF {{s}}.c FOR VALUES IN ('a'); \
                    ALTER TABLE {{s}}.c ADD FOREIGN KEY (ukey) REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE; \
                    CREATE TRIGGER hook AFTER DELETE ON {{s}}.here FOR EACH ROW EXECUTE FUNCTION public.hook()"),
            update(&["ukey"]),
            false,
        ),
        (
            "the_named_table_moves_its_own_row",
            "CREATE TABLE {s}.p(code text, ukey text, other text, label text) PARTITION BY LIST (ukey); \
             CREATE TABLE {s}.here PARTITION OF {s}.p FOR VALUES IN ('k1'); \
             CREATE TRIGGER hook AFTER DELETE ON {s}.here FOR EACH ROW EXECUTE FUNCTION public.hook()"
                .to_owned(),
            update(&["ukey"]),
            true,
        ),
        (
            "the_named_table_stays_where_it_is",
            "CREATE TABLE {s}.p(code text, ukey text, other text, label text) PARTITION BY LIST (label); \
             CREATE TABLE {s}.here PARTITION OF {s}.p FOR VALUES IN ('a'); \
             CREATE TRIGGER hook AFTER DELETE ON {s}.here FOR EACH ROW EXECUTE FUNCTION public.hook()"
                .to_owned(),
            update(&["ukey"]),
            false,
        ),
        (
            "a_foreign_key_declared_on_one_partition",
            format!("{head}CREATE TABLE {{s}}.c(id int, ukey text) PARTITION BY RANGE (id); \
                    CREATE TABLE {{s}}.here PARTITION OF {{s}}.c FOR VALUES FROM (0) TO (10); \
                    CREATE TABLE {{s}}.there PARTITION OF {{s}}.c FOR VALUES FROM (10) TO (20); \
                    ALTER TABLE {{s}}.here ADD FOREIGN KEY (ukey) REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE; \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.here FOR EACH ROW EXECUTE FUNCTION public.hook()"),
            update(&["ukey"]),
            true,
        ),
        (
            "a_sibling_of_the_partition_the_key_was_declared_on",
            format!("{head}CREATE TABLE {{s}}.c(id int, ukey text) PARTITION BY RANGE (id); \
                    CREATE TABLE {{s}}.here PARTITION OF {{s}}.c FOR VALUES FROM (0) TO (10); \
                    CREATE TABLE {{s}}.there PARTITION OF {{s}}.c FOR VALUES FROM (10) TO (20); \
                    ALTER TABLE {{s}}.here ADD FOREIGN KEY (ukey) REFERENCES {{s}}.p(ukey) ON UPDATE CASCADE; \
                    CREATE TRIGGER hook AFTER UPDATE ON {{s}}.there FOR EACH ROW EXECUTE FUNCTION public.hook(); \
                    CREATE TRIGGER also AFTER UPDATE ON {{s}}.c FOR EACH STATEMENT EXECUTE FUNCTION public.hook()"),
            update(&["ukey"]),
            false,
        ),
        (
            "a_generated_referenced_column_of_a_set_column",
            "CREATE TABLE {s}.p(code text PRIMARY KEY, label text, ukey text GENERATED ALWAYS AS (upper(label)) STORED UNIQUE); \
                    CREATE TABLE {s}.c(id int PRIMARY KEY, ukey text REFERENCES {s}.p(ukey) ON UPDATE CASCADE); \
                    CREATE TRIGGER hook AFTER UPDATE ON {s}.c FOR EACH ROW EXECUTE FUNCTION public.hook()"
                .to_owned(),
            update(&["label"]),
            true,
        ),
    ];
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
            .await
            .unwrap();
        let dialect = pbps_pg::Postgres::new();
        for (label, ddl, operation, refused) in cases {
            conn.execute(&format!("CREATE SCHEMA {label}"))
                .await
                .unwrap();
            conn.execute(&ddl.replace("{s}", label))
                .await
                .unwrap_or_else(|e| panic!("{label} setup: {e}"));
            let write = RowWrite {
                table: pbps_model::TableName::new(label, "p"),
                operation,
            };
            conn.begin(dialect.transaction_framing()).await.unwrap();
            let checked = pbps_pg::data_triggers::prepare(
                &mut conn,
                &[write],
                &Default::default(),
                &Default::default(),
            )
            .await;
            assert_eq!(
                checked.is_err(),
                refused,
                "{label}: {:?}",
                checked.err().map(|e| e.to_string())
            );
            conn.rollback(dialect.transaction_framing()).await.unwrap();
        }
    });
}

/// The lock the guard takes on a table it reaches through an action is the same
/// guarantee it takes on the named one: authentication that a concurrent
/// CREATE OR REPLACE TRIGGER could undo before the write is no guarantee.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_cascade_reached_table_is_held_until_the_write_finishes() {
    use pbps_dialect::{Dialect, RowOperation, RowWrite};
    let own = OwnDatabase::new(&server(), "fk_lock");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; \
        CREATE TABLE app.p(code text PRIMARY KEY, ukey text UNIQUE); \
        CREATE TABLE app.c(id integer PRIMARY KEY, ukey text REFERENCES app.p(ukey) ON UPDATE CASCADE); \
        CREATE FUNCTION app.trusted() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$; \
        CREATE FUNCTION app.replacement() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$; \
        CREATE TRIGGER guarded BEFORE UPDATE ON app.c FOR EACH ROW EXECUTE FUNCTION app.trusted(); \
        INSERT INTO app.p VALUES ('first', 'k1'); INSERT INTO app.c VALUES (1, 'k1')",
    );
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let mut writer = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection).await.unwrap();
        let mut other = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection).await.unwrap();
        let baseline = pbps_pg::catalog::introspect(&mut writer).await.unwrap().schema;
        let dialect = pbps_pg::Postgres::new();
        let write = RowWrite {
            table: pbps_model::TableName::new("app", "p"),
            operation: RowOperation::Update { columns: ["ukey".to_owned()].into() },
        };
        writer.begin(dialect.transaction_framing()).await.unwrap();
        let guard = pbps_pg::data_triggers::prepare(&mut writer, std::slice::from_ref(&write), &baseline, &Default::default()).await.unwrap();
        other.execute("SET lock_timeout = '100ms'").await.unwrap();
        // The named table is not the one under contention here: `app.c` is
        // only ever written by the cascade the guard followed.
        let replace = "CREATE OR REPLACE TRIGGER guarded BEFORE UPDATE ON app.c FOR EACH ROW EXECUTE FUNCTION app.replacement()";
        let blocked = other.execute(replace).await.unwrap_err();
        assert!(matches!(blocked, pbps_db::DbError::Driver { ref code, .. } if code.as_deref() == Some("55P03")), "{blocked}");
        pbps_pg::data_triggers::check(&mut writer, &write, &guard).await.unwrap();
        writer.execute("UPDATE app.p SET ukey = 'k2' WHERE code = 'first'").await.unwrap();
        writer.commit(dialect.transaction_framing()).await.unwrap();
        // After release the same replacement succeeds, proving the first
        // failure came from the guard's lock, not invalid DDL or permissions.
        let baseline = pbps_pg::catalog::introspect(&mut writer).await.unwrap().schema;
        other.execute(replace).await.unwrap();
        writer.begin(dialect.transaction_framing()).await.unwrap();
        let refused = pbps_pg::data_triggers::prepare(&mut writer, &[write], &baseline, &Default::default()).await;
        assert!(refused.is_err(), "a replaced trigger on the cascade's table must refuse the next write");
        writer.rollback(dialect.transaction_framing()).await.unwrap();
    });
}

/// Pin the engine race measured in DECISIONS 451: a NOT EXISTS predicate sees
/// no child while the foreign-key action can see a concurrently committed one.
/// This is the measured statement, not the emitter's separate parent-lock and
/// referencing-count block; the closure-membership cases above pin the policy.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_child_committed_while_a_guarded_delete_waits_fires_cascade_row_triggers() {
    use pbps_dialect::Dialect;
    use std::time::Duration;
    let own = OwnDatabase::new(&server(), "fk_delete_race");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; \
        CREATE TABLE app.p(k text PRIMARY KEY); \
        CREATE TABLE app.c(k text PRIMARY KEY REFERENCES app.p(k) ON DELETE CASCADE); \
        CREATE TABLE app.g(k text PRIMARY KEY REFERENCES app.c(k) ON DELETE CASCADE); \
        CREATE TABLE app.fired(relation text, level text); \
        CREATE FUNCTION app.log_delete() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN \
            INSERT INTO app.fired VALUES (TG_TABLE_NAME, TG_LEVEL); RETURN OLD; END$$; \
        CREATE TRIGGER row_hook AFTER DELETE ON app.c FOR EACH ROW EXECUTE FUNCTION app.log_delete(); \
        CREATE TRIGGER statement_hook AFTER DELETE ON app.c FOR EACH STATEMENT EXECUTE FUNCTION app.log_delete(); \
        CREATE TRIGGER row_hook AFTER DELETE ON app.g FOR EACH ROW EXECUTE FUNCTION app.log_delete(); \
        CREATE TRIGGER statement_hook AFTER DELETE ON app.g FOR EACH STATEMENT EXECUTE FUNCTION app.log_delete(); \
        INSERT INTO app.p VALUES ('k1')",
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut writer = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
                .await
                .unwrap();
            let mut other = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
                .await
                .unwrap();
            let dialect = pbps_pg::Postgres::new();
            let pid = writer
                .query("SELECT pg_backend_pid()::int8 AS pid")
                .await
                .unwrap()[0]
                .try_get::<i64>("pid")
                .unwrap()
                .unwrap();
            writer
                .execute("SET statement_timeout = '10s'")
                .await
                .unwrap();
            other.begin(dialect.transaction_framing()).await.unwrap();
            other
                .execute("INSERT INTO app.c VALUES ('k1'); INSERT INTO app.g VALUES ('k1')")
                .await
                .unwrap();
            let delete = "DELETE FROM app.p AS p WHERE k = 'k1' \
                AND NOT EXISTS (SELECT 1 FROM app.c AS c WHERE c.k = p.k)";
            let deleting = tokio::spawn(async move {
                let deleted = writer.execute(delete).await;
                (writer, deleted)
            });
            let hand_off = async {
                // pg_blocking_pids reads current lock state, unlike a sleep or
                // a transaction-cached activity snapshot. Do not commit until
                // the delete has taken its snapshot and is waiting on us.
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let rows = other.query(&format!(
                            "SELECT pg_backend_pid() = ANY(pg_blocking_pids({pid}::int)) AS blocked"
                        )).await.unwrap();
                        if rows[0].try_get::<bool>("blocked").unwrap() == Some(true) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("guarded delete never waited on the inserting session");
                other.commit(dialect.transaction_framing()).await.unwrap();
            };
            hand_off.await;
            let (mut writer, deleted) = deleting.await.unwrap();
            deleted.unwrap();
            let rows = writer
                .query(
                    "SELECT (SELECT count(*) FROM app.p) + \
                (SELECT count(*) FROM app.c) + (SELECT count(*) FROM app.g) AS n",
                )
                .await
                .unwrap();
            assert_eq!(
                rows[0].try_get::<i64>("n").unwrap(),
                Some(0),
                "the admitted delete must cascade through both committed rows"
            );
            let rows = writer
                .query("SELECT relation, level FROM app.fired ORDER BY relation, level")
                .await
                .unwrap();
            let fired: Vec<_> = rows
                .iter()
                .map(|row| {
                    (
                        row.try_get::<&str>("relation").unwrap().unwrap(),
                        row.try_get::<&str>("level").unwrap().unwrap(),
                    )
                })
                .collect();
            assert_eq!(
                fired,
                [
                    ("c", "ROW"),
                    ("c", "STATEMENT"),
                    ("g", "ROW"),
                    ("g", "STATEMENT")
                ]
            );

            // With the same rows committed before the statement snapshot,
            // NOT EXISTS refuses the parent and neither cascade fires.
            writer
                .execute(
                    "TRUNCATE app.fired; INSERT INTO app.p VALUES ('k1'); \
                INSERT INTO app.c VALUES ('k1'); INSERT INTO app.g VALUES ('k1')",
                )
                .await
                .unwrap();
            writer.execute(delete).await.unwrap();
            let rows = writer
                .query(
                    "SELECT (SELECT count(*) FROM app.p) + \
                (SELECT count(*) FROM app.c) + (SELECT count(*) FROM app.g) AS n, \
                (SELECT count(*) FROM app.fired) AS fired",
                )
                .await
                .unwrap();
            assert_eq!(rows[0].try_get::<i64>("n").unwrap(), Some(3));
            assert_eq!(rows[0].try_get::<i64>("fired").unwrap(), Some(0));
        });
}

/// The guard holds every table it authenticates, and a table it cannot hold is
/// a refusal with a name in it rather than a bare `permission denied`.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_reached_table_the_deployment_role_cannot_lock_is_named_in_the_refusal() {
    use pbps_dialect::{Dialect, RowOperation, RowWrite};
    struct Roles(String, String);
    impl Drop for Roles {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let admin = server();
    let own = OwnDatabase::new(&admin, "fk_lock_denied");
    let connection = own.connection();
    let deployer = format!("pbps_fk_locker_{}", std::process::id());
    let _roles = Roles(admin.clone(), deployer.clone());
    on_server(
        &admin,
        &format!("CREATE ROLE {deployer} LOGIN NOSUPERUSER PASSWORD 'trigger-test'"),
    );
    // The child belongs to someone else, and the deployment role may only read
    // it. PostgreSQL runs the action as the child's owner and needs nothing
    // from the deployer, so only the guard's own lock is missing.
    on_server(
        connection,
        &format!(
            "CREATE SCHEMA app AUTHORIZATION {deployer}; GRANT CREATE ON SCHEMA public TO {deployer}; \
            CREATE TABLE public.c(id integer PRIMARY KEY, ukey text); \
            GRANT SELECT ON public.c TO {deployer}"
        ),
    );
    let deployment = format!(
        "{} user={deployer} password=trigger-test",
        connection
            .split_whitespace()
            .filter(|w| !w.starts_with("user=") && !w.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" ")
    );
    on_server(
        &deployment,
        "CREATE TABLE app.p(code text PRIMARY KEY, ukey text UNIQUE)",
    );
    on_server(
        connection,
        "ALTER TABLE public.c ADD FOREIGN KEY (ukey) REFERENCES app.p(ukey) ON UPDATE CASCADE",
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &deployment)
                .await
                .unwrap();
            let dialect = pbps_pg::Postgres::new();
            let write = RowWrite {
                table: pbps_model::TableName::new("app", "p"),
                operation: RowOperation::Update {
                    columns: ["ukey".to_owned()].into(),
                },
            };
            conn.begin(dialect.transaction_framing()).await.unwrap();
            let refused = pbps_pg::data_triggers::prepare(
                &mut conn,
                &[write],
                &Default::default(),
                &Default::default(),
            )
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
            assert!(
                refused.contains("cannot lock") && refused.contains("public.c"),
                "{refused}"
            );
            conn.rollback(dialect.transaction_framing()).await.unwrap();
        });
}

/// An action uses ONLY on a regular table but still reaches every partition.
/// Keep the lock privilege bar at the named relation and retain the engine's
/// recursive partition lock, including the FK's concurrent-attach protection.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn referential_action_locks_exclude_plain_children_and_hold_partitions() {
    use pbps_dialect::{Dialect, RowOperation, RowWrite};
    let admin = server();
    let deployer = format!("pbps_action_locks_{}", std::process::id());
    let _roles = ClusterRoles {
        server: admin.clone(),
        names: vec![deployer.clone()],
    };
    on_server(
        &admin,
        &format!("CREATE ROLE {deployer} LOGIN NOSUPERUSER PASSWORD 'trigger-test'"),
    );
    let own = OwnDatabase::new(&admin, "action_lock_scope");
    let connection = own.connection();
    on_server(
        connection,
        &format!(
            "CREATE SCHEMA app; CREATE TABLE app.p(id integer PRIMARY KEY); \
             CREATE TABLE app.c(id integer REFERENCES app.p ON UPDATE CASCADE); \
             CREATE TABLE app.unreached() INHERITS (app.c); \
             CREATE TABLE app.pc(id integer REFERENCES app.p ON UPDATE CASCADE, region integer) PARTITION BY RANGE(region); \
             CREATE TABLE app.pc_first PARTITION OF app.pc FOR VALUES FROM (0) TO (10); \
             CREATE TABLE app.pc_next(id integer, region integer CHECK (region >= 10 AND region < 20)); \
             INSERT INTO app.p VALUES (1); INSERT INTO app.c VALUES (1); \
             INSERT INTO app.unreached VALUES (1); INSERT INTO app.pc VALUES (1, 0); \
             GRANT USAGE ON SCHEMA app TO {deployer}; \
             GRANT UPDATE, SELECT ON app.p TO {deployer}; \
             GRANT INSERT ON app.c, app.pc TO {deployer}"
        ),
    );
    // A partition tree cannot contain the plain inheritance edge we exclude.
    // Pin the engine rule instead of relying on a mixed, impossible fixture.
    for sql in [
        "CREATE TABLE app.mixed() INHERITS (app.pc)",
        "CREATE TABLE app.mixed() INHERITS (app.pc_first)",
        "CREATE TABLE app.mixed() INHERITS (app.c) PARTITION BY RANGE(id)",
    ] {
        assert!(try_on_server(connection, sql).is_err(), "{sql}");
    }
    let deployment = as_role(connection, &deployer);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut writer = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &deployment)
                .await
                .unwrap();
            let mut other = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
                .await
                .unwrap();
            let framing = pbps_pg::Postgres::new().transaction_framing();
            writer.execute("SET lock_timeout = '250ms'").await.unwrap();
            other.execute("SET lock_timeout = '250ms'").await.unwrap();
            other.begin(framing).await.unwrap();
            other
                .execute("LOCK TABLE ONLY app.unreached IN SHARE MODE")
                .await
                .unwrap();
            writer.begin(framing).await.unwrap();
            let write = RowWrite {
                table: pbps_model::TableName::new("app", "p"),
                operation: RowOperation::Update {
                    columns: ["id".to_owned()].into(),
                },
            };
            let guard = pbps_pg::data_triggers::prepare(
                &mut writer,
                std::slice::from_ref(&write),
                &Default::default(),
                &Default::default(),
            )
            .await
            .expect(
                "a SHARE lock on an unreached inheritance child must not delay the action guard",
            );
            pbps_pg::data_triggers::check(&mut writer, &write, &guard)
                .await
                .unwrap();
            let locks = writer
                .query(
                    "SELECT c.relname AS name FROM pg_catalog.pg_locks l \
                 JOIN pg_catalog.pg_class c ON c.oid = l.relation \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE l.pid = pg_catalog.pg_backend_pid() AND n.nspname = 'app' \
                   AND l.mode = 'RowExclusiveLock' AND l.granted ORDER BY c.relname",
                )
                .await
                .unwrap();
            let names: Vec<_> = locks
                .iter()
                .map(|row| row.try_get::<&str>("name").unwrap().unwrap())
                .collect();
            assert_eq!(names, ["c", "p", "pc", "pc_first"]);
            writer
                .execute("UPDATE app.p SET id = 2 WHERE id = 1")
                .await
                .unwrap();
            writer.commit(framing).await.unwrap();
            // Only the action's actual targets moved. The concurrently locked
            // inheritance child is unaffected by both guard and engine DML.
            let rows = other
                .query(
                    "SELECT (SELECT id FROM ONLY app.c) AS ordinary, \
                 (SELECT id FROM app.pc) AS partitioned, \
                 (SELECT id FROM app.unreached) AS unreached",
                )
                .await
                .unwrap();
            assert_eq!(rows[0].try_get::<i32>("ordinary").unwrap(), Some(2));
            assert_eq!(rows[0].try_get::<i32>("partitioned").unwrap(), Some(2));
            assert_eq!(rows[0].try_get::<i32>("unreached").unwrap(), Some(1));

            // Direct DML has no ONLY, so its own guard must still recurse and
            // block on that same child. The first refusal is lock contention,
            // not missing privileges on the child (the deployer has none).
            writer.begin(framing).await.unwrap();
            let direct = RowWrite {
                table: pbps_model::TableName::new("app", "c"),
                operation: RowOperation::Update {
                    columns: ["id".to_owned()].into(),
                },
            };
            let refused = pbps_pg::data_triggers::prepare(
                &mut writer,
                &[direct],
                &Default::default(),
                &Default::default(),
            )
            .await
            .err()
            .expect("direct DML reaches the locked inheritance child");
            assert_eq!(refused.server_error_code().as_deref(), Some("55P03"));
            writer.rollback(framing).await.unwrap();
            other.rollback(framing).await.unwrap();

            // Keep the existing protection against a new partition joining
            // this FK's referencing side while the guard holds its locks.
            // Pair the refusal with success after release on both PGs.
            writer.begin(framing).await.unwrap();
            pbps_pg::data_triggers::prepare(
                &mut writer,
                &[write],
                &Default::default(),
                &Default::default(),
            )
            .await
            .unwrap();
            let attach =
                "ALTER TABLE app.pc ATTACH PARTITION app.pc_next FOR VALUES FROM (10) TO (20)";
            let refused = other
                .execute(attach)
                .await
                .expect_err("ATTACH must wait for the guarded FK write");
            assert_eq!(refused.server_error_code().as_deref(), Some("55P03"));
            writer.rollback(framing).await.unwrap();
            other
                .execute(attach)
                .await
                .expect("ATTACH succeeds after the guard releases its locks");
        });
}

/// A guard failure must retain its table and remedy after driver redaction,
/// on both the ordinary and staged failed-attempt recording paths.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_cascade_lock_denial_keeps_its_remedy_on_the_failed_ledger() {
    let admin = server();
    let deployer = format!("pbps_context488_{}", std::process::id());
    let _roles = ClusterRoles {
        server: admin.clone(),
        names: vec![deployer.clone()],
    };
    on_server(
        &admin,
        &format!("CREATE ROLE {deployer} LOGIN NOSUPERUSER PASSWORD 'trigger-test'"),
    );
    for staged in [false, true] {
        let own = OwnDatabase::new(&admin, &format!("lock_context_{staged}"));
        let connection = own.connection();
        let deployment = as_role(connection, &deployer);
        on_server(
            connection,
            &format!(
                "CREATE SCHEMA app AUTHORIZATION {deployer}; GRANT CREATE ON SCHEMA public TO {deployer}"
            ),
        );
        let d = Demo::new("lock-context");
        std::fs::write(
            d.dir.join("pbps.yml"),
            "dialect: postgres\nunmanaged: ignore\n",
        )
        .unwrap();
        let declared = "table: app.p\ncolumns:\n  code: {type: text, nullable: false}\n  ukey: {type: text, nullable: false}\nprimary_key: {name: pk_p, columns: [code]}\nunique:\n  uq_p_ukey: [ukey]\ndata:\n  mode: exact\n  rows:\n    first: {ukey: old}\n";
        d.table(declared);
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", &deployment]));
        on_server(
            connection,
            &format!(
                "CREATE TABLE public.c(id integer PRIMARY KEY, ukey text REFERENCES app.p(ukey) ON UPDATE CASCADE); \
             GRANT SELECT, TRIGGER, UPDATE ON public.c TO {deployer}"
            ),
        );
        d.table(&declared.replace("ukey: old", "ukey: new"));
        let plan = connected_artifact(&d, &deployment, staged);
        // Plan with the lock right, then remove it so the failure is apply's
        // own guard and reaches the durable failed-attempt recording path.
        on_server(
            connection,
            &format!("REVOKE UPDATE ON public.c FROM {deployer}"),
        );
        assert_eq!(
            scalar(
                &deployment,
                "SELECT (has_table_privilege('public.c', 'TRIGGER') AND NOT \
             has_table_privilege('public.c', 'INSERT,UPDATE,DELETE,TRUNCATE'))::int::int8"
            ),
            1
        );
        let extra = if staged {
            vec!["--staged", "--allow", "data-update"]
        } else {
            vec!["--allow", "data-update"]
        };
        let failed = approved_apply(&d, &deployment, &plan, &extra);
        assert_eq!(code(&failed), 1, "{}{}", stdout(&failed), stderr(&failed));
        assert!(
            stderr(&failed).contains("permission denied for table c"),
            "{}",
            stderr(&failed)
        );
        let after = latest_snapshot(connection);
        assert_eq!(after.kind, pbps_model::StateKind::Failed);
        let reason = after.reason.unwrap_or_default();
        for text in [
            "42501",
            "cannot lock `public.c`",
            "INSERT, UPDATE, DELETE or TRUNCATE",
        ] {
            assert!(reason.contains(text), "missing {text}: {reason}");
        }
        assert!(
            !reason.contains("permission denied for table c"),
            "{reason}"
        );
        assert_eq!(
            scalar(connection, "SELECT count(*) FROM app.p WHERE ukey = 'old'"),
            1
        );
        // Restoring a listed privilege lets the very same plan complete.
        on_server(
            connection,
            &format!("GRANT UPDATE ON public.c TO {deployer}"),
        );
        succeeds(approved_apply(&d, &deployment, &plan, &extra));
        assert_eq!(
            scalar(connection, "SELECT count(*) FROM app.p WHERE ukey = 'new'"),
            1
        );
        succeeds(d.run(&["verify", "--db", &deployment]));
    }
}

/// A referential action that the plan removes, or that this session cannot
/// fire, writes nothing — and a closure that followed it anyway would refuse a
/// plan for a table the write never reaches.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn an_action_that_cannot_write_is_not_in_the_closure() {
    use pbps_dialect::{Dialect, RowOperation, RowWrite};
    use pbps_pg::data_triggers::Dropped;
    let own = OwnDatabase::new(&server(), "fk_not_firing");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; \
        CREATE FUNCTION public.hook() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NULL; END$$; \
        CREATE TABLE app.p(code text PRIMARY KEY, ukey text UNIQUE); \
        CREATE TABLE app.c(id integer PRIMARY KEY, ukey text, \
            CONSTRAINT fk_c FOREIGN KEY (ukey) REFERENCES app.p(ukey) ON UPDATE CASCADE); \
        CREATE TRIGGER hook AFTER UPDATE ON app.c FOR EACH ROW EXECUTE FUNCTION public.hook()",
    );
    let child: pbps_model::TableName = pbps_model::TableName::new("app", "c");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
                .await
                .unwrap();
            let dialect = pbps_pg::Postgres::new();
            let write = RowWrite {
                table: pbps_model::TableName::new("app", "p"),
                operation: RowOperation::Update {
                    columns: ["ukey".to_owned()].into(),
                },
            };
            let key = Dropped {
                foreign_keys: [(child.clone(), "fk_c".to_owned())].into(),
                ..Default::default()
            };
            let table = Dropped {
                tables: [child.clone()].into(),
                ..Default::default()
            };
            for (label, dropped, refused) in [
                ("nothing removed", Dropped::default(), true),
                ("the key the plan drops first", key, false),
                ("the table the plan drops first", table, false),
            ] {
                conn.begin(dialect.transaction_framing()).await.unwrap();
                let checked = pbps_pg::data_triggers::prepare(
                    &mut conn,
                    std::slice::from_ref(&write),
                    &Default::default(),
                    &dropped,
                )
                .await;
                assert_eq!(
                    checked.is_err(),
                    refused,
                    "{label}: {:?}",
                    checked.err().map(|e| e.to_string())
                );
                conn.rollback(dialect.transaction_framing()).await.unwrap();
            }
            // The allowance is the plan's promise, and `check` is where the
            // promise is kept: a key still in the catalog at the write is
            // followed whatever the plan said about it.
            conn.begin(dialect.transaction_framing()).await.unwrap();
            let guard = pbps_pg::data_triggers::prepare(
                &mut conn,
                std::slice::from_ref(&write),
                &Default::default(),
                &Dropped {
                    tables: [child.clone()].into(),
                    ..Default::default()
                },
            )
            .await
            .expect("the dropped table is not followed");
            let refused = pbps_pg::data_triggers::check(&mut conn, &write, &guard).await;
            assert!(
                refused.is_err(),
                "a foreign key the plan promised to remove must actually be gone by the write"
            );
            conn.rollback(dialect.transaction_framing()).await.unwrap();
            // A partitioned referencing side is catalogued as the declared
            // key plus a copy per partition. The plan can only name the
            // declared one, so a removal has to be matched against it or the
            // copy keeps the action alive after the plan took it away.
            conn.execute(
                "CREATE TABLE app.parted(id integer, ukey text) PARTITION BY RANGE (id); \
                 CREATE TABLE app.parted1 PARTITION OF app.parted FOR VALUES FROM (0) TO (10); \
                 ALTER TABLE app.parted ADD CONSTRAINT fk_parted FOREIGN KEY (ukey) \
                     REFERENCES app.p(ukey) ON UPDATE CASCADE; \
                 CREATE TRIGGER hook AFTER UPDATE ON app.parted1 FOR EACH ROW EXECUTE FUNCTION public.hook(); \
                 DROP TABLE app.c",
            )
            .await
            .unwrap();
            for (label, dropped, refused) in [
                ("nothing removed", Dropped::default(), true),
                (
                    "the declared key the plan drops first",
                    Dropped {
                        foreign_keys: [(
                            pbps_model::TableName::new("app", "parted"),
                            "fk_parted".to_owned(),
                        )]
                        .into(),
                        ..Default::default()
                    },
                    false,
                ),
            ] {
                conn.begin(dialect.transaction_framing()).await.unwrap();
                let checked = pbps_pg::data_triggers::prepare(
                    &mut conn,
                    std::slice::from_ref(&write),
                    &Default::default(),
                    &dropped,
                )
                .await;
                assert_eq!(
                    checked.is_err(),
                    refused,
                    "partitioned, {label}: {:?}",
                    checked.err().map(|e| e.to_string())
                );
                conn.rollback(dialect.transaction_framing()).await.unwrap();
            }
            conn.execute(
                "DROP TABLE app.parted; \
                 CREATE TABLE app.c(id integer PRIMARY KEY, ukey text, \
                     CONSTRAINT fk_c FOREIGN KEY (ukey) REFERENCES app.p(ukey) ON UPDATE CASCADE); \
                 CREATE TRIGGER hook AFTER UPDATE ON app.c FOR EACH ROW EXECUTE FUNCTION public.hook()",
            )
            .await
            .unwrap();
            // An action whose own trigger does not fire writes nothing.
            // Measured on 18.6: neither of these cascades at all.
            for (label, sql, reset) in [
                (
                    "the action's trigger is disabled",
                    "ALTER TABLE app.p DISABLE TRIGGER ALL",
                    "ALTER TABLE app.p ENABLE TRIGGER ALL",
                ),
                (
                    "the session is a replica",
                    "SET session_replication_role = 'replica'",
                    "RESET session_replication_role",
                ),
            ] {
                conn.execute(sql).await.unwrap();
                conn.begin(dialect.transaction_framing()).await.unwrap();
                let checked = pbps_pg::data_triggers::prepare(
                    &mut conn,
                    std::slice::from_ref(&write),
                    &Default::default(),
                    &Dropped::default(),
                )
                .await;
                assert!(
                    checked.is_ok(),
                    "{label}: {:?}",
                    checked.err().map(|e| e.to_string())
                );
                conn.rollback(dialect.transaction_framing()).await.unwrap();
                conn.execute(reset).await.unwrap();
                conn.begin(dialect.transaction_framing()).await.unwrap();
                let checked = pbps_pg::data_triggers::prepare(
                    &mut conn,
                    std::slice::from_ref(&write),
                    &Default::default(),
                    &Dropped::default(),
                )
                .await;
                assert!(
                    checked.is_err(),
                    "{label}: restored, the action fires again"
                );
                conn.rollback(dialect.transaction_framing()).await.unwrap();
            }
            // The same two with a partitioned referencing side. The copy
            // PostgreSQL catalogues on each partition owns no action trigger:
            // measured on 18.6, the referenced side carries one pair for the
            // *declared* constraint and nothing for the copies. Enablement
            // asked of the copy therefore finds no trigger at all and reads a
            // disabled action as a firing one, which drags a partition nothing
            // writes into the closure and refuses a valid plan.
            conn.execute(
                "DROP TABLE app.c; \
                 CREATE TABLE app.parted(id integer, ukey text) PARTITION BY RANGE (id); \
                 CREATE TABLE app.parted1 PARTITION OF app.parted FOR VALUES FROM (0) TO (10); \
                 ALTER TABLE app.parted ADD CONSTRAINT fk_parted FOREIGN KEY (ukey) \
                     REFERENCES app.p(ukey) ON UPDATE CASCADE; \
                 CREATE TRIGGER hook AFTER UPDATE ON app.parted1 \
                     FOR EACH ROW EXECUTE FUNCTION public.hook()",
            )
            .await
            .unwrap();
            for (label, sql, reset) in [
                (
                    "partitioned, the declared action's trigger is disabled",
                    "ALTER TABLE app.p DISABLE TRIGGER ALL",
                    "ALTER TABLE app.p ENABLE TRIGGER ALL",
                ),
                (
                    "partitioned, the session is a replica",
                    "SET session_replication_role = 'replica'",
                    "RESET session_replication_role",
                ),
            ] {
                conn.execute(sql).await.unwrap();
                conn.begin(dialect.transaction_framing()).await.unwrap();
                let checked = pbps_pg::data_triggers::prepare(
                    &mut conn,
                    std::slice::from_ref(&write),
                    &Default::default(),
                    &Dropped::default(),
                )
                .await;
                assert!(
                    checked.is_ok(),
                    "{label}: {:?}",
                    checked.err().map(|e| e.to_string())
                );
                conn.rollback(dialect.transaction_framing()).await.unwrap();
                conn.execute(reset).await.unwrap();
                conn.begin(dialect.transaction_framing()).await.unwrap();
                let checked = pbps_pg::data_triggers::prepare(
                    &mut conn,
                    std::slice::from_ref(&write),
                    &Default::default(),
                    &Dropped::default(),
                )
                .await;
                assert!(
                    checked.is_err(),
                    "{label}: restored, the action fires again"
                );
                conn.rollback(dialect.transaction_framing()).await.unwrap();
            }
        });
}

/// A referential action fires on the row the write leaves behind, not on the
/// statement's SET list: an approved BEFORE ROW UPDATE trigger can rewrite a
/// referenced key nothing set, and the action then cascades through it. The
/// table that cascade reaches is part of the closure or the boundary has a
/// hole in it — the trigger doing the rewriting is trusted, the one waiting on
/// the other side of the key need not be.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_key_a_before_trigger_can_rewrite_is_in_the_closure() {
    use pbps_dialect::{Dialect, RowOperation, RowWrite};
    let own = OwnDatabase::new(&server(), "fk_rewrite");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; \
        CREATE TABLE app.p(code text PRIMARY KEY, ukey text UNIQUE, other text UNIQUE); \
        CREATE TABLE app.c(id integer PRIMARY KEY, other text REFERENCES app.p(other) ON UPDATE CASCADE); \
        CREATE TABLE public.reached(value integer); \
        CREATE FUNCTION app.rewrite() RETURNS trigger LANGUAGE plpgsql AS \
        $$BEGIN NEW.other := 'rewritten'; RETURN NEW; END$$; \
        CREATE FUNCTION public.hook() RETURNS trigger LANGUAGE plpgsql AS \
        $$BEGIN INSERT INTO public.reached VALUES (1); RETURN NULL; END$$; \
        CREATE TRIGGER rewrite BEFORE UPDATE ON app.p FOR EACH ROW EXECUTE FUNCTION app.rewrite(); \
        INSERT INTO app.p VALUES ('first', 'k1', 'o1'); INSERT INTO app.c VALUES (1, 'o1')",
    );
    // The engine's own behaviour first, so the guard is answering a question
    // this server actually asks: an UPDATE of `ukey` alone reaches `app.c`.
    on_server(
        connection,
        "CREATE TRIGGER hook AFTER UPDATE ON app.c FOR EACH ROW EXECUTE FUNCTION public.hook()",
    );
    on_server(
        connection,
        "UPDATE app.p SET ukey = 'k2' WHERE code = 'first'",
    );
    assert_eq!(scalar(connection, "SELECT count(*) FROM public.reached"), 1);
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM app.c WHERE other = 'rewritten'"
        ),
        1
    );
    on_server(connection, "DROP TRIGGER hook ON app.c");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
                .await
                .unwrap();
            let dialect = pbps_pg::Postgres::new();
            let write = RowWrite {
                table: pbps_model::TableName::new("app", "p"),
                operation: RowOperation::Update {
                    columns: ["ukey".to_owned()].into(),
                },
            };
            // Both baselines are recorded while `rewrite` is the only trigger
            // there is, so the rewriting trigger is approved in each and the
            // one that arrives next is the only thing the guard can refuse.
            let wide = pbps_pg::catalog::introspect(&mut conn).await.unwrap().schema;
            conn.begin(dialect.transaction_framing()).await.unwrap();
            pbps_pg::data_triggers::prepare(
                &mut conn,
                std::slice::from_ref(&write),
                &wide,
                &Default::default(),
            )
            .await
            .expect("the approved rewriting trigger is not itself a refusal");
            conn.rollback(dialect.transaction_framing()).await.unwrap();
            conn.execute(
                "CREATE OR REPLACE TRIGGER rewrite BEFORE UPDATE OF code ON app.p \
                 FOR EACH ROW EXECUTE FUNCTION app.rewrite()",
            )
            .await
            .unwrap();
            let narrow = pbps_pg::catalog::introspect(&mut conn).await.unwrap().schema;
            // The unapproved trigger arrives, and the engine says what this
            // narrowed rewriter does: an UPDATE of `ukey` does not fire a
            // trigger that names `code`, so nothing rewrites the key and
            // nothing cascades.
            conn.execute(
                "CREATE TRIGGER hook AFTER UPDATE ON app.c FOR EACH ROW EXECUTE FUNCTION public.hook(); \
                 TRUNCATE public.reached; \
                 UPDATE app.p SET ukey = 'k3' WHERE code = 'first'",
            )
            .await
            .unwrap();
            let rows = conn
                .query("SELECT count(*)::int8 AS n FROM public.reached")
                .await
                .unwrap();
            assert_eq!(rows[0].try_get::<i64>("n").unwrap(), Some(0));
            // A rewriter the plan drops rewrites nothing when the row
            // statement runs: `DropModule` is `order_key` 0, and a closure
            // that widened for it would refuse a plan whose write never
            // reaches `app.c`.
            let removed = pbps_pg::data_triggers::Dropped {
                modules: [pbps_model::ModuleId::Trigger {
                    on: pbps_model::TableName::new("app", "p"),
                    name: "rewrite".to_owned(),
                }]
                .into(),
                ..Default::default()
            };
            for (label, form, baseline, dropped, refused) in [
                (
                    "the rewriter this statement fires",
                    "BEFORE UPDATE",
                    &wide,
                    &Default::default(),
                    true,
                ),
                (
                    "a rewriter this statement cannot fire",
                    "BEFORE UPDATE OF code",
                    &narrow,
                    &Default::default(),
                    false,
                ),
                (
                    "a rewriter the plan drops first",
                    "BEFORE UPDATE",
                    &wide,
                    &removed,
                    false,
                ),
            ] {
                conn.execute(&format!(
                    "CREATE OR REPLACE TRIGGER rewrite {form} ON app.p \
                     FOR EACH ROW EXECUTE FUNCTION app.rewrite()"
                ))
                .await
                .unwrap();
                conn.begin(dialect.transaction_framing()).await.unwrap();
                let checked = pbps_pg::data_triggers::prepare(
                    &mut conn,
                    std::slice::from_ref(&write),
                    baseline,
                    dropped,
                )
                .await;
                assert_eq!(
                    checked.is_err(),
                    refused,
                    "{label}: {:?}",
                    checked.err().map(|e| e.to_string())
                );
                conn.rollback(dialect.transaction_framing()).await.unwrap();
            }
            // Disabled, it rewrites nothing — measured — and the same
            // unapproved trigger on `app.c` is out of reach again.
            conn.execute(
                "CREATE OR REPLACE TRIGGER rewrite BEFORE UPDATE ON app.p \
                 FOR EACH ROW EXECUTE FUNCTION app.rewrite(); \
                 ALTER TABLE app.p DISABLE TRIGGER rewrite",
            )
            .await
            .unwrap();
            conn.begin(dialect.transaction_framing()).await.unwrap();
            let checked =
                pbps_pg::data_triggers::prepare(&mut conn, &[write], &wide, &Default::default())
                    .await;
            assert!(
                checked.is_ok(),
                "a disabled trigger writes nothing: {:?}",
                checked.err().map(|e| e.to_string())
            );
            conn.rollback(dialect.transaction_framing()).await.unwrap();
        });
}

/// A rewrite rule decides what a write does, so a table the closure reaches
/// carrying one is refused: the statements the rule adds are not in the plan,
/// and the triggers they fire are nobody's to authenticate.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_rewrite_rule_on_a_reached_table_is_refused() {
    use pbps_dialect::{Dialect, RowOperation, RowWrite};
    let own = OwnDatabase::new(&server(), "fk_rules");
    let connection = own.connection();
    on_server(
        connection,
        "CREATE SCHEMA app; CREATE TABLE public.reached(value integer); \
        CREATE TABLE app.p(code text PRIMARY KEY, ukey text UNIQUE); \
        CREATE TABLE app.c(ukey text REFERENCES app.p(ukey) ON UPDATE CASCADE ON DELETE CASCADE); \
        CREATE TABLE app.side(value text); \
        CREATE FUNCTION public.hook() RETURNS trigger LANGUAGE plpgsql AS \
        $$BEGIN INSERT INTO public.reached VALUES (1); RETURN NULL; END$$; \
        CREATE TRIGGER hook AFTER INSERT ON app.side FOR EACH STATEMENT EXECUTE FUNCTION public.hook(); \
        INSERT INTO app.p VALUES ('first', 'k1'); INSERT INTO app.c VALUES ('k1')",
    );
    // The engine first: the rule really does carry the cascade into a third
    // table and fire its trigger, which is the write nobody authenticated.
    on_server(
        connection,
        "CREATE RULE also AS ON UPDATE TO app.c DO ALSO INSERT INTO app.side VALUES ('from the rule'); \
        UPDATE app.p SET ukey = 'k2' WHERE code = 'first'",
    );
    assert_eq!(scalar(connection, "SELECT count(*) FROM public.reached"), 1);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, connection)
                .await
                .unwrap();
            let dialect = pbps_pg::Postgres::new();
            let write = |operation| RowWrite {
                table: pbps_model::TableName::new("app", "p"),
                operation,
            };
            let updates = write(RowOperation::Update {
                columns: ["ukey".to_owned()].into(),
            });
            for (label, write, refused) in [
                ("the event the rule is on", updates.clone(), true),
                // The rule is an ON UPDATE rule; the delete's own cascade
                // reaches the same table and that rule cannot rewrite it.
                (
                    "another event on the same table",
                    write(RowOperation::Delete),
                    false,
                ),
            ] {
                conn.begin(dialect.transaction_framing()).await.unwrap();
                let message = pbps_pg::data_triggers::prepare(
                    &mut conn,
                    &[write],
                    &Default::default(),
                    &Default::default(),
                )
                .await
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
                assert_eq!(
                    message.contains("unsafe rewrite rule") && message.contains("app.c"),
                    refused,
                    "{label}: {message}"
                );
                conn.rollback(dialect.transaction_framing()).await.unwrap();
            }
            // A rule lives on the relation a statement names. Measured, the
            // rewriter runs before partition routing, so a partition's rule
            // does not fire for the action that names its root.
            conn.execute(
                "CREATE TABLE app.parted(id integer, ukey text) PARTITION BY RANGE (id); \
                 CREATE TABLE app.parted1 PARTITION OF app.parted FOR VALUES FROM (0) TO (10); \
                 ALTER TABLE app.parted ADD FOREIGN KEY (ukey) REFERENCES app.p(ukey) ON UPDATE CASCADE; \
                 CREATE RULE also AS ON UPDATE TO app.parted1 DO ALSO INSERT INTO app.side VALUES ('leaf'); \
                 DROP TABLE app.c",
            )
            .await
            .unwrap();
            conn.begin(dialect.transaction_framing()).await.unwrap();
            let checked = pbps_pg::data_triggers::prepare(
                &mut conn,
                std::slice::from_ref(&updates),
                &Default::default(),
                &Default::default(),
            )
            .await;
            assert!(
                checked.is_ok(),
                "a partition's rule cannot rewrite the action that names its root: {:?}",
                checked.err().map(|e| e.to_string())
            );
            conn.rollback(dialect.transaction_framing()).await.unwrap();
            conn.execute(
                "CREATE RULE also AS ON UPDATE TO app.parted DO ALSO INSERT INTO app.side VALUES ('root')",
            )
            .await
            .unwrap();
            conn.begin(dialect.transaction_framing()).await.unwrap();
            let message = pbps_pg::data_triggers::prepare(
                &mut conn,
                std::slice::from_ref(&updates),
                &Default::default(),
                &Default::default(),
            )
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
            assert!(
                message.contains("unsafe rewrite rule") && message.contains("app.parted"),
                "the rule on the relation the action names: {message}"
            );
            conn.rollback(dialect.transaction_framing()).await.unwrap();
            conn.execute(
                "DROP TABLE app.parted; \
                 CREATE TABLE app.c(ukey text REFERENCES app.p(ukey) ON UPDATE CASCADE ON DELETE CASCADE); \
                 CREATE RULE also AS ON UPDATE TO app.c DO ALSO INSERT INTO app.side VALUES ('from the rule')",
            )
            .await
            .unwrap();
            // Disabled, it rewrites nothing.
            conn.execute("ALTER TABLE app.c DISABLE RULE also")
                .await
                .unwrap();
            conn.begin(dialect.transaction_framing()).await.unwrap();
            let checked = pbps_pg::data_triggers::prepare(
                &mut conn,
                &[updates],
                &Default::default(),
                &Default::default(),
            )
            .await;
            assert!(
                checked.is_ok(),
                "a disabled rule rewrites nothing: {:?}",
                checked.err().map(|e| e.to_string())
            );
            conn.rollback(dialect.transaction_framing()).await.unwrap();
        });
}

#[test]
#[ignore = "needs a live database; run the engine's live-test script"]
fn a_referenced_key_replacement_applies_and_the_next_plan_is_empty() {
    let server = server();
    for kind in ["index", "unique", "primary"] {
        for staged in [false, true] {
            let slug = format!("referenced_key_{kind}_{staged}");
            let own = OwnDatabase::new(&server, &slug);
            let connection = own.connection();
            let d = Demo::new(&slug);
            on_server(connection, "CREATE SCHEMA app");
            let key = if kind == "primary" {
                "primary_key: {name: old_key, columns: [id]}\n"
            } else if kind == "index" {
                "indexes:\n  old_key: {columns: [id], unique: true}\n"
            } else {
                "unique:\n  old_key: [id]\n"
            };
            let parent =
                format!("table: app.t\ncolumns:\n  id: {{type: integer, nullable: false}}\n{key}");
            d.table(&parent);
            std::fs::write(d.dir.join("schema/child.yml"),
                "table: app.child\ncolumns:\n  id: {type: integer}\nforeign_keys:\n  fk_child:\n    columns: [id]\n    references: app.t(id)\n").unwrap();
            let must_succeed = |out: Output| {
                assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
            };
            must_succeed(d.run(&["plan"]));
            d.commit();
            must_succeed(d.run(&["bootstrap", "--db", connection]));
            on_server(
                connection,
                "INSERT INTO app.t (id) VALUES (1); INSERT INTO app.child (id) VALUES (1);",
            );
            d.table(&parent.replace("old_key", "new_key"));
            must_succeed(d.run(&["plan"]));
            d.commit();
            let path = d.dir.join("key-plan.json");
            let mut args = vec!["plan", "--db", connection, "--out", path.to_str().unwrap()];
            if staged {
                args.push("--staged");
            }
            let planned = d.run(&args);
            if staged {
                assert_eq!(code(&planned), 1);
                assert!(stderr(&planned).contains("--staged applies one logical change"));
                assert!(!path.exists());
                continue;
            }
            must_succeed(planned);
            let saved: pbps_model::SavedPlan =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(saved.changes.changes.len(), 4, "{:?}", saved.changes);
            assert!(
                matches!(&saved.changes.changes[0].change, pbps_model::Change::DropForeignKey {name, ..} if name == "fk_child")
            );
            assert!(
                matches!(&saved.changes.changes[3].change, pbps_model::Change::AddForeignKey {name, ..} if name == "fk_child")
            );
            let checksum = plan_checksum(&path);
            let mut args = vec![
                "apply",
                "--db",
                connection,
                "--plan",
                path.to_str().unwrap(),
                "--checksum",
                &checksum,
                "--allow",
                "destructive,constraint",
            ];
            if staged {
                args.push("--staged");
            }
            must_succeed(d.run(&args));
            assert!(
                try_on_server(connection, "INSERT INTO app.child (id) VALUES (2)").is_err(),
                "the recreated FK must still reject an orphan"
            );
            must_succeed(d.run(&["verify", "--db", connection]));
            must_succeed(d.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()]));
            let next: pbps_model::SavedPlan =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert!(next.changes.is_empty(), "{:?}", next.changes);
        }
    }
}

#[test]
#[ignore = "needs a live database; run the engine's live-test script"]
fn external_referenced_key_dependencies_are_refused_before_planning_and_writing() {
    let server = server();
    for kind in ["index", "unique", "primary"] {
        let slug = format!("external_key_{kind}");
        let own = OwnDatabase::new(&server, &slug);
        let connection = own.connection();
        let d = Demo::new(&slug);
        on_server(connection, "CREATE SCHEMA app");
        let key = if kind == "primary" {
            "primary_key: {name: old_key, columns: [id]}\nunique:\n  other_key: [code]\n"
        } else if kind == "index" {
            "indexes:\n  old_key: {columns: [id], unique: true}\nunique:\n  other_key: [code]\n"
        } else {
            "unique:\n  old_key: [id]\n  other_key: [code]\n"
        };
        let parent = format!(
            "table: app.t\ncolumns:\n  id: {{type: integer, nullable: false}}\n  code: {{type: integer, nullable: false}}\n{key}"
        );
        let success = |out: Output| assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
        d.table(&parent);
        success(d.run(&["plan"]));
        d.commit();
        success(d.run(&["bootstrap", "--db", connection]));
        on_server(connection, "CREATE SCHEMA outside");
        let external =
            "CREATE TABLE outside.child (id integer CONSTRAINT external_fk REFERENCES app.t(id))";
        on_server(connection, external);
        d.table(&parent.replace("old_key", "new_key"));
        success(d.run(&["plan"]));
        d.commit();
        let path = d.dir.join("external-key-plan.json");
        let refused = d.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()]);
        assert_eq!(
            code(&refused),
            1,
            "{}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(
            stderr(&refused).contains("drop_blockers")
                && stderr(&refused).contains("outside.child"),
            "{}",
            stderr(&refused)
        );
        assert!(
            !path.exists(),
            "a refused plan must not produce an artifact"
        );
        on_server(connection, "DROP TABLE outside.child");
        // Another external FK still exists, but its backing key is unchanged.
        on_server(
            connection,
            "CREATE TABLE outside.unrelated (code integer CONSTRAINT unrelated_fk REFERENCES app.t(code))",
        );
        success(d.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()]));
        let checksum = plan_checksum(&path);
        // An unmanaged dependency arriving after approval must be caught again.
        on_server(connection, external);
        let args = [
            "apply",
            "--db",
            connection,
            "--plan",
            path.to_str().unwrap(),
            "--checksum",
            &checksum,
            "--allow",
            "destructive,constraint",
        ];
        let refused = d.run(&args);
        assert_eq!(
            code(&refused),
            1,
            "{}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(
            stderr(&refused).contains("drop_blockers")
                && stderr(&refused).contains("outside.child"),
            "{}",
            stderr(&refused)
        );
        on_server(
            connection,
            "DO $$ BEGIN IF NOT EXISTS (SELECT FROM pg_class WHERE oid=to_regclass('app.old_key')) OR EXISTS (SELECT FROM pg_class WHERE oid=to_regclass('app.new_key')) THEN RAISE EXCEPTION 'refused apply changed the key'; END IF; END $$",
        );
        on_server(connection, "DROP TABLE outside.child");
        success(d.run(&args));
        // The unrelated external dependency survives this permitted replacement.
        assert!(
            try_on_server(
                connection,
                "INSERT INTO outside.unrelated (code) VALUES (99)"
            )
            .is_err()
        );
    }
}

#[test]
fn impossible_identity_increments_are_refused_before_connecting() {
    let unreachable = "host=127.0.0.1 port=1 user=postgres password=no dbname=none";
    for (seed, increment) in [(1, 40000), (-1, -40000)] {
        let d = Demo::new(&format!("identity-span-validation-{increment}"));
        d.table(&format!("table: app.t\ncolumns:\n  id: {{type: smallint, nullable: false, identity: [{seed}, {increment}]}}\n"));
        succeeds(d.run(&["plan"]));
        d.commit();
        for args in [vec!["validate"], vec!["bootstrap", "--db", unreachable]] {
            let out = d.run(&args);
            assert_ne!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
            let message = format!("{}{}", stdout(&out), stderr(&out));
            assert!(
                message.contains("sequence span") && message.contains(&increment.to_string()),
                "{message}"
            );
            assert!(
                !message.contains("127.0.0.1"),
                "validation must precede connection: {message}"
            );
        }
    }
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB (see scripts/live-tests-pg.sh)"]
fn identity_increments_at_the_sequence_span_produce_two_values() {
    for (declared, low, high) in [
        ("smallint", i64::from(i16::MIN), i64::from(i16::MAX)),
        ("integer", i64::from(i32::MIN), i64::from(i32::MAX)),
        ("bigint", i64::MIN, i64::MAX),
    ] {
        for (direction, seed, increment, second) in
            [("up", 1, high - 1, high), ("down", -1, low + 1, low)]
        {
            let slug = format!("identity_span_{declared}_{direction}");
            let own = OwnDatabase::new(&server(), &slug);
            let connection = own.connection();
            on_server(connection, "CREATE SCHEMA app");
            let d = Demo::new(&slug);
            d.table(&format!("table: app.t\ncolumns:\n  id: {{type: {declared}, nullable: false, identity: [{seed}, {increment}]}}\nprimary_key: {{name: pk_t, columns: [id]}}\n"));
            succeeds(d.run(&["plan"]));
            d.commit();
            succeeds(d.run(&["validate"]));
            succeeds(d.run(&["bootstrap", "--db", connection]));
            assert_eq!(
                scalar(
                    connection,
                    "INSERT INTO app.t DEFAULT VALUES RETURNING id::bigint"
                ),
                seed
            );
            assert_eq!(
                scalar(
                    connection,
                    "INSERT INTO app.t DEFAULT VALUES RETURNING id::bigint"
                ),
                second
            );
            succeeds(d.run(&["verify", "--db", connection]));
        }
    }
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn failed_concurrent_index_builds_are_cleaned_recorded_and_retried_without_resume() {
    let own = OwnDatabase::new(&server(), "concurrent_index_recovery");
    let connection = own.connection();
    let mut base = "table: app.t\ncolumns:\n  id: {type: integer, nullable: false}\n".to_owned();
    let mut keys = vec!["id".to_owned()];
    // The emitted SQL exceeds the ledger reason limit. Recovery still has to
    // be recorded, rather than truncated behind the statement's column list.
    for n in 0..24 {
        let name = format!("column_{n:02}_{}", "x".repeat(48));
        base.push_str(&format!(
            "  {name}: {{type: integer, nullable: false, default: '1'}}\n"
        ));
        keys.push(name);
    }
    let d = bootstrapped_demo(connection, "concurrent-index-recovery", &base);
    on_server(connection, "INSERT INTO app.t(id) VALUES (1)");
    d.table(&format!(
        "{base}strategy: {{online: true}}\nindexes:\n  ix_recovery: {{columns: [{}], unique: true}}\n", keys.join(", ")
    ));
    let plan = connected_artifact(&d, connection, true);
    // Introduce the duplicate only after the preflight has passed. This
    // controlled DDL trigger pins the engine failure without a timing race.
    on_server(
        connection,
        r#"
CREATE SCHEMA witness;
CREATE FUNCTION witness.duplicate_at_index_build() RETURNS event_trigger LANGUAGE plpgsql AS $$
BEGIN
  IF current_query() LIKE '%ix_recovery%' THEN
    INSERT INTO app.t(id) VALUES (1);
  END IF;
END $$;
CREATE EVENT TRIGGER duplicate_at_index_build ON ddl_command_start
  WHEN TAG IN ('CREATE INDEX') EXECUTE FUNCTION witness.duplicate_at_index_build();
"#,
    );
    for _ in 0..2 {
        let failed = approved_apply(
            &d,
            connection,
            &plan,
            &["--staged", "--allow", "constraint"],
        );
        assert_eq!(code(&failed), 1, "{}{}", stdout(&failed), stderr(&failed));
        assert!(
            stderr(&failed).contains("could not create unique index"),
            "{}",
            stderr(&failed)
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_class WHERE oid=to_regclass('app.ix_recovery')"
            ),
            0,
            "the failed build must not leave its invalid index behind"
        );
        let recorded = latest_snapshot(connection);
        assert_eq!(recorded.kind, pbps_model::StateKind::Failed);
        assert!(recorded.staged.is_none());
        let reason = recorded.reason.as_deref().unwrap();
        assert!(
            reason.contains("ix_recovery") && reason.contains("removed invalid index"),
            "{reason}"
        );
        assert!(reason.contains("23505"), "{reason}");
        assert!(
            stderr(&failed).contains("no staged checkpoint"),
            "{}",
            stderr(&failed)
        );
        let resume = approved_apply(
            &d,
            connection,
            &plan,
            &["--staged", "--resume", "--allow", "constraint"],
        );
        assert_eq!(code(&resume), 1);
        assert!(
            stderr(&resume).contains("no staged apply in progress"),
            "{}",
            stderr(&resume)
        );
        on_server(
            connection,
            "TRUNCATE app.t; INSERT INTO app.t(id) VALUES (1)",
        );
    }
    on_server(
        connection,
        r#"
CREATE FUNCTION witness.deny_recovery() RETURNS event_trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'cleanup-sensitive-value' USING ERRCODE='P0001'; END $$;
CREATE EVENT TRIGGER deny_recovery ON ddl_command_start WHEN TAG IN ('DROP INDEX')
  EXECUTE FUNCTION witness.deny_recovery();
"#,
    );
    let failed = approved_apply(
        &d,
        connection,
        &plan,
        &["--staged", "--allow", "constraint"],
    );
    assert_eq!(code(&failed), 1);
    assert!(
        stderr(&failed).contains("could not create unique index"),
        "{}",
        stderr(&failed)
    );
    let reason = latest_snapshot(connection).reason.unwrap();
    assert!(
        reason.contains("Recovery of index") && reason.contains("artifact may remain"),
        "{reason}"
    );
    assert!(
        reason.contains("23505") && !reason.contains("cleanup-sensitive-value"),
        "{reason}"
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_index WHERE indexrelid='app.ix_recovery'::regclass AND NOT indisvalid"
        ),
        1
    );
    on_server(
        connection,
        "DROP EVENT TRIGGER deny_recovery; DROP EVENT TRIGGER duplicate_at_index_build",
    );
    on_server(connection, "DROP INDEX CONCURRENTLY app.ix_recovery");
    on_server(
        connection,
        "TRUNCATE app.t; INSERT INTO app.t(id) VALUES (1)",
    );
    succeeds(approved_apply(
        &d,
        connection,
        &plan,
        &["--staged", "--allow", "constraint"],
    ));
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_index WHERE indexrelid='app.ix_recovery'::regclass AND indisvalid"
        ),
        1
    );
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn doctor_json_reports_external_schema_absence_before_grant_authority() {
    struct Roles(String, Vec<String>);
    impl Drop for Roles {
        fn drop(&mut self) {
            for role in &self.1 {
                let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {role}"));
            }
        }
    }
    let server = server();
    let roles = Roles(
        server.clone(),
        vec![
            format!("pbps_doctor_ext333_d_{}", std::process::id()),
            format!("pbps_doctor_ext333_r_{}", std::process::id()),
        ],
    );
    let deployer = &roles.1[0];
    let reader = &roles.1[1];
    let own = OwnDatabase::new(&server, "doctor-external333");
    let connection = own.connection();
    on_server(
        connection,
        &format!(
            "CREATE ROLE {deployer} LOGIN PASSWORD 'doctor-test'; CREATE ROLE {reader};
         GRANT CREATE ON SCHEMA public TO {deployer};
         CREATE SCHEMA app AUTHORIZATION {deployer};
         CREATE TABLE app.t (id bigint NOT NULL PRIMARY KEY); ALTER TABLE app.t OWNER TO {deployer};
         CREATE TABLE public.\"External Space\"(id integer)"
        ),
    );
    let login = format!(
        "{} user={deployer} password=doctor-test",
        connection
            .split_whitespace()
            .filter(|w| !w.starts_with("user=") && !w.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let d = Demo::new("doctor-external333");
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: postgres\nenvironments:\n  dev:\n    url_env: PBPS_FLOW_PG_DEV\n",
    )
    .unwrap();
    d.table(ONE_COLUMN);
    std::fs::write(
        d.dir.join("schema/reader.yml"),
        format!("role: {reader}\ngrants:\n  'schema::External Space': [usage]\n"),
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let diagnose = || {
        let out = d.run_with_env(
            &["doctor", "--format", "json"],
            &[("PBPS_FLOW_PG_DEV", login.as_str())],
        );
        let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
        (code(&out), value)
    };
    let (exit, absent) = diagnose();
    assert_eq!(exit, 2, "{absent}");
    assert_eq!(
        absent["data"]["environments"][0]["absent_schemas"],
        serde_json::json!(["External Space"])
    );
    let finding = absent["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "schema.absent")
        .unwrap();
    assert_eq!(finding["remedy"], "CREATE SCHEMA \"External Space\";");
    assert_eq!(
        absent["data"]["environments"][0]["missing_permissions"],
        serde_json::json!([])
    );
    on_server(connection, "CREATE SCHEMA \"External Space\"");
    let (_, present) = diagnose();
    assert!(
        present["data"]["environments"][0]
            .get("absent_schemas")
            .is_none(),
        "{present}"
    );
    assert!(
        !present["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "schema.absent")
    );
    let gaps = present["data"]["environments"][0]["missing_permissions"]
        .as_array()
        .unwrap();
    assert_eq!(gaps.len(), 1, "{present}");
    assert!(
        gaps[0]
            .as_str()
            .unwrap()
            .starts_with("USAGE WITH GRANT OPTION on SCHEMA \"External Space\"")
    );
    on_server(
        connection,
        &format!("GRANT USAGE ON SCHEMA \"External Space\" TO {deployer} WITH GRANT OPTION"),
    );
    let (_, authorized) = diagnose();
    assert!(
        authorized["data"]["environments"][0]
            .get("absent_schemas")
            .is_none(),
        "{authorized}"
    );
    assert_eq!(
        authorized["data"]["environments"][0]["missing_permissions"],
        serde_json::json!([])
    );
}

#[test]
#[ignore = "needs live PostgreSQL"]
fn doctor_json_follows_recorded_table_and_column_ids_before_pending_renames() {
    struct Role(String, String);
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let server = server();
    let role = Role(
        server.clone(),
        format!("pbps_doctor_data331_{}", std::process::id()),
    );
    let own = OwnDatabase::new(&server, "doctor-data331");
    let connection = own.connection();
    on_server(
        connection,
        &format!(
            "CREATE ROLE {} LOGIN PASSWORD 'doctor-test'; GRANT CREATE ON SCHEMA public TO {}; CREATE SCHEMA app AUTHORIZATION {}",
            role.1, role.1, role.1
        ),
    );
    let login = format!(
        "{} user={} password=doctor-test",
        connection
            .split_whitespace()
            .filter(|w| !w.starts_with("user=") && !w.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" "),
        role.1
    );
    let d = Demo::new("doctor-data331");
    d.table("table: app.old_name\ncolumns:\n  id: {type: integer, nullable: false}\n  old_label: {type: text, default: \"'seed'\"}\nprimary_key: [id]\n");
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", &login]));
    d.table("table: app.new_name\nrenamed_from: app.old_name\ncolumns:\n  id: {type: integer, nullable: false}\n  label: {type: text, default: \"'seed'\", renamed_from: old_label}\nprimary_key: [id]\ndata:\n  mode: ensure\n  rows:\n    1: {}\n");
    succeeds(d.run(&["plan"]));
    on_server(
        connection,
        &format!(
            "REVOKE INSERT, UPDATE ON app.old_name FROM {}; GRANT INSERT(id), UPDATE(old_label) ON app.old_name TO {}",
            role.1, role.1
        ),
    );
    let diagnose = || {
        let out = d.run(&["doctor", "--db", &login, "--format", "json"]);
        let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
        (code(&out), value)
    };
    let (ok_code, covered) = diagnose();
    assert_eq!(ok_code, 0, "{covered}");
    assert_eq!(
        covered["data"]["environments"][0]["missing_permissions"],
        serde_json::json!([])
    );
    on_server(
        connection,
        &format!("REVOKE UPDATE(old_label) ON app.old_name FROM {}", role.1),
    );
    let (gap_code, missing) = diagnose();
    assert_eq!(gap_code, 2, "{missing}");
    let gaps = missing["data"]["environments"][0]["missing_permissions"]
        .as_array()
        .unwrap();
    assert_eq!(gaps.len(), 1, "{missing}");
    let gap = gaps[0].as_str().unwrap();
    assert!(gap.contains("UPDATE") && gap.contains("old_name"), "{gap}");
    assert!(!gap.contains("new_name"), "{gap}");
    on_server(
        connection,
        &format!("GRANT UPDATE(old_label) ON app.old_name TO {}", role.1),
    );
    assert_eq!(diagnose().0, 0);
}

fn connected_policy_change(connection: &str, slug: &str) -> Demo {
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new(slug);
    d.table(TWO_COLUMNS);
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    d.table("table: app.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  added: {type: integer}\nprimary_key: {name: pk_t, columns: [id]}\n");
    succeeds(d.run(&["drop", "app.t.label", "--reason", "retire the old field"]));
    succeeds(d.run(&["plan"]));
    d.commit();
    d
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn connected_plan_json_retains_error_policy_findings_without_artifacts() {
    let own = OwnDatabase::new(&server(), "json-policy-error");
    let connection = own.connection();
    let d = connected_policy_change(connection, "json-policy-error");
    std::fs::write(
        d.dir.join("pbps.yml"),
        "dialect: postgres\npolicies:\n  rules:\n    change.expand-contract: error\n",
    )
    .unwrap();
    let plan = d.dir.join("rejected.json");
    let sql = d.dir.join("rejected.sql");
    let args = [
        "plan",
        "--db",
        connection,
        "--format",
        "json",
        "--out",
        plan.to_str().unwrap(),
        "--sql",
        sql.to_str().unwrap(),
    ];
    let out = d.run(&args);
    assert_eq!(code(&out), 2, "{}{}", stdout(&out), stderr(&out));
    let report = json_output(out);
    assert_eq!(report["result"], "findings", "{report}");
    let findings = report["findings"].as_array().unwrap();
    assert_eq!(
        findings
            .iter()
            .filter(|f| f["id"] == "change.expand-contract" && f["severity"] == "error")
            .count(),
        1,
        "{report}"
    );
    assert!(
        !findings.iter().any(|f| f["id"] == "plan.failed"),
        "{report}"
    );
    assert!(!plan.exists() && !sql.exists());
    // A refused report cannot overwrite an artifact from an earlier attempt.
    std::fs::write(&plan, "previous plan").unwrap();
    std::fs::write(&sql, "previous SQL").unwrap();
    assert_eq!(code(&d.run(&args)), 2);
    assert_eq!(std::fs::read_to_string(&plan).unwrap(), "previous plan");
    assert_eq!(std::fs::read_to_string(&sql).unwrap(), "previous SQL");
    let human = d.run(&["plan", "--db", connection]);
    assert_eq!(code(&human), 1, "{}{}", stdout(&human), stderr(&human));
    assert!(stderr(&human).contains("error: change.expand-contract"));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn connected_plan_json_keeps_policy_warnings_without_duplicate_prose() {
    let own = OwnDatabase::new(&server(), "json-policy-warning");
    let connection = own.connection();
    let d = connected_policy_change(connection, "json-policy-warning");
    let path = d.dir.join("approved.json");
    let out = succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--format",
        "json",
        "--out",
        path.to_str().unwrap(),
    ]));
    assert!(
        !stderr(&out).contains("change.expand-contract"),
        "{}",
        stderr(&out)
    );
    let report = json_output(out);
    assert_eq!(report["result"], "ok");
    assert_eq!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["id"] == "change.expand-contract" && f["severity"] == "warning")
            .count(),
        1,
        "{report}"
    );
    assert!(report["data"]["changes"].as_u64().unwrap() > 0);
    assert!(report["data"]["connected_checks"].is_array());
    let saved: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(!saved.changes.is_empty());
    let human = succeeds(d.run(&["plan", "--db", connection]));
    assert!(stderr(&human).contains("warning: change.expand-contract"));
    assert!(stdout(&human).contains("Baseline:"));
}

#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn connected_plan_json_retains_identity_remedies_and_operational_errors() {
    let own = OwnDatabase::new(&server(), "json-identity");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("json-identity");
    d.table(TWO_COLUMNS);
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    d.table(ONE_COLUMN);
    let path = d.dir.join("identity.json");
    let sql = d.dir.join("identity.sql");
    let out = d.run(&[
        "plan",
        "--db",
        connection,
        "--format",
        "json",
        "--out",
        path.to_str().unwrap(),
        "--sql",
        sql.to_str().unwrap(),
    ]);
    assert_eq!(code(&out), 2, "{}{}", stdout(&out), stderr(&out));
    assert!(stderr(&out).is_empty(), "{}", stderr(&out));
    let connected = json_output(out);
    assert_eq!(connected["result"], "findings");
    let offline = d.run(&["plan", "--check", "--format", "json"]);
    assert_eq!(
        code(&offline),
        2,
        "{}{}",
        stdout(&offline),
        stderr(&offline)
    );
    let offline = json_output(offline);
    assert_eq!(connected["findings"], offline["findings"]);
    let blocker = connected["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "identity.drop-column-needs-reason")
        .unwrap();
    assert!(
        blocker["remedy"]
            .as_str()
            .unwrap()
            .contains("pbps drop app.t.label")
    );
    assert!(!path.exists() && !sql.exists());
    let human = d.run(&["plan", "--db", connection]);
    assert_eq!(code(&human), 1);
    assert!(stderr(&human).contains("pbps drop app.t.label"));
    d.table("table: app.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  renamed: {type: varchar(50)}\nprimary_key: {name: pk_t, columns: [id]}\n");
    let ambiguous = d.run(&["plan", "--db", connection, "--format", "json"]);
    assert_eq!(
        code(&ambiguous),
        2,
        "{}{}",
        stdout(&ambiguous),
        stderr(&ambiguous)
    );
    assert!(stderr(&ambiguous).is_empty(), "{}", stderr(&ambiguous));
    let ambiguous = json_output(ambiguous);
    let offline = json_output(d.run(&["plan", "--check", "--format", "json"]));
    assert_eq!(ambiguous["findings"], offline["findings"]);
    assert!(
        ambiguous["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "identity.ambiguous-columns"
                && f["remedy"]
                    .as_str()
                    .unwrap()
                    .contains("pbps rename app.t.label renamed")),
        "{ambiguous}"
    );
    d.table(ONE_COLUMN);
    succeeds(d.run(&["drop", "app.t.label", "--reason", "retire the old field"]));
    succeeds(d.run(&["plan"]));
    d.commit();
    let resolved = succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--format",
        "json",
        "--out",
        path.to_str().unwrap(),
        "--sql",
        sql.to_str().unwrap(),
    ]));
    assert_eq!(json_output(resolved)["result"], "ok");
    assert!(path.exists() && sql.exists());
    let unreachable = d.run(&[
        "plan",
        "--db",
        "host=127.0.0.1 port=1 user=postgres dbname=unreachable connect_timeout=1",
        "--format",
        "json",
    ]);
    assert_eq!(
        code(&unreachable),
        1,
        "{}{}",
        stdout(&unreachable),
        stderr(&unreachable)
    );
    let report = json_output(unreachable);
    assert_eq!(report["result"], "unanswerable");
    assert_eq!(report["findings"][0]["id"], "plan.failed");
}

/// A foreign key's target is somebody else's table even when it shares a
/// schema with the declarations, and the deployer needs `SELECT` and
/// `REFERENCES` **on it** (issues #315 and #510).
///
/// The filter used to be the schema, because the managed `SELECT` was asked
/// there too. Both dialects have since narrowed that read to the tables, and
/// an undeclared parent inside a managed schema fell out of both lists: on
/// this engine a schema carries only `USAGE` and `CREATE`, so nothing covered
/// either half. Measured here rather than reasoned about — the report, the
/// exit code, and the statements the two privileges authorize.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn doctor_asks_about_an_undeclared_foreign_key_parent_in_a_managed_schema() {
    struct Role(String, String);
    impl Drop for Role {
        fn drop(&mut self) {
            let _ = try_on_server(&self.0, &format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let server = server();
    let role = Role(
        server.clone(),
        format!("pbps_doctor_fk315_{}", std::process::id()),
    );
    let deployer = &role.1;
    let own = OwnDatabase::new(&server, "doctor_fk315");
    let connection = own.connection();
    // `app.parent` shares the managed schema and is not declared; `far.parent`
    // is the case that already worked, outside it. Both are owned by somebody
    // else, so the deployer holds nothing on them but what is granted below.
    on_server(
        connection,
        &format!(
            "CREATE ROLE {deployer} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE PASSWORD 'doctor-test';
             GRANT CREATE ON SCHEMA public TO {deployer};
             CREATE SCHEMA app AUTHORIZATION {deployer};
             CREATE SCHEMA far;
             GRANT USAGE ON SCHEMA far TO {deployer};
             CREATE TABLE app.parent (code bigint NOT NULL PRIMARY KEY);
             CREATE TABLE far.parent (code bigint NOT NULL PRIMARY KEY);
             GRANT SELECT, REFERENCES ON far.parent TO {deployer}"
        ),
    );
    let login = format!(
        "{} user={deployer} password=doctor-test",
        connection
            .split_whitespace()
            .filter(|w| !w.starts_with("user=") && !w.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let d = Demo::new("doctor-fk315");
    // A declared table pointing at both, and a second key into the same
    // undeclared parent: the target is asked about once however many keys
    // name it.
    d.table(
        "table: app.child\ncolumns:\n  id: {type: bigint, nullable: false}\n  \
         other: {type: bigint}\n  away: {type: bigint}\n\
         primary_key: {name: pk_child, columns: [id]}\n\
         foreign_keys:\n  \
         fk_near:\n    columns: [id]\n    references: app.parent(code)\n  \
         fk_near_again:\n    columns: [other]\n    references: app.parent(code)\n  \
         fk_far:\n    columns: [away]\n    references: far.parent(code)\n",
    );
    succeeds(d.run(&["plan"]));
    d.commit();
    // Granted for the bootstrap itself, which really does write the key: the
    // point of the check is that it is needed, so a deployer without it
    // cannot get as far as having a ledger to ask about.
    on_server(
        connection,
        &format!("GRANT SELECT, REFERENCES ON app.parent TO {deployer}"),
    );
    succeeds(d.run(&["bootstrap", "--db", &login]));
    on_server(
        connection,
        &format!("REVOKE SELECT, REFERENCES ON app.parent FROM {deployer}"),
    );

    let diagnose = || {
        let out = d.run(&["doctor", "--db", &login, "--format", "json"]);
        let value: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
        (code(&out), value)
    };
    // The report spells a name the way a `GRANT` would.
    const PARENT: &str = "\"app\".\"parent\"";
    const FAR: &str = "\"far\".\"parent\"";
    let named = |report: &serde_json::Value| -> Vec<String> {
        report["data"]["environments"][0]["missing_permissions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    };

    // Neither privilege, and the managed schema's own rights say nothing
    // about them: two gaps on the one table, and none on the declared child
    // the deployer owns or on the external target it was granted.
    let (exit, none) = diagnose();
    assert_eq!(exit, 2, "{none}");
    let missing = named(&none);
    for privilege in ["SELECT", "REFERENCES"] {
        assert_eq!(
            missing
                .iter()
                .filter(|m| m.contains(privilege) && m.contains(PARENT))
                .count(),
            1,
            "{privilege} on the undeclared parent, named once: {none}"
        );
    }
    assert!(
        !missing.iter().any(|m| m.contains(FAR)),
        "the external target is granted and must not be reported: {none}"
    );
    assert!(
        !missing.iter().any(|m| m.contains("\"child\"")),
        "the declared child is the deployer's own: {none}"
    );

    // One at a time, so each gap is named for the privilege that is actually
    // absent rather than for whichever is checked first.
    on_server(
        connection,
        &format!("GRANT SELECT ON app.parent TO {deployer}"),
    );
    let (exit, read_only) = diagnose();
    assert_eq!(exit, 2, "{read_only}");
    let missing = named(&read_only);
    assert!(
        missing
            .iter()
            .any(|m| m.contains("REFERENCES") && m.contains(PARENT)),
        "{read_only}"
    );
    assert!(
        !missing
            .iter()
            .any(|m| m.contains("SELECT") && m.contains(PARENT)),
        "{read_only}"
    );

    on_server(
        connection,
        &format!(
            "REVOKE SELECT ON app.parent FROM {deployer}; GRANT REFERENCES ON app.parent TO {deployer}"
        ),
    );
    let (exit, write_only) = diagnose();
    assert_eq!(exit, 2, "{write_only}");
    let missing = named(&write_only);
    assert!(
        missing
            .iter()
            .any(|m| m.contains("SELECT") && m.contains(PARENT)),
        "{write_only}"
    );
    assert!(
        !missing
            .iter()
            .any(|m| m.contains("REFERENCES") && m.contains(PARENT)),
        "{write_only}"
    );

    // Both, and the report is clean — and the deployer really can do what the
    // plan will ask of it.
    on_server(
        connection,
        &format!("GRANT SELECT ON app.parent TO {deployer}"),
    );
    let (exit, ready) = diagnose();
    assert_eq!(exit, 0, "{ready}");
    assert_eq!(
        ready["data"]["environments"][0]["missing_permissions"],
        serde_json::json!([]),
        "{ready}"
    );
    // And both gaps were the truth rather than caution: each privilege
    // authorizes one of the two statements a key into this parent makes.
    assert_eq!(scalar(&login, "SELECT count(*) FROM app.parent"), 0);
    on_server(
        connection,
        &format!("REVOKE REFERENCES ON app.parent FROM {deployer}"),
    );
    assert!(
        try_on_server(
            &login,
            "ALTER TABLE app.child ADD CONSTRAINT fk_probe \
             FOREIGN KEY (other) REFERENCES app.parent (code)"
        )
        .is_err(),
        "REFERENCES on the parent is what authorizes the key"
    );
    on_server(
        connection,
        &format!("REVOKE SELECT ON app.parent FROM {deployer}"),
    );
    assert!(
        try_on_server(&login, "SELECT count(*) FROM app.parent").is_err(),
        "SELECT on the parent is what authorizes the probe's read"
    );
}

/// A fingerprint key for the routine-pin tests: base64 of 32 copies of `byte`.
fn fingerprint_key(byte: u8) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode([byte; 32])
}

/// The routine-pin setup (DEC-319.1): a bootstrapped project whose database
/// also holds `ext.helper(integer)`, owned by a role short of a superuser and
/// managed by nothing, and an environment that names a fingerprint key.
struct PinnedHelper {
    db: OwnDatabase,
    demo: Demo,
    owner: String,
}

impl PinnedHelper {
    const URL: &'static str = "PBPS_319_PIN_DB";
    const KEY: &'static str = "PBPS_319_PIN_KEY";

    fn new(slug: &str) -> Self {
        let db = OwnDatabase::new(&server(), slug);
        let owner = format!("pbps_pin_owner_{}", std::process::id());
        let _ = try_on_server(db.connection(), &format!("DROP ROLE IF EXISTS {owner}"));
        on_server(
            db.connection(),
            &format!(
                "CREATE ROLE {owner} NOSUPERUSER; CREATE SCHEMA ext AUTHORIZATION {owner}; \
                 SET ROLE {owner}; \
                 CREATE FUNCTION ext.helper(integer) RETURNS boolean LANGUAGE sql \
                   AS 'SELECT $1 > 0'; \
                 RESET ROLE"
            ),
        );
        let demo = bootstrapped_demo(db.connection(), slug, ONE_COLUMN);
        std::fs::write(
            demo.dir.join("pbps.yml"),
            format!(
                "dialect: postgres\nenvironments:\n  dev:\n    url_env: {}\n    \
                 fingerprint_key_env: {}\n",
                Self::URL,
                Self::KEY
            ),
        )
        .unwrap();
        demo.table(TWO_COLUMNS);
        succeeds(demo.run(&["plan"]));
        demo.commit();
        Self { db, demo, owner }
    }

    fn run(&self, args: &[&str], key: Option<&str>) -> Output {
        let mut env = vec![(Self::URL, self.db.connection())];
        if let Some(key) = key {
            env.push((Self::KEY, key));
        }
        self.demo.run_with_env(args, &env)
    }

    fn plan(&self, staged: bool, key: &str) -> PathBuf {
        let path = self.demo.dir.join("pinned-plan.json");
        let mut args = vec!["plan", "--env", "dev", "--out", path.to_str().unwrap()];
        if staged {
            args.push("--staged");
        }
        succeeds(self.run(&args, Some(key)));
        path
    }

    fn apply(&self, plan: &std::path::Path, staged: bool, key: &str) -> Output {
        let checksum = plan_checksum(plan);
        let mut args = vec![
            "apply",
            "--env",
            "dev",
            "--plan",
            plan.to_str().unwrap(),
            "--checksum",
            &checksum,
        ];
        if staged {
            args.push("--staged");
        }
        self.run(&args, Some(key))
    }

    fn replace_helper(&self) {
        on_server(
            self.db.connection(),
            &format!(
                "SET ROLE {}; CREATE OR REPLACE FUNCTION ext.helper(integer) RETURNS boolean \
                 LANGUAGE sql AS 'SELECT true'; RESET ROLE",
                self.owner
            ),
        );
    }

    fn label_exists(&self) -> bool {
        scalar(
            self.db.connection(),
            "SELECT count(*) FROM information_schema.columns \
             WHERE table_schema = 'app' AND table_name = 't' AND column_name = 'label'",
        ) == 1
    }
}

impl Drop for PinnedHelper {
    fn drop(&mut self) {
        let _ = try_on_server(
            &self.db.server,
            &format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", self.db.name),
        );
        let _ = try_on_server(
            &self.db.server,
            &format!("DROP ROLE IF EXISTS {}", self.owner),
        );
    }
}

#[test]
#[ignore = "needs live PostgreSQL"]
fn a_routine_replaced_after_approval_refuses_the_apply_before_its_first_statement() {
    let h = PinnedHelper::new("pin_replaced");
    let key = fingerprint_key(1);
    let plan = h.plan(false, &key);
    let saved: pbps_model::SavedPlan =
        serde_json::from_str(&std::fs::read_to_string(&plan).unwrap()).unwrap();
    let pins = saved.routine_pins.expect("the helper is pinned");
    assert_eq!(
        pins.schemas
            .iter()
            .map(|s| s.schema.as_str())
            .collect::<Vec<_>>(),
        ["ext"],
        "only the unmanaged, non-superuser routine is pinned"
    );
    let raw = std::fs::read_to_string(&plan).unwrap();
    assert!(!raw.contains("$1 > 0"), "the plan carries no routine body");

    h.replace_helper();
    let refused = h.apply(&plan, false, &key);
    assert_ne!(code(&refused), 0, "{}", stdout(&refused));
    assert!(
        stderr(&refused).contains("pinned routines changed in `ext`")
            && stderr(&refused).contains("before the pre-flight probes"),
        "{}",
        stderr(&refused)
    );
    assert!(!h.label_exists(), "no statement ran");

    // Negative: planned again against the replaced helper, the same change
    // applies.
    let plan = h.plan(false, &key);
    succeeds(h.apply(&plan, false, &key));
    assert!(h.label_exists());
}

#[test]
#[ignore = "needs live PostgreSQL"]
fn a_plan_that_must_pin_a_routine_is_refused_without_a_key_and_names_the_remedy() {
    let h = PinnedHelper::new("pin_no_key");
    let refused = h.run(&["plan", "--env", "dev"], None);
    assert_ne!(code(&refused), 0);
    assert!(
        stderr(&refused).contains("`ext` (1)")
            && stderr(&refused).contains("pbps key generate")
            && stderr(&refused).contains("fingerprint_key_env"),
        "{}",
        stderr(&refused)
    );
    let bare = h.demo.run(&["plan", "--db", h.db.connection()]);
    assert_ne!(code(&bare), 0);
    assert!(stderr(&bare).contains("--env"), "{}", stderr(&bare));

    // Negative: once the helper belongs to a superuser nobody else holds,
    // nothing is pinned and no key is needed.
    on_server(
        h.db.connection(),
        "ALTER FUNCTION ext.helper(integer) OWNER TO CURRENT_USER",
    );
    succeeds(h.run(&["plan", "--env", "dev"], None));
}

#[test]
#[ignore = "needs live PostgreSQL"]
fn a_plan_pinned_under_one_key_is_refused_under_another() {
    let h = PinnedHelper::new("pin_other_key");
    let plan = h.plan(false, &fingerprint_key(1));
    let refused = h.apply(&plan, false, &fingerprint_key(2));
    assert_ne!(code(&refused), 0);
    assert!(
        stderr(&refused).contains("no evidence under another"),
        "{}",
        stderr(&refused)
    );
    assert!(!h.label_exists());
}

/// A superuser's event trigger replaces the helper while the plan's own
/// `ALTER TABLE` runs, which is the window only the closing check sees.
fn replace_helper_during_alter_table(h: &PinnedHelper) {
    on_server(
        h.db.connection(),
        "CREATE FUNCTION public.pbps_319_swap() RETURNS event_trigger LANGUAGE plpgsql AS $$ \
         BEGIN \
           CREATE OR REPLACE FUNCTION ext.helper(integer) RETURNS boolean \
             LANGUAGE sql AS 'SELECT false'; \
         END $$; \
         CREATE EVENT TRIGGER pbps_319_swap ON ddl_command_end \
           WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION public.pbps_319_swap()",
    );
}

#[test]
#[ignore = "needs live PostgreSQL"]
fn a_routine_replaced_while_the_statements_run_rolls_the_apply_back() {
    let h = PinnedHelper::new("pin_during_ddl");
    let key = fingerprint_key(1);
    let plan = h.plan(false, &key);
    replace_helper_during_alter_table(&h);
    let refused = h.apply(&plan, false, &key);
    assert_ne!(code(&refused), 0, "{}", stdout(&refused));
    assert!(
        stderr(&refused).contains("after the statements, before recording")
            && stderr(&refused).contains("rolled back"),
        "{}",
        stderr(&refused)
    );
    assert!(!h.label_exists(), "the transaction was rolled back");
    assert_eq!(
        text_of(
            h.db.connection(),
            "SELECT prosrc FROM pg_proc WHERE oid = 'ext.helper(integer)'::regprocedure"
        ),
        "SELECT $1 > 0",
        "the replacement rolled back with the DDL"
    );
}

/// Another session replaces the helper while the apply's `ALTER TABLE` waits
/// on a lock, after the in-transaction check before the first statement has
/// already read `pg_proc`. Under a `repeatable read` default, a bare `BEGIN`
/// would keep that first snapshot for the closing check, which would then pass
/// (DEC-319.1).
#[test]
#[ignore = "needs live PostgreSQL"]
fn a_concurrent_replacement_is_seen_under_a_repeatable_read_default() {
    let h = PinnedHelper::new("pin_rr_default");
    let key = fingerprint_key(1);
    let plan = h.plan(false, &key);
    on_server(
        h.db.connection(),
        &format!(
            "ALTER DATABASE \"{}\" SET default_transaction_isolation = 'repeatable read'",
            h.db.name
        ),
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let connect = || async {
        pbps_db::Conn::connect(pbps_db::Driver::Postgres, h.db.connection())
            .await
            .unwrap()
    };
    let mut holder = rt.block_on(async {
        let mut c = connect().await;
        c.execute("BEGIN ISOLATION LEVEL READ COMMITTED")
            .await
            .unwrap();
        c.execute("LOCK TABLE app.t IN ACCESS SHARE MODE")
            .await
            .unwrap();
        c
    });
    let checksum = plan_checksum(&plan);
    let child = Command::new(BIN)
        .arg("--project")
        .arg(&h.demo.dir)
        .args([
            "apply",
            "--env",
            "dev",
            "--plan",
            plan.to_str().unwrap(),
            "--checksum",
            &checksum,
        ])
        .env(PinnedHelper::URL, h.db.connection())
        .env(PinnedHelper::KEY, &key)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Wait until the apply's ALTER TABLE is queued behind the lock.
    let mut waited = 0;
    while scalar(
        h.db.connection(),
        "SELECT count(*) FROM pg_locks l JOIN pg_class c ON c.oid = l.relation \
         WHERE c.relname = 't' AND NOT l.granted AND l.mode = 'AccessExclusiveLock'",
    ) == 0
    {
        waited += 1;
        assert!(waited < 600, "the apply never reached its ALTER TABLE");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    h.replace_helper();
    rt.block_on(async { holder.execute("COMMIT").await.unwrap() });
    let refused = child.wait_with_output().unwrap();
    assert_ne!(code(&refused), 0, "{}", stdout(&refused));
    assert!(
        stderr(&refused).contains("after the statements, before recording"),
        "{}",
        stderr(&refused)
    );
    assert!(!h.label_exists(), "the transaction was rolled back");
}

#[test]
#[ignore = "needs live PostgreSQL"]
fn a_routine_replaced_during_a_staged_step_is_caught_after_its_checkpoint() {
    let h = PinnedHelper::new("pin_staged_step");
    let key = fingerprint_key(1);
    let plan = h.plan(true, &key);
    replace_helper_during_alter_table(&h);
    let refused = h.apply(&plan, true, &key);
    assert_ne!(code(&refused), 0, "{}", stdout(&refused));
    let err = stderr(&refused);
    assert!(
        err.contains("after statement 1 of 1 committed")
            && err.contains("checkpoint #")
            && err.contains("pinned routines changed in `ext`"),
        "{err}"
    );
    assert!(h.label_exists(), "the step committed");
    let latest = latest_snapshot(h.db.connection());
    assert_eq!(
        latest.staged.map(|s| s.completed),
        Some(1),
        "the checkpoint was recorded before the run stopped"
    );
    // A resume checks the same pins, so it refuses too.
    let resumed = h.run(
        &[
            "apply",
            "--env",
            "dev",
            "--plan",
            plan.to_str().unwrap(),
            "--checksum",
            &plan_checksum(&plan),
            "--staged",
            "--resume",
        ],
        Some(&key),
    );
    assert_ne!(code(&resumed), 0, "{}", stdout(&resumed));
}

#[test]
#[ignore = "needs live PostgreSQL"]
fn an_unchanged_pinned_routine_lets_both_apply_modes_through() {
    for staged in [false, true] {
        let h = PinnedHelper::new(if staged {
            "pin_same_staged"
        } else {
            "pin_same"
        });
        let key = fingerprint_key(1);
        let plan = h.plan(staged, &key);
        succeeds(h.apply(&plan, staged, &key));
        assert!(h.label_exists());
    }
}

#[test]
#[ignore = "needs live PostgreSQL"]
fn another_sessions_temporary_routine_is_not_pinned() {
    let db = OwnDatabase::new(&server(), "pin_temp");
    let role = format!("pbps_pin_temp_{}", std::process::id());
    let password = "pbps-pin-temp";
    let _ = try_on_server(db.connection(), &format!("DROP ROLE IF EXISTS {role}"));
    on_server(
        db.connection(),
        &format!(
            "CREATE ROLE {role} LOGIN PASSWORD '{password}' NOSUPERUSER; \
             GRANT TEMPORARY ON DATABASE \"{}\" TO {role}",
            db.name
        ),
    );
    let d = bootstrapped_demo(db.connection(), "pin_temp", ONE_COLUMN);
    d.table(TWO_COLUMNS);
    let as_role: String = db
        .connection()
        .split_whitespace()
        .filter(|w| !w.starts_with("user=") && !w.starts_with("password="))
        .chain([
            format!("user={role}").as_str(),
            format!("password={password}").as_str(),
        ])
        .collect::<Vec<_>>()
        .join(" ");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut session = rt.block_on(async {
        let mut c = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &as_role)
            .await
            .unwrap();
        c.execute("CREATE FUNCTION pg_temp.scratch() RETURNS integer LANGUAGE sql AS 'SELECT 1'")
            .await
            .unwrap();
        c
    });
    // Planned while the temporary routine exists, with no key configured:
    // it is another session's and is not pinned.
    let plan = connected_artifact(&d, db.connection(), false);
    rt.block_on(async { session.execute("SELECT 1").await.unwrap() });
    drop(session);
    // Its session ends, the engine drops it, and the apply still goes through.
    succeeds(approved_apply(&d, db.connection(), &plan, &[]));
    drop(d);
    drop(db);
    let _ = try_on_server(&server(), &format!("DROP ROLE IF EXISTS {role}"));
}

fn routine_exists(connection: &str, signature: &str) -> bool {
    scalar(
        connection,
        &format!("SELECT count(*) FROM pg_proc WHERE oid = to_regprocedure('{signature}')"),
    ) == 1
}

/// #322, DEC-322.1. A SECURITY DEFINER routine binds its unqualified names
/// when a caller runs it, through the caller's `search_path` unless it pins
/// its own. A routine a plan writes must pin one with `pg_temp` last,
/// PostgreSQL's documented safe form, or the apply rolls it back.
#[test]
#[ignore = "needs live PostgreSQL"]
fn a_definer_routine_a_plan_writes_must_pin_search_path_with_pg_temp_last() {
    let own = OwnDatabase::new(&server(), "definer_path");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "definer-path", ONE_COLUMN);
    for (name, clauses, accepted, why) in [
        ("bare", "SECURITY DEFINER", false, "sets no search_path"),
        (
            "no_temp",
            "SECURITY DEFINER SET search_path = app",
            false,
            "does not name pg_temp once and last",
        ),
        (
            "temp_first",
            "SECURITY DEFINER SET search_path = pg_temp, app",
            false,
            "does not name pg_temp once and last",
        ),
        // #1009: a quoted trailing space names another schema.
        (
            "temp_spaced",
            "SECURITY DEFINER SET search_path = app, \"pg_temp \"",
            false,
            "does not name pg_temp once and last",
        ),
        // #1012: an earlier pg_temp is searched first however the path ends.
        (
            "temp_twice",
            "SECURITY DEFINER SET search_path = pg_temp, app, pg_temp",
            false,
            "does not name pg_temp once and last",
        ),
        ("invoker", "", true, ""),
        (
            "safe",
            "SECURITY DEFINER SET search_path = app, pg_temp",
            true,
            "",
        ),
    ] {
        let file = d.dir.join(format!("schema/{name}.yml"));
        std::fs::write(
            &file,
            format!(
                "function: app.{name}()\ndefinition: () RETURNS integer LANGUAGE sql {clauses} \
                 AS $$ SELECT 1 $$\n"
            ),
        )
        .unwrap();
        let plan = connected_artifact(&d, connection, false);
        let applied = approved_apply(&d, connection, &plan, &[]);
        let signature = format!("app.{name}()");
        if accepted {
            succeeds(applied);
            assert!(routine_exists(connection, &signature), "{name}");
        } else {
            assert_ne!(code(&applied), 0, "{name}: {}", stdout(&applied));
            assert!(
                stderr(&applied).contains(&signature)
                    && stderr(&applied).contains(why)
                    && stderr(&applied).contains("rolled back"),
                "{name}: {}",
                stderr(&applied)
            );
            assert!(
                !routine_exists(connection, &signature),
                "{name} rolled back"
            );
            std::fs::remove_file(&file).unwrap();
            d.commit();
        }
    }
    // A definer routine the plan does not write is not its to judge: one
    // already there in the unsafe form stays, and an unrelated plan applies.
    on_server(
        connection,
        "CREATE FUNCTION app.untouched() RETURNS integer LANGUAGE sql SECURITY DEFINER \
         AS $$ SELECT 1 $$",
    );
    d.table(TWO_COLUMNS);
    let plan = connected_artifact(&d, connection, false);
    succeeds(approved_apply(&d, connection, &plan, &[]));
}

#[test]
#[ignore = "needs live PostgreSQL"]
fn a_staged_or_bootstrapped_definer_routine_without_a_pinned_path_is_refused_before_it_commits() {
    let own = OwnDatabase::new(&server(), "definer_staged");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "definer-staged", ONE_COLUMN);
    std::fs::write(
        d.dir.join("schema/staged.yml"),
        // Open to PUBLIC, because a staged run cannot close a routine it
        // creates in the same step (DECISIONS 517).
        "function: app.staged()\npublic_execute: true\ndefinition: () RETURNS integer LANGUAGE sql \
         SECURITY DEFINER AS $$ SELECT 1 $$\n",
    )
    .unwrap();
    let plan = connected_artifact(&d, connection, true);
    let refused = approved_apply(&d, connection, &plan, &["--staged"]);
    assert_ne!(code(&refused), 0, "{}", stdout(&refused));
    assert!(
        stderr(&refused).contains("sets no search_path"),
        "{}",
        stderr(&refused)
    );
    assert!(
        !routine_exists(connection, "app.staged()"),
        "the step's transaction rolled the routine back"
    );

    // Bootstrap builds routines too.
    let fresh = OwnDatabase::new(&server(), "definer_bootstrap");
    on_server(fresh.connection(), "CREATE SCHEMA app");
    let b = Demo::new("definer-bootstrap");
    b.table(ONE_COLUMN);
    std::fs::write(
        b.dir.join("schema/boot.yml"),
        "function: app.boot()\ndefinition: () RETURNS integer LANGUAGE sql SECURITY DEFINER \
         AS $$ SELECT 1 $$\n",
    )
    .unwrap();
    succeeds(b.run(&["plan"]));
    b.commit();
    let refused = b.run(&["bootstrap", "--db", fresh.connection()]);
    assert_ne!(code(&refused), 0, "{}", stdout(&refused));
    assert!(
        stderr(&refused).contains("sets no search_path"),
        "{}",
        stderr(&refused)
    );
    assert!(!routine_exists(fresh.connection(), "app.boot()"));
    assert_eq!(
        scalar(
            fresh.connection(),
            "SELECT count(*) FROM information_schema.tables WHERE table_schema = 'app'"
        ),
        0,
        "the bootstrap left the database empty"
    );
}

/// #700: a revoke the engine did not carry out. A `REVOKE` that reports
/// success and changes nothing (a third grantor's entry, the wide statement
/// before DECISIONS 518, or a trigger that grants the permission straight
/// back) must not be recorded as converged. The closing read holds the role
/// to what the plan leaves it holding (DECISIONS 160). Measured here with an
/// event trigger that re-grants on `REVOKE`: the apply exits non-zero, names
/// the permission that survived, rolls back and records no apply entry. The
/// same plan without the trigger lands and is recorded.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_revoke_that_does_not_land_is_refused_at_the_closing_read() {
    for undone in [true, false] {
        let slug = if undone {
            "revoke-undone"
        } else {
            "revoke-lands"
        };
        let server = server();
        let reader = format!("pbps_rv_{}_{}", std::process::id(), u8::from(undone));
        let _roles = ClusterRoles {
            server: server.clone(),
            names: vec![reader.clone()],
        };
        on_server(&server, &format!("CREATE ROLE {reader} NOSUPERUSER"));
        let own = OwnDatabase::new(&server, slug);
        let connection = own.connection();
        on_server(connection, "CREATE SCHEMA app");
        let d = Demo::new(slug);
        d.table(ONE_COLUMN);
        let file = d.dir.join("schema/reader.yml");
        let declaration = |permissions: &str| {
            format!("role: {reader}\ngrants:\n  schema::app: [usage]\n  app.t: [{permissions}]\n")
        };
        std::fs::write(&file, declaration("select, insert")).unwrap();
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", connection]));
        if undone {
            on_server(
                connection,
                &format!(
                    "CREATE FUNCTION public.regrant() RETURNS event_trigger LANGUAGE plpgsql \
                     AS $$ BEGIN EXECUTE 'GRANT INSERT ON app.t TO {reader}'; END $$; \
                     CREATE EVENT TRIGGER regrant ON ddl_command_end WHEN TAG IN ('REVOKE') \
                     EXECUTE FUNCTION public.regrant()"
                ),
            );
        }
        std::fs::write(&file, declaration("select")).unwrap();
        succeeds(d.run(&["plan"]));
        d.commit();
        let plan = d.dir.join("plan.json");
        succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
        let recorded = || {
            scalar(
                connection,
                "SELECT count(*) FROM public.__pbps_state WHERE kind <> 'failed'",
            )
        };
        let inserts = || {
            scalar(
                connection,
                &format!(
                    "SELECT CASE WHEN has_table_privilege('{reader}', 'app.t', 'INSERT') \
                     THEN 1 ELSE 0 END::bigint"
                ),
            )
        };
        let before = recorded();
        let applied = approved_apply(&d, connection, &plan, &["--allow", "revoke"]);
        if undone {
            assert_eq!(
                code(&applied),
                1,
                "{}{}",
                stdout(&applied),
                stderr(&applied)
            );
            assert!(
                stderr(&applied).contains(&format!("role {reader} does not hold on app.t"))
                    && stderr(&applied).contains("it still holds insert"),
                "{}",
                stderr(&applied)
            );
            assert_eq!(recorded(), before, "nothing was recorded as applied");
        } else {
            succeeds(applied);
            assert_eq!(inserts(), 0, "the revoke landed");
            assert_eq!(recorded(), before + 1, "and was recorded");
        }
    }
}

/// #428. The trigger check and the row statement are two server commands. A
/// membership granted and a trigger function replaced *between* them run the
/// replacement inside the approved write. The routine pins (DEC-319.1) catch
/// it before the write can commit: a transactional apply re-reads them before
/// recording, and a staged row write re-reads them inside its own transaction,
/// before its commit. Either way the replacement's row is rolled back and no
/// apply is recorded.
///
/// The window is held open by an approved statement-level trigger that waits
/// on an advisory lock. It fires after the check and before the row trigger,
/// so the replacement lands exactly there.
#[test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
fn a_trigger_function_replaced_between_the_check_and_the_row_write_is_rolled_back() {
    let admin = server();
    let declared = "table: app.t\ncolumns:\n  code: {type: varchar(20), nullable: false}\n  \
                    label: {type: text, nullable: false}\nprimary_key: {name: pk_t, columns: [code]}\n";
    // The deferred case holds the commit rather than the row statement: its
    // triggers run when the transaction settles, after the row write.
    for (staged, deferred) in [(false, false), (true, false), (true, true)] {
        let slug = match (staged, deferred) {
            (_, true) => "window428_deferred",
            (true, false) => "window428_staged",
            (false, false) => "window428",
        };
        let topology = inherited_owner_topology(&admin, slug);
        let connection = topology.connection().to_owned();
        let deployment = topology.deployment.clone();
        let d = Demo::new(slug);
        std::fs::write(
            d.dir.join("pbps.yml"),
            "dialect: postgres\nunmanaged: ignore\n",
        )
        .unwrap();
        d.keyed(&deployment);
        d.table(declared);
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", &deployment]));
        let triggers = if deferred {
            "CREATE CONSTRAINT TRIGGER a_hold AFTER INSERT ON app.t \
               DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.hold428(); \
             CREATE CONSTRAINT TRIGGER audit AFTER INSERT ON app.t \
               DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION hook.audit()"
        } else {
            "CREATE TRIGGER a_hold BEFORE INSERT ON app.t FOR EACH STATEMENT \
               EXECUTE FUNCTION public.hold428(); \
             CREATE TRIGGER audit BEFORE INSERT ON app.t FOR EACH ROW \
               EXECUTE FUNCTION hook.audit()"
        };
        on_server(
            &deployment,
            &format!(
                "CREATE FUNCTION public.hold428() RETURNS trigger LANGUAGE plpgsql AS \
                 $$BEGIN PERFORM pg_advisory_xact_lock(428); RETURN NULL; END$$; {triggers}"
            ),
        );
        // Both triggers adopted in the engine's own spelling; the functions
        // stay unmanaged, so the pins are what watch them.
        let pulled = Demo::new(&format!("{slug}-pull"));
        succeeds(pulled.run(&["pull", "--db", &deployment]));
        let mut pending = vec![pulled.dir.join("schema")];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                let body = std::fs::read_to_string(&path).unwrap();
                if body.lines().any(|line| line.starts_with("trigger:")) {
                    std::fs::write(d.dir.join("schema").join(path.file_name().unwrap()), body)
                        .unwrap();
                }
            }
        }
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            &deployment,
            "--reason",
            "adopt the triggers",
        ]));
        d.table(&format!(
            "{declared}data:\n  mode: exact\n  rows:\n    new: {{label: New}}\n"
        ));
        let plan = connected_artifact(&d, &deployment, staged);
        let applies = scalar(
            &connection,
            "SELECT count(*) FROM public.__pbps_state WHERE kind IN ('apply', 'staged')",
        );

        // Hold the window, start the apply, and wait until it sits in it.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut holder = rt.block_on(async {
            let mut c = pbps_db::Conn::connect(pbps_db::Driver::Postgres, &connection)
                .await
                .unwrap();
            c.execute("SELECT pg_advisory_lock(428)").await.unwrap();
            c
        });
        let checksum = plan_checksum(&plan);
        let mut args = vec![
            "apply",
            "--env",
            KEYED_ENV,
            "--plan",
            plan.to_str().unwrap(),
            "--checksum",
            &checksum,
        ];
        if staged {
            args.push("--staged");
        }
        let child = Command::new(BIN)
            .arg("--project")
            .arg(&d.dir)
            .args(&args)
            .env(KEYED_URL, &deployment)
            .env(KEYED_KEY, fingerprint_key(9))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut waited = 0;
        while scalar(
            &connection,
            "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND objid = 428 \
             AND NOT granted",
        ) == 0
        {
            waited += 1;
            assert!(
                waited < 3000,
                "{slug}: the apply never reached the row write"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // Inside the window: the membership that lets the inheritor replace
        // the function, and the replacement.
        on_server(
            &connection,
            &format!(
                "GRANT {} TO {} WITH INHERIT TRUE, SET FALSE",
                topology.owner, topology.inheritor
            ),
        );
        on_server(
            &as_role(&connection, &topology.inheritor),
            "CREATE OR REPLACE FUNCTION hook.audit() RETURNS trigger LANGUAGE plpgsql AS \
             $$BEGIN INSERT INTO hook.calls VALUES (428); RETURN NEW; END$$",
        );
        rt.block_on(async {
            holder
                .execute("SELECT pg_advisory_unlock(428)")
                .await
                .unwrap()
        });
        let refused = child.wait_with_output().unwrap();
        assert_ne!(code(&refused), 0, "{slug}: {}", stdout(&refused));
        assert!(
            stderr(&refused).contains("pinned routines changed in `hook`"),
            "{slug}: {}",
            stderr(&refused)
        );
        assert_eq!(
            scalar(
                &connection,
                "SELECT count(*) FROM hook.calls WHERE value = 428"
            ),
            0,
            "{slug}: the replacement ran, and its write must not survive"
        );
        assert_eq!(
            scalar(&connection, "SELECT count(*) FROM app.t"),
            0,
            "{slug}: the approved row did not commit either"
        );
        assert_eq!(
            scalar(
                &connection,
                "SELECT count(*) FROM public.__pbps_state WHERE kind IN ('apply', 'staged')"
            ),
            applies,
            "{slug}: nothing was recorded as applied"
        );
    }
}

/// A view whose only change is the spacing after an escaped quote in a
/// continued `E'…'` piece: the `E` carries to the continuation, so that
/// spacing is the literal's, and the plan has to rebuild the view (#539).
/// Normalized as a plain string, the two definitions compared equal, and the
/// plan recorded the new one over the old view without touching it.
#[test]
#[ignore = "needs live PostgreSQL"]
fn a_continued_escape_strings_spacing_is_a_change_to_the_view() {
    let own = OwnDatabase::new(&server(), "continued539");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "continued539", ONE_COLUMN);
    let view = |spacing: &str| {
        std::fs::write(
            d.dir.join("schema/app.v.view.yml"),
            format!("view: app.v\ndefinition: |-\n  SELECT E'a'\n  'b\\'c{spacing}d' AS x\n"),
        )
        .unwrap();
    };
    let apply = |d: &Demo| {
        let plan = d.dir.join("plan.json");
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
        let checksum = plan_checksum(&plan);
        succeeds(d.run(&[
            "apply",
            "--db",
            connection,
            "--plan",
            plan.to_str().unwrap(),
            "--checksum",
            &checksum,
        ]));
    };
    view("  ");
    apply(&d);
    assert_eq!(scalar(connection, "SELECT length(x)::int8 FROM app.v"), 7);

    view(" ");
    apply(&d);
    assert_eq!(
        scalar(connection, "SELECT length(x)::int8 FROM app.v"),
        6,
        "the view still returns the old literal"
    );
    succeeds(d.run(&["verify", "--db", connection]));
}

/// A routine whose parameter default is an `E'…'` continued across a line
/// break, with an escaped quote in the continuation. The parameter scan has
/// to read the quote as data, or it finds no end to the list and the plan is
/// refused as having none (#539). Measured, the engine creates it and stores
/// the default as `'xy''z'::text`.
#[test]
#[ignore = "needs live PostgreSQL"]
fn a_routine_with_a_continued_escape_string_default_plans_and_applies() {
    let own = OwnDatabase::new(&server(), "continueddefault539");
    let connection = own.connection();
    let d = bootstrapped_demo(connection, "continueddefault539", ONE_COLUMN);
    std::fs::write(
        d.dir.join("schema/app.f.function.yml"),
        "function: app.f(text, integer)\ndefinition: |-\n  (a text DEFAULT E'x'\n  'y\\'z', b integer DEFAULT 1) RETURNS text LANGUAGE sql AS $$ SELECT a $$\n",
    )
    .unwrap();
    succeeds(d.run(&["plan"]));
    d.commit();
    let plan = d.dir.join("plan.json");
    succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
    let checksum = plan_checksum(&plan);
    succeeds(d.run(&[
        "apply",
        "--db",
        connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &checksum,
    ]));
    assert_eq!(
        scalar(connection, "SELECT length(app.f())::int8"),
        4,
        "the default is the one constant `xy'z`"
    );
    succeeds(d.run(&["verify", "--db", connection]));
}
/// Two revisions deployed at once, where the second renames a table into the
/// name the first dropped (#536). The drop ran in its own class after every
/// rename, and the engine refused the rename while the doomed table still held
/// the name (`42P07`). The doomed table cannot go while `child`'s key still
/// names it, and its own key to `parent` is dropped first as it always is, so
/// both keys move ahead with it.
#[test]
#[ignore = "needs live PostgreSQL"]
fn a_table_drop_and_a_later_rename_that_reuses_its_name_apply_together() {
    let slug = "reusedtable536";
    let own = OwnDatabase::new(&server(), slug);
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new(slug);
    let declare = |name: &str, body: Option<String>| {
        let path = d.dir.join(format!("schema/app.{name}.yml"));
        match body {
            Some(body) => std::fs::write(path, format!("table: app.{name}\n{body}")).unwrap(),
            None => std::fs::remove_file(path).unwrap(),
        }
    };
    let keyed = |pk: &str, rest: &str| {
        format!(
            "columns:\n  id: {{type: bigint, nullable: false}}\n{rest}\
             primary_key: {{name: {pk}, columns: [id]}}\n"
        )
    };
    let target_key = "foreign_keys:\n  fk_target_parent:\n    columns: [parent_id]\n    \
                      references: app.parent(id)\n";
    let keeper_key = |to: &str| {
        format!(
            "foreign_keys:\n  fk_keeper_old:\n    columns: [old_id]\n    \
             references: {to}(id)\n"
        )
    };
    let child_key = "foreign_keys:\n  fk_child_target:\n    columns: [target_id]\n    \
                     references: app.target(id)\n";

    // v1, and the only revision this database ever gets deployed.
    declare("parent", Some(keyed("pk_parent", "")));
    declare(
        "target",
        Some(keyed("pk_target", "  parent_id: {type: bigint}\n") + target_key),
    );
    declare(
        "child",
        Some(keyed("pk_child", "  target_id: {type: bigint}\n") + child_key),
    );
    // `other` goes before `target` does, in a revision of its own.
    declare(
        "other",
        Some(
            keyed("pk_other", "  target_id: {type: bigint}\n")
                + &child_key.replace("fk_child_target", "fk_other_target"),
        ),
    );
    // `keeper` is untouched: its key follows `old` through the rename.
    declare(
        "keeper",
        Some(keyed("pk_keeper", "  old_id: {type: bigint}\n") + &keeper_key("app.old")),
    );
    declare("old", Some(keyed("pk_old", "  label: {type: text}\n")));
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    let o = d.run(&["bootstrap", "--db", connection]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));

    // v1b: `other` goes, and its key to `target` with it. Never deployed.
    declare("other", None);
    let o = d.run(&["drop-table", "app.other", "--reason", "no longer used"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    // v2: `target` goes, and `child`'s key to it. Committed, never deployed.
    declare("target", None);
    declare(
        "child",
        Some(keyed("pk_child", "  target_id: {type: bigint}\n")),
    );
    let o = d.run(&["drop-table", "app.target", "--reason", "no longer used"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    // v3: `old` takes the name `target` just vacated.
    declare("old", None);
    declare(
        "keeper",
        Some(keyed("pk_keeper", "  old_id: {type: bigint}\n") + &keeper_key("app.target")),
    );
    declare("target", Some(keyed("pk_old", "  label: {type: text}\n")));
    // And `child` points a key of the same name at the new occupant.
    declare(
        "child",
        Some(keyed("pk_child", "  target_id: {type: bigint}\n") + child_key),
    );
    let o = d.run(&["rename-table", "app.old", "app.target"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();

    // The database is still at v1, so this one plan carries both revisions.
    let plan = d.dir.join("plan.json");
    let o = d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let checksum = plan_checksum(&plan);
    let args = [
        "apply",
        "--db",
        connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &checksum,
        "--allow",
        "rename,destructive,constraint",
    ];
    let o = d.run(&args);
    assert_eq!(
        code(&o),
        0,
        "the drop and the rename must apply together: {}{}",
        stdout(&o),
        stderr(&o)
    );

    // The database really is in the declared shape, not merely un-errored.
    let o = d.run(&["verify", "--db", connection]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    let o = d.run(&["plan", "--db", connection]);
    assert!(stdout(&o).contains("No changes"), "{}", stdout(&o));
}

/// The same reuse, where the renamed table's key and the doomed table's key
/// share a name, which a key name may on this engine (it is the table's own),
/// and the renamed table's key pointed at the doomed table. The two drops are
/// one address apart only in time: the doomed table's runs under its name,
/// the renamed table's under its source, and the key comes back pointing at
/// the table's new self (DEC-536.1).
#[test]
#[ignore = "needs live PostgreSQL"]
fn a_reused_tables_key_and_the_renamed_tables_key_of_one_name_both_go() {
    let own = OwnDatabase::new(&server(), "reusedfk536");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("reusedfk536");
    let declare = |name: &str, body: Option<String>| {
        let path = d.dir.join(format!("schema/app.{name}.yml"));
        match body {
            Some(body) => std::fs::write(path, format!("table: app.{name}\n{body}")).unwrap(),
            None => std::fs::remove_file(path).unwrap(),
        }
    };
    let keyed = |pk: &str, rest: &str| {
        format!(
            "columns:\n  id: {{type: bigint, nullable: false}}\n{rest}\
             primary_key: {{name: {pk}, columns: [id]}}\n"
        )
    };
    let key = |column: &str, to: &str| {
        format!("foreign_keys:\n  fk:\n    columns: [{column}]\n    references: {to}(id)\n")
    };

    declare("parent", Some(keyed("pk_parent", "")));
    declare(
        "target",
        Some(keyed("pk_target", "  parent_id: {type: bigint}\n") + &key("parent_id", "app.parent")),
    );
    declare(
        "old",
        Some(keyed("pk_old", "  target_id: {type: bigint}\n") + &key("target_id", "app.target")),
    );
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));

    declare("target", None);
    declare(
        "old",
        Some(keyed("pk_old", "  target_id: {type: bigint}\n")),
    );
    succeeds(d.run(&["drop-table", "app.target", "--reason", "no longer used"]));
    succeeds(d.run(&["plan"]));
    d.commit();

    declare("old", None);
    declare(
        "target",
        Some(keyed("pk_old", "  target_id: {type: bigint}\n") + &key("target_id", "app.target")),
    );
    succeeds(d.run(&["rename-table", "app.old", "app.target"]));
    succeeds(d.run(&["plan"]));
    d.commit();

    let plan = d.dir.join("plan.json");
    succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
    let checksum = plan_checksum(&plan);
    succeeds(d.run(&[
        "apply",
        "--db",
        connection,
        "--plan",
        plan.to_str().unwrap(),
        "--checksum",
        &checksum,
        "--allow",
        "rename,destructive,constraint",
    ]));
    succeeds(d.run(&["verify", "--db", connection]));
    let o = d.run(&["plan", "--db", connection]);
    assert!(stdout(&o).contains("No changes"), "{}", stdout(&o));
    assert_eq!(
        scalar(
            connection,
            "SELECT count(*) FROM pg_constraint WHERE conname = 'fk' \
             AND conrelid = 'app.target'::regclass AND confrelid = 'app.target'::regclass"
        ),
        1,
        "the key points at the table's new self"
    );
}
