//! The reads behind the `ADD COLUMN` gate (#1523 PR C1, design #2046 layer 1),
//! on PostgreSQL 16 and 18.
//!
//! Every test runs the same fixture on both servers, in a database of its own
//! that the test creates and drops. The gate is read as a role that is not a
//! superuser (the deployer), because "trusted" is relative to it.

use std::collections::BTreeSet;

use super::user_types_tests::{Scratch, servers};
use super::*;
use pbps_model::TypeRef;
use pbps_pg::input_path::{
    self, Added, InputPath, OwnerReason, Part, Reason, RoutineKind, RoutineReason, Unreadable, Why,
};
use pbps_pg::pins::Within;
use pbps_pg::user_types::{self, TypeRead, UserType};

/// The types, read as `conn` and put through the gate with `trusted` declared.
async fn gate_all(conn: &mut Conn, trusted: &[&str], names: &[(&str, &str)]) -> Vec<InputPath> {
    let references: Vec<TypeRef> = names
        .iter()
        .map(|(schema, name)| {
            TypeRef::new(*schema, *name).unwrap_or_else(|e| panic!("{schema}.{name}: {e}"))
        })
        .collect();
    let reads = user_types::read(conn, Within::OwnTransaction, &references).await;
    let types: Vec<UserType> = reads
        .into_iter()
        .zip(names)
        .map(|(read, (schema, name))| match read {
            TypeRead::Present(t) => *t,
            other @ (TypeRead::Absent
            | TypeRead::NotDefined
            | TypeRead::Unreadable(_)
            | TypeRead::NotAdmitted(_)) => {
                panic!("{schema}.{name} should be present: {other:?}")
            }
        })
        .collect();
    let columns: Vec<Added<'_>> = types.iter().map(|t| Added::new(t, false)).collect();
    let trusted: Vec<pbps_model::RoleName> = trusted.iter().map(|t| (*t).into()).collect();
    input_path::read(conn, Within::OwnTransaction, &trusted, &columns).await
}

async fn gate(conn: &mut Conn, trusted: &[&str], name: &str) -> InputPath {
    gate_all(conn, trusted, &[("app", name)]).await.remove(0)
}

fn refused<'a>(path: &'a InputPath, what: &str) -> &'a [Reason] {
    match path {
        InputPath::Refused(reasons) => reasons,
        other @ (InputPath::Admitted(_) | InputPath::Unreadable(_)) => {
            panic!("{what} should be refused: {other:?}")
        }
    }
}

/// Whether the input path of an admitted type runs code.
fn admitted(path: &InputPath, what: &str) -> bool {
    match path {
        InputPath::Admitted(a) => a.runs_code(),
        other @ (InputPath::Refused(_) | InputPath::Unreadable(_)) => {
            panic!("{what} should be admitted: {other:?}")
        }
    }
}

fn routines(reasons: &[Reason]) -> Vec<&RoutineReason> {
    reasons
        .iter()
        .filter_map(|r| match r {
            Reason::Routine(r) => Some(r),
            Reason::Owner(_) | Reason::RoutineOwner(_) => None,
        })
        .collect()
}

fn owner_reasons(reasons: &[Reason]) -> Vec<&OwnerReason> {
    reasons
        .iter()
        .filter_map(|r| match r {
            Reason::Owner(o) => Some(o),
            Reason::Routine(_) | Reason::RoutineOwner(_) => None,
        })
        .collect()
}

fn actors(reasons: &[Reason]) -> BTreeSet<String> {
    owner_reasons(reasons)
        .iter()
        .map(|o| o.actor.raw().to_owned())
        .collect()
}

async fn run(conn: &mut Conn, label: &str, sql: &str) {
    conn.execute(sql)
        .await
        .unwrap_or_else(|e| panic!("on {label}: `{sql}`: {e}"));
}

/// A deployer that is not a superuser, the schema `app` it owns, and its
/// connection. What the fixture creates through that connection is the
/// deployer's.
async fn deployer(db: &mut Scratch) -> (String, Conn) {
    let role = db.role("deployer", "LOGIN").await;
    db.sql(&format!("CREATE SCHEMA app AUTHORIZATION {role}"))
        .await;
    let conn = db.as_role(&role).await;
    (role, conn)
}

/// Domains whose CHECK and DEFAULT call built-in code only, on a type the
/// deployer owns, are admitted: a comparison, a pattern match, a built-in
/// function, `now()` and `gen_random_uuid()`, casts of every shape (a cast
/// function, an I/O coercion, an array coercion, a domain), a user operator
/// whose function is built in; and the same through a nested domain, a
/// composite attribute, an array element and a range subtype. `pg_depend`, the
/// engine's own record, holds none of the built-ins they call, which is why the
/// gate reads the stored node trees (#1523, #2046).
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn built_in_code_on_a_type_the_deployer_owns_is_admitted() {
    for server in servers() {
        let mut db = Scratch::create(&server, "gbuiltin").await;
        let (_, mut me) = deployer(&mut db).await;
        let label = server.label;
        run(
            &mut me,
            label,
            "CREATE SEQUENCE app.sq;
             CREATE OPERATOR app.=== (leftarg = integer, rightarg = integer,
                 function = pg_catalog.int4eq);
             CREATE DOMAIN app.positive AS integer CHECK (VALUE > 0);
             CREATE DOMAIN app.lowercase AS text CHECK (VALUE ~ '^[a-z]+$');
             CREATE DOMAIN app.short AS text CHECK (length(VALUE) < 100);
             CREATE DOMAIN app.stamped AS timestamptz DEFAULT now();
             CREATE DOMAIN app.ident AS uuid DEFAULT gen_random_uuid();
             CREATE DOMAIN app.counted AS bigint DEFAULT nextval('app.sq');
             CREATE DOMAIN app.casted AS integer
                 CHECK (VALUE::bigint > 0 AND VALUE::numeric > 0 AND VALUE::text <> '');
             CREATE DOMAIN app.coerced AS integer[] CHECK (VALUE::bigint[] IS NOT NULL);
             CREATE DOMAIN app.misc AS integer
                 CHECK (VALUE IN (1, 2, 3) AND VALUE BETWEEN 1 AND 5
                        AND COALESCE(VALUE, 1) > 0 AND GREATEST(VALUE, 1) > 0);
             CREATE DOMAIN app.dated AS date CHECK (VALUE <= current_date);
             CREATE DOMAIN app.searched AS text
                 CHECK (pg_catalog.to_tsvector('pg_catalog.simple'::pg_catalog.regconfig, VALUE)
                        IS NOT NULL);
             CREATE DOMAIN app.user_op_builtin_fn AS integer CHECK (VALUE OPERATOR(app.===) 3);
             CREATE DOMAIN app.cast_nested AS integer CHECK (VALUE::app.positive IS NOT NULL);
             CREATE DOMAIN app.nested AS app.positive CHECK (VALUE < 10);
             CREATE TYPE app.pair AS (a app.positive, b app.short);
             CREATE DOMAIN app.positives AS app.positive[];
             CREATE TYPE app.span AS RANGE (subtype = app.positive);
             CREATE TYPE app.holds_span AS (s app.span);
             CREATE DOMAIN app.lower_positive AS int4range CHECK (lower(VALUE) > 0);",
        )
        .await;
        for name in [
            "positive",
            "lowercase",
            "short",
            "stamped",
            "ident",
            "counted",
            "casted",
            "coerced",
            "misc",
            "dated",
            "searched",
            "user_op_builtin_fn",
            "cast_nested",
            "nested",
            "pair",
            "positives",
            "holds_span",
            "lower_positive",
        ] {
            let path = gate(&mut me, &[], name).await;
            assert!(
                admitted(&path, &format!("app.{name} on {label}"))
                    == !matches!(name, "pair" | "positives" | "holds_span"),
                "app.{name} runs code on {label}"
            );
        }

        // The engine's record omits what it pinned: none of the built-ins these
        // call is in `pg_depend`, so it could not have answered the question.
        let rows = db
            .conn
            .query(
                "SELECT count(*)::bigint AS n
                   FROM pg_catalog.pg_depend d
                   JOIN pg_catalog.pg_constraint c
                     ON d.classid = 'pg_catalog.pg_constraint'::regclass AND d.objid = c.oid
                  WHERE c.contypid IN ('app.short'::regtype, 'app.positive'::regtype,
                                       'app.casted'::regtype, 'app.misc'::regtype)
                    AND d.refclassid IN ('pg_catalog.pg_proc'::regclass,
                                         'pg_catalog.pg_operator'::regclass)",
            )
            .await
            .unwrap();
        assert_eq!(
            rows[0].try_get::<i64>("n").unwrap(),
            Some(0),
            "pg_depend records a built-in on {label}"
        );

        // Negative: a domain with neither CHECK nor DEFAULT runs nothing.
        run(&mut me, label, "CREATE DOMAIN app.bare AS integer").await;
        let bare = gate(&mut me, &[], "bare").await;
        assert!(!admitted(&bare, "app.bare"), "a bare domain runs no code");

        // Inside the caller's transaction: the same answer, the transaction
        // still usable, the path it set gone with the savepoint.
        let before = me.query("SHOW search_path").await.unwrap();
        let before = before[0]
            .try_get::<&str>("search_path")
            .unwrap()
            .map(str::to_owned);
        run(&mut me, label, "BEGIN").await;
        let t = user_types::read(
            &mut me,
            Within::CallersTransaction,
            &[TypeRef::new("app", "short").unwrap()],
        )
        .await;
        let TypeRead::Present(short) = &t[0] else {
            panic!("{:?}", t[0])
        };
        let inside = input_path::read(
            &mut me,
            Within::CallersTransaction,
            &[],
            &[Added::new(short, false)],
        )
        .await;
        assert!(admitted(&inside[0], "app.short in a transaction"));
        let during = me.query("SHOW search_path").await.unwrap();
        assert_eq!(
            during[0]
                .try_get::<&str>("search_path")
                .unwrap()
                .map(str::to_owned),
            before,
            "the read left its search_path behind on {label}"
        );
        run(&mut me, label, "COMMIT").await;
        drop(me);
        db.drop().await;
    }
}

/// A function, an operator or a cast a role wrote, and the built-ins that run
/// SQL or a type's code by name, refuse a type that runs them, naming what and
/// whose; and so does a type that holds or names the domain: a nested domain, a
/// composite attribute, an array element, a range subtype.
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn user_code_on_the_input_path_is_refused_by_name_wherever_it_sits() {
    // name, the DDL of the base domain, an expected identity fragment, kind,
    // why, whether it is in the DEFAULT, and whether a range can be over it.
    struct Case {
        name: &'static str,
        ddl: &'static str,
        identity: &'static str,
        kind: RoutineKind,
        why: Why,
        default: bool,
        ranged: bool,
    }
    let cases = [
        Case {
            name: "u_fn",
            ddl: "CREATE DOMAIN app.u_fn AS integer CHECK (app.is_pos(VALUE))",
            identity: "app.is_pos(integer)",
            kind: RoutineKind::Function,
            why: Why::NotBuiltin,
            default: false,
            ranged: true,
        },
        Case {
            name: "u_op",
            ddl: "CREATE DOMAIN app.u_op AS integer CHECK (VALUE OPERATOR(app.@+@) 1 > 0)",
            identity: "app.@+@(integer,integer)",
            kind: RoutineKind::Operator,
            why: Why::NotBuiltin,
            default: false,
            ranged: true,
        },
        Case {
            name: "u_cast",
            ddl: "CREATE DOMAIN app.u_cast AS app.ut CHECK ((VALUE)::integer > 0)",
            identity: "app.ut_int(app.ut)",
            kind: RoutineKind::Function,
            why: Why::NotBuiltin,
            default: false,
            ranged: false,
        },
        Case {
            name: "u_default",
            ddl: "CREATE DOMAIN app.u_default AS integer DEFAULT app.plus(1, 2)",
            identity: "app.plus(integer,integer)",
            kind: RoutineKind::Function,
            why: Why::NotBuiltin,
            default: true,
            ranged: true,
        },
        Case {
            name: "q_xml",
            ddl: "CREATE DOMAIN app.q_xml AS text
                  CHECK (query_to_xml(VALUE, true, true, '') IS NOT NULL)",
            identity: "query_to_xml",
            kind: RoutineKind::Function,
            why: Why::RunsSql,
            default: false,
            ranged: true,
        },
        Case {
            name: "q_table",
            ddl: "CREATE DOMAIN app.q_table AS text
                  CHECK (table_to_xml('pg_class'::regclass, true, true, '') IS NOT NULL)",
            identity: "table_to_xml",
            kind: RoutineKind::Function,
            why: Why::RunsSql,
            default: false,
            ranged: true,
        },
        Case {
            name: "q_rewrite",
            ddl: "CREATE DOMAIN app.q_rewrite AS text
                  CHECK (ts_rewrite('a & b'::tsquery, 'select ''a''::tsquery, ''c''::tsquery')
                         IS NOT NULL)",
            identity: "ts_rewrite",
            kind: RoutineKind::Function,
            why: Why::RunsSql,
            default: false,
            ranged: true,
        },
        Case {
            name: "q_input",
            ddl: "CREATE DOMAIN app.q_input AS text CHECK (pg_input_is_valid(VALUE, 'integer'))",
            identity: "pg_input_is_valid",
            kind: RoutineKind::Function,
            why: Why::RunsSql,
            default: false,
            ranged: true,
        },
        Case {
            name: "q_error",
            ddl: "CREATE DOMAIN app.q_error AS text
                  CHECK ((pg_input_error_info(VALUE, 'integer')).message IS NULL)",
            identity: "pg_input_error_info",
            kind: RoutineKind::Function,
            why: Why::RunsSql,
            default: false,
            ranged: true,
        },
    ];
    for server in servers() {
        let mut db = Scratch::create(&server, "guser").await;
        let (me_name, mut me) = deployer(&mut db).await;
        let label = server.label;
        run(
            &mut me,
            label,
            "CREATE FUNCTION app.is_pos(integer) RETURNS boolean LANGUAGE sql IMMUTABLE
                 AS 'SELECT $1 > 0';
             CREATE FUNCTION app.plus(integer, integer) RETURNS integer LANGUAGE sql IMMUTABLE
                 AS 'SELECT $1 + $2';
             CREATE OPERATOR app.@+@ (leftarg = integer, rightarg = integer, function = app.plus);
             CREATE TYPE app.ut AS (x integer);
             CREATE FUNCTION app.ut_int(app.ut) RETURNS integer LANGUAGE sql IMMUTABLE
                 AS 'SELECT ($1).x';
             CREATE CAST (app.ut AS integer) WITH FUNCTION app.ut_int(app.ut);",
        )
        .await;
        for case in &cases {
            run(&mut me, label, case.ddl).await;
            let n = case.name;
            run(
                &mut me,
                label,
                &format!(
                    "CREATE DOMAIN app.nest_{n} AS app.{n};
                     CREATE TYPE app.comp_{n} AS (v app.{n});
                     CREATE DOMAIN app.arr_{n} AS app.{n}[];"
                ),
            )
            .await;
            let mut wrappers = vec![
                n.to_owned(),
                format!("nest_{n}"),
                format!("comp_{n}"),
                format!("arr_{n}"),
            ];
            if case.ranged {
                run(
                    &mut me,
                    label,
                    &format!(
                        "CREATE TYPE app.rng_{n} AS RANGE (subtype = app.{n});
                         CREATE TYPE app.holds_rng_{n} AS (r app.rng_{n});
                         CREATE DOMAIN app.dom_rng_{n} AS app.rng_{n};"
                    ),
                )
                .await;
                wrappers.push(format!("holds_rng_{n}"));
                wrappers.push(format!("dom_rng_{n}"));
            }
            for wrapper in &wrappers {
                let what = format!("app.{wrapper} on {label}");
                let path = gate(&mut me, &[], wrapper).await;
                if wrapper != n && wrapper != &format!("nest_{n}") {
                    assert!(
                        !admitted(&path, &what),
                        "{what}: NULL does not evaluate its parts"
                    );
                    continue;
                }
                let all = refused(&path, &what);
                let hits: Vec<&RoutineReason> = routines(all)
                    .into_iter()
                    .filter(|r| {
                        r.identity.contains(case.identity)
                            || r.runs.as_deref().is_some_and(|f| f.contains(case.identity))
                    })
                    .collect();
                assert!(!hits.is_empty(), "{what}: {all:?}");
                for hit in hits {
                    assert_eq!(hit.kind, case.kind, "{what}");
                    assert_eq!(hit.why, case.why, "{what}");
                    // Where it is written: the base domain's CHECK, or, for a
                    // DEFAULT, also the domain that inherited it.
                    assert!(hit.used_by.ty.ends_with(n), "{what}: {:?}", hit.used_by);
                    // The owner of what is called: the deployer's own routine,
                    // or the engine's bootstrap superuser for a built-in.
                    if case.why == Why::NotBuiltin {
                        assert_eq!(hit.owner, me_name.clone().into(), "{what}");
                    }
                    assert_eq!(
                        matches!(hit.used_by.part, Part::Default),
                        case.default,
                        "{what}: {:?}",
                        hit.used_by
                    );
                }
            }
        }
        drop(me);
        db.drop().await;
    }
}

/// A type an expression names, without being made of it, is on the path: the
/// cast runs that domain's CHECKs, and its owner can rewrite them. Followed
/// through a chain of casts, and through a DEFAULT.
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn a_type_an_expression_names_is_on_the_input_path() {
    for server in servers() {
        let mut db = Scratch::create(&server, "gnamed").await;
        let (_, mut me) = deployer(&mut db).await;
        let stranger = db.role("stranger", "NOLOGIN").await;
        let label = server.label;
        run(
            &mut me,
            label,
            "CREATE FUNCTION app.is_pos(integer) RETURNS boolean LANGUAGE sql IMMUTABLE
                 AS 'SELECT $1 > 0';
             CREATE DOMAIN app.inner_user AS integer CHECK (app.is_pos(VALUE));
             CREATE DOMAIN app.casts_inner AS integer CHECK (VALUE::app.inner_user IS NOT NULL);
             CREATE DOMAIN app.chain_c AS integer CHECK (app.is_pos(VALUE));
             CREATE DOMAIN app.chain_b AS integer CHECK (VALUE::app.chain_c IS NOT NULL);
             CREATE DOMAIN app.chain_a AS integer CHECK (VALUE::app.chain_b IS NOT NULL);
             CREATE DOMAIN app.defaults_to_inner AS integer DEFAULT 1::app.inner_user;
             CREATE DOMAIN app.holds_cast AS integer[] CHECK (VALUE::app.inner_user[] IS NOT NULL);
             CREATE DOMAIN app.built_in_inner AS integer CHECK (VALUE > 0);
             CREATE DOMAIN app.casts_built_in AS integer
                 CHECK (VALUE::app.built_in_inner IS NOT NULL);",
        )
        .await;
        db.sql(&format!(
            "GRANT USAGE ON SCHEMA app TO {stranger};
             CREATE DOMAIN app.owned_by_stranger AS integer CHECK (VALUE > 0);
             ALTER DOMAIN app.owned_by_stranger OWNER TO {stranger};"
        ))
        .await;
        run(
            &mut me,
            label,
            "CREATE DOMAIN app.casts_stranger AS integer
                 CHECK (VALUE::app.owned_by_stranger IS NOT NULL);",
        )
        .await;
        // The user function is in the CHECK of the domain the cast names.
        for (name, inner) in [
            ("casts_inner", "app.inner_user"),
            ("chain_a", "app.chain_c"),
            ("chain_b", "app.chain_c"),
            ("defaults_to_inner", "app.inner_user"),
            ("holds_cast", "app.inner_user"),
        ] {
            let path = gate(&mut me, &[], name).await;
            let all = refused(&path, name);
            let calls = routines(all);
            assert_eq!(calls.len(), 1, "{name} on {label}: {all:?}");
            assert_eq!(calls[0].identity, "app.is_pos(integer)");
            assert_eq!(calls[0].used_by.ty, inner, "{name} on {label}");
        }
        // Built-in all the way down: admitted.
        let path = gate(&mut me, &[], "casts_built_in").await;
        assert!(admitted(&path, "casts_built_in"));
        // The named domain is built in, and a stranger owns it: its owner is
        // named, though the outer type is the deployer's and is made of nothing
        // the stranger owns.
        let path = gate(&mut me, &[], "casts_stranger").await;
        let all = refused(&path, "casts_stranger");
        assert!(routines(all).is_empty(), "{all:?}");
        let owners = owner_reasons(all);
        assert_eq!(owners.len(), 1, "{all:?}");
        assert_eq!(owners[0].ty, "app.owned_by_stranger");
        assert_eq!(owners[0].owner, stranger.clone().into());
        assert_eq!(owners[0].actor, stranger.clone().into());
        drop(me);
        db.drop().await;
    }
}

/// `ADD COLUMN` evaluates the declared type's own DEFAULT and no other: a
/// domain it is made of contributes its CHECKs (measured on 16 and 18), so a
/// nested domain's DEFAULT that calls a user function does not refuse a type
/// that never runs it, and one the declared domain copied at creation does.
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn only_the_declared_types_own_default_is_on_the_input_path() {
    for server in servers() {
        let mut db = Scratch::create(&server, "gdefault").await;
        let (_, mut me) = deployer(&mut db).await;
        let label = server.label;
        run(
            &mut me,
            label,
            "CREATE FUNCTION app.boom_d() RETURNS integer LANGUAGE plpgsql
                 AS $$ BEGIN RAISE EXCEPTION 'boom default ran'; END $$;
             CREATE FUNCTION app.boom_c(integer) RETURNS boolean LANGUAGE plpgsql
                 AS $$ BEGIN RAISE EXCEPTION 'boom check ran'; END $$;
             -- the outer default shadows the inner one
             CREATE DOMAIN app.s_inner AS integer DEFAULT 5;
             ALTER DOMAIN app.s_inner SET DEFAULT app.boom_d();
             CREATE DOMAIN app.s_outer AS app.s_inner DEFAULT 1;
             -- a composite and an array hold a domain whose default is a user function
             CREATE DOMAIN app.p_inner AS integer DEFAULT app.boom_d();
             CREATE TYPE app.p_comp AS (v app.p_inner);
             CREATE DOMAIN app.p_arr AS app.p_inner[];
             -- the outer was created before the inner had a default
             CREATE DOMAIN app.l_inner AS integer;
             CREATE DOMAIN app.l_outer AS app.l_inner;
             ALTER DOMAIN app.l_inner SET DEFAULT app.boom_d();
             -- the outer copied the inner default
             CREATE DOMAIN app.cp_inner AS integer DEFAULT app.boom_d();
             CREATE DOMAIN app.cp_outer AS app.cp_inner;
             -- a nested domain's CHECK does run
             CREATE DOMAIN app.k_inner AS integer CHECK (app.boom_c(VALUE));
             CREATE DOMAIN app.k_outer AS app.k_inner DEFAULT 1;",
        )
        .await;
        // The engine runs none of the nested defaults (the same statements,
        // with the same domains, on a table that holds rows).
        run(
            &mut me,
            label,
            "CREATE TABLE app.rows (i integer);
             INSERT INTO app.rows VALUES (1), (2);
             ALTER TABLE app.rows ADD COLUMN a app.s_outer;
             ALTER TABLE app.rows ADD COLUMN b app.p_comp;
             ALTER TABLE app.rows ADD COLUMN c app.p_arr;
             ALTER TABLE app.rows ADD COLUMN d app.l_outer;",
        )
        .await;
        for (name, sql) in [
            ("cp_outer", "ALTER TABLE app.rows ADD COLUMN e app.cp_outer"),
            ("k_outer", "ALTER TABLE app.rows ADD COLUMN f app.k_outer"),
        ] {
            let err = me.execute(sql).await.expect_err("the user code must run");
            assert!(err.to_string().contains("boom"), "{name} on {label}: {err}");
        }
        // And the gate says the same.
        for name in ["s_outer", "p_comp", "p_arr", "l_outer"] {
            let path = gate(&mut me, &[], name).await;
            let ran = admitted(&path, &format!("{name} on {label}"));
            // `s_outer` has a default of its own (a literal); the rest have none.
            assert_eq!(ran, name == "s_outer", "{name} on {label}");
        }
        for (name, ty, part) in [
            ("cp_outer", "app.cp_outer", Part::Default),
            (
                "k_outer",
                "app.k_inner",
                Part::Check {
                    constraint: "k_inner_check".to_owned(),
                },
            ),
        ] {
            let path = gate(&mut me, &[], name).await;
            let all = refused(&path, name);
            let hits = routines(all);
            assert_eq!(hits.len(), 1, "{name} on {label}: {all:?}");
            assert_eq!(hits[0].used_by.ty, ty, "{name} on {label}");
            assert_eq!(hits[0].used_by.part, part, "{name} on {label}");
        }
        drop(me);
        db.drop().await;
    }
}

/// A DEFAULT the `ADD COLUMN` statement carries replaces the domain's, so the
/// domain's is not code the statement runs; the domain's CHECKs still run on the
/// value the statement supplies.
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn a_default_the_statement_carries_replaces_the_domains_and_its_checks_still_run() {
    for server in servers() {
        let mut db = Scratch::create(&server, "gowndefault").await;
        let (_, mut me) = deployer(&mut db).await;
        let label = server.label;
        run(
            &mut me,
            label,
            "CREATE FUNCTION app.boom_d() RETURNS integer LANGUAGE plpgsql
                 AS $$ BEGIN RAISE EXCEPTION 'boom default ran'; END $$;
             CREATE FUNCTION app.is_pos(integer) RETURNS boolean LANGUAGE sql IMMUTABLE
                 AS 'SELECT $1 > 0';
             CREATE DOMAIN app.default_only AS integer DEFAULT app.boom_d();
             CREATE DOMAIN app.default_and_builtin_check AS integer
                 DEFAULT app.boom_d() CHECK (VALUE > 0);
             CREATE DOMAIN app.default_and_user_check AS integer
                 DEFAULT app.boom_d() CHECK (app.is_pos(VALUE));
             CREATE TABLE app.rows (i integer);
             INSERT INTO app.rows VALUES (1), (2);",
        )
        .await;
        // The engine: the domain's default runs without a default of the
        // statement's own, and does not with one.
        let err = me
            .execute("ALTER TABLE app.rows ADD COLUMN a app.default_only")
            .await
            .expect_err("the domain default must run");
        assert!(err.to_string().contains("boom default ran"), "{err}");
        run(
            &mut me,
            label,
            "ALTER TABLE app.rows ADD COLUMN a app.default_only DEFAULT 7;
             ALTER TABLE app.rows ADD COLUMN b app.default_and_builtin_check DEFAULT 7;",
        )
        .await;

        let read = async |me: &mut Conn, name: &str, own_default: bool| -> InputPath {
            let TypeRead::Present(t) = user_types::read(
                me,
                Within::OwnTransaction,
                &[TypeRef::new("app", name).unwrap()],
            )
            .await
            .remove(0) else {
                panic!("{name} should be present")
            };
            input_path::read(
                me,
                Within::OwnTransaction,
                &[],
                &[Added::new(&t, own_default)],
            )
            .await
            .remove(0)
        };
        for name in ["default_only", "default_and_builtin_check"] {
            let without = read(&mut me, name, false).await;
            let all = refused(&without, name);
            assert_eq!(routines(all).len(), 1, "{name} on {label}: {all:?}");
            assert!(matches!(routines(all)[0].used_by.part, Part::Default));
        }
        // With a default of the statement's own: the domain's is not read. A
        // domain left with nothing to run runs no code.
        assert!(!admitted(
            &read(&mut me, "default_only", true).await,
            "default_only, own default"
        ));
        assert!(admitted(
            &read(&mut me, "default_and_builtin_check", true).await,
            "default_and_builtin_check, own default"
        ));
        // Negative: the CHECK is still read.
        let with = read(&mut me, "default_and_user_check", true).await;
        let all = refused(&with, "default_and_user_check");
        let hits = routines(all);
        assert_eq!(hits.len(), 1, "on {label}: {all:?}");
        assert_eq!(hits[0].identity, "app.is_pos(integer)");
        assert!(matches!(hits[0].used_by.part, Part::Check { .. }));
        drop(me);
        db.drop().await;
    }
}

/// A row comparison runs its operator family's btree comparison function for
/// the operator's types and never the operator's own function (measured on 16
/// and 18): a user operator over a family with a built-in comparison function
/// runs no user code, and a built-in operator in a family whose comparison
/// function a role wrote runs it.
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn a_row_comparison_runs_its_familys_comparison_function_and_not_the_operators() {
    for server in servers() {
        let mut db = Scratch::create(&server, "growcmp").await;
        let (_, mut me) = deployer(&mut db).await;
        let label = server.label;
        // An operator class takes a superuser.
        db.sql(
            "CREATE FUNCTION app.op_boom(integer, integer) RETURNS boolean LANGUAGE plpgsql
                 IMMUTABLE AS $$ BEGIN RAISE EXCEPTION 'operator ran'; END $$;
             CREATE FUNCTION app.cmp_boom(integer, integer) RETURNS integer LANGUAGE plpgsql
                 IMMUTABLE AS $$ BEGIN RAISE EXCEPTION 'comparison ran'; END $$;
             -- a user operator, in a family whose comparison function is built in
             CREATE OPERATOR app.@<@ (leftarg = integer, rightarg = integer,
                 function = app.op_boom);
             CREATE OPERATOR CLASS app.user_op_ops FOR TYPE integer USING btree AS
                 OPERATOR 1 app.@<@, OPERATOR 2 <=, OPERATOR 3 =, OPERATOR 4 >=, OPERATOR 5 >,
                 FUNCTION 1 btint4cmp(integer, integer);
             -- a built-in operator function, in a family whose comparison is a user's
             CREATE OPERATOR app.@<# (leftarg = integer, rightarg = integer,
                 function = int4lt);
             CREATE OPERATOR CLASS app.user_cmp_ops FOR TYPE integer USING btree AS
                 OPERATOR 1 app.@<#, OPERATOR 2 <=, OPERATOR 3 =, OPERATOR 4 >=, OPERATOR 5 >,
                 FUNCTION 1 app.cmp_boom(integer, integer);",
        )
        .await;
        run(
            &mut me,
            label,
            "CREATE DOMAIN app.user_operator AS integer
                 CHECK (ROW(VALUE, 1) OPERATOR(app.@<@) ROW(5, 5));
             CREATE DOMAIN app.user_comparison AS integer
                 CHECK (ROW(VALUE, 1) OPERATOR(app.@<#) ROW(5, 5));
             CREATE TABLE app.rows (i integer);
             INSERT INTO app.rows VALUES (1), (2);
             ALTER TABLE app.rows ADD COLUMN a app.user_operator DEFAULT 3;",
        )
        .await;
        let err = me
            .execute("ALTER TABLE app.rows ADD COLUMN b app.user_comparison DEFAULT 3")
            .await
            .expect_err("the comparison function must run");
        assert!(err.to_string().contains("comparison ran"), "{err}");

        let path = gate(&mut me, &[], "user_operator").await;
        assert!(
            admitted(&path, &format!("user_operator on {label}")),
            "the operator's own function is not what runs"
        );
        let path = gate(&mut me, &[], "user_comparison").await;
        let all = refused(&path, "user_comparison");
        let hits = routines(all);
        assert_eq!(hits.len(), 1, "on {label}: {all:?}");
        assert_eq!(hits[0].kind, RoutineKind::Operator);
        assert_eq!(hits[0].why, Why::NotBuiltin);
        assert!(hits[0].identity.contains("@<#"), "{:?}", hits[0]);
        assert_eq!(
            hits[0].runs.as_deref(),
            Some("app.cmp_boom(integer,integer)")
        );
        drop(me);
        db.drop().await;
    }
}

/// The node tree finds every user routine the engine's own record (`pg_depend`)
/// does, in every shape an expression can hold one (a branch, an argument, a
/// list, a range of `BETWEEN`, a row, a subscript, a coercion, a constructor),
/// and nothing else that is a user routine: the two answers are equal on both
/// servers. `pg_depend` alone cannot do it, since it holds no built-in.
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn the_node_tree_finds_every_user_routine_the_engines_record_holds() {
    let shapes: &[(&str, &str)] = &[
        ("plain", "integer CHECK (app.is_pos(VALUE))"),
        (
            "branch",
            "integer CHECK (CASE WHEN VALUE > 1 THEN app.is_pos(VALUE) ELSE true END)",
        ),
        (
            "coalesce",
            "integer CHECK (COALESCE(app.plus(VALUE, 1), 0) > 0)",
        ),
        (
            "between",
            "integer CHECK (VALUE BETWEEN app.plus(1, 1) AND 5)",
        ),
        ("in_list", "integer CHECK (VALUE IN (1, app.plus(1, 1)))"),
        (
            "any_array",
            "integer CHECK (VALUE = ANY (ARRAY[app.plus(1, 1), 3]))",
        ),
        ("argument", "integer CHECK (length(app.lbl(VALUE)) > 0)"),
        (
            "row",
            "integer CHECK (ROW(VALUE, app.plus(1, 1)) < ROW(5, 5))",
        ),
        (
            "nullif",
            "integer CHECK (NULLIF(VALUE, app.plus(1, 1)) IS DISTINCT FROM 7)",
        ),
        (
            "subscript",
            "integer CHECK ((ARRAY[app.plus(1, 1)])[1] > 0)",
        ),
        (
            "coercion",
            "integer CHECK (app.lbl(VALUE)::text::varchar IS NOT NULL)",
        ),
        (
            "greatest",
            "integer CHECK (GREATEST(VALUE, app.plus(1, 2)) > 0)",
        ),
        ("boolean_test", "integer CHECK (app.is_pos(VALUE) IS TRUE)"),
        ("operator", "integer CHECK (VALUE OPERATOR(app.@+@) 1 > 0)"),
        ("default_function", "integer DEFAULT app.plus(1, 2)"),
        (
            "default_operator",
            "integer DEFAULT (1 OPERATOR(app.@+@) 2)",
        ),
        (
            "constructor",
            "integer CHECK (JSON_OBJECT('a' : app.plus(1, 2)) IS NOT NULL)",
        ),
        (
            "xml",
            "integer CHECK (xmlexists('//a' PASSING BY VALUE
                 (xmlelement(name a, app.plus(1, 2))::text::xml)))",
        ),
        ("user_cast", "app.ut CHECK ((VALUE)::integer > 0)"),
    ];
    for server in servers() {
        let mut db = Scratch::create(&server, "gtree").await;
        let (_, mut me) = deployer(&mut db).await;
        let label = server.label;
        run(
            &mut me,
            label,
            "CREATE FUNCTION app.is_pos(integer) RETURNS boolean LANGUAGE sql IMMUTABLE
                 AS 'SELECT $1 > 0';
             CREATE FUNCTION app.plus(integer, integer) RETURNS integer LANGUAGE sql IMMUTABLE
                 AS 'SELECT $1 + $2';
             CREATE FUNCTION app.lbl(integer) RETURNS text LANGUAGE sql IMMUTABLE
                 AS 'SELECT $1::text';
             CREATE OPERATOR app.@+@ (leftarg = integer, rightarg = integer, function = app.plus);
             CREATE TYPE app.ut AS (x integer);
             CREATE FUNCTION app.ut_int(app.ut) RETURNS integer LANGUAGE sql IMMUTABLE
                 AS 'SELECT ($1).x';
             CREATE CAST (app.ut AS integer) WITH FUNCTION app.ut_int(app.ut);",
        )
        .await;
        // The same spelling for identities as the gate's.
        db.sql("SET search_path TO ''").await;
        for (name, body) in shapes {
            run(
                &mut me,
                label,
                &format!("CREATE DOMAIN app.shape_{name} AS {body}"),
            )
            .await;
            let engines: BTreeSet<String> = db
                .conn
                .query(&format!(
                    "SELECT i.identity AS identity
                       FROM pg_catalog.pg_depend d
                      CROSS JOIN LATERAL pg_catalog.pg_identify_object(
                                d.refclassid, d.refobjid, 0) i
                      WHERE ((d.classid = 'pg_catalog.pg_constraint'::regclass
                              AND d.objid IN (SELECT c.oid FROM pg_catalog.pg_constraint c
                                               WHERE c.contypid = 'app.shape_{name}'::regtype))
                          OR (d.classid = 'pg_catalog.pg_type'::regclass
                              AND d.objid = 'app.shape_{name}'::regtype))
                        AND d.refclassid IN ('pg_catalog.pg_proc'::regclass,
                                             'pg_catalog.pg_operator'::regclass)
                        AND d.refobjid >= 16384"
                ))
                .await
                .unwrap()
                .iter()
                .map(|r| r.try_get::<&str>("identity").unwrap().unwrap().to_owned())
                .collect();
            assert!(!engines.is_empty(), "{name} on {label} records no routine");
            let path = gate(&mut me, &[], &format!("shape_{name}")).await;
            let found: BTreeSet<String> = routines(refused(&path, name))
                .iter()
                .map(|r| r.identity.clone())
                .collect();
            assert_eq!(found, engines, "{name} on {label}");
        }
        drop(me);
        db.drop().await;
    }
}

/// The owner of a type that runs code must be trusted: a superuser, a role that
/// can `SET ROLE` to the deployer or to a superuser, or one the caller declared.
/// An owner that is none of them is named in the refusal, and so is a role that
/// can act as an owner that was declared.
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn the_owner_of_a_type_that_runs_code_must_be_trusted() {
    for server in servers() {
        let mut db = Scratch::create(&server, "gowners").await;
        let (deployer_name, mut me) = deployer(&mut db).await;
        let label = server.label;
        let pal = db.role("pal", "NOLOGIN").await;
        let stranger = db.role("stranger", "NOLOGIN").await;
        let listed = db.role("listed", "NOLOGIN").await;
        let setter = db.role("setter", "NOLOGIN").await;
        db.sql(&format!(
            "GRANT {deployer_name} TO {pal} WITH SET TRUE, INHERIT FALSE;
             GRANT {listed} TO {setter} WITH SET TRUE, INHERIT FALSE;
             GRANT USAGE ON SCHEMA app TO {pal}, {stranger}, {listed};
             CREATE DOMAIN app.by_deployer AS integer CHECK (VALUE > 0);
             ALTER DOMAIN app.by_deployer OWNER TO {deployer_name};
             CREATE DOMAIN app.by_pal AS integer CHECK (VALUE > 0);
             ALTER DOMAIN app.by_pal OWNER TO {pal};
             CREATE DOMAIN app.by_superuser AS integer CHECK (VALUE > 0);
             CREATE DOMAIN app.by_stranger AS integer CHECK (VALUE > 0);
             ALTER DOMAIN app.by_stranger OWNER TO {stranger};
             CREATE DOMAIN app.by_listed AS integer CHECK (VALUE > 0);
             ALTER DOMAIN app.by_listed OWNER TO {listed};"
        ))
        .await;
        // The deployer, a role that can become it, a superuser: trusted.
        for name in ["by_deployer", "by_pal", "by_superuser"] {
            let path = gate(&mut me, &[], name).await;
            assert!(admitted(&path, &format!("{name} on {label}")), "{name}");
        }
        // A role nobody trusts owns it: named, as the owner and as the actor.
        let path = gate(&mut me, &[], "by_stranger").await;
        let all = refused(&path, "by_stranger");
        assert!(routines(all).is_empty(), "{all:?}");
        assert_eq!(
            owner_reasons(all),
            vec![&OwnerReason {
                ty: "app.by_stranger".to_owned(),
                owner: stranger.clone().into(),
                actor: stranger.clone().into(),
            }],
            "on {label}"
        );
        // Declared trusted: the same type is admitted.
        assert!(admitted(
            &gate(&mut me, &[stranger.as_str()], "by_stranger").await,
            "by_stranger, declared"
        ));
        // Declaring a role does not declare whoever can act as it: the member
        // that can `SET ROLE` to `listed` is named until it is declared too.
        let path = gate(&mut me, &[], "by_listed").await;
        assert_eq!(
            actors(refused(&path, "by_listed")),
            BTreeSet::from([listed.clone(), setter.clone()]),
            "on {label}"
        );
        let path = gate(&mut me, &[listed.as_str()], "by_listed").await;
        assert_eq!(
            actors(refused(&path, "by_listed, listed")),
            BTreeSet::from([setter.clone()]),
            "on {label}"
        );
        let both = gate(&mut me, &[listed.as_str(), setter.as_str()], "by_listed").await;
        assert!(admitted(&both, "by_listed, both declared"));
        // Negative: a declared role never admits a function.
        run(
            &mut me,
            label,
            "CREATE FUNCTION app.is_pos(integer) RETURNS boolean LANGUAGE sql IMMUTABLE
                 AS 'SELECT $1 > 0';
             CREATE DOMAIN app.user_fn AS integer CHECK (app.is_pos(VALUE));",
        )
        .await;
        let path = gate(&mut me, &[stranger.as_str(), listed.as_str()], "user_fn").await;
        assert_eq!(routines(refused(&path, "user_fn")).len(), 1);

        // A role declared trusted that does not exist is an error, for a type
        // that runs code and for one that does not, and is never an empty grant.
        let ghost = "no_such_role_for_1523";
        db.sql("CREATE TYPE app.plain_enum AS ENUM ('a')").await;
        for name in ["by_deployer", "plain_enum"] {
            let path = gate(&mut me, &[ghost], name).await;
            assert_eq!(
                path,
                InputPath::Unreadable(Unreadable::TrustedRolesMissing(vec![
                    ghost.to_owned().into()
                ])),
                "{name} on {label}"
            );
            assert!(!path.is_admitted());
        }
        let missing = input_path::missing_roles(
            &mut me,
            &[
                stranger.clone().into(),
                ghost.to_owned().into(),
                ghost.to_owned().into(),
                pbps_model::RoleName::default(),
            ],
        )
        .await
        .unwrap();
        assert_eq!(missing, vec![pbps_model::RoleName::default(), ghost.into()]);
        assert!(
            input_path::missing_roles(&mut me, &[stranger.clone().into(), listed.clone().into()])
                .await
                .unwrap()
                .is_empty()
        );
        drop(me);
        db.drop().await;
    }
}

/// A trusted role owns the type, and a role that is not trusted can act as it
/// by inheriting its privileges or by administering it: both are named. A
/// membership with no option set acts as nothing, and is not.
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn an_untrusted_role_that_can_act_as_a_trusted_owner_is_named() {
    for server in servers() {
        let mut db = Scratch::create(&server, "gacting").await;
        let (deployer_name, mut me) = deployer(&mut db).await;
        let label = server.label;
        let pal = db.role("pal", "NOLOGIN").await;
        let inheritor = db.role("inheritor", "NOLOGIN").await;
        let administrator = db.role("admin", "NOLOGIN").await;
        let inert = db.role("inert", "NOLOGIN").await;
        let setter = db.role("setter", "NOLOGIN").await;
        let owned = db.role("owned", "NOLOGIN").await;
        db.sql(&format!(
            "GRANT {deployer_name} TO {pal} WITH SET TRUE, INHERIT FALSE;
             GRANT {pal} TO {inheritor} WITH INHERIT TRUE, SET FALSE;
             GRANT {pal} TO {administrator} WITH ADMIN TRUE, INHERIT FALSE, SET FALSE;
             GRANT {pal} TO {inert} WITH ADMIN FALSE, INHERIT FALSE, SET FALSE;
             GRANT {owned} TO {setter} WITH SET TRUE, INHERIT FALSE;
             GRANT USAGE ON SCHEMA app TO {pal}, {owned};
             CREATE DOMAIN app.by_pal AS integer CHECK (VALUE > 0);
             ALTER DOMAIN app.by_pal OWNER TO {pal};
             CREATE DOMAIN app.by_owned AS integer CHECK (VALUE > 0);
             ALTER DOMAIN app.by_owned OWNER TO {owned};"
        ))
        .await;
        let path = gate(&mut me, &[], "by_pal").await;
        let all = refused(&path, "by_pal");
        assert_eq!(
            actors(all),
            BTreeSet::from([inheritor.clone(), administrator.clone()]),
            "on {label}"
        );
        assert!(
            owner_reasons(all)
                .iter()
                .all(|o| o.owner == pal.clone().into())
        );
        assert!(!actors(all).contains(&inert));
        // The same type, once the owner and the two are declared.
        let declared = gate(
            &mut me,
            &[inheritor.as_str(), administrator.as_str()],
            "by_pal",
        )
        .await;
        assert!(admitted(&declared, "by_pal, members declared"));
        // `SET` alone is enough to act as an owner that is not trusted.
        let path = gate(&mut me, &[owned.as_str()], "by_owned").await;
        assert_eq!(
            actors(refused(&path, "by_owned")),
            BTreeSet::from([setter.clone()]),
            "on {label}"
        );
        drop(me);
        db.drop().await;
    }
}

/// A role that inherits from, or can administer, the deployer itself can act as
/// what the deployer owns, and is named; a role that can `SET ROLE` to it is the
/// deployer for these purposes and is not.
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn a_role_that_inherits_from_or_administers_the_deployer_is_named() {
    for server in servers() {
        let mut db = Scratch::create(&server, "gmembers").await;
        let (deployer_name, mut me) = deployer(&mut db).await;
        let label = server.label;
        let inherits = db.role("inherits", "NOLOGIN").await;
        let administers = db.role("administers", "NOLOGIN").await;
        let becomes = db.role("becomes", "NOLOGIN").await;
        let via_becomes = db.role("via_becomes", "NOLOGIN").await;
        db.sql(&format!(
            "GRANT {deployer_name} TO {inherits} WITH INHERIT TRUE, SET FALSE;
             GRANT {deployer_name} TO {administers} WITH ADMIN TRUE, INHERIT FALSE, SET FALSE;
             GRANT {deployer_name} TO {becomes} WITH SET TRUE, INHERIT FALSE;
             GRANT {becomes} TO {via_becomes} WITH SET TRUE, INHERIT TRUE;"
        ))
        .await;
        run(
            &mut me,
            label,
            "CREATE DOMAIN app.mine AS integer CHECK (VALUE > 0);
             CREATE TYPE app.pair AS (m app.mine);",
        )
        .await;
        let path = gate(&mut me, &[], "pair").await;
        assert_eq!(
            actors(refused(&path, "NULL pair")),
            BTreeSet::from([inherits.clone(), administers.clone()])
        );
        assert!(!admitted(
            &gate(&mut me, &[inherits.as_str(), administers.as_str()], "pair").await,
            "trusted NULL pair"
        ));
        for name in ["mine"] {
            let path = gate(&mut me, &[], name).await;
            assert_eq!(
                actors(refused(&path, name)),
                BTreeSet::from([inherits.clone(), administers.clone()]),
                "{name} on {label}"
            );
            // Declared, they are trusted.
            let declared = gate(&mut me, &[inherits.as_str(), administers.as_str()], name).await;
            assert!(admitted(&declared, name));
        }
        drop(me);
        db.drop().await;
    }
}

/// Owners of a code-free input path still need trust: they can add code later.
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn a_type_that_runs_no_code_still_requires_trusted_owners() {
    for server in servers() {
        let mut db = Scratch::create(&server, "gnocode").await;
        let (_, mut me) = deployer(&mut db).await;
        let stranger = db.role("stranger", "NOLOGIN").await;
        let label = server.label;
        db.sql(&format!(
            "GRANT USAGE ON SCHEMA app TO {stranger};
             CREATE TYPE app.mood AS ENUM ('sad', 'happy');
             CREATE TYPE app.pair AS (a integer, b text, c app.mood);
             CREATE DOMAIN app.required AS integer NOT NULL;
             CREATE DOMAIN app.required_text AS text NOT NULL;
             CREATE DOMAIN app.over_enum AS app.mood;
             CREATE DOMAIN app.moods AS app.mood[];
             CREATE TYPE app.holds AS (p app.pair, r app.required, m app.moods);
             ALTER TYPE app.mood OWNER TO {stranger};
             ALTER TYPE app.pair OWNER TO {stranger};
             ALTER DOMAIN app.required OWNER TO {stranger};
             ALTER DOMAIN app.required_text OWNER TO {stranger};
             ALTER DOMAIN app.over_enum OWNER TO {stranger};
             ALTER DOMAIN app.moods OWNER TO {stranger};
             ALTER TYPE app.holds OWNER TO {stranger};"
        ))
        .await;
        for name in [
            "mood",
            "pair",
            "required",
            "required_text",
            "over_enum",
            "moods",
            "holds",
        ] {
            let path = gate(&mut me, &[], name).await;
            assert!(actors(refused(&path, name)).contains(&stranger), "{path:?}");
            assert!(!admitted(
                &gate(&mut me, &[stranger.as_str()], name).await,
                &format!("{name} trusted on {label}")
            ));
        }
        let reference = TypeRef::new("app", "required").unwrap();
        let reads = user_types::read(&mut me, Within::OwnTransaction, &[reference]).await;
        let TypeRead::Present(ty) = &reads[0] else {
            panic!("required type disappeared")
        };
        // NULL and proven-empty arrays skip element evaluation, never owners.
        for column in [
            Added::new(ty, true).array(true).null_fill(),
            Added::new(ty, true).array(true).empty_array_fill(),
        ] {
            let paths = input_path::read(&mut me, Within::OwnTransaction, &[], &[column]).await;
            assert!(actors(refused(&paths[0], "array owner")).contains(&stranger));
            let paths = input_path::read(
                &mut me,
                Within::OwnTransaction,
                &[stranger.clone().into()],
                &[column],
            )
            .await;
            assert!(!admitted(&paths[0], "trusted array owner"));
        }
        // Code remains classified separately from unconditional owner trust.
        db.sql(&format!(
            "CREATE DOMAIN app.checked AS integer CHECK (VALUE > 0);
             ALTER DOMAIN app.checked OWNER TO {stranger};
             CREATE TYPE app.holds_checked AS (c app.checked, m app.mood);
             ALTER TYPE app.holds_checked OWNER TO {stranger};
             CREATE DOMAIN app.defaulted AS integer DEFAULT 5;
             ALTER DOMAIN app.defaulted OWNER TO {stranger};"
        ))
        .await;
        assert!(!admitted(
            &gate(&mut me, &[stranger.as_str()], "holds_checked").await,
            "NULL composite"
        ));
        for name in ["checked", "defaulted"] {
            let path = gate(&mut me, &[], name).await;
            let all = refused(&path, name);
            assert!(
                actors(all).contains(&stranger),
                "{name} on {label}: {all:?}"
            );
        }
        // An enum contributes no code even beside a domain CHECK.
        run(
            &mut me,
            label,
            "CREATE DOMAIN app.checked_mood AS app.mood CHECK (VALUE IS NULL OR VALUE = 'happy');
             CREATE DOMAIN app.constant_pair AS app.pair DEFAULT '(1,x,happy)'::app.pair;",
        )
        .await;
        assert!(admitted(
            &gate(&mut me, &[stranger.as_str()], "checked_mood").await,
            "the enum owner is explicitly trusted"
        ));
        assert!(admitted(
            &gate(&mut me, &[stranger.as_str()], "constant_pair").await,
            "a stored composite constant runs no input conversion"
        ));
        run(
            &mut me,
            label,
            "CREATE TABLE app.rows (id integer); INSERT INTO app.rows VALUES (1);
            ALTER TABLE app.rows ADD COLUMN mood app.mood;
            ALTER TABLE app.rows ADD COLUMN checked app.checked_mood;
            ALTER TABLE app.rows ADD COLUMN pair app.constant_pair;",
        )
        .await;
        drop(me);
        db.drop().await;
    }
}

/// A read that fails, or finds the type gone, is unreadable and never admitted;
/// a built-in function the audited list names and the server lacks would be the
/// same, and the list is on both servers.
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn a_read_that_fails_is_unreadable_and_never_admitted() {
    for server in servers() {
        let mut db = Scratch::create(&server, "gfails").await;
        let (_, mut me) = deployer(&mut db).await;
        let label = server.label;
        run(
            &mut me,
            label,
            "CREATE DOMAIN app.gone AS integer CHECK (VALUE > 0);
             CREATE DOMAIN app.stays AS integer CHECK (VALUE > 0);",
        )
        .await;
        // The type is read, then dropped, then put through the gate.
        let read = user_types::read(
            &mut me,
            Within::OwnTransaction,
            &[
                TypeRef::new("app", "gone").unwrap(),
                TypeRef::new("app", "stays").unwrap(),
            ],
        )
        .await;
        let types: Vec<UserType> = read
            .into_iter()
            .map(|r| match r {
                TypeRead::Present(t) => *t,
                other @ (TypeRead::Absent
                | TypeRead::NotDefined
                | TypeRead::Unreadable(_)
                | TypeRead::NotAdmitted(_)) => panic!("{other:?}"),
            })
            .collect();
        run(&mut me, label, "DROP DOMAIN app.gone").await;
        let columns: Vec<Added<'_>> = types.iter().map(|t| Added::new(t, false)).collect();
        let paths = input_path::read(&mut me, Within::OwnTransaction, &[], &columns).await;
        assert_eq!(paths.len(), 2);
        // A failed read is a failed read for every type: nothing was learned.
        for path in &paths {
            let InputPath::Unreadable(Unreadable::Failed(why)) = path else {
                panic!("expected a failed read on {label}: {path:?}")
            };
            assert!(why.contains("disappeared"), "{why}");
            assert!(!path.is_admitted());
        }
        // The same types, read in a transaction already aborted, are unreadable
        // too, not admitted.
        run(&mut me, label, "BEGIN").await;
        assert!(me.execute("SELECT 1/0").await.is_err());
        let aborted =
            input_path::read(&mut me, Within::CallersTransaction, &[], &columns[1..]).await;
        assert!(
            matches!(aborted[0], InputPath::Unreadable(Unreadable::Failed(_))),
            "{:?}",
            aborted[0]
        );
        run(&mut me, label, "ROLLBACK").await;
        drop(me);
        db.drop().await;
    }
}

/// Every function on the list of built-ins that run SQL or a type's code exists
/// on the server, is built in, and does what the list says: it runs the SQL it
/// is given, or the CHECK of the domain it is told to read. A new major has to
/// be audited against this list (the module's "Re-audit per major").
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn every_function_on_the_runs_sql_list_exists_is_built_in_and_runs_what_it_is_given() {
    for server in servers() {
        let mut db = Scratch::create(&server, "gdeny").await;
        let label = server.label;
        for sig in input_path::RUNS_SQL {
            let rows = db
                .conn
                .query(&format!(
                    "SELECT pg_catalog.to_regprocedure('{sig}')::pg_catalog.oid::pg_catalog.int8 AS oid"
                ))
                .await
                .unwrap();
            let oid = rows[0].try_get::<i64>("oid").unwrap();
            assert!(
                oid.is_some_and(|oid| oid < 16384),
                "{sig} is not a built-in function of {label}: {oid:?}"
            );
        }
        // Negative: the one overload of `ts_rewrite` that runs no SQL exists
        // and is not listed.
        let rows = db
            .conn
            .query(
                "SELECT pg_catalog.to_regprocedure(
                     'pg_catalog.ts_rewrite(tsquery,tsquery,tsquery)')::pg_catalog.oid::pg_catalog.int8
                     AS oid",
            )
            .await
            .unwrap();
        assert!(rows[0].try_get::<i64>("oid").unwrap().is_some());
        assert!(
            !input_path::RUNS_SQL
                .iter()
                .any(|s| s.contains("tsquery,tsquery,tsquery"))
        );

        db.sql(
            "CREATE SCHEMA app;
             CREATE FUNCTION app.boom() RETURNS text LANGUAGE plpgsql
                 AS $$ BEGIN RAISE EXCEPTION 'boom ran'; END $$;
             CREATE FUNCTION app.boom_tsv() RETURNS tsvector LANGUAGE plpgsql
                 AS $$ BEGIN RAISE EXCEPTION 'boom ran'; END $$;
             CREATE FUNCTION app.boom_q() RETURNS tsquery LANGUAGE plpgsql
                 AS $$ BEGIN RAISE EXCEPTION 'boom ran'; END $$;
             CREATE FUNCTION app.boom_check(integer) RETURNS boolean LANGUAGE plpgsql
                 AS $$ BEGIN RAISE EXCEPTION 'boom ran'; END $$;
             CREATE DOMAIN app.trap AS integer CHECK (app.boom_check(VALUE));",
        )
        .await;
        for sql in [
            "SELECT query_to_xml('select app.boom()', true, true, '')",
            "SELECT pg_input_is_valid('1', 'app.trap')",
            "SELECT pg_input_error_info('1', 'app.trap')",
            "SELECT * FROM ts_stat('select app.boom_tsv()')",
            "SELECT ts_rewrite('a'::tsquery, 'select app.boom_q(), app.boom_q()')",
        ] {
            let err = db
                .conn
                .execute(sql)
                .await
                .expect_err(&format!("`{sql}` ran no user code on {label}"));
            assert!(err.to_string().contains("boom ran"), "`{sql}`: {err}");
        }
        db.drop().await;
    }
}

/// Reading a type for the gate runs none of its code: a domain whose CHECK
/// raises is refused by name, and the function never ran.
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn reading_the_gate_runs_none_of_the_types_code() {
    for server in servers() {
        let mut db = Scratch::create(&server, "ginert").await;
        let (_, mut me) = deployer(&mut db).await;
        let label = server.label;
        run(
            &mut me,
            label,
            "CREATE FUNCTION app.boom(integer) RETURNS boolean LANGUAGE plpgsql
                 AS $$ BEGIN RAISE EXCEPTION 'boom ran'; END $$;
             CREATE FUNCTION app.boom_default() RETURNS integer LANGUAGE plpgsql
                 AS $$ BEGIN RAISE EXCEPTION 'boom default ran'; END $$;
             CREATE DOMAIN app.trap AS integer DEFAULT app.boom_default() CHECK (app.boom(VALUE));
             CREATE TYPE app.holds AS (t app.trap);",
        )
        .await;
        // The declared domain runs its CHECK and its DEFAULT; a composite
        // holding it runs the CHECK and never the DEFAULT.
        for (name, expected) in [
            ("trap", vec!["app.boom(integer)", "app.boom_default()"]),
            ("holds", vec![]),
        ] {
            let path = gate(&mut me, &[], name).await;
            if expected.is_empty() {
                assert!(!admitted(&path, name));
                continue;
            }
            let all = refused(&path, name);
            let identities: BTreeSet<&str> =
                routines(all).iter().map(|r| r.identity.as_str()).collect();
            assert_eq!(
                identities,
                expected.into_iter().collect::<BTreeSet<_>>(),
                "{name} on {label}"
            );
            let parts: BTreeSet<bool> = routines(all)
                .iter()
                .map(|r| matches!(r.used_by.part, Part::Default))
                .collect();
            let want: BTreeSet<bool> = if name == "trap" {
                BTreeSet::from([false, true])
            } else {
                BTreeSet::from([false])
            };
            assert_eq!(parts, want, "{name} on {label}");
        }
        drop(me);
        db.drop().await;
    }
}

/// The NULL value itself contains no attributes, elements or bounds to coerce.
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn a_null_fill_checks_domains_but_not_composite_attributes_array_elements_or_range_bounds() {
    for server in servers() {
        let mut db = Scratch::create(&server, "nullfill").await;
        db.sql(
            "CREATE SCHEMA app;
            CREATE TABLE app.calls (n integer);
            CREATE FUNCTION app.seen(integer) RETURNS boolean LANGUAGE plpgsql AS $$
                BEGIN INSERT INTO app.calls VALUES (1); RETURN true; END $$;
            CREATE DOMAIN app.checked AS integer CHECK (app.seen(VALUE));
            CREATE TYPE app.pair AS (v app.checked);
            CREATE TYPE app.span AS RANGE (subtype = app.checked);
            CREATE DOMAIN app.array_domain AS app.checked[];
            CREATE DOMAIN app.range_domain AS app.span;
            CREATE TABLE app.rows (id integer);
            INSERT INTO app.rows VALUES (1), (2);
            TRUNCATE app.calls;
            ALTER TABLE app.rows ADD COLUMN c app.pair;
            ALTER TABLE app.rows ADD COLUMN a app.checked[];
            ALTER TABLE app.rows ADD COLUMN r app.span;
            ALTER TABLE app.rows ADD COLUMN ad app.array_domain;
            ALTER TABLE app.rows ADD COLUMN rd app.range_domain;",
        )
        .await;
        assert_eq!(
            number(&mut db.conn, "SELECT count(*)::integer AS n FROM app.calls").await,
            0,
            "PG {}",
            server.label
        );
        for name in ["pair", "array_domain", "range_domain"] {
            let path = gate(&mut db.conn, &[], name).await;
            assert!(
                !admitted(&path, name),
                "NULL fill runs no nested code on PG {}",
                server.label
            );
        }
        let checked = super::user_types_tests::present(&mut db.conn, "app", "checked").await;
        let answer = input_path::read(
            &mut db.conn,
            Within::OwnTransaction,
            &[],
            &[Added::new(&checked, false).array(true)],
        )
        .await;
        assert!(!admitted(&answer[0], "NULL array"));
        db.sql("ALTER TABLE app.rows ADD COLUMN d app.checked")
            .await;
        assert_eq!(
            number(&mut db.conn, "SELECT count(*)::integer AS n FROM app.calls").await,
            2,
            "PG {}",
            server.label
        );
        db.sql(
            "ALTER TABLE app.rows ADD COLUMN nc app.pair DEFAULT NULL;
                ALTER TABLE app.rows ADD COLUMN na app.checked[] DEFAULT NULL;
                ALTER TABLE app.rows ADD COLUMN nd app.checked DEFAULT NULL;",
        )
        .await;
        assert_eq!(
            number(&mut db.conn, "SELECT count(*)::integer FROM app.calls").await,
            4
        );
        assert_eq!(number(&mut db.conn, "SELECT count(*)::integer FROM pg_catalog.pg_attrdef d JOIN pg_catalog.pg_attribute a ON a.attrelid=d.adrelid AND a.attnum=d.adnum WHERE d.adrelid='app.rows'::regclass AND a.attname IN ('nc','na')").await, 0);
        assert_eq!(number(&mut db.conn, "SELECT count(*)::integer FROM pg_catalog.pg_attrdef d JOIN pg_catalog.pg_attribute a ON a.attrelid=d.adrelid AND a.attnum=d.adnum WHERE d.adrelid='app.rows'::regclass AND a.attname='nd'").await, 1);
        db.drop().await;
    }
}

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn constructed_defaults_check_composite_attributes_array_elements_and_range_bounds() {
    for server in servers() {
        let mut db = Scratch::create(&server, "fillparts").await;
        db.sql("CREATE SCHEMA app;
            CREATE TABLE app.calls (n integer);
            CREATE FUNCTION app.seen(integer) RETURNS boolean LANGUAGE plpgsql AS $$
                BEGIN INSERT INTO app.calls VALUES (1); RETURN true; END $$;
            CREATE DOMAIN app.checked AS integer CHECK (app.seen(VALUE));
            CREATE TYPE app.pair AS (v app.checked);
            CREATE TYPE app.span AS RANGE (subtype = app.checked);
            CREATE DOMAIN app.default_pair AS app.pair DEFAULT ROW(1)::app.pair;
            CREATE DOMAIN app.default_array AS app.checked[] DEFAULT ARRAY[1]::app.checked[];
            CREATE DOMAIN app.default_range AS app.span DEFAULT app.span(1::app.checked, 2::app.checked);
            CREATE TABLE app.rows (id integer);
            INSERT INTO app.rows VALUES (1), (2);").await;
        for (name, ty, default) in [
            ("c", "app.pair", "ROW(1)::app.pair"),
            ("a", "app.checked[]", "ARRAY[1]::app.checked[]"),
            ("r", "app.span", "app.span(1::app.checked, 2::app.checked)"),
            ("dc", "app.default_pair", ""),
            ("da", "app.default_array", ""),
            ("dr", "app.default_range", ""),
        ] {
            db.sql("TRUNCATE app.calls").await;
            let suffix = if default.is_empty() {
                String::new()
            } else {
                format!(" DEFAULT {default}")
            };
            db.sql(&format!(
                "ALTER TABLE app.rows ADD COLUMN {name} {ty}{suffix}"
            ))
            .await;
            assert!(
                number(&mut db.conn, "SELECT count(*)::integer AS n FROM app.calls").await > 0,
                "{name} on PG {} must evaluate the nested CHECK",
                server.label
            );
        }
        for (name, own_default, array) in [
            ("pair", true, false),
            ("checked", true, true),
            ("default_pair", false, false),
            ("default_array", false, false),
            ("default_range", false, false),
        ] {
            let ty = super::user_types_tests::present(&mut db.conn, "app", name).await;
            let answer = input_path::read(
                &mut db.conn,
                Within::OwnTransaction,
                &[],
                &[Added::new(&ty, own_default).array(array)],
            )
            .await;
            let all = refused(&answer[0], name);
            assert!(
                routines(all)
                    .iter()
                    .any(|r| r.identity == "app.seen(integer)"),
                "{name}: {all:?}"
            );
        }
        db.drop().await;
    }
}

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn constructing_two_composite_levels_evaluates_and_refuses_the_nested_untrusted_domain_check()
{
    for server in servers() {
        let mut db = Scratch::create(&server, "nestedfill").await;
        let (_, mut me) = deployer(&mut db).await;
        let stranger = db.role("stranger", "NOLOGIN").await;
        run(
            &mut me,
            server.label,
            "CREATE TABLE app.calls (n integer);
            CREATE FUNCTION app.seen(integer) RETURNS boolean LANGUAGE plpgsql AS $$
                BEGIN INSERT INTO app.calls VALUES (1); RETURN true; END $$;
            CREATE DOMAIN app.checked AS integer CHECK (app.seen(VALUE));
            CREATE TYPE app.inner_pair AS (v app.checked);
            CREATE TYPE app.outer_pair AS (v app.inner_pair);
            CREATE TABLE app.rows (id integer);
            INSERT INTO app.rows VALUES (1), (2);",
        )
        .await;
        db.sql(&format!(
            "GRANT USAGE ON SCHEMA app TO {stranger};
            ALTER FUNCTION app.seen(integer) OWNER TO {stranger}"
        ))
        .await;
        let ty = super::user_types_tests::present(&mut me, "app", "outer_pair").await;
        let answer = input_path::read(
            &mut me,
            Within::OwnTransaction,
            &[],
            &[Added::new(&ty, true)],
        )
        .await;
        assert!(
            routines(refused(&answer[0], server.label))
                .iter()
                .any(|r| r.identity == "app.seen(integer)"
                    && r.owner.raw() == stranger.as_str()
                    && r.why == Why::NotBuiltin)
        );
        assert_eq!(
            number(&mut me, "SELECT count(*)::integer FROM app.calls").await,
            0
        );
        // A NULL outer composite contains no inner value to construct.
        assert!(!admitted(
            &gate(&mut me, &[], "outer_pair").await,
            server.label
        ));
        run(
            &mut me,
            server.label,
            "ALTER TABLE app.rows ADD COLUMN v app.outer_pair
            DEFAULT ROW(ROW(1)::app.inner_pair)::app.outer_pair",
        )
        .await;
        assert!(
            number(&mut me, "SELECT count(*)::integer FROM app.calls").await > 0,
            "the constructed default runs the nested CHECK on PG {}",
            server.label
        );
        drop(me);
        db.drop().await;
    }
}

#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn stored_typed_composite_array_and_range_defaults_do_not_recheck_nested_domains() {
    for server in servers() {
        let mut db = Scratch::create(&server, "fillconstants").await;
        db.sql(
            "CREATE SCHEMA app;
            CREATE TABLE app.calls (n integer);
            CREATE FUNCTION app.seen(integer) RETURNS boolean LANGUAGE plpgsql AS $$
                BEGIN INSERT INTO app.calls VALUES (1); RETURN true; END $$;
            CREATE DOMAIN app.checked AS integer CHECK (app.seen(VALUE));
            CREATE TYPE app.pair AS (v app.checked);
            CREATE TYPE app.span AS RANGE (subtype = app.checked);
            CREATE TABLE app.rows (id integer);
            INSERT INTO app.rows VALUES (1), (2);",
        )
        .await;
        db.sql(
            "CREATE DOMAIN app.constant_array AS app.checked[] DEFAULT '{1}'::app.checked[];
                CREATE DOMAIN app.constant_pair AS app.pair DEFAULT '(1)'::app.pair;
                CREATE DOMAIN app.constant_range AS app.span DEFAULT '[1,2)'::app.span;
                TRUNCATE app.calls;
                ALTER TABLE app.rows ADD COLUMN ca app.constant_array;
                ALTER TABLE app.rows ADD COLUMN cp app.constant_pair;
                ALTER TABLE app.rows ADD COLUMN cr app.constant_range;",
        )
        .await;
        assert_eq!(
            number(&mut db.conn, "SELECT count(*)::integer AS n FROM app.calls").await,
            0,
            "stored constants on PG {}",
            server.label
        );
        for name in ["constant_array", "constant_pair", "constant_range"] {
            assert!(admitted(&gate(&mut db.conn, &[], name).await, name));
        }
        db.drop().await;
    }
}

#[tokio::test]
#[ignore = "needs live PostgreSQL 18; see scripts/live-tests-pg.sh"]
async fn io_coercions_execute_nested_checks_and_require_trust_on_postgres_18() {
    io_coercions_execute_nested_checks("18").await;
}

#[tokio::test]
#[ignore = "needs live PostgreSQL 16; see scripts/live-tests-pg.sh"]
async fn io_coercions_execute_nested_checks_and_require_trust_on_postgres_16() {
    io_coercions_execute_nested_checks("16").await;
}

async fn io_coercions_execute_nested_checks(major: &str) {
    let server = servers().into_iter().find(|s| s.label == major).unwrap();
    let mut db = Scratch::create(&server, "io_proof").await;
    let (_, mut me) = deployer(&mut db).await;
    let stranger = db.role("stranger", "NOLOGIN").await;
    run(
        &mut me,
        major,
        "CREATE TABLE app.calls (n integer);
         CREATE FUNCTION app.seen(integer) RETURNS boolean LANGUAGE plpgsql AS $$
           BEGIN INSERT INTO app.calls VALUES (1); RETURN true; END $$;
         CREATE DOMAIN app.checked AS integer CHECK (app.seen(VALUE));
         CREATE TYPE app.pair AS (v app.checked);
         CREATE TYPE app.nested_pair AS (v app.pair);
         CREATE TYPE app.span AS RANGE (subtype = app.checked);
         CREATE DOMAIN app.io_array AS text CHECK (VALUE::app.checked[] IS NOT NULL);
         CREATE DOMAIN app.io_pair AS text CHECK (VALUE::app.pair IS NOT NULL);
         CREATE DOMAIN app.io_nested AS text CHECK (VALUE::app.nested_pair IS NOT NULL);
         CREATE DOMAIN app.io_range AS text CHECK (VALUE::app.span IS NOT NULL);
         CREATE DOMAIN app.array_io AS text[] CHECK (VALUE::app.pair[] IS NOT NULL);
         CREATE DOMAIN app.reconstructed AS app.pair DEFAULT '(1)'::app.pair
           CHECK (VALUE::text::app.pair IS NOT NULL);
         CREATE DOMAIN app.row_build AS app.pair DEFAULT ROW(1::app.checked)::app.pair;
         CREATE DOMAIN app.array_build AS app.checked[] DEFAULT ARRAY[1::app.checked];
         CREATE DOMAIN app.relabel AS app.checked[] DEFAULT '{1}'::app.checked[]
           CHECK (VALUE::integer[] IS NOT NULL);
         CREATE DOMAIN app.subscript AS app.checked[] DEFAULT '{1}'::app.checked[]
           CHECK (VALUE[1] > 0);
         CREATE DOMAIN app.positive AS integer CHECK (VALUE > 0);
         CREATE DOMAIN app.io_builtin AS text CHECK (VALUE::app.positive[] IS NOT NULL);
         CREATE DOMAIN app.checked_json AS jsonb CHECK (app.seen(1));
         CREATE DOMAIN app.json_build AS integer
           CHECK (JSON_OBJECT('v': VALUE RETURNING app.checked_json) IS NOT NULL);
         TRUNCATE app.calls;",
    )
    .await;
    let mut errors = Vec::new();
    for (name, sql) in [
        ("io_array", "SELECT '{1}'::app.io_array"),
        ("io_pair", "SELECT '(1)'::app.io_pair"),
        ("io_nested", "SELECT '(\"(1)\")'::app.io_nested"),
        ("io_range", "SELECT '[1,2)'::app.io_range"),
        ("array_io", "SELECT ARRAY['(1)']::app.array_io"),
        ("reconstructed", "SELECT '(1)'::app.reconstructed"),
        ("json_build", "SELECT 1::app.json_build"),
        (
            "row_build",
            "CREATE TABLE app.r(v app.row_build); INSERT INTO app.r DEFAULT VALUES",
        ),
        (
            "array_build",
            "CREATE TABLE app.a(v app.array_build); INSERT INTO app.a DEFAULT VALUES",
        ),
    ] {
        let path = gate(&mut me, &[], name).await;
        if !matches!(&path, InputPath::Refused(reasons) if routines(reasons).iter().any(|r| r.identity == "app.seen(integer)"))
        {
            errors.push(format!("{name} did not find the nested CHECK: {path:?}"));
        }
        if number(&mut me, "SELECT count(*)::integer FROM app.calls").await != 0 {
            errors.push(format!("reading {name} executed code"));
        }
        run(&mut me, major, sql).await;
        if number(&mut me, "SELECT count(*)::integer FROM app.calls").await == 0 {
            errors.push(format!("{name} did not execute its nested CHECK"));
        }
        run(&mut me, major, "TRUNCATE app.calls").await;
    }
    // Already typed values stay typed: neither the array relabel nor fetching
    // an element re-runs its CHECK. Constructor coercions above do run it.
    for (name, sql) in [
        (
            "relabel",
            "CREATE TABLE app.rl(v app.relabel); INSERT INTO app.rl DEFAULT VALUES",
        ),
        (
            "subscript",
            "CREATE TABLE app.ss(v app.subscript); INSERT INTO app.ss DEFAULT VALUES",
        ),
    ] {
        let path = gate(&mut me, &[], name).await;
        if !matches!(path, InputPath::Admitted(_)) {
            errors.push(format!("{name} refused an already typed value: {path:?}"));
        }
        run(&mut me, major, sql).await;
        if number(&mut me, "SELECT count(*)::integer FROM app.calls").await != 0 {
            errors.push(format!("{name} rechecked an already typed value"));
        }
    }
    let path = gate(&mut me, &[], "io_builtin").await;
    if !matches!(path, InputPath::Admitted(_)) {
        errors.push(format!("builtin I/O input refused: {path:?}"));
    }
    db.sql(&format!(
        "GRANT USAGE ON SCHEMA app TO {stranger}; ALTER DOMAIN app.positive OWNER TO {stranger}"
    ))
    .await;
    let path = gate(&mut me, &[], "io_builtin").await;
    if !matches!(&path, InputPath::Refused(reasons) if owner_reasons(reasons).iter().any(|r| r.owner.raw() == stranger.as_str()))
    {
        errors.push(format!("nested I/O target owner not checked: {path:?}"));
    }
    let path = gate(&mut me, &[&stranger], "io_builtin").await;
    if !matches!(path, InputPath::Admitted(_)) {
        errors.push(format!("trusted I/O target owner refused: {path:?}"));
    }
    drop(me);
    db.drop().await;
    assert!(errors.is_empty(), "PG {major}: {errors:#?}");
}

#[tokio::test]
#[ignore = "needs live PostgreSQL 18; see scripts/live-tests-pg.sh"]
async fn json_return_coercion_runs_a_domain_check_and_is_unreadable_on_postgres_18() {
    let server = servers().into_iter().find(|s| s.label == "18").unwrap();
    let mut db = Scratch::create(&server, "json_proof").await;
    let (_, mut me) = deployer(&mut db).await;
    run(
        &mut me,
        "18",
        "CREATE TABLE app.calls(n integer);
         CREATE FUNCTION app.seen(integer) RETURNS boolean LANGUAGE plpgsql AS $$
           BEGIN INSERT INTO app.calls VALUES(1); RETURN true; END $$;
         CREATE DOMAIN app.checked AS integer CHECK (app.seen(VALUE));
         CREATE DOMAIN app.json_input AS jsonb
           CHECK (JSON_VALUE(VALUE, '$.v' RETURNING app.checked) IS NOT NULL);",
    )
    .await;
    let path = gate(&mut me, &[], "json_input").await;
    let before = number(&mut me, "SELECT count(*)::integer FROM app.calls").await;
    run(&mut me, "18", "SELECT '{\"v\":1}'::app.json_input").await;
    let after = number(&mut me, "SELECT count(*)::integer FROM app.calls").await;
    drop(me);
    db.drop().await;
    assert!(
        matches!(path, InputPath::Unreadable(Unreadable::Failed(ref why)) if why.contains("JSONEXPR")),
        "{path:?}"
    );
    assert_eq!((before, after), (0, 1));
}
