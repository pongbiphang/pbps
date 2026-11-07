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
                RowOperation::Insert,
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
                RowOperation::Copy,
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
                RowOperation::Insert,
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
                operation: RowOperation::Insert,
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
                RowOperation::Insert,
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
                RowOperation::Insert,
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
                RowOperation::Insert,
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
                RowOperation::Insert,
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
                on_server(
                    own.connection(),
                    &format!(
                        "ALTER TABLE app.t {} FORCE ROW LEVEL SECURITY",
                        if forced { "" } else { "NO" }
                    ),
                );
                for (what, operation) in [
                    ("INSERT", RowOperation::Insert),
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
                RowOperation::Insert,
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

const FIX: &str = "ALTER FUNCTION app.ok(text) SET search_path = pg_catalog, pg_temp";

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
            "{}data:\n  mode: exact\n  rows:\n    1: {{email: a}}\n",
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
