//! The connected read of a referenced type and its pin (#1523, plan step 2),
//! on PostgreSQL 16 and 18.
//!
//! Every test runs the same fixture on both servers, in a database of its own
//! that the test creates and drops, and with the roles a permission question
//! needs: `postgres` is a superuser, and a superuser passes every privilege
//! check without asking the catalog.

use std::collections::BTreeSet;

use super::*;
use pbps_model::TypeRef;
use pbps_pg::pins::Within;
use pbps_pg::user_types::{
    self, Body, Component, Definition, Kind, NotAdmitted, TypeRead, Unreadable, UserType,
};

/// One server of the pair, as the libpq keyword string that reaches it.
struct Server {
    label: &'static str,
    connection: String,
}

fn servers() -> [Server; 2] {
    let old = std::env::var("PBPS_TEST_PG_OLD_DB").expect("the PostgreSQL 16 fixture");
    [
        Server {
            label: "18",
            connection: conn_str(),
        },
        Server {
            label: "16",
            connection: old,
        },
    ]
}

/// `connection` with some of its keywords replaced. Keyword form only: a URL
/// is not rewritten, because guessing at which part names the database is how
/// a suite that drops databases reaches the wrong one.
fn with(connection: &str, overrides: &[(&str, &str)]) -> String {
    assert!(!connection.contains("://"), "{connection}");
    let mut parts: Vec<String> = connection
        .split_whitespace()
        .filter(|p| {
            !overrides
                .iter()
                .any(|(key, _)| p.starts_with(&format!("{key}=")))
        })
        .map(str::to_owned)
        .collect();
    parts.extend(overrides.iter().map(|(k, v)| format!("{k}={v}")));
    parts.join(" ")
}

/// A database of its own on one server, and the roles made in the cluster for
/// it. Dropped explicitly: a role outlives its database.
struct Scratch {
    label: &'static str,
    tag: String,
    server: String,
    name: String,
    conn: Conn,
    roles: Vec<String>,
}

impl Scratch {
    async fn create(server: &Server, tag: &str) -> Scratch {
        let name = format!("pbps_t1523b_{tag}_{}_{}", server.label, std::process::id());
        let mut admin = Conn::connect(Driver::Postgres, &server.connection)
            .await
            .expect("connect to the server");
        admin
            .execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
            .await
            .expect("clear a database left by an earlier run");
        admin
            .execute(&format!("CREATE DATABASE {name}"))
            .await
            .expect("create the scratch database");
        let conn = Conn::connect(
            Driver::Postgres,
            &with(&server.connection, &[("dbname", &name)]),
        )
        .await
        .expect("connect to the scratch database");
        Scratch {
            label: server.label,
            tag: tag.to_owned(),
            server: server.connection.clone(),
            name,
            conn,
            roles: Vec::new(),
        }
    }

    /// Runs statements as the superuser that made the database.
    async fn sql(&mut self, sql: &str) {
        self.conn
            .execute(sql)
            .await
            .unwrap_or_else(|e| panic!("on {}: `{sql}`: {e}", self.label));
    }

    /// A role of this test's own, with `options` (`LOGIN`, `NOLOGIN`).
    async fn role(&mut self, tag: &str, options: &str) -> String {
        // The test's own tag too: two tests run at once, and a role is shared
        // by the whole cluster.
        let role = format!(
            "pbps_t1523b_{}_{tag}_{}_{}",
            self.tag,
            self.label,
            std::process::id()
        );
        // A role left by an earlier run holds nothing here: its database is
        // gone, and dropping a role that still owns something elsewhere fails
        // loudly in the create that follows.
        let _ = self
            .conn
            .execute(&format!("DROP ROLE IF EXISTS {role}"))
            .await;
        self.sql(&format!(
            "CREATE ROLE {role} {options} PASSWORD 'live-test'"
        ))
        .await;
        self.roles.push(role.clone());
        role
    }

    async fn as_role(&self, role: &str) -> Conn {
        let connection = with(
            &self.server,
            &[
                ("dbname", &self.name),
                ("user", role),
                ("password", "live-test"),
            ],
        );
        Conn::connect(Driver::Postgres, &connection)
            .await
            .unwrap_or_else(|e| panic!("connect as `{role}` on {}: {e}", self.label))
    }

    async fn drop(self) {
        let Scratch {
            server,
            name,
            conn,
            roles,
            ..
        } = self;
        std::mem::drop(conn);
        let mut admin = Conn::connect(Driver::Postgres, &server)
            .await
            .expect("connect to the server");
        let _ = admin
            .execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
            .await;
        for role in roles {
            let _ = admin.execute(&format!("DROP ROLE IF EXISTS {role}")).await;
        }
    }
}

fn reference(schema: &str, name: &str) -> TypeRef {
    TypeRef::new(schema, name).unwrap_or_else(|e| panic!("{schema}.{name}: {e}"))
}

/// One reference, read in a transaction of the read's own.
async fn read_as(conn: &mut Conn, schema: &str, name: &str) -> TypeRead {
    let mut reads =
        user_types::read(conn, Within::OwnTransaction, &[reference(schema, name)]).await;
    assert_eq!(reads.len(), 1);
    reads.remove(0)
}

async fn present(conn: &mut Conn, schema: &str, name: &str) -> UserType {
    let read = read_as(conn, schema, name).await;
    let TypeRead::Present(t) = read else {
        panic!("{schema}.{name} should be present: {read:?}")
    };
    *t
}

async fn pin_of(conn: &mut Conn, schema: &str, name: &str) -> String {
    present(conn, schema, name).await.pin().to_string()
}

/// The refusal text of a read that is not a present type.
async fn refusal_of(conn: &mut Conn, schema: &str, name: &str) -> (TypeRead, String) {
    let read = read_as(conn, schema, name).await;
    let message = read
        .refusal(&reference(schema, name))
        .unwrap_or_else(|| panic!("{schema}.{name} should be refused: {read:?}"));
    (read, message)
}

/// A definition with every OID taken out, so that the same declarations on
/// two servers (whose OIDs differ) can be compared.
fn without_oids(def: &Definition) -> Definition {
    fn component(c: &Component) -> Component {
        match c {
            Component::Array(e) => Component::Array(Box::new(component(e))),
            Component::Defined(d) => Component::Defined(Box::new(without_oids(d))),
            Component::Range {
                schema,
                name,
                typtype,
                subtype,
                ..
            } => Component::Range {
                oid: 0,
                owner: 0,
                schema: schema.clone(),
                name: name.clone(),
                typtype: typtype.clone(),
                subtype: Box::new(component(subtype)),
            },
            Component::Other {
                schema,
                name,
                typtype,
                ..
            } => Component::Other {
                oid: 0,
                owner: 0,
                schema: schema.clone(),
                name: name.clone(),
                typtype: typtype.clone(),
            },
        }
    }
    let mut out = def.clone();
    out.oid = 0;
    out.owner = 0;
    match &mut out.body {
        Body::Enum { .. } => {}
        Body::Domain(d) => {
            d.base = component(&d.base);
            d.collation = if d.collation == 0 { 0 } else { 1 };
        }
        Body::Composite(c) => {
            for a in &mut c.attributes {
                a.ty = component(&a.ty);
                a.collation = if a.collation == 0 { 0 } else { 1 };
            }
        }
    }
    out
}

/// Runs one statement and asserts the pin of `schema.name` is one this
/// sequence of changes has not produced yet.
async fn step(
    db: &mut Scratch,
    seen: &mut BTreeSet<String>,
    label: &str,
    schema: &str,
    name: &str,
    sql: &str,
) {
    db.sql(sql).await;
    let pin = pin_of(&mut db.conn, schema, name).await;
    assert!(
        seen.insert(pin),
        "`{sql}` left the pin of {schema}.{name} on {label} as it was"
    );
}

/// An enum, a domain and a composite are read with their kind and their pin,
/// and what is read is the same on 16 and 18 apart from OIDs (#1523). The
/// definition holds the catalog's own text, so it is the pin's input as well.
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn an_enum_a_domain_and_a_composite_are_read_with_their_kind_and_their_pin() {
    let mut shapes: Vec<(&str, Vec<Definition>)> = Vec::new();
    for server in servers() {
        let mut db = Scratch::create(&server, "kinds").await;
        db.sql(
            "CREATE SCHEMA app;
             CREATE TYPE app.mood AS ENUM ('sad', 'ok', 'happy');
             CREATE TYPE app.nothing AS ENUM ();
             CREATE DOMAIN app.positive AS integer
                 DEFAULT 1 NOT NULL CONSTRAINT positive_check CHECK (VALUE > 0);
             ALTER DOMAIN app.positive ADD CONSTRAINT small CHECK (VALUE < 100) NOT VALID;
             CREATE DOMAIN app.label AS varchar(10) COLLATE \"C\";
             CREATE TYPE app.pair AS (a app.positive, b text[], c app.mood, d numeric(8, 2));
             CREATE TYPE app.bare AS ();",
        )
        .await;

        let mood = present(&mut db.conn, "app", "mood").await;
        assert_eq!(mood.kind(), Kind::Enum);
        let Body::Enum { labels } = &mood.definition().body else {
            panic!("{:?}", mood.definition())
        };
        // Declaration order, not name order: the sort order is the type's.
        assert_eq!(labels, &["sad", "ok", "happy"]);
        // An enum with no labels is present, and is not empty in any sense
        // that makes it absent.
        let nothing = present(&mut db.conn, "app", "nothing").await;
        assert_eq!(nothing.kind(), Kind::Enum);
        assert!(matches!(
            &nothing.definition().body,
            Body::Enum { labels } if labels.is_empty()
        ));

        let positive = present(&mut db.conn, "app", "positive").await;
        assert_eq!(positive.kind(), Kind::Domain);
        let Body::Domain(d) = &positive.definition().body else {
            panic!("{:?}", positive.definition())
        };
        assert!(d.not_null, "{d:?}");
        assert_eq!(d.default.as_deref(), Some("1"));
        assert_eq!(d.typmod, -1);
        // The `NOT NULL` that 18 also lists as a constraint row is not one of
        // the constraints on either release.
        let constraints: Vec<(&str, bool, &str)> = d
            .constraints
            .iter()
            .map(|c| (c.name.as_str(), c.validated, c.definition.as_str()))
            .collect();
        assert_eq!(
            constraints,
            [
                ("positive_check", true, "CHECK ((VALUE > 0))"),
                ("small", false, "CHECK ((VALUE < 100)) NOT VALID"),
            ],
            "on {}",
            server.label
        );
        let Component::Other { schema, name, .. } = &d.base else {
            panic!("{:?}", d.base)
        };
        assert_eq!((schema.as_str(), name.as_str()), ("pg_catalog", "int4"));

        let label = present(&mut db.conn, "app", "label").await;
        let Body::Domain(l) = &label.definition().body else {
            panic!("{:?}", label.definition())
        };
        assert_eq!(l.typmod, 14, "varchar(10) is a typmod of 10 + 4");
        assert_ne!(l.collation, 0, "a domain with a collation keeps it");
        assert!(!l.not_null && l.default.is_none() && l.constraints.is_empty());

        let pair = present(&mut db.conn, "app", "pair").await;
        assert_eq!(pair.kind(), Kind::Composite);
        let Body::Composite(c) = &pair.definition().body else {
            panic!("{:?}", pair.definition())
        };
        assert_eq!(c.relkind, "c");
        let names: Vec<&str> = c.attributes.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c", "d"]);
        // The attribute that is a domain is read in full, so a change to it
        // is a change to the composite; an array of text is an array of a
        // built-in; an enum is read in full as well.
        assert!(matches!(&c.attributes[0].ty, Component::Defined(d) if d.name == "positive"));
        assert!(matches!(&c.attributes[1].ty, Component::Array(e)
            if matches!(&**e, Component::Other { name, .. } if name == "text")));
        assert!(matches!(&c.attributes[2].ty, Component::Defined(d) if d.name == "mood"));
        assert_eq!(c.attributes[3].typmod, (8 << 16) + 2 + 4);
        assert!(matches!(
            &present(&mut db.conn, "app", "bare").await.definition().body,
            Body::Composite(c) if c.attributes.is_empty()
        ));

        // Everything read has a pin of 64 hexadecimal digits, and a pin per
        // type: no two of these share one.
        let pins: BTreeSet<String> = [&mood, &nothing, &positive, &label, &pair]
            .iter()
            .map(|t| t.pin().to_string())
            .collect();
        assert_eq!(pins.len(), 5, "{pins:?}");
        for pin in &pins {
            assert!(
                pin.len() == 64 && pin.bytes().all(|b| b.is_ascii_hexdigit()),
                "{pin}"
            );
        }
        // Reading again, in the same state, gives the same pin.
        assert_eq!(
            pin_of(&mut db.conn, "app", "pair").await,
            pair.pin().as_str()
        );

        shapes.push((
            server.label,
            [mood, nothing, positive, label, pair]
                .iter()
                .map(|t| without_oids(t.definition()))
                .collect(),
        ));
        db.drop().await;
    }
    // The same declarations read the same on both servers.
    assert_eq!(shapes[0].1, shapes[1].1);
}

/// Absent, a shell type, and a type the role cannot use are three results with
/// three messages; a read that fails is a fourth, and is never an absence
/// (#1523). The lookup does not cast: without `USAGE` on the schema
/// `to_regtype` raises, and the `pg_type` row is still there to find.
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn absent_shell_and_unusable_types_are_three_different_results() {
    for server in servers() {
        let mut db = Scratch::create(&server, "outcomes").await;
        let role = db.role("deployer", "LOGIN").await;
        db.sql(&format!(
            "CREATE SCHEMA open; CREATE SCHEMA closed;
             GRANT USAGE ON SCHEMA open TO {role};
             CREATE TYPE open.usable AS ENUM ('a');
             CREATE TYPE open.shell;
             CREATE TYPE open.no_type_usage AS ENUM ('a');
             REVOKE USAGE ON TYPE open.no_type_usage FROM PUBLIC;
             CREATE TYPE closed.hidden AS ENUM ('a');"
        ))
        .await;

        // The engine's own answers, as the role: the casts raise, and that
        // is what a lookup by cast would have taken for something else.
        let mut me = db.as_role(&role).await;
        let cast = me
            .execute("SELECT to_regtype('closed.hidden')")
            .await
            .expect_err("to_regtype raises without USAGE on the schema");
        assert_eq!(
            cast.server_error_code().as_deref(),
            Some("42501"),
            "on {}",
            server.label
        );
        let readable = me
            .query(
                "SELECT t.typname::text AS name FROM pg_type t
                   JOIN pg_namespace n ON n.oid = t.typnamespace
                  WHERE n.nspname = 'closed' AND t.typname = 'hidden'",
            )
            .await
            .expect("the pg_type row stays readable");
        assert_eq!(readable.len(), 1);

        let (absent, absent_text) = refusal_of(&mut me, "open", "nothing_here").await;
        let (shell, shell_text) = refusal_of(&mut me, "open", "shell").await;
        let (no_schema, no_schema_text) = refusal_of(&mut me, "closed", "hidden").await;
        let (no_type, no_type_text) = refusal_of(&mut me, "open", "no_type_usage").await;
        assert_eq!(absent, TypeRead::Absent);
        // A schema that does not exist is an absence too, and says so: it is
        // not the "permission denied" a cast to it would raise.
        assert_eq!(
            read_as(&mut me, "no_such_schema", "mood").await,
            TypeRead::Absent
        );
        assert_eq!(shell, TypeRead::NotDefined);
        assert_eq!(
            no_schema,
            TypeRead::Unreadable(Unreadable::SchemaUsage {
                schema: "closed".to_owned()
            })
        );
        assert_eq!(no_type, TypeRead::Unreadable(Unreadable::TypeUsage));
        let texts = [&absent_text, &shell_text, &no_schema_text, &no_type_text];
        let distinct: BTreeSet<&&String> = texts.iter().collect();
        assert_eq!(distinct.len(), 4, "{texts:#?}");
        assert!(
            no_schema_text.contains("USAGE on schema `closed`"),
            "{no_schema_text}"
        );
        assert!(no_type_text.contains("USAGE on the type"), "{no_type_text}");
        assert!(shell_text.contains("shell"), "{shell_text}");
        assert!(absent_text.contains("does not exist"), "{absent_text}");
        // Negative: a type the role can use is present, as the same role.
        assert!(matches!(
            read_as(&mut me, "open", "usable").await,
            TypeRead::Present(_)
        ));
        // And granting what was missing makes the type readable, and nothing
        // more: the pin is the same before and after, since a grant is not a
        // change to the type.
        let before = {
            let mut su = Conn::connect(
                Driver::Postgres,
                &with(&server.connection, &[("dbname", &db.name)]),
            )
            .await
            .unwrap();
            pin_of(&mut su, "closed", "hidden").await
        };
        db.sql(&format!("GRANT USAGE ON SCHEMA closed TO {role}"))
            .await;
        assert_eq!(pin_of(&mut me, "closed", "hidden").await, before);

        std::mem::drop(me);
        db.drop().await;
    }
}

/// A read that fails is `Unreadable::Failed` and never `Absent`: an aborted
/// transaction of the caller's stands for any failure, and the type it asks
/// for is there (#1523).
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn a_read_that_fails_is_unreadable_and_never_absent() {
    for server in servers() {
        let mut db = Scratch::create(&server, "failed").await;
        db.sql("CREATE SCHEMA app; CREATE TYPE app.mood AS ENUM ('a');")
            .await;
        db.sql("BEGIN").await;
        assert!(db.conn.execute("SELECT 1 / 0").await.is_err());
        let reads = user_types::read(
            &mut db.conn,
            Within::CallersTransaction,
            &[reference("app", "mood"), reference("app", "missing")],
        )
        .await;
        db.sql("ROLLBACK").await;
        assert_eq!(reads.len(), 2);
        for read in &reads {
            assert!(
                matches!(read, TypeRead::Unreadable(Unreadable::Failed(_))),
                "{read:?}"
            );
            assert_ne!(read, &TypeRead::Absent);
        }
        // The same references in a healthy transaction: one present, one
        // absent, and the transaction is left as it was found.
        db.sql("BEGIN").await;
        db.sql("SET LOCAL search_path = public").await;
        let reads = user_types::read(
            &mut db.conn,
            Within::CallersTransaction,
            &[reference("app", "mood"), reference("app", "missing")],
        )
        .await;
        let path = db.conn.query("SHOW search_path").await.unwrap();
        assert_eq!(path[0].try_get_at::<&str>(0).unwrap(), Some("public"));
        db.sql("ROLLBACK").await;
        assert!(matches!(&reads[0], TypeRead::Present(_)), "{reads:?}");
        assert_eq!(reads[1], TypeRead::Absent);
        db.drop().await;
    }
}

/// Types that exist and are not an enum, a domain or a standalone composite
/// are refused by name, each for what it is (#1523).
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn other_kinds_of_type_are_refused_by_name() {
    for server in servers() {
        let mut db = Scratch::create(&server, "refused").await;
        db.sql(
            "CREATE EXTENSION citext SCHEMA public;
             CREATE SCHEMA app;
             CREATE TYPE app.span AS RANGE (subtype = integer);
             CREATE TABLE app.t (id integer);
             CREATE TABLE app.p (id integer) PARTITION BY LIST (id);
             CREATE VIEW app.v AS SELECT 1 AS x;
             CREATE MATERIALIZED VIEW app.m AS SELECT 1 AS x;
             CREATE FOREIGN DATA WRAPPER w;
             CREATE SERVER s FOREIGN DATA WRAPPER w;
             CREATE FOREIGN TABLE app.f (id integer) SERVER s;
             CREATE SEQUENCE app.q;",
        )
        .await;
        let label = server.label;
        let cases: Vec<(&str, &str, NotAdmitted)> = vec![
            (
                "public",
                "citext",
                NotAdmitted::Base {
                    extension: Some("citext".to_owned()),
                },
            ),
            ("pg_catalog", "int4", NotAdmitted::Base { extension: None }),
            ("pg_catalog", "text", NotAdmitted::Base { extension: None }),
            // Array category and an element type, and not an array.
            (
                "pg_catalog",
                "int2vector",
                NotAdmitted::Base { extension: None },
            ),
            (
                "pg_catalog",
                "oidvector",
                NotAdmitted::Base { extension: None },
            ),
            ("app", "span", NotAdmitted::Range),
            ("pg_catalog", "int4range", NotAdmitted::Range),
            ("app", "span_multirange", NotAdmitted::Multirange),
            ("pg_catalog", "record", NotAdmitted::Pseudo),
            ("pg_catalog", "anyelement", NotAdmitted::Pseudo),
            (
                "app",
                "t",
                NotAdmitted::RowType {
                    relkind: "r".to_owned(),
                },
            ),
            (
                "app",
                "p",
                NotAdmitted::RowType {
                    relkind: "p".to_owned(),
                },
            ),
            (
                "app",
                "v",
                NotAdmitted::RowType {
                    relkind: "v".to_owned(),
                },
            ),
            (
                "app",
                "m",
                NotAdmitted::RowType {
                    relkind: "m".to_owned(),
                },
            ),
            (
                "app",
                "f",
                NotAdmitted::RowType {
                    relkind: "f".to_owned(),
                },
            ),
        ];
        let mut texts = BTreeSet::new();
        for (schema, name, expected) in cases {
            let (read, text) = refusal_of(&mut db.conn, schema, name).await;
            assert_eq!(
                read,
                TypeRead::NotAdmitted(expected),
                "{schema}.{name} on {label}"
            );
            assert!(text.contains(&format!("{schema}.{name}")), "{text}");
            texts.insert(text);
        }
        assert_eq!(
            texts.len(),
            15,
            "every refusal names its own kind: {texts:#?}"
        );
        // A sequence has no row type: nothing by its name is found, which is
        // an absence and not one of the refusals above.
        assert_eq!(read_as(&mut db.conn, "app", "q").await, TypeRead::Absent);
        // Negative: the same fixture's admitted kinds are not refused.
        db.sql(
            "CREATE TYPE app.c AS (a integer);
             CREATE DOMAIN app.d AS integer;
             CREATE TYPE app.e AS ENUM ('x');",
        )
        .await;
        for name in ["c", "d", "e"] {
            assert!(
                matches!(
                    read_as(&mut db.conn, "app", name).await,
                    TypeRead::Present(_)
                ),
                "{name}"
            );
        }
        db.drop().await;
    }
}

/// Each change to a type changes its pin, a no-op does not, and the pin does
/// not depend on anything a change leaves alone (#1523, decision 4).
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn each_change_to_a_type_changes_its_pin_and_a_no_op_does_not() {
    for server in servers() {
        let mut db = Scratch::create(&server, "pins").await;
        let other = db.role("other", "NOLOGIN").await;
        db.sql(
            "CREATE SCHEMA app;
             CREATE TYPE app.mood AS ENUM ('sad', 'happy');
             CREATE DOMAIN app.positive AS integer CHECK (VALUE > 0);
             CREATE TYPE app.pair AS (a app.positive, b text);",
        )
        .await;
        let label = server.label;

        // Steps that must each give a pin never seen before on this type.
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let initial = pin_of(&mut db.conn, "app", "mood").await;
        seen.insert(initial.clone());

        // -- no-ops: none of these is a change to the type.
        for sql in [
            "COMMENT ON TYPE app.mood IS 'how it feels'",
            "GRANT USAGE ON TYPE app.mood TO PUBLIC",
            "REVOKE USAGE ON TYPE app.mood FROM PUBLIC",
            "ALTER TYPE app.mood OWNER TO CURRENT_USER",
        ] {
            db.sql(sql).await;
            assert_eq!(
                pin_of(&mut db.conn, "app", "mood").await,
                initial,
                "`{sql}` moved the pin on {label}"
            );
        }
        db.sql("GRANT USAGE ON TYPE app.mood TO PUBLIC").await;

        // -- an enum.
        step(
            &mut db,
            &mut seen,
            label,
            "app",
            "mood",
            "ALTER TYPE app.mood ADD VALUE 'ok' BEFORE 'happy'",
        )
        .await;
        step(
            &mut db,
            &mut seen,
            label,
            "app",
            "mood",
            "ALTER TYPE app.mood ADD VALUE 'z'",
        )
        .await;
        step(
            &mut db,
            &mut seen,
            label,
            "app",
            "mood",
            "ALTER TYPE app.mood RENAME VALUE 'z' TO 'zed'",
        )
        .await;
        step(
            &mut db,
            &mut seen,
            label,
            "app",
            "mood",
            &format!("ALTER TYPE app.mood OWNER TO {other}"),
        )
        .await;
        let oid = present(&mut db.conn, "app", "mood").await.oid();
        // A rename is a change: the old name finds nothing, the new name finds
        // the same OID with another pin. (The replay finds the type absent.)
        let before_rename = pin_of(&mut db.conn, "app", "mood").await;
        db.sql("ALTER TYPE app.mood RENAME TO mood2").await;
        assert_eq!(read_as(&mut db.conn, "app", "mood").await, TypeRead::Absent);
        let renamed = present(&mut db.conn, "app", "mood2").await;
        assert_eq!(renamed.oid(), oid);
        assert_ne!(renamed.pin().as_str(), before_rename, "a rename on {label}");
        db.sql("ALTER TYPE app.mood2 SET SCHEMA public").await;
        assert_ne!(
            pin_of(&mut db.conn, "public", "mood2").await,
            renamed.pin().as_str(),
            "a move to another schema on {label}"
        );
        // A type dropped and made again with the same labels is another type.
        db.sql("CREATE TYPE app.fresh AS ENUM ('a')").await;
        let first = present(&mut db.conn, "app", "fresh").await;
        db.sql("DROP TYPE app.fresh").await;
        db.sql("CREATE TYPE app.fresh AS ENUM ('a')").await;
        let second = present(&mut db.conn, "app", "fresh").await;
        assert_ne!(first.oid(), second.oid());
        assert_ne!(first.pin(), second.pin(), "the same labels, another type");

        // -- a domain.
        seen.clear();
        seen.insert(pin_of(&mut db.conn, "app", "positive").await);
        let d = ("app", "positive");
        step(
            &mut db,
            &mut seen,
            label,
            d.0,
            d.1,
            "ALTER DOMAIN app.positive ADD CONSTRAINT small CHECK (VALUE < 100) NOT VALID",
        )
        .await;
        let not_valid = pin_of(&mut db.conn, d.0, d.1).await;
        step(
            &mut db,
            &mut seen,
            label,
            d.0,
            d.1,
            "ALTER DOMAIN app.positive VALIDATE CONSTRAINT small",
        )
        .await;
        step(
            &mut db,
            &mut seen,
            label,
            d.0,
            d.1,
            "ALTER DOMAIN app.positive RENAME CONSTRAINT small TO tiny",
        )
        .await;
        step(
            &mut db,
            &mut seen,
            label,
            d.0,
            d.1,
            "ALTER DOMAIN app.positive SET DEFAULT 5",
        )
        .await;
        step(
            &mut db,
            &mut seen,
            label,
            d.0,
            d.1,
            "ALTER DOMAIN app.positive SET DEFAULT 6",
        )
        .await;
        step(
            &mut db,
            &mut seen,
            label,
            d.0,
            d.1,
            "ALTER DOMAIN app.positive SET NOT NULL",
        )
        .await;
        step(
            &mut db,
            &mut seen,
            label,
            d.0,
            d.1,
            &format!("ALTER DOMAIN app.positive OWNER TO {other}"),
        )
        .await;
        step(
            &mut db,
            &mut seen,
            label,
            d.0,
            d.1,
            "ALTER DOMAIN app.positive DROP CONSTRAINT tiny",
        )
        .await;
        // Dropping and adding back the same constraint gives the pin it had:
        // the constraint's OID is not part of the definition.
        let with_check = pin_of(&mut db.conn, d.0, d.1).await;
        db.sql("ALTER DOMAIN app.positive DROP CONSTRAINT positive_check")
            .await;
        assert_ne!(pin_of(&mut db.conn, d.0, d.1).await, with_check);
        db.sql("ALTER DOMAIN app.positive ADD CONSTRAINT positive_check CHECK (VALUE > 0)")
            .await;
        assert_eq!(
            pin_of(&mut db.conn, d.0, d.1).await,
            with_check,
            "on {label}"
        );
        assert_ne!(not_valid, with_check);

        // -- a composite, and a change inside the domain it holds.
        seen.clear();
        let pair = ("app", "pair");
        seen.insert(pin_of(&mut db.conn, pair.0, pair.1).await);
        step(
            &mut db,
            &mut seen,
            label,
            pair.0,
            pair.1,
            "ALTER DOMAIN app.positive ADD CONSTRAINT even CHECK (VALUE % 2 = 0)",
        )
        .await;
        let after_domain = pin_of(&mut db.conn, pair.0, pair.1).await;
        step(
            &mut db,
            &mut seen,
            label,
            pair.0,
            pair.1,
            "ALTER TYPE app.pair ADD ATTRIBUTE c integer",
        )
        .await;
        step(
            &mut db,
            &mut seen,
            label,
            pair.0,
            pair.1,
            "ALTER TYPE app.pair RENAME ATTRIBUTE c TO cc",
        )
        .await;
        step(
            &mut db,
            &mut seen,
            label,
            pair.0,
            pair.1,
            "ALTER TYPE app.pair ALTER ATTRIBUTE cc TYPE bigint",
        )
        .await;
        // Dropping the attribute again gives back the definition the composite
        // had after the domain changed, and so that pin: a dropped column
        // leaves nothing of itself in it.
        db.sql("ALTER TYPE app.pair DROP ATTRIBUTE cc").await;
        let dropped = pin_of(&mut db.conn, pair.0, pair.1).await;
        assert_eq!(dropped, after_domain, "on {label}");
        step(
            &mut db,
            &mut seen,
            label,
            pair.0,
            pair.1,
            "ALTER TYPE app.pair ADD ATTRIBUTE e varchar(5) COLLATE \"C\"",
        )
        .await;
        step(
            &mut db,
            &mut seen,
            label,
            pair.0,
            pair.1,
            &format!("ALTER TYPE app.pair OWNER TO {other}"),
        )
        .await;
        db.drop().await;
    }
}

/// Reading a type runs none of its code: a domain whose CHECK and DEFAULT call
/// a function that raises is read as the text of the catalog, and the read
/// does not cast to it (DEC-1663.1).
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn reading_a_type_runs_none_of_its_code() {
    for server in servers() {
        let mut db = Scratch::create(&server, "inert").await;
        db.sql(
            "CREATE SCHEMA app;
             CREATE FUNCTION app.boom(integer) RETURNS boolean LANGUAGE plpgsql
                 AS $$ BEGIN RAISE EXCEPTION 'boom ran'; END $$;
             CREATE FUNCTION app.boom_default() RETURNS integer LANGUAGE plpgsql
                 AS $$ BEGIN RAISE EXCEPTION 'boom default ran'; END $$;
             CREATE DOMAIN app.trap AS integer DEFAULT app.boom_default() CHECK (app.boom(VALUE));
             CREATE TYPE app.holds AS (t app.trap);",
        )
        .await;
        for name in ["trap", "holds"] {
            let read = present(&mut db.conn, "app", name).await;
            let domain = if let Body::Composite(c) = &read.definition().body {
                let Component::Defined(inner) = &c.attributes[0].ty else {
                    panic!("{:?}", c.attributes[0])
                };
                &**inner
            } else {
                read.definition()
            };
            let Body::Domain(domain) = &domain.body else {
                panic!("{domain:?}")
            };
            assert_eq!(domain.default.as_deref(), Some("app.boom_default()"));
            assert_eq!(domain.constraints[0].definition, "CHECK (app.boom(VALUE))");
            // Both functions are what the closure reaches, by identity.
            let identities: Vec<&str> =
                read.reached().iter().map(|r| r.identity.as_str()).collect();
            assert!(identities.contains(&"app.boom(integer)"), "{identities:?}");
            assert!(identities.contains(&"app.boom_default()"), "{identities:?}");
        }
        db.drop().await;
    }
}

/// The closure names what a type reaches through `pg_depend`: the functions of
/// a CHECK and a DEFAULT, a function's own tables, a sequence, an operator, and
/// the table behind a row type nested in a composite. What a caller manages is
/// the caller's to say, and an enum reaches nothing (#1523).
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn the_closure_names_what_a_type_reaches() {
    for server in servers() {
        let mut db = Scratch::create(&server, "closure").await;
        db.sql(
            "CREATE SCHEMA app;
             CREATE TABLE app.t0 (id integer);
             CREATE SEQUENCE app.sq;
             CREATE FUNCTION app.chk(integer) RETURNS boolean LANGUAGE sql IMMUTABLE
                 AS 'SELECT $1 > 0';
             CREATE FUNCTION app.dflt() RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT 7';
             CREATE FUNCTION app.body_fn(integer) RETURNS boolean LANGUAGE sql
                 BEGIN ATOMIC SELECT EXISTS (SELECT 1 FROM app.t0 WHERE id = $1); END;
             CREATE OPERATOR app.=== (leftarg = integer, rightarg = integer,
                 function = pg_catalog.int4eq);
             CREATE TYPE app.mood AS ENUM ('a');
             CREATE DOMAIN app.d1 AS integer DEFAULT app.dflt() CHECK (app.chk(VALUE));
             CREATE DOMAIN app.d2 AS app.d1 CHECK (VALUE <> 3);
             CREATE DOMAIN app.d3 AS integer DEFAULT nextval('app.sq');
             CREATE DOMAIN app.d4 AS integer CHECK (app.body_fn(VALUE));
             CREATE DOMAIN app.d5 AS integer CHECK (VALUE OPERATOR(app.===) 3);
             CREATE TYPE app.rows_of_t0 AS (r app.t0, d app.d1);",
        )
        .await;
        let identities = |t: &UserType| -> BTreeSet<String> {
            t.reached().iter().map(|r| r.identity.clone()).collect()
        };
        let set = |names: &[&str]| -> BTreeSet<String> {
            names.iter().map(|n| (*n).to_owned()).collect()
        };
        // Nothing: an enum, and a domain with neither.
        let mood = present(&mut db.conn, "app", "mood").await;
        assert!(mood.reached().is_empty(), "{:?}", mood.reached());
        // Through its own CHECK and DEFAULT.
        let d1 = present(&mut db.conn, "app", "d1").await;
        assert_eq!(identities(&d1), set(&["app.chk(integer)", "app.dflt()"]));
        // Through the domain it is over.
        let d2 = present(&mut db.conn, "app", "d2").await;
        assert_eq!(identities(&d2), set(&["app.chk(integer)", "app.dflt()"]));
        // A default that reads a sequence.
        let d3 = present(&mut db.conn, "app", "d3").await;
        assert_eq!(identities(&d3), set(&["app.sq"]));
        assert_eq!(d3.reached()[0].kind, "sequence");
        // A function body written with BEGIN ATOMIC depends on its table.
        let d4 = present(&mut db.conn, "app", "d4").await;
        assert_eq!(identities(&d4), set(&["app.body_fn(integer)", "app.t0"]));
        // An operator in a CHECK.
        let d5 = present(&mut db.conn, "app", "d5").await;
        assert_eq!(identities(&d5), set(&["app.===(integer,integer)"]));
        // A table's row type nested in a composite reaches the table.
        let rows = present(&mut db.conn, "app", "rows_of_t0").await;
        assert_eq!(
            identities(&rows),
            set(&["app.chk(integer)", "app.dflt()", "app.t0"])
        );
        let table = rows
            .reaches_managed(|r| r.kind == "table" && r.name.as_deref() == Some("t0"))
            .expect("the table is reached");
        assert_eq!(table.schema.as_deref(), Some("app"));
        assert_eq!(table.catalog, "pg_class");
        // Negative: what the caller does not manage is not reported, and the
        // types themselves, and a type's own constraints, are not "reached".
        assert!(
            rows.reaches_managed(|r| r.name.as_deref() == Some("t1"))
                .is_none()
        );
        assert!(
            rows.reached()
                .iter()
                .all(|r| r.kind != "type" && r.kind != "domain constraint")
        );
        let function = d1
            .reaches_managed(|r| r.name.as_deref() == Some("chk"))
            .expect("the CHECK's function");
        assert_eq!(
            (function.kind.as_str(), function.catalog.as_str()),
            ("function", "pg_proc")
        );
        // An extension's type reaches its extension.
        db.sql("CREATE EXTENSION citext SCHEMA public; CREATE TYPE app.ci AS (v public.citext);")
            .await;
        let ci = present(&mut db.conn, "app", "ci").await;
        assert!(
            ci.reached()
                .iter()
                .any(|r| r.kind == "extension" && r.identity == "citext"),
            "{:?}",
            ci.reached()
        );
        db.drop().await;
    }
}

/// An owner is trusted when it is a superuser or can become the deployer or
/// a superuser, and every other owner of anything the type is made of or
/// reaches is named untrusted (DEC-1663.1).
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn an_owner_is_trusted_only_when_it_can_become_the_deployer_or_is_a_superuser() {
    for server in servers() {
        let mut db = Scratch::create(&server, "owners").await;
        let deployer = db.role("deployer", "LOGIN").await;
        let becomes = db.role("becomes", "NOLOGIN").await;
        let inherits = db.role("inherits", "NOLOGIN").await;
        let stranger = db.role("stranger", "NOLOGIN").await;
        let other_stranger = db.role("stranger2", "NOLOGIN").await;
        db.sql(&format!(
            "GRANT {deployer} TO {becomes} WITH SET TRUE, INHERIT FALSE;
             GRANT {deployer} TO {inherits} WITH SET FALSE, INHERIT TRUE;
             CREATE SCHEMA app AUTHORIZATION {deployer};
             GRANT CREATE ON SCHEMA app TO {stranger};
             CREATE FUNCTION app.strangers_check(integer) RETURNS boolean LANGUAGE sql IMMUTABLE
                 AS 'SELECT $1 > 0';
             ALTER FUNCTION app.strangers_check(integer) OWNER TO {stranger};
             CREATE FUNCTION app.own_check(integer) RETURNS boolean LANGUAGE sql IMMUTABLE
                 AS 'SELECT $1 > 0';
             ALTER FUNCTION app.own_check(integer) OWNER TO {deployer};
             CREATE TYPE app.by_deployer AS ENUM ('a');
             ALTER TYPE app.by_deployer OWNER TO {deployer};
             CREATE TYPE app.by_becomes AS ENUM ('a');
             ALTER TYPE app.by_becomes OWNER TO {becomes};
             CREATE TYPE app.by_inherits AS ENUM ('a');
             ALTER TYPE app.by_inherits OWNER TO {inherits};
             CREATE TYPE app.by_stranger AS ENUM ('a');
             ALTER TYPE app.by_stranger OWNER TO {stranger};
             CREATE TYPE app.by_postgres AS ENUM ('a');
             CREATE DOMAIN app.mine AS integer CHECK (app.own_check(VALUE));
             ALTER DOMAIN app.mine OWNER TO {deployer};
             CREATE DOMAIN app.calls_strangers AS integer CHECK (app.strangers_check(VALUE));
             ALTER DOMAIN app.calls_strangers OWNER TO {deployer};
             CREATE DOMAIN app.strangers AS integer;
             ALTER DOMAIN app.strangers OWNER TO {stranger};
             CREATE TYPE app.holds_strangers AS (s app.strangers, m app.mine);
             ALTER TYPE app.holds_strangers OWNER TO {deployer};
             CREATE TEXT SEARCH DICTIONARY app.dict (TEMPLATE = pg_catalog.simple);
             ALTER TEXT SEARCH DICTIONARY app.dict OWNER TO {other_stranger};
             CREATE TEXT SEARCH CONFIGURATION app.cfg (COPY = pg_catalog.simple);
             ALTER TEXT SEARCH CONFIGURATION app.cfg
                 ALTER MAPPING FOR asciiword WITH app.dict;
             ALTER TEXT SEARCH CONFIGURATION app.cfg OWNER TO {stranger};
             CREATE DOMAIN app.searches AS text
                 CHECK (pg_catalog.to_tsvector('app.cfg'::pg_catalog.regconfig, VALUE) IS NOT NULL);
             ALTER DOMAIN app.searches OWNER TO {deployer};
             "
        ))
        .await;
        let label = server.label;
        let mut me = db.as_role(&deployer).await;
        let names = |t: &UserType| -> Vec<String> {
            t.untrusted_owners()
                .iter()
                .map(|o| o.name.clone())
                .collect()
        };
        // The deployer owns, or can become the owner of, what is trusted.
        let by_deployer = present(&mut me, "app", "by_deployer").await;
        assert!(
            by_deployer.owner().trusted,
            "the deployer itself, on {label}"
        );
        let by_becomes = present(&mut me, "app", "by_becomes").await;
        assert!(
            by_becomes.owner().trusted,
            "a role that can SET ROLE to it, on {label}"
        );
        let by_postgres = present(&mut me, "app", "by_postgres").await;
        assert!(by_postgres.owner().trusted, "a superuser, on {label}");
        assert!(names(&by_postgres).is_empty());
        // Inheriting the deployer's privileges is not becoming it.
        let by_inherits = present(&mut me, "app", "by_inherits").await;
        assert_eq!(names(&by_inherits), [inherits.as_str()], "on {label}");
        let by_stranger = present(&mut me, "app", "by_stranger").await;
        assert_eq!(names(&by_stranger), [stranger.as_str()], "on {label}");
        assert_eq!(by_stranger.owner().name, stranger);
        // A domain the deployer owns whose CHECK calls its own function.
        let mine = present(&mut me, "app", "mine").await;
        assert!(names(&mine).is_empty(), "{:?} on {label}", names(&mine));
        // The same domain calling a function a stranger owns: the function is
        // code the stranger can replace, and the owner of the type is not.
        let calls = present(&mut me, "app", "calls_strangers").await;
        assert!(calls.owner().trusted);
        assert_eq!(names(&calls), [stranger.as_str()], "on {label}");
        // A composite the deployer owns holding a domain a stranger owns.
        let holds = present(&mut me, "app", "holds_strangers").await;
        assert!(holds.owner().trusted);
        assert_eq!(names(&holds), [stranger.as_str()], "on {label}");
        assert!(holds.owners().count() >= 2);
        // A text search configuration and the dictionary it maps to are not
        // routines, and their owners can change what a CHECK means in place.
        let searches = present(&mut me, "app", "searches").await;
        assert!(searches.owner().trusted);
        assert_eq!(
            names(&searches),
            [other_stranger.as_str(), stranger.as_str()],
            "on {label}"
        );
        let catalogs: BTreeSet<&str> = searches
            .reached()
            .iter()
            .map(|r| r.catalog.as_str())
            .collect();
        assert!(
            catalogs.contains("pg_ts_config") && catalogs.contains("pg_ts_dict"),
            "{catalogs:?}"
        );
        // Read as the superuser that made the fixture, the question changes
        // with the deployer: a type the deployer owns is a stranger's now,
        // since the deployer cannot become a superuser, and a superuser's is
        // still trusted.
        let su = present(&mut db.conn, "app", "by_deployer").await;
        assert_eq!(names(&su), [deployer.as_str()], "on {label}");
        let su_own = present(&mut db.conn, "app", "by_postgres").await;
        assert!(names(&su_own).is_empty());

        std::mem::drop(me);
        db.drop().await;
    }
}

/// A range inside a type is pinned with the type it ranges over: the range
/// keeps its identity when that type gains a CHECK (review of #2039). So does
/// a multirange, and a domain over the range.
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn a_range_inside_a_type_is_pinned_with_the_type_it_ranges_over() {
    for server in servers() {
        let mut db = Scratch::create(&server, "ranges").await;
        db.sql(
            "CREATE SCHEMA app;
             CREATE DOMAIN app.pos AS integer CHECK (VALUE > 0);
             CREATE TYPE app.pos_range AS RANGE (subtype = app.pos);
             CREATE TYPE app.holds_range AS (r app.pos_range);
             CREATE TYPE app.holds_multirange AS (m app.pos_multirange);
             CREATE TYPE app.holds_array AS (a app.pos_range[]);
             CREATE DOMAIN app.over_range AS app.pos_range;",
        )
        .await;
        let label = server.label;
        let names = [
            "holds_range",
            "holds_multirange",
            "holds_array",
            "over_range",
        ];
        let mut before = Vec::new();
        for name in names {
            before.push(pin_of(&mut db.conn, "app", name).await);
        }
        // The nested range is read with its subtype, and shows it.
        let holds = present(&mut db.conn, "app", "holds_range").await;
        let Body::Composite(c) = &holds.definition().body else {
            panic!("{:?}", holds.definition())
        };
        assert!(
            matches!(&c.attributes[0].ty, Component::Range { name, typtype, subtype, .. }
                if name == "pos_range" && typtype == "r"
                    && matches!(&**subtype, Component::Defined(d) if d.name == "pos")),
            "{:?} on {label}",
            c.attributes[0].ty
        );
        // A no-op leaves every pin as it was.
        db.sql("COMMENT ON DOMAIN app.pos IS 'positive'").await;
        for (name, pin) in names.iter().zip(&before) {
            assert_eq!(&pin_of(&mut db.conn, "app", name).await, pin, "{name}");
        }
        db.sql("ALTER DOMAIN app.pos ADD CONSTRAINT small CHECK (VALUE < 100) NOT VALID")
            .await;
        let mut after = Vec::new();
        for (name, pin) in names.iter().zip(&before) {
            let now = pin_of(&mut db.conn, "app", name).await;
            assert_ne!(
                &now, pin,
                "{name} on {label} kept its pin when the range's subtype changed"
            );
            after.push(now);
        }
        // A new owner of the range itself is a change too, and is named.
        let stranger = db.role("stranger", "NOLOGIN").await;
        db.sql(&format!("ALTER TYPE app.pos_range OWNER TO {stranger}"))
            .await;
        // 18 gives the range's multirange the new owner as well and 16 does
        // not (measured), so the multirange's own owner says what to expect.
        let multirange_owner = db
            .conn
            .query(
                "SELECT pg_get_userbyid(typowner)::text AS owner FROM pg_type
                  WHERE typname = 'pos_multirange'",
            )
            .await
            .unwrap()[0]
            .try_get::<&str>("owner")
            .unwrap()
            .map(str::to_owned);
        let multirange_follows = multirange_owner.as_deref() == Some(stranger.as_str());
        for (name, pin) in names.iter().zip(&after) {
            let read = present(&mut db.conn, "app", name).await;
            let untrusted: Vec<&str> = read
                .untrusted_owners()
                .iter()
                .map(|o| o.name.as_str())
                .collect();
            if *name == "holds_multirange" && !multirange_follows {
                assert_eq!(read.pin().as_str(), pin, "{name} on {label}");
                assert!(untrusted.is_empty(), "{name} on {label}: {untrusted:?}");
            } else {
                assert_ne!(read.pin().as_str(), pin, "{name} on {label}");
                assert_eq!(untrusted, [stranger.as_str()], "{name} on {label}");
            }
        }
        db.drop().await;
    }
}

/// `int2vector` is not `smallint[]`, though the catalog gives both the array
/// category and an element type: a composite that swaps one for the other has
/// another pin (review of #2039).
#[tokio::test]
#[ignore = "needs both live PostgreSQL versions; see scripts/live-tests-pg.sh"]
async fn a_vector_type_is_not_an_array_of_its_element() {
    for server in servers() {
        let mut db = Scratch::create(&server, "vectors").await;
        db.sql(
            "CREATE SCHEMA app;
             CREATE TYPE app.with_vector AS (v int2vector, o oidvector);
             CREATE TYPE app.with_array AS (v smallint[], o oid[]);",
        )
        .await;
        let vector = present(&mut db.conn, "app", "with_vector").await;
        let array = present(&mut db.conn, "app", "with_array").await;
        let attribute = |t: &UserType, i: usize| match &t.definition().body {
            Body::Composite(c) => c.attributes[i].ty.clone(),
            Body::Enum { .. } | Body::Domain(_) => panic!("{:?}", t.definition()),
        };
        assert!(
            matches!(attribute(&vector, 0), Component::Other { ref name, .. } if name == "int2vector")
                && matches!(attribute(&vector, 1), Component::Other { ref name, .. } if name == "oidvector"),
            "{:?} on {}",
            vector.definition(),
            server.label
        );
        assert!(matches!(attribute(&array, 0), Component::Array(_)));
        assert!(matches!(attribute(&array, 1), Component::Array(_)));
        // Same names and positions, and the one pin is not the other's.
        let (a, b) = (
            without_oids(vector.definition()),
            without_oids(array.definition()),
        );
        assert_ne!(a, b);
        db.drop().await;
    }
}
