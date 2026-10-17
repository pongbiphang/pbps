//! #2141: the SQL Server target capture (#620 (a)) against a live server.
//! Each test reads a database it built with the capture itself and checks
//! what it binds, what it leaves to the candidates, and what it refuses.

use super::*;
use pbps_db::resolver::capture::{CaptureError, InputChange};
use pbps_mssql::resolver::capture::{
    self, CandidateClass, CandidateSet, CaptureScope, ObjectIdentity, class,
};
use std::collections::BTreeSet;

async fn run(conn: &mut Conn, batches: &[&str]) {
    for batch in batches {
        conn.execute(batch)
            .await
            .unwrap_or_else(|e| panic!("the engine rejected:\n{batch}\n{e}"));
    }
}

fn object(schema: &str, name: &str) -> ObjectIdentity {
    capture::object(schema, name)
}

fn column(table: &ObjectIdentity, name: &str) -> ObjectIdentity {
    ObjectIdentity {
        class: class::COLUMN.into(),
        name: vec![name.into()],
        signature: vec![table.clone()],
    }
}

fn user_type(schema: &str, name: &str) -> ObjectIdentity {
    ObjectIdentity {
        class: class::TYPE.into(),
        name: vec![schema.into(), name.into()],
        signature: Vec::new(),
    }
}

fn principal(name: &str) -> ObjectIdentity {
    ObjectIdentity {
        class: class::PRINCIPAL.into(),
        name: vec![name.into()],
        signature: Vec::new(),
    }
}

fn objects(names: &[(&str, &str)]) -> CaptureScope {
    CaptureScope {
        retained: names.iter().map(|(s, n)| object(s, n)).collect(),
        candidates: BTreeSet::new(),
    }
}

fn bound(captured: &capture::CapturedInputs, of: &ObjectIdentity) -> BTreeSet<ObjectIdentity> {
    captured
        .bound_objects(of)
        .unwrap_or_else(|| panic!("{of:?} was not captured"))
        .cloned()
        .collect()
}

fn candidate(class: CandidateClass, schema: &str, name: &str) -> CandidateSet {
    CandidateSet {
        class,
        schema: schema.into(),
        name: name.into(),
    }
}

/// A login of the test's own, mapped into `db` as a `db_ddladmin` with the
/// given extra grants, and a connection as it.
async fn login(db: &mut TestDb, tag: &str, grants: &str) -> (Conn, String) {
    let login = format!("pbps2141_{tag}_{}", std::process::id());
    let password = "Pbps2141!Login1";
    db.conn
        .execute(&format!(
            "IF SUSER_ID(N'{login}') IS NULL CREATE LOGIN [{login}] WITH PASSWORD = N'{password}', CHECK_POLICY = OFF; \
             USE [{0}]; CREATE USER [{login}] FOR LOGIN [{login}]; \
             ALTER ROLE db_ddladmin ADD MEMBER [{login}]; {grants}",
            db.name
        ))
        .await
        .expect("create the login");
    let base = conn_str()
        .split(';')
        .filter(|p| {
            let k = p
                .split('=')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            !matches!(
                k.as_str(),
                "user id" | "uid" | "password" | "pwd" | "database"
            )
        })
        .collect::<Vec<_>>()
        .join(";");
    let conn = connect_live(&format!(
        "{base};User Id={login};Password={password};Database={}",
        db.name
    ))
    .await
    .expect("connect as the login");
    (conn, login)
}

/// An environment key from a file of the test's own, as an operator's
/// would be.
fn environment_key() -> pbps_db::fingerprint::EnvironmentFingerprintKey {
    use std::io::Write as _;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt as _;
    let path = std::env::temp_dir().join(format!("pbps2141-key-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&path).expect("write the key file");
    file.write_all(b"AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=")
        .expect("write the key file");
    drop(file);
    let key =
        pbps_db::fingerprint::EnvironmentFingerprintKey::from_file(&path).expect("read the key");
    let _ = std::fs::remove_file(path);
    key
}

async fn drop_login(db: &mut TestDb, login: &str) {
    let _ = db
        .conn
        .execute(&format!(
            "USE master; IF SUSER_ID(N'{login}') IS NOT NULL DROP LOGIN [{login}];"
        ))
        .await;
}

/// Case 2: each surface gets the engine's answer. Id-bound references —
/// two-part names, schema-bound ones, the function a computed column,
/// DEFAULT or CHECK calls, and every type — are bindings. A one-part name in
/// a body that is not schema-bound is not: its module is runtime-bound, and
/// its candidates are the module's schema and `dbo`.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn mssql_each_surface_gets_the_engines_answer() {
    let mut db = TestDb::create("surfaces2141").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA app;",
            "CREATE TABLE dbo.price (v int);",
            "CREATE TYPE app.amount FROM decimal(10, 2);",
            "CREATE FUNCTION dbo.f(@x int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN @x END;",
            "CREATE VIEW app.one_part AS SELECT v FROM price;",
            "CREATE VIEW app.two_part AS SELECT v FROM dbo.price;",
            "CREATE VIEW app.bound WITH SCHEMABINDING AS SELECT v, dbo.f(v) AS w FROM dbo.price;",
            "CREATE TABLE app.t (a int, b AS dbo.f(a), c int CONSTRAINT df_c DEFAULT dbo.f(1), \
             x app.amount, CONSTRAINT ck_a CHECK (dbo.f(a) > 0));",
            "CREATE SYNONYM app.syn FOR dbo.price;",
            "CREATE PROCEDURE app.typed @a app.amount AS SELECT @a;",
        ],
    )
    .await;
    let scope = objects(&[
        ("app", "one_part"),
        ("app", "two_part"),
        ("app", "bound"),
        ("app", "t"),
        ("app", "syn"),
        ("app", "typed"),
    ]);
    let captured = capture::capture(&mut db.conn, &scope)
        .await
        .expect("capture");

    let price = object("dbo", "price");
    let f = object("dbo", "f");
    // The one-part view binds nothing by id; its name is runtime-bound.
    assert!(
        !bound(&captured, &object("app", "one_part")).contains(&price),
        "a one-part name is not a binding"
    );
    let runtime: BTreeSet<_> = captured.runtime_bound().cloned().collect();
    assert_eq!(runtime, BTreeSet::from([object("app", "one_part")]));
    let wanted = captured.runtime_scope().candidates;
    assert!(wanted.contains(&candidate(CandidateClass::Object, "app", "price")));
    assert!(wanted.contains(&candidate(CandidateClass::Object, "dbo", "price")));
    // A two-part name and a schema-bound reference are bindings.
    assert!(bound(&captured, &object("app", "two_part")).contains(&price));
    let schema_bound = bound(&captured, &object("app", "bound"));
    assert!(schema_bound.contains(&f) && schema_bound.contains(&column(&price, "v")));
    // The computed column, the DEFAULT and the CHECK each bind the function.
    let t = object("app", "t");
    assert!(bound(&captured, &column(&t, "b")).contains(&f));
    assert!(bound(&captured, &object("app", "df_c")).contains(&f));
    assert!(bound(&captured, &object("app", "ck_a")).contains(&f));
    // A user type binds where it is used: a column and a parameter.
    assert!(bound(&captured, &column(&t, "x")).contains(&user_type("app", "amount")));
    assert!(bound(&captured, &object("app", "typed")).contains(&user_type("app", "amount")));
    // A synonym's target is a property, not a binding: it is resolved when
    // the synonym is used.
    assert!(!bound(&captured, &object("app", "syn")).contains(&price));
    // What the captured objects bind is captured too.
    assert!(captured.objects().any(|o| *o == f));
    db.drop().await;
}

/// The motivating pair on SQL Server (`docs/RESOLVER-MSSQL.md` §2), through
/// the capture: a table arriving in the module's own schema joins the
/// one-part name's candidates; one arriving off the path joins nothing, and
/// the schema-bound binding stays where it was.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn mssql_a_one_part_names_candidates_see_an_arrival_in_the_modules_schema_only() {
    let mut db = TestDb::create("arrival2141").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA app;",
            "CREATE SCHEMA other;",
            "CREATE TABLE dbo.price (v int);",
            "CREATE VIEW app.one_part AS SELECT v FROM price;",
            "CREATE VIEW app.bound WITH SCHEMABINDING AS SELECT v FROM dbo.price;",
        ],
    )
    .await;
    let first = capture::capture(
        &mut db.conn,
        &objects(&[("app", "one_part"), ("app", "bound")]),
    )
    .await
    .expect("capture");
    let scope = first.runtime_scope();
    let sealed = capture::capture(&mut db.conn, &scope)
        .await
        .expect("capture");
    let in_app = candidate(CandidateClass::Object, "app", "price");
    assert_eq!(sealed.members(&in_app), Some(&BTreeSet::new()));

    run(&mut db.conn, &["CREATE TABLE other.price (v int);"]).await;
    let (_, differences) = capture::recapture(&mut db.conn, &sealed)
        .await
        .expect("recapture");
    assert!(
        !differences
            .iter()
            .any(|d| d.change == InputChange::Membership),
        "an arrival off the path is not a candidate: {differences:?}"
    );

    run(&mut db.conn, &["CREATE TABLE app.price (v int);"]).await;
    let (now, differences) = capture::recapture(&mut db.conn, &sealed)
        .await
        .expect("recapture");
    assert!(
        differences
            .iter()
            .any(|d| d.change == InputChange::Membership),
        "an arrival in the module's schema is a candidate: {differences:?}"
    );
    assert_eq!(
        now.members(&in_app),
        Some(&BTreeSet::from([object("app", "price")]))
    );
    assert_eq!(
        bound(&now, &object("app", "bound")),
        bound(&sealed, &object("app", "bound")),
        "a schema-bound binding does not move"
    );
    db.drop().await;
}

/// Case 3: a schema-bound binding is the one the target holds, by id, and a
/// name in another database refuses by name: the capture reads one database.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn mssql_a_schema_bound_binding_is_the_targets_observed_one() {
    let mut db = TestDb::create("bound2141").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA app;",
            "CREATE TABLE app.price (v int);",
            "CREATE VIEW app.bound WITH SCHEMABINDING AS SELECT v FROM app.price;",
            "CREATE PROCEDURE app.far AS SELECT * FROM otherdb.dbo.t;",
        ],
    )
    .await;
    let captured = capture::capture(&mut db.conn, &objects(&[("app", "bound")]))
        .await
        .expect("capture");
    let price = object("app", "price");
    assert!(bound(&captured, &object("app", "bound")).contains(&column(&price, "v")));
    let (_, differences) = capture::recapture(&mut db.conn, &captured)
        .await
        .expect("recapture");
    assert_eq!(differences, Vec::new(), "nothing changed");

    let refused = capture::capture(&mut db.conn, &objects(&[("app", "far")])).await;
    assert!(
        matches!(&refused, Err(CaptureError::Coverage(u)) if u.condition.contains("another database")),
        "{:?}",
        refused.err()
    );
    db.drop().await;
}

/// Case 4: unreadable dependency metadata never verifies. Without SELECT on
/// the dependency catalog the read is refused; with SELECT but without VIEW
/// DEFINITION it reads no rows, which cannot be told from no references:
/// both refuse by name. A procedure naming an object that does not exist
/// yet has no binding to read.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn mssql_unreadable_dependency_metadata_never_verifies() {
    let mut db = TestDb::create("unreadable2141").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA app;",
            "CREATE TABLE app.price (v int);",
            "CREATE VIEW app.v AS SELECT v FROM app.price;",
            "CREATE PROCEDURE app.later AS SELECT * FROM app.not_yet;",
        ],
    )
    .await;
    let scope = objects(&[("app", "v")]);

    let (mut neither, first) = login(&mut db, "neither", "").await;
    let refused = capture::capture(&mut neither, &scope).await;
    assert!(
        matches!(&refused, Err(CaptureError::Coverage(u)) if u.condition.contains("VIEW DEFINITION")),
        "{:?}",
        refused.err()
    );
    drop(neither);

    let (mut select_only, second) = login(
        &mut db,
        "select",
        "GRANT SELECT ON sys.sql_expression_dependencies TO [pbps2141_select_PID];"
            .replace("PID", &std::process::id().to_string())
            .as_str(),
    )
    .await;
    // The premise: this account reads the dependency catalog as empty.
    let rows = select_only
        .query("SELECT CONVERT(nvarchar(10), COUNT(*)) AS n FROM sys.sql_expression_dependencies;")
        .await
        .expect("the view answers");
    assert_eq!(rows[0].try_get::<&str>("n").unwrap(), Some("0"));
    let refused = capture::capture(&mut select_only, &scope).await;
    assert!(
        matches!(&refused, Err(CaptureError::Coverage(u)) if u.condition.contains("VIEW DEFINITION")),
        "{:?}",
        refused.err()
    );
    drop(select_only);

    let (mut both, third) = login(
        &mut db,
        "both",
        "GRANT VIEW DEFINITION TO [pbps2141_both_PID]; GRANT SELECT ON sys.sql_expression_dependencies TO [pbps2141_both_PID];"
            .replace("PID", &std::process::id().to_string())
            .as_str(),
    )
    .await;
    let captured = capture::capture(&mut both, &scope).await.expect("capture");
    assert!(bound(&captured, &object("app", "v")).contains(&object("app", "price")));
    let refused = capture::capture(&mut both, &objects(&[("app", "later")])).await;
    assert!(
        matches!(&refused, Err(CaptureError::Coverage(u)) if u.condition.contains("does not exist yet")),
        "{:?}",
        refused.err()
    );
    drop(both);

    for login in [first, second, third] {
        drop_login(&mut db, &login).await;
    }
    db.drop().await;
}

/// A one-part `EXEC` looks in `sys`, then the executing principal's default
/// schema, then `dbo`, never the module's schema (§2). A system procedure's
/// call leaves no dependency row, so it is no runtime name at all. A fixed
/// `EXECUTE AS` has one principal; any other module every principal, and a
/// principal's default schema is a captured property, so moving it differs.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn mssql_a_procedure_calls_candidates_follow_sys_and_the_executing_principals() {
    let mut db = TestDb::create("calls2141").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA app;",
            "CREATE SCHEMA other;",
            "CREATE SCHEMA third;",
            "CREATE USER u_other WITHOUT LOGIN WITH DEFAULT_SCHEMA = other;",
            "CREATE USER u_third WITHOUT LOGIN WITH DEFAULT_SCHEMA = third;",
            "CREATE PROCEDURE dbo.q AS SELECT 1;",
            "CREATE PROCEDURE app.anyone AS EXEC q;",
            "CREATE PROCEDURE app.fixed WITH EXECUTE AS 'u_other' AS EXEC q;",
            "CREATE PROCEDURE app.system AS EXEC sp_who;",
        ],
    )
    .await;
    let captured = capture::capture(
        &mut db.conn,
        &objects(&[("app", "anyone"), ("app", "fixed"), ("app", "system")]),
    )
    .await
    .expect("capture");
    let runtime: BTreeSet<_> = captured.runtime_bound().cloned().collect();
    assert_eq!(
        runtime,
        BTreeSet::from([object("app", "anyone"), object("app", "fixed")]),
        "a system procedure's call is no runtime name"
    );
    let wanted = captured.runtime_scope().candidates;
    for schema in ["dbo", "other", "third"] {
        assert!(
            wanted.contains(&candidate(CandidateClass::Object, schema, "q")),
            "{schema}: {wanted:?}"
        );
    }
    assert!(wanted.contains(&candidate(CandidateClass::SystemProcedure, "sys", "q")));
    assert!(
        !wanted.contains(&candidate(CandidateClass::Object, "app", "q")),
        "a call never looks in the module's own schema"
    );

    // The fixed principal alone: its default schema and dbo.
    run(&mut db.conn, &["DROP PROCEDURE app.anyone;"]).await;
    let fixed = capture::capture(&mut db.conn, &objects(&[("app", "fixed")]))
        .await
        .expect("capture");
    let wanted = fixed.runtime_scope().candidates;
    assert!(wanted.contains(&candidate(CandidateClass::Object, "other", "q")));
    assert!(wanted.contains(&candidate(CandidateClass::Object, "dbo", "q")));
    assert!(!wanted.contains(&candidate(CandidateClass::Object, "third", "q")));

    // `sys` holds `sp_who`: a set asked about it has that member.
    let asked = CaptureScope {
        retained: BTreeSet::new(),
        candidates: BTreeSet::from([
            candidate(CandidateClass::SystemProcedure, "sys", "sp_who"),
            candidate(CandidateClass::SystemProcedure, "sys", "q"),
        ]),
    };
    let system = capture::capture(&mut db.conn, &asked)
        .await
        .expect("capture");
    assert_eq!(
        system.members(&candidate(CandidateClass::SystemProcedure, "sys", "sp_who")),
        Some(&BTreeSet::from([object("sys", "sp_who")]))
    );
    assert_eq!(
        system.members(&candidate(CandidateClass::SystemProcedure, "sys", "q")),
        Some(&BTreeSet::new())
    );

    // Moving a principal's default schema moves the call: a difference.
    run(
        &mut db.conn,
        &["ALTER USER u_other WITH DEFAULT_SCHEMA = third;"],
    )
    .await;
    let (_, differences) = capture::recapture(&mut db.conn, &fixed)
        .await
        .expect("recapture");
    assert!(
        differences
            .iter()
            .any(|d| d.object.as_ref() == Some(&principal("u_other"))
                && d.change == InputChange::Properties),
        "{differences:?}"
    );
    db.drop().await;
}

/// Case 9, through the capture (DEC-619.1): another session's change that
/// lasts is found by the next capture, as the closing read finds it in
/// apply; one made and restored between two captures leaves nothing.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn mssql_a_recapture_finds_a_lasting_change_from_another_session_and_not_a_restored_one() {
    let mut db = TestDb::create("sessions2141").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA app;",
            "CREATE TABLE dbo.price (v int);",
            "CREATE VIEW app.one_part AS SELECT v FROM price;",
        ],
    )
    .await;
    let first = capture::capture(&mut db.conn, &objects(&[("app", "one_part")]))
        .await
        .expect("capture");
    let sealed = capture::capture(&mut db.conn, &first.runtime_scope())
        .await
        .expect("capture");
    let mut other = connect_live(&conn_str()).await.expect("connect");
    other.execute(&format!("USE [{}];", db.name)).await.unwrap();

    run(
        &mut other,
        &["CREATE TABLE app.price (v int);", "DROP TABLE app.price;"],
    )
    .await;
    let (_, differences) = capture::recapture(&mut db.conn, &sealed)
        .await
        .expect("recapture");
    assert_eq!(differences, Vec::new(), "a restored change leaves nothing");

    run(&mut other, &["CREATE TABLE app.price (v int);"]).await;
    let (_, differences) = capture::recapture(&mut db.conn, &sealed)
        .await
        .expect("recapture");
    assert!(!differences.is_empty(), "a lasting change is found");
    drop(other);
    db.drop().await;
}

/// Ownership is qualified from the start (DEC-614.2): a declared table and
/// its columns own their records, a referenced object owns nothing, a
/// declared name the target holds as another kind refuses, and an owner
/// change is a difference.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn mssql_ownership_follows_the_declaration_and_an_owner_change_is_a_difference() {
    use pbps_model::resolver::{ObjectOwnership, Surface};
    let mut db = TestDb::create("ownership2141").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA app;",
            "CREATE TABLE app.customer (id int NOT NULL, name nvarchar(50) NULL);",
            "CREATE TABLE dbo.other (v int);",
            "CREATE VIEW app.names AS SELECT name FROM app.customer;",
            "CREATE USER u_owner WITHOUT LOGIN;",
        ],
    )
    .await;
    let pulled = pbps_mssql::catalog::introspect(&mut db.conn)
        .await
        .expect("introspect");
    let mut declared = Schema::default();
    let table = pbps_model::TableName::new("app", "customer");
    declared
        .tables
        .insert(table.clone(), pulled.schema.tables[&table].clone());
    let ids = mint_ids(&declared, &IdsFile::default(), &[]);
    let namespaces = BTreeSet::from(["app".to_owned()]);
    let recorded = capture::RecordedOwnership {
        schema: &declared,
        ids: &ids,
        namespaces: &namespaces,
    };
    let key = environment_key();
    let scope = objects(&[("app", "customer"), ("dbo", "other"), ("app", "names")]);
    let (captured, manifest) = capture::capture_qualified(&mut db.conn, &scope, &key, &recorded)
        .await
        .expect("capture");
    let ownership = |object: &ObjectIdentity| {
        manifest
            .prerequisites()
            .iter()
            .find(|p| p.object == *object)
            .map(|p| p.ownership.clone())
    };
    let customer = object("app", "customer");
    assert_eq!(
        ownership(&customer),
        Some(ObjectOwnership::Surface(Surface::Table(table.clone())))
    );
    assert_eq!(
        ownership(&column(&customer, "name")),
        Some(ObjectOwnership::Surface(Surface::Column(
            table.column("name")
        )))
    );
    assert_eq!(
        ownership(&object("dbo", "other")),
        Some(ObjectOwnership::Unqualified),
        "an undeclared object is owned by nothing"
    );
    assert_eq!(
        ownership(&object("app", "names")),
        Some(ObjectOwnership::Unqualified),
        "a reference is not ownership"
    );

    run(
        &mut db.conn,
        &["ALTER AUTHORIZATION ON OBJECT::app.customer TO u_owner;"],
    )
    .await;
    let (_, differences) = capture::recapture(&mut db.conn, &captured)
        .await
        .expect("recapture");
    assert!(
        differences
            .iter()
            .any(|d| d.object.as_ref() == Some(&customer) && d.change == InputChange::Properties),
        "{differences:?}"
    );

    // A declared table the target holds as a view refuses.
    run(&mut db.conn, &["CREATE VIEW app.shaped AS SELECT 1 AS id;"]).await;
    let mut shaped = Schema::default();
    let as_view = pbps_model::TableName::new("app", "shaped");
    shaped
        .tables
        .insert(as_view.clone(), pulled.schema.tables[&table].clone());
    let shaped_ids = mint_ids(&shaped, &IdsFile::default(), &[]);
    let refused = capture::capture_qualified(
        &mut db.conn,
        &objects(&[("app", "shaped")]),
        &key,
        &capture::RecordedOwnership {
            schema: &shaped,
            ids: &shaped_ids,
            namespaces: &BTreeSet::new(),
        },
    )
    .await;
    assert!(
        matches!(&refused, Err(CaptureError::Coverage(u)) if u.condition.contains("different catalog kind")),
        "{:?}",
        refused.err()
    );
    db.drop().await;
}

/// A name is matched by the engine under the database's collation, as its
/// resolution is: on a case-insensitive database a candidate spelled
/// `App.PRICE` finds `app.price`, and on a case-sensitive one it does not.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn mssql_a_candidate_is_matched_under_the_databases_collation() {
    for (collation, found) in [
        ("Latin1_General_CI_AS", true),
        ("Latin1_General_CS_AS", false),
    ] {
        let mut db = TestDb::create_collated("collation2141", collation).await;
        run(
            &mut db.conn,
            &["CREATE SCHEMA app;", "CREATE TABLE app.price (v int);"],
        )
        .await;
        let asked = candidate(CandidateClass::Object, "App", "PRICE");
        let scope = CaptureScope {
            retained: BTreeSet::new(),
            candidates: BTreeSet::from([asked.clone()]),
        };
        let captured = capture::capture(&mut db.conn, &scope)
            .await
            .expect("capture");
        let expected = if found {
            BTreeSet::from([object("app", "price")])
        } else {
            BTreeSet::new()
        };
        assert_eq!(captured.members(&asked), Some(&expected), "{collation}");
        db.drop().await;
    }
}

/// A module created `WITH ENCRYPTION` reads its definition as NULL to every
/// reader, `sa` included: an unreadable definition is not an empty one, so
/// the capture refuses it by name rather than seal what it cannot see.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn mssql_an_encrypted_module_refuses_by_name() {
    let mut db = TestDb::create("encrypted2141").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA app;",
            "CREATE VIEW app.plain AS SELECT 1 AS one;",
            "CREATE VIEW app.secret WITH ENCRYPTION AS SELECT 1 AS one;",
        ],
    )
    .await;
    capture::capture(&mut db.conn, &objects(&[("app", "plain")]))
        .await
        .expect("a readable module is captured");
    let refused = capture::capture(&mut db.conn, &objects(&[("app", "secret")])).await;
    assert!(
        matches!(&refused, Err(CaptureError::Coverage(u)) if u.condition.contains("encrypted")),
        "{:?}",
        refused.err()
    );
    db.drop().await;
}

/// A table type is its columns, CHECKs and keys. Its own table object is the
/// engine's (`is_ms_shipped = 1`), so the ordinary object reads never list
/// it: a type rebuilt under the same name with another shape, with the
/// procedure that takes it rebuilt alike, must still differ.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn mssql_a_table_types_shape_is_captured_through_the_procedure_that_takes_it() {
    let mut db = TestDb::create("tabletype2141").await;
    let build = |label: &'static str| {
        [
            format!("CREATE TYPE app.rows AS TABLE (id int NOT NULL PRIMARY KEY, {label});"),
            "CREATE PROCEDURE app.takes @r app.rows READONLY AS SELECT * FROM @r;".to_owned(),
        ]
    };
    run(&mut db.conn, &["CREATE SCHEMA app;"]).await;
    for batch in build("label nvarchar(20)") {
        run(&mut db.conn, &[&batch]).await;
    }
    let sealed = capture::capture(&mut db.conn, &objects(&[("app", "takes")]))
        .await
        .expect("capture");
    let rows = user_type("app", "rows");
    assert!(bound(&sealed, &object("app", "takes")).contains(&rows));
    assert!(bound(&sealed, &rows).contains(&user_type("sys", "nvarchar")));

    run(
        &mut db.conn,
        &["DROP PROCEDURE app.takes;", "DROP TYPE app.rows;"],
    )
    .await;
    for batch in build("label nvarchar(40) CHECK (label <> N'')") {
        run(&mut db.conn, &[&batch]).await;
    }
    let (_, differences) = capture::recapture(&mut db.conn, &sealed)
        .await
        .expect("recapture");
    assert!(
        differences
            .iter()
            .any(|d| d.object.as_ref() == Some(&rows) && d.change == InputChange::Properties),
        "{differences:?}"
    );
    db.drop().await;
}

/// A key is its ordered columns: rebuilt under the same name on another
/// column, it differs, though no column did. A foreign key binds its table
/// and, by index id, the key it references (`docs/RESOLVER-MSSQL.md` §8).
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn mssql_a_keys_columns_and_a_foreign_keys_target_key_are_captured() {
    let mut db = TestDb::create("keys2141").await;
    run(
        &mut db.conn,
        &[
            "CREATE SCHEMA app;",
            "CREATE TABLE app.k (a int NOT NULL, b int NOT NULL, \
             CONSTRAINT pk_k PRIMARY KEY (a), CONSTRAINT uq_k UNIQUE (b));",
            "CREATE TABLE app.r (x int, CONSTRAINT fk_r FOREIGN KEY (x) REFERENCES app.k (b));",
        ],
    )
    .await;
    let sealed = capture::capture(&mut db.conn, &objects(&[("app", "r")]))
        .await
        .expect("capture");
    let fk = bound(&sealed, &object("app", "fk_r"));
    assert!(fk.contains(&object("app", "k")) && fk.contains(&object("app", "uq_k")));

    run(
        &mut db.conn,
        &[
            "ALTER TABLE app.r DROP CONSTRAINT fk_r;",
            "ALTER TABLE app.k DROP CONSTRAINT uq_k;",
            "ALTER TABLE app.k ADD CONSTRAINT uq_k UNIQUE (a);",
            "ALTER TABLE app.r ADD CONSTRAINT fk_r FOREIGN KEY (x) REFERENCES app.k (a);",
        ],
    )
    .await;
    let (_, differences) = capture::recapture(&mut db.conn, &sealed)
        .await
        .expect("recapture");
    for changed in [object("app", "uq_k"), object("app", "fk_r")] {
        assert!(
            differences
                .iter()
                .any(|d| d.object.as_ref() == Some(&changed) && d.change == InputChange::Properties),
            "{changed:?}: {differences:?}"
        );
    }

    // A unique index that is no constraint is the table's: its columns too.
    run(&mut db.conn, &["CREATE UNIQUE INDEX ux_b ON app.k (b);"]).await;
    let indexed = capture::capture(&mut db.conn, &objects(&[("app", "k")]))
        .await
        .expect("capture");
    run(
        &mut db.conn,
        &[
            "DROP INDEX ux_b ON app.k;",
            "CREATE UNIQUE INDEX ux_b ON app.k (a, b);",
        ],
    )
    .await;
    let (_, differences) = capture::recapture(&mut db.conn, &indexed)
        .await
        .expect("recapture");
    assert!(
        differences
            .iter()
            .any(|d| d.object.as_ref() == Some(&object("app", "k"))
                && d.change == InputChange::Properties),
        "{differences:?}"
    );
    db.drop().await;
}
