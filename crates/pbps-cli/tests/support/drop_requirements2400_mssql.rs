use super::{BIN, Demo, OwnDatabase, code, on_server, plan_checksum, stderr, stdout};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
const BEFORE: &str = "table: dbo.t\ncolumns:\n  id: {type: int, nullable: false}\n  amount: {type: int}\n  cents: {type: int}\nprimary_key: {name: pk_t, columns: [id]}\n";
const AFTER: &str = "table: dbo.t\ncolumns:\n  id: {type: int, nullable: false}\n  cents: {type: int}\n  marker: {type: int}\nprimary_key: {name: pk_t, columns: [id]}\n";
const PREDICATE: &str = "cents=amount*100 AND ABS(amount)>=0";
fn ok(out: Output) -> Output {
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    out
}
fn server() -> String {
    std::env::var("PBPS_TEST_DB").unwrap()
}
fn setup(connection: &str, tag: &str) -> Demo {
    let d = Demo::new(tag);
    d.table(BEFORE);
    ok(d.run(&["plan", "--no-dev"]));
    d.commit();
    ok(d.run(&["bootstrap", "--db", connection]));
    d.table(AFTER);
    d
}
fn bytes(d: &Demo) -> Vec<u8> {
    std::fs::read(d.ids_path()).unwrap()
}
fn record(d: &Demo, connection: &str, predicate: &str) {
    ok(d.run(&[
        "drop",
        "dbo.t.amount",
        "--reason",
        "copied",
        "--require",
        predicate,
        "--db",
        connection,
    ]));
}
fn plan(d: &Demo, connection: &str) -> PathBuf {
    ok(d.run(&["plan", "--no-dev"]));
    d.commit();
    let path = d.dir.join("guarded-plan.json");
    ok(d.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()]));
    path
}
fn apply(d: &Demo, connection: &str, path: &Path) -> Output {
    d.run(&[
        "apply",
        "--db",
        connection,
        "--plan",
        path.to_str().unwrap(),
        "--checksum",
        &plan_checksum(path),
        "--allow",
        "destructive,rename",
    ])
}
fn scalar(connection: &str, sql: &str) -> i64 {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut c = super::connect_live(connection).await.unwrap();
            c.query(sql).await.unwrap()[0]
                .try_get_at::<i64>(0)
                .unwrap()
                .unwrap()
        })
}

#[test]
#[ignore = "needs live mssql"]
fn guarded_drops_record_replace_clear_checksum_and_apply_on_sql_server() {
    let own = OwnDatabase::new(&server(), "require2400_success_ms");
    let connection = own.connection();
    let d = setup(connection, "require2400_success_ms");
    on_server(connection, "INSERT dbo.t VALUES(1,2,200)");
    let mut child = Command::new(BIN)
        .arg("--project")
        .arg(&d.dir)
        .args([
            "drop",
            "dbo.t.amount",
            "--reason",
            "copied",
            "--require",
            "-",
            "--db",
            connection,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{PREDICATE}\r\n").as_bytes())
        .unwrap();
    ok(child.wait_with_output().unwrap());
    let first = bytes(&d);
    record(&d, connection, PREDICATE);
    assert_eq!(bytes(&d), first);
    let path = plan(&d, connection);
    let checksum = plan_checksum(&path);
    let saved: pbps_model::SavedPlan =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let guard = saved
        .changes
        .changes
        .iter()
        .find_map(|p| p.drop_guard.as_ref())
        .unwrap();
    assert_eq!(
        guard.requirements.values().next().unwrap().predicate,
        PREDICATE
    );
    let explained = ok(d.run(&["explain", "--plan", path.to_str().unwrap()]));
    assert!(stdout(&explained).contains("exclusive table lock through the drop, so writers wait"));
    let sql = d.dir.join("guarded.sql");
    let refused = d.run(&["plan", "--db", connection, "--sql", sql.to_str().unwrap()]);
    assert_ne!(code(&refused), 0);
    assert!(
        stderr(&refused).contains("standalone SQL export"),
        "{}",
        stderr(&refused)
    );
    assert!(!sql.exists());
    let refused = d.run(&["plan", "--db", connection, "--staged"]);
    assert_ne!(code(&refused), 0);
    assert!(
        stderr(&refused).contains("transactional plan"),
        "{}",
        stderr(&refused)
    );
    ok(d.run(&[
        "drop",
        "dbo.t.amount",
        "--reason",
        "copied",
        "--clear-require",
        "--db",
        connection,
    ]));
    let ids: pbps_model::IdsFile = serde_json::from_slice(&bytes(&d)).unwrap();
    assert!(ids.tombstones.values().all(|t| t.require.is_none()));
    record(&d, connection, "cents=amount*100 AND amount>=0");
    let path = plan(&d, connection);
    assert_ne!(plan_checksum(&path), checksum);
    let stale = d.run(&[
        "apply",
        "--db",
        connection,
        "--plan",
        path.to_str().unwrap(),
        "--checksum",
        &checksum,
        "--allow",
        "destructive",
    ]);
    assert_ne!(code(&stale), 0);
    assert!(
        stderr(&stale).contains("no longer matches"),
        "{}",
        stderr(&stale)
    );
    ok(apply(&d, connection, &path));
    assert_eq!(
        scalar(
            connection,
            "SELECT COUNT_BIG(*) FROM sys.columns WHERE object_id=OBJECT_ID(N'dbo.t') AND name=N'amount'"
        ),
        0
    );
    assert_eq!(
        scalar(
            connection,
            &format!(
                "SELECT COUNT_BIG(*) FROM dbo.__pbps_state WHERE plan_checksum=N'{}'",
                plan_checksum(&path)
            )
        ),
        1
    );
}

#[test]
#[ignore = "needs live mssql"]
fn a_late_violating_row_rolls_back_the_drop_and_prior_statements_on_sql_server() {
    let own = OwnDatabase::new(&server(), "require2400_late_ms");
    let connection = own.connection();
    let d = setup(connection, "require2400_late_ms");
    on_server(connection, "INSERT dbo.t VALUES(1,2,200)");
    record(&d, connection, PREDICATE);
    let path = plan(&d, connection);
    on_server(connection, "INSERT dbo.t VALUES(2,2,199)");
    let refused = apply(&d, connection, &path);
    assert_ne!(code(&refused), 0);
    assert!(
        stderr(&refused).contains(&format!(
            "1 row(s) of dbo.t violate the row requirement `{PREDICATE}`"
        )),
        "{}",
        stderr(&refused)
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT COUNT_BIG(*) FROM sys.columns WHERE object_id=OBJECT_ID(N'dbo.t') AND name=N'amount'"
        ),
        1
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT COUNT_BIG(*) FROM sys.columns WHERE object_id=OBJECT_ID(N'dbo.t') AND name=N'marker'"
        ),
        0
    );
    assert_eq!(scalar(connection, "SELECT COUNT_BIG(*) FROM dbo.t"), 2);
}

#[test]
#[ignore = "needs live mssql"]
fn unsafe_requirements_write_nothing_and_referenced_renames_split_the_plan_on_sql_server() {
    let own = OwnDatabase::new(&server(), "require2400_record_ms");
    let connection = own.connection();
    let d = setup(connection, "require2400_record_ms");
    on_server(connection, "CREATE TABLE dbo.other(amount int)");
    on_server(
        connection,
        "CREATE FUNCTION dbo.user_check(@x int) RETURNS int AS BEGIN RETURN @x END",
    );
    let before = bytes(&d);
    for predicate in [
        "GETDATE() IS NOT NULL",
        "dbo.user_check(amount)=0",
        "other.amount=amount",
        "EXISTS(SELECT 1 FROM dbo.other)",
        "1=1); DROP TABLE dbo.t; --",
    ] {
        let refused = d.run(&[
            "drop",
            "dbo.t.amount",
            "--reason",
            "copied",
            "--require",
            predicate,
            "--db",
            connection,
        ]);
        assert_ne!(code(&refused), 0, "{predicate}");
        assert!(
            stderr(&refused).contains("plan-time assertion")
                && stderr(&refused).contains("CHECK constraint"),
            "{}",
            stderr(&refused)
        );
        assert_eq!(bytes(&d), before);
    }
    on_server(
        connection,
        "DROP FUNCTION dbo.user_check; DROP TABLE dbo.other;",
    );
    record(&d, connection, PREDICATE);
    let original = bytes(&d);
    let mut ids: pbps_model::IdsFile = serde_json::from_slice(&original).unwrap();
    for t in ids.tombstones.values_mut() {
        if let Some(r) = &mut t.require {
            r.predicate = "GETDATE() IS NOT NULL".into();
        }
    }
    std::fs::write(d.ids_path(), serde_json::to_vec_pretty(&ids).unwrap()).unwrap();
    d.commit();
    let refused = d.run(&["plan", "--db", connection]);
    assert_ne!(code(&refused), 0);
    assert!(
        stderr(&refused).contains("plan-time assertion"),
        "{}",
        stderr(&refused)
    );
    std::fs::write(d.ids_path(), original).unwrap();
    d.commit();
    d.table(&AFTER.replace("  cents: {type: int}", "  amount_cents: {type: int}"));
    ok(d.run(&["rename", "dbo.t.cents", "amount_cents"]));
    d.commit();
    let refused = d.run(&["plan", "--db", connection]);
    assert_ne!(code(&refused), 0);
    assert!(
        stderr(&refused).contains("split the plan"),
        "{}",
        stderr(&refused)
    );
}

#[test]
#[ignore = "needs live mssql"]
fn a_table_drop_honors_its_removed_columns_requirements_on_sql_server() {
    let own = OwnDatabase::new(&server(), "require2400_table_ms");
    let connection = own.connection();
    let d = setup(connection, "require2400_table_ms");
    on_server(connection, "INSERT dbo.t VALUES(1,2,199)");
    record(&d, connection, PREDICATE);
    std::fs::remove_file(d.dir.join("schema/dbo.t.yml")).unwrap();
    ok(d.run(&[
        "drop-table",
        "dbo.t",
        "--reason",
        "archived",
        "--require",
        "id>0",
        "--db",
        connection,
    ]));
    let path = plan(&d, connection);
    let refused = apply(&d, connection, &path);
    assert_ne!(code(&refused), 0);
    assert!(
        stderr(&refused).contains("1 row(s) of dbo.t violate"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT COUNT_BIG(*) FROM sys.tables WHERE object_id=OBJECT_ID(N'dbo.t')"
        ),
        1
    );
    on_server(connection, "UPDATE dbo.t SET cents=amount*100");
    ok(apply(&d, connection, &path));
    assert_eq!(
        scalar(
            connection,
            "SELECT COUNT_BIG(*) FROM sys.tables WHERE object_id=OBJECT_ID(N'dbo.t')"
        ),
        0
    );
}

#[test]
#[ignore = "needs live mssql"]
fn guarded_drops_follow_the_table_uid_through_a_rename_on_sql_server() {
    let own = OwnDatabase::new(&server(), "require2400_rename_ms");
    let connection = own.connection();
    let d = setup(connection, "require2400_rename_ms");
    on_server(connection, "INSERT dbo.t VALUES(1,2,199)");
    record(&d, connection, PREDICATE);
    d.table(&AFTER.replace("dbo.t", "dbo.renamed"));
    ok(d.run(&["rename-table", "dbo.t", "dbo.renamed"]));
    let path = plan(&d, connection);
    let refused = apply(&d, connection, &path);
    assert_ne!(code(&refused), 0);
    assert!(
        stderr(&refused).contains("1 row(s) of dbo.renamed violate"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT COUNT_BIG(*) FROM sys.tables WHERE name=N't'"
        ),
        1
    );
    assert_eq!(
        scalar(
            connection,
            "SELECT COUNT_BIG(*) FROM sys.tables WHERE name=N'renamed'"
        ),
        0
    );
    on_server(connection, "UPDATE dbo.t SET cents=amount*100");
    ok(apply(&d, connection, &path));
    assert_eq!(
        scalar(
            connection,
            "SELECT COUNT_BIG(*) FROM sys.columns WHERE object_id=OBJECT_ID(N'dbo.renamed') AND name=N'amount'"
        ),
        0
    );
}
