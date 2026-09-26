//! Desired-namespace reconstruction and binding comparison on real engines
//! (ADR-0016 cases 1–4; issue #613).
//!
//! Each case builds a target by hand, the way history left it, and declares a
//! desired schema. The desired schema is compiled on a separate scratch
//! database by the reconstruction under test, both sides are captured under
//! one derived scope, and the assessment is read per declaration. Scratch is a
//! second database here, not a qualified run: the runtime and authorization
//! gates around it are the dedicated-server fixture's to exercise.

use crate::Postgres;
use crate::resolver::capture::{self, Assessment, Managed, Verdict};
use crate::resolver::reconstruct::Reconstruction;
use pbps_db::resolver::capture::ObjectIdentity;
use pbps_db::transport::{StreamConn, StreamLogin};
use pbps_db::{Conn, Driver};
use pbps_model::{Hints, IdsFile, Module, ModuleId, ModuleKind, Schema};

fn ctx() -> pbps_diff::Context {
    pbps_diff::Context {
        operator: "live-test".into(),
        today: "2026-09-25".into(),
    }
}

const SERVERS: [&str; 2] = ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"];

fn setting<'a>(connection: &'a str, key: &str) -> &'a str {
    connection
        .split_whitespace()
        .find_map(|part| part.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("{key} in the live connection setting"))
}

/// A declared schema, built the way the loader would hand it over.
#[derive(Default)]
struct Declared {
    schema: Schema,
}

impl Declared {
    fn module(mut self, id: &str, kind: ModuleKind, definition: &str) -> Self {
        let id: ModuleId = id.parse().unwrap();
        self.schema.modules.insert(
            id,
            Module {
                kind,
                description: None,
                definition: definition.to_owned(),
            },
        );
        self
    }
    fn view(self, id: &str, definition: &str) -> Self {
        self.module(id, ModuleKind::View, definition)
    }
    fn function(self, id: &str, definition: &str) -> Self {
        self.module(id, ModuleKind::Function, definition)
    }
    fn table(mut self, name: &str, table: pbps_model::Table) -> Self {
        self.schema.tables.insert(name.parse().unwrap(), table);
        self
    }
}

/// One analysis: the target as built by `target`, the desired declarations,
/// and the target's managed side, which is what pbps's model says it owns
/// there. Schemas and extras describe the write path.
struct Case<'a> {
    schemas: &'a [&'a str],
    extras: &'a [&'a str],
    target: &'a str,
    base: Declared,
    desired: Declared,
}

struct Databases {
    server: String,
    names: Vec<String>,
}

impl Databases {
    async fn create(&mut self, name: String, schemas: &[&str]) -> Conn {
        let mut admin = Conn::connect(Driver::Postgres, &self.server).await.unwrap();
        admin
            .execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
            .await
            .unwrap();
        admin
            .execute(&format!("CREATE DATABASE {name} TEMPLATE template0"))
            .await
            .unwrap();
        self.names.push(name.clone());
        let mut conn = Conn::connect(Driver::Postgres, &format!("{} dbname={name}", self.server))
            .await
            .unwrap();
        for schema in schemas {
            if *schema != "public" {
                conn.execute(&format!("CREATE SCHEMA {schema}"))
                    .await
                    .unwrap();
            }
        }
        conn
    }

    async fn stream(&self, name: &str) -> StreamConn {
        let host = setting(&self.server, "host");
        let port: u16 = setting(&self.server, "port").parse().unwrap();
        let socket = tokio::net::TcpStream::connect((host, port)).await.unwrap();
        StreamConn::connect(
            Driver::Postgres,
            socket,
            StreamLogin {
                user: setting(&self.server, "user").to_owned(),
                password: setting(&self.server, "password").to_owned(),
                database: name.to_owned(),
            },
        )
        .await
        .unwrap()
    }

    async fn drop(self) {
        let mut admin = Conn::connect(Driver::Postgres, &self.server).await.unwrap();
        for name in self.names {
            admin
                .execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                .await
                .unwrap();
        }
    }
}

fn bootstrap(desired: &Schema, extras: &[&str]) -> Vec<pbps_model::Change> {
    let ids = pbps_diff::resolve(desired, &IdsFile::default(), &[], &ctx())
        .expect("resolve")
        .ids;
    let empty = Schema::default();
    pbps_diff::diff(
        pbps_diff::Side {
            schema: &empty,
            ids: &IdsFile::default(),
        },
        pbps_diff::Side {
            schema: desired,
            ids: &ids,
        },
        &dialect(extras),
        &Hints::default(),
    )
    .expect("diff")
    .changes
    .into_iter()
    .map(|planned| planned.change)
    .collect()
}

fn dialect(extras: &[&str]) -> Postgres {
    Postgres::with_write_path_extras(extras.iter().map(|&e| e.to_owned()).collect())
}

/// Runs one case on one server. `Err` is a refusal before any verdict.
async fn analyze(server: &str, tag: &str, case: Case<'_>) -> Result<Assessment, String> {
    analyze_on(server, tag, case, capture::Paths::default()).await
}

/// [`analyze`] with a measured path per schema, as a qualified scope
/// supplies it; without one, each schema falls back to the write path.
async fn analyze_on(
    server: &str,
    tag: &str,
    case: Case<'_>,
    measured: capture::Paths,
) -> Result<Assessment, String> {
    let mut databases = Databases {
        server: server.to_owned(),
        names: Vec::new(),
    };
    let token = format!("{tag}_{}", std::process::id());
    let mut target = databases
        .create(format!("pbps_bind613_t_{token}"), case.schemas)
        .await;
    let scratch_name = format!("pbps_bind613_s_{token}");
    databases.create(scratch_name.clone(), case.schemas).await;
    let result = async {
        target
            .execute(case.target)
            .await
            .map_err(|e| e.to_string())?;
        let pg = dialect(case.extras);
        let mut reconstruction =
            Reconstruction::new(&pg, &bootstrap(&case.desired.schema, case.extras))
                .map_err(|e| e.to_string())?;
        let mut scratch = databases.stream(&scratch_name).await;
        reconstruction
            .compile(&pg, &mut scratch)
            .await
            .map_err(|e| e.to_string())?;
        let mut scratch =
            Conn::connect(Driver::Postgres, &format!("{server} dbname={scratch_name}"))
                .await
                .unwrap();
        let desired_managed = Managed::from_schema(&case.desired.schema);
        let base_managed = Managed::from_schema(&case.base.schema);
        let first = capture::capture(&mut scratch, &capture::managed_scope(&desired_managed))
            .await
            .map_err(|e| e.to_string())?;
        let paths = if measured == capture::Paths::default() {
            capture::Paths::new(
                case.extras.iter().map(|&e| e.to_owned()).collect(),
                Default::default(),
            )
        } else {
            measured
        };
        let scope = capture::scope(&first, &[&base_managed, &desired_managed], &paths);
        let desired = capture::capture(&mut scratch, &scope)
            .await
            .map_err(|e| e.to_string())?;
        let current = capture::capture(&mut target, &scope)
            .await
            .map_err(|e| e.to_string())?;
        Ok(capture::assess(
            &current,
            &desired,
            &base_managed,
            &paths,
            &reconstruction,
        ))
    }
    .await;
    drop(target);
    databases.drop().await;
    result
}

/// The relation or routine a surface belongs to, as the assessment reads it.
fn owner(object: &ObjectIdentity) -> &ObjectIdentity {
    match object.class.as_str() {
        "pg_class" | "pg_proc" | "pg_type" => object,
        "pg_constraint" => object
            .signature
            .iter()
            .find(|part| part.class == "pg_class" && !part.name.is_empty())
            .unwrap_or(object),
        _ => object.signature.first().map_or(object, owner),
    }
}

/// Every verdict on the surfaces of `schema.name`, deduplicated.
fn verdicts(assessment: &Assessment, schema: &str, name: &str) -> Vec<Verdict> {
    let mut found: Vec<Verdict> = assessment
        .surfaces
        .iter()
        .filter(|(object, _)| owner(object).name == [schema, name])
        .map(|(_, verdict)| verdict.clone())
        .collect();
    found.dedup();
    found
}

fn only(assessment: &Assessment, schema: &str, name: &str) -> Verdict {
    match verdicts(assessment, schema, name).as_slice() {
        [verdict] => verdict.clone(),
        other => panic!("{schema}.{name}: {other:?} in {assessment:#?}"),
    }
}

#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn an_unrelated_arriving_routine_leaves_a_view_and_a_capturing_one_rebuilds_it() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        // The target's view binds f(numeric), and an unmanaged view depends on
        // it: rebuilding it would hit the unmanaged-dependent gate.
        let target = "
            CREATE FUNCTION app.f(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1;
            SET search_path = app;
            CREATE VIEW app.v AS SELECT f(1) AS x;
            CREATE VIEW app.dependent AS SELECT x FROM app.v;";
        let base = || {
            Declared::default()
                .function(
                    "app.f(numeric)",
                    "(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1",
                )
                .view("app.v", "SELECT f(1) AS x")
        };
        let unrelated = analyze(
            &server,
            "unrelated",
            Case {
                schemas: &["app"],
                extras: &[],
                target,
                base: base(),
                desired: base().function(
                    "app.g(integer)",
                    "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1",
                ),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            only(&unrelated, "app", "v"),
            Verdict::Unaffected,
            "{variable}"
        );
        let capturing = analyze(
            &server,
            "capturing",
            Case {
                schemas: &["app"],
                extras: &[],
                target,
                base: base(),
                desired: base().function(
                    "app.f(integer)",
                    "(integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1",
                ),
            },
        )
        .await
        .unwrap();
        assert_eq!(only(&capturing, "app", "v"), Verdict::Rebuild, "{variable}");
    }
}

fn numeric_f() -> Declared {
    Declared::default().function(
        "app.f(numeric)",
        "(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1",
    )
}

const INTEGER_F: (&str, &str) = (
    "app.f(integer)",
    "(integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1",
);

#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn candidates_earlier_on_the_path_capture_and_other_overloads_and_qualified_names_do_not() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        // `util` follows `app` on the write path. The unqualified view binds
        // util.f(integer); the qualified one names util whatever arrives.
        let target = "
            CREATE FUNCTION util.f(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1;
            SET search_path = app, util;
            CREATE VIEW app.bare AS SELECT f(1) AS x;
            CREATE VIEW app.qualified AS SELECT util.f(1) AS x;";
        let base = || {
            Declared::default()
                .function(
                    "util.f(integer)",
                    "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1",
                )
                .view("app.bare", "SELECT f(1) AS x")
                .view("app.qualified", "SELECT util.f(1) AS x")
        };
        let case = |desired: Declared| Case {
            schemas: &["app", "util"],
            extras: &["util"],
            target,
            base: base(),
            desired,
        };
        let earlier = analyze(
            &server,
            "earlier",
            case(base().function(
                "app.f(integer)",
                "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1",
            )),
        )
        .await
        .unwrap();
        assert_eq!(
            only(&earlier, "app", "bare"),
            Verdict::Rebuild,
            "{variable}"
        );
        assert_eq!(
            only(&earlier, "app", "qualified"),
            Verdict::Unaffected,
            "{variable}"
        );
        // Another overload of the same name, where the call already has an
        // exact match: a candidate, and not the engine's choice.
        let overload = analyze(
            &server,
            "overload",
            case(base().function(
                "util.f(text)",
                "(text) RETURNS integer LANGUAGE sql IMMUTABLE RETURN 0",
            )),
        )
        .await
        .unwrap();
        assert_eq!(
            only(&overload, "app", "bare"),
            Verdict::Unaffected,
            "{variable}"
        );
        assert_eq!(
            only(&overload, "app", "qualified"),
            Verdict::Unaffected,
            "{variable}"
        );
    }
}

/// The target's binding is history, not a bootstrap of its old source: the
/// view was created before f(integer) existed. A fresh bootstrap of the very
/// same declarations binds f(integer), so comparing with one would call the
/// view unchanged; the observed binding says it must be rebuilt. Replanning
/// once it has been rebuilt finds nothing, although f(numeric) is still a
/// visible candidate.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn the_observed_historical_binding_is_compared_not_a_fresh_bootstrap_of_the_old_source() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || {
            numeric_f()
                .function(INTEGER_F.0, INTEGER_F.1)
                .view("app.v", "SELECT f(1) AS x")
        };
        let historical = "
            CREATE FUNCTION app.f(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1;
            SET search_path = app;
            CREATE VIEW app.v AS SELECT f(1) AS x;
            CREATE FUNCTION app.f(integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1;";
        let assessment = analyze(
            &server,
            "history",
            Case {
                schemas: &["app"],
                extras: &[],
                target: historical,
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            only(&assessment, "app", "v"),
            Verdict::Rebuild,
            "{variable}"
        );
        let rebuilt = "
            CREATE FUNCTION app.f(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1;
            CREATE FUNCTION app.f(integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1;
            SET search_path = app;
            CREATE VIEW app.v AS SELECT f(1) AS x;";
        let assessment = analyze(
            &server,
            "replanned",
            Case {
                schemas: &["app"],
                extras: &[],
                target: rebuilt,
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            only(&assessment, "app", "v"),
            Verdict::Unaffected,
            "{variable}"
        );
    }
}

/// Object numbers from two databases never meet: the target's are pushed
/// far from scratch's and an unchanged view still compares equal. And a
/// rename keeps the number while changing the name, so a view whose table
/// was swapped with another under it binds a different logical object than
/// its declaration does.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn bindings_compare_logical_names_never_object_numbers() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let table = || {
            let mut table = pbps_model::Table::default();
            table.columns.insert(
                "id".into(),
                pbps_model::Column::new("integer".parse().unwrap()),
            );
            table
        };
        let declared = || {
            Declared::default()
                .table("app.a", table())
                .table("app.b", table())
                .view("app.v", "SELECT id FROM a")
        };
        let shifted = "
            CREATE TABLE app.filler AS SELECT 1 AS id;
            DROP TABLE app.filler;
            CREATE TABLE app.filler2 (id integer);
            DROP TABLE app.filler2;
            CREATE TABLE app.a (id integer);
            CREATE TABLE app.b (id integer);
            SET search_path = app;
            CREATE VIEW app.v AS SELECT id FROM a;";
        let assessment = analyze(
            &server,
            "numbers",
            Case {
                schemas: &["app"],
                extras: &[],
                target: shifted,
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            only(&assessment, "app", "v"),
            Verdict::Unaffected,
            "{variable}"
        );
        let swapped = "
            CREATE TABLE app.a (id integer);
            CREATE TABLE app.b (id integer);
            SET search_path = app;
            CREATE VIEW app.v AS SELECT id FROM a;
            ALTER TABLE app.a RENAME TO swap;
            ALTER TABLE app.b RENAME TO a;
            ALTER TABLE app.swap RENAME TO b;";
        let assessment = analyze(
            &server,
            "swapped",
            Case {
                schemas: &["app"],
                extras: &[],
                target: swapped,
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            only(&assessment, "app", "v"),
            Verdict::Rebuild,
            "{variable}"
        );
    }
}

/// A default, a CHECK, an index predicate, a routine's parameter default and
/// its SQL-standard body each bind a routine at creation. A capturing
/// overload rebuilds every one of them; an unrelated routine none. A
/// procedural body is runtime-bound: its header is compared and the routine
/// is named as outside the proof, never proven unaffected by silence.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn every_covered_expression_surface_gets_the_engines_answer() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let target = "
            CREATE FUNCTION app.f(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1;
            SET search_path = app;
            CREATE TABLE app.t (c numeric DEFAULT f(1), CONSTRAINT ck CHECK (c > f(1)));
            CREATE INDEX ix ON app.t (c) WHERE c > f(1);
            CREATE FUNCTION app.h(x numeric DEFAULT f(1)) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN f(1) + x;
            CREATE FUNCTION app.k() RETURNS numeric LANGUAGE plpgsql AS $$ BEGIN RETURN f(1); END $$;";
        let declared = || {
            let mut table = pbps_model::Table::default();
            let mut column = pbps_model::Column::new("numeric".parse().unwrap());
            column.default = Some("f(1)".into());
            table.columns.insert("c".into(), column);
            table.checks.insert(
                "ck".into(),
                pbps_model::CheckConstraint {
                    expression: "c > f(1)".into(),
                },
            );
            table.indexes.insert(
                "ix".into(),
                pbps_model::Index {
                    columns: vec![pbps_model::IndexColumn {
                        name: "c".into(),
                        descending: false,
                    }],
                    include: Vec::new(),
                    unique: false,
                    filter: Some("c > f(1)".into()),
                },
            );
            numeric_f()
                .table("app.t", table)
                .function(
                    "app.h(numeric)",
                    "(x numeric DEFAULT f(1)) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN f(1) + x",
                )
                .function(
                    "app.k()",
                    "() RETURNS numeric LANGUAGE plpgsql AS $$ BEGIN RETURN f(1); END $$",
                )
        };
        let case = |desired| Case {
            schemas: &["app"],
            extras: &[],
            target,
            base: declared(),
            desired,
        };
        let capturing = analyze(
            &server,
            "surfaces",
            case(declared().function(INTEGER_F.0, INTEGER_F.1)),
        )
        .await
        .unwrap();
        for owner in ["t", "ix", "h"] {
            assert_eq!(
                only(&capturing, "app", owner),
                Verdict::Rebuild,
                "{variable} {owner}"
            );
        }
        assert!(
            capturing
                .runtime_bound
                .iter()
                .any(|routine| routine.name == ["app", "k"]),
            "{variable}: {capturing:#?}"
        );
        assert!(
            verdicts(&capturing, "app", "k")
                .iter()
                .all(|verdict| *verdict == Verdict::Unaffected),
            "{variable}: only the header can be compared"
        );
        let unrelated = analyze(
            &server,
            "unrelated_surfaces",
            case(declared().function(
                "app.g(integer)",
                "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1",
            )),
        )
        .await
        .unwrap();
        for owner in ["t", "ix", "h"] {
            assert_eq!(
                only(&unrelated, "app", owner),
                Verdict::Unaffected,
                "{variable} {owner}"
            );
        }
    }
}

/// Scratch holds the engine's objects and the managed ones. A candidate the
/// target has beside them — an unmanaged overload elsewhere on the path or
/// beside a managed one, a routine planted among the built-ins, a built-in
/// cast whose context was changed — was not
/// reconstructed, and the answer scratch gave is no proof either way.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_candidate_scratch_did_not_reproduce_leaves_the_surface_unresolved() {
    let unresolved = |assessment: &Assessment| {
        matches!(
            only(assessment, "app", "v"),
            Verdict::Unresolved { condition } if condition.contains("not reconstructed")
        )
    };
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = |tag: &str| {
            let declared = numeric_f().view("app.v", "SELECT f(1) AS x");
            if tag == "drifted" {
                declared.function(
                    "app.f(boolean)",
                    "(boolean) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN 0",
                )
            } else {
                declared
            }
        };
        for (tag, target, schemas, extras) in [
            (
                "unmanaged",
                "CREATE FUNCTION app.f(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1;
                 CREATE FUNCTION util.f(integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1;
                 SET search_path = app, util;
                 CREATE VIEW app.v AS SELECT f(1) AS x;",
                &["app", "util"][..],
                &["util"][..],
            ),
            (
                "sibling",
                "CREATE FUNCTION app.f(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1;
                 CREATE FUNCTION app.f(integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1;
                 SET search_path = app;
                 CREATE VIEW app.v AS SELECT f(1) AS x;",
                &["app"][..],
                &[][..],
            ),
            // The target lost a declared overload and gained an unmanaged
            // one: the count still matches, the identities do not.
            (
                "drifted",
                "CREATE FUNCTION app.f(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1;
                 SET search_path = app;
                 CREATE VIEW app.v AS SELECT f(1) AS x;
                 CREATE FUNCTION app.f(integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1;",
                &["app"][..],
                &[][..],
            ),
            (
                "planted",
                "CREATE FUNCTION app.f(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1;
                 CREATE FUNCTION pg_catalog.f(integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1;
                 SET search_path = app;
                 CREATE VIEW app.v AS SELECT f(1) AS x;",
                &["app"][..],
                &[][..],
            ),
            (
                "cast",
                "CREATE FUNCTION app.f(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1;
                 SET search_path = app;
                 CREATE VIEW app.v AS SELECT f(1) AS x;
                 UPDATE pg_catalog.pg_cast SET castcontext = 'a'
                 WHERE castsource = 'integer'::regtype AND casttarget = 'numeric'::regtype;",
                &["app"][..],
                &[][..],
            ),
        ] {
            let assessment = analyze(
                &server,
                tag,
                Case {
                    schemas,
                    extras,
                    target,
                    base: declared(tag),
                    desired: declared(tag),
                },
            )
            .await
            .unwrap();
            assert!(unresolved(&assessment), "{variable} {tag}: {assessment:#?}");
        }
    }
}

/// A declaration that needs an object nobody reconstructed, and two that
/// need each other, cannot be compiled; the refusal names the declaration,
/// and no verdict is read from the half-built namespace.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_missing_prerequisite_or_a_cycle_refuses_the_reconstruction_by_declaration() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let missing = analyze(
            &server,
            "missing",
            Case {
                schemas: &["app"],
                extras: &[],
                target: "CREATE SCHEMA ext; CREATE TABLE ext.t (id integer);
                         CREATE VIEW app.v AS SELECT id FROM ext.t;",
                base: Declared::default().view("app.v", "SELECT id FROM ext.t"),
                desired: Declared::default().view("app.v", "SELECT id FROM ext.t"),
            },
        )
        .await
        .unwrap_err();
        assert!(
            missing.contains("scratch compilation of view app.v failed"),
            "{variable}: {missing}"
        );
        let cycle = Declared::default()
            .function("app.a()", "() RETURNS integer LANGUAGE sql RETURN app.b()")
            .function("app.b()", "() RETURNS integer LANGUAGE sql RETURN app.a()");
        let refused = analyze(
            &server,
            "cycle",
            Case {
                schemas: &["app"],
                extras: &[],
                target: "",
                base: Declared::default(),
                desired: cycle,
            },
        )
        .await
        .unwrap_err();
        assert!(
            refused.contains("scratch compilation of function app."),
            "{variable}: {refused}"
        );
    }
}

/// Overloads of one name are ordered only by `depends_on`, so scratch may
/// compile a body before the overload it would have preferred. f(integer)
/// then binds f(character varying) through an implicit cast although
/// f(text) is the exact match; its binding cannot be trusted either way.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_body_compiled_before_a_same_named_candidate_is_unresolved() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || {
            Declared::default()
                .function(
                    "app.f(character varying)",
                    "(character varying) RETURNS integer LANGUAGE sql IMMUTABLE RETURN 1",
                )
                .function(
                    "app.f(text)",
                    "(text) RETURNS integer LANGUAGE sql IMMUTABLE RETURN 2",
                )
                .function(
                    "app.f(integer)",
                    "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN f('x'::text)",
                )
        };
        let assessment = analyze(
            &server,
            "early",
            Case {
                schemas: &["app"],
                extras: &[],
                target: "
                    SET search_path = app;
                    CREATE FUNCTION f(character varying) RETURNS integer LANGUAGE sql IMMUTABLE RETURN 1;
                    CREATE FUNCTION f(text) RETURNS integer LANGUAGE sql IMMUTABLE RETURN 2;
                    CREATE FUNCTION f(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN f('x'::text);",
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        let early = assessment
            .surfaces
            .iter()
            .find(|(object, _)| {
                object.name == ["app", "f"]
                    && object
                        .signature
                        .first()
                        .is_some_and(|t| t.name == ["pg_catalog", "int4"])
            })
            .map(|(_, verdict)| verdict.clone());
        assert!(
            matches!(early, Some(Verdict::Unresolved { condition }) if condition.contains("compiled before")),
            "{variable}: {assessment:#?}"
        );
    }
}

/// The order check measures from the overload a step created, not from the
/// first of its name. Here the name scan puts g() before f(text), whose body
/// calls it, while f(integer) was compiled before both: f(text) bound
/// nothing made nameable after it, and its verdict stands.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_later_overload_is_measured_from_its_own_step() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || {
            Declared::default()
                .function(
                    "app.f(integer)",
                    "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN 1",
                )
                .function(
                    "app.g()",
                    "() RETURNS integer LANGUAGE sql IMMUTABLE RETURN 2",
                )
                .function(
                    "app.f(text)",
                    "(text) RETURNS integer LANGUAGE sql IMMUTABLE RETURN g()",
                )
        };
        let assessment = analyze(
            &server,
            "own_step",
            Case {
                schemas: &["app"],
                extras: &[],
                target: "
                    SET search_path = app;
                    CREATE FUNCTION f(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN 1;
                    CREATE FUNCTION g() RETURNS integer LANGUAGE sql IMMUTABLE RETURN 2;
                    CREATE FUNCTION f(text) RETURNS integer LANGUAGE sql IMMUTABLE RETURN g();",
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        let text = assessment
            .surfaces
            .iter()
            .find(|(object, _)| {
                object.name == ["app", "f"]
                    && object
                        .signature
                        .first()
                        .is_some_and(|t| t.name == ["pg_catalog", "text"])
            })
            .map(|(_, verdict)| verdict.clone());
        assert_eq!(
            text,
            Some(Verdict::Unaffected),
            "{variable}: {assessment:#?}"
        );
    }
}

/// Creating a declaration makes objects the model does not name: an identity
/// column's sequence is one. Scratch makes it too, so a view reading it has a
/// faithful candidate and its verdict stands.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn an_object_a_declaration_creates_is_reproduced_with_it() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || {
            let mut table = pbps_model::Table::default();
            let mut id = pbps_model::Column::new("integer".parse().unwrap());
            id.nullable = false;
            id.identity = Some(pbps_model::Identity {
                seed: 1,
                increment: 1,
            });
            table.columns.insert("id".into(), id);
            Declared::default()
                .table("app.t", table)
                .view("app.v", "SELECT last_value FROM t_id_seq")
        };
        let assessment = analyze(
            &server,
            "created_sequence",
            Case {
                schemas: &["app"],
                extras: &[],
                target: "
                    CREATE TABLE app.t (id integer GENERATED BY DEFAULT AS IDENTITY (START WITH 1 INCREMENT BY 1) NOT NULL);
                    SET search_path = app;
                    CREATE VIEW app.v AS SELECT last_value FROM t_id_seq;",
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            only(&assessment, "app", "v"),
            Verdict::Unaffected,
            "{variable}"
        );
    }
}

/// A member both sides hold under one identity is reproduced only when the
/// target's is the project's own too. An unmanaged routine the plan creates
/// again, or an unmanaged sequence that took an identity column's generated
/// name first, shares the identity with scratch's copy without being it. An
/// unmanaged index, or an unmanaged constraint's, depends on a managed table
/// without being made by it.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_shared_identity_the_target_did_not_get_from_a_managed_object_is_unresolved() {
    let identity_table = || {
        let mut table = pbps_model::Table::default();
        let mut id = pbps_model::Column::new("integer".parse().unwrap());
        id.nullable = false;
        id.identity = Some(pbps_model::Identity {
            seed: 1,
            increment: 1,
        });
        table.columns.insert("id".into(), id);
        table
    };
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let view = || Declared::default().view("app.v", "SELECT f(1) AS x");
        let sequence = || {
            Declared::default()
                .table("app.t", identity_table())
                .view("app.v", "SELECT last_value FROM t_id_seq")
        };
        let indexed = || {
            let mut table = pbps_model::Table::default();
            table.columns.insert(
                "x".into(),
                pbps_model::Column::new("integer".parse().unwrap()),
            );
            Declared::default()
                .table("util.t", table.clone())
                .table("app.owner", table)
                .view("app.v", "SELECT x FROM t")
        };
        for (tag, target, base, desired, extras) in [
            (
                "created_routine",
                "CREATE FUNCTION app.f(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1;
                 SET search_path = app;
                 CREATE VIEW app.v AS SELECT f(1) AS x;",
                view(),
                view().function(
                    "app.f(integer)",
                    "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1",
                ),
                &[][..],
            ),
            (
                "taken_sequence",
                "CREATE SEQUENCE app.t_id_seq;
                 CREATE TABLE app.t (id integer GENERATED BY DEFAULT AS IDENTITY (START WITH 1 INCREMENT BY 1) NOT NULL);
                 SET search_path = app;
                 CREATE VIEW app.v AS SELECT last_value FROM t_id_seq;",
                sequence(),
                sequence(),
                &[][..],
            ),
            (
                "automatic_index",
                "CREATE TABLE util.t (x integer);
                 CREATE TABLE app.owner (x integer);
                 SET search_path = app, util;
                 CREATE VIEW app.v AS SELECT x FROM t;
                 CREATE INDEX t ON app.owner (x);",
                indexed(),
                indexed(),
                &["util"][..],
            ),
            (
                "constraint_index",
                "CREATE TABLE util.t (x integer);
                 CREATE TABLE app.owner (x integer);
                 SET search_path = app, util;
                 CREATE VIEW app.v AS SELECT x FROM t;
                 ALTER TABLE app.owner ADD CONSTRAINT t UNIQUE (x);",
                indexed(),
                indexed(),
                &["util"][..],
            ),
        ] {
            let assessment = analyze(
                &server,
                tag,
                Case {
                    schemas: &["app", "util"],
                    extras,
                    target,
                    base,
                    desired,
                },
            )
            .await
            .unwrap();
            assert!(
                matches!(
                    only(&assessment, "app", "v"),
                    Verdict::Unresolved { condition } if condition.contains("not reconstructed")
                ),
                "{variable} {tag}: {assessment:#?}"
            );
        }
    }
}

/// A configured extra the deployer has no USAGE on is not on its measured
/// path, so an unmanaged overload there cannot win and leaves the view's
/// verdict standing. Measured by the analysis scope, not read from the
/// configuration: the same extra on the path is unresolved.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_schema_the_deployer_cannot_use_holds_no_candidate() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || numeric_f().view("app.v", "SELECT f(1) AS x");
        let case = || {
            Case {
            schemas: &["app", "util"],
            extras: &["util"],
            target: "CREATE FUNCTION app.f(numeric) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1;
                     CREATE FUNCTION util.f(integer) RETURNS numeric LANGUAGE sql IMMUTABLE RETURN $1;
                     SET search_path = app;
                     CREATE VIEW app.v AS SELECT f(1) AS x;",
            base: declared(),
            desired: declared(),
        }
        };
        let hidden = capture::Paths::new(
            vec!["util".into()],
            [(
                "app".to_owned(),
                vec!["pg_catalog".to_owned(), "app".to_owned()],
            )]
            .into_iter()
            .collect(),
        );
        let assessment = analyze_on(&server, "hidden", case(), hidden).await.unwrap();
        assert_eq!(
            only(&assessment, "app", "v"),
            Verdict::Unaffected,
            "{variable}"
        );
        let visible = analyze(&server, "visible", case()).await.unwrap();
        assert!(
            matches!(only(&visible, "app", "v"), Verdict::Unresolved { .. }),
            "{variable}: {visible:#?}"
        );
    }
}

/// An index made nameable after a view cannot shadow the routine the view
/// calls, even when the two share a name; the view's verdict stands.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_later_object_of_another_kind_is_no_candidate() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || {
            let mut table = pbps_model::Table::default();
            table.columns.insert(
                "c".into(),
                pbps_model::Column::new("integer".parse().unwrap()),
            );
            table.indexes.insert(
                "ix".into(),
                pbps_model::Index {
                    columns: vec![pbps_model::IndexColumn {
                        name: "c".into(),
                        descending: false,
                    }],
                    include: Vec::new(),
                    unique: false,
                    filter: None,
                },
            );
            Declared::default()
                .table("app.t", table)
                .function(
                    "app.ix(integer)",
                    "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1",
                )
                .view("app.v", "SELECT app.ix(1) AS x")
        };
        let assessment = analyze(
            &server,
            "kind",
            Case {
                schemas: &["app"],
                extras: &[],
                target: "
                    CREATE TABLE app.t (c integer);
                    CREATE INDEX ix ON app.t (c);
                    CREATE FUNCTION app.ix(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1;
                    CREATE VIEW app.v AS SELECT app.ix(1) AS x;",
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            only(&assessment, "app", "v"),
            Verdict::Unaffected,
            "{variable}"
        );
    }
}

/// A function-style cast binds a type, but a same-named routine that fits
/// is what the same call would bind if created now. One arriving on the path
/// after the view is a candidate scratch never reproduced.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_routine_that_would_take_a_function_style_cast_is_a_candidate() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || Declared::default().view("app.v", "SELECT int4('1'::text) AS x");
        let assessment = analyze(
            &server,
            "cast_call",
            Case {
                schemas: &["app"],
                extras: &[],
                target: "
                    SET search_path = app;
                    CREATE VIEW app.v AS SELECT int4('1'::text) AS x;
                    CREATE FUNCTION app.int4(text) RETURNS integer LANGUAGE sql IMMUTABLE RETURN 7;",
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(only(&assessment, "app", "v"), Verdict::Unresolved { .. }),
            "{variable}: {assessment:#?}"
        );
    }
}

/// The reverse: a call that bound a routine without an exact match becomes a
/// cast once a type of that name exists. A type arriving on the path after
/// the view is a candidate scratch never reproduced.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_type_that_would_take_a_routine_call_is_a_candidate() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || {
            Declared::default()
                .function(
                    "app.foo(text)",
                    "(text) RETURNS text LANGUAGE sql IMMUTABLE RETURN 'routine'",
                )
                .view("app.v", "SELECT foo('x')::text AS x")
        };
        let assessment = analyze(
            &server,
            "call_cast",
            Case {
                schemas: &["app"],
                extras: &[],
                target: "
                    SET search_path = app;
                    CREATE FUNCTION foo(text) RETURNS text LANGUAGE sql IMMUTABLE RETURN 'routine';
                    CREATE VIEW app.v AS SELECT foo('x')::text AS x;
                    CREATE TYPE app.foo AS ENUM ('x');",
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(only(&assessment, "app", "v"), Verdict::Unresolved { .. }),
            "{variable}: {assessment:#?}"
        );
    }
}

/// A cast takes one argument, so a type cannot take a call of any other
/// arity: an unmanaged type named like a zero-argument routine is no
/// candidate for `foo()`, and the view's verdict stands.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_type_cannot_take_a_call_that_is_not_a_cast() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || {
            Declared::default()
                .function(
                    "app.foo()",
                    "() RETURNS text LANGUAGE sql IMMUTABLE RETURN 'routine'",
                )
                .view("app.v", "SELECT foo() AS x")
        };
        let assessment = analyze(
            &server,
            "arity",
            Case {
                schemas: &["app"],
                extras: &[],
                target: "
                    SET search_path = app;
                    CREATE FUNCTION foo() RETURNS text LANGUAGE sql IMMUTABLE RETURN 'routine';
                    CREATE VIEW app.v AS SELECT foo() AS x;
                    CREATE TYPE app.foo AS ENUM ('x');",
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            only(&assessment, "app", "v"),
            Verdict::Unaffected,
            "{variable}"
        );
    }
}

/// The call site's arity, not the declaration's: `foo('x')` reaches
/// foo(text, integer DEFAULT 0) with one argument, and a type `foo` arriving
/// after the view turns the same call into a cast (measured on 16 and 18).
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_type_can_take_a_call_that_defaults_reduce_to_one_argument() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || {
            Declared::default()
                .function(
                    "app.foo(text,integer)",
                    "(text, integer DEFAULT 0) RETURNS text LANGUAGE sql IMMUTABLE RETURN 'routine'",
                )
                .view("app.v", "SELECT foo('x')::text AS x")
        };
        let assessment = analyze(
            &server,
            "defaulted",
            Case {
                schemas: &["app"],
                extras: &[],
                target: "
                    SET search_path = app;
                    CREATE FUNCTION foo(text, integer DEFAULT 0) RETURNS text LANGUAGE sql IMMUTABLE RETURN 'routine';
                    CREATE VIEW app.v AS SELECT foo('x')::text AS x;
                    CREATE TYPE app.foo AS ENUM ('x');",
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(only(&assessment, "app", "v"), Verdict::Unresolved { .. }),
            "{variable}: {assessment:#?}"
        );
    }
}

/// An operator's implementation routine follows from the operator; no name
/// was looked up to find it. A same-named unmanaged routine is no candidate
/// for `1 + 1`, and the view's verdict stands.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn an_operators_implementation_is_no_named_candidate() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || Declared::default().view("app.v", "SELECT 1 + 1 AS x");
        let assessment = analyze(
            &server,
            "implementation",
            Case {
                schemas: &["app"],
                extras: &[],
                target: "
                    SET search_path = app;
                    CREATE VIEW app.v AS SELECT 1 + 1 AS x;
                    CREATE FUNCTION app.int4pl(integer, integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN 0;",
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            only(&assessment, "app", "v"),
            Verdict::Unaffected,
            "{variable}"
        );
    }
}

/// COALESCE's type is the operands' common type; no name was looked up to
/// find it. A same-named unmanaged type is no candidate, and the view's
/// verdict stands.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_common_type_the_operands_decide_is_no_named_candidate() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || {
            let mut table = pbps_model::Table::default();
            for column in ["a", "b"] {
                table.columns.insert(
                    column.into(),
                    pbps_model::Column::new("text".parse().unwrap()),
                );
            }
            Declared::default()
                .table("app.t", table)
                .view("app.v", "SELECT COALESCE(a, b) AS x FROM t")
        };
        let assessment = analyze(
            &server,
            "common",
            Case {
                schemas: &["app"],
                extras: &[],
                target: "
                    CREATE TABLE app.t (a text, b text);
                    SET search_path = app;
                    CREATE VIEW app.v AS SELECT COALESCE(a, b) AS x FROM t;
                    CREATE TYPE app.text AS ENUM ('x');",
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            only(&assessment, "app", "v"),
            Verdict::Unaffected,
            "{variable}"
        );
    }
}

/// A later object in a schema the declaration never searches, and not the
/// one its binding named, could not have been resolved in its place: an
/// index `z.t` compiled after a view in `app` reading `app.t` leaves the
/// view's verdict standing.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_later_object_outside_the_path_is_no_candidate() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || {
            let column = || pbps_model::Column::new("integer".parse().unwrap());
            let mut t = pbps_model::Table::default();
            t.columns.insert("id".into(), column());
            let mut o = pbps_model::Table::default();
            o.columns.insert("id".into(), column());
            o.indexes.insert(
                "t".into(),
                pbps_model::Index {
                    columns: vec![pbps_model::IndexColumn {
                        name: "id".into(),
                        descending: false,
                    }],
                    include: Vec::new(),
                    unique: false,
                    filter: None,
                },
            );
            Declared::default()
                .table("app.t", t)
                .table("z.o", o)
                .view("app.v", "SELECT id FROM t")
        };
        let assessment = analyze(
            &server,
            "outside",
            Case {
                schemas: &["app", "z"],
                extras: &[],
                target: "
                    CREATE TABLE app.t (id integer);
                    CREATE TABLE z.o (id integer);
                    CREATE INDEX t ON z.o (id);
                    SET search_path = app;
                    CREATE VIEW app.v AS SELECT id FROM t;",
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            only(&assessment, "app", "v"),
            Verdict::Unaffected,
            "{variable}"
        );
    }
}

/// A CTE's output types follow from its query; no name is looked up to
/// find them. A same-named unmanaged type is no candidate for them.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_ctes_output_types_are_no_named_candidates() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || {
            let mut table = pbps_model::Table::default();
            table
                .columns
                .insert("a".into(), pbps_model::Column::new("text".parse().unwrap()));
            Declared::default()
                .table("app.t", table)
                .view("app.v", "WITH c AS (SELECT a FROM t) SELECT a FROM c")
        };
        let assessment = analyze(
            &server,
            "cte",
            Case {
                schemas: &["app"],
                extras: &[],
                target: "
                    CREATE TABLE app.t (a text);
                    SET search_path = app;
                    CREATE VIEW app.v AS WITH c AS (SELECT a FROM t) SELECT a FROM c;
                    CREATE TYPE app.text AS ENUM ('x');",
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            only(&assessment, "app", "v"),
            Verdict::Unaffected,
            "{variable}"
        );
    }
}

/// Creating a view also creates its row type's array type, `_x`. A view
/// compiled before `app.x` that bound `_x` to `shared._x` would bind
/// `app._x` once `app.x` exists, so its binding is not trusted.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_later_relations_array_type_is_a_later_name() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || {
            let mut table = pbps_model::Table::default();
            table.columns.insert(
                "id".into(),
                pbps_model::Column::new("integer".parse().unwrap()),
            );
            Declared::default()
                .table("shared.x", table)
                .view("app.a", "SELECT NULL::_x AS v")
                .view("app.x", "SELECT 1 AS id")
        };
        let assessment = analyze(
            &server,
            "array",
            Case {
                schemas: &["app", "shared"],
                extras: &["shared"],
                target: "
                    CREATE TABLE shared.x (id integer);
                    SET search_path = app, shared;
                    CREATE VIEW app.a AS SELECT NULL::_x AS v;
                    CREATE VIEW app.x AS SELECT 1 AS id;",
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(
                only(&assessment, "app", "a"),
                Verdict::Unresolved { condition } if condition.contains("compiled before")
            ),
            "{variable}: {assessment:#?}"
        );
    }
}

/// A later view's generated array type is a type, never a relation: a view
/// that read the relation `shared._x` before `app.x` existed would still
/// read it, and its verdict stands.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_later_array_type_does_not_shadow_a_relation() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || {
            let mut table = pbps_model::Table::default();
            table.columns.insert(
                "id".into(),
                pbps_model::Column::new("integer".parse().unwrap()),
            );
            Declared::default()
                .table("shared._x", table)
                .view("app.a", "SELECT id FROM _x")
                .view("app.x", "SELECT 1 AS id")
        };
        let assessment = analyze(
            &server,
            "array_relation",
            Case {
                schemas: &["app", "shared"],
                extras: &["shared"],
                target: "
                    CREATE TABLE shared._x (id integer);
                    SET search_path = app, shared;
                    CREATE VIEW app.a AS SELECT id FROM _x;
                    CREATE VIEW app.x AS SELECT 1 AS id;",
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            only(&assessment, "app", "a"),
            Verdict::Unaffected,
            "{variable}"
        );
    }
}

/// The engine clips a generated array type's name to the identifier limit,
/// so the name is read back rather than spelled: a view of a 63-byte name
/// makes `_` and its first 62 bytes nameable, which a view compiled before
/// it may have bound elsewhere on the path.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_clipped_array_type_name_is_read_back() {
    let long = "v".repeat(63);
    let array = format!("_{}", "v".repeat(62));
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || {
            let mut table = pbps_model::Table::default();
            table.columns.insert(
                "id".into(),
                pbps_model::Column::new("integer".parse().unwrap()),
            );
            Declared::default()
                .table(&format!("shared.{long}"), table)
                .view("app.a", &format!("SELECT NULL::{array} AS v"))
                .view(&format!("app.{long}"), "SELECT 1 AS id")
        };
        let target = format!(
            "CREATE TABLE shared.{long} (id integer);
             SET search_path = app, shared;
             CREATE VIEW app.a AS SELECT NULL::{array} AS v;
             CREATE VIEW app.{long} AS SELECT 1 AS id;"
        );
        let assessment = analyze(
            &server,
            "array_clipped",
            Case {
                schemas: &["app", "shared"],
                extras: &["shared"],
                target: &target,
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(
                only(&assessment, "app", "a"),
                Verdict::Unresolved { condition } if condition.contains("compiled before")
            ),
            "{variable}: {assessment:#?}"
        );
    }
}

/// A set operation's equality operator comes from the type's default
/// operator class, never a name lookup, so a same-named unmanaged operator
/// is no candidate for it.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_set_operations_inferred_operators_are_no_named_candidates() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || Declared::default().view("app.v", "SELECT 1 AS a UNION SELECT 2");
        let assessment = analyze(
            &server,
            "set_operation",
            Case {
                schemas: &["app"],
                extras: &[],
                target: "
                    SET search_path = app;
                    CREATE VIEW app.v AS SELECT 1 AS a UNION SELECT 2;
                    CREATE FUNCTION app.never(integer, integer) RETURNS boolean LANGUAGE sql IMMUTABLE RETURN false;
                    CREATE OPERATOR app.= (LEFTARG = integer, RIGHTARG = integer, FUNCTION = app.never);",
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            only(&assessment, "app", "v"),
            Verdict::Unaffected,
            "{variable}"
        );
    }
}

/// An SQL value function's type is fixed: CURRENT_DATE is a date whatever
/// else is named `date`, so a same-named unmanaged type is no candidate.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn an_sql_value_functions_type_is_no_named_candidate() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || Declared::default().view("app.v", "SELECT CURRENT_DATE AS d");
        let assessment = analyze(
            &server,
            "sql_value",
            Case {
                schemas: &["app"],
                extras: &[],
                target: "
                    SET search_path = app;
                    CREATE VIEW app.v AS SELECT CURRENT_DATE AS d;
                    CREATE TYPE app.date AS ENUM ('x');",
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            only(&assessment, "app", "v"),
            Verdict::Unaffected,
            "{variable}"
        );
    }
}

/// A cast takes one argument, so of the routines named like a cast's type
/// only one a single argument can reach could take the call: a two-argument
/// `app.int4` beside `int4('1'::text)` is no candidate.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_routine_no_single_argument_reaches_cannot_take_a_cast() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || Declared::default().view("app.v", "SELECT int4('1'::text) AS x");
        let assessment = analyze(
            &server,
            "two_arguments",
            Case {
                schemas: &["app"],
                extras: &[],
                target: "
                    SET search_path = app;
                    CREATE VIEW app.v AS SELECT int4('1'::text) AS x;
                    CREATE FUNCTION app.int4(integer, integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN 0;",
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            only(&assessment, "app", "v"),
            Verdict::Unaffected,
            "{variable}"
        );
    }
}

/// Casts are consulted by routine, operator and coercion resolution. A view
/// that only reads a column resolves nothing a cast could change, so an
/// unrelated cast on the target leaves its verdict standing.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_cast_cannot_change_a_surface_that_resolves_no_call() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || {
            let mut table = pbps_model::Table::default();
            table.columns.insert(
                "c".into(),
                pbps_model::Column::new("integer".parse().unwrap()),
            );
            Declared::default()
                .table("app.t", table)
                .view("app.v", "SELECT c FROM t")
        };
        let assessment = analyze(
            &server,
            "column_only",
            Case {
                schemas: &["app"],
                extras: &[],
                target: "
                    CREATE TABLE app.t (c integer);
                    SET search_path = app;
                    CREATE VIEW app.v AS SELECT c FROM t;
                    CREATE CAST (date AS money) WITH INOUT;",
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            only(&assessment, "app", "v"),
            Verdict::Unaffected,
            "{variable}"
        );
    }
}

/// An index is a relation with no type. A managed index named `foo` does not
/// make an unmanaged type `app.foo` one of the project's own, and that type
/// can take a call `foo('x')` as a cast.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_managed_index_does_not_account_for_a_same_named_type() {
    for variable in SERVERS {
        let server = std::env::var(variable).unwrap();
        let declared = || {
            let mut table = pbps_model::Table::default();
            table.columns.insert(
                "c".into(),
                pbps_model::Column::new("integer".parse().unwrap()),
            );
            table.indexes.insert(
                "foo".into(),
                pbps_model::Index {
                    columns: vec![pbps_model::IndexColumn {
                        name: "c".into(),
                        descending: false,
                    }],
                    include: Vec::new(),
                    unique: false,
                    filter: None,
                },
            );
            Declared::default()
                .table("app.t", table)
                .function(
                    "app.foo(text)",
                    "(text) RETURNS text LANGUAGE sql IMMUTABLE RETURN 'routine'",
                )
                .view("app.v", "SELECT foo('x')::text AS x")
        };
        let assessment = analyze(
            &server,
            "index_type",
            Case {
                schemas: &["app"],
                extras: &[],
                target: "
                    CREATE TABLE app.t (c integer);
                    CREATE INDEX foo ON app.t (c);
                    SET search_path = app;
                    CREATE FUNCTION foo(text) RETURNS text LANGUAGE sql IMMUTABLE RETURN 'routine';
                    CREATE VIEW app.v AS SELECT foo('x')::text AS x;
                    CREATE TYPE app.foo AS ENUM ('x');",
                base: declared(),
                desired: declared(),
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(only(&assessment, "app", "v"), Verdict::Unresolved { .. }),
            "{variable}: {assessment:#?}"
        );
    }
}
