//! The same recording contract through both engine-facing CLI paths.
use super::stale_checkout::{apply, git, ledger, plan, records};
use super::{Demo, OwnDatabase, code, on_server, stderr, stdout};
use pbps_model::StateSnapshot;
use std::process::Output;

struct PendingChild(Option<std::process::Child>);
impl Drop for PendingChild {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn ok(out: Output) -> Output {
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    out
}
fn configure(d: &Demo, pg: bool) {
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!(
            "dialect: {}\nenvironments:\n  target:\n    url_env: PBPS_DATAWORK2402_DB\n",
            if pg { "postgres" } else { "mssql" }
        ),
    )
    .unwrap();
}
fn table(pg: bool, expanded: bool, pending: bool) -> String {
    format!(
        "table: {}.t\ncolumns:\n  id: {{type: int, nullable: false}}\n{}{}",
        if pg { "app" } else { "dbo" },
        if expanded {
            "  expanded: {type: int}\n"
        } else {
            ""
        },
        if pending {
            "  pending: {type: int}\n"
        } else {
            ""
        }
    )
}
fn record(d: &Demo, connection: &str, checksum: &str, outcome: &str, extra: &[&str]) -> Output {
    let mut args = vec![
        "state",
        "record-data-work",
        "--env",
        "target",
        "--expand-checksum",
        checksum,
        "--outcome",
        outcome,
        "--reason",
        "Populate the replacement column.",
    ];
    args.extend_from_slice(extra);
    d.run_with_env(&args, &[("PBPS_DATAWORK2402_DB", connection)])
}
fn latest(connection: &str, pg: bool) -> StateSnapshot {
    StateSnapshot::from_json(&records(connection, pg).last().unwrap().1).unwrap()
}
fn managed(snapshot: &StateSnapshot) -> serde_json::Value {
    let mut value = serde_json::to_value(snapshot).unwrap();
    for key in [
        "version",
        "kind",
        "data_work",
        "git_sha",
        "plan_checksum",
        "operator",
        "reason",
    ] {
        value.as_object_mut().unwrap().remove(key);
    }
    value
}
fn edit_snapshot(connection: &str, pg: bool, id: i64, value: &serde_json::Value) {
    runtime().block_on(async {
        let mut conn = connect(connection, pg).await;
        let sql = format!(
            "UPDATE {} SET state_json = {}, state_version = {} WHERE id = {id}",
            ledger(pg),
            if pg { "$1" } else { "@P1" },
            value["version"].as_u64().unwrap()
        );
        conn.execute_with(&sql, &[value.to_string().as_str().into()])
            .await
            .unwrap();
    });
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
async fn connect(connection: &str, pg: bool) -> pbps_db::Conn {
    pbps_db::Conn::connect(
        if pg {
            pbps_db::Driver::Postgres
        } else {
            pbps_db::Driver::Mssql
        },
        connection,
    )
    .await
    .unwrap()
}
fn scalar(connection: &str, pg: bool, sql: &str) -> i64 {
    runtime().block_on(async {
        connect(connection, pg).await.query(sql).await.unwrap()[0]
            .try_get::<i64>("n")
            .unwrap()
            .unwrap()
    })
}
fn expanded(
    server: &str,
    pg: bool,
    slug: &str,
    existing_column: bool,
) -> (OwnDatabase, Demo, Demo, String) {
    let own = OwnDatabase::new(server, slug);
    let connection = own.connection();
    if pg {
        on_server(connection, "CREATE SCHEMA app");
    }
    let d = Demo::new(slug);
    let artifacts = Demo::new(&format!("{slug}-artifacts"));
    configure(&d, pg);
    d.table(&table(pg, existing_column, false));
    ok(d.run(&["plan", "--no-dev"]));
    d.commit();
    ok(d.run(&["bootstrap", "--db", connection]));
    let expanded = table(pg, true, false);
    d.table(&if existing_column {
        expanded.replace("expanded: {type: int}", "expanded: {type: bigint}")
    } else {
        expanded
    });
    ok(d.run(&["plan", "--no-dev"]));
    d.commit();
    let path = artifacts.dir.join("expand.json");
    let expand = plan(&d, connection, &path);
    ok(apply(&d, connection, &path, &expand));
    (own, d, artifacts, expand.wire_fingerprint())
}

pub fn acceptance(server: &str, pg: bool) {
    let (own, d, artifacts, checksum) = expanded(server, pg, "datawork2402", false);
    let connection = own.connection();
    // A genuine version-33 snapshot remains version 33; appending is the upgrade.
    let id = records(connection, pg).last().unwrap().0;
    let mut old = serde_json::to_value(latest(connection, pg)).unwrap();
    old["version"] = 33.into();
    edit_snapshot(connection, pg, id, &old);
    let before = latest(connection, pg);
    assert_eq!(before.version, 33);
    let columns = [
        "state_version",
        "tables_count",
        "modules_count",
        "staged_completed",
        "staged_total",
    ];
    let drops = if pg {
        columns.map(|c| format!("DROP COLUMN {c}")).join(", ")
    } else {
        format!("DROP COLUMN {}", columns.join(", "))
    };
    on_server(connection, &format!("ALTER TABLE {} {drops}", ledger(pg)));

    d.table(&table(pg, true, true));
    ok(d.run(&["plan", "--no-dev"]));
    d.commit();
    let candidate = artifacts.dir.join("candidate.json");
    let before_plan = plan(&d, connection, &candidate);
    let saved_bytes = std::fs::read(&candidate).unwrap();
    let before_verify = ok(d.run(&["verify", "--db", connection, "--format", "json"]));
    let script = format!(
        "INSERT INTO {}.t(id) VALUES (999);\n",
        if pg { "app" } else { "dbo" }
    );
    std::fs::write(d.dir.join("backfill.sql"), &script).unwrap();
    let notes = "line\n\u{1b}\u{202e} operator note";
    // Record a failed result successfully, then a completed result with another identity.
    let out = ok(record(
        &d,
        connection,
        &checksum,
        "failed",
        &[
            "--script",
            "backfill.sql",
            "--notes",
            notes,
            "--format",
            "json",
        ],
    ));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let published: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../schemas/envelope.schema.json"),
        )
        .unwrap(),
    )
    .unwrap();
    jsonschema::validator_for(&published)
        .unwrap()
        .validate(&report)
        .unwrap();
    assert_eq!(report["result"], "ok");
    assert_eq!(report["data"]["self_reported"], true);
    assert_eq!(report["data"]["data_work"]["outcome"], "failed");
    assert_eq!(report["data"]["data_work"]["notes"], notes);
    let recorded_id = report["data"]["entry_id"].as_i64().unwrap();
    let after = latest(connection, pg);
    assert_eq!(after.kind, pbps_model::StateKind::DataWork);
    assert_eq!(after.version, pbps_model::state::CURRENT_VERSION);
    assert_eq!(after.operator, "demo");
    assert_eq!(managed(&after), managed(&before));
    use sha2::Digest as _;
    assert_eq!(
        report["data"]["data_work"]["identity"]["sha256"],
        format!("{:x}", sha2::Sha256::digest(script.as_bytes()))
    );
    assert_eq!(
        report["data"]["data_work"]["identity"]["path"],
        "backfill.sql"
    );
    let show = ok(d.run(&[
        "state",
        "show",
        &recorded_id.to_string(),
        "--db",
        connection,
        "--format",
        "json",
    ]));
    let shown: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(
        shown["data"]["entry"]["snapshot"]["data_work"],
        report["data"]["data_work"]
    );
    let human = ok(d.run(&[
        "state",
        "show",
        &recorded_id.to_string(),
        "--db",
        connection,
    ]));
    assert!(
        stdout(&human).contains(&pbps_model::Text::new(notes).shown().to_string()),
        "{}",
        stdout(&human)
    );
    assert!(!stdout(&human).contains('\u{1b}'));
    let listed = ok(d.run(&["state", "list", "--db", connection, "--format", "json"]));
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed["data"]["entries"][0]["kind"], "data_work");
    assert_eq!(
        listed["data"]["entries"][0]["reason"],
        "Populate the replacement column."
    );
    assert!(stdout(&ok(d.run(&["state", "list", "--db", connection]))).contains("data_work"));
    let legacy = ok(d.run(&[
        "state",
        "show",
        &id.to_string(),
        "--db",
        connection,
        "--format",
        "json",
    ]));
    let legacy: serde_json::Value = serde_json::from_slice(&legacy.stdout).unwrap();
    assert_eq!(legacy["data"]["entry"]["snapshot"]["version"], 33);
    let out = ok(record(
        &d,
        connection,
        &checksum,
        "completed",
        &[
            "--external-version",
            "job\n\u{1b}\u{202e}v7",
            "--notes",
            notes,
        ],
    ));
    assert_eq!(
        stdout(&out),
        format!(
            "Recorded self-reported data work as entry #{} in `target`: completed.\nExternal version: job\\n\\u{{1b}}\\u{{202e}}v7\nExpand checksum: {checksum}\nReason: Populate the replacement column.\nNotes: line\\n\\u{{1b}}\\u{{202e}} operator note\npbps did not execute or verify this work.\n",
            recorded_id + 1
        )
    );
    assert_eq!(managed(&latest(connection, pg)), managed(&before));
    let mut after_plan = serde_json::to_value(plan(&d, connection, &candidate)).unwrap();
    let mut before_artifact = serde_json::to_value(&before_plan).unwrap();
    // History advances, so its source-entry label and creation time do too.
    // Every executable input, including the baseline checksum, stays identical.
    for value in [&mut after_plan, &mut before_artifact] {
        value.as_object_mut().unwrap().remove("created_at");
        value["baseline"]
            .as_object_mut()
            .unwrap()
            .remove("description");
    }
    assert_eq!(after_plan, before_artifact);
    let after_verify = ok(d.run(&["verify", "--db", connection, "--format", "json"]));
    let mut verified_before: serde_json::Value =
        serde_json::from_slice(&before_verify.stdout).unwrap();
    let mut verified_after: serde_json::Value =
        serde_json::from_slice(&after_verify.stdout).unwrap();
    assert_eq!(verified_before["data"]["baseline"]["entry_id"], id);
    assert_eq!(
        verified_after["data"]["baseline"]["entry_id"],
        recorded_id + 1
    );
    for value in [&mut verified_before, &mut verified_after] {
        let data = value["data"].as_object_mut().unwrap();
        data.remove("checked_at");
        let baseline = data["baseline"].as_object_mut().unwrap();
        baseline.remove("entry_id");
        baseline.remove("applied_at");
    }
    assert_eq!(verified_before, verified_after);
    assert_eq!(
        scalar(
            connection,
            pg,
            &format!(
                "SELECT CAST(COUNT(*) AS BIGINT) AS n FROM {}.t",
                if pg { "app" } else { "dbo" }
            )
        ),
        0,
        "the supplied script must never execute"
    );
    denied_column(server, connection, &d, pg, &checksum);
    on_server(
        connection,
        &format!("DELETE FROM {} WHERE kind = 'apply'", ledger(pg)),
    );
    let retained = records(connection, pg);
    let pruned = record(
        &d,
        connection,
        &checksum,
        "completed",
        &["--external-version", "v7", "--format", "json"],
    );
    assert_eq!(code(&pruned), 2, "{}{}", stdout(&pruned), stderr(&pruned));
    assert_eq!(records(connection, pg), retained);
    // Recording does not invalidate an artifact approved before the report.
    std::fs::write(&candidate, saved_bytes).unwrap();
    ok(apply(&d, connection, &candidate, &before_plan));
    assert!(latest(connection, pg).data_work.is_none());
}

pub fn refusals(server: &str, pg: bool) {
    let own = OwnDatabase::new(server, "datawork-refusals2402");
    let d = Demo::new("datawork-refusals2402");
    configure(&d, pg);
    let connection = own.connection();
    let hash = "a".repeat(64);
    let extra = ["--external-version", "job-v7", "--format", "json"];
    let absent = record(&d, connection, &hash, "completed", &extra);
    assert_eq!(code(&absent), 2, "{}{}", stdout(&absent), stderr(&absent));
    assert!(stdout(&absent).contains("ledger is absent"));
    if pg {
        on_server(connection, "CREATE SCHEMA app");
    }
    d.table(&table(pg, false, false));
    ok(d.run(&["plan", "--no-dev"]));
    d.commit();
    ok(d.run(&["bootstrap", "--db", connection]));
    let before = records(connection, pg);
    let blank = d.run_with_env(
        &[
            "state",
            "record-data-work",
            "--env",
            "target",
            "--expand-checksum",
            &hash,
            "--outcome",
            "completed",
            "--reason",
            " \t",
            "--external-version",
            "v7",
        ],
        &[("PBPS_DATAWORK2402_DB", connection)],
    );
    assert_eq!(code(&blank), 2);
    assert!(stderr(&blank).contains("audit reason must not be empty"));
    assert_eq!(records(connection, pg), before);
    let unknown = record(&d, connection, &hash, "completed", &extra);
    assert_eq!(
        code(&unknown),
        2,
        "{}{}",
        stdout(&unknown),
        stderr(&unknown)
    );
    assert!(stdout(&unknown).contains("no retained successful apply"));
    assert_eq!(records(connection, pg), before);
    // Even a retained failed event with this checksum cannot authorize the report.
    let mut failed = latest(connection, pg);
    failed.kind = pbps_model::StateKind::Failed;
    failed.plan_checksum = Some(hash.clone());
    runtime().block_on(async {
        let mut conn = connect(connection, pg).await;
        if pg {
            pbps_pg::state::record(&mut conn, &failed).await.unwrap();
        } else {
            pbps_mssql::state::record(&mut conn, &failed).await.unwrap();
        }
    });
    let unknown = record(&d, connection, &hash, "completed", &extra);
    assert_eq!(
        code(&unknown),
        2,
        "{}{}",
        stdout(&unknown),
        stderr(&unknown)
    );
    assert!(stdout(&unknown).contains("no retained successful apply"));
    let unknown_env = d.run_with_env(
        &[
            "state",
            "record-data-work",
            "--env",
            "missing",
            "--external-version",
            "v7",
            "--expand-checksum",
            &hash,
            "--outcome",
            "completed",
            "--reason",
            "work",
            "--format",
            "json",
        ],
        &[("PBPS_DATAWORK2402_DB", connection)],
    );
    assert_eq!(code(&unknown_env), 1);
    assert!(stdout(&unknown_env).contains("environment.unresolved"));
    let before_invalid = records(connection, pg);
    let mut invalid = latest(connection, pg);
    invalid.kind = pbps_model::StateKind::DataWork;
    runtime().block_on(async {
        let mut conn = connect(connection, pg).await;
        let result = if pg {
            pbps_pg::state::record(&mut conn, &invalid).await
        } else {
            pbps_mssql::state::record(&mut conn, &invalid).await
        };
        assert!(
            result.is_err(),
            "invalid new-kind metadata must not be persisted"
        );
    });
    assert_eq!(records(connection, pg), before_invalid);
    // Unsupported, damaged and empty are separate answers, never an empty default snapshot.
    let rows = records(connection, pg);
    let (id, json, _) = rows.last().unwrap();
    let good: serde_json::Value = serde_json::from_str(json).unwrap();
    for bad in [
        serde_json::json!({"version":5}),
        serde_json::json!({"version":999}),
        serde_json::json!({"version":34}),
    ] {
        edit_snapshot(connection, pg, *id, &bad);
        let out = record(&d, connection, &hash, "completed", &extra);
        assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
        assert!(stdout(&out).contains("state.data-work.unreadable"));
        assert_eq!(records(connection, pg).len(), rows.len());
    }
    edit_snapshot(connection, pg, *id, &good);
    denied_column(server, connection, &d, pg, &hash);
    on_server(connection, &format!("DELETE FROM {}", ledger(pg)));
    let empty = record(&d, connection, &hash, "completed", &extra);
    assert_eq!(code(&empty), 2);
    assert!(stdout(&empty).contains("ledger has no entries"));
}

pub fn locked_and_staged(server: &str, pg: bool) {
    let (own, d, _artifacts, checksum) = expanded(server, pg, "datawork-lock2402", false);
    let connection = own.connection();
    let before = records(connection, pg);
    let rt = runtime();
    let mut holder = rt.block_on(connect(connection, pg));
    rt.block_on(async {
        if pg {
            pbps_pg::state::lock(&mut holder, "other deployer")
                .await
                .unwrap();
        } else {
            pbps_mssql::state::lock(&mut holder, "other deployer")
                .await
                .unwrap();
        }
    });
    let out = record(
        &d,
        connection,
        &checksum,
        "completed",
        &["--external-version", "v7", "--format", "json"],
    );
    assert_ne!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    assert_eq!(records(connection, pg), before);
    rt.block_on(async {
        if pg {
            assert!(pbps_pg::state::unlock(&mut holder).await.unwrap());
        } else {
            assert!(pbps_mssql::state::unlock(&mut holder).await.unwrap());
        }
    });
    rereads_after_writer_finishes(connection, &d, pg, &checksum);
    let after_concurrent = records(connection, pg).len();
    let mut staged = latest(connection, pg);
    staged.data_work = None;
    staged.plan_checksum = Some(checksum.clone());
    staged.kind = pbps_model::StateKind::Staged;
    staged.staged = Some(pbps_model::StagedProgress {
        completed: 1,
        total: 2,
        last_statement: "ALTER TABLE".into(),
    });
    rt.block_on(async {
        if pg {
            pbps_pg::state::record(&mut holder, &staged).await.unwrap();
        } else {
            pbps_mssql::state::record(&mut holder, &staged)
                .await
                .unwrap();
        }
    });
    let out = record(
        &d,
        connection,
        &checksum,
        "completed",
        &["--external-version", "v7", "--format", "json"],
    );
    assert_eq!(code(&out), 2, "{}{}", stdout(&out), stderr(&out));
    assert!(stdout(&out).contains("finish the staged deployment"));
    assert_eq!(records(connection, pg).len(), after_concurrent + 1);
}

pub fn ancestry(server: &str, pg: bool) {
    let (own, d, _artifacts, checksum) = expanded(server, pg, "datawork-ancestry2402", true);
    let connection = own.connection();
    let deployed = git(&d.dir, &["rev-parse", "HEAD"]);
    d.git(&["checkout", "-q", "--detach", "HEAD~1"]);
    d.table(&table(pg, false, false));
    // Record real drop intent so the ancestry check is the only refusal, even
    // after #2414 tightens deletion admission. The old branch knows this UID.
    ok(d.run(&[
        "drop",
        &format!("{}.t.expanded", if pg { "app" } else { "dbo" }),
        "--reason",
        "retired",
    ]));
    d.commit();
    let operator_sha = git(&d.dir, &["rev-parse", "HEAD"]);
    let refusal = || {
        let out = d.run_with_env(
            &["plan", "--env", "target", "--format", "json"],
            &[("PBPS_DATAWORK2402_DB", connection)],
        );
        assert_eq!(code(&out), 2, "{}{}", stdout(&out), stderr(&out));
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["findings"][0]["id"], "plan.checkout-ancestry");
        report["findings"].clone()
    };
    let before = refusal();
    assert!(before.to_string().contains(&deployed));
    for outcome in ["completed", "failed"] {
        ok(record(
            &d,
            connection,
            &checksum,
            outcome,
            &["--external-version", "v7"],
        ));
        assert_eq!(latest(connection, pg).git_sha.unwrap().raw(), operator_sha);
        assert_eq!(refusal(), before);
    }
}

fn denied_column(server: &str, connection: &str, d: &Demo, pg: bool, checksum: &str) {
    struct RestrictedReader<'a> {
        server: &'a str,
        connection: &'a str,
        name: String,
        pg: bool,
    }
    impl Drop for RestrictedReader<'_> {
        fn drop(&mut self) {
            if self.pg {
                let _ =
                    super::try_on_server(self.connection, &format!("DROP OWNED BY {}", self.name));
                let _ = super::try_on_server(self.server, &format!("DROP ROLE {}", self.name));
            } else {
                super::on_server(self.connection, &format!("DROP USER [{}]", self.name));
                drop_test_login(self.server, &self.name);
            }
        }
    }
    let role = RestrictedReader {
        server,
        connection,
        name: format!("pbps_datawork_reader_{}", std::process::id()),
        pg,
    };
    let name = &role.name;
    let password = "Datawork!2402Denied";
    let restricted = if pg {
        on_server(
            server,
            &format!("CREATE ROLE {name} LOGIN PASSWORD '{password}'"),
        );
        on_server(
            connection,
            &format!(
                "GRANT USAGE ON SCHEMA public TO {name}; GRANT SELECT (id, applied_at, kind, git_sha, plan_checksum, operator, reason, state_version, tables_count, modules_count, staged_completed, staged_total) ON public.__pbps_state TO {name}"
            ),
        );
        let base = connection
            .split_whitespace()
            .filter(|s| !s.starts_with("user=") && !s.starts_with("password="))
            .collect::<Vec<_>>()
            .join(" ");
        format!("{base} user={name} password={password}")
    } else {
        on_server(
            server,
            &format!("CREATE LOGIN [{name}] WITH PASSWORD = '{password}', CHECK_POLICY = OFF"),
        );
        on_server(
            connection,
            &format!(
                "CREATE USER [{name}] FOR LOGIN [{name}]; GRANT SELECT ON dbo.__pbps_state TO [{name}]; DENY SELECT ON dbo.__pbps_state(state_json) TO [{name}]"
            ),
        );
        let base = connection
            .split(';')
            .filter(|s| {
                !matches!(
                    s.split('=')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase()
                        .as_str(),
                    "user id" | "uid" | "password" | "pwd"
                )
            })
            .collect::<Vec<_>>()
            .join(";");
        format!("{base};User Id={name};Password={password}")
    };
    let before = records(connection, pg);
    let out = record(
        d,
        &restricted,
        checksum,
        "completed",
        &["--external-version", "v7", "--format", "json"],
    );
    assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
    assert!(stdout(&out).contains("state.data-work.unreadable"));
    assert_eq!(records(connection, pg), before);
    let listing = ok(d.run(&["state", "list", "--db", &restricted, "--format", "json"]));
    let listing: serde_json::Value = serde_json::from_slice(&listing.stdout).unwrap();
    assert_eq!(
        listing["data"]["entries"][0]["kind"],
        latest(connection, pg).kind.as_str()
    );
}

fn rereads_after_writer_finishes(connection: &str, d: &Demo, pg: bool, checksum: &str) {
    let mut next = latest(connection, pg);
    next.kind = pbps_model::StateKind::Baseline;
    next.plan_checksum = None;
    next.operator = "concurrent baseline".into();
    next.reason = Some("adopt concurrent change".into());
    let table = next.schema.tables.values_mut().next().unwrap();
    table
        .columns
        .insert("concurrent".into(), table.columns["id"].clone());
    let rt = runtime();
    let mut writer = rt.block_on(connect(connection, pg));
    rt.block_on(async {
        writer
            .execute(if pg { "BEGIN" } else { "BEGIN TRANSACTION" })
            .await
            .unwrap();
        if pg {
            pbps_pg::state::lock(&mut writer, "concurrent baseline")
                .await
                .unwrap();
        } else {
            pbps_mssql::state::lock(&mut writer, "concurrent baseline")
                .await
                .unwrap();
        }
    });
    let writer_id = rt.block_on(async {
        writer
            .query(if pg {
                "SELECT CAST(pg_backend_pid() AS BIGINT) AS n"
            } else {
                "SELECT CAST(@@SPID AS BIGINT) AS n"
            })
            .await
            .unwrap()[0]
            .try_get::<i64>("n")
            .unwrap()
            .unwrap()
    });
    let mut child = PendingChild(Some(
        std::process::Command::new(super::BIN)
            .arg("--project")
            .arg(&d.dir)
            .args([
                "state",
                "record-data-work",
                "--env",
                "target",
                "--expand-checksum",
                checksum,
                "--outcome",
                "completed",
                "--reason",
                "concurrent work",
                "--external-version",
                "v7",
                "--format",
                "json",
            ])
            .env("PBPS_DATAWORK2402_DB", connection)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    // Observe a real server wait rather than assuming the CLI reached preflight
    // after a sleep. Its first read sees the old snapshot; the lock waits here.
    let blocked = rt.block_on(async {
        let mut observer = connect(connection, pg).await;
        let sql = if pg {
            format!("SELECT CAST(COUNT(*) AS BIGINT) AS n FROM pg_stat_activity WHERE {writer_id} = ANY(pg_blocking_pids(pid))")
        } else {
            format!("SELECT CAST(COUNT(*) AS BIGINT) AS n FROM sys.dm_exec_requests WHERE blocking_session_id = {writer_id}")
        };
        let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < until {
            if observer.query(&sql).await.unwrap()[0].try_get::<i64>("n").unwrap().unwrap() > 0 { return true; }
            if child.0.as_mut().unwrap().try_wait().unwrap().is_some() { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        false
    });
    if !blocked {
        let _ = child.0.as_mut().unwrap().kill();
        let out = child.0.take().unwrap().wait_with_output().unwrap();
        rt.block_on(writer.execute("ROLLBACK")).unwrap();
        panic!(
            "recording never waited for the writer: {}{}",
            stdout(&out),
            stderr(&out)
        );
    }
    rt.block_on(async {
        writer
            .execute(&format!(
                "ALTER TABLE {}.t ADD concurrent int NOT NULL",
                if pg { "app" } else { "dbo" }
            ))
            .await
            .unwrap();
        if pg {
            pbps_pg::state::record(&mut writer, &next).await.unwrap();
            pbps_pg::state::unlock(&mut writer).await.unwrap();
        } else {
            pbps_mssql::state::record(&mut writer, &next).await.unwrap();
            pbps_mssql::state::unlock(&mut writer).await.unwrap();
        }
        writer.execute("COMMIT").await.unwrap();
    });
    ok(child.0.take().unwrap().wait_with_output().unwrap());
    let reported = latest(connection, pg);
    assert_eq!(reported.operator, "demo");
    assert_eq!(reported.reason.unwrap().raw(), "concurrent work");
    assert_eq!(
        managed(&latest(connection, pg)),
        managed(&next),
        "copy the state read after taking the lock, not the preflight read"
    );
}

pub fn cli_boundaries(pg: bool) {
    let d = Demo::new("datawork-cli2402");
    configure(&d, pg);
    let hash = "a".repeat(64);
    let args = [
        "state",
        "record-data-work",
        "--env",
        "target",
        "--external-version",
        "v7",
        "--expand-checksum",
        &hash,
        "--outcome",
        "completed",
        "--reason",
        "work",
        "--format",
        "json",
    ];
    let connection = if pg {
        "host=127.0.0.1 port=9 user=unused dbname=unused sslmode=disable"
    } else {
        "Server=127.0.0.1,9;User Id=unused;Password=unused;TrustServerCertificate=true"
    };
    let mut file_args = args;
    file_args[4] = "--script";
    file_args[5] = "missing.sql";
    let missing_file = d.run_with_env(&file_args, &[("PBPS_DATAWORK2402_DB", connection)]);
    assert_eq!(code(&missing_file), 1);
    let report: serde_json::Value = serde_json::from_slice(&missing_file.stdout).unwrap();
    assert_eq!(
        report["findings"][0]["id"], "state.data-work.identity",
        "file identity is read before connecting"
    );
    if cfg!(unix) {
        assert!(
            std::process::Command::new("mkfifo")
                .arg(d.dir.join("input-pipe.sql"))
                .status()
                .unwrap()
                .success()
        );
        file_args[5] = "input-pipe.sql";
        let mut pipe = PendingChild(Some(
            std::process::Command::new(super::BIN)
                .arg("--project")
                .arg(&d.dir)
                .args(file_args)
                .env("PBPS_DATAWORK2402_DB", connection)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        ));
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pipe.0.as_mut().unwrap().try_wait().unwrap().is_none()
            && std::time::Instant::now() < until
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if pipe.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            pipe.0.as_mut().unwrap().kill().unwrap();
        }
        let out = pipe.0.take().unwrap().wait_with_output().unwrap();
        assert_eq!(
            code(&out),
            1,
            "refuse a FIFO without waiting for a writer: {}{}",
            stdout(&out),
            stderr(&out)
        );
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["findings"][0]["id"], "state.data-work.identity");
    }
    std::fs::remove_file(d.dir.join("pbps.yml")).unwrap();
    let identity = d.run_with_env(&args, &[("PBPS_OPERATOR", " \t")]);
    assert_eq!(code(&identity), 2);
    assert!(stderr(&identity).contains("PBPS_OPERATOR"));
    let missing_project = d.run(&args);
    assert_eq!(code(&missing_project), 1);
    let report: serde_json::Value = serde_json::from_slice(&missing_project.stdout).unwrap();
    assert_eq!(report["command"], "state record-data-work");
    assert_eq!(report["result"], "unanswerable");
    assert_eq!(report["findings"][0]["id"], "project.undiscoverable");
}

/// A warm fixture must not leave server principals for a later case. The child
/// has exited; wait only for SQL Server to finish closing its session (15434).
pub(super) fn drop_test_login(connection: &str, login: &str) {
    let quoted = login.replace(']', "]]");
    let literal = login.replace('\'', "''");
    super::on_server(
        connection,
        &format!(
            "USE master; DECLARE @deadline datetime2 = DATEADD(second,30,SYSUTCDATETIME()); \
         WHILE 1=1 BEGIN BEGIN TRY DROP LOGIN [{quoted}]; BREAK; END TRY \
         BEGIN CATCH IF ERROR_NUMBER()<>15434 OR SYSUTCDATETIME()>=@deadline THROW; \
         WAITFOR DELAY '00:00:00.100'; END CATCH END; \
         IF SUSER_ID(N'{literal}') IS NOT NULL THROW 51000,'owned login was not removed',1;"
        ),
    );
}
