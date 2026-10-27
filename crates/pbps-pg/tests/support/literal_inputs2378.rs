//! Parse analysis must not call an extension's input before the bound-tree vet.
use super::user_types_tests::{Scratch, servers};
use pbps_model::{ColumnRef, ColumnType, TypeRef};
use pbps_pg::input_path::vet_conversion;

async fn install(db: &mut Scratch) {
    // Alias an existing C entry point: no compiler or test extension needed.
    // Its session-lock side effect survives a transaction rollback. The value
    // it returns is immaterial: none of these constants may be evaluated.
    db.sql(
        "CREATE TYPE app.unsafe;
        CREATE FUNCTION app.unsafe_in(cstring) RETURNS app.unsafe
          AS 'pg_advisory_unlock_all' LANGUAGE internal VOLATILE STRICT;
        CREATE FUNCTION app.unsafe_out(app.unsafe) RETURNS cstring
          AS 'int8out' LANGUAGE internal IMMUTABLE STRICT;
        CREATE TYPE app.unsafe (INPUT=app.unsafe_in, OUTPUT=app.unsafe_out,
          INTERNALLENGTH=8, PASSEDBYVALUE, ALIGNMENT=double);
        CREATE DOMAIN app.unsafe_domain AS app.unsafe;
        CREATE DOMAIN app.\"cast\" AS app.unsafe;
        CREATE DOMAIN app.\"coalesce\" AS app.unsafe;
        CREATE DOMAIN app.length AS app.unsafe;
        CREATE DOMAIN app.zone AS app.unsafe;
        CREATE DOMAIN app.\"integer\" AS app.unsafe;
        CREATE DOMAIN app.\"double\" AS app.unsafe;
        CREATE DOMAIN app.\"Unsafe Type\" AS app.unsafe;
        CREATE TYPE app.unsafe_composite AS (v app.unsafe);
        CREATE FUNCTION app.unsafe_cmp(app.unsafe, app.unsafe) RETURNS integer
          AS 'btint8cmp' LANGUAGE internal IMMUTABLE STRICT;
        CREATE OPERATOR CLASS app.unsafe_ops DEFAULT FOR TYPE app.unsafe USING btree
          AS FUNCTION 1 app.unsafe_cmp(app.unsafe, app.unsafe);
        CREATE TYPE app.unsafe_range AS RANGE (subtype=app.unsafe);
        CREATE FUNCTION app.accepts(app.unsafe) RETURNS boolean LANGUAGE sql AS 'SELECT true';
        CREATE FUNCTION app.op(text, app.unsafe) RETURNS boolean LANGUAGE sql AS 'SELECT true';
        CREATE OPERATOR app.@@ (LEFTARG=text, RIGHTARG=app.unsafe, FUNCTION=app.op);
        CREATE FUNCTION app.unary(app.unsafe) RETURNS text LANGUAGE sql AS 'SELECT NULL::text';
        CREATE OPERATOR app.+ (RIGHTARG=app.unsafe, FUNCTION=app.unary);
        ALTER TABLE app.t ADD COLUMN secret app.unsafe;
        ALTER TABLE app.t ADD COLUMN \"end\" app.unsafe;
        ALTER TABLE app.t ADD COLUMN \"coalesce\" app.unsafe;
        ALTER TABLE app.t ADD COLUMN \"Unsafe Type Extended\" text;
        SET search_path = app, pg_catalog",
    )
    .await;
}

async fn locks(db: &mut Scratch) -> i64 {
    db.conn
        .query(
            "SELECT count(*)::pg_catalog.int8 AS n FROM pg_catalog.pg_locks
         WHERE locktype = 'advisory' AND pid = pg_backend_pid() AND objid = 2378",
        )
        .await
        .unwrap()[0]
        .try_get::<i64>("n")
        .unwrap()
        .unwrap()
}

#[tokio::test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn literal_input_is_refused_before_it_can_release_a_session_lock_on_postgres_18() {
    literal_input_lock_witness(0).await;
}

#[tokio::test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn literal_input_is_refused_before_it_can_release_a_session_lock_on_postgres_16() {
    literal_input_lock_witness(1).await;
}

async fn literal_input_lock_witness(index: usize) {
    let server = &servers()[index];
    {
        let mut db = Scratch::create(server, "literal2378").await;
        db.sql("CREATE SCHEMA app; CREATE TABLE app.t (status text)")
            .await;
        install(&mut db).await;
        let column: ColumnRef = "app.t.status".parse().unwrap();
        for (using, target) in [
            ("'x'::app.unsafe", "app.unsafe"),
            ("CAST('x' AS app.unsafe)", "app.unsafe"),
            ("'x'::unsafe", "app.unsafe"),
            ("E'x'::app.unsafe", "app.unsafe"),
            ("$tag$x$tag$::app.unsafe", "app.unsafe"),
            ("'x'\n'y'::app.unsafe", "app.unsafe"),
            ("U&'x'::app.unsafe", "app.unsafe"),
            ("'x'::app.\"unsafe\"", "app.unsafe"),
            (r#"'x'::app.U&"uns\0061fe""#, "app.unsafe"),
            ("'{x}'::app.unsafe[]", "app.unsafe"),
            ("'x'::app.unsafe_domain", "app.unsafe_domain"),
            ("'(x)'::app.unsafe_composite", "app.unsafe_composite"),
            ("'[x,x]'::app.unsafe_range", "app.unsafe_range"),
            ("'{[x,x]}'::app.unsafe_multirange", "app.unsafe_multirange"),
            ("app.accepts('x')", "app.unsafe"),
            ("accepts('x')", "app.unsafe"),
            ("status OPERATOR(app.@@) 'x'", "app.unsafe"),
            ("status = OPERATOR(app.+) 'x'", "app.unsafe"),
            (r#"'x'::app."Unsafe Type""#, "app.Unsafe Type"),
        ] {
            db.sql("SELECT pg_advisory_lock(2378)").await;
            assert_eq!(locks(&mut db).await, 1);
            let result =
                vet_conversion(&mut db.conn, &column, &ColumnType::simple("text"), using).await;
            let held = locks(&mut db).await;
            db.sql("SELECT pg_advisory_unlock_all()").await;
            assert_eq!(
                held, 1,
                "PG{} {using}: the input ran: {result:?}",
                server.label
            );
            let why = result.expect_err(using);
            assert!(
                why.contains("cannot be bound safely") && why.contains(target),
                "PG{} {using}: {why}",
                server.label
            );
            let left = db
                .conn
                .query("SELECT to_regclass('pg_temp.pbps_conversion') IS NULL AS gone")
                .await
                .unwrap();
            assert_eq!(left[0].try_get::<bool>("gone").unwrap(), Some(true));
        }
        // COALESCE is grammar, not a function overload. Here only the column's
        // type supplies the unknown parameter's type during Describe.
        db.sql("SELECT pg_advisory_lock(2378)").await;
        let ty = ColumnType::reference(TypeRef::new("app", "unsafe").unwrap());
        for name in ["secret", "coalesce"] {
            let column = "app.t"
                .parse::<pbps_model::TableName>()
                .unwrap()
                .column(name);
            let using = format!("COALESCE(\"{name}\", 'x')");
            let result = vet_conversion(&mut db.conn, &column, &ty, &using).await;
            let held = locks(&mut db).await;
            db.sql("SELECT pg_advisory_unlock_all()").await;
            assert_eq!(held, 1, "PG{}: column input ran: {result:?}", server.label);
            assert!(result.unwrap_err().contains("cannot be bound safely"));
            db.sql("SELECT pg_advisory_lock(2378)").await;
        }
        db.drop().await;
    }
}

#[tokio::test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn unrelated_extension_types_and_absent_literals_do_not_add_refusals() {
    for server in servers() {
        let mut db = Scratch::create(&server, "unrelated2378").await;
        db.sql("CREATE SCHEMA app; CREATE TABLE app.t (status text)")
            .await;
        let column: ColumnRef = "app.t.status".parse().unwrap();
        let ty = ColumnType::simple("text");
        let expressions = [
            "status = 'active'",
            "status @@ 'x'",
            "nullif(trim(status), '')::integer",
            "status::integer",
            "CAST(status AS text)",
            "CAST(status AS integer)",
            "status::double precision",
            "COALESCE(status, 'x')",
            "CASE WHEN status = 'active' THEN 1 ELSE 0 END",
            "'a' || 'b'",
            "B'101'",
            "X'ff'",
            "CAST(status AS varchar(3))",
            "CAST('2000-01-01 UTC' AS timestamp with time zone)",
            "length(status || 'x')",
            "status IS NULL",
            "status || 'app.unsafe' /* app.accepts('x') */",
            "status = E'active'",
            "status = $$active$$",
            "status = U&'active'",
        ];
        // With the unsafe closure empty, even forms that parameters cannot
        // describe keep their existing acceptance.
        for using in [
            "'x'",
            "date '2000-01-01'",
            "timestamp with time zone '2000-01-01 UTC'",
        ] {
            vet_conversion(&mut db.conn, &column, &ty, using)
                .await
                .unwrap();
        }
        // Identical expressions are admitted before and after an extension-like
        // unsafe base type, its wrappers and an unrelated operator are installed.
        for installed in [false, true] {
            if installed {
                install(&mut db).await;
            }
            for using in expressions {
                vet_conversion(&mut db.conn, &column, &ty, using)
                    .await
                    .unwrap_or_else(|why| {
                        panic!("PG{} installed={installed} {using}: {why}", server.label)
                    });
            }
        }
        vet_conversion(
            &mut db.conn,
            &column,
            &ColumnType::simple("timestamp with time zone"),
            "status AT TIME ZONE 'UTC'",
        )
        .await
        .unwrap();
        let spaced = column.table.column("Unsafe Type Extended");
        vet_conversion(
            &mut db.conn,
            &spaced,
            &ty,
            r#""Unsafe Type Extended" = 'active'"#,
        )
        .await
        .unwrap();
        for using in ["NULL::app.unsafe", "app.accepts(NULL)"] {
            let why = vet_conversion(&mut db.conn, &column, &ty, using)
                .await
                .unwrap_err();
            assert!(
                !why.contains("cannot be bound safely"),
                "PG{}: {why}",
                server.label
            );
        }
        for (using, code) in [
            ("date '2000-01-01'", "42601"),
            ("timestamp with time zone '2000-01-01 UTC'", "42601"),
            ("app.unsafe 'x'", "42601"),
            ("app.unsafe('x')", "42883"),
            ("status=+'x'", "42725"),
            ("'x'", "42P18"),
        ] {
            db.sql("SELECT pg_advisory_lock(2378)").await;
            let result = vet_conversion(&mut db.conn, &column, &ty, using).await;
            let held = locks(&mut db).await;
            db.sql("SELECT pg_advisory_unlock_all()").await;
            assert_eq!(held, 1, "PG{} {using}: input ran: {result:?}", server.label);
            let why = result.unwrap_err();
            assert!(
                why.contains(code) && why.contains("CAST('…' AS type) or '…'::type"),
                "PG{} {using}: {why}",
                server.label
            );
        }
        // A hidden function with the same spelling and unsafe arguments cannot
        // make a qualified, unrelated built-in call unsafe.
        db.sql("CREATE FUNCTION app.length(app.unsafe) RETURNS integer LANGUAGE sql AS 'SELECT 0'")
            .await;
        vet_conversion(
            &mut db.conn,
            &column,
            &ty,
            "pg_catalog.length(status || 'x')",
        )
        .await
        .unwrap();
        db.drop().await;
    }
}

#[tokio::test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn an_unreadable_literal_input_catalog_is_a_refusal() {
    for server in servers() {
        let mut db = Scratch::create(&server, "unreadable2378").await;
        db.sql("CREATE SCHEMA app; CREATE TABLE app.t (status text)")
            .await;
        install(&mut db).await;
        let role = db.role("deployer", "LOGIN").await;
        db.sql("REVOKE SELECT ON pg_catalog.pg_range FROM PUBLIC")
            .await;
        let mut conn = db.as_role(&role).await;
        let column: ColumnRef = "app.t.status".parse().unwrap();
        let literal_free = vet_conversion(
            &mut conn,
            &column,
            &ColumnType::simple("text"),
            "status::integer",
        )
        .await;
        let result = vet_conversion(
            &mut conn,
            &column,
            &ColumnType::simple("text"),
            "'x'::app.unsafe",
        )
        .await;
        db.sql("GRANT SELECT ON pg_catalog.pg_range TO PUBLIC; REVOKE SELECT ON pg_catalog.pg_type FROM PUBLIC").await;
        let modifiers = vet_conversion(
            &mut conn,
            &column,
            &ColumnType::simple("text"),
            "status::integer",
        )
        .await;
        drop(conn);
        db.drop().await;
        assert!(
            modifiers
                .unwrap_err()
                .contains("modifier-input catalog could not be read")
        );
        literal_free.unwrap_or_else(|why| {
            panic!(
                "PG{}: literal-free conversion must not read the closure: {why}",
                server.label
            )
        });
        assert!(
            result
                .unwrap_err()
                .contains("literal-input catalog could not be read")
        );
    }
}

#[tokio::test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn modifiers_are_refused_before_parse_can_release_a_session_lock_on_postgres_18() {
    modifier_lock_witness(0).await;
}

#[tokio::test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn modifiers_are_refused_before_parse_can_release_a_session_lock_on_postgres_16() {
    modifier_lock_witness(1).await;
}

async fn modifier_lock_witness(index: usize) {
    let server = &servers()[index];
    let mut db = Scratch::create(server, "modifiers2378").await;
    db.sql("CREATE SCHEMA app; CREATE TABLE app.t(status text)")
        .await;
    let column: ColumnRef = "app.t.status".parse().unwrap();
    let ty = ColumnType::simple("text");
    // With the set empty, ordinary built-in modifiers add no refusal.
    vet_conversion(&mut db.conn, &column, &ty, "CAST(status AS varchar(3))")
        .await
        .unwrap();
    for name in ["modtype", "ModType"] {
        let q = format!("\"{name}\"");
        db.sql(&format!("CREATE TYPE app.{q};
            CREATE FUNCTION app.{q}(cstring) RETURNS app.{q} AS 'int8in' LANGUAGE internal IMMUTABLE STRICT;
            CREATE FUNCTION app.{q}(app.{q}) RETURNS cstring AS 'int8out' LANGUAGE internal IMMUTABLE STRICT;
            CREATE FUNCTION app.{q}(cstring[]) RETURNS integer AS 'pg_advisory_unlock_all' LANGUAGE internal IMMUTABLE STRICT;
            CREATE TYPE app.{q}(INPUT=app.{q}, OUTPUT=app.{q}, TYPMOD_IN=app.{q}, INTERNALLENGTH=8, PASSEDBYVALUE, ALIGNMENT=double)")).await;
    }
    for (using, name) in [
        ("status::app.MODTYPE(1)", "modtype"),
        (r#"status::app."ModType"(1)"#, "ModType"),
        ("'x'::app.modtype(1)", "modtype"),
        ("CAST(status AS app.modtype(1))", "modtype"),
        ("NULL::app.modtype(1)[]", "modtype"),
        (r#"status::app."modtype" /* gap */ (1)"#, "modtype"),
        (r#"status::app.U&"modt\0079pe"(1)"#, "modtype"),
    ] {
        db.sql("SELECT pg_advisory_lock(2378)").await;
        let result = vet_conversion(&mut db.conn, &column, &ty, using).await;
        let held = locks(&mut db).await;
        db.sql("SELECT pg_advisory_unlock_all()").await;
        assert_eq!(
            held, 1,
            "PG{} {using}: modifier input ran: {result:?}",
            server.label
        );
        let why = result.unwrap_err();
        assert!(
            why.contains(&format!("type `app.{name}`"))
                && why.contains("modifier routine that is not built into PostgreSQL"),
            "PG{} {using}: {why}",
            server.label
        );
    }
    for using in [
        "status || 'app.modtype(1)'",
        "status /* app.modtype(1) */",
        "status -- app.modtype(1)\n",
    ] {
        vet_conversion(&mut db.conn, &column, &ty, using)
            .await
            .unwrap();
    }
    // Quoted spelling is exact: this type does not exist, and binding (rather
    // than the modifier guard) must say so, without calling either routine.
    let why = vet_conversion(&mut db.conn, &column, &ty, r#"status::app."MODTYPE"(1)"#)
        .await
        .unwrap_err();
    assert!(!why.contains("modifier routine"), "{why}");
    assert!(why.contains("42704"), "{why}");
    db.drop().await;
}
