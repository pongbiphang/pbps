//! `plan --assert` through the binary on both engines (#2401 PR B, DEC-2401.4).
//!
//! The engines' own boundary (identity, a separate login, effective
//! permissions, the read-only transaction) is qualified natively by PR A. These
//! tests pin what only the CLI decides: which outcomes write a plan, what the
//! plan, `explain` and the ledger record, and that nothing runs at apply.
use super::{Demo, OwnDatabase, code, on_server, stderr, stdout, try_on_server};
use pbps_model::{SavedPlan, StateSnapshot};
use sha2::Digest as _;
use std::path::PathBuf;
use std::process::Output;

const VAR: &str = "PBPS_ASSERTION_CONN";
const PASSWORD: &str = "OnlyAssert!2401";
const FINDING: i32 = 2;

/// Logins and system-database users are instance-wide on SQL Server. The live
/// script runs one test at a time, and this keeps even a parallel run apart.
static INSTANCE: std::sync::Mutex<()> = std::sync::Mutex::new(());

const BASE: &str = "id: {type: int, nullable: false}\n";
fn table(pg: bool, extra: bool) -> String {
    format!(
        "table: {}.t\ncolumns:\n  {BASE}{}primary_key: {{name: pk_t, columns: [id]}}\n",
        if pg { "app" } else { "dbo" },
        if extra { "  extra: {type: int}\n" } else { "" }
    )
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn driver(pg: bool) -> pbps_db::Driver {
    if pg {
        pbps_db::Driver::Postgres
    } else {
        pbps_db::Driver::Mssql
    }
}

/// A count read through the same driver, for "the target did not change".
fn count(connection: &str, pg: bool, table: &str) -> i64 {
    rt().block_on(async {
        let mut c = pbps_db::Conn::connect(driver(pg), connection)
            .await
            .unwrap();
        let sql = format!(
            "SELECT {}(*) AS n FROM {table}",
            if pg { "count" } else { "COUNT_BIG" }
        );
        c.query(&sql).await.unwrap()[0]
            .try_get::<i64>("n")
            .unwrap()
            .unwrap()
    })
}

fn latest_snapshot(connection: &str, pg: bool) -> StateSnapshot {
    rt().block_on(async {
        let mut c = pbps_db::Conn::connect(driver(pg), connection)
            .await
            .unwrap();
        let sql = if pg {
            "SELECT state_json FROM public.__pbps_state ORDER BY id DESC LIMIT 1"
        } else {
            "SELECT TOP 1 state_json FROM dbo.__pbps_state ORDER BY id DESC"
        };
        let rows = c.query(sql).await.unwrap();
        StateSnapshot::from_json(rows[0].try_get::<&str>("state_json").unwrap().unwrap()).unwrap()
    })
}

/// One target database, a project bootstrapped against it, and a separate
/// login that can only read.
struct Case {
    pg: bool,
    server: String,
    login: String,
    assertion: String,
    d: Demo,
    own: OwnDatabase,
    _instance: std::sync::MutexGuard<'static, ()>,
}

impl Case {
    fn new(server: &str, pg: bool, slug: &str) -> Self {
        let instance = INSTANCE.lock().unwrap_or_else(|e| e.into_inner());
        let slug = format!("pa{slug}");
        let own = OwnDatabase::new(server, &slug);
        let connection = own.connection().to_owned();
        let t = if pg { "app.t" } else { "dbo.t" };
        if pg {
            on_server(&connection, "CREATE SCHEMA app");
        }
        let d = Demo::new(&slug);
        d.table(&table(pg, false));
        succeeds(d.run(&["plan"]));
        d.commit();
        let out = d.run(&["bootstrap", "--db", &connection]);
        assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
        on_server(&connection, &format!("INSERT INTO {t}(id) VALUES (1)"));
        let login = format!("{slug}_{}", std::process::id());
        let assertion = Self::create_login(server, &connection, &login, pg);
        // The edit the plan will carry; the declarations change, the target not.
        d.table(&table(pg, true));
        succeeds(d.run(&["plan"]));
        d.commit();
        Self {
            pg,
            server: server.to_owned(),
            login,
            assertion,
            d,
            own,
            _instance: instance,
        }
    }

    fn table(&self) -> &'static str {
        if self.pg { "app.t" } else { "dbo.t" }
    }

    fn create_login(server: &str, db: &str, login: &str, pg: bool) -> String {
        if pg {
            on_server(
                db,
                &format!(
                    "CREATE ROLE \"{login}\" LOGIN PASSWORD '{PASSWORD}'; \
                     GRANT USAGE ON SCHEMA app TO \"{login}\"; \
                     GRANT SELECT ON app.t TO \"{login}\""
                ),
            );
            let kept: Vec<_> = db
                .split_whitespace()
                .filter(|w| !w.starts_with("user=") && !w.starts_with("password="))
                .collect();
            return format!("{} user={login} password={PASSWORD}", kept.join(" "));
        }
        // The same least-privilege shape PR A qualifies: a mapped read-only
        // user in the target and in every accessible system database (guest
        // would otherwise own permanent schemas), VIEW DEFINITION for the
        // metadata the admission needs, and EXECUTE denied on system procedures.
        on_server(
            server,
            &format!("CREATE LOGIN [{login}] WITH PASSWORD='{PASSWORD}',CHECK_POLICY=OFF;"),
        );
        on_server(
            db,
            &format!(
                "CREATE USER [{login}] FOR LOGIN [{login}]; GRANT VIEW DEFINITION TO [{login}]; \
                 GRANT SELECT ON dbo.t TO [{login}]"
            ),
        );
        for system in ["master", "msdb", "tempdb"] {
            on_server(
                server,
                &format!(
                    "USE [{system}]; CREATE USER [{login}] FOR LOGIN [{login}]; \
                     GRANT VIEW DEFINITION TO [{login}]; DENY EXECUTE TO [{login}]"
                ),
            );
        }
        on_server(
            server,
            &format!(
                "USE master; DECLARE @s nvarchar(max)=N''; \
                 SELECT @s+=N'DENY EXECUTE ON OBJECT::'+QUOTENAME(SCHEMA_NAME(schema_id))+N'.'+QUOTENAME(name)+N' TO [{login}]; ' \
                 FROM sys.system_objects WHERE type IN ('P','PC','X','RF'); EXEC(@s)"
            ),
        );
        let parts: Vec<_> = db
            .split(';')
            .filter(|part| {
                let key = part.split('=').next().unwrap().trim().to_ascii_lowercase();
                !["user id", "userid", "uid", "password", "pwd"].contains(&key.as_str())
            })
            .collect();
        format!("{};User Id={login};Password={PASSWORD}", parts.join(";"))
    }

    fn connection(&self) -> &str {
        self.own.connection()
    }

    fn file(&self, name: &str, sql: &str) -> PathBuf {
        let path = self.d.dir.join("checks").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, sql).unwrap();
        path
    }

    /// `plan --db` with the given assertion files and the assertion login.
    fn plan(&self, files: &[&PathBuf], out: &std::path::Path, extra: &[&str]) -> Output {
        self.plan_with(files, out, extra, Some(&self.assertion))
    }

    fn plan_with(
        &self,
        files: &[&PathBuf],
        out: &std::path::Path,
        extra: &[&str],
        conn: Option<&str>,
    ) -> Output {
        let mut args = vec![
            "plan",
            "--db",
            self.connection(),
            "--out",
            out.to_str().unwrap(),
        ];
        for file in files {
            args.push("--assert");
            args.push(file.to_str().unwrap());
        }
        args.extend_from_slice(extra);
        // Always set, even blank: a stray variable in the runner's own
        // environment must not decide a test.
        self.d.run_with_env(&args, &[(VAR, conn.unwrap_or(""))])
    }

    fn out(&self, name: &str) -> PathBuf {
        self.d.dir.join(name)
    }

    fn read_plan(path: &std::path::Path) -> SavedPlan {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    fn violating_row(&self) {
        on_server(
            self.connection(),
            &format!("INSERT INTO {}(id) VALUES (-1)", self.table()),
        );
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        // Tolerant, as the databases' own guards are: the failure being
        // reported is the test's, not the clean-up's.
        if self.pg {
            let _ = try_on_server(
                self.own.connection(),
                &format!("DROP OWNED BY \"{}\"", self.login),
            );
            let _ = try_on_server(
                &self.server,
                &format!("DROP ROLE IF EXISTS \"{}\"", self.login),
            );
        } else {
            for system in ["master", "msdb", "tempdb"] {
                let _ = try_on_server(
                    &self.server,
                    &format!("USE [{system}]; DROP USER IF EXISTS [{}]", self.login),
                );
            }
            let _ = try_on_server(&self.server, &format!("DROP LOGIN [{}]", self.login));
        }
    }
}

fn sha(sql: &str) -> String {
    format!("{:x}", sha2::Sha256::digest(sql.as_bytes()))
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(out))
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}\n{}", stdout(out), stderr(out)))
}

fn ids(envelope: &serde_json::Value) -> Vec<String> {
    envelope["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap().to_owned())
        .collect()
}

fn succeeds(out: Output) -> Output {
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    out
}

/// The refusal a user reads: exit 2 and nothing written at `out`.
fn refused(out: &Output, out_path: &std::path::Path) -> String {
    assert_eq!(code(out), FINDING, "{}{}", stdout(out), stderr(out));
    assert!(!out_path.exists(), "a refused plan must not be written");
    format!("{}{}", stdout(out), stderr(out))
}

pub(super) fn passing_assertions_are_recorded_explained_and_kept_in_the_ledger(
    server: &str,
    pg: bool,
) {
    let c = Case::new(server, pg, "pass");
    let t = c.table();
    let first_sql = format!("SELECT id FROM {t} WHERE id < 0");
    let first = c.file("negative_ids.sql", &first_sql);
    let second = c.file("anything.sql", "SELECT 1 WHERE 1=0");
    let planned = c.out("plan.json");
    let out = succeeds(c.plan(&[&first, &second], &planned, &[]));
    assert!(
        stdout(&out).contains("Target assertions: 2 passed"),
        "{}",
        stdout(&out)
    );
    let plan = Case::read_plan(&planned);
    let recorded: Vec<_> = plan
        .target_assertions
        .iter()
        .map(|a| (a.path.raw(), a.sha256.as_str(), a.outcome.as_str()))
        .collect();
    assert_eq!(
        recorded,
        [
            ("checks/negative_ids.sql", sha(&first_sql).as_str(), "pass"),
            (
                "checks/anything.sql",
                sha("SELECT 1 WHERE 1=0").as_str(),
                "pass"
            ),
        ]
    );

    // Covered by the checksum: the same plan without them is another plan.
    let bare_path = c.out("bare.json");
    succeeds(c.plan(&[], &bare_path, &[]));
    let bare = Case::read_plan(&bare_path);
    assert!(bare.target_assertions.is_empty());
    assert!(
        !std::fs::read_to_string(&bare_path)
            .unwrap()
            .contains("target_assertions")
    );
    assert_ne!(plan.wire_fingerprint(), bare.wire_fingerprint());

    // The approver sees them, in text and in JSON.
    let shown = succeeds(c.d.run(&["explain", "--plan", planned.to_str().unwrap()]));
    let text = stdout(&shown);
    assert!(text.contains("Target assertions"), "{text}");
    assert!(text.contains("checks/negative_ids.sql"), "{text}");
    assert!(text.contains(&sha(&first_sql)), "{text}");
    let shown = succeeds(c.d.run(&[
        "explain",
        "--plan",
        planned.to_str().unwrap(),
        "--format",
        "json",
    ]));
    let envelope = json(&shown);
    assert_eq!(
        envelope["data"]["target_assertions"][0]["sha256"],
        sha(&first_sql)
    );
    let absent = json(&succeeds(c.d.run(&[
        "explain",
        "--plan",
        bare_path.to_str().unwrap(),
        "--format",
        "json",
    ])));
    assert!(absent["data"].get("target_assertions").is_none());

    // Editing the file changes the record, so the old plan is distinguishable.
    let edited_sql = format!("{first_sql} -- reviewed again");
    c.file("negative_ids.sql", &edited_sql);
    let replanned = c.out("replanned.json");
    let out = succeeds(c.plan(&[&first, &second], &replanned, &["--format", "json"]));
    assert_eq!(
        json(&out)["data"]["target_assertions"][0]["sha256"],
        sha(&edited_sql)
    );
    let newer = Case::read_plan(&replanned);
    assert_ne!(newer.target_assertions, plan.target_assertions);
    assert_ne!(newer.wire_fingerprint(), plan.wire_fingerprint());

    // Nothing user-supplied runs at apply: a violating row added after `plan`
    // is not caught there (docs: the layer-3 guarantee table).
    c.violating_row();
    assert_eq!(count(c.connection(), pg, &format!("{t} WHERE id < 0")), 1);
    let checksum = plan.wire_fingerprint();
    succeeds(c.d.run(&[
        "apply",
        "--db",
        c.connection(),
        "--plan",
        planned.to_str().unwrap(),
        "--checksum",
        &checksum,
    ]));
    let ledger = latest_snapshot(c.connection(), pg);
    assert_eq!(ledger.target_assertions, plan.target_assertions);
    assert_eq!(ledger.plan_checksum.as_deref(), Some(checksum.as_str()));
    // And planning again does catch it: the observation is made at plan time.
    let again = c.out("again.json");
    let refusal = refused(&c.plan(&[&first], &again, &[]), &again);
    assert!(refusal.contains("checks/negative_ids.sql"), "{refusal}");
}

pub(super) fn a_violation_writes_no_plan_and_names_the_rows(server: &str, pg: bool) {
    let c = Case::new(server, pg, "viol");
    let t = c.table();
    c.violating_row();
    let bad = c.file("bad.sql", &format!("SELECT id FROM {t} WHERE id < 0"));
    let good = c.file("good.sql", "SELECT 1 WHERE 1=0");
    let never = c.file("never.sql", "SELECT 1/0");
    let planned = c.out("plan.json");
    // A previous plan at the same path is neither replaced nor removed.
    std::fs::write(&planned, "previous bytes").unwrap();
    let out = c.plan(&[&good, &bad, &never], &planned, &[]);
    assert_eq!(code(&out), FINDING, "{}{}", stdout(&out), stderr(&out));
    assert_eq!(std::fs::read_to_string(&planned).unwrap(), "previous bytes");
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(text.contains("checks/bad.sql"), "{text}");
    assert!(text.contains("returned 1 row(s)"), "{text}");
    assert!(text.contains("id=-1"), "{text}");
    assert!(
        !text.contains("never.sql"),
        "stopped at the first failure: {text}"
    );

    std::fs::remove_file(&planned).unwrap();
    let out = c.plan(&[&good, &bad, &never], &planned, &["--format", "json"]);
    assert_eq!(code(&out), FINDING, "{}", stderr(&out));
    let envelope = json(&out);
    assert_eq!(envelope["result"], "findings");
    assert_eq!(ids(&envelope), ["plan.assertion.rows"]);
    assert_eq!(
        envelope["findings"][0]["location"]["file"],
        "checks/bad.sql"
    );
    assert!(!planned.exists());
}

pub(super) fn errors_and_write_attempts_are_errors_not_passes(server: &str, pg: bool) {
    let c = Case::new(server, pg, "err");
    let t = c.table();
    let before = count(c.connection(), pg, t);
    let planned = c.out("plan.json");
    let mut attempts: Vec<(&str, String)> = vec![
        ("error.sql", "SELECT 1/0".to_owned()),
        (
            "write.sql",
            if pg {
                format!("INSERT INTO {t}(id) VALUES (900) RETURNING id")
            } else {
                format!("INSERT INTO {t}(id) VALUES (900); SELECT 1 WHERE 1=0")
            },
        ),
        // No result set, or none with a column, is invalid rather than a pass.
        (
            "no_columns.sql",
            if pg { "SELECT" } else { "DECLARE @a int = 1" }.to_owned(),
        ),
    ];
    if pg {
        // Sequential transaction control: the engine refuses more than one
        // statement per call, and the runner owns the transaction anyway.
        attempts.push((
            "commit_then_write.sql",
            format!("COMMIT; INSERT INTO {t}(id) VALUES (901)"),
        ));
        attempts.push((
            "rollback_then_read.sql",
            "ROLLBACK; SELECT 1 WHERE false".to_owned(),
        ));
    }
    for (name, sql) in attempts {
        let file = c.file(name, &sql);
        let out = c.plan(&[&file], &planned, &["--format", "json"]);
        assert_eq!(
            code(&out),
            FINDING,
            "{name}: {}{}",
            stdout(&out),
            stderr(&out)
        );
        let envelope = json(&out);
        assert_eq!(ids(&envelope), ["plan.assertion.sql"], "{name}: {envelope}");
        assert!(!planned.exists(), "{name} wrote a plan");
        assert_eq!(
            count(c.connection(), pg, t),
            before,
            "{name} changed the target"
        );
    }
}

pub(super) fn the_assertion_connection_is_required_and_never_the_planning_login(
    server: &str,
    pg: bool,
) {
    let c = Case::new(server, pg, "conn");
    let ok = c.file("ok.sql", "SELECT 1 WHERE 1=0");
    let planned = c.out("plan.json");
    // Absent and blank are refused before any connection, by id: falling back
    // to the planning login would surface as the engine's refusal instead.
    let unset = std::process::Command::new(super::BIN)
        .arg("--project")
        .arg(&c.d.dir)
        .args([
            "plan",
            "--db",
            c.connection(),
            "--out",
            planned.to_str().unwrap(),
        ])
        .args(["--assert", ok.to_str().unwrap(), "--format", "json"])
        .env_remove(VAR)
        .output()
        .unwrap();
    refused(&unset, &planned);
    assert_eq!(ids(&json(&unset)), ["plan.assertion.connection"]);
    for conn in [None, Some("  ")] {
        let out = c.plan_with(&[&ok], &planned, &["--format", "json"], conn);
        refused(&out, &planned);
        let envelope = json(&out);
        assert_eq!(ids(&envelope), ["plan.assertion.connection"]);
        assert!(
            envelope["findings"][0]["message"]
                .as_str()
                .unwrap()
                .contains(VAR)
        );
    }
    let planning = c.connection().to_owned();
    let out = c.plan_with(&[&ok], &planned, &["--format", "json"], Some(&planning));
    refused(&out, &planned);
    let envelope = json(&out);
    assert_eq!(ids(&envelope), ["plan.assertion.refused"]);
    assert!(
        envelope["findings"][0]["message"]
            .as_str()
            .unwrap()
            .contains("separate assertion login")
    );
    let missing = c.d.dir.join("checks/missing.sql");
    let out = c.plan(&[&ok, &missing], &planned, &[]);
    let text = refused(&out, &planned);
    assert!(
        text.contains("missing.sql") && text.contains("does not exist"),
        "{text}"
    );
    // Offline there is no target to assert against.
    let out = c.d.run_with_env(
        &["plan", "--assert", ok.to_str().unwrap()],
        &[(VAR, &c.assertion)],
    );
    assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
    assert!(stderr(&out).contains("--db or --env"), "{}", stderr(&out));
    // Absent or refused never means "no assertions": an ordinary plan still
    // needs no variable at all.
    succeeds(c.d.run_with_env(
        &[
            "plan",
            "--db",
            c.connection(),
            "--out",
            planned.to_str().unwrap(),
        ],
        &[],
    ));
}

pub(super) fn a_login_that_can_write_is_refused_and_a_read_only_login_runs(server: &str, pg: bool) {
    let c = Case::new(server, pg, "priv");
    let ok = c.file("ok.sql", "SELECT 1 WHERE 1=0");
    let planned = c.out("plan.json");
    succeeds(c.plan(&[&ok], &planned, &[]));
    std::fs::remove_file(&planned).unwrap();
    let t = c.table();
    let (grant, revoke) = if pg {
        (
            format!("GRANT INSERT ON {t} TO \"{}\"", c.login),
            format!("REVOKE INSERT ON {t} FROM \"{}\"", c.login),
        )
    } else {
        (
            format!("GRANT INSERT ON {t} TO [{}]", c.login),
            format!("REVOKE INSERT ON {t} FROM [{}]", c.login),
        )
    };
    on_server(c.connection(), &grant);
    let out = c.plan(&[&ok], &planned, &["--format", "json"]);
    assert_eq!(code(&out), FINDING, "{}{}", stdout(&out), stderr(&out));
    let envelope = json(&out);
    assert_eq!(ids(&envelope), ["plan.assertion.refused"]);
    assert!(
        envelope["findings"][0]["message"]
            .as_str()
            .unwrap()
            .contains("INSERT"),
        "{envelope}"
    );
    assert!(!planned.exists());
    on_server(c.connection(), &revoke);
    // A role that grants the right indirectly is refused the same way.
    let indirect = if pg {
        format!(
            "CREATE ROLE \"{0}_w\" NOLOGIN; GRANT INSERT ON {t} TO \"{0}_w\"; GRANT \"{0}_w\" TO \"{0}\"",
            c.login
        )
    } else {
        format!("ALTER ROLE db_datawriter ADD MEMBER [{}]", c.login)
    };
    on_server(c.connection(), &indirect);
    let out = c.plan(&[&ok], &planned, &[]);
    let text = refused(&out, &planned);
    assert!(text.contains("INSERT"), "{text}");
    // PostgreSQL's helper role is cluster-wide: dropped before the case ends.
    if pg {
        on_server(
            c.connection(),
            &format!(
                "REVOKE \"{0}_w\" FROM \"{0}\"; DROP OWNED BY \"{0}_w\"",
                c.login
            ),
        );
        on_server(&c.server, &format!("DROP ROLE \"{0}_w\"", c.login));
    } else {
        on_server(
            c.connection(),
            &format!("ALTER ROLE db_datawriter DROP MEMBER [{}]", c.login),
        );
    }
    succeeds(c.plan(&[&ok], &planned, &[]));
}
