use super::user_types_tests::{Scratch, servers};
use pbps_dialect::Dialect;
use pbps_model::{DropGuard, DropRequirement};
use pbps_pg::{
    Postgres,
    drop_requirements::check,
    input_path::{vet_conversion, vet_requirement},
};

fn guard(predicate: &str) -> DropGuard {
    DropGuard {
        table_uid: "t_000001".parse().unwrap(),
        table: "app.t".parse().unwrap(),
        detach_from: None,
        requirements: [(
            "c_000001".parse().unwrap(),
            DropRequirement {
                table_uid: "t_000001".parse().unwrap(),
                predicate: predicate.into(),
                required_at: "2026-10-28".into(),
                operator: "reviewer".into(),
            },
        )]
        .into(),
    }
}

async fn checked(db: &mut Scratch, guard: &DropGuard) -> Result<(), String> {
    let framing = Postgres::new().transaction_framing(&pbps_model::SessionZone::utc());
    db.conn.begin(&framing).await.unwrap();
    let result = check(&mut db.conn, guard).await;
    db.conn.rollback(&framing).await.unwrap();
    result
}

#[tokio::test]
#[ignore = "needs live PostgreSQL 18"]
async fn requirement_parentheses_cannot_splice_the_binding_or_count_wrapper_on_postgres_18() {
    parentheses(0).await;
}
#[tokio::test]
#[ignore = "needs live PostgreSQL 16"]
async fn requirement_parentheses_cannot_splice_the_binding_or_count_wrapper_on_postgres_16() {
    parentheses(1).await;
}

async fn parentheses(index: usize) {
    let mut db = Scratch::create(&servers()[index], "requirements_parens2400").await;
    db.sql("CREATE SCHEMA app; CREATE TABLE app.t (amount integer, \")\" integer); INSERT INTO app.t VALUES (1,1)").await;
    let table = "app.t".parse().unwrap();
    for predicate in ["false) AND (false", "(false", "false)"] {
        let message = format!(
            "the row requirement `{predicate}` of app.t has unbalanced parentheses; it must be one complete expression"
        );
        assert_eq!(
            vet_requirement(
                &mut db.conn,
                pbps_pg::pins::Within::OwnTransaction,
                &table,
                predicate
            )
            .await
            .unwrap_err(),
            message
        );
        assert_eq!(
            checked(&mut db, &guard(predicate)).await.unwrap_err(),
            message
        );
    }
    // Only code contributes depth: quoted names, plain/E/dollar strings and
    // nested or line comments may themselves hold unmatched parentheses.
    let balanced =
        "((amount = 1)) AND \")\" = 1 AND '(' = '(' AND E')' = $$)$$ /* ) /* ( */ */ -- (\n";
    vet_requirement(
        &mut db.conn,
        pbps_pg::pins::Within::OwnTransaction,
        &table,
        balanced,
    )
    .await
    .unwrap();
    checked(&mut db, &guard(balanced)).await.unwrap();
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs live PostgreSQL 18"]
async fn requirements_bind_only_own_columns_and_builtin_nonvolatile_code_on_postgres_18() {
    binding(0).await;
}
#[tokio::test]
#[ignore = "needs live PostgreSQL 16"]
async fn requirements_bind_only_own_columns_and_builtin_nonvolatile_code_on_postgres_16() {
    binding(1).await;
}

async fn binding(index: usize) {
    let mut db = Scratch::create(&servers()[index], "requirements_binding2400").await;
    db.sql("CREATE SCHEMA app; CREATE TABLE app.t (amount integer NOT NULL, cents integer, status text); CREATE TABLE app.other (amount integer);
        CREATE FUNCTION app.user_check(integer) RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT true'").await;
    let table = "app.t".parse().unwrap();
    assert_eq!(
        vet_requirement(
            &mut db.conn,
            pbps_pg::pins::Within::OwnTransaction,
            &table,
            "cents = amount * 100 AND length(status) > 0"
        )
        .await
        .unwrap(),
        ["amount".into(), "cents".into(), "status".into()].into()
    );
    let udf = vet_requirement(
        &mut db.conn,
        pbps_pg::pins::Within::OwnTransaction,
        &table,
        "app.user_check(amount)",
    )
    .await
    .unwrap_err();
    assert_eq!(
        udf,
        "the row requirement of app.t calls the function `app.user_check(integer)` (owned by `postgres`), which is not built into PostgreSQL and cannot be proven to run only built-in code: it has a string body (AS '...' or AS $$...$$), which the catalog keeps as text and not as a parsed tree; only a BEGIN ATOMIC or RETURN body can be proven; nothing has been changed"
    );
    assert_eq!(
        vet_requirement(
            &mut db.conn,
            pbps_pg::pins::Within::OwnTransaction,
            &table,
            "random() > 0"
        )
        .await
        .unwrap_err(),
        "the row requirement `random() > 0` of app.t calls a volatile routine, which cannot be used in a row requirement"
    );
    for (predicate, state) in [
        ("app.other.amount = amount", "42P01"),
        ("EXISTS (SELECT 1 FROM app.other)", "0A000"),
        ("amount", "42804"),
    ] {
        let refused = vet_requirement(
            &mut db.conn,
            pbps_pg::pins::Within::OwnTransaction,
            &table,
            predicate,
        )
        .await
        .unwrap_err();
        assert_eq!(
            refused,
            format!(
                "the row requirement `{predicate}` of app.t does not bind against this table's columns (SQLSTATE {state}): only this table's columns and no subqueries are allowed"
            )
        );
    }
    // An unused custom modifier column must not execute code while LIKE copies
    // its catalog identity, and must not reject ordinary built-in predicates.
    db.sql("CREATE TYPE app.modtype;
        CREATE FUNCTION app.modtype(cstring) RETURNS app.modtype AS 'int8in' LANGUAGE internal IMMUTABLE STRICT;
        CREATE FUNCTION app.modtype(app.modtype) RETURNS cstring AS 'int8out' LANGUAGE internal IMMUTABLE STRICT;
        CREATE FUNCTION app.modtype(cstring[]) RETURNS integer AS 'pg_advisory_unlock_all' LANGUAGE internal IMMUTABLE STRICT;
        CREATE TYPE app.modtype(INPUT=app.modtype,OUTPUT=app.modtype,TYPMOD_IN=app.modtype,INTERNALLENGTH=8,PASSEDBYVALUE,ALIGNMENT=double);
        ALTER TABLE app.t ADD COLUMN hidden app.modtype(1); SELECT pg_advisory_lock(2400)").await;
    vet_requirement(
        &mut db.conn,
        pbps_pg::pins::Within::OwnTransaction,
        &table,
        "cents = amount * 100",
    )
    .await
    .unwrap();
    let locks = db.conn.query("SELECT count(*)::bigint AS n FROM pg_locks WHERE pid=pg_backend_pid() AND locktype='advisory'").await.unwrap();
    assert_eq!(locks[0].try_get::<i64>("n").unwrap(), Some(1));
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs live PostgreSQL 18"]
async fn requirements_refuse_nulls_query_errors_rls_and_missing_privilege_on_postgres_18() {
    refusals(0).await;
}
#[tokio::test]
#[ignore = "needs live PostgreSQL 16"]
async fn requirements_refuse_nulls_query_errors_rls_and_missing_privilege_on_postgres_16() {
    refusals(1).await;
}

async fn refusals(index: usize) {
    let mut db = Scratch::create(&servers()[index], "requirements_refuse2400").await;
    db.sql("CREATE SCHEMA app; CREATE TABLE app.t (amount integer, cents integer); INSERT INTO app.t VALUES (1,100),(1,NULL),(1,99)").await;
    assert_eq!(
        checked(&mut db, &guard("cents = amount * 100"))
            .await
            .unwrap_err(),
        "2 row(s) of app.t violate the row requirement `cents = amount * 100`; nothing was removed"
    );
    assert_eq!(
        checked(&mut db, &guard("amount / 0 > 0"))
            .await
            .unwrap_err(),
        "the row requirement `amount / 0 > 0` of app.t could not be evaluated (SQLSTATE 22012); a query or privilege failure never permits removal"
    );
    checked(&mut db, &guard("amount = 1")).await.unwrap();
    let role = db.role("limited", "LOGIN").await;
    db.sql(&format!(
        "GRANT USAGE ON SCHEMA app TO {role}; GRANT UPDATE ON app.t TO {role}"
    ))
    .await;
    let framing = Postgres::new().transaction_framing(&pbps_model::SessionZone::utc());
    let mut actor = db.as_role(&role).await;
    actor.begin(&framing).await.unwrap();
    let refused = check(&mut actor, &guard("amount = 1")).await.unwrap_err();
    assert!(refused.contains("42501"), "{refused}");
    actor.rollback(&framing).await.unwrap();
    db.sql(&format!("ALTER TABLE app.t OWNER TO {role}; ALTER TABLE app.t ENABLE ROW LEVEL SECURITY; ALTER TABLE app.t FORCE ROW LEVEL SECURITY; CREATE POLICY hidden ON app.t USING (false)")).await;
    actor.begin(&framing).await.unwrap();
    assert_eq!(
        check(&mut actor, &guard("cents = amount * 100"))
            .await
            .unwrap_err(),
        "the row requirement of app.t cannot count all rows while row security is active"
    );
    actor.rollback(&framing).await.unwrap();
    drop(actor);
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs live PostgreSQL 18"]
async fn the_drop_lock_covers_partitions_and_blocks_a_writer_through_the_drop_on_postgres_18() {
    partition_lock(0).await;
}
#[tokio::test]
#[ignore = "needs live PostgreSQL 16"]
async fn the_drop_lock_covers_partitions_and_blocks_a_writer_through_the_drop_on_postgres_16() {
    partition_lock(1).await;
}

async fn partition_lock(index: usize) {
    let mut db = Scratch::create(&servers()[index], "requirements_lock2400").await;
    db.sql("CREATE SCHEMA app; CREATE TABLE app.t (id integer, amount integer) PARTITION BY RANGE(id); CREATE TABLE app.leaf PARTITION OF app.t FOR VALUES FROM (0) TO (100); INSERT INTO app.t VALUES (1,1)").await;
    let role = db.role("writer", "LOGIN").await;
    db.sql(&format!(
        "GRANT USAGE ON SCHEMA app TO {role}; GRANT INSERT ON app.t,app.leaf TO {role}"
    ))
    .await;
    let mut writer = db.as_role(&role).await;
    let pid: i32 = writer
        .query("SELECT pg_backend_pid() AS pid")
        .await
        .unwrap()[0]
        .try_get("pid")
        .unwrap()
        .unwrap();
    let framing = Postgres::new().transaction_framing(&pbps_model::SessionZone::utc());
    db.conn.begin(&framing).await.unwrap();
    check(&mut db.conn, &guard("amount = 1")).await.unwrap();
    // This return is the test-only barrier between the production check and
    // the drop. Inspect real locks before allowing the writer to proceed.
    let locks = db.conn.query("SELECT count(*)::bigint AS n FROM pg_locks WHERE pid=pg_backend_pid() AND granted AND mode='AccessExclusiveLock' AND relation IN ('app.t'::regclass,'app.leaf'::regclass)").await.unwrap();
    assert_eq!(locks[0].try_get::<i64>("n").unwrap(), Some(2));
    let task = tokio::spawn(async move {
        writer
            .execute("INSERT INTO app.leaf (id,amount) VALUES (2,0)")
            .await
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waiting = db
            .conn
            .query(&format!(
                "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE pid={pid} AND NOT granted) AS waiting"
            ))
            .await
            .unwrap();
        if waiting[0].try_get::<bool>("waiting").unwrap() == Some(true) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "writer never reached the real lock wait"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    db.conn
        .execute("ALTER TABLE app.t DROP COLUMN amount")
        .await
        .unwrap();
    db.conn.commit(&framing).await.unwrap();
    let refused = task.await.unwrap().unwrap_err();
    assert_eq!(refused.server_error_code().as_deref(), Some("42703"));
    let rows = db
        .conn
        .query("SELECT count(*)::bigint AS n FROM app.t")
        .await
        .unwrap();
    assert_eq!(rows[0].try_get::<i64>("n").unwrap(), Some(1));
    db.drop().await;
}

/// The table-expression sites share tier 1 of #2563 with the domain sites: a
/// row requirement and a conversion `USING` admit a function whose stored body
/// proves built-in-only, and refuse what the proof refuses, by name.
#[tokio::test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn requirements_and_conversions_admit_a_function_with_a_built_in_only_body() {
    use pbps_model::ColumnType;
    for server in servers() {
        let mut db = Scratch::create(&server, "requirements_tier1_2563").await;
        let label = server.label;
        let stranger = db.role("stranger", "NOLOGIN").await;
        db.sql(&format!(
            "CREATE SCHEMA app; CREATE TABLE app.t (amount integer NOT NULL, cents integer);
             GRANT USAGE ON SCHEMA app TO {stranger};
             CREATE FUNCTION app.positive(integer) RETURNS boolean LANGUAGE sql IMMUTABLE
                 BEGIN ATOMIC SELECT $1 > 0; END;
             CREATE FUNCTION app.chained(integer) RETURNS boolean LANGUAGE sql IMMUTABLE
                 RETURN app.positive($1) AND $1 < 1000;
             CREATE FUNCTION app.clamped(integer) RETURNS integer LANGUAGE sql IMMUTABLE
                 BEGIN ATOMIC SELECT CASE WHEN app.chained($1) THEN $1 ELSE 0 END; END;
             CREATE FUNCTION app.stable_check(integer) RETURNS boolean LANGUAGE sql STABLE
                 BEGIN ATOMIC SELECT $1 > 0; END;
             CREATE FUNCTION app.plpgsql_check(integer) RETURNS boolean LANGUAGE plpgsql IMMUTABLE
                 AS $$ BEGIN RETURN $1 > 0; END $$;
             CREATE FUNCTION app.calls_plpgsql(integer) RETURNS boolean LANGUAGE sql IMMUTABLE
                 BEGIN ATOMIC SELECT app.plpgsql_check($1); END;
             CREATE FUNCTION app.volatile_inside(integer) RETURNS boolean LANGUAGE sql IMMUTABLE
                 BEGIN ATOMIC SELECT random() < 2 AND $1 > 0; END;
             CREATE FUNCTION app.theirs(integer) RETURNS boolean LANGUAGE sql IMMUTABLE
                 BEGIN ATOMIC SELECT $1 > 0; END;
             ALTER FUNCTION app.theirs(integer) OWNER TO {stranger};"
        ))
        .await;
        let table: pbps_model::TableName = "app.t".parse().unwrap();
        // Admitted: alone, in a chain, and with the columns it reads reported.
        for predicate in ["app.positive(amount)", "app.chained(cents) AND amount > 0"] {
            let columns = vet_requirement(
                &mut db.conn,
                pbps_pg::pins::Within::OwnTransaction,
                &table,
                predicate,
            )
            .await
            .unwrap_or_else(|why| panic!("{predicate} on {label}: {why}"));
            assert!(!columns.is_empty(), "{predicate}");
        }
        // Refused by name, with the reason and the nested function that caused it.
        for (predicate, identity, phrase) in [
            (
                "app.stable_check(amount)",
                "app.stable_check(integer)",
                "declared STABLE, not IMMUTABLE",
            ),
            (
                "app.plpgsql_check(amount)",
                "app.plpgsql_check(integer)",
                "language `plpgsql`",
            ),
            (
                "app.calls_plpgsql(amount)",
                "app.calls_plpgsql(integer)",
                "calls `app.plpgsql_check(integer)`, which is written in the language `plpgsql`",
            ),
        ] {
            let why = vet_requirement(
                &mut db.conn,
                pbps_pg::pins::Within::OwnTransaction,
                &table,
                predicate,
            )
            .await
            .expect_err(predicate);
            assert!(
                why.contains(identity) && why.contains(phrase),
                "{predicate} on {label}: {why}"
            );
            assert!(why.ends_with("nothing has been changed"), "{why}");
        }
        // A volatile built-in inside a proven body is still a volatile call
        // in a row requirement, which cannot hold one.
        let why = vet_requirement(
            &mut db.conn,
            pbps_pg::pins::Within::OwnTransaction,
            &table,
            "app.volatile_inside(amount)",
        )
        .await
        .expect_err("volatile inside");
        assert!(why.contains("volatile routine"), "{why}");
        // The owner rule, with no role declared trusted at this site.
        let why = vet_requirement(
            &mut db.conn,
            pbps_pg::pins::Within::OwnTransaction,
            &table,
            "app.theirs(amount)",
        )
        .await
        .expect_err("a stranger's function");
        assert!(
            why.contains("app.theirs(integer)") && why.contains(&stranger) && why.contains("owner"),
            "on {label}: {why}"
        );

        // The same proof judges a conversion's `USING`.
        let column = table.column("amount");
        let integer = ColumnType::simple("integer");
        vet_conversion(&mut db.conn, &column, &integer, "app.clamped(amount)")
            .await
            .unwrap_or_else(|why| panic!("a conversion on {label}: {why}"));
        for (using, identity, phrase) in [
            (
                "CASE WHEN app.stable_check(amount) THEN 1 ELSE 0 END",
                "app.stable_check(integer)",
                "declared STABLE",
            ),
            (
                "CASE WHEN app.calls_plpgsql(amount) THEN 1 ELSE 0 END",
                "app.calls_plpgsql(integer)",
                "language `plpgsql`",
            ),
        ] {
            let why = vet_conversion(&mut db.conn, &column, &integer, using)
                .await
                .expect_err(using);
            assert!(
                why.contains(identity) && why.contains(phrase),
                "{using} on {label}: {why}"
            );
        }
        db.drop().await;
    }
}
