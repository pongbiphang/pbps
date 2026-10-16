//! #619: what SQL Server binds at creation, how it resolves names, and what
//! keeps a catalog read consistent — measured, not inferred from PostgreSQL.
//!
//! These tests pin the claims `docs/RESOLVER-MSSQL.md` and DEC-619.1–619.4
//! rest on. None enables a binding adapter (#620); each is a measurement with
//! its positive and its negative case, so a later build that behaves
//! differently fails here before the adapter believes the old answer.

use super::*;

/// The first column of the first row as text; SQL NULL reads as `NULL`.
async fn text(conn: &mut Conn, sql: &str) -> String {
    let rows = conn
        .query(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}\n{e}"));
    rows.first()
        .and_then(|row| row.try_get_at::<&str>(0).unwrap().map(str::to_owned))
        .unwrap_or_else(|| "NULL".into())
}

/// The first column of the first row, which the query converts to `bigint`.
async fn number(conn: &mut Conn, sql: &str) -> i64 {
    conn.query(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}\n{e}"))[0]
        .try_get_at::<i64>(0)
        .unwrap()
        .unwrap()
}

/// Every row's first column as text.
async fn texts(conn: &mut Conn, sql: &str) -> Vec<String> {
    conn.query(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}\n{e}"))
        .iter()
        .map(|row| {
            row.try_get_at::<&str>(0)
                .unwrap()
                .unwrap_or("NULL")
                .to_owned()
        })
        .collect()
}

/// Runs each batch in turn; a batch the engine refuses fails the test.
async fn run(conn: &mut Conn, batches: &[&str]) {
    for batch in batches {
        conn.execute(batch)
            .await
            .unwrap_or_else(|e| panic!("the engine rejected:\n{batch}\n{e}"));
    }
}

/// The engine's error number for a batch it refuses, or `None` if it ran.
async fn refusal(conn: &mut Conn, sql: &str) -> Option<String> {
    match conn.execute(sql).await {
        Ok(()) => None,
        Err(e) => Some(
            e.server_error_code()
                .unwrap_or_else(|| panic!("{sql} failed without an engine error: {e}")),
        ),
    }
}

/// Another session in the test's database.
async fn session(db: &TestDb) -> Conn {
    let mut conn = connect_live(&conn_str()).await.expect("connect");
    conn.execute(&format!("USE [{}];", db.name)).await.unwrap();
    conn
}

/// Drops a login once its sessions have ended. A closed connection's session
/// ends asynchronously, and `DROP LOGIN` refuses a login still logged in
/// (15434), so the wait is for the engine, not for the client.
async fn drop_login(admin: &mut Conn, login: &str) {
    let open = format!(
        "SELECT CONVERT(bigint, COUNT(*)) FROM sys.dm_exec_sessions WHERE login_name = N'{login}';"
    );
    for _ in 0..100 {
        if number(admin, &open).await == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    run(admin, &[&*format!("DROP LOGIN [{login}];")]).await;
}

/// What each object reference of `referencing` names when the catalog is
/// read, as `schema.name`; an unresolved one as `unresolved:name`.
async fn bindings(conn: &mut Conn, referencing: &str) -> Vec<String> {
    let mut names = texts(
        conn,
        &format!(
            "SELECT CONVERT(nvarchar(300), COALESCE(OBJECT_SCHEMA_NAME(d.referenced_id) + N'.' \
                 + OBJECT_NAME(d.referenced_id), N'unresolved:' + d.referenced_entity_name)) \
             FROM sys.sql_expression_dependencies d \
             WHERE d.referencing_id = OBJECT_ID(N'{referencing}') AND d.referenced_class = 1 \
               AND (d.referenced_id IS NULL OR d.referenced_id <> d.referencing_id) \
             ORDER BY 1;"
        ),
    )
    .await;
    names.dedup();
    names
}

/// The motivating pair (#619 scope 2). A module body's one-part name is
/// resolved in the module's own schema and then `dbo`, when it runs: an
/// object arriving in that schema changes what an unchanged view reads, with
/// no DDL on the view. An arrival anywhere else, or one only two-part
/// callers could name, changes nothing; nor does a schema-bound reference, a
/// computed column's, or the compatibility level.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn an_arrival_in_a_modules_own_schema_rebinds_its_one_part_names_and_no_other_arrival_does() {
    let mut db = TestDb::create("arrival619").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA app;",
            "CREATE SCHEMA other;",
            "CREATE TABLE dbo.price (v int); INSERT dbo.price VALUES (1);",
            "CREATE VIEW app.one_part AS SELECT v FROM price;",
            "CREATE VIEW app.two_part AS SELECT v FROM dbo.price;",
            "CREATE VIEW app.bound WITH SCHEMABINDING AS SELECT v FROM dbo.price;",
            "CREATE PROCEDURE app.p AS SELECT v FROM price;",
            "CREATE FUNCTION dbo.fmt (@v int) RETURNS int AS BEGIN RETURN @v END;",
            "CREATE TABLE app.t (v int, d AS dbo.fmt(v));",
        ],
    )
    .await;
    let views = ["app.one_part", "app.two_part", "app.bound", "app.p"];
    for view in views {
        assert_eq!(bindings(&mut db.conn, view).await, ["dbo.price"], "{view}");
    }

    run(
        &mut db.conn,
        &[
            "CREATE TABLE other.price (v int);",
            "CREATE FUNCTION app.fmt (@v int) RETURNS int AS BEGIN RETURN 99 END;",
        ],
    )
    .await;
    for view in views {
        assert_eq!(
            bindings(&mut db.conn, view).await,
            ["dbo.price"],
            "an arrival off the path moved {view}"
        );
    }
    assert_eq!(bindings(&mut db.conn, "app.t").await, ["dbo.fmt"]);

    run(
        &mut db.conn,
        &["CREATE TABLE app.price (v int); INSERT app.price VALUES (2);"],
    )
    .await;
    assert_eq!(bindings(&mut db.conn, "app.one_part").await, ["app.price"]);
    assert_eq!(bindings(&mut db.conn, "app.p").await, ["app.price"]);
    assert_eq!(bindings(&mut db.conn, "app.two_part").await, ["dbo.price"]);
    assert_eq!(bindings(&mut db.conn, "app.bound").await, ["dbo.price"]);
    assert_eq!(bindings(&mut db.conn, "app.t").await, ["dbo.fmt"]);
    assert_eq!(
        number(&mut db.conn, "SELECT CONVERT(bigint, v) FROM app.one_part;").await,
        2
    );
    assert_eq!(
        number(&mut db.conn, "SELECT CONVERT(bigint, v) FROM app.two_part;").await,
        1
    );

    // Every kind of object occupies the name: a function arriving under it
    // breaks the view rather than being passed over for `dbo.price`.
    run(
        &mut db.conn,
        &[
            "DROP TABLE app.price;",
            "CREATE FUNCTION app.price () RETURNS int AS BEGIN RETURN 1 END;",
        ],
    )
    .await;
    let broken = refusal(&mut db.conn, "SELECT v FROM app.one_part;").await;
    assert!(
        matches!(broken.as_deref(), Some("208" | "4413")),
        "{broken:?}"
    );
    run(&mut db.conn, &["DROP FUNCTION app.price;"]).await;

    run(
        &mut db.conn,
        &[&*format!(
            "ALTER DATABASE [{}] SET COMPATIBILITY_LEVEL = 130;",
            db.name
        )],
    )
    .await;
    for view in views {
        assert_eq!(
            bindings(&mut db.conn, view).await,
            ["dbo.price"],
            "the compatibility level moved {view}"
        );
    }
    db.drop().await;
}

/// Inside a module a one-part name looks in the module's schema, whoever
/// runs it and however the module executes. Outside one — in the statement
/// that creates a table or a module — it looks in the executing user's
/// default schema, so scratch has to reproduce that schema (#611 reads it).
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_modules_one_part_names_resolve_in_its_schema_and_a_statements_in_the_users() {
    let mut db = TestDb::create("names619").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA a;",
            "CREATE TABLE dbo.t (which varchar(10)); INSERT dbo.t VALUES ('dbo.t');",
            "CREATE TABLE a.t (which varchar(10)); INSERT a.t VALUES ('a.t');",
            "CREATE VIEW dbo.v AS SELECT which FROM t;",
            "CREATE VIEW a.v AS SELECT which FROM t;",
            "CREATE PROCEDURE dbo.p WITH EXECUTE AS OWNER AS SELECT which FROM t;",
            "CREATE TYPE a.amount FROM decimal(10, 2);",
            "CREATE TYPE dbo.amount FROM decimal(12, 4);",
            "CREATE USER reader WITHOUT LOGIN WITH DEFAULT_SCHEMA = a;",
            "ALTER ROLE db_owner ADD MEMBER reader;",
        ],
    )
    .await;
    let read_both =
        "SELECT CONVERT(nvarchar(30), (SELECT which FROM dbo.v) + N'|' + (SELECT which FROM a.v));";
    assert_eq!(text(&mut db.conn, read_both).await, "dbo.t|a.t");
    run(&mut db.conn, &["EXECUTE AS USER = 'reader';"]).await;
    assert_eq!(
        text(&mut db.conn, read_both).await,
        "dbo.t|a.t",
        "the caller's default schema reached a module body"
    );
    run(
        &mut db.conn,
        &[
            "CREATE TABLE dbo.typed (v amount);",
            "CREATE TABLE unqualified (i int);",
            "CREATE VIEW dbo.made_by_reader AS SELECT which FROM t;",
            "REVERT;",
        ],
    )
    .await;
    let column_type = "SELECT CONVERT(nvarchar(300), SCHEMA_NAME(ty.schema_id) + N'.' + ty.name) \
                       FROM sys.columns c JOIN sys.types ty ON ty.user_type_id = c.user_type_id \
                       WHERE c.object_id = OBJECT_ID(N'{}');";
    assert_eq!(
        text(&mut db.conn, &column_type.replace("{}", "dbo.typed")).await,
        "a.amount"
    );
    assert_eq!(
        text(
            &mut db.conn,
            "SELECT CONVERT(nvarchar(128), SCHEMA_NAME(schema_id)) FROM sys.objects WHERE name = N'unqualified';"
        )
        .await,
        "a"
    );
    assert_eq!(
        bindings(&mut db.conn, "dbo.made_by_reader").await,
        ["dbo.t"]
    );
    // The negative: the same statement as a user whose default schema is dbo.
    run(&mut db.conn, &["CREATE TABLE dbo.typed_by_dbo (v amount);"]).await;
    assert_eq!(
        text(&mut db.conn, &column_type.replace("{}", "dbo.typed_by_dbo")).await,
        "dbo.amount"
    );
    db.drop().await;
}

/// The dependency catalog stores no creation-time binding for a one-part
/// name: `referenced_id` is resolved when the view is read, so an arrival
/// moves it without any compile and a drop moves it back. A capture reads
/// the current resolution, never a record of the past one.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn the_dependency_catalog_resolves_a_one_part_name_when_it_is_read() {
    let mut db = TestDb::create("depread619").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA a;",
            "CREATE TABLE dbo.t (x int);",
            "CREATE VIEW a.v AS SELECT x FROM t;",
        ],
    )
    .await;
    assert_eq!(bindings(&mut db.conn, "a.v").await, ["dbo.t"]);
    run(&mut db.conn, &["CREATE TABLE a.t (x int);"]).await;
    assert_eq!(bindings(&mut db.conn, "a.v").await, ["a.t"]);
    assert_eq!(
        text(
            &mut db.conn,
            "SELECT COALESCE(referenced_schema_name, N'NULL') FROM sys.dm_sql_referenced_entities(N'a.v', N'OBJECT') WHERE referenced_minor_id = 0;"
        )
        .await,
        "NULL",
        "the reference itself names no schema"
    );
    run(&mut db.conn, &["DROP TABLE a.t;"]).await;
    assert_eq!(bindings(&mut db.conn, "a.v").await, ["dbo.t"]);
    db.drop().await;
}

/// A schema-bound reference is recorded by id and the engine holds it: the
/// referenced table's column cannot be altered nor the table dropped, and a
/// function a DEFAULT, CHECK or computed column calls cannot be dropped or
/// altered. A plain view holds nothing. A schema-bound module may only call
/// schema-bound functions, which the externals comparison must therefore
/// read.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn schema_bound_references_are_held_by_the_engine_and_plain_ones_are_not() {
    let mut db = TestDb::create("schemabound619").await;
    run(
        &mut db.conn,
        &[
            "CREATE TABLE dbo.t (c int NOT NULL, w varchar(10));",
            "CREATE TABLE dbo.loose (c int);",
            "CREATE VIEW dbo.plain AS SELECT c FROM dbo.loose;",
            "CREATE VIEW dbo.bound WITH SCHEMABINDING AS SELECT c, w FROM dbo.t;",
            "CREATE FUNCTION dbo.f (@x int) RETURNS int AS BEGIN RETURN @x END;",
            "CREATE TABLE dbo.u (c int, cc AS dbo.f(c), d int CONSTRAINT df_u DEFAULT dbo.f(1), \
             CONSTRAINT ck_u CHECK (dbo.f(c) > 0));",
        ],
    )
    .await;
    let schema_bound = "SELECT CONVERT(bigint, COUNT(*)) FROM sys.sql_expression_dependencies \
                        WHERE referencing_id = OBJECT_ID(N'{}') AND is_schema_bound_reference = 1 \
                        AND referenced_id IS NOT NULL;";
    assert!(number(&mut db.conn, &schema_bound.replace("{}", "dbo.bound")).await > 0);
    assert!(number(&mut db.conn, &schema_bound.replace("{}", "dbo.u")).await > 0);
    assert_eq!(
        number(&mut db.conn, &schema_bound.replace("{}", "dbo.plain")).await,
        0
    );

    for (sql, code) in [
        ("ALTER TABLE dbo.t ALTER COLUMN w varchar(20);", "5074"),
        ("DROP TABLE dbo.t;", "3729"),
        ("DROP FUNCTION dbo.f;", "3729"),
        (
            "ALTER FUNCTION dbo.f (@x int) RETURNS int AS BEGIN RETURN @x + 1 END;",
            "3729",
        ),
        (
            "CREATE VIEW dbo.bound_over_f WITH SCHEMABINDING AS SELECT dbo.f(c) AS fc FROM dbo.t;",
            "4513",
        ),
    ] {
        assert_eq!(
            refusal(&mut db.conn, sql).await.as_deref(),
            Some(code),
            "{sql}"
        );
    }
    assert_eq!(refusal(&mut db.conn, "DROP TABLE dbo.loose;").await, None);
    db.drop().await;
}

/// `QUOTED_IDENTIFIER` and `ANSI_NULLS` are fixed when a module is created
/// and recorded on it; the session that later runs the module does not
/// change its meaning. Both belong in a module's fingerprint.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_modules_set_options_are_fixed_when_it_is_created() {
    let mut db = TestDb::create("setoptions619").await;
    run(
        &mut db.conn,
        &[
            "SET QUOTED_IDENTIFIER OFF;",
            "CREATE VIEW dbo.quoted_off AS SELECT \"literal\" AS c;",
            "SET QUOTED_IDENTIFIER ON;",
            "SET ANSI_NULLS OFF;",
            "CREATE FUNCTION dbo.nulls_off () RETURNS int AS BEGIN RETURN CASE WHEN NULL = NULL THEN 1 ELSE 0 END END;",
            "SET ANSI_NULLS ON;",
            "CREATE FUNCTION dbo.nulls_on () RETURNS int AS BEGIN RETURN CASE WHEN NULL = NULL THEN 1 ELSE 0 END END;",
        ],
    )
    .await;
    assert_eq!(
        text(&mut db.conn, "SELECT c FROM dbo.quoted_off;").await,
        "literal"
    );
    assert_eq!(
        number(&mut db.conn, "SELECT CONVERT(bigint, dbo.nulls_off());").await,
        1
    );
    assert_eq!(
        number(&mut db.conn, "SELECT CONVERT(bigint, dbo.nulls_on());").await,
        0
    );
    assert_eq!(
        texts(
            &mut db.conn,
            "SELECT CONVERT(nvarchar(200), OBJECT_NAME(object_id) + N':' + CONVERT(nchar(1), uses_ansi_nulls) \
                 + CONVERT(nchar(1), uses_quoted_identifier)) \
             FROM sys.sql_modules ORDER BY 1;"
        )
        .await,
        ["nulls_off:01", "nulls_on:11", "quoted_off:10"]
    );
    // The negative: with the option on, the same text is an identifier.
    assert_eq!(
        refusal(
            &mut db.conn,
            "CREATE VIEW dbo.quoted_on AS SELECT \"literal\" AS c;"
        )
        .await
        .as_deref(),
        Some("207")
    );
    db.drop().await;
}

/// A synonym is bound by name: a view names the synonym, and retargeting it
/// changes what the view reads with no DDL on the view. A three-part name is
/// recorded with its database and no id, and refused at creation when the
/// object is missing. A one-part built-in is never shadowed by a function of
/// the same name. A type in a module's parameter resolves in the module's
/// schema, then `dbo`, and is stored by id.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn synonyms_and_other_databases_are_named_and_built_ins_are_never_shadowed() {
    let mut db = TestDb::create("names2_619").await;
    let far = format!("far619_{}", std::process::id());
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA a;",
            "CREATE TABLE dbo.first_base (x int); INSERT dbo.first_base VALUES (1);",
            "CREATE TABLE dbo.second_base (x int); INSERT dbo.second_base VALUES (2);",
            "CREATE SYNONYM a.syn FOR dbo.first_base;",
            "CREATE VIEW a.through_synonym AS SELECT x FROM syn;",
            "CREATE FUNCTION a.len (@s nvarchar(10)) RETURNS int AS BEGIN RETURN 999 END;",
            "CREATE VIEW a.uses_len AS SELECT len(N'abc') AS l;",
            "CREATE TYPE a.amount FROM decimal(10, 2);",
            "CREATE TYPE dbo.amount FROM decimal(12, 4);",
            "CREATE FUNCTION a.takes_amount (@v amount) RETURNS int AS BEGIN RETURN 1 END;",
            "CREATE FUNCTION dbo.takes_amount (@v amount) RETURNS int AS BEGIN RETURN 1 END;",
            &*format!("CREATE TABLE tempdb.dbo.{far} (x int);"),
            &*format!("CREATE VIEW a.across AS SELECT x FROM tempdb.dbo.{far};"),
        ],
    )
    .await;
    assert_eq!(bindings(&mut db.conn, "a.through_synonym").await, ["a.syn"]);
    assert_eq!(
        number(
            &mut db.conn,
            "SELECT CONVERT(bigint, x) FROM a.through_synonym;"
        )
        .await,
        1
    );
    run(
        &mut db.conn,
        &[
            "DROP SYNONYM a.syn;",
            "CREATE SYNONYM a.syn FOR dbo.second_base;",
        ],
    )
    .await;
    assert_eq!(
        number(
            &mut db.conn,
            "SELECT CONVERT(bigint, x) FROM a.through_synonym;"
        )
        .await,
        2
    );

    assert_eq!(
        number(&mut db.conn, "SELECT CONVERT(bigint, l) FROM a.uses_len;").await,
        3
    );
    assert_eq!(
        texts(
            &mut db.conn,
            "SELECT CONVERT(nvarchar(300), OBJECT_SCHEMA_NAME(p.object_id) + N'.' + OBJECT_NAME(p.object_id) \
                 + N'->' + SCHEMA_NAME(t.schema_id) + N'.' + t.name COLLATE DATABASE_DEFAULT) \
             FROM sys.parameters p JOIN sys.types t ON t.user_type_id = p.user_type_id \
             WHERE t.is_user_defined = 1 ORDER BY 1;"
        )
        .await,
        ["a.takes_amount->a.amount", "dbo.takes_amount->dbo.amount"]
    );

    assert_eq!(
        text(
            &mut db.conn,
            "SELECT CONVERT(nvarchar(300), COALESCE(referenced_database_name, N'-') + N'|' \
                 + COALESCE(CONVERT(nvarchar(20), referenced_id), N'NULL')) \
             FROM sys.sql_expression_dependencies WHERE referencing_id = OBJECT_ID(N'a.across');"
        )
        .await,
        "tempdb|NULL"
    );
    assert_eq!(
        refusal(
            &mut db.conn,
            "CREATE VIEW a.across_missing AS SELECT x FROM tempdb.dbo.no_such_table_619;"
        )
        .await
        .as_deref(),
        Some("208")
    );
    run(&mut db.conn, &[&*format!("DROP TABLE tempdb.dbo.{far};")]).await;
    db.drop().await;
}

/// Catalog metadata is not versioned and no isolation level or hint holds
/// it: another session's DDL commits while a `SERIALIZABLE` transaction has
/// read the catalog with `HOLDLOCK`, and the transaction's next read sees
/// it; under `SNAPSHOT` the catalog views see it too and touching the new
/// object dooms the transaction (3961). The negative control: uncommitted
/// DDL does hold a catalog scan, so the waits above would have shown.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn no_isolation_level_or_hint_holds_the_catalog_against_another_session() {
    let mut db = TestDb::create("isolation619").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA a;",
            "CREATE TABLE a.t (x int);",
            "CREATE VIEW a.v AS SELECT x FROM a.t;",
            "CREATE USER u WITHOUT LOGIN;",
            &*format!(
                "ALTER DATABASE [{}] SET ALLOW_SNAPSHOT_ISOLATION ON;",
                db.name
            ),
        ],
    )
    .await;
    let count = "SELECT CONVERT(bigint, COUNT(*)) FROM sys.objects WITH (HOLDLOCK) WHERE schema_id = SCHEMA_ID(N'a');";
    let mut other = session(&db).await;
    run(&mut other, &["SET LOCK_TIMEOUT 2000;"]).await;

    run(
        &mut db.conn,
        &["SET TRANSACTION ISOLATION LEVEL SERIALIZABLE; BEGIN TRANSACTION;"],
    )
    .await;
    let before = number(&mut db.conn, count).await;
    for sql in [
        "SELECT CONVERT(bigint, COUNT(*)) FROM sys.sql_modules WITH (HOLDLOCK);",
        "SELECT CONVERT(bigint, COUNT(*)) FROM sys.database_permissions WITH (HOLDLOCK);",
        "SELECT CONVERT(bigint, COUNT(*)) FROM sys.database_role_members WITH (HOLDLOCK);",
        "SELECT CONVERT(bigint, COUNT(*)) FROM sys.sql_expression_dependencies WITH (HOLDLOCK);",
    ] {
        number(&mut db.conn, sql).await;
    }
    for sql in [
        "CREATE TABLE a.arrived (x int);",
        "ALTER VIEW a.v AS SELECT x FROM a.t WHERE 1 = 1;",
        "GRANT SELECT ON a.v TO u;",
        "ALTER ROLE db_datareader ADD MEMBER u;",
        "DROP TABLE a.arrived;",
        "CREATE TABLE a.arrived (x int);",
    ] {
        assert_eq!(refusal(&mut other, sql).await, None, "{sql} was held");
    }
    assert_eq!(number(&mut db.conn, count).await, before + 1);
    run(&mut db.conn, &["COMMIT;"]).await;

    run(
        &mut db.conn,
        &["SET TRANSACTION ISOLATION LEVEL SNAPSHOT; BEGIN TRANSACTION;"],
    )
    .await;
    number(&mut db.conn, "SELECT CONVERT(bigint, COUNT(*)) FROM a.t;").await;
    let before = number(&mut db.conn, count).await;
    run(&mut other, &["CREATE TABLE a.snapshot_arrival (x int);"]).await;
    assert_eq!(number(&mut db.conn, count).await, before + 1);
    assert_eq!(
        refusal(&mut db.conn, "SELECT x FROM a.snapshot_arrival;")
            .await
            .as_deref(),
        Some("3961")
    );
    run(
        &mut db.conn,
        &["IF @@TRANCOUNT > 0 ROLLBACK; SET TRANSACTION ISOLATION LEVEL READ COMMITTED;"],
    )
    .await;

    run(
        &mut db.conn,
        &[
            "BEGIN TRANSACTION;",
            "CREATE VIEW a.uncommitted AS SELECT x FROM a.t;",
        ],
    )
    .await;
    assert_eq!(
        refusal(
            &mut other,
            "SELECT COUNT(*) FROM sys.objects WHERE schema_id = SCHEMA_ID(N'a');"
        )
        .await
        .as_deref(),
        Some("1222")
    );
    run(&mut db.conn, &["ROLLBACK;"]).await;
    drop(other);
    db.drop().await;
}

/// One statement is not one catalog instant either: two scans of
/// `sys.objects` in one `SELECT`, either side of a scalar function, see a
/// table another session commits in between.
///
/// Nothing here waits on a clock. The function returns once the table
/// exists, so the second scan follows the commit; the writer starts once
/// the reading statement is inside the function, past the first scan. A writer that still lands before the first
/// scan (`1|1`) is a round to repeat under a new name, never a verdict.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn one_statement_does_not_read_the_catalog_at_one_instant() {
    let mut db = TestDb::create("statement619").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA a;",
            // `@after` ties the wait to the first scan and the second scan to
            // the wait, so the plan cannot reorder them. The deadline bounds a
            // writer that never commits.
            "CREATE FUNCTION dbo.wait_for (@name nvarchar(300), @after bigint) RETURNS bigint AS BEGIN \
                 DECLARE @end datetime2 = DATEADD(second, 30, SYSDATETIME()), @i bigint = 0; \
                 IF @after < 0 RETURN -1; \
                 WHILE OBJECT_ID(@name) IS NULL AND SYSDATETIME() < @end SET @i += 1; \
                 RETURN CASE WHEN OBJECT_ID(@name) IS NULL THEN -1 ELSE @i END; END;",
        ],
    )
    .await;
    let reader = number(&mut db.conn, "SELECT CONVERT(bigint, @@SPID);").await;
    let mut other = session(&db).await;
    let mut verdict = None;
    for round in 0..5 {
        let table = format!("during_{round}");
        let statement = format!(
            "SELECT CONVERT(nvarchar(40), CONCAT(f.n, N'|', s.n)) \
             FROM (SELECT COUNT_BIG(*) AS n FROM sys.objects \
                   WHERE schema_id = SCHEMA_ID(N'a') AND name = N'{table}') f \
             CROSS APPLY (SELECT dbo.wait_for(N'a.{table}', f.n) AS waited) p \
             CROSS APPLY (SELECT COUNT_BIG(*) AS n FROM sys.objects \
                          WHERE schema_id = SCHEMA_ID(N'a') AND name = N'{table}' AND p.waited >= 0) s;"
        );
        let running = format!(
            "SELECT CONVERT(bigint, COUNT(*)) FROM sys.dm_exec_requests r \
             CROSS APPLY sys.dm_exec_sql_text(r.sql_handle) t \
             WHERE r.session_id = {reader} AND t.objectid = OBJECT_ID(N'dbo.wait_for');"
        );
        let (counts, ()) = tokio::join!(text(&mut db.conn, &statement), async {
            for _ in 0..500 {
                if number(&mut other, &running).await > 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            run(&mut other, &[&*format!("CREATE TABLE a.{table} (x int);")]).await;
        });
        if counts != "1|1" {
            verdict = Some(counts);
            break;
        }
    }
    assert_eq!(
        verdict.as_deref(),
        Some("0|1"),
        "the second scan of one statement saw the commit the first did not"
    );
    drop(other);
    db.drop().await;
}

/// The closing comparison DEC-619.1 relies on, read the way a capture reads:
/// component by component. A commit between the components either reaches a
/// later component, where the comparison with the seal refuses, or touches
/// only components already read, which is a change after their last
/// observation. A change restored before the next component leaves nothing
/// lasting to find.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_commit_between_component_reads_reaches_a_later_read_or_follows_the_earlier_one() {
    let mut db = TestDb::create("components619").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA app;",
            "CREATE TABLE dbo.price (v int);",
            "CREATE VIEW app.one_part AS SELECT v FROM price;",
        ],
    )
    .await;
    let membership = "SELECT CONVERT(nvarchar(128), name) FROM sys.objects WHERE schema_id = SCHEMA_ID(N'app') ORDER BY name;";
    let sealed_members = texts(&mut db.conn, membership).await;
    let sealed_bindings = bindings(&mut db.conn, "app.one_part").await;
    let mut other = session(&db).await;

    // Reaches the later component: the capture differs from the seal.
    let members = texts(&mut db.conn, membership).await;
    run(&mut other, &["CREATE TABLE app.price (v int);"]).await;
    let later = bindings(&mut db.conn, "app.one_part").await;
    assert_eq!(members, sealed_members);
    assert_ne!(
        later, sealed_bindings,
        "the rebinding reached the later read"
    );
    run(&mut other, &["DROP TABLE app.price;"]).await;

    // Touches only what was read: the capture equals the seal, and the
    // next capture — the closing read, in apply — finds it.
    let members = texts(&mut db.conn, membership).await;
    run(&mut other, &["CREATE TABLE app.unrelated (v int);"]).await;
    let later = bindings(&mut db.conn, "app.one_part").await;
    assert_eq!((&members, &later), (&sealed_members, &sealed_bindings));
    assert_ne!(texts(&mut db.conn, membership).await, sealed_members);
    run(&mut other, &["DROP TABLE app.unrelated;"]).await;

    // Changed and restored between the reads: nothing lasting remains.
    let members = texts(&mut db.conn, membership).await;
    run(
        &mut other,
        &["CREATE TABLE app.price (v int);", "DROP TABLE app.price;"],
    )
    .await;
    let later = bindings(&mut db.conn, "app.one_part").await;
    assert_eq!((&members, &later), (&sealed_members, &sealed_bindings));
    assert_eq!(texts(&mut db.conn, membership).await, sealed_members);
    assert_eq!(
        bindings(&mut db.conn, "app.one_part").await,
        sealed_bindings
    );
    drop(other);
    db.drop().await;
}

/// The apply transaction's closing read sees its own uncommitted DDL and the
/// binding it records. That DDL holds another session's catalog scan of the
/// schema, but not that session's DDL: the transaction detects another
/// writer, it does not lock one out (SPEC 7.6).
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn the_apply_transaction_reads_its_own_ddl_and_does_not_lock_out_another_writer() {
    let mut db = TestDb::create("applytx619").await;
    run(
        &mut db.conn,
        &["CREATE SCHEMA a;", "CREATE TABLE dbo.m (x int);"],
    )
    .await;
    let mut other = session(&db).await;
    run(&mut other, &["SET LOCK_TIMEOUT 2000;"]).await;
    run(
        &mut db.conn,
        &[
            "SET XACT_ABORT ON; BEGIN TRANSACTION;",
            "CREATE VIEW a.installed AS SELECT x FROM m;",
        ],
    )
    .await;
    assert_eq!(
        number(
            &mut db.conn,
            "SELECT CONVERT(bigint, COUNT(*)) FROM sys.objects WHERE object_id = OBJECT_ID(N'a.installed');"
        )
        .await,
        1
    );
    assert_eq!(bindings(&mut db.conn, "a.installed").await, ["dbo.m"]);
    assert_eq!(
        refusal(
            &mut other,
            "SELECT COUNT(*) FROM sys.objects WHERE schema_id = SCHEMA_ID(N'a');"
        )
        .await
        .as_deref(),
        Some("1222")
    );
    assert_eq!(
        refusal(&mut other, "CREATE TABLE a.another_writer (i int);").await,
        None
    );
    run(&mut db.conn, &["ROLLBACK;"]).await;
    drop(other);
    db.drop().await;
}

/// `modify_date` is no version of the manifest: it moves when an object is
/// renamed and renamed back or altered to the same text, but a grant and its
/// revoke leave it as it was. So an optimistic double read keyed on it
/// misses a change-and-restore of a permission.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn modify_date_moves_on_a_restored_definition_but_not_on_a_restored_grant() {
    let mut db = TestDb::create("marker619").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA a;",
            "CREATE TABLE dbo.m (x int);",
            "CREATE VIEW a.v AS SELECT x FROM dbo.m;",
            "CREATE USER u WITHOUT LOGIN;",
        ],
    )
    .await;
    let stamp = "SELECT CONVERT(nvarchar(30), modify_date, 126) FROM sys.objects WHERE object_id = OBJECT_ID(N'a.v');";
    let pause = "WAITFOR DELAY '00:00:00.050';";
    for restore in [
        &[
            "EXEC sp_rename N'a.v', N'v2';",
            "EXEC sp_rename N'a.v2', N'v';",
        ][..],
        &["ALTER VIEW a.v AS SELECT x FROM dbo.m;"][..],
    ] {
        let before = text(&mut db.conn, stamp).await;
        run(&mut db.conn, &[pause]).await;
        run(&mut db.conn, restore).await;
        assert_ne!(text(&mut db.conn, stamp).await, before, "{restore:?}");
    }
    let before = text(&mut db.conn, stamp).await;
    run(
        &mut db.conn,
        &[
            pause,
            "GRANT SELECT ON a.v TO u;",
            "REVOKE SELECT ON a.v TO u;",
        ],
    )
    .await;
    assert_eq!(text(&mut db.conn, stamp).await, before);
    db.drop().await;
}

/// The rejected lock candidate. A no-op `ALTER AUTHORIZATION` of a schema
/// to its own owner holds a schema modification lock to the end of the
/// transaction, which keeps another session from creating, altering or
/// granting in that schema; it does not hold a database-scope change such as
/// a role membership, and a deployer with `ALTER` on the schema, or
/// `db_ddladmin`, may not take it.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_schema_lock_needs_ownership_a_deployer_lacks_and_misses_database_scope_changes() {
    let mut db = TestDb::create("schemalock619").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA a;",
            "CREATE TABLE a.t (x int);",
            "CREATE USER u WITHOUT LOGIN;",
            "CREATE USER deployer WITHOUT LOGIN;",
            "GRANT ALTER ON SCHEMA::a TO deployer;",
            "GRANT CREATE TABLE, CREATE VIEW TO deployer;",
        ],
    )
    .await;
    let mut other = session(&db).await;
    run(&mut other, &["SET LOCK_TIMEOUT 1500;"]).await;
    run(
        &mut db.conn,
        &[
            "BEGIN TRANSACTION;",
            "ALTER AUTHORIZATION ON SCHEMA::a TO dbo;",
        ],
    )
    .await;
    for sql in [
        "CREATE TABLE a.arrival (x int);",
        "GRANT SELECT ON a.t TO u;",
        "GRANT SELECT ON SCHEMA::a TO u;",
    ] {
        assert_eq!(
            refusal(&mut other, sql).await.as_deref(),
            Some("1222"),
            "{sql}"
        );
    }
    assert_eq!(
        refusal(&mut other, "ALTER ROLE db_datareader ADD MEMBER u;").await,
        None
    );
    run(&mut db.conn, &["ROLLBACK;"]).await;

    let attempt = "BEGIN TRANSACTION; ALTER AUTHORIZATION ON SCHEMA::a TO dbo; ROLLBACK;";
    run(&mut db.conn, &["EXECUTE AS USER = 'deployer';"]).await;
    assert_eq!(
        refusal(&mut db.conn, attempt).await.as_deref(),
        Some("15151")
    );
    run(
        &mut db.conn,
        &[
            "IF @@TRANCOUNT > 0 ROLLBACK;",
            "REVERT;",
            "ALTER ROLE db_ddladmin ADD MEMBER deployer;",
            "EXECUTE AS USER = 'deployer';",
        ],
    )
    .await;
    assert_eq!(
        refusal(&mut db.conn, attempt).await.as_deref(),
        Some("15151")
    );
    run(&mut db.conn, &["IF @@TRANCOUNT > 0 ROLLBACK;", "REVERT;"]).await;
    drop(other);
    db.drop().await;
}

/// The rejected witness candidate. A database DDL trigger writing a row per
/// event counts a grant and its revoke, but not an `ALTER DATABASE` option
/// or a view refresh, which changes the view's columns. Worse, it is a
/// failure point for every user's DDL: once a user may no longer write the
/// row, that user's DDL fails and rolls back.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_ddl_witness_trigger_misses_some_changes_and_fails_other_users_ddl() {
    let mut db = TestDb::create("witness619").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA a;",
            "CREATE TABLE dbo.witness (id bigint IDENTITY PRIMARY KEY);",
            "CREATE TRIGGER witness_events ON DATABASE FOR DDL_DATABASE_LEVEL_EVENTS AS INSERT dbo.witness DEFAULT VALUES;",
            "GRANT INSERT ON dbo.witness TO public;",
            "CREATE TABLE a.base (x int);",
            "CREATE VIEW a.star AS SELECT * FROM a.base;",
            "CREATE USER u WITHOUT LOGIN;",
            "CREATE USER other_user WITHOUT LOGIN;",
            "GRANT ALTER ON SCHEMA::a TO other_user;",
            "GRANT CREATE TABLE TO other_user;",
        ],
    )
    .await;
    let events = "SELECT COUNT_BIG(*) FROM dbo.witness;";
    for (batches, expected) in [
        (
            &[
                "GRANT SELECT ON a.base TO u;",
                "REVOKE SELECT ON a.base TO u;",
            ][..],
            2,
        ),
        (
            &[&*format!("ALTER DATABASE [{}] SET ANSI_NULLS ON;", db.name)][..],
            0,
        ),
        (&["ALTER TABLE a.base ADD y int;"][..], 1),
        (&["EXEC sp_refreshview N'a.star';"][..], 0),
    ] {
        let before = number(&mut db.conn, events).await;
        run(&mut db.conn, batches).await;
        assert_eq!(
            number(&mut db.conn, events).await - before,
            expected,
            "{batches:?}"
        );
    }
    assert_eq!(
        number(
            &mut db.conn,
            "SELECT CONVERT(bigint, COUNT(*)) FROM sys.columns WHERE object_id = OBJECT_ID(N'a.star');"
        )
        .await,
        2,
        "the uncounted refresh changed the view"
    );

    let other_ddl = "CREATE TABLE a.by_other_user (i int); DROP TABLE a.by_other_user;";
    run(&mut db.conn, &["EXECUTE AS USER = 'other_user';"]).await;
    assert_eq!(refusal(&mut db.conn, other_ddl).await, None);
    run(
        &mut db.conn,
        &["REVERT;", "REVOKE INSERT ON dbo.witness TO public;"],
    )
    .await;
    run(&mut db.conn, &["EXECUTE AS USER = 'other_user';"]).await;
    assert_eq!(
        refusal(&mut db.conn, other_ddl).await.as_deref(),
        Some("229")
    );
    run(&mut db.conn, &["IF @@TRANCOUNT > 0 ROLLBACK;", "REVERT;"]).await;
    assert_eq!(
        number(
            &mut db.conn,
            "SELECT CONVERT(bigint, COUNT(*)) FROM sys.objects WHERE name = N'by_other_user';"
        )
        .await,
        0
    );
    db.drop().await;
}

/// What a login can read of its own server reach, without any privilege:
/// its server permissions, its server roles, and every grant its token holds
/// on a server-scoped securable. The first two are not enough: a login that
/// may impersonate an administrator reads exactly as a confined one there,
/// and becomes `sysadmin` by `EXECUTE AS LOGIN`; only its own row in
/// `sys.server_permissions` shows it. A confined account holds `CONNECT SQL`
/// and `VIEW ANY DATABASE` and nothing else; `securityadmin` and `dbcreator`
/// each add a permission that reaches past one database.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_login_reads_its_own_server_reach_without_privilege() {
    let pid = std::process::id();
    let password = "Pbps!Reach619x";
    let mut admin = connect_live(&conn_str()).await.unwrap();
    let administrator = text(&mut admin, "SELECT CONVERT(nvarchar(128), SUSER_NAME());").await;
    let logins = [
        (format!("pbps619_confined_{pid}"), None),
        (
            format!("pbps619_security_{pid}"),
            Some("ALTER SERVER ROLE securityadmin ADD MEMBER"),
        ),
        (
            format!("pbps619_creator_{pid}"),
            Some("ALTER SERVER ROLE dbcreator ADD MEMBER"),
        ),
        (
            format!("pbps619_impersonator_{pid}"),
            Some("GRANT IMPERSONATE ON LOGIN::[{administrator}] TO"),
        ),
    ];
    for (login, grant) in &logins {
        run(
            &mut admin,
            &[&*format!(
                "CREATE LOGIN [{login}] WITH PASSWORD = N'{password}', CHECK_POLICY = OFF;"
            )],
        )
        .await;
        if let Some(grant) = grant {
            let grant = grant.replace("{administrator}", &administrator);
            run(&mut admin, &[&*format!("{grant} [{login}];")]).await;
        }
    }
    let reach = "SELECT CONVERT(nvarchar(4000), STRING_AGG(CONVERT(nvarchar(4000), permission_name), N',') \
                     WITHIN GROUP (ORDER BY permission_name)) FROM fn_my_permissions(NULL, N'SERVER');";
    let roles = "SELECT CONVERT(nvarchar(4000), STRING_AGG(CONVERT(nvarchar(4000), name), N',') \
                     WITHIN GROUP (ORDER BY name)) FROM sys.login_token WHERE type = N'SERVER ROLE';";
    // Every grant to a principal in the login's token, but `public`'s
    // defaults: connecting, listing databases, and the endpoints.
    let grants = "SELECT CONVERT(nvarchar(4000), STRING_AGG(CONVERT(nvarchar(4000), \
                         p.class_desc + N':' + p.permission_name), N',')) \
                  FROM sys.server_permissions p JOIN sys.login_token t ON t.principal_id = p.grantee_principal_id \
                  WHERE p.state IN ('G', 'W') \
                    AND NOT (p.class = 100 AND p.permission_name IN (N'CONNECT SQL', N'VIEW ANY DATABASE')) \
                    AND NOT (p.class = 105 AND p.permission_name = N'CONNECT');";
    let mut seen = Vec::new();
    for (login, _) in &logins {
        let mut conn = connect_live(&super::resolver_tests::login_url(login, password, "master"))
            .await
            .unwrap();
        seen.push((
            text(&mut conn, reach).await,
            text(&mut conn, roles).await,
            text(&mut conn, grants).await,
        ));
        if login.starts_with("pbps619_impersonator_") {
            assert_eq!(
                number(
                    &mut conn,
                    &format!(
                        "EXECUTE AS LOGIN = N'{administrator}'; \
                         SELECT CONVERT(bigint, IS_SRVROLEMEMBER(N'sysadmin')); REVERT;"
                    )
                )
                .await,
                1,
                "the impersonation reaches the administrator"
            );
        }
    }
    let confined = "CONNECT SQL,VIEW ANY DATABASE".to_owned();
    let public = "public".to_owned();
    let none = "NULL".to_owned();
    assert_eq!(
        seen,
        [
            (confined.clone(), public.clone(), none.clone()),
            (
                "ALTER ANY LOGIN,CONNECT SQL,CREATE LOGIN,VIEW ANY DATABASE".to_owned(),
                "public,securityadmin".to_owned(),
                none.clone()
            ),
            (
                "CONNECT SQL,CREATE ANY DATABASE,VIEW ANY DATABASE".to_owned(),
                "dbcreator,public".to_owned(),
                none.clone()
            ),
            (confined, public, "SERVER_PRINCIPAL:IMPERSONATE".to_owned()),
        ]
    );
    assert!(
        text(&mut admin, reach).await.contains("CONTROL SERVER"),
        "the negative: an administrator reads its reach the same way"
    );
    for (login, _) in &logins {
        drop_login(&mut admin, login).await;
    }
}

/// A database born from an unmodified `model` on the pinned build holds the
/// fresh-install inventory #723's empty-at-birth check compares with. The
/// same reads find a user object once there is one.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_database_born_from_model_holds_only_the_fresh_install_inventory() {
    let mut db = TestDb::create("born619").await;
    let user_objects = "SELECT COUNT_BIG(*) FROM sys.objects WHERE is_ms_shipped = 0;";
    assert_eq!(number(&mut db.conn, user_objects).await, 0);
    assert_eq!(
        texts(
            &mut db.conn,
            "SELECT CONVERT(nvarchar(128), name) FROM sys.schemas ORDER BY schema_id;"
        )
        .await,
        [
            "dbo",
            "guest",
            "INFORMATION_SCHEMA",
            "sys",
            "db_owner",
            "db_accessadmin",
            "db_securityadmin",
            "db_ddladmin",
            "db_backupoperator",
            "db_datareader",
            "db_datawriter",
            "db_denydatareader",
            "db_denydatawriter",
        ]
    );
    assert_eq!(
        texts(
            &mut db.conn,
            "SELECT CONVERT(nvarchar(200), name COLLATE DATABASE_DEFAULT + N':' + type COLLATE DATABASE_DEFAULT) FROM sys.database_principals ORDER BY principal_id;"
        )
        .await,
        [
            "public:R",
            "dbo:S",
            "guest:S",
            "INFORMATION_SCHEMA:S",
            "sys:S",
            "db_owner:R",
            "db_accessadmin:R",
            "db_securityadmin:R",
            "db_ddladmin:R",
            "db_backupoperator:R",
            "db_datareader:R",
            "db_datawriter:R",
            "db_denydatareader:R",
            "db_denydatawriter:R",
        ]
    );
    assert_eq!(
        texts(
            &mut db.conn,
            "SELECT CONVERT(nvarchar(200), USER_NAME(role_principal_id) + N'<-' + USER_NAME(member_principal_id)) \
             FROM sys.database_role_members;"
        )
        .await,
        ["db_owner<-dbo"]
    );
    assert_eq!(
        texts(
            &mut db.conn,
            "SELECT CONVERT(nvarchar(200), CONCAT(USER_NAME(grantee_principal_id), N':', \
                 class_desc COLLATE DATABASE_DEFAULT, N':', state_desc COLLATE DATABASE_DEFAULT, N':', COUNT(*))) \
             FROM sys.database_permissions GROUP BY grantee_principal_id, class_desc, state_desc ORDER BY 1;"
        )
        .await,
        [
            "dbo:DATABASE:GRANT:1",
            "public:DATABASE:GRANT:2",
            "public:OBJECT_OR_COLUMN:GRANT:232",
        ]
    );
    assert_eq!(
        number(
            &mut db.conn,
            "SELECT (SELECT COUNT_BIG(*) FROM sys.types WHERE is_user_defined = 1) \
                  + (SELECT COUNT_BIG(*) FROM sys.assemblies WHERE is_user_defined = 1) \
                  + (SELECT COUNT_BIG(*) FROM sys.synonyms) \
                  + (SELECT COUNT_BIG(*) FROM sys.xml_schema_collections WHERE schema_id <> 4) \
                  + (SELECT COUNT_BIG(*) FROM sys.fulltext_catalogs) \
                  + (SELECT COUNT_BIG(*) FROM sys.triggers WHERE parent_class = 0) \
                  + (SELECT COUNT_BIG(*) FROM sys.database_scoped_configurations WHERE is_value_default = 0);"
        )
        .await,
        0
    );
    run(&mut db.conn, &["CREATE TABLE dbo.left_behind (i int);"]).await;
    assert_eq!(number(&mut db.conn, user_objects).await, 1);
    db.drop().await;
}

/// Metadata visibility hides what a database holds from an account that is
/// not `db_owner`: it reads no user object where one exists. So the empty
/// check must run as the database's owner, and an account that is not one
/// cannot read emptiness at all.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn an_account_that_is_not_db_owner_cannot_see_what_its_database_holds() {
    let mut db = TestDb::create("visible619").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA hidden;",
            "CREATE TABLE hidden.left_behind (i int);",
            "CREATE USER scratch_user WITHOUT LOGIN;",
            "GRANT CREATE TABLE TO scratch_user;",
            "GRANT ALTER ON SCHEMA::dbo TO scratch_user;",
        ],
    )
    .await;
    let seen = "SELECT CONVERT(bigint, IS_MEMBER(N'db_owner') * 10 + (SELECT COUNT(*) FROM sys.objects WHERE is_ms_shipped = 0));";
    run(&mut db.conn, &["EXECUTE AS USER = 'scratch_user';"]).await;
    assert_eq!(number(&mut db.conn, seen).await, 0);
    run(
        &mut db.conn,
        &[
            "REVERT;",
            "ALTER ROLE db_owner ADD MEMBER scratch_user;",
            "EXECUTE AS USER = 'scratch_user';",
        ],
    )
    .await;
    assert_eq!(number(&mut db.conn, seen).await, 11);
    run(&mut db.conn, &["REVERT;"]).await;
    db.drop().await;
}

/// No reported identifier separates two installations (a copied one keeps
/// `master`'s `family_guid`, measured on #619), but a global temporary
/// table does, live and without privilege: every login on the instance that
/// holds it sees it, another instance does not, and it goes with the
/// session that made it.
#[tokio::test]
#[ignore = "needs a second live SQL Server; set PBPS_TEST_EXPRESS_DB (see scripts/live-tests.sh)"]
async fn a_global_temporary_table_marks_one_instance_to_every_login_on_it() {
    let express = std::env::var("PBPS_TEST_EXPRESS_DB").expect(
        "PBPS_TEST_EXPRESS_DB is not set; this test needs the Express server scripts/live-tests.sh starts",
    );
    let pid = std::process::id();
    let login = format!("pbps619_marker_{pid}");
    let password = "Pbps!Marker619x";
    let marker = format!(
        "##pbps619_{pid}_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let seen = format!(
        "SELECT CONVERT(bigint, CASE WHEN OBJECT_ID(N'tempdb..{marker}') IS NULL THEN 0 ELSE 1 END);"
    );
    let mut admin = connect_live(&conn_str()).await.unwrap();
    run(
        &mut admin,
        &[&*format!(
            "CREATE LOGIN [{login}] WITH PASSWORD = N'{password}', CHECK_POLICY = OFF;"
        )],
    )
    .await;
    let mut confined = connect_live(&super::resolver_tests::login_url(
        &login, password, "master",
    ))
    .await
    .unwrap();
    let mut maker = connect_live(&conn_str()).await.unwrap();
    run(&mut maker, &[&*format!("CREATE TABLE {marker} (i int);")]).await;

    assert_eq!(number(&mut admin, &seen).await, 1);
    assert_eq!(number(&mut confined, &seen).await, 1);
    let mut elsewhere = connect_live(&express).await.unwrap();
    assert_eq!(number(&mut elsewhere, &seen).await, 0);
    drop(maker);
    // The session's end is reported to the engine asynchronously.
    let mut gone = 1;
    for _ in 0..50 {
        gone = number(&mut admin, &seen).await;
        if gone == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(gone, 0, "the marker outlived its session");
    drop(confined);
    drop_login(&mut admin, &login).await;
}
