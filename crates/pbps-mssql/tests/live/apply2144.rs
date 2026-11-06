//! #2144: the SQL Server captures an apply runs inside its own transaction —
//! the opening recheck before the first statement and the closing capture
//! after the last — measured on a real server (DEC-2144.1). Each test states
//! what the engine does, with its negative case.

use super::*;
use pbps_mssql::resolver::capture::{self, CaptureError, CaptureScope};
use std::collections::BTreeSet;

const APPLY_FRAMING: &str = "SET XACT_ABORT ON; BEGIN TRANSACTION;";

async fn run(conn: &mut Conn, batches: &[&str]) {
    for batch in batches {
        conn.execute(batch)
            .await
            .unwrap_or_else(|e| panic!("the engine rejected:\n{batch}\n{e}"));
    }
}

async fn session(db: &TestDb) -> Conn {
    let mut conn = connect_live(&conn_str()).await.expect("connect");
    conn.execute(&format!("USE [{}];", db.name)).await.unwrap();
    conn
}

async fn number(conn: &mut Conn, sql: &str) -> i64 {
    conn.query(sql).await.unwrap()[0]
        .try_get_at::<i64>(0)
        .unwrap()
        .unwrap()
}

/// The planning side: capture `roots` and seal them under the test's key,
/// the way the producer seals a plan's evidence.
async fn seal(conn: &mut Conn, roots: &[(&str, &str)]) -> pbps_model::resolver::InputManifest {
    let scope = CaptureScope {
        retained: roots.iter().map(|(s, n)| capture::object(s, n)).collect(),
        candidates: BTreeSet::new(),
    };
    let first = capture::capture(conn, &scope, &crate::capture_key())
        .await
        .expect("capture");
    let schema = pbps_model::Schema::default();
    let ids = pbps_model::IdsFile::default();
    let namespaces = BTreeSet::from(["app".to_owned()]);
    let recorded = capture::RecordedOwnership {
        schema: &schema,
        ids: &ids,
        namespaces: &namespaces,
    };
    capture::capture_qualified(
        conn,
        &first.runtime_scope(),
        &crate::capture_key(),
        &recorded,
    )
    .await
    .expect("capture_qualified")
    .1
}

async fn opening(conn: &mut Conn, sealed: &pbps_model::resolver::InputManifest) -> Vec<String> {
    let key = crate::capture_key();
    let fresh = capture::recheck_opening(conn, sealed, &Default::default(), &key)
        .await
        .expect("the opening recheck reads");
    sealed.opening_differences(&fresh)
}

async fn closing(conn: &mut Conn, sealed: &pbps_model::resolver::InputManifest) -> Vec<String> {
    let schema = pbps_model::Schema::default();
    let ids = pbps_model::IdsFile::default();
    let namespaces = BTreeSet::from(["app".to_owned()]);
    let recorded = capture::RecordedOwnership {
        schema: &schema,
        ids: &ids,
        namespaces: &namespaces,
    };
    let key = crate::capture_key();
    let fresh = capture::capture_closing(
        conn,
        sealed,
        &sealed.vanishable(),
        &Default::default(),
        &key,
        &recorded,
    )
    .await
    .expect("the closing capture reads");
    sealed.closing_differences(&fresh, &[])
}

/// The opening recheck needs nothing but the sealed manifest and the plan's
/// declarations, and on a target that did not move it finds nothing, in the
/// apply's own framing on its own connection. A name another session brings
/// into the scope, or a lasting change to a captured column, is found; one
/// made and restored is not (DEC-619.1).
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn the_opening_recheck_in_the_apply_transaction_finds_only_a_lasting_change() {
    let mut db = TestDb::create("open2144").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA app;",
            "CREATE TABLE dbo.price (v int NOT NULL);",
            "CREATE TABLE app.t (id int NOT NULL PRIMARY KEY, v decimal(10,2) NOT NULL);",
            "CREATE VIEW app.vw AS SELECT id, v FROM app.t;",
            "CREATE VIEW app.one_part AS SELECT v FROM price;",
        ],
    )
    .await;
    let sealed = seal(&mut db.conn, &[("app", "vw"), ("app", "one_part")]).await;
    let mut apply = session(&db).await;
    let mut other = session(&db).await;
    run(&mut apply, &[APPLY_FRAMING]).await;
    assert_eq!(
        number(&mut apply, "SELECT CONVERT(bigint, @@TRANCOUNT);").await,
        1
    );
    assert_eq!(
        opening(&mut apply, &sealed).await,
        Vec::<String>::new(),
        "a target that did not move reads as sealed, in the apply's transaction"
    );

    // Made and restored between the seal and the read: nothing lasting.
    run(
        &mut other,
        &["CREATE TABLE app.price (v int);", "DROP TABLE app.price;"],
    )
    .await;
    assert_eq!(opening(&mut apply, &sealed).await, Vec::<String>::new());

    // A table the one-part name now resolves to first, in the view's own
    // schema: found, naming the candidate set it arrived in.
    run(&mut other, &["CREATE TABLE app.price (v int);"]).await;
    let arrived = opening(&mut apply, &sealed).await;
    assert!(
        arrived.iter().any(|d| d == "sys.objects app.price arrived"),
        "a name that captures a runtime-bound reference is found: {arrived:?}"
    );
    run(&mut other, &["DROP TABLE app.price;"]).await;
    assert_eq!(opening(&mut apply, &sealed).await, Vec::<String>::new());

    // A lasting change to a column the view binds: found.
    run(
        &mut other,
        &["ALTER TABLE app.t ALTER COLUMN v decimal(12,4) NOT NULL;"],
    )
    .await;
    let changed = opening(&mut apply, &sealed).await;
    assert!(
        changed.iter().any(|d| d == "sys.columns v(app.t) changed"),
        "a lasting change to a captured column is found: {changed:?}"
    );
    run(&mut apply, &["ROLLBACK;"]).await;
    drop(other);
    db.drop().await;
}

/// The same recheck as another principal differs in the session's inputs: an
/// apply that is not run by the principal that planned is caught here and by
/// the authorization check, not read as unchanged.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_different_deployer_differs_at_the_opening_recheck() {
    let mut db = TestDb::create("who2144").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA app;",
            "CREATE TABLE app.t (id int NOT NULL);",
            "CREATE VIEW app.vw AS SELECT id FROM app.t;",
        ],
    )
    .await;
    let sealed = seal(&mut db.conn, &[("app", "vw")]).await;
    let login = format!("who2144_{}", std::process::id());
    let password = "Pbps!Who2144x";
    run(
        &mut db.conn,
        &[
            &format!("CREATE LOGIN [{login}] WITH PASSWORD = '{password}', CHECK_POLICY = OFF;"),
            &format!("CREATE USER [{login}] FOR LOGIN [{login}];"),
            &format!("GRANT VIEW DEFINITION TO [{login}];"),
            &format!("GRANT SELECT ON sys.sql_expression_dependencies TO [{login}];"),
        ],
    )
    .await;
    let url = super::resolver_tests::login_url(&login, password, &db.name);
    let mut as_user = connect_live(&url).await.expect("connect as the deployer");
    let found = opening(&mut as_user, &sealed).await;
    assert!(
        found.iter().any(|d| d.contains("session")),
        "another principal differs in the session's roles and settings: {found:?}"
    );
    drop(as_user);
    drop_login_when_idle(&mut db.conn, &login).await.ok();
    db.drop().await;
}

/// A deployer without both grants is refused by name, and the remedy names
/// both statements, so one fix is enough; with both it reads (measured: schema
/// `CONTROL` alone reads neither).
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_deployer_missing_the_dependency_grants_is_refused_with_both_named() {
    let mut db = TestDb::create("grants2144").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA app;",
            "CREATE TABLE app.t (id int NOT NULL);",
            "CREATE VIEW app.vw AS SELECT id FROM app.t;",
        ],
    )
    .await;
    let sealed = seal(&mut db.conn, &[("app", "vw")]).await;
    let login = format!("grants2144_{}", std::process::id());
    let password = "Pbps!Grant2144x";
    run(
        &mut db.conn,
        &[
            &format!("CREATE LOGIN [{login}] WITH PASSWORD = '{password}', CHECK_POLICY = OFF;"),
            &format!("CREATE USER [{login}] FOR LOGIN [{login}];"),
            &format!("GRANT CONTROL ON SCHEMA::app TO [{login}];"),
        ],
    )
    .await;
    let url = super::resolver_tests::login_url(&login, password, &db.name);
    let key = crate::capture_key();
    for (step, grant) in [
        ("schema CONTROL only", None),
        (
            "plus VIEW DEFINITION on the database",
            Some(format!("GRANT VIEW DEFINITION TO [{login}];")),
        ),
    ] {
        if let Some(grant) = grant {
            run(&mut db.conn, &[&grant]).await;
        }
        let mut as_user = connect_live(&url).await.expect("connect as the deployer");
        let refusal = capture::recheck_opening(&mut as_user, &sealed, &Default::default(), &key)
            .await
            .expect_err(step);
        let text = refusal.to_string();
        assert!(
            text.contains("GRANT VIEW DEFINITION TO [deployer]")
                && text.contains("GRANT SELECT ON sys.sql_expression_dependencies TO [deployer]"),
            "{step}: {text}"
        );
    }
    run(
        &mut db.conn,
        &[&format!(
            "GRANT SELECT ON sys.sql_expression_dependencies TO [{login}];"
        )],
    )
    .await;
    let mut as_user = connect_live(&url).await.expect("connect as the deployer");
    capture::recheck_opening(&mut as_user, &sealed, &Default::default(), &key)
        .await
        .expect("both grants read");
    drop(as_user);
    drop_login_when_idle(&mut db.conn, &login).await.ok();
    db.drop().await;
}

/// What the plan installs under names the engine chose afresh reads equal to
/// what scratch made, so no name special case is needed; a changed property
/// still shows (the control).
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn the_unnamed_objects_a_plan_installs_compare_equal_to_scratchs() {
    let mut scratch = TestDb::create("scr2144").await;
    let db = TestDb::create("tgt2144").await;
    let ddl = [
        "CREATE SCHEMA app;",
        "CREATE TABLE app.p (id int NOT NULL PRIMARY KEY, u int NOT NULL UNIQUE);",
        "CREATE TABLE app.c (id int NOT NULL PRIMARY KEY, pid int NOT NULL REFERENCES app.p (id), v int NOT NULL DEFAULT 5, CHECK (v >= 0));",
        "CREATE VIEW app.vw AS SELECT c.id, p.u FROM app.c c JOIN app.p p ON p.id = c.pid;",
    ];
    run(&mut scratch.conn, &ddl).await;
    let sealed = seal(
        &mut scratch.conn,
        &[("app", "vw"), ("app", "c"), ("app", "p")],
    )
    .await;
    let mut apply = session(&db).await;
    run(&mut apply, &[APPLY_FRAMING]).await;
    run(&mut apply, &ddl).await;
    assert_eq!(
        closing(&mut apply, &sealed).await,
        Vec::<String>::new(),
        "the PK, UNIQUE, FK, DEFAULT and CHECK the target named afresh read as scratch's"
    );
    run(
        &mut apply,
        &[
            "DECLARE @n sysname = (SELECT name FROM sys.default_constraints WHERE parent_object_id = OBJECT_ID(N'app.c')); EXEC(N'ALTER TABLE app.c DROP CONSTRAINT ' + @n);",
            "ALTER TABLE app.c ADD DEFAULT 6 FOR v;",
        ],
    )
    .await;
    let found = closing(&mut apply, &sealed).await;
    assert!(
        found
            .iter()
            .any(|d| d.contains("DEFAULT") && d.contains("changed")),
        "a changed DEFAULT still shows: {found:?}"
    );
    run(&mut apply, &["ROLLBACK;"]).await;
    scratch.drop().await;
    db.drop().await;
}

/// The closing capture sees another session's committed change to what it
/// reads, and the plan's own DDL; the target is left as it was by a rollback.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn the_closing_capture_finds_another_writers_lasting_change_and_not_a_restored_one() {
    let mut db = TestDb::create("close2144").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA app;",
            "CREATE TABLE app.t (id int NOT NULL, v int NOT NULL);",
            "CREATE VIEW app.vw AS SELECT id FROM app.t;",
        ],
    )
    .await;
    let sealed = seal(&mut db.conn, &[("app", "vw")]).await;
    let mut apply = session(&db).await;
    let mut other = session(&db).await;
    run(&mut apply, &[APPLY_FRAMING]).await;
    run(&mut apply, &["CREATE TABLE app.made_by_the_plan (i int);"]).await;
    assert_eq!(closing(&mut apply, &sealed).await, Vec::<String>::new());

    run(
        &mut other,
        &[
            "ALTER TABLE app.t ADD w int NULL;",
            "ALTER TABLE app.t DROP COLUMN w;",
        ],
    )
    .await;
    assert_eq!(
        closing(&mut apply, &sealed).await,
        Vec::<String>::new(),
        "a column added and dropped leaves nothing to find"
    );
    run(
        &mut other,
        &["ALTER TABLE app.t ALTER COLUMN v bigint NOT NULL;"],
    )
    .await;
    let found = closing(&mut apply, &sealed).await;
    assert!(
        !found.is_empty(),
        "a lasting change after the last statement is found"
    );
    run(&mut apply, &["ROLLBACK;"]).await;
    assert_eq!(
        number(
            &mut db.conn,
            "SELECT CONVERT(bigint, COUNT(*)) FROM sys.objects WHERE name = N'made_by_the_plan';"
        )
        .await,
        0,
        "a rollback leaves nothing of the plan"
    );
    drop(other);
    db.drop().await;
}

/// A catalog read the engine refuses keeps the engine's number and text in
/// the refusal. A lock timeout under another session's uncommitted DDL is the
/// case measured here; the engine rolls the transaction back itself under
/// `XACT_ABORT`, and the framing's rollback tolerates that.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn an_engine_refusal_during_the_read_keeps_its_number_and_text() {
    let mut db = TestDb::create("engine2144").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA app;",
            "CREATE TABLE app.t (id int NOT NULL);",
            "CREATE VIEW app.vw AS SELECT id FROM app.t;",
        ],
    )
    .await;
    let sealed = seal(&mut db.conn, &[("app", "vw")]).await;
    let mut apply = session(&db).await;
    let mut other = session(&db).await;
    run(
        &mut apply,
        &[APPLY_FRAMING, "CREATE TABLE app.made_by_apply (i int);"],
    )
    .await;
    run(
        &mut other,
        &["BEGIN TRANSACTION; CREATE TABLE app.other_uncommitted (i int);"],
    )
    .await;
    run(&mut apply, &["SET LOCK_TIMEOUT 1000;"]).await;
    let key = crate::capture_key();
    let refusal = capture::recheck_opening(&mut apply, &sealed, &Default::default(), &key)
        .await
        .expect_err("another session's open DDL holds the read");
    let CaptureError::Engine { number, text } = &refusal else {
        panic!("the engine's refusal was reduced to {refusal:?}");
    };
    assert_eq!(number.as_deref(), Some("1222"), "{text}");
    assert!(text.contains("Lock request time out"), "{text}");
    let shown = refusal.to_string();
    assert!(
        shown.contains("1222") && shown.contains("Lock request time out"),
        "{shown}"
    );
    // Under XACT_ABORT the engine has already rolled the transaction back, and
    // the framing's guarded rollback is the one that tolerates it.
    assert_eq!(number_or_zero(&mut apply).await, 0);
    run(&mut apply, &["IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;"]).await;
    run(&mut other, &["ROLLBACK;"]).await;
    drop(other);
    db.drop().await;
}

async fn number_or_zero(conn: &mut Conn) -> i64 {
    number(conn, "SELECT CONVERT(bigint, @@TRANCOUNT);").await
}

/// With no lock timeout set, the read waits for another session's uncommitted
/// DDL in the scope and then reads what it committed. This is the same wait
/// the apply's own DDL has on that session's locks; the deployment lock
/// already serialises pbps deployers, so only a session outside pbps can hold
/// it (DEC-2144.1).
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_read_waits_for_another_sessions_uncommitted_ddl_and_then_reads() {
    let mut db = TestDb::create("wait2144").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA app;",
            "CREATE TABLE app.t (id int NOT NULL);",
            "CREATE VIEW app.vw AS SELECT id FROM app.t;",
        ],
    )
    .await;
    let sealed = seal(&mut db.conn, &[("app", "vw")]).await;
    let mut apply = session(&db).await;
    let mut other = session(&db).await;
    run(&mut apply, &[APPLY_FRAMING]).await;
    run(
        &mut other,
        &["BEGIN TRANSACTION; CREATE TABLE app.t2 (i int);"],
    )
    .await;
    let key = crate::capture_key();
    let declarations = Default::default();
    let started = std::time::Instant::now();
    let read = capture::recheck_opening(&mut apply, &sealed, &declarations, &key);
    let commit = async {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        other.execute("COMMIT;").await.unwrap();
        other
    };
    let (fresh, _other) = tokio::join!(read, commit);
    fresh.expect("the read finishes once the other session commits");
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(3),
        "the read did not wait for the open DDL: {:?}",
        started.elapsed()
    );
    run(&mut apply, &["ROLLBACK;"]).await;
    db.drop().await;
}
