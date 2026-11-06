//! A table expression bound as a view over the real table is judged as the
//! same expression is judged from `conbin` (DEC-2563.8): same verdict, same
//! reasons, for a trusted routine, an untrusted one and a constant call.

use pbps_db::{Conn, Driver};
use pbps_model::{Change, CheckConstraint, ObjectLocalName, RoleName, TableName};

use super::{Moment, Vetted, vet_change};
use crate::admission::Admission;
use crate::input_path::{Part, Use, judge_trees, judge_view};
use crate::pins::Within;

fn used() -> Use {
    Use {
        ty: "the expression under test".to_owned(),
        part: Part::TableExpression,
    }
}

async fn text(conn: &mut Conn, sql: &str, column: &str) -> String {
    let rows = conn.query(sql).await.unwrap();
    rows[0].try_get::<&str>(column).unwrap().unwrap().to_owned()
}

/// What the engine stores for `expression` as a CHECK of the real table (the
/// statement is rolled back), and as the one target entry of a view over it.
async fn both_trees(conn: &mut Conn, expression: &str) -> (String, String) {
    conn.execute("BEGIN").await.unwrap();
    conn.execute(&format!(
        "ALTER TABLE app.t ADD CONSTRAINT zz CHECK ({expression}\n)"
    ))
    .await
    .unwrap();
    let check = text(
        conn,
        "SELECT conbin::pg_catalog.text AS tree FROM pg_catalog.pg_constraint
          WHERE conname = 'zz' AND conrelid = 'app.t'::pg_catalog.regclass",
        "tree",
    )
    .await;
    conn.execute("ROLLBACK").await.unwrap();
    conn.execute(&format!(
        "CREATE TEMPORARY VIEW pg_temp.pbps_vt AS SELECT ({expression}\n) IS TRUE AS c0 FROM app.t"
    ))
    .await
    .unwrap();
    let rule = text(
        conn,
        "SELECT r.ev_action::pg_catalog.text AS rule FROM pg_catalog.pg_rewrite r
          WHERE r.ev_class = 'pg_temp.pbps_vt'::pg_catalog.regclass",
        "rule",
    )
    .await;
    conn.execute("DROP VIEW pg_temp.pbps_vt").await.unwrap();
    (check, rule)
}

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_view_over_the_table_is_judged_as_its_check_tree_is_on_both_majors() {
    for (variable, major) in [("PBPS_TEST_PG_OLD_DB", 16), ("PBPS_TEST_PG_DB", 18)] {
        let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
        let token = crate::catalog::probe_token().replace('-', "_");
        let (name, role) = (format!("pbps_vt_db_{token}"), format!("pbps_vt_r_{token}"));
        let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
        admin
            .execute(&format!("CREATE DATABASE {name}"))
            .await
            .unwrap();
        let connection = format!("{base} dbname={name}");
        let theirs = role.clone();
        let outcome = tokio::task::LocalSet::new()
            .run_until(async move {
                tokio::task::spawn_local(async move {
                    let mut conn = Conn::connect(Driver::Postgres, &connection).await.unwrap();
                    let me = text(&mut conn, "SELECT current_user::text AS me", "me").await;
                    let function = |name: &str, args: &str| {
                        format!(
                            "CREATE FUNCTION app.{name}({args}) RETURNS {} LANGUAGE sql \
                             IMMUTABLE SET search_path = pg_catalog AS 'select {}'",
                            if args == "app.t" { "boolean" } else { "int" },
                            if args == "app.t" { "true" } else { "1" },
                        )
                    };
                    admin_setup(&mut conn, &theirs, &function).await;
                    let admission = Admission::without_review([RoleName::new(me)]);
                    let cases = [
                        ("built-in only", "id > 0"),
                        ("trusted, whole row", "app.mine(t)"),
                        ("trusted, whole row as t.*", "app.mine(t.*)"),
                        ("trusted, constant call", "app.mine_c(1) > 0"),
                        ("untrusted, whole row", "app.theirs(t)"),
                        ("untrusted, whole row as t.*", "app.theirs(t.*)"),
                        ("untrusted, constant call", "app.theirs_c(1) > 0"),
                        (
                            "untrusted behind a trusted one",
                            "app.mine(t) AND app.theirs_c(id) > 0",
                        ),
                    ];
                    let mut seen = Vec::new();
                    for (label, expression) in cases {
                        let (check, rule) = both_trees(&mut conn, expression).await;
                        let refuse = |why: &str| format!("the expression {why}");
                        let from_check = judge_trees(
                            &mut conn,
                            &admission,
                            vec![(used(), check)],
                            false,
                            &refuse,
                        )
                        .await
                        .map(|_| ());
                        let from_view =
                            judge_view(&mut conn, &admission, &rule, vec![used()], &refuse)
                                .await
                                .map(|_| ());
                        assert_eq!(
                            from_check, from_view,
                            "PG{major} {label}: the view's verdict differs from the CHECK's"
                        );
                        seen.push((label, from_view.is_ok()));
                    }
                    seen
                })
                .await
            })
            .await;
        let _ = admin
            .execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .await;
        let _ = admin.execute(&format!("DROP ROLE IF EXISTS {role}")).await;
        let seen = outcome.unwrap();
        let admitted: Vec<_> = seen.iter().filter(|(_, ok)| *ok).map(|(l, _)| *l).collect();
        let refused: Vec<_> = seen
            .iter()
            .filter(|(_, ok)| !*ok)
            .map(|(l, _)| *l)
            .collect();
        // The comparison is only a witness if both verdicts occur.
        assert!(!admitted.is_empty(), "PG{major}: nothing was admitted");
        assert!(!refused.is_empty(), "PG{major}: nothing was refused");
        assert!(
            refused.contains(&"untrusted, whole row as t.*"),
            "PG{major}"
        );
        assert!(admitted.contains(&"trusted, whole row as t.*"), "PG{major}");
    }
}

async fn admin_setup(conn: &mut Conn, role: &str, function: &dyn Fn(&str, &str) -> String) {
    conn.execute(&format!("CREATE ROLE {role}")).await.unwrap();
    conn.execute("CREATE SCHEMA app").await.unwrap();
    conn.execute("CREATE TABLE app.t (id int, email text)")
        .await
        .unwrap();
    for (name, args) in [
        ("mine", "app.t"),
        ("mine_c", "int"),
        ("theirs", "app.t"),
        ("theirs_c", "int"),
    ] {
        conn.execute(&function(name, args)).await.unwrap();
    }
    for (name, args) in [("theirs", "app.t"), ("theirs_c", "int")] {
        conn.execute(&format!(
            "ALTER FUNCTION app.{name}({args}) OWNER TO {role}"
        ))
        .await
        .unwrap();
    }
}

async fn vet(
    conn: &mut Conn,
    admission: &Admission,
    moment: Moment,
    expression: &str,
) -> Result<Vetted, String> {
    vet_change(
        conn,
        admission,
        Within::OwnTransaction,
        moment,
        &add_check(expression),
    )
    .await
}

fn add_check(expression: &str) -> Change {
    Change::AddCheck {
        table: TableName::new("app", "t"),
        name: ObjectLocalName::new("ck"),
        constraint: CheckConstraint {
            expression: expression.to_owned(),
        },
    }
}

/// What the binder says about a CHECK, as a role that holds `USAGE` on the
/// schema and nothing else on the table or the routines: the view needs no more
/// than that, where the copy it replaced needed `SELECT` (DEC-2563.8). A function
/// that takes another composite type stays unbound, and a sublink is refused, so
/// neither is admitted by the view that binds it.
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn the_view_binds_for_a_role_with_usage_only_and_refuses_what_it_must_not_admit() {
    for (variable, major) in [("PBPS_TEST_PG_OLD_DB", 16), ("PBPS_TEST_PG_DB", 18)] {
        let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
        let token = crate::catalog::probe_token().replace('-', "_");
        let (name, role) = (format!("pbps_vu_db_{token}"), format!("pbps_vu_r_{token}"));
        let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
        admin
            .execute(&format!("CREATE DATABASE {name}"))
            .await
            .unwrap();
        let connection = format!("{base} dbname={name}");
        let limited = role.clone();
        let outcome = tokio::task::LocalSet::new()
            .run_until(async move {
                tokio::task::spawn_local(async move {
                    let mut conn = Conn::connect(Driver::Postgres, &connection).await.unwrap();
                    let me = text(&mut conn, "SELECT current_user::text AS me", "me").await;
                    conn.execute(&format!("CREATE ROLE {limited}"))
                        .await
                        .unwrap();
                    conn.execute(
                        "CREATE SCHEMA app; CREATE TABLE app.t (id int, email text); \
                         CREATE FUNCTION app.mine(app.t) RETURNS int LANGUAGE sql IMMUTABLE \
                           SET search_path = pg_catalog AS 'select 1'; \
                         CREATE FUNCTION app.other(pg_catalog.pg_class) RETURNS int \
                           LANGUAGE sql IMMUTABLE SET search_path = pg_catalog AS 'select 1'; \
                         REVOKE EXECUTE ON FUNCTION app.mine(app.t), \
                           app.other(pg_catalog.pg_class) FROM PUBLIC",
                    )
                    .await
                    .unwrap();
                    conn.execute(&format!("GRANT USAGE ON SCHEMA app TO {limited}"))
                        .await
                        .unwrap();
                    conn.execute(&format!("SET ROLE {limited}")).await.unwrap();
                    let admission = Admission::without_review([RoleName::new(me)]);
                    let usage_only = vet(
                        &mut conn,
                        &admission,
                        Moment::Early,
                        "app.mine(t) > 0 AND app.mine(t.*) > 0",
                    )
                    .await;
                    let other_early =
                        vet(&mut conn, &admission, Moment::Early, "app.other(t) > 0").await;
                    let other_at_statement =
                        vet(&mut conn, &admission, Moment::Statement, "app.other(t) > 0").await;
                    let sublink = vet(&mut conn, &admission, Moment::Early, "(SELECT 1) > 0").await;
                    // A CHECK must be a boolean: selected `IS TRUE`, a number is
                    // refused by the engine at bind, as it is at the statement.
                    let number = vet(&mut conn, &admission, Moment::Early, "id + 1").await;
                    // The copy needs SELECT, which this role lacks: that is the
                    // contrast the view removes.
                    let copy = conn
                        .execute("CREATE TEMPORARY TABLE pg_temp.zz (LIKE app.t)")
                        .await;
                    (
                        usage_only,
                        other_early,
                        other_at_statement,
                        sublink,
                        number,
                        copy.is_err(),
                    )
                })
                .await
            })
            .await;
        let _ = admin
            .execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .await;
        let _ = admin.execute(&format!("DROP ROLE IF EXISTS {role}")).await;
        let (usage_only, other_early, other_at_statement, sublink, number, copy_refused) =
            outcome.unwrap();
        assert_eq!(usage_only, Ok(Vetted::Admitted), "PG{major}: USAGE only");
        assert!(copy_refused, "PG{major}: the copy needs SELECT");
        match other_early {
            Ok(Vetted::Unjudged(notes)) => assert!(
                notes.iter().any(|n| n.contains("judged at apply only")),
                "PG{major}: {notes:?}"
            ),
            other => panic!("PG{major}: another composite type must stay unbound: {other:?}"),
        }
        let at_statement = other_at_statement.expect_err("refused at its statement");
        assert!(
            at_statement.contains("could not be bound just before they run"),
            "PG{major}: {at_statement}"
        );
        assert!(
            matches!(&number, Ok(Vetted::Unjudged(notes)) if notes.iter().any(|n| n.contains("42804"))),
            "PG{major}: a number is not a CHECK: {number:?}"
        );
        let refused = sublink.expect_err("a sublink is never admitted");
        assert!(
            refused.contains("more than a list of plain expressions"),
            "PG{major}: {refused}"
        );
    }
}

/// A binding that stood leaves nothing in the session, so a second one in the
/// same transaction builds: the apply vets each statement in its own transaction
/// but one, and a view left behind would make the next fail to build and refuse a
/// valid plan.
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_binding_that_stood_leaves_no_view_so_the_next_one_in_the_transaction_builds() {
    for (variable, major) in [("PBPS_TEST_PG_OLD_DB", 16), ("PBPS_TEST_PG_DB", 18)] {
        let base = std::env::var(variable).expect("live PostgreSQL fixture setting");
        let name = format!(
            "pbps_vl_db_{}",
            crate::catalog::probe_token().replace('-', "_")
        );
        let mut admin = Conn::connect(Driver::Postgres, &base).await.unwrap();
        admin
            .execute(&format!("CREATE DATABASE {name}"))
            .await
            .unwrap();
        let connection = format!("{base} dbname={name}");
        let outcome = tokio::task::LocalSet::new()
            .run_until(async move {
                tokio::task::spawn_local(async move {
                    let mut conn = Conn::connect(Driver::Postgres, &connection).await.unwrap();
                    let me = text(&mut conn, "SELECT current_user::text AS me", "me").await;
                    conn.execute("CREATE SCHEMA app; CREATE TABLE app.t (id int, email text)")
                        .await
                        .unwrap();
                    let admission = Admission::without_review([RoleName::new(me)]);
                    conn.execute("BEGIN").await.unwrap();
                    let mut seen = Vec::new();
                    for expression in ["id > 0", "email IS NOT NULL"] {
                        seen.push(
                            vet_change(
                                &mut conn,
                                &admission,
                                Within::CallersTransaction,
                                Moment::Statement,
                                &add_check(expression),
                            )
                            .await,
                        );
                    }
                    let left = text(
                        &mut conn,
                        "SELECT count(*)::text AS n FROM pg_catalog.pg_class
                          WHERE relnamespace = pg_catalog.pg_my_temp_schema()",
                        "n",
                    )
                    .await;
                    conn.execute("ROLLBACK").await.unwrap();
                    (seen, left)
                })
                .await
            })
            .await;
        let _ = admin
            .execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .await;
        let (seen, left) = outcome.unwrap();
        assert_eq!(
            seen,
            vec![Ok(Vetted::Admitted), Ok(Vetted::Admitted)],
            "PG{major}"
        );
        assert_eq!(left, "0", "PG{major}: the binding left a relation behind");
    }
}
