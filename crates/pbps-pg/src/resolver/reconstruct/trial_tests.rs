//! Real-engine trial, barrier and publication contracts for DEC-2465.1.
use super::*;
use pbps_db::{Conn, Driver, transport::ExecuteConnection};
use pbps_model::{Column, Generated, Module, Table, TableName, Uid, UidKind};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Point {
    Readback,
    Release,
    CommitAck,
}
impl Reconstruction {
    pub(super) async fn inject(
        &self,
        conn: &mut impl ExecuteConnection,
        point: Point,
        at: usize,
    ) -> Result<(), ReconstructError> {
        if self.fault != Some((point, at)) {
            return Ok(());
        }
        let error = conn
            .execute(if point == Point::Release {
                "RELEASE SAVEPOINT absent_trial"
            } else {
                "SELECT 1/0"
            })
            .await
            .unwrap_err();
        Err(if point == Point::Release {
            ReconstructError::Transaction("release savepoint")
        } else {
            ReconstructError::Read {
                declaration: self.steps[at].declaration.clone(),
                reason: error.to_string(),
            }
        })
    }
}
pub(crate) fn module(id: &str, kind: ModuleKind, definition: &str) -> Change {
    Change::CreateModule {
        id: id.parse().unwrap(),
        module: Box::new(Module {
            kind,
            description: None,
            definition: definition.into(),
            options: Default::default(),
        }),
    }
}
fn function(name: &str, body: &str) -> Change {
    module(
        &format!("app.{name}(integer)"),
        ModuleKind::Function,
        &format!("(integer) RETURNS integer LANGUAGE sql IMMUTABLE {body}"),
    )
}
pub(crate) fn table(expression: &str) -> Change {
    let mut t = Table::default();
    t.columns
        .insert("a".into(), Column::new("integer".parse().unwrap()));
    let mut g = Column::new("integer".parse().unwrap());
    g.value_source = ValueSource::Generated(Generated {
        expression: expression.into(),
        stored: true,
    });
    t.columns.insert("g".into(), g);
    t.columns
        .insert("z".into(), Column::new("integer".parse().unwrap()));
    Change::CreateTable {
        uid: Uid::derived(UidKind::Table, "t", 0),
        name: TableName::new("app", "t"),
        table: Box::new(t),
        beside_default: None,
        moves_at_most: None,
    }
}
pub(crate) fn build(
    pg: &crate::Postgres,
    changes: &[Change],
    dependencies: &pbps_model::ModuleDeps,
    guesses: bool,
) -> Reconstruction {
    let plan = fixture_plan(changes);
    let prepared =
        pbps_diff::order::scratch::prepare(&plan, &fixture_ids(changes), dependencies, pg).unwrap();
    // This fault models a missed lexical inference, retaining every hard edge.
    Reconstruction::from_prepared(
        pg,
        if guesses {
            prepared
        } else {
            prepared.without_guesses()
        },
    )
    .unwrap()
}
pub(crate) async fn database(server: &str) -> (Conn, Conn, String, crate::Postgres) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let mut admin = Conn::connect(Driver::Postgres, server).await.unwrap();
    let version = crate::roles::server_version_num(&mut admin).await.unwrap();
    let name = format!(
        "c2465_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    admin
        .execute(&format!("CREATE DATABASE {name} TEMPLATE template0"))
        .await
        .unwrap();
    let mut conn = Conn::connect(Driver::Postgres, &format!("{server} dbname={name}"))
        .await
        .unwrap();
    conn.execute("CREATE SCHEMA app").await.unwrap();
    (
        admin,
        conn,
        name,
        crate::Postgres::new().with_server_version(version),
    )
}
pub(crate) async fn remove(admin: &mut Conn, name: &str) {
    admin
        .execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
        .await
        .unwrap();
}
pub(crate) fn servers() -> Vec<String> {
    ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"]
        .iter()
        .map(|key| std::env::var(key).expect("both PG endpoints required"))
        .collect()
}
async fn compile(
    r: &mut Reconstruction,
    pg: &crate::Postgres,
    conn: &mut Conn,
) -> Result<(), ReconstructError> {
    r.compile(pg, &pbps_model::SessionZone::utc(), conn).await
}
fn actual(r: &Reconstruction) -> Vec<String> {
    r.committed.iter().map(|&work| r.work_name(work)).collect()
}

#[tokio::test]
#[ignore = "requires PostgreSQL 16 and 18"]
async fn missing_guesses_do_not_prevent_the_round9_view_from_compiling() {
    for server in servers() {
        let (mut admin, mut conn, name, pg) = database(&server).await;
        let changes = [
            module(
                "app.v",
                ModuleKind::View,
                "SELECT pg_relation_size('app.t') AS n",
            ),
            table("app.f(z)"),
            function("f", "RETURN $1"),
        ];
        let mut r = build(&pg, &changes, &Default::default(), false);
        assert!(r.observations().is_none());
        let result = compile(&mut r, &pg, &mut conn).await;
        let order = actual(&r);
        remove(&mut admin, &name).await;
        result.unwrap();
        assert!(
            order.iter().position(|d| d == "view app.v").unwrap()
                > order.iter().position(|d| d == "table app.t").unwrap()
        );
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL 16 and 18"]
#[allow(clippy::wildcard_enum_match_arm)]
async fn the_three_call_counterexamples_compile_deterministically() {
    for server in servers() {
        for case in 0..3 {
            let mut orders = Vec::new();
            for _ in 0..2 {
                let (mut admin, mut conn, name, pg) = database(&server).await;
                let mut t = table(["f + 1", "ext.f(z)", "app.f(z)"][case]);
                if let Change::CreateTable { table, .. } = &mut t {
                    table
                        .columns
                        .insert("f".into(), Column::new("integer".parse().unwrap()));
                }
                if case == 1 {
                    conn.execute("CREATE SCHEMA ext; CREATE FUNCTION ext.f(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1").await.unwrap();
                }
                let mut changes = vec![
                    t,
                    if case == 2 {
                        module(
                            "app.f(text)",
                            ModuleKind::Function,
                            "(text) RETURNS integer LANGUAGE sql IMMUTABLE AS $$ SELECT count(*)::int FROM app.t $$",
                        )
                    } else {
                        function("f", "AS $$ SELECT count(*)::int FROM app.t $$")
                    },
                ];
                if case == 2 {
                    changes.push(function("f", "RETURN $1"));
                }
                // Run the real differ/lower path, not the omitted-guess fault.
                let mut schema = pbps_model::Schema::default();
                for c in &changes {
                    match c {
                        Change::CreateTable { name, table, .. } => {
                            schema.tables.insert(name.clone(), *table.clone());
                        }
                        Change::CreateModule { id, module } => {
                            schema.modules.insert(id.clone(), *module.clone());
                        }
                        _ => unreachable!(),
                    }
                }
                let ids = pbps_diff::resolve(
                    &schema,
                    &Default::default(),
                    &[],
                    &pbps_diff::Context {
                        operator: "trial".into(),
                        today: "2026-10-28".into(),
                    },
                )
                .unwrap()
                .ids;
                let plan = pbps_diff::diff(
                    pbps_diff::Side {
                        schema: &Default::default(),
                        ids: &Default::default(),
                    },
                    pbps_diff::Side {
                        schema: &schema,
                        ids: &ids,
                    },
                    &pg,
                    &Default::default(),
                )
                .unwrap();
                let mut r = Reconstruction::new(&pg, &plan, &ids).unwrap();
                let result = compile(&mut r, &pg, &mut conn).await;
                orders.push(actual(&r));
                if result.is_ok() {
                    conn.execute("INSERT INTO app.t(a,z,f) VALUES (1,4,3)")
                        .await
                        .unwrap();
                    let rows = conn
                        .query("SELECT g = 4 AS correct FROM app.t")
                        .await
                        .unwrap();
                    assert_eq!(rows[0].try_get::<bool>("correct").unwrap(), Some(true));
                    assert!(r.observations().is_some());
                    let managed = crate::resolver::capture::Managed::from_schema(&schema);
                    let first = crate::resolver::capture::capture(
                        &mut conn,
                        &crate::resolver::capture::managed_scope(&managed),
                    )
                    .await
                    .unwrap();
                    let paths = crate::resolver::capture::Paths::default();
                    let scope = crate::resolver::capture::scope(&first, &[&managed], &paths);
                    let captured = crate::resolver::capture::capture(&mut conn, &scope)
                        .await
                        .unwrap();
                    let assessment = crate::resolver::capture::assess(
                        &captured, &captured, &managed, &paths, &r,
                    );
                    assert!(!assessment.surfaces.is_empty());
                    let pg16 =
                        crate::roles::server_version_num(&mut conn).await.unwrap() / 10_000 == 16;
                    let conditions: Vec<_> = assessment
                        .surfaces
                        .values()
                        .filter_map(|verdict| {
                            if let crate::resolver::capture::Verdict::Unresolved { condition } =
                                verdict
                            {
                                Some(condition.as_str())
                            } else {
                                assert_eq!(*verdict, crate::resolver::capture::Verdict::Unaffected);
                                None
                            }
                        })
                        .collect();
                    // Compilation grants no binding authority (DEC-2465.1).
                    // Ordinary assess has no staged-external comparison here.
                    // PG16's whole table also binds before the text overload;
                    // the existing conservative early-binding guard still holds.
                    let expected = if case == 1 {
                        vec![
                            "a same-named candidate on the target was not reconstructed on scratch",
                        ]
                    } else if case == 2 && pg16 {
                        vec![
                            "the declaration was compiled before an object sharing a name it bound",
                        ]
                    } else {
                        Vec::new()
                    };
                    assert_eq!(conditions, expected);
                }
                remove(&mut admin, &name).await;
                result.unwrap();
            }
            assert_eq!(orders[0], orders[1]);
        }
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL 16 and 18"]
async fn hard_predecessors_block_independently_executable_steps() {
    for server in servers() {
        let (mut admin, mut conn, name, pg) = database(&server).await;
        let changes = [
            function("f1", "RETURN app.f2($1)"),
            module("app.v", ModuleKind::View, "SELECT 1 AS n"),
            function("f2", "RETURN length('f1')"),
            table("a + 1"),
        ];
        let deps = [(
            "app.v".parse().unwrap(),
            ["app.f1(integer)".parse().unwrap()].into_iter().collect(),
        )]
        .into_iter()
        .collect();
        let mut r = build(&pg, &changes, &deps, false);
        let result = compile(&mut r, &pg, &mut conn).await;
        let order = actual(&r);
        remove(&mut admin, &name).await;
        result.unwrap();
        assert!(
            order
                .iter()
                .position(|d| d == "function app.f1(integer)")
                .unwrap()
                < order.iter().position(|d| d == "view app.v").unwrap(),
            "{order:?}"
        );
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL 16 and 18"]
async fn percent_type_waits_for_all_ordinary_providers_before_one_recovery() {
    for server in servers() {
        for count in [1, 2] {
            let (mut admin, mut conn, name, pg) = database(&server).await;
            let mut changes = vec![
                module(
                    "app.f(integer)",
                    ModuleKind::Function,
                    if count == 1 {
                        "(integer) RETURNS integer LANGUAGE plpgsql AS $$ DECLARE x app.v.x%TYPE; BEGIN RETURN 1; END $$"
                    } else {
                        "(integer) RETURNS integer LANGUAGE plpgsql AS $$ DECLARE x app.v.x%TYPE; y app.w.y%TYPE; BEGIN RETURN 1; END $$"
                    },
                ),
                module("app.v", ModuleKind::View, "SELECT 1 AS x"),
            ];
            if count == 2 {
                changes.push(module("app.w", ModuleKind::View, "SELECT 1 AS y"));
            }
            // Actual scratch major controls 42601 even with another expansion capability.
            let capability = crate::Postgres::new().with_server_version(180_000);
            let mut r = build(&capability, &changes, &Default::default(), false);
            let result = compile(&mut r, &capability, &mut conn).await;
            let order = actual(&r);
            remove(&mut admin, &name).await;
            result.unwrap_or_else(|e| panic!("{pg:?}: {e}"));
            assert!(order.last().unwrap().starts_with("function"));
        }
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL 16 and 18"]
async fn stalls_name_every_pending_declaration_and_respect_range_barriers() {
    for server in servers() {
        for barrier in [false, true] {
            let (mut admin, mut conn, name, pg) = database(&server).await;
            let changes = if barrier {
                vec![
                    module("app.v", ModuleKind::View, "SELECT * FROM app.w"),
                    module("app.w", ModuleKind::View, "SELECT 1 AS n"),
                ]
            } else {
                vec![
                    function("f", "RETURN app.g($1)"),
                    function("g", "RETURN app.f($1)"),
                ]
            };
            let mut r = build(&pg, &changes, &Default::default(), false);
            if barrier {
                r.fixture_fill_at(1, Vec::new());
            }
            let before = r.steps.clone();
            let error = r
                .compile_next(&pg, &pbps_model::SessionZone::utc(), &mut conn)
                .await
                .unwrap_err()
                .to_string();
            remove(&mut admin, &name).await;
            assert!(
                error.contains(if barrier { "42P01" } else { "42883" }),
                "{error}"
            );
            if barrier {
                assert!(error.contains("range boundaries"));
            } else {
                assert!(error.contains("app.f(integer)"));
                assert!(error.contains("app.g(integer)"));
            }
            assert_eq!(r.steps, before);
            assert!(r.committed.is_empty());
            assert!(r.observations().is_none());
        }
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL 16 and 18"]
async fn rejected_ranges_publish_no_metadata_and_cannot_be_reused() {
    for server in servers() {
        for point in [Point::Readback, Point::Release, Point::CommitAck] {
            let (mut admin, mut conn, name, pg) = database(&server).await;
            let mut r = build(
                &pg,
                &[function("f", "RETURN $1"), table("a+1")],
                &Default::default(),
                false,
            );
            let before = r.steps.clone();
            r.fault = Some((point, if point == Point::CommitAck { 0 } else { 1 }));
            let result = compile(&mut r, &pg, &mut conn).await;
            remove(&mut admin, &name).await;
            assert!(result.is_err(), "{point:?}");
            assert_eq!(r.steps, before, "{point:?}");
            assert!(r.committed.is_empty());
            assert!(r.observations().is_none());
        }
        // Real deferred constraint rejection, after successful catalog readback.
        let (mut admin, mut conn, name, pg) = database(&server).await;
        conn.execute("CREATE TABLE public.commit_gate(x integer UNIQUE DEFERRABLE INITIALLY DEFERRED); CREATE FUNCTION public.reject_commit() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO public.commit_gate VALUES (1),(1); END $$; CREATE EVENT TRIGGER reject_commit ON ddl_command_end WHEN TAG IN ('CREATE TABLE') EXECUTE FUNCTION public.reject_commit()").await.unwrap();
        let mut r = build(&pg, &[table("a+1")], &Default::default(), false);
        let before = r.steps.clone();
        let error = compile(&mut r, &pg, &mut conn).await.unwrap_err();
        assert!(matches!(error, ReconstructError::Transaction("commit")));
        assert_eq!(r.steps, before);
        assert!(r.committed.is_empty());
        assert!(r.observations().is_none());
        assert!(compile(&mut r, &pg, &mut conn).await.is_err());
        remove(&mut admin, &name).await;
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL 16 and 18"]
async fn suffix_failure_keeps_prefix_private_and_fill_at_the_committed_boundary() {
    for server in servers() {
        let (mut admin, mut conn, name, pg) = database(&server).await;
        let changes = [
            module("app.v", ModuleKind::View, "SELECT 1 AS n"),
            function("f", "RETURN 1 / 0"),
        ];
        let mut r = build(&pg, &changes, &Default::default(), false);
        r.fixture_fill_at(1, vec![(Nameable::Type, "ext".into(), "filled".into())]);
        r.compile_next(&pg, &pbps_model::SessionZone::utc(), &mut conn)
            .await
            .unwrap();
        conn.execute("CREATE SCHEMA ext").await.unwrap();
        let before_fill = crate::resolver::externals::inventory(&mut conn)
            .await
            .unwrap();
        conn.execute("CREATE DOMAIN ext.filled AS integer")
            .await
            .unwrap();
        let after_fill = crate::resolver::externals::inventory(&mut conn)
            .await
            .unwrap();
        r.finish_fill(&before_fill, &after_fill).unwrap();
        // Data failure at CREATE time (constant folding in a view).
        let mut invalid = build(
            &pg,
            &[module(
                "app.bad",
                ModuleKind::View,
                "SELECT 'x'::integer AS n",
            )],
            &Default::default(),
            false,
        );
        assert!(compile(&mut invalid, &pg, &mut conn).await.is_err());
        r.fault = Some((Point::Readback, 1));
        let error = r
            .compile_next(&pg, &pbps_model::SessionZone::utc(), &mut conn)
            .await;
        remove(&mut admin, &name).await;
        assert!(error.is_err());
        assert_eq!(r.committed, [FillWork::Declared(0), FillWork::Fill(0)]);
        assert!(r.steps[1].created.is_none());
        assert!(r.observations().is_none());
        assert!(
            r.later_names(&ObjectIdentity {
                class: "pg_class".into(),
                name: vec!["app".into(), "v".into()],
                signature: vec![]
            })
            .is_none()
        );
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL 16 and 18"]
async fn terminal_statement_errors_do_not_retry_or_attempt_a_later_provider() {
    for server in servers() {
        for case in 0..4 {
            let (mut admin, mut conn, name, pg) = database(&server).await;
            let role = format!("{name}_role");
            let definition = match case {
                0 => {
                    conn.execute("CREATE VIEW app.v AS SELECT 1 AS n")
                        .await
                        .unwrap();
                    "SELECT 1 AS n"
                }
                1 => {
                    conn.execute(&format!("CREATE ROLE {role}; SET ROLE {role}"))
                        .await
                        .unwrap();
                    "SELECT 1 AS n"
                }
                2 => "SELECT 'invalid'::integer AS n",
                _ => "SELECT DISTINCT 1 AS n FOR UPDATE",
            };
            let changes = [
                module("app.v", ModuleKind::View, definition),
                module("app.provider", ModuleKind::View, "SELECT 1 AS n"),
            ];
            let mut r = build(&pg, &changes, &Default::default(), false);
            let error = compile(&mut r, &pg, &mut conn)
                .await
                .unwrap_err()
                .to_string();
            conn.execute("RESET ROLE").await.unwrap();
            let rows = conn
                .query("SELECT to_regclass('app.provider') IS NULL AS absent")
                .await
                .unwrap();
            assert_eq!(rows[0].try_get::<bool>("absent").unwrap(), Some(true));
            remove(&mut admin, &name).await;
            if case == 1 {
                admin.execute(&format!("DROP ROLE {role}")).await.unwrap();
            }
            assert!(
                error.contains(["42P07", "42501", "22P02", "0A000"][case]),
                "{error}"
            );
            assert!(!error.contains("recovery refusal"));
            assert!(r.committed.is_empty());
        }
    }
}

#[test]
fn explicit_same_leaf_overload_dependencies_remain_hard() {
    let pg = crate::Postgres::new().with_server_version(160_000);
    let changes = [
        function("f", "RETURN $1"),
        module(
            "app.f(text)",
            ModuleKind::Function,
            "(text) RETURNS integer LANGUAGE sql IMMUTABLE RETURN 1",
        ),
    ];
    let deps = [(
        "app.f(integer)".parse().unwrap(),
        ["app.f(text)".parse().unwrap()].into_iter().collect(),
    )]
    .into_iter()
    .collect();
    let r = build(&pg, &changes, &deps, false);
    let mut cursor = r.trial.start(0..r.len(), &r.progress).unwrap();
    let a = cursor.next_ready().unwrap();
    assert_eq!(r.steps[a.position()].declaration, "function app.f(text)");
    a.succeeded();
    let a = cursor.next_ready().unwrap();
    assert_eq!(r.steps[a.position()].declaration, "function app.f(integer)");
}

#[tokio::test]
#[ignore = "requires PostgreSQL 16 and 18"]
async fn genuine_syntax_is_refused_with_its_original_diagnosis() {
    for server in servers() {
        for body in [
            "AS $$ SELECT FROM $$",
            "LANGUAGE plpgsql AS $$ BEGN RETURN 1; END $$",
        ] {
            let (mut admin, mut conn, name, pg) = database(&server).await;
            let pg16 = crate::roles::server_version_num(&mut conn).await.unwrap() / 10_000 == 16;
            let definition = if body.starts_with("LANGUAGE") {
                format!("(integer) RETURNS integer {body}")
            } else {
                format!("(integer) RETURNS integer LANGUAGE sql {body}")
            };
            let changes = [
                module("app.bad(integer)", ModuleKind::Function, &definition),
                module("app.provider", ModuleKind::View, "SELECT 1 AS n"),
            ];
            let mut r = build(&pg, &changes, &Default::default(), false);
            let error = compile(&mut r, &pg, &mut conn)
                .await
                .unwrap_err()
                .to_string();
            remove(&mut admin, &name).await;
            assert!(error.contains("42601"), "{error}");
            assert!(error.contains("syntax error"), "{error}");
            assert_eq!(error.matches("recovery refusal").count(), usize::from(pg16));
            assert!(r.committed.is_empty());
        }
    }
}

#[test]
fn pending_or_failed_fill_cannot_grant_observations() {
    let pg = crate::Postgres::new();
    let mut r = build(&pg, &[], &Default::default(), false);
    r.fixture_fill_at(0, Vec::new());
    assert_eq!(r.len(), 1);
    assert!(!r.is_empty());
    assert!(r.observations().is_none());
    let root = r.fills[0].root.clone();
    let after = crate::resolver::externals::Inventory::from([(
        (root.0, 1),
        crate::resolver::externals::Inventoried {
            described: root.1,
            carried: None,
            name: Some((Nameable::Type, "ext".into(), "filled".into())),
            generated: false,
            makers: Vec::new(),
        },
    )]);
    r.finish_fill(&Default::default(), &after).unwrap();
    assert!(r.observations().is_some());
    let mut r = build(&pg, &[], &Default::default(), false);
    r.fixture_fill_at(0, Vec::new());
    assert!(
        r.finish_fill(&Default::default(), &Default::default())
            .is_err()
    );
    assert!(r.observations().is_none());
}

#[tokio::test]
#[ignore = "requires PostgreSQL 16 and 18"]
async fn retries_roll_back_whole_groups_but_can_repeat_sequence_side_effects() {
    for server in servers() {
        let (mut admin, mut conn, name, pg) = database(&server).await;
        conn.execute("CREATE SEQUENCE public.attempts; CREATE FUNCTION public.count_attempt() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM nextval('public.attempts'); END $$; CREATE EVENT TRIGGER count_attempt ON ddl_command_start WHEN TAG IN ('CREATE TABLE') EXECUTE FUNCTION public.count_attempt()").await.unwrap();
        let mut t = table("a+1");
        if let Change::CreateTable { table, .. } = &mut t {
            table.indexes.insert(
                "missing_class".into(),
                pbps_model::Index {
                    columns: vec![pbps_model::IndexColumn {
                        key: pbps_model::IndexKey::Column("a".into()),
                        descending: false,
                        opclass: Some("app.absent_ops".into()),
                    }],
                    include: vec![],
                    unique: false,
                    filter: None,
                    method: Default::default(),
                    storage_parameters: Default::default(),
                },
            );
        }
        let mut r = build(
            &pg,
            &[t, function("f", "RETURN $1")],
            &Default::default(),
            false,
        );
        assert!(r.lowered.groups()[0].statements().len() > 1);
        let before = r.steps.clone();
        let error = compile(&mut r, &pg, &mut conn)
            .await
            .unwrap_err()
            .to_string();
        let rows=conn.query("SELECT last_value = 2 AS twice, to_regclass('app.t') IS NULL AS absent FROM public.attempts").await.unwrap();
        let twice = rows[0].try_get::<bool>("twice").unwrap();
        let absent = rows[0].try_get::<bool>("absent").unwrap();
        remove(&mut admin, &name).await;
        assert!(error.contains("42704"), "{error}");
        assert!(
            !error.contains("42P07"),
            "the earlier CREATE must be undone before retry: {error}"
        );
        assert_eq!(twice, Some(true));
        assert_eq!(absent, Some(true));
        assert_eq!(r.steps, before);
        assert!(r.observations().is_none());
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL 16 and 18"]
async fn recovery_interruptions_are_not_creation_refusals() {
    for server in servers() {
        let mut check = Conn::connect(Driver::Postgres, &server).await.unwrap();
        if crate::roles::server_version_num(&mut check).await.unwrap() / 10_000 != 16 {
            continue;
        }
        for killed in [true, false] {
            let (mut admin, mut conn, name, pg) = database(&server).await;
            // Sequence increments survive the first failed savepoint, so the
            // trigger can distinguish the recovery attempt without external I/O.
            let action = if killed {
                "PERFORM pg_terminate_backend(pg_backend_pid())"
            } else {
                "RAISE EXCEPTION 'recovery refused by the scratch trigger'"
            };
            conn.execute(&format!(
                "CREATE SEQUENCE public.recovery_attempt; \
                 CREATE FUNCTION public.recovery_stop() RETURNS event_trigger LANGUAGE plpgsql AS $$ \
                 BEGIN IF nextval('public.recovery_attempt') = 1 THEN \
                 RAISE EXCEPTION 'parked syntax verdict' USING ERRCODE = '42601'; END IF; \
                 {action}; END $$; CREATE EVENT TRIGGER recovery_stop ON ddl_command_start \
                 WHEN TAG IN ('CREATE VIEW') EXECUTE FUNCTION public.recovery_stop();"
            )).await.unwrap();
            let changes = [
                module("app.v", ModuleKind::View, "SELECT 1 AS n"),
                table("1"),
            ];
            let mut r = build(&pg, &changes, &Default::default(), false);
            assert_eq!(r.steps[0].module.as_ref(), Some(&"app.v".parse().unwrap()));
            let compiled = compile(&mut r, &pg, &mut conn).await;
            drop(conn);
            let mut probe = Conn::connect(Driver::Postgres, &format!("{server} dbname={name}"))
                .await
                .unwrap();
            let attempts = probe
                .query("SELECT last_value FROM public.recovery_attempt")
                .await
                .unwrap()[0]
                .try_get_at::<i64>(0)
                .unwrap();
            let absent = probe
                .query("SELECT to_regclass('app.t') IS NULL AS absent")
                .await
                .unwrap()[0]
                .try_get::<bool>("absent")
                .unwrap();
            drop(probe);
            remove(&mut admin, &name).await;
            assert_eq!(
                attempts,
                Some(2),
                "the backend reached the recovery attempt"
            );
            assert_eq!(absent, Some(true), "the failed range was rolled back");
            assert!(r.committed.is_empty() && r.observations().is_none());
            if killed {
                assert!(
                    matches!(compiled, Err(ReconstructError::Interrupted { .. })),
                    "{compiled:?}"
                );
            } else {
                let Err(ReconstructError::Refused(report)) = compiled else {
                    panic!("{compiled:?}")
                };
                assert_eq!(
                    report.pending[0]
                        .last
                        .as_ref()
                        .unwrap()
                        .error
                        .server_error_code()
                        .as_deref(),
                    Some("42601")
                );
                assert_eq!(
                    report.pending[0]
                        .recovery
                        .as_ref()
                        .unwrap()
                        .error
                        .server_error_code()
                        .as_deref(),
                    Some("P0001")
                );
            }
        }
    }
}
