//! Live acceptance of the admission predicate at the expressions that already
//! stand on a table a plan writes rows to or rewrites (#2563, DEC-2563.9 and
//! DEC-2563.10).
//!
//! The first group drives the reference-data guard directly, one statement kind
//! at a time, against a table that carries every kind of expression, each calling
//! its own routine that sets no `search_path`. A refusal names the routines the
//! statement runs, so the set it names is the measured set (the table in
//! `pbps_pg::existing_expressions`). The second group runs whole plans.
use super::*;

use pbps_dialect::{Dialect, RowOperation, RowWrite};
use pbps_pg::admission::Admission;
use pbps_pg::data_triggers::{self, Dropped};
use std::collections::BTreeSet;

/// Every routine the fixture calls, by the identity a refusal shows.
const ROUTINES: [&str; 11] = [
    "app.f_check(integer)",
    "app.f_default()",
    "app.f_gen(integer)",
    "app.f_key(integer)",
    "app.f_filter(integer)",
    "app.f_dom(integer)",
    "app.f_domdef()",
    "app.f_pol(integer)",
    "app.f_cchk(integer)",
    "app.f_ckey(integer)",
    "app.f_other(integer)",
];

/// The routines of the tests that build a schema of their own: a refusal names
/// them too, but the shared fixture has none of them to alter.
const OWN_ROUTINES: [&str; 3] = ["app.f_defa()", "app.f_defb()", "app.f_defx()"];

/// A table with every kind of expression, each calling a routine of its own, and
/// a child table whose foreign key cascades.
fn fixture(owner: &str) -> String {
    let functions = [
        ("f_check", "integer", "boolean", "IMMUTABLE"),
        ("f_gen", "integer", "integer", "IMMUTABLE"),
        ("f_key", "integer", "integer", "IMMUTABLE"),
        ("f_filter", "integer", "boolean", "IMMUTABLE"),
        ("f_dom", "integer", "boolean", "IMMUTABLE"),
        ("f_pol", "integer", "boolean", "IMMUTABLE"),
        ("f_cchk", "integer", "boolean", "IMMUTABLE"),
        ("f_ckey", "integer", "integer", "IMMUTABLE"),
        ("f_other", "integer", "boolean", "IMMUTABLE"),
    ];
    let mut sql = String::from("CREATE SCHEMA app;");
    for (name, arg, returns, volatility) in functions {
        let body = if returns == "boolean" { "true" } else { "1" };
        sql.push_str(&format!(
            "CREATE FUNCTION app.{name}({arg}) RETURNS {returns} LANGUAGE plpgsql {volatility} \
             AS $$BEGIN RETURN {body}; END$$;"
        ));
    }
    sql.push_str(
        "CREATE FUNCTION app.f_default() RETURNS integer LANGUAGE plpgsql AS $$BEGIN RETURN 1; END$$;
         CREATE FUNCTION app.f_domdef() RETURNS integer LANGUAGE plpgsql AS $$BEGIN RETURN 1; END$$;
         CREATE DOMAIN app.d AS integer DEFAULT app.f_domdef() CONSTRAINT dc CHECK (app.f_dom(VALUE));
         CREATE TABLE app.t (
             id integer PRIMARY KEY,
             a integer,
             b integer,
             c app.d,
             d integer DEFAULT app.f_default(),
             g integer GENERATED ALWAYS AS (app.f_gen(a)) STORED,
             CONSTRAINT ck CHECK (app.f_check(b))
         );
         CREATE INDEX ix_key ON app.t ((app.f_key(a)));
         CREATE INDEX ix_filter ON app.t (id) WHERE app.f_other(b) AND app.f_filter(b);
         CREATE TABLE app.u (
             id integer PRIMARY KEY,
             t_id integer REFERENCES app.t (id) ON UPDATE CASCADE ON DELETE CASCADE,
             CONSTRAINT uck CHECK (app.f_cchk(id))
         );
         CREATE INDEX ux_key ON app.u ((app.f_ckey(t_id)));",
    );
    if !owner.is_empty() {
        sql.push_str(&format!(
            "ALTER TABLE app.t OWNER TO {owner}; ALTER TABLE app.u OWNER TO {owner};
             GRANT USAGE ON SCHEMA app TO {owner};"
        ));
    }
    sql
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// What `prepare` says to `operation` on `table`, with `as_role` set for the
/// transaction: `Ok` or the refusal's text. Rolled back.
async fn prepared(
    conn: &mut pbps_db::Conn,
    admission: &Admission,
    table: &str,
    operation: RowOperation,
    dropped: &Dropped,
    as_role: Option<&str>,
) -> Result<(), String> {
    let baseline = pbps_pg::catalog::introspect(conn).await.unwrap().schema;
    let write = RowWrite {
        table: table.parse().unwrap(),
        operation,
    };
    let framing = pbps_pg::Postgres::new().transaction_framing(&pbps_model::SessionZone::utc());
    conn.begin(&framing).await.unwrap();
    if let Some(role) = as_role {
        conn.execute(&format!("SET LOCAL ROLE {role}"))
            .await
            .unwrap();
    }
    let result = match data_triggers::prepare(
        conn,
        admission,
        std::slice::from_ref(&write),
        &baseline,
        dropped,
    )
    .await
    {
        // The recheck just before the row statement is the authority, and it
        // allows nothing the plan meant to remove.
        Ok(guard) => data_triggers::check(conn, admission, &write, &guard)
            .await
            .map_err(|e| format!("{e:?}")),
        Err(e) => Err(format!("{e:?}")),
    };
    conn.rollback(&framing).await.unwrap();
    result
}

fn update(columns: &[&str]) -> RowOperation {
    RowOperation::Update {
        columns: columns.iter().map(|c| (*c).into()).collect(),
    }
}

/// The routines a refusal names.
fn named(text: &str) -> BTreeSet<&'static str> {
    ROUTINES
        .iter()
        .chain(OWN_ROUTINES.iter())
        .copied()
        .filter(|routine| text.contains(routine))
        .collect()
}

fn each_server(f: impl Fn(i32, &str)) {
    for (version, url) in [(16, old_server()), (18, server())] {
        f(version, &url);
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn each_statement_is_judged_for_exactly_the_expressions_it_runs() {
    each_server(|version, url| {
        let own = OwnDatabase::new(url, &format!("ee2563-matrix-{version}"));
        on_server(own.connection(), &fixture(""));
        let cases: Vec<(&str, RowOperation, Vec<&str>)> = vec![
            (
                "an INSERT runs the check, the generated expression, every index, the domain and the defaults",
                RowOperation::Insert {
                    columns: Default::default(),
                },
                vec![
                    "app.f_check(integer)",
                    "app.f_default()",
                    "app.f_gen(integer)",
                    "app.f_key(integer)",
                    "app.f_filter(integer)",
                    "app.f_other(integer)",
                    "app.f_dom(integer)",
                    "app.f_domdef()",
                ],
            ),
            (
                "a bulk copy into a table runs what an INSERT runs",
                RowOperation::Copy {
                    columns: Default::default(),
                },
                vec![
                    "app.f_check(integer)",
                    "app.f_default()",
                    "app.f_gen(integer)",
                    "app.f_key(integer)",
                    "app.f_filter(integer)",
                    "app.f_other(integer)",
                    "app.f_dom(integer)",
                    "app.f_domdef()",
                ],
            ),
            (
                "an UPDATE of a column nothing is generated from runs the check and every index",
                update(&["b"]),
                vec![
                    "app.f_check(integer)",
                    "app.f_key(integer)",
                    "app.f_filter(integer)",
                    "app.f_other(integer)",
                ],
            ),
            (
                "an UPDATE of a column the generated one reads also runs it",
                update(&["a"]),
                vec![
                    "app.f_check(integer)",
                    "app.f_gen(integer)",
                    "app.f_key(integer)",
                    "app.f_filter(integer)",
                    "app.f_other(integer)",
                ],
            ),
            (
                "an UPDATE of the domain's column runs the domain's check and not its default",
                update(&["c"]),
                vec![
                    "app.f_check(integer)",
                    "app.f_dom(integer)",
                    "app.f_key(integer)",
                    "app.f_filter(integer)",
                    "app.f_other(integer)",
                ],
            ),
            (
                "an UPDATE of a key cascades into the child, which runs its own",
                update(&["id"]),
                vec![
                    "app.f_check(integer)",
                    "app.f_key(integer)",
                    "app.f_filter(integer)",
                    "app.f_other(integer)",
                    "app.f_cchk(integer)",
                    "app.f_ckey(integer)",
                ],
            ),
            (
                "a rewrite re-runs the index keys and filters and nothing else",
                RowOperation::Rewrite,
                vec![
                    "app.f_key(integer)",
                    "app.f_filter(integer)",
                    "app.f_other(integer)",
                ],
            ),
            (
                "a DELETE, with its cascade, runs nothing",
                RowOperation::Delete,
                vec![],
            ),
        ];
        runtime().block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, own.connection())
                .await
                .unwrap();
            for (what, operation, expected) in cases {
                let result = prepared(
                    &mut conn,
                    &Admission::default(),
                    "app.t",
                    operation,
                    &Dropped::default(),
                    None,
                )
                .await;
                let expected: BTreeSet<&str> = expected.into_iter().collect();
                match (&result, expected.is_empty()) {
                    (Ok(()), true) => {}
                    (Err(text), false) => {
                        assert_eq!(named(text), expected, "PG{version}: {what}: {text}");
                        // One refusal lists every routine, each with its fix.
                        for routine in &expected {
                            let fix = pbps_pg::admission::search_path_fix(routine);
                            assert!(
                                text.contains(&fix),
                                "PG{version}: {what}: no fix for {routine}: {text}"
                            );
                        }
                    }
                    (other, _) => panic!("PG{version}: {what}: {other:?}"),
                }
            }
            // A child's own INSERT is checked against the parent by the foreign
            // key and runs none of the parent's expressions.
            let child = prepared(
                &mut conn,
                &Admission::default(),
                "app.u",
                RowOperation::Insert {
                    columns: Default::default(),
                },
                &Dropped::default(),
                None,
            )
            .await
            .unwrap_err();
            assert_eq!(
                named(&child),
                BTreeSet::from(["app.f_cchk(integer)", "app.f_ckey(integer)"]),
                "PG{version}: {child}"
            );
        });
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn what_the_plan_drops_is_not_judged_at_plan_time_and_is_at_the_statement() {
    each_server(|version, url| {
        let own = OwnDatabase::new(url, &format!("ee2563-dropped-{version}"));
        on_server(own.connection(), &fixture(""));
        let table: pbps_model::TableName = "app.t".parse().unwrap();
        let mut dropped = Dropped::default();
        dropped.checks.insert((table.clone(), "ck".to_owned()));
        dropped.indexes.insert((table.clone(), "ix_key".to_owned()));
        dropped.values.insert((table.clone(), "d".to_owned()));
        dropped.values.insert((table.clone(), "g".to_owned()));
        dropped
            .domain_checks
            .insert(("app.d".parse().unwrap(), "dc".to_owned()));
        runtime().block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, own.connection())
                .await
                .unwrap();
            let baseline = pbps_pg::catalog::introspect(&mut conn)
                .await
                .unwrap()
                .schema;
            let write = RowWrite {
                table: table.clone(),
                operation: RowOperation::Insert {
                    columns: Default::default(),
                },
            };
            let framing =
                pbps_pg::Postgres::new().transaction_framing(&pbps_model::SessionZone::utc());
            conn.begin(&framing).await.unwrap();
            let plan_time = data_triggers::prepare(
                &mut conn,
                &Admission::default(),
                std::slice::from_ref(&write),
                &baseline,
                &dropped,
            )
            .await;
            let text = format!("{:?}", plan_time.as_ref().err());
            // What remains: the filter, the other filter routine and the
            // domain's default (the dropped ones are skipped).
            assert_eq!(
                named(&text),
                BTreeSet::from([
                    "app.f_filter(integer)",
                    "app.f_other(integer)",
                    "app.f_domdef()"
                ]),
                "PG{version}: {text}"
            );
            // The statement's own recheck allows nothing: all of it is still
            // standing, so the dropped ones are named too.
            let at_the_statement = data_triggers::check(
                &mut conn,
                &Admission::default(),
                &write,
                &data_triggers::Guard::default(),
            )
            .await
            .unwrap_err();
            let text = format!("{at_the_statement:?}");
            assert!(
                named(&text).contains("app.f_check(integer)")
                    && named(&text).contains("app.f_key(integer)")
                    && named(&text).contains("app.f_dom(integer)"),
                "PG{version}: {text}"
            );
            conn.rollback(&framing).await.unwrap();
        });
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_trusted_owner_with_a_fixed_path_is_admitted_and_the_review_pins_the_rest() {
    each_server(|version, url| {
        let own = OwnDatabase::new(url, &format!("ee2563-admit-{version}"));
        let stranger = format!("ee2563_s_{version}_{}", std::process::id());
        on_server(url, &format!("DROP ROLE IF EXISTS {stranger}"));
        on_server(url, &format!("CREATE ROLE {stranger}"));
        on_server(own.connection(), &fixture(""));
        // Every routine fixes its path, and the stranger owns the check's.
        let mut fix = String::new();
        for routine in ROUTINES {
            fix.push_str(&format!(
                "ALTER FUNCTION {routine} SET search_path = pg_catalog, pg_temp;"
            ));
        }
        fix.push_str(&format!(
            "GRANT USAGE ON SCHEMA app TO {stranger};
             ALTER FUNCTION app.f_check(integer) OWNER TO {stranger};"
        ));
        on_server(own.connection(), &fix);
        runtime().block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, own.connection())
                .await
                .unwrap();
            // Without the review, the stranger's routine is refused, with all
            // three ways out.
            let refused = prepared(
                &mut conn,
                &Admission::default(),
                "app.t",
                RowOperation::Insert {
                    columns: Default::default(),
                },
                &Dropped::default(),
                None,
            )
            .await
            .unwrap_err();
            assert_eq!(
                named(&refused),
                BTreeSet::from(["app.f_check(integer)"]),
                "PG{version}: {refused}"
            );
            assert!(
                refused.contains("`trusted_roles`")
                    && refused.contains("bring the routine into the project"),
                "PG{version}: {refused}"
            );
            // Trusting the owner admits it.
            let trusting = Admission::new([pbps_model::RoleName::new(stranger.as_str())], false);
            prepared(
                &mut conn,
                &trusting,
                "app.t",
                RowOperation::Insert {
                    columns: Default::default(),
                },
                &Dropped::default(),
                None,
            )
            .await
            .unwrap_or_else(|e| panic!("PG{version}: {e}"));
            // The review admits it and records it for the plan to show and pin.
            let reviewing = Admission::new(Vec::new(), true);
            prepared(
                &mut conn,
                &reviewing,
                "app.t",
                RowOperation::Insert {
                    columns: Default::default(),
                },
                &Dropped::default(),
                None,
            )
            .await
            .unwrap_or_else(|e| panic!("PG{version}: {e}"));
            assert_eq!(reviewing.reviewed().len(), 1, "PG{version}");
            // At apply, a routine the plan did not show and pin is refused.
            let approving = Admission::new(Vec::new(), true).approving(Vec::new());
            let late = prepared(
                &mut conn,
                &approving,
                "app.t",
                RowOperation::Insert {
                    columns: Default::default(),
                },
                &Dropped::default(),
                None,
            )
            .await
            .unwrap_err();
            assert!(late.contains("did not show and pin"), "PG{version}: {late}");
        });
        drop(own);
        on_server(url, &format!("DROP ROLE IF EXISTS {stranger}"));
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_policy_is_judged_only_where_row_security_applies_to_the_deploying_role() {
    each_server(|version, url| {
        let own = OwnDatabase::new(url, &format!("ee2563-policy-{version}"));
        let pid = std::process::id();
        let (owner, other, bypass, member) = (
            format!("ee2563_o_{version}_{pid}"),
            format!("ee2563_x_{version}_{pid}"),
            format!("ee2563_b_{version}_{pid}"),
            format!("ee2563_m_{version}_{pid}"),
        );
        for role in [&owner, &other, &bypass, &member] {
            on_server(url, &format!("DROP ROLE IF EXISTS {role}"));
        }
        on_server(
            url,
            &format!(
                "CREATE ROLE {owner}; CREATE ROLE {other}; CREATE ROLE {bypass} BYPASSRLS;
                 CREATE ROLE {member} IN ROLE {owner}"
            ),
        );
        on_server(own.connection(), &fixture(&owner));
        // Only the policy's routine fixes nothing: fix the rest so the policy's
        // is the one name a refusal can hold.
        let mut fix = String::new();
        for routine in ROUTINES
            .iter()
            .filter(|r| !["app.f_pol(integer)", "app.f_cchk(integer)"].contains(*r))
        {
            fix.push_str(&format!(
                "ALTER FUNCTION {routine} SET search_path = pg_catalog, pg_temp;"
            ));
        }
        fix.push_str(&format!(
            "GRANT ALL ON ALL TABLES IN SCHEMA app TO {other}, {bypass};
             GRANT USAGE ON SCHEMA app TO {other}, {bypass};
             ALTER TABLE app.t ENABLE ROW LEVEL SECURITY;
             CREATE POLICY p ON app.t USING (app.f_pol(id)) WITH CHECK (app.f_pol(id));
             CREATE POLICY elsewhere ON app.t TO {bypass} USING (app.f_cchk(id))"
        ));
        on_server(own.connection(), &fix);
        runtime().block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, own.connection())
                .await
                .unwrap();
            let judged = |text: &Result<(), String>| {
                text.as_ref()
                    .err()
                    .is_some_and(|t| t.contains("app.f_pol(integer)"))
            };
            for (who, role, forced, expect) in [
                ("a superuser", None, false, false),
                (
                    "the owner without FORCE",
                    Some(owner.as_str()),
                    false,
                    false,
                ),
                (
                    "a role that bypasses row security",
                    Some(bypass.as_str()),
                    false,
                    false,
                ),
                (
                    "a role that is not the owner",
                    Some(other.as_str()),
                    false,
                    true,
                ),
                ("the owner with FORCE", Some(owner.as_str()), true, true),
                (
                    "a member of the owner without FORCE",
                    Some(member.as_str()),
                    false,
                    false,
                ),
                (
                    "a member of the owner with FORCE",
                    Some(member.as_str()),
                    true,
                    true,
                ),
            ] {
                conn.execute(&format!(
                    "ALTER TABLE app.t {} FORCE ROW LEVEL SECURITY",
                    if forced { "" } else { "NO" }
                ))
                .await
                .unwrap();
                for (what, operation) in [
                    (
                        "INSERT",
                        RowOperation::Insert {
                            columns: Default::default(),
                        },
                    ),
                    ("UPDATE", update(&["b"])),
                    ("DELETE", RowOperation::Delete),
                ] {
                    let result = prepared(
                        &mut conn,
                        &Admission::default(),
                        "app.t",
                        operation,
                        &Dropped::default(),
                        role,
                    )
                    .await;
                    assert_eq!(
                        judged(&result),
                        expect,
                        "PG{version}: {what} as {who}: {result:?}"
                    );
                }
            }
            // A policy made for another role is never run by this one: the
            // routine only it calls is left unfixed, and is not named.
            let result = prepared(
                &mut conn,
                &Admission::default(),
                "app.t",
                RowOperation::Insert {
                    columns: Default::default(),
                },
                &Dropped::default(),
                Some(&other),
            )
            .await
            .unwrap_err();
            assert!(
                result.contains("app.f_pol(integer)") && !result.contains("app.f_cchk(integer)"),
                "PG{version}: {result}"
            );
        });
        drop(own);
        for role in [&owner, &other, &bypass, &member] {
            on_server(url, &format!("DROP ROLE IF EXISTS {role}"));
        }
    });
}

// ---------------------------------------------------------------------------
// Draft round 1 (#2748): the policy expression each command runs, and the
// relation that owns what the plan drops.
// ---------------------------------------------------------------------------

/// What `prepare` alone says to `operation` on `table`: the plan-time
/// judgement, which allows what `dropped` names. `Ok` or the refusal's text.
async fn plan_time(
    conn: &mut pbps_db::Conn,
    table: &str,
    operation: RowOperation,
    dropped: &Dropped,
    as_role: Option<&str>,
) -> Result<(), String> {
    let baseline = pbps_pg::catalog::introspect(conn).await.unwrap().schema;
    let write = RowWrite {
        table: table.parse().unwrap(),
        operation,
    };
    let framing = pbps_pg::Postgres::new().transaction_framing(&pbps_model::SessionZone::utc());
    conn.begin(&framing).await.unwrap();
    if let Some(role) = as_role {
        conn.execute(&format!("SET LOCAL ROLE {role}"))
            .await
            .unwrap();
    }
    let result = data_triggers::prepare(
        conn,
        &Admission::default(),
        std::slice::from_ref(&write),
        &baseline,
        dropped,
    )
    .await
    .map(|_| ())
    .map_err(|e| format!("{e:?}"));
    conn.rollback(&framing).await.unwrap();
    result
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_command_is_judged_for_the_policy_expression_it_runs_and_not_the_other() {
    each_server(|version, url| {
        let own = OwnDatabase::new(url, &format!("ee2748-pol-{version}"));
        let role = format!("ee2748_x_{version}_{}", std::process::id());
        on_server(url, &format!("DROP ROLE IF EXISTS {role}"));
        on_server(url, &format!("CREATE ROLE {role}"));
        on_server(
            own.connection(),
            &format!(
                "CREATE SCHEMA app;
                 CREATE FUNCTION app.f_pol(integer) RETURNS boolean LANGUAGE plpgsql IMMUTABLE
                     AS $$BEGIN RETURN true; END$$;
                 CREATE TABLE app.t (id integer PRIMARY KEY);
                 ALTER TABLE app.t ENABLE ROW LEVEL SECURITY;
                 GRANT USAGE ON SCHEMA app TO {role}; GRANT ALL ON app.t TO {role}"
            ),
        );
        // (the policy, then whether an INSERT, an UPDATE and a DELETE run the
        // routine it calls). Measured on 16 and 18 (DEC-2563.10): a command runs
        // a policy's WITH CHECK (an INSERT, an UPDATE's new row) or its USING
        // (an UPDATE's old row, a DELETE), and a policy without a WITH CHECK
        // uses its USING for the check.
        let shapes: [(&str, [bool; 3]); 9] = [
            // A `FOR ALL` policy whose WITH CHECK is the routine: a DELETE
            // never runs a WITH CHECK, so it is not judged for it.
            (
                "FOR ALL USING (true) WITH CHECK (app.f_pol(id))",
                [true, true, false],
            ),
            // The routine in the USING only: an INSERT checks the WITH CHECK.
            (
                "FOR ALL USING (app.f_pol(id)) WITH CHECK (true)",
                [false, true, true],
            ),
            // No WITH CHECK: the USING is used for it.
            ("FOR ALL USING (app.f_pol(id))", [true, true, true]),
            (
                "FOR INSERT WITH CHECK (app.f_pol(id))",
                [true, false, false],
            ),
            (
                "FOR UPDATE USING (true) WITH CHECK (app.f_pol(id))",
                [false, true, false],
            ),
            (
                "FOR UPDATE USING (app.f_pol(id)) WITH CHECK (true)",
                [false, true, false],
            ),
            ("FOR UPDATE USING (app.f_pol(id))", [false, true, false]),
            ("FOR DELETE USING (app.f_pol(id))", [false, false, true]),
            // A SELECT policy is read by an UPDATE and a DELETE, which find the
            // row first.
            ("FOR SELECT USING (app.f_pol(id))", [false, true, true]),
        ];
        runtime().block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, own.connection())
                .await
                .unwrap();
            for (policy, expected) in shapes {
                conn.execute("DROP POLICY IF EXISTS shape ON app.t")
                    .await
                    .unwrap();
                conn.execute(&format!("CREATE POLICY shape ON app.t {policy}"))
                    .await
                    .unwrap();
                for ((what, operation), expect) in [
                    (
                        "INSERT",
                        RowOperation::Insert {
                            columns: Default::default(),
                        },
                    ),
                    ("UPDATE", update(&["id"])),
                    ("DELETE", RowOperation::Delete),
                ]
                .into_iter()
                .zip(expected)
                {
                    let result = prepared(
                        &mut conn,
                        &Admission::default(),
                        "app.t",
                        operation,
                        &Dropped::default(),
                        Some(&role),
                    )
                    .await;
                    let judged = result
                        .as_ref()
                        .err()
                        .is_some_and(|t| t.contains("app.f_pol(integer)"));
                    assert_eq!(
                        judged, expect,
                        "PG{version}: {what} on a policy {policy}: {result:?}"
                    );
                }
            }
        });
        drop(own);
        on_server(url, &format!("DROP ROLE IF EXISTS {role}"));
    });
}

/// What `check` says to an INSERT on `table`, which allows nothing the plan
/// dropped.
async fn at_the_statement(conn: &mut pbps_db::Conn, table: &str) -> Result<(), String> {
    at_the_statement_of(
        conn,
        table,
        RowOperation::Insert {
            columns: Default::default(),
        },
    )
    .await
}

/// The same for the statement `operation` names.
async fn at_the_statement_of(
    conn: &mut pbps_db::Conn,
    table: &str,
    operation: RowOperation,
) -> Result<(), String> {
    let write = RowWrite {
        table: table.parse().unwrap(),
        operation,
    };
    let framing = pbps_pg::Postgres::new().transaction_framing(&pbps_model::SessionZone::utc());
    conn.begin(&framing).await.unwrap();
    let result = data_triggers::check(
        conn,
        &Admission::default(),
        &write,
        &data_triggers::Guard::default(),
    )
    .await
    .map_err(|e| format!("{e:?}"));
    conn.rollback(&framing).await.unwrap();
    result
}

/// What `result` names of the routines, or none for `Ok`.
fn names(result: &Result<(), String>) -> BTreeSet<&'static str> {
    result.as_ref().err().map(|t| named(t)).unwrap_or_default()
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn what_an_inheritance_child_drops_is_excused_by_the_child_that_owns_it() {
    each_server(|version, url| {
        let own = OwnDatabase::new(url, &format!("ee2748-inh-{version}"));
        let role = format!("ee2748_i_{version}_{}", std::process::id());
        on_server(url, &format!("DROP ROLE IF EXISTS {role}"));
        on_server(url, &format!("CREATE ROLE {role}"));
        on_server(
            own.connection(),
            &format!(
                "CREATE SCHEMA app;
                 CREATE FUNCTION app.f_check(integer) RETURNS boolean LANGUAGE plpgsql IMMUTABLE
                     AS $$BEGIN RETURN true; END$$;
                 CREATE FUNCTION app.f_cchk(integer) RETURNS boolean LANGUAGE plpgsql IMMUTABLE
                     AS $$BEGIN RETURN true; END$$;
                 CREATE FUNCTION app.f_ckey(integer) RETURNS integer LANGUAGE plpgsql IMMUTABLE
                     AS $$BEGIN RETURN 1; END$$;
                 CREATE FUNCTION app.f_pol(integer) RETURNS boolean LANGUAGE plpgsql IMMUTABLE
                     AS $$BEGIN RETURN true; END$$;
                 CREATE TABLE app.p (id integer PRIMARY KEY, b integer,
                     CONSTRAINT pck CHECK (app.f_check(b)));
                 CREATE TABLE app.c (CONSTRAINT cck CHECK (app.f_cchk(id))) INHERITS (app.p);
                 CREATE INDEX cx ON app.c ((app.f_ckey(id)));
                 ALTER TABLE app.c ENABLE ROW LEVEL SECURITY;
                 CREATE POLICY cpol ON app.c USING (app.f_pol(id));
                 GRANT USAGE ON SCHEMA app TO {role}; GRANT ALL ON app.p, app.c TO {role}"
            ),
        );
        let t = |name: &str| -> pbps_model::TableName { format!("app.{name}").parse().unwrap() };
        // The child's policy is not among them: row security is the named
        // table's, so an UPDATE through the parent runs the parent's policies
        // and not the child's (DEC-2563.10).
        let all = BTreeSet::from([
            "app.f_check(integer)",
            "app.f_cchk(integer)",
            "app.f_ckey(integer)",
        ]);
        runtime().block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, own.connection())
                .await
                .unwrap();
            // An UPDATE through the parent reaches the child and the CHECKs and
            // indexes it carries.
            macro_rules! ask {
                ($dropped:expr) => {
                    plan_time(&mut conn, "app.p", update(&["b"]), &$dropped, Some(&role)).await
                };
            }
            // Nothing dropped: every routine is named.
            assert_eq!(names(&ask!(Dropped::default())), all, "PG{version}");
            // The child's own CHECK, dropped by name on the child.
            let mut d = Dropped::default();
            d.checks.insert((t("c"), "cck".to_owned()));
            let mut left = all.clone();
            left.remove("app.f_cchk(integer)");
            assert_eq!(names(&ask!(d)), left, "PG{version}: child CHECK");
            // The child's index.
            let mut d = Dropped::default();
            d.indexes.insert((t("c"), "cx".to_owned()));
            let mut left = all.clone();
            left.remove("app.f_ckey(integer)");
            assert_eq!(names(&ask!(d)), left, "PG{version}: child index");
            // The child's policy is judged where the child is the table named,
            // and the plan that drops it there excuses it; the same name on the
            // parent excuses nothing.
            macro_rules! ask_child {
                ($dropped:expr) => {
                    plan_time(&mut conn, "app.c", update(&["b"]), &$dropped, Some(&role)).await
                };
            }
            let on_child: BTreeSet<&'static str> =
                all.iter().copied().chain(["app.f_pol(integer)"]).collect();
            assert_eq!(
                names(&ask_child!(Dropped::default())),
                on_child,
                "PG{version}: written directly"
            );
            let mut d = Dropped::default();
            d.policies.insert((t("c"), "cpol".to_owned()));
            let mut left = on_child.clone();
            left.remove("app.f_pol(integer)");
            assert_eq!(names(&ask_child!(d)), left, "PG{version}: child policy");
            let mut d = Dropped::default();
            d.policies.insert((t("p"), "cpol".to_owned()));
            assert_eq!(names(&ask_child!(d)), on_child, "PG{version}: wrong owner");
            // Dropping the parent's CHECK takes the child's inherited copy with
            // it: the routine is named by neither.
            let mut d = Dropped::default();
            d.checks.insert((t("p"), "pck".to_owned()));
            let mut left = all.clone();
            left.remove("app.f_check(integer)");
            assert_eq!(names(&ask!(d)), left, "PG{version}: inherited CHECK");
            // Everything dropped: nothing is refused at plan time.
            let mut d = Dropped::default();
            d.checks.insert((t("p"), "pck".to_owned()));
            d.checks.insert((t("c"), "cck".to_owned()));
            d.indexes.insert((t("c"), "cx".to_owned()));
            d.policies.insert((t("c"), "cpol".to_owned()));
            assert_eq!(ask!(d), Ok(()), "PG{version}: all dropped");
            // The name alone excuses nothing: it must be the owner's.
            let mut d = Dropped::default();
            d.checks.insert((t("p"), "cck".to_owned()));
            d.indexes.insert((t("p"), "cx".to_owned()));
            d.policies.insert((t("p"), "cpol".to_owned()));
            assert_eq!(names(&ask!(d)), all, "PG{version}: the wrong owner");
            // A child that also declares the CHECK itself keeps it when the
            // parent's goes, so it stays judged.
            conn.execute(
                "CREATE TABLE app.c2 (CONSTRAINT pck CHECK (app.f_check(b))) INHERITS (app.p)",
            )
            .await
            .unwrap();
            let mut d = Dropped::default();
            d.checks.insert((t("p"), "pck".to_owned()));
            d.checks.insert((t("c"), "cck".to_owned()));
            d.indexes.insert((t("c"), "cx".to_owned()));
            d.policies.insert((t("c"), "cpol".to_owned()));
            assert_eq!(
                names(&ask!(d)),
                BTreeSet::from(["app.f_check(integer)"]),
                "PG{version}: a child's own copy of the CHECK"
            );
            // A table the plan drops carries nothing to judge.
            let mut d = Dropped::default();
            d.checks.insert((t("p"), "pck".to_owned()));
            d.tables.insert(t("c2"));
            d.tables.insert(t("c"));
            assert_eq!(ask!(d), Ok(()), "PG{version}: dropped children");
        });
        drop(own);
        on_server(url, &format!("DROP ROLE IF EXISTS {role}"));
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn what_a_partition_or_its_parent_drops_is_excused_across_the_tree() {
    each_server(|version, url| {
        let own = OwnDatabase::new(url, &format!("ee2748-part-{version}"));
        on_server(
            own.connection(),
            "CREATE SCHEMA app;
             CREATE FUNCTION app.f_check(integer) RETURNS boolean LANGUAGE plpgsql IMMUTABLE
                 AS $$BEGIN RETURN true; END$$;
             CREATE FUNCTION app.f_default() RETURNS integer LANGUAGE plpgsql
                 AS $$BEGIN RETURN 1; END$$;
             CREATE FUNCTION app.f_key(integer) RETURNS integer LANGUAGE plpgsql IMMUTABLE
                 AS $$BEGIN RETURN 1; END$$;
             CREATE FUNCTION app.f_cchk(integer) RETURNS boolean LANGUAGE plpgsql IMMUTABLE
                 AS $$BEGIN RETURN true; END$$;
             CREATE TABLE app.pp (id integer, b integer DEFAULT app.f_default(),
                 CONSTRAINT ppck CHECK (app.f_check(b))) PARTITION BY RANGE (id);
             CREATE INDEX ppx ON app.pp ((app.f_key(id)));
             CREATE TABLE app.p1 PARTITION OF app.pp FOR VALUES FROM (0) TO (10);
             CREATE TABLE app.p2 PARTITION OF app.pp FOR VALUES FROM (10) TO (20)
                 PARTITION BY RANGE (id);
             CREATE TABLE app.p2a PARTITION OF app.p2 FOR VALUES FROM (10) TO (20);
             ALTER TABLE app.p2a ADD CONSTRAINT leaf_ck CHECK (app.f_cchk(id))",
        );
        let t = |name: &str| -> pbps_model::TableName { format!("app.{name}").parse().unwrap() };
        let all = BTreeSet::from([
            "app.f_check(integer)",
            "app.f_default()",
            "app.f_key(integer)",
            "app.f_cchk(integer)",
        ]);
        runtime().block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, own.connection())
                .await
                .unwrap();
            macro_rules! ask {
                ($dropped:expr) => {
                    plan_time(
                        &mut conn,
                        "app.pp",
                        RowOperation::Insert {
                            columns: Default::default(),
                        },
                        &$dropped,
                        None,
                    )
                    .await
                };
            }
            let without = |gone: &[&'static str]| -> BTreeSet<&'static str> {
                all.iter().copied().filter(|r| !gone.contains(r)).collect()
            };
            assert_eq!(names(&ask!(Dropped::default())), all, "PG{version}");
            // The parent's CHECK, default and index take their copies on every
            // partition with them.
            let mut d = Dropped::default();
            d.checks.insert((t("pp"), "ppck".to_owned()));
            assert_eq!(
                names(&ask!(d)),
                without(&["app.f_check(integer)"]),
                "PG{version}: the parent's CHECK"
            );
            let mut d = Dropped::default();
            d.values.insert((t("pp"), "b".to_owned()));
            assert_eq!(
                names(&ask!(d)),
                without(&["app.f_default()"]),
                "PG{version}: the parent's default"
            );
            let mut d = Dropped::default();
            d.indexes.insert((t("pp"), "ppx".to_owned()));
            assert_eq!(
                names(&ask!(d)),
                without(&["app.f_key(integer)"]),
                "PG{version}: the parent's index"
            );
            // A leaf's own CHECK, dropped on the leaf, with the write on the root.
            let mut d = Dropped::default();
            d.checks.insert((t("p2a"), "leaf_ck".to_owned()));
            assert_eq!(
                names(&ask!(d)),
                without(&["app.f_cchk(integer)"]),
                "PG{version}: the leaf's CHECK"
            );
            // A partition dropped with its own partitions takes theirs along.
            let mut d = Dropped::default();
            d.tables.insert(t("p2"));
            assert_eq!(
                names(&ask!(d)),
                without(&["app.f_cchk(integer)"]),
                "PG{version}: a dropped sub-partitioned partition"
            );
            // All of it: nothing is left to refuse at plan time.
            let mut d = Dropped::default();
            d.checks.insert((t("pp"), "ppck".to_owned()));
            d.values.insert((t("pp"), "b".to_owned()));
            d.indexes.insert((t("pp"), "ppx".to_owned()));
            d.checks.insert((t("p2a"), "leaf_ck".to_owned()));
            assert_eq!(ask!(d), Ok(()), "PG{version}: all dropped");
            // The right name on the wrong table excuses only that table's copy:
            // the parent's own is still judged.
            let mut d = Dropped::default();
            d.checks.insert((t("p1"), "ppck".to_owned()));
            d.values.insert((t("p1"), "b".to_owned()));
            d.indexes.insert((t("p1"), "ppx".to_owned()));
            d.checks.insert((t("pp"), "leaf_ck".to_owned()));
            assert_eq!(names(&ask!(d)), all, "PG{version}: the wrong owner");
            // At the statement nothing is excused: the whole tree is judged.
            let at_the_statement = at_the_statement(&mut conn, "app.pp").await;
            assert_eq!(
                names(&at_the_statement),
                all,
                "PG{version}: at the statement"
            );
        });
    });
}

fn insert(columns: &[&str]) -> RowOperation {
    RowOperation::Insert {
        columns: columns.iter().map(|c| (*c).into()).collect(),
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_insert_judges_the_default_only_of_a_column_it_leaves_out() {
    each_server(|version, url| {
        let own = OwnDatabase::new(url, &format!("ee2748-def-{version}"));
        on_server(
            own.connection(),
            "CREATE SCHEMA app;
             CREATE FUNCTION app.f_defa() RETURNS integer LANGUAGE plpgsql AS $$BEGIN RETURN 1; END$$;
             CREATE FUNCTION app.f_defb() RETURNS integer LANGUAGE plpgsql AS $$BEGIN RETURN 1; END$$;
             CREATE FUNCTION app.f_defx() RETURNS integer LANGUAGE plpgsql AS $$BEGIN RETURN 1; END$$;
             CREATE FUNCTION app.f_domdef() RETURNS integer LANGUAGE plpgsql AS $$BEGIN RETURN 1; END$$;
             CREATE DOMAIN app.dm AS integer DEFAULT app.f_domdef();
             CREATE TABLE app.t (id integer PRIMARY KEY,
                 a integer DEFAULT app.f_defa(), b integer DEFAULT app.f_defb());
             CREATE TABLE app.u (id integer, x app.dm DEFAULT app.f_defx(), y app.dm);
             CREATE TABLE app.v (id integer, x app.dm DEFAULT app.f_defx());
             CREATE TABLE app.w (id integer, g app.dm GENERATED ALWAYS AS (id) STORED)",
        );
        runtime().block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, own.connection())
                .await
                .unwrap();
            macro_rules! ask {
                ($table:expr, $columns:expr) => {
                    names(
                        &plan_time(
                            &mut conn,
                            $table,
                            insert($columns),
                            &Dropped::default(),
                            None,
                        )
                        .await,
                    )
                };
            }
            let set = |routines: &[&'static str]| -> BTreeSet<&'static str> {
                routines.iter().copied().collect()
            };
            let (defa, defb, defx, dom) = (
                "app.f_defa()",
                "app.f_defb()",
                "app.f_defx()",
                "app.f_domdef()",
            );
            // A column the statement supplies runs no DEFAULT.
            assert_eq!(ask!("app.t", &["id", "a", "b"]), set(&[]), "PG{version}");
            assert_eq!(ask!("app.t", &["id", "a"]), set(&[defb]), "PG{version}");
            assert_eq!(ask!("app.t", &["id", "b"]), set(&[defa]), "PG{version}");
            assert_eq!(ask!("app.t", &["id"]), set(&[defa, defb]), "PG{version}");
            // A column list that is not known judges every DEFAULT.
            assert_eq!(
                ask!("app.t", &[]),
                set(&[defa, defb]),
                "PG{version}: unknown"
            );
            // The copy of a rebuild lists its columns as an INSERT does.
            let copied = |columns: &[&str]| RowOperation::Copy {
                columns: columns.iter().map(|c| (*c).into()).collect(),
            };
            for (columns, expected) in [
                (&["id", "a", "b"][..], set(&[])),
                (&["id", "a"][..], set(&[defb])),
                (&[][..], set(&[defa, defb])),
            ] {
                let got = names(
                    &plan_time(
                        &mut conn,
                        "app.t",
                        copied(columns),
                        &Dropped::default(),
                        None,
                    )
                    .await,
                );
                assert_eq!(got, expected, "PG{version}: a copy of {columns:?}");
            }
            // The domain's DEFAULT is for a column with none of its own that the
            // statement leaves out.
            assert_eq!(ask!("app.u", &["id"]), set(&[defx, dom]), "PG{version}");
            assert_eq!(ask!("app.u", &["id", "y"]), set(&[defx]), "PG{version}");
            assert_eq!(ask!("app.u", &["id", "x"]), set(&[dom]), "PG{version}");
            assert_eq!(ask!("app.u", &["id", "x", "y"]), set(&[]), "PG{version}");
            // A column DEFAULT overrides the domain's.
            assert_eq!(ask!("app.v", &["id"]), set(&[defx]), "PG{version}");
            assert_eq!(ask!("app.v", &["id", "x"]), set(&[]), "PG{version}");
            // A generated column never takes the domain's DEFAULT.
            assert_eq!(ask!("app.w", &["id"]), set(&[]), "PG{version}");
            // And the recheck just before the statement is judged the same way.
            let at = |columns: &[&str]| insert(columns);
            let rechecked = at_the_statement_of(&mut conn, "app.t", at(&["id", "a"])).await;
            assert_eq!(
                names(&rechecked),
                set(&[defb]),
                "PG{version}: at the statement"
            );
            let rechecked = at_the_statement_of(&mut conn, "app.u", at(&["id", "x"])).await;
            assert_eq!(
                names(&rechecked),
                set(&[dom]),
                "PG{version}: at the statement"
            );
        });
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_policy_is_judged_only_on_the_relation_a_statement_names() {
    each_server(|version, url| {
        for (kind, parent_rls) in [("inh", true), ("part", true), ("inh", false)] {
            let tag = format!("{kind}{}", if parent_rls { "" } else { "-norls" });
            let own = OwnDatabase::new(url, &format!("ee2748-rls-{tag}-{version}"));
            let role = format!("ee2748_r_{kind}_{version}_{}", std::process::id());
            on_server(url, &format!("DROP ROLE IF EXISTS {role}"));
            on_server(url, &format!("CREATE ROLE {role}"));
            let (parent, child) = if kind == "inh" {
                (
                    "CREATE TABLE app.par (id integer, v integer)",
                    "CREATE TABLE app.chi () INHERITS (app.par)",
                )
            } else {
                (
                    "CREATE TABLE app.par (id integer, v integer) PARTITION BY RANGE (id)",
                    "CREATE TABLE app.chi PARTITION OF app.par FOR VALUES FROM (0) TO (100)",
                )
            };
            let parent_policy = if parent_rls {
                "ALTER TABLE app.par ENABLE ROW LEVEL SECURITY;
                 CREATE POLICY ppol ON app.par USING (app.f_pol(id)) WITH CHECK (app.f_pol(id));"
            } else {
                ""
            };
            on_server(
                own.connection(),
                &format!(
                    "CREATE SCHEMA app;
                     CREATE FUNCTION app.f_pol(integer) RETURNS boolean LANGUAGE plpgsql IMMUTABLE
                         AS $$BEGIN RETURN true; END$$;
                     CREATE FUNCTION app.f_other(integer) RETURNS boolean LANGUAGE plpgsql IMMUTABLE
                         AS $$BEGIN RETURN true; END$$;
                     {parent}; {child};
                     {parent_policy}
                     ALTER TABLE app.chi ENABLE ROW LEVEL SECURITY;
                     CREATE POLICY cpol ON app.chi USING (app.f_other(id)) WITH CHECK (app.f_other(id));
                     GRANT USAGE ON SCHEMA app TO {role}; GRANT ALL ON app.par, app.chi TO {role}"
                ),
            );
            let wrote = [
                ("UPDATE", update(&["v"])),
                ("DELETE", RowOperation::Delete),
                ("INSERT", insert(&["id", "v"])),
            ];
            runtime().block_on(async {
                let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, own.connection())
                    .await
                    .unwrap();
                for (command, operation) in wrote {
                    // Through the parent: the parent's policy and not the child's.
                    let through = names(
                        &plan_time(
                            &mut conn,
                            "app.par",
                            operation.clone(),
                            &Dropped::default(),
                            Some(&role),
                        )
                        .await,
                    );
                    let expected: BTreeSet<&'static str> = if parent_rls {
                        BTreeSet::from(["app.f_pol(integer)"])
                    } else {
                        BTreeSet::new()
                    };
                    assert_eq!(
                        through, expected,
                        "PG{version} {tag}: {command} through the parent"
                    );
                    // Directly: the child's own.
                    let direct = names(
                        &plan_time(
                            &mut conn,
                            "app.chi",
                            operation,
                            &Dropped::default(),
                            Some(&role),
                        )
                        .await,
                    );
                    assert_eq!(
                        direct,
                        BTreeSet::from(["app.f_other(integer)"]),
                        "PG{version} {tag}: {command} directly on the child"
                    );
                }
            });
            drop(own);
            on_server(url, &format!("DROP ROLE IF EXISTS {role}"));
        }
    });
}

fn table_named(name: &str) -> pbps_model::TableName {
    format!("app.{name}").parse().unwrap()
}

fn domain_named(name: &str) -> pbps_model::TypeRef {
    format!("app.{name}").parse().unwrap()
}

/// The routines the fixture's `app.t` runs for an `INSERT`: every kind of site.
const INSERT_SET: [&str; 8] = [
    "app.f_check(integer)",
    "app.f_default()",
    "app.f_gen(integer)",
    "app.f_key(integer)",
    "app.f_filter(integer)",
    "app.f_other(integer)",
    "app.f_dom(integer)",
    "app.f_domdef()",
];

/// The sweep of what `prepare` excuses (#2748): for each kind of site on the
/// fixture's table, each step of a plan that removes it before the rows, and the
/// same step aimed at something else, which excuses nothing.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn every_site_kind_is_excused_by_each_step_that_removes_it_and_by_no_other() {
    type Step = fn(&mut Dropped);
    let cases: [(&str, Step, &[&str]); 16] = [
        (
            "CHECK: DropCheck",
            |d| {
                d.checks.insert((table_named("t"), "ck".to_owned()));
            },
            &["app.f_check(integer)"],
        ),
        (
            "CHECK: the same name dropped on another table",
            |d| {
                d.checks.insert((table_named("u"), "ck".to_owned()));
            },
            &[],
        ),
        (
            "index key: DropIndex",
            |d| {
                d.indexes.insert((table_named("t"), "ix_key".to_owned()));
            },
            &["app.f_key(integer)"],
        ),
        (
            "index filter: DropIndex",
            |d| {
                d.indexes.insert((table_named("t"), "ix_filter".to_owned()));
            },
            &["app.f_filter(integer)", "app.f_other(integer)"],
        ),
        (
            "DEFAULT: DropColumn or AlterColumnDefault",
            |d| {
                d.values.insert((table_named("t"), "d".to_owned()));
            },
            &["app.f_default()"],
        ),
        (
            "generated: DropColumn or AlterColumnExpression",
            |d| {
                d.values.insert((table_named("t"), "g".to_owned()));
            },
            &["app.f_gen(integer)"],
        ),
        (
            "domain: a changed default of the domain-typed column keeps the domain",
            |d| {
                d.values.insert((table_named("t"), "c".to_owned()));
            },
            &[],
        ),
        (
            "domain CHECK and DEFAULT: DropColumn or AlterColumnType of the column",
            |d| {
                d.columns.insert((table_named("t"), "c".to_owned()));
            },
            &["app.f_dom(integer)", "app.f_domdef()"],
        ),
        (
            "domain: another column dropped",
            |d| {
                d.columns.insert((table_named("t"), "a".to_owned()));
            },
            &[],
        ),
        (
            "domain: the same column of another table dropped",
            |d| {
                d.columns.insert((table_named("u"), "c".to_owned()));
            },
            &[],
        ),
        (
            "domain CHECK: AlterDomain drops the check",
            |d| {
                d.domain_checks.insert((domain_named("d"), "dc".to_owned()));
            },
            &["app.f_dom(integer)"],
        ),
        (
            "domain CHECK: another domain's check dropped",
            |d| {
                d.domain_checks
                    .insert((domain_named("other"), "dc".to_owned()));
            },
            &[],
        ),
        (
            "domain DEFAULT: AlterDomain sets or drops the default",
            |d| {
                d.domain_defaults.insert(domain_named("d"));
            },
            &["app.f_domdef()"],
        ),
        (
            "domain DEFAULT: another domain's default",
            |d| {
                d.domain_defaults.insert(domain_named("other"));
            },
            &[],
        ),
        (
            "every kind: DropTable",
            |d| {
                d.tables.insert(table_named("t"));
            },
            &INSERT_SET,
        ),
        (
            "every kind: DropTable of another table",
            |d| {
                d.tables.insert(table_named("u"));
            },
            &[],
        ),
    ];
    each_server(|version, url| {
        let own = OwnDatabase::new(url, &format!("ee2748-sweep-{version}"));
        on_server(own.connection(), &fixture(""));
        let all: BTreeSet<&'static str> = INSERT_SET.into_iter().collect();
        runtime().block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, own.connection())
                .await
                .unwrap();
            let mut everything = Dropped::default();
            for (label, step, gone) in &cases {
                let mut dropped = Dropped::default();
                step(&mut dropped);
                let expected: BTreeSet<&'static str> =
                    all.iter().copied().filter(|r| !gone.contains(r)).collect();
                let got = names(&plan_time(&mut conn, "app.t", insert(&[]), &dropped, None).await);
                assert_eq!(got, expected, "PG{version}: {label}");
                if !gone.is_empty() {
                    step(&mut everything);
                }
            }
            // Every removing step together leaves nothing to refuse at plan time
            // (the table's own drop would do it alone, so leave that one out)...
            everything.tables.clear();
            let got = names(&plan_time(&mut conn, "app.t", insert(&[]), &everything, None).await);
            assert!(got.is_empty(), "PG{version}: {got:?}");
            // ...and the recheck just before the statement allows none of it.
            let at = names(&at_the_statement(&mut conn, "app.t").await);
            assert_eq!(at, all, "PG{version}: at the statement");
        });
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_dropped_inherited_column_or_a_detached_partition_excuses_what_it_carried() {
    each_server(|version, url| {
        let own = OwnDatabase::new(url, &format!("ee2748-tree-{version}"));
        on_server(
            own.connection(),
            "CREATE SCHEMA app;
             CREATE FUNCTION app.f_check(integer) RETURNS boolean LANGUAGE plpgsql IMMUTABLE
                 AS $$BEGIN RETURN true; END$$;
             CREATE FUNCTION app.f_cchk(integer) RETURNS boolean LANGUAGE plpgsql IMMUTABLE
                 AS $$BEGIN RETURN true; END$$;
             CREATE FUNCTION app.f_dom(integer) RETURNS boolean LANGUAGE plpgsql IMMUTABLE
                 AS $$BEGIN RETURN true; END$$;
             CREATE DOMAIN app.dx AS integer CONSTRAINT dxc CHECK (app.f_dom(VALUE));
             CREATE DOMAIN app.dz AS integer CONSTRAINT dzc CHECK (app.f_cchk(VALUE));
             CREATE TABLE app.pp (id integer, x app.dx);
             CREATE TABLE app.cc (z app.dz) INHERITS (app.pp);
             CREATE TABLE app.pt (id integer, b integer) PARTITION BY RANGE (id);
             CREATE TABLE app.p1 PARTITION OF app.pt FOR VALUES FROM (0) TO (10);
             ALTER TABLE app.p1 ADD CONSTRAINT p1ck CHECK (app.f_check(b));
             CREATE TABLE app.p2 PARTITION OF app.pt FOR VALUES FROM (10) TO (20)
                 PARTITION BY RANGE (id);
             CREATE TABLE app.p2a PARTITION OF app.p2 FOR VALUES FROM (10) TO (20);
             ALTER TABLE app.p2a ADD CONSTRAINT p2ack CHECK (app.f_cchk(b))",
        );
        let set = |routines: &[&'static str]| -> BTreeSet<&'static str> {
            routines.iter().copied().collect()
        };
        let (dom, cchk, check) = (
            "app.f_dom(integer)",
            "app.f_cchk(integer)",
            "app.f_check(integer)",
        );
        runtime().block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, own.connection())
                .await
                .unwrap();
            // An UPDATE of `x` and `z` through the parent reaches the child's
            // inherited `x` and its own `z`.
            macro_rules! through_the_parent {
                ($dropped:expr) => {
                    names(
                        &plan_time(&mut conn, "app.pp", update(&["x", "z"]), &$dropped, None).await,
                    )
                };
            }
            assert_eq!(
                through_the_parent!(Dropped::default()),
                set(&[dom, cchk]),
                "PG{version}"
            );
            // The parent's column goes, and the child's copy of it with it.
            let mut d = Dropped::default();
            d.columns.insert((table_named("pp"), "x".to_owned()));
            assert_eq!(
                through_the_parent!(d),
                set(&[cchk]),
                "PG{version}: the parent's column"
            );
            // The child's own column goes alone.
            let mut d = Dropped::default();
            d.columns.insert((table_named("cc"), "z".to_owned()));
            assert_eq!(
                through_the_parent!(d),
                set(&[dom]),
                "PG{version}: the child's column"
            );
            // Named on the child alone, `x` is the parent's still.
            let mut d = Dropped::default();
            d.columns.insert((table_named("cc"), "x".to_owned()));
            assert_eq!(
                through_the_parent!(d),
                set(&[dom, cchk]),
                "PG{version}: the parent's x stays"
            );
            let mut d = Dropped::default();
            d.columns.insert((table_named("pp"), "x".to_owned()));
            d.columns.insert((table_named("cc"), "z".to_owned()));
            assert_eq!(through_the_parent!(d), set(&[]), "PG{version}: both");

            // An INSERT through the partitioned parent reaches both leaves' checks.
            macro_rules! into_the_tree {
                ($table:expr, $detached:expr) => {{
                    let mut d = Dropped::default();
                    for name in $detached {
                        d.detached.insert(table_named(name));
                    }
                    names(&plan_time(&mut conn, $table, insert(&[]), &d, None).await)
                }};
            }
            let none: [&str; 0] = [];
            assert_eq!(
                into_the_tree!("app.pt", none),
                set(&[check, cchk]),
                "PG{version}"
            );
            assert_eq!(
                into_the_tree!("app.pt", ["p1"]),
                set(&[cchk]),
                "PG{version}: p1 detached"
            );
            // A detached partition takes its own partitions with it.
            assert_eq!(
                into_the_tree!("app.pt", ["p2"]),
                set(&[check]),
                "PG{version}: p2 detached"
            );
            assert_eq!(
                into_the_tree!("app.pt", ["p1", "p2"]),
                set(&[]),
                "PG{version}: both"
            );
            // A table that is no partition of it excuses nothing.
            assert_eq!(
                into_the_tree!("app.pt", ["pp"]),
                set(&[check, cchk]),
                "PG{version}: not a partition"
            );
            // A write that names the detached partition still runs what it holds.
            assert_eq!(
                into_the_tree!("app.p1", ["p1"]),
                set(&[check]),
                "PG{version}: written directly"
            );
            assert_eq!(
                into_the_tree!("app.p2", ["p2"]),
                set(&[cchk]),
                "PG{version}: its partitions, written directly"
            );
            // The recheck allows no detach: all of it is still reached.
            let at = names(&at_the_statement(&mut conn, "app.pt").await);
            assert_eq!(at, set(&[check, cchk]), "PG{version}: at the statement");
        });
    });
}

/// What a referential action's UPDATE runs on the referencing table (#2748).
/// Measured on 16 and 18 (DEC-2563.9): the CHECKs, the indexes and the generated
/// and domain CHECKs of the columns it sets run for `CASCADE`, `SET NULL` and
/// `SET DEFAULT`; the DEFAULT of those columns, and the DEFAULT their domain
/// gives one without its own, only for `SET DEFAULT`; a cascaded DELETE runs
/// nothing; `RESTRICT` and `NO ACTION` write nothing.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_referential_action_judges_the_default_only_when_it_sets_the_default() {
    each_server(|version, url| {
        let own = OwnDatabase::new(url, &format!("ee2748-fk-{version}"));
        let actions = [
            ("cascade", "CASCADE"),
            ("setnull", "SET NULL"),
            ("setdefault", "SET DEFAULT"),
            ("restrict", "RESTRICT"),
            ("noaction", "NO ACTION"),
        ];
        let mut sql = String::from(
            "CREATE SCHEMA app;
             CREATE FUNCTION app.f_check(integer) RETURNS boolean LANGUAGE plpgsql IMMUTABLE
                 AS $$BEGIN RETURN true; END$$;
             CREATE FUNCTION app.f_dom(integer) RETURNS boolean LANGUAGE plpgsql IMMUTABLE
                 AS $$BEGIN RETURN true; END$$;
             CREATE FUNCTION app.f_defa() RETURNS integer LANGUAGE plpgsql AS $$BEGIN RETURN 2; END$$;
             CREATE FUNCTION app.f_defb() RETURNS integer LANGUAGE plpgsql AS $$BEGIN RETURN 2; END$$;
             CREATE FUNCTION app.f_domdef() RETURNS integer LANGUAGE plpgsql AS $$BEGIN RETURN 2; END$$;
             CREATE DOMAIN app.dm AS integer DEFAULT app.f_domdef() CONSTRAINT dmc CHECK (app.f_dom(VALUE));",
        );
        for (key, action) in actions {
            sql.push_str(&format!(
                "CREATE TABLE app.p_{key} (id integer PRIMARY KEY);
                 CREATE TABLE app.c_{key} (id integer PRIMARY KEY, pid integer DEFAULT app.f_defa(),
                     CONSTRAINT ck_{key} CHECK (app.f_check(pid)),
                     FOREIGN KEY (pid) REFERENCES app.p_{key} (id)
                         ON UPDATE {action} ON DELETE {action});
                 CREATE TABLE app.d_{key} (id integer PRIMARY KEY, pid app.dm,
                     FOREIGN KEY (pid) REFERENCES app.p_{key} (id)
                         ON UPDATE {action} ON DELETE {action});"
            ));
        }
        // A column list (PostgreSQL 15 and later): only `b` is set, `a` keeps its value.
        for (key, action) in [("lnull", "SET NULL"), ("ldefault", "SET DEFAULT")] {
            sql.push_str(&format!(
                "CREATE TABLE app.p_{key} (a integer, b integer, PRIMARY KEY (a, b));
                 CREATE TABLE app.c_{key} (id integer PRIMARY KEY,
                     a integer DEFAULT app.f_defb(), b integer DEFAULT app.f_defa(),
                     CONSTRAINT ck_{key} CHECK (app.f_check(a)),
                     FOREIGN KEY (a, b) REFERENCES app.p_{key} (a, b) ON DELETE {action} (b));"
            ));
        }
        on_server(own.connection(), &sql);
        let set = |routines: &[&'static str]| -> BTreeSet<&'static str> {
            routines.iter().copied().collect()
        };
        let (check, dom, defa, defb, domdef) = (
            "app.f_check(integer)",
            "app.f_dom(integer)",
            "app.f_defa()",
            "app.f_defb()",
            "app.f_domdef()",
        );
        runtime().block_on(async {
            let mut conn = pbps_db::Conn::connect(pbps_db::Driver::Postgres, own.connection())
                .await
                .unwrap();
            // (action, what an UPDATE of the referenced key runs, what a DELETE runs)
            let expected = [
                ("cascade", set(&[check, dom]), set(&[])),
                ("setnull", set(&[check, dom]), set(&[check, dom])),
                (
                    "setdefault",
                    set(&[check, dom, defa, domdef]),
                    set(&[check, dom, defa, domdef]),
                ),
                ("restrict", set(&[]), set(&[])),
                ("noaction", set(&[]), set(&[])),
            ];
            for (key, on_update, on_delete) in expected {
                let table = format!("app.p_{key}");
                let updated = names(
                    &plan_time(
                        &mut conn,
                        &table,
                        update(&["id"]),
                        &Dropped::default(),
                        None,
                    )
                    .await,
                );
                assert_eq!(updated, on_update, "PG{version}: ON UPDATE {key}");
                let deleted = names(
                    &plan_time(
                        &mut conn,
                        &table,
                        RowOperation::Delete,
                        &Dropped::default(),
                        None,
                    )
                    .await,
                );
                assert_eq!(deleted, on_delete, "PG{version}: ON DELETE {key}");
            }
            // The column list: `a` is not set, so its default is not run.
            for (key, expected) in [("lnull", set(&[check])), ("ldefault", set(&[check, defa]))] {
                let deleted = names(
                    &plan_time(
                        &mut conn,
                        &format!("app.p_{key}"),
                        RowOperation::Delete,
                        &Dropped::default(),
                        None,
                    )
                    .await,
                );
                assert_eq!(deleted, expected, "PG{version}: column list {key}");
                assert!(
                    !deleted.contains(defb),
                    "PG{version}: {key} judged a default it does not run"
                );
            }
            // The recheck just before the statement judges the same.
            let at = names(&at_the_statement_of(&mut conn, "app.p_cascade", update(&["id"])).await);
            assert_eq!(
                at,
                set(&[check, dom]),
                "PG{version}: cascade at the statement"
            );
            let at =
                names(&at_the_statement_of(&mut conn, "app.p_setdefault", update(&["id"])).await);
            assert_eq!(
                at,
                set(&[check, dom, defa, domdef]),
                "PG{version}: set default at the statement"
            );
        });
    });
}

// ---------------------------------------------------------------------------
// Whole plans: a table that already carries an expression, then a plan that
// writes rows to it, or rewrites it.
// ---------------------------------------------------------------------------

const BASE: &str = "table: app.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  email: {type: text}\n  n: {type: integer}\nprimary_key: {name: pk_t, columns: [id]}\n";

/// Which site of the standing table calls the routine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Site {
    Check,
    IndexKey,
}

impl Site {
    fn declared(self) -> String {
        match self {
            Site::Check => format!("{BASE}checks:\n  ck: 'app.ok(email)'\n"),
            Site::IndexKey => {
                format!("{BASE}indexes:\n  ix:\n    keys:\n      - expression: 'app.ok(email)'\n")
            }
        }
    }

    /// The phrase a refusal uses for it.
    fn phrase(self) -> &'static str {
        match self {
            Site::Check => "the existing CHECK `ck` of app.t",
            Site::IndexKey => "the existing index `ix` of app.t (its keys)",
        }
    }
}

const FIX: &str = "ALTER FUNCTION app.ok(pg_catalog.text) SET search_path = pg_catalog, pg_temp";

struct Standing {
    _own: OwnDatabase,
    d: Demo,
    connection: String,
    plan: std::path::PathBuf,
    site: Site,
}

/// A deployed table whose `site` calls `app.ok(text)`, which is then made one the
/// predicate refuses: it sets no `search_path`. The routine was admitted when
/// the site was introduced, so this is exactly a pre-existing expression the
/// user never touched.
fn standing(version: i32, url: &str, site: Site, tag: &str, pin: bool) -> Standing {
    let slug = format!("ee2563-{tag}-{site:?}-{version}").to_lowercase();
    let own = OwnDatabase::new(url, &slug);
    let connection = own.connection().to_owned();
    let d = bootstrapped_demo(&connection, &slug, BASE);
    d.keyed(&connection);
    if pin {
        let config = d.dir.join("pbps.yml");
        let current = std::fs::read_to_string(&config).unwrap();
        std::fs::write(&config, format!("{current}pin_type_routines: true\n")).unwrap();
    }
    on_server(
        &connection,
        "CREATE FUNCTION app.ok(text) RETURNS boolean LANGUAGE plpgsql IMMUTABLE \
         SET search_path = pg_catalog AS $$BEGIN RETURN true; END$$",
    );
    d.table(&site.declared());
    let standing = Standing {
        _own: own,
        d,
        connection,
        plan: std::path::PathBuf::new(),
        site,
    };
    let standing = Standing {
        plan: standing.d.dir.join("ee-plan.json"),
        ..standing
    };
    let planned = standing.plan_now();
    assert_eq!(code(&planned), 0, "PG{version}: {}", text(&planned));
    let applied = standing.apply();
    assert_eq!(code(&applied), 0, "PG{version}: {}", text(&applied));
    on_server(
        &standing.connection,
        "ALTER FUNCTION app.ok(text) RESET search_path",
    );
    standing
}

fn text(out: &Output) -> String {
    format!("{}{}", stdout(out), stderr(out))
}

impl Standing {
    fn plan_now(&self) -> Output {
        succeeds(self.d.run(&["plan"]));
        self.d.commit();
        let _ = std::fs::remove_file(&self.plan);
        self.d.run(&[
            "plan",
            "--db",
            &self.connection,
            "--out",
            self.plan.to_str().unwrap(),
        ])
    }

    fn apply(&self) -> Output {
        approved_apply(
            &self.d,
            &self.connection,
            &self.plan,
            &[
                "--allow",
                "destructive,narrowing,constraint,row-security,data-delete,data-update",
            ],
        )
    }

    /// Declares the table as it stands, plus `more` (rows, a retyped column).
    fn declare(&self, table: &str) {
        self.d.table(table);
    }

    fn rows(&self) -> i64 {
        scalar(&self.connection, "SELECT count(*) FROM app.t")
    }

    fn saved(&self) -> pbps_model::SavedPlan {
        serde_json::from_slice(&std::fs::read(&self.plan).unwrap()).unwrap()
    }
}

fn each_site(f: impl Fn(i32, &str, Site)) {
    for (version, url) in [(16, old_server()), (18, server())] {
        for site in [Site::Check, Site::IndexKey] {
            f(version, &url, site);
        }
    }
}

fn with_rows(site: Site, rows: &str) -> String {
    format!("{}data:\n  mode: ensure\n  rows:\n{rows}", site.declared())
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_plan_that_writes_rows_is_refused_for_an_expression_it_never_touched() {
    each_site(|version, url, site| {
        let s = standing(version, url, site, "insert", false);
        s.declare(&with_rows(site, "    1: {email: a}\n"));
        let planned = s.plan_now();
        let shown = text(&planned);
        // At plan time, before anything is written, with the fix.
        assert_ne!(code(&planned), 0, "PG{version} {site:?}: {shown}");
        assert!(
            shown.contains(site.phrase()),
            "PG{version} {site:?}: {shown}"
        );
        assert!(shown.contains(FIX), "PG{version} {site:?}: {shown}");
        assert!(
            shown.contains("nothing has been changed"),
            "PG{version} {site:?}: {shown}"
        );
        assert_eq!(s.rows(), 0, "PG{version} {site:?}");
        // After the fix the same declaration plans and applies.
        on_server(&s.connection, FIX);
        let planned = s.plan_now();
        assert_eq!(
            code(&planned),
            0,
            "PG{version} {site:?}: {}",
            text(&planned)
        );
        let applied = s.apply();
        assert_eq!(
            code(&applied),
            0,
            "PG{version} {site:?}: {}",
            text(&applied)
        );
        assert_eq!(s.rows(), 1, "PG{version} {site:?}");
    });
}

/// A deployed table whose column `n` defaults to `app.okd()`, which is then made
/// one the predicate refuses: it sets no `search_path`.
fn standing_default(version: i32, url: &str) -> Standing {
    let slug = format!("ee2748-plan-default-{version}");
    let own = OwnDatabase::new(url, &slug);
    let connection = own.connection().to_owned();
    let d = bootstrapped_demo(&connection, &slug, BASE);
    d.keyed(&connection);
    on_server(
        &connection,
        "CREATE FUNCTION app.okd() RETURNS integer LANGUAGE plpgsql \
         SET search_path = pg_catalog AS $$BEGIN RETURN 7; END$$",
    );
    d.table(&BASE.replace(
        "n: {type: integer}",
        "n: {type: integer, default: 'app.okd()'}",
    ));
    let standing = Standing {
        plan: d.dir.join("ee-plan.json"),
        _own: own,
        d,
        connection,
        site: Site::Check,
    };
    let planned = standing.plan_now();
    assert_eq!(code(&planned), 0, "PG{version}: {}", text(&planned));
    let applied = standing.apply();
    assert_eq!(code(&applied), 0, "PG{version}: {}", text(&applied));
    on_server(
        &standing.connection,
        "ALTER FUNCTION app.okd() RESET search_path",
    );
    standing
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_row_that_supplies_a_column_is_not_refused_for_its_default() {
    for (version, url) in [(16, old_server()), (18, server())] {
        let s = standing_default(version, &url);
        let declared = BASE.replace(
            "n: {type: integer}",
            "n: {type: integer, default: 'app.okd()'}",
        );
        let with_rows = |rows: &str| format!("{declared}data:\n  mode: ensure\n  rows:\n{rows}");
        // The row names `n`, so its DEFAULT never runs: planned and applied.
        s.declare(&with_rows("    1: {email: a, n: 5}\n"));
        let planned = s.plan_now();
        assert_eq!(code(&planned), 0, "PG{version}: {}", text(&planned));
        let applied = s.apply();
        assert_eq!(code(&applied), 0, "PG{version}: {}", text(&applied));
        assert_eq!(s.rows(), 1, "PG{version}");
        // A row that leaves `n` out runs it, and the existing DEFAULT is refused.
        s.declare(&with_rows("    1: {email: a, n: 5}\n    2: {email: b}\n"));
        let planned = s.plan_now();
        let shown = text(&planned);
        assert_ne!(code(&planned), 0, "PG{version}: {shown}");
        assert!(
            shown.contains("the existing DEFAULT of the column `n` of app.t"),
            "PG{version}: {shown}"
        );
        assert_eq!(s.rows(), 1, "PG{version}");
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_update_is_refused_and_a_delete_only_plan_never_is() {
    each_site(|version, url, site| {
        let s = standing(version, url, site, "update", false);
        on_server(
            &s.connection,
            "ALTER FUNCTION app.ok(text) SET search_path = pg_catalog;
             INSERT INTO app.t VALUES (1, 'a', 1), (2, 'b', 2);
             ALTER FUNCTION app.ok(text) RESET search_path",
        );
        // An UPDATE of a row runs the check and every index.
        s.declare(&with_rows(site, "    1: {email: z}\n    2: {email: b}\n"));
        let planned = s.plan_now();
        let shown = text(&planned);
        assert_ne!(code(&planned), 0, "PG{version} {site:?}: {shown}");
        assert!(
            shown.contains(site.phrase()),
            "PG{version} {site:?}: {shown}"
        );
        // A DELETE runs neither (measured on 16 and 18): never refused.
        s.declare(&format!(
            "{}data:\n  mode: exact\n  rows:\n    1: {{email: a, n: 1}}\n",
            site.declared()
        ));
        let planned = s.plan_now();
        assert_eq!(
            code(&planned),
            0,
            "PG{version} {site:?}: {}",
            text(&planned)
        );
        let applied = s.apply();
        assert_eq!(
            code(&applied),
            0,
            "PG{version} {site:?}: {}",
            text(&applied)
        );
        assert_eq!(s.rows(), 1, "PG{version} {site:?}");
    });
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_column_retype_that_rewrites_the_table_is_refused_for_an_existing_index() {
    for (version, url) in [(16, old_server()), (18, server())] {
        let s = standing(version, &url, Site::IndexKey, "rewrite", false);
        // `integer` to `bigint` rebuilds the table and every index on it.
        s.declare(
            &s.site
                .declared()
                .replace("n: {type: integer}", "n: {type: bigint}"),
        );
        let planned = s.plan_now();
        let shown = text(&planned);
        assert_ne!(code(&planned), 0, "PG{version}: {shown}");
        assert!(shown.contains(s.site.phrase()), "PG{version}: {shown}");
        assert!(shown.contains(FIX), "PG{version}: {shown}");
        // A check is not run by a rewrite, so a table whose only expression is
        // one is not refused for it.
        let c = standing(version, &url, Site::Check, "rewrite", false);
        c.declare(
            &c.site
                .declared()
                .replace("n: {type: integer}", "n: {type: bigint}"),
        );
        let planned = c.plan_now();
        assert_eq!(code(&planned), 0, "PG{version}: {}", text(&planned));
        // Widening a bound in place touches no row: not a rewrite at all.
        let w = standing(version, &url, Site::IndexKey, "widen", false);
        w.declare(&w.site.declared().replace(
            "email: {type: text}",
            "email: {type: text}\n  note: {type: 'character varying(10)'}",
        ));
        assert_eq!(
            code(&w.plan_now()),
            0,
            "PG{version}: adding a column rewrites nothing"
        );
        succeeds(w.apply_sibling_widen());
    }
}

impl Standing {
    /// Applies the plan just made, and widens the bound of the column it added.
    fn apply_sibling_widen(&self) -> Output {
        let applied = self.apply();
        assert_eq!(code(&applied), 0, "{}", text(&applied));
        self.declare(&self.site.declared().replace(
            "email: {type: text}",
            "email: {type: text}\n  note: {type: 'character varying(20)'}",
        ));
        let planned = self.plan_now();
        assert_eq!(code(&planned), 0, "{}", text(&planned));
        self.apply()
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_table_the_plan_creates_has_nothing_standing_to_judge() {
    for (version, url) in [(16, old_server()), (18, server())] {
        let s = standing(version, &url, Site::Check, "created", false);
        // The standing check is unfit, and the plan neither writes to nor
        // rewrites the table it is on: it creates another and fills it.
        std::fs::write(
            s.d.dir.join("schema/app.u.yml"),
            "table: app.u\ncolumns:\n  id: {type: bigint, nullable: false}\n  label: {type: text}\nprimary_key: {name: pk_u, columns: [id]}\ndata:\n  mode: ensure\n  rows:\n    1: {label: x}\n",
        )
        .unwrap();
        let planned = s.plan_now();
        assert_eq!(code(&planned), 0, "PG{version}: {}", text(&planned));
        let applied = s.apply();
        assert_eq!(code(&applied), 0, "PG{version}: {}", text(&applied));
        assert_eq!(
            scalar(&s.connection, "SELECT count(*) FROM app.u"),
            1,
            "PG{version}"
        );
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_untrusted_owners_routine_is_shown_pinned_and_rechecked_at_apply() {
    for (version, url) in [(16, old_server()), (18, server())] {
        let stranger = format!("ee2563_p_{version}_{}", std::process::id());
        on_server(&url, &format!("DROP ROLE IF EXISTS {stranger}"));
        on_server(&url, &format!("CREATE ROLE {stranger}"));
        let s = standing(version, &url, Site::Check, "pin", true);
        on_server(
            &s.connection,
            &format!(
                "GRANT USAGE ON SCHEMA app TO {stranger};
                 ALTER FUNCTION app.ok(text) OWNER TO {stranger};
                 ALTER FUNCTION app.ok(text) SET search_path = pg_catalog"
            ),
        );
        s.declare(&with_rows(Site::Check, "    1: {email: a}\n"));
        let planned = s.plan_now();
        assert_eq!(code(&planned), 0, "PG{version}: {}", text(&planned));
        // The plan shows the routine and pins it.
        let saved = s.saved();
        assert_eq!(saved.type_routines.len(), 1, "PG{version}");
        assert!(saved.type_routine_pins.is_some(), "PG{version}");
        let applied = s.apply();
        assert_eq!(code(&applied), 0, "PG{version}: {}", text(&applied));
        assert_eq!(s.rows(), 1, "PG{version}");
        drop(s);
        on_server(&url, &format!("DROP ROLE IF EXISTS {stranger}"));
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn without_the_review_an_untrusted_owners_routine_is_refused_with_every_way_out() {
    for (version, url) in [(16, old_server()), (18, server())] {
        let stranger = format!("ee2563_q_{version}_{}", std::process::id());
        on_server(&url, &format!("DROP ROLE IF EXISTS {stranger}"));
        on_server(&url, &format!("CREATE ROLE {stranger}"));
        let s = standing(version, &url, Site::Check, "nopin", false);
        on_server(
            &s.connection,
            &format!(
                "GRANT USAGE ON SCHEMA app TO {stranger};
                 ALTER FUNCTION app.ok(text) OWNER TO {stranger};
                 ALTER FUNCTION app.ok(text) SET search_path = pg_catalog"
            ),
        );
        s.declare(&with_rows(Site::Check, "    1: {email: a}\n"));
        let planned = s.plan_now();
        let shown = text(&planned);
        assert_ne!(code(&planned), 0, "PG{version}: {shown}");
        for needle in [
            "`trusted_roles`",
            "`pin_type_routines: true`",
            "bring the routine into the project",
        ] {
            assert!(shown.contains(needle), "PG{version} {needle}: {shown}");
        }
        drop(s);
        on_server(&url, &format!("DROP ROLE IF EXISTS {stranger}"));
    }
}
