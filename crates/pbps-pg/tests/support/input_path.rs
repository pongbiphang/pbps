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
    self, InputPath, OwnerReason, Part, Reason, RoutineKind, RoutineReason, Unreadable, Why,
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
    let refs: Vec<&UserType> = types.iter().collect();
    let trusted: Vec<String> = trusted.iter().map(|t| (*t).to_owned()).collect();
    input_path::read(conn, Within::OwnTransaction, &trusted, &refs).await
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
            Reason::Owner(_) => None,
        })
        .collect()
}

fn owner_reasons(reasons: &[Reason]) -> Vec<&OwnerReason> {
    reasons
        .iter()
        .filter_map(|r| match r {
            Reason::Owner(o) => Some(o),
            Reason::Routine(_) => None,
        })
        .collect()
}

fn actors(reasons: &[Reason]) -> BTreeSet<String> {
    owner_reasons(reasons)
        .iter()
        .map(|o| o.actor.clone())
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
                admitted(&path, &format!("app.{name} on {label}")),
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
        let inside = input_path::read(&mut me, Within::CallersTransaction, &[], &[short]).await;
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
                // A DEFAULT runs only for the declared type: a domain over the
                // default's domain has a copy of it, and a composite, an array
                // or a range holding that domain does not run it at all.
                if case.default && wrapper != n && wrapper != &format!("nest_{n}") {
                    assert!(
                        !admitted(&path, &what),
                        "{what}: the nested DEFAULT is not evaluated"
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
                        assert_eq!(hit.owner, me_name, "{what}");
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
        assert_eq!(owners[0].owner, stranger);
        assert_eq!(owners[0].actor, stranger);
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
                owner: stranger.clone(),
                actor: stranger.clone(),
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
                InputPath::Unreadable(Unreadable::TrustedRolesMissing(vec![ghost.to_owned()])),
                "{name} on {label}"
            );
            assert!(!path.is_admitted());
        }
        let missing = input_path::missing_roles(
            &mut me,
            &[
                stranger.clone(),
                ghost.to_owned(),
                ghost.to_owned(),
                String::new(),
            ],
        )
        .await
        .unwrap();
        assert_eq!(missing, vec![String::new(), ghost.to_owned()]);
        assert!(
            input_path::missing_roles(&mut me, &[stranger.clone(), listed.clone()])
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
        assert!(owner_reasons(all).iter().all(|o| o.owner == pal));
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
        for name in ["mine", "pair"] {
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

/// A type whose input path runs no code is admitted whoever owns it, and
/// whatever its leaves are owned by: nothing is evaluated.
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn a_type_that_runs_no_code_is_admitted_whoever_owns_it() {
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
            assert!(
                !admitted(&path, &format!("{name} on {label}")),
                "{name} runs no code"
            );
        }
        // Negative: the moment a CHECK is on the path, the stranger counts.
        db.sql(&format!(
            "CREATE DOMAIN app.checked AS integer CHECK (VALUE > 0);
             ALTER DOMAIN app.checked OWNER TO {stranger};
             CREATE TYPE app.holds_checked AS (c app.checked, m app.mood);
             ALTER TYPE app.holds_checked OWNER TO {stranger};
             CREATE DOMAIN app.defaulted AS integer DEFAULT 5;
             ALTER DOMAIN app.defaulted OWNER TO {stranger};"
        ))
        .await;
        for name in ["checked", "holds_checked", "defaulted"] {
            let path = gate(&mut me, &[], name).await;
            let all = refused(&path, name);
            assert!(
                actors(all).contains(&stranger),
                "{name} on {label}: {all:?}"
            );
        }
        // An enum on the path of a type that does run code is checked like the
        // rest: its owner is not asked about only while nothing runs.
        run(
            &mut me,
            label,
            "CREATE DOMAIN app.positive AS integer CHECK (VALUE > 0);
             CREATE TYPE app.positive_mood AS (p app.positive, m app.mood);",
        )
        .await;
        let path = gate(&mut me, &[], "positive_mood").await;
        let all = refused(&path, "positive_mood");
        assert_eq!(
            owner_reasons(all)
                .iter()
                .map(|o| o.ty.as_str())
                .collect::<Vec<_>>(),
            ["app.mood"],
            "on {label}"
        );
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
        let refs: Vec<&UserType> = types.iter().collect();
        let paths = input_path::read(&mut me, Within::OwnTransaction, &[], &refs).await;
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
        let aborted = input_path::read(&mut me, Within::CallersTransaction, &[], &refs[1..]).await;
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
            ("holds", vec!["app.boom(integer)"]),
        ] {
            let path = gate(&mut me, &[], name).await;
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
