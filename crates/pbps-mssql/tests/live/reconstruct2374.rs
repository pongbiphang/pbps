//! The reconstruction's statement order compiles on the engine (#2374), in
//! the shapes the ordinary plan order cannot write: a computed column that
//! names a declared function, and what stands over it. Each case compiles
//! on a database of its own as the administrator; the deployer's rights are
//! the CLI's live tests.

use super::connect_live;
use indexmap::IndexMap;
use pbps_db::Conn;
use pbps_model::{
    Change, ChangeSet, Column, ComputedColumn, IdsFile, Module, ModuleDeps, ModuleId, ModuleKind,
    PlannedChange, RoutineId, Table, TableName, Uid, UidKind,
};
use pbps_mssql::Mssql;
use pbps_mssql::resolver::reconstruct::{ReconstructError, Reconstruction};

async fn case<F, Fut>(tag: &str, run: F)
where
    F: FnOnce(Conn) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let server = std::env::var("PBPS_TEST_DB").unwrap();
    let name = format!("pbps2374_{tag}_{}", std::process::id());
    let mut admin = connect_live(&server).await.unwrap();
    admin
        .execute(&format!("CREATE DATABASE [{name}];"))
        .await
        .unwrap();
    let mut conn = connect_live(&format!("{server};Database={name}"))
        .await
        .unwrap();
    conn.execute("CREATE SCHEMA app;").await.unwrap();
    let result = tokio::spawn(run(conn)).await;
    admin
        .execute(&format!(
            "ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}];"
        ))
        .await
        .unwrap();
    result.unwrap();
}

fn name(table: &str) -> TableName {
    TableName::new("app", table)
}

fn int() -> Column {
    Column::new("int".parse().unwrap())
}

fn table(table: &str, spec: impl FnOnce(&mut Table)) -> Change {
    let mut made = Table {
        columns: IndexMap::from([("id".into(), int()), ("v".into(), int())]),
        ..Default::default()
    };
    spec(&mut made);
    Change::CreateTable {
        uid: Uid::derived(UidKind::Table, table, 0),
        name: name(table),
        table: Box::new(made),
        beside_default: None,
        moves_at_most: None,
    }
}

fn computed(expression: &str) -> ComputedColumn {
    ComputedColumn {
        expression: expression.into(),
        persisted: false,
        not_null: false,
    }
}

fn module(kind: ModuleKind, id: &str, definition: &str) -> Change {
    let id = match kind {
        ModuleKind::View => ModuleId::Named(name(id)),
        ModuleKind::Function | ModuleKind::Procedure | ModuleKind::Trigger => {
            ModuleId::Routine(RoutineId::new(name(id), Vec::new()))
        }
    };
    Change::CreateModule {
        id,
        module: Box::new(Module {
            kind,
            definition: definition.into(),
            description: None,
        }),
    }
}

fn reconstruction(changes: Vec<Change>) -> Reconstruction {
    let plan = pbps_diff::order::propose(ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    });
    Reconstruction::new(&Mssql, &plan, &IdsFile::default(), &ModuleDeps::new()).unwrap()
}

async fn compile(conn: &mut Conn, changes: Vec<Change>) -> Result<(), ReconstructError> {
    let reconstruction = reconstruction(changes);
    reconstruction
        .compile_steps(conn, 0..reconstruction.len())
        .await
}

async fn count(conn: &mut Conn, sql: &str) -> i32 {
    conn.query(sql).await.unwrap()[0]
        .try_get_at::<i32>(0)
        .unwrap()
        .unwrap()
}

const FUNCTION: &str = "(@x int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN @x + 1; END";

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_computed_column_over_a_declared_function_and_a_view_reading_it_compile() {
    case("computed", |mut conn| async move {
        // The inline form is what the ordinary order writes, and it fails.
        let inline = conn
            .execute("CREATE TABLE app.inline_t (id int, c AS app.f(id));")
            .await
            .unwrap_err();
        assert_eq!(inline.server_error_code().as_deref(), Some("4121"));
        compile(
            &mut conn,
            vec![
                table("t", |t| {
                    t.computed.insert("c".into(), computed("app.f(v)"));
                }),
                module(ModuleKind::View, "w", "SELECT id, c FROM app.t"),
                module(ModuleKind::Function, "f", FUNCTION),
            ],
        )
        .await
        .unwrap();
        assert_eq!(
            count(
                &mut conn,
                "SELECT COUNT(*) FROM sys.computed_columns WHERE object_id = OBJECT_ID(N'app.t')"
            )
            .await,
            1
        );
        assert_eq!(
            count(
                &mut conn,
                "SELECT COUNT(*) FROM sys.views WHERE name = N'w'"
            )
            .await,
            1
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_schema_bound_function_over_the_table_compiles_before_the_column_it_serves() {
    case("bound", |mut conn| async move {
        compile(
            &mut conn,
            vec![
                table("t", |t| {
                    t.computed.insert("c".into(), computed("app.f(id)"));
                }),
                module(
                    ModuleKind::Function,
                    "f",
                    "(@x int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN (SELECT COUNT(*) FROM app.t WHERE id = @x); END",
                ),
            ],
        )
        .await
        .unwrap();
        assert_eq!(
            count(
                &mut conn,
                "SELECT COUNT(*) FROM sys.computed_columns WHERE object_id = OBJECT_ID(N'app.t')"
            )
            .await,
            1
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn defaults_checks_and_keys_over_a_computed_column_compile_after_their_function() {
    case("expressions", |mut conn| async move {
        compile(
            &mut conn,
            vec![
                table("t", |t| {
                    t.columns.get_mut("v").unwrap().value_source =
                        pbps_model::ValueSource::Default("app.f(1)".into());
                    t.computed.insert("c".into(), computed("app.f(v)"));
                    t.checks.insert(
                        "ck".into(),
                        pbps_model::CheckConstraint {
                            expression: "app.f(v) > 0".into(),
                        },
                    );
                    t.unique.insert(
                        "uq_c".into(),
                        pbps_model::UniqueConstraint {
                            columns: vec!["c".into()],
                            storage_parameters: Default::default(),
                        },
                    );
                }),
                module(ModuleKind::Function, "f", FUNCTION),
            ],
        )
        .await
        .unwrap();
        for (what, sql) in [
            ("default", "SELECT COUNT(*) FROM sys.default_constraints WHERE parent_object_id = OBJECT_ID(N'app.t')"),
            ("check", "SELECT COUNT(*) FROM sys.check_constraints WHERE parent_object_id = OBJECT_ID(N'app.t')"),
            ("unique over the computed column", "SELECT COUNT(*) FROM sys.key_constraints WHERE parent_object_id = OBJECT_ID(N'app.t') AND type = 'UQ'"),
        ] {
            assert_eq!(count(&mut conn, sql).await, 1, "{what}");
        }
    })
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_declaration_that_does_not_compile_names_itself_and_leaves_the_connection_usable() {
    case("refused", |mut conn| async move {
        let refused = compile(
            &mut conn,
            vec![
                table("t", |_| {}),
                module(ModuleKind::View, "v", "SELECT i FROM app.nowhere"),
            ],
        )
        .await
        .unwrap_err();
        let ReconstructError::Compile {
            declaration,
            reason,
        } = &refused
        else {
            panic!("a missing table is a verdict on the view: {refused}");
        };
        assert!(
            declaration == "view app.v" && reason.contains("nowhere") && reason.contains("208"),
            "{refused}"
        );
        // What came before stands (autocommit), and the session is intact.
        assert_eq!(
            count(
                &mut conn,
                "SELECT COUNT(*) FROM sys.tables WHERE name = N't'"
            )
            .await,
            1
        );
        assert_eq!(count(&mut conn, "SELECT @@TRANCOUNT").await, 0);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_view_projecting_every_column_of_a_held_table_exposes_its_computed_column() {
    case("wildcard", |mut conn| async move {
        compile(
            &mut conn,
            vec![
                table("t", |t| {
                    t.computed.insert("c".into(), computed("app.f(v)"));
                }),
                module(ModuleKind::View, "s", "SELECT * FROM app.t"),
                module(ModuleKind::View, "a", "SELECT x.* FROM app.t AS x"),
                module(ModuleKind::View, "down", "SELECT id, c FROM app.s"),
                module(ModuleKind::View, "down_a", "SELECT c FROM app.a"),
                module(
                    ModuleKind::Function,
                    "tvf",
                    "() RETURNS TABLE AS RETURN SELECT * FROM app.t",
                ),
                module(ModuleKind::Function, "f", FUNCTION),
            ],
        )
        .await
        .unwrap();
        for (view, kind) in [("s", "view"), ("a", "view"), ("tvf", "function")] {
            assert_eq!(
                count(
                    &mut conn,
                    &format!(
                        "SELECT COUNT(*) FROM sys.columns WHERE object_id = OBJECT_ID(N'app.{view}') AND name = N'c'"
                    )
                )
                .await,
                1,
                "the {kind} {view} lists the computed column"
            );
        }
    })
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_scalar_function_with_an_exists_select_all_compiles_before_the_column_it_serves() {
    case("exists", |mut conn| async move {
        compile(
            &mut conn,
            vec![
                table("t", |t| {
                    t.computed.insert("c".into(), computed("app.f(id)"));
                }),
                module(
                    ModuleKind::Function,
                    "f",
                    "(@x int) RETURNS int AS BEGIN RETURN (CASE WHEN EXISTS (SELECT * FROM app.t WHERE id = @x) THEN 1 ELSE 0 END); END",
                ),
            ],
        )
        .await
        .unwrap();
        assert_eq!(
            count(
                &mut conn,
                "SELECT COUNT(*) FROM sys.computed_columns WHERE object_id = OBJECT_ID(N'app.t')"
            )
            .await,
            1
        );
    })
    .await;
}

fn persisted(expression: &str) -> ComputedColumn {
    ComputedColumn {
        expression: expression.into(),
        persisted: true,
        not_null: true,
    }
}

/// A key over a computed column, and a foreign key from one, compile in
/// either table order: the first CREATE cannot name a column that does not
/// exist yet (1911), and a foreign key before its own column or the key it
/// references fails 1769 and 1776. The key keeps its name and layout.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_primary_key_and_foreign_key_over_computed_columns_compile_in_either_table_order() {
    for (tag, child_first, clustered) in [
        ("keys_a", false, None),
        ("keys_b", true, None),
        ("keys_c", false, Some(pbps_model::Clustered::Heap)),
    ] {
        case(tag, move |mut conn| async move {
            let parent = table("parent", |t| {
                t.computed.insert("k".into(), persisted("app.f(v)"));
                t.primary_key = Some(pbps_model::PrimaryKey {
                    name: Some("pk_k".into()),
                    columns: vec!["k".into()],
                    storage_parameters: Default::default(),
                });
                t.clustered = clustered.clone();
            });
            // The key's own end alone: a computed column referencing a plain key.
            let plain = table("plain", |t| {
                t.columns.get_mut("id").unwrap().nullable = false;
                t.primary_key = Some(pbps_model::PrimaryKey {
                    name: Some("pk_id".into()),
                    columns: vec!["id".into()],
                    storage_parameters: Default::default(),
                });
            });
            let child = table("child", |t| {
                t.computed.insert("ref".into(), persisted("app.f(v)"));
                t.foreign_keys.insert(
                    "fk_inline".into(),
                    pbps_model::ForeignKey {
                        columns: vec!["ref".into()],
                        references_table: name("plain"),
                        references_columns: vec!["id".into()],
                        on_delete: Default::default(),
                        on_update: Default::default(),
                    },
                );
            });
            let mut changes = vec![parent, child, plain];
            if child_first {
                changes.reverse();
            }
            changes.push(Change::AddForeignKey {
                table: name("child"),
                name: "fk_child".into(),
                constraint: Box::new(pbps_model::ForeignKey {
                    columns: vec!["ref".into()],
                    references_table: name("parent"),
                    references_columns: vec!["k".into()],
                    on_delete: Default::default(),
                    on_update: Default::default(),
                }),
            });
            // The other end alone: a plain column referencing the computed key.
            changes.push(Change::AddForeignKey {
                table: name("child"),
                name: "fk_to_key".into(),
                constraint: Box::new(pbps_model::ForeignKey {
                    columns: vec!["id".into()],
                    references_table: name("parent"),
                    references_columns: vec!["k".into()],
                    on_delete: Default::default(),
                    on_update: Default::default(),
                }),
            });
            changes.push(module(ModuleKind::Function, "f", FUNCTION));
            compile(&mut conn, changes).await.unwrap();
            let layout = if clustered.is_some() {
                "NONCLUSTERED"
            } else {
                "CLUSTERED"
            };
            assert_eq!(
                count(
                    &mut conn,
                    &format!(
                        "SELECT COUNT(*) FROM sys.key_constraints k JOIN sys.indexes i \
                         ON i.object_id = k.parent_object_id AND i.index_id = k.unique_index_id \
                         WHERE k.name = N'pk_k' AND k.type = 'PK' AND i.type_desc = N'{layout}'"
                    )
                )
                .await,
                1,
                "{tag}: the key keeps its name and layout"
            );
            assert_eq!(
                count(
                    &mut conn,
                    "SELECT COUNT(*) FROM sys.foreign_keys WHERE name IN (N'fk_child', N'fk_to_key', N'fk_inline')"
                )
                .await,
                3,
                "{tag}"
            );
        })
        .await;
    }
}

fn reading(function: &str, column: &str) -> Change {
    module(
        ModuleKind::Function,
        function,
        &format!(
            "(@x int) RETURNS int AS BEGIN RETURN (SELECT COUNT({column}) FROM app.t WHERE id = @x); END"
        ),
    )
}

/// Each computed column is held on its own: `d AS (id + 1)` calls nothing, so
/// the function that reads it need not wait for `c`, which calls that
/// function. Bare table, `d`, `f`, `c` compiles; one group for both columns
/// asked `f` to follow a column that must follow `f`.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_function_reading_one_computed_column_compiles_before_a_column_that_calls_it() {
    for (tag, functions_first) in [("per_a", false), ("per_b", true)] {
        case(tag, move |mut conn| async move {
            let table = table("t", |t| {
                t.computed.insert("c".into(), computed("app.f(id)"));
                t.computed.insert("d".into(), computed("id + 1"));
            });
            let changes = if functions_first {
                vec![reading("f", "d"), table]
            } else {
                vec![table, reading("f", "d")]
            };
            compile(&mut conn, changes).await.unwrap();
            assert_eq!(
                count(
                    &mut conn,
                    "SELECT COUNT(*) FROM sys.computed_columns WHERE object_id = OBJECT_ID(N'app.t')"
                )
                .await,
                2,
                "{tag}"
            );
        })
        .await;
    }
}

/// A key over two computed columns follows both, whichever order the two
/// functions their columns call are declared in.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_key_over_two_computed_columns_follows_both_and_their_functions() {
    for (tag, functions) in [("two_a", ["f1", "f2"]), ("two_b", ["f2", "f1"])] {
        case(tag, move |mut conn| async move {
            let mut changes = vec![table("t", |t| {
                t.columns.get_mut("id").unwrap().nullable = false;
                t.computed.insert("c1".into(), persisted("app.f1(v)"));
                t.computed.insert("c2".into(), persisted("app.f2(v)"));
                t.unique.insert(
                    "uq_two".into(),
                    pbps_model::UniqueConstraint {
                        columns: vec!["c1".into(), "c2".into()],
                        storage_parameters: Default::default(),
                    },
                );
            })];
            changes.extend(functions.map(|name| module(ModuleKind::Function, name, FUNCTION)));
            compile(&mut conn, changes).await.unwrap();
            assert_eq!(
                count(
                    &mut conn,
                    "SELECT COUNT(*) FROM sys.key_constraints WHERE name = N'uq_two' AND type = 'UQ'"
                )
                .await,
                1,
                "{tag}"
            );
        })
        .await;
    }
}

/// A real cycle across two columns (each function reads the other's column)
/// has no order. The scratch route finds only a reader its own column
/// calls, so this one reaches the engine, which refuses it by naming the
/// function and its error: a refusal either way, never an accepted order.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_cycle_across_two_computed_columns_is_refused_naming_the_declaration() {
    case("cycle", |mut conn| async move {
        let refused = compile(
            &mut conn,
            vec![
                table("t", |t| {
                    t.computed.insert("c1".into(), computed("app.g1(id)"));
                    t.computed.insert("c2".into(), computed("app.g2(id)"));
                }),
                reading("g1", "c2"),
                reading("g2", "c1"),
            ],
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&refused, ReconstructError::Cycle(_))
                || matches!(&refused, ReconstructError::Compile { reason, .. } if reason.contains("207")),
            "no order compiles it, so something names it: {refused}"
        );
    })
    .await;
}
