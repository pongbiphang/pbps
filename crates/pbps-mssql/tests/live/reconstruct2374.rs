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
    case_collated(tag, None, run).await;
}

/// The same, on a database whose catalog has the given collation: it decides
/// which names bind, which pbps cannot read offline.
async fn case_collated<F, Fut>(tag: &str, collation: Option<&str>, run: F)
where
    F: FnOnce(Conn) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let server = std::env::var("PBPS_TEST_DB").unwrap();
    let name = format!("pbps2374_{tag}_{}", std::process::id());
    let mut admin = connect_live(&server).await.unwrap();
    let collate = collation.map_or(String::new(), |c| format!(" COLLATE {c}"));
    admin
        .execute(&format!("CREATE DATABASE [{name}]{collate};"))
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
            options: Default::default(),
        }),
    }
}

fn reconstruction(
    changes: Vec<Change>,
    deps: &ModuleDeps,
) -> Result<Reconstruction, ReconstructError> {
    // The identities the declarations record: a split default names its column.
    let mut declared = pbps_model::Schema::default();
    for change in &changes {
        if let Change::CreateTable { name, table, .. } = change {
            declared.tables.insert(name.clone(), (**table).clone());
        }
    }
    let ids = pbps_diff::resolve(
        &declared,
        &IdsFile::default(),
        &[],
        &pbps_diff::Context {
            operator: "reconstruct-live-tests".into(),
            today: "2026-10-25".into(),
        },
    )
    .unwrap()
    .ids;
    let plan = pbps_diff::order::propose(ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    });
    Reconstruction::new(&Mssql, &plan, &ids, deps)
}

async fn compile(conn: &mut Conn, changes: Vec<Change>) -> Result<(), ReconstructError> {
    compile_with(conn, changes, &ModuleDeps::new()).await
}

async fn compile_with(
    conn: &mut Conn,
    changes: Vec<Change>,
    deps: &ModuleDeps,
) -> Result<(), ReconstructError> {
    let mut reconstruction = reconstruction(changes, deps)?;
    let len = reconstruction.len();
    reconstruction.compile_steps(conn, 0..len).await
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
async fn a_check_over_a_hash_named_computed_column_compiles_after_the_column() {
    case("hashed", |mut conn| async move {
        compile(
            &mut conn,
            vec![
                table("t", |t| {
                    t.computed.insert(
                        "c#d".into(),
                        ComputedColumn {
                            persisted: true,
                            ..computed("app.f(v)")
                        },
                    );
                    t.checks.insert(
                        "ck".into(),
                        pbps_model::CheckConstraint {
                            expression: "[c#d] > 0".into(),
                        },
                    );
                }),
                module(ModuleKind::Function, "f", FUNCTION),
            ],
        )
        .await
        .unwrap();
        let sql = "SELECT COUNT(*) FROM sys.check_constraints WHERE parent_object_id = OBJECT_ID(N'app.t')";
        assert_eq!(count(&mut conn, sql).await, 1);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_seed_row_beside_a_split_computed_column_is_skipped_and_compiles() {
    case("seeded", |mut conn| async move {
        compile(
            &mut conn,
            vec![
                table("t", |t| {
                    t.columns.get_mut("id").unwrap().nullable = false;
                    t.primary_key = Some(pbps_model::PrimaryKey {
                        name: Some("pk_id".into()),
                        columns: vec!["id".into()],
                        storage_parameters: Default::default(),
                    });
                    t.computed.insert("c".into(), computed("app.f(v)"));
                }),
                module(ModuleKind::Function, "f", FUNCTION),
                Change::InsertRow {
                    table: TableName::new("app", "t"),
                    key_column: "id".into(),
                    identity_key: false,
                    key: pbps_model::RowKey::from("1"),
                    defaults: Default::default(),
                    types: Default::default(),
                    row: Default::default(),
                },
            ],
        )
        .await
        .unwrap();
        let sql = "SELECT COUNT(*) FROM sys.columns WHERE object_id = OBJECT_ID(N'app.t') AND name = N'c'";
        assert_eq!(count(&mut conn, sql).await, 1);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_check_naming_a_column_by_an_accent_insensitive_equal_compiles_and_keeps_positions() {
    // On a CI_AI catalog `cafe` binds `[café]` (measured), so the check must
    // follow the column; on the target created inline the positions are these.
    case_collated("accent", Some("Latin1_General_100_CI_AI"), |mut conn| async move {
        let declare = |t: &mut Table| {
            t.computed.insert(
                "café".into(),
                ComputedColumn {
                    persisted: true,
                    ..computed("app.f(v)")
                },
            );
            t.checks.insert(
                "ck".into(),
                pbps_model::CheckConstraint {
                    expression: "cafe > 0".into(),
                },
            );
        };
        compile(
            &mut conn,
            vec![
                table("t", declare),
                module(ModuleKind::Function, "f", FUNCTION),
            ],
        )
        .await
        .unwrap();
        conn.execute(
            "CREATE TABLE app.target (id int, v int, [café] AS (app.f(v)) PERSISTED NOT NULL, CONSTRAINT ck_target CHECK (cafe > 0));",
        )
        .await
        .unwrap();
        let positions = async |conn: &mut Conn, table: &str| -> Vec<(String, i32)> {
            conn.query(&format!(
                "SELECT name, column_id FROM sys.columns WHERE object_id = OBJECT_ID(N'app.{table}') ORDER BY column_id"
            ))
            .await
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row.try_get::<&str>("name").unwrap().unwrap().to_owned(),
                    row.try_get::<i32>("column_id").unwrap().unwrap(),
                )
            })
            .collect()
        };
        assert_eq!(
            positions(&mut conn, "t").await,
            positions(&mut conn, "target").await
        );
        let sql = "SELECT COUNT(*) FROM sys.check_constraints WHERE parent_object_id = OBJECT_ID(N'app.t')";
        assert_eq!(count(&mut conn, sql).await, 1);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_check_naming_a_column_by_a_letter_the_fold_leaves_alone_compiles() {
    // On a CI_AI catalog `lodz` binds `[łódź]` though NFKD leaves ł alone
    // (measured), so the check must follow the column.
    case_collated("unresolved", Some("Latin1_General_100_CI_AI"), |mut conn| async move {
        compile(
            &mut conn,
            vec![
                table("t", |t| {
                    t.computed.insert(
                        "łódź".into(),
                        ComputedColumn {
                            persisted: true,
                            ..computed("app.f(v)")
                        },
                    );
                    t.checks.insert(
                        "ck".into(),
                        pbps_model::CheckConstraint {
                            expression: "lodz > 0".into(),
                        },
                    );
                }),
                module(ModuleKind::Function, "f", FUNCTION),
            ],
        )
        .await
        .unwrap();
        let sql = "SELECT COUNT(*) FROM sys.check_constraints WHERE parent_object_id = OBJECT_ID(N'app.t')";
        assert_eq!(count(&mut conn, sql).await, 1);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_view_reading_a_column_by_a_mapped_letter_compiles_after_the_column() {
    // `lodz` binds `[łódź]` on a CI_AI catalog, and a view binds its columns
    // when it is created, so it must follow the column.
    case_collated(
        "mapped_read",
        Some("Latin1_General_100_CI_AI"),
        |mut conn| async move {
            compile(
                &mut conn,
                vec![
                    module(ModuleKind::View, "v", "SELECT lodz FROM app.t"),
                    table("t", |t| {
                        t.computed.insert("łódź".into(), computed("app.f(v)"));
                    }),
                    module(ModuleKind::Function, "f", FUNCTION),
                ],
            )
            .await
            .unwrap();
            let sql = "SELECT COUNT(*) FROM sys.views WHERE object_id = OBJECT_ID(N'app.v')";
            assert_eq!(count(&mut conn, sql).await, 1);
        },
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_call_by_a_mapped_letter_compiles_after_the_function() {
    // `app.lodz` binds the function `app.[łódź]` on a CI_AI catalog, so the
    // computed column calling it must follow the function.
    case_collated(
        "mapped_call",
        Some("Latin1_General_100_CI_AI"),
        |mut conn| async move {
            compile(
                &mut conn,
                vec![
                    table("t", |t| {
                        t.computed.insert("c".into(), computed("app.lodz(v)"));
                    }),
                    module(ModuleKind::Function, "łódź", FUNCTION),
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
        },
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_scalar_function_with_an_exists_over_a_top_wildcard_compiles_before_the_column_it_serves()
{
    case("exists_top", |mut conn| async move {
        compile(
            &mut conn,
            vec![
                table("t", |t| {
                    t.computed.insert("c".into(), computed("app.f(id)"));
                }),
                module(
                    ModuleKind::Function,
                    "f",
                    "(@x int) RETURNS int AS BEGIN RETURN (CASE WHEN EXISTS (SELECT TOP (1) * FROM app.t WHERE id = @x) THEN 1 ELSE 0 END); END",
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
        // Nothing declared makes `app.nowhere`, so the view waits for an
        // object no other declaration can supply: a stall that names it and
        // carries the engine's own sentence (DEC-2143.9).
        let ReconstructError::Stalled(stalled) = &refused else {
            panic!("a missing table stalls the view: {refused}");
        };
        assert!(
            stalled.contains("view app.v")
                && stalled.contains("nowhere")
                && stalled.contains("code: 208"),
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

/// `a AS (id + 1)` is declared before `c AS (f(id))`, and `f` reads `a`: it
/// stays in the CREATE, so bare table with `a`, then `f`, then `c` compiles,
/// and the columns are in declared order, as on the target.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_function_reading_one_computed_column_compiles_before_a_column_that_calls_it() {
    for (tag, functions_first) in [("per_a", false), ("per_b", true)] {
        case(tag, move |mut conn| async move {
            let table = table("t", |t| {
                t.computed.insert("a".into(), computed("id + 1"));
                t.computed.insert("c".into(), computed("app.f(id)"));
            });
            let changes = if functions_first {
                vec![reading("f", "a"), table]
            } else {
                vec![table, reading("f", "a")]
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

/// Names that look like reads or calls and are neither: the column and the
/// function that share a name, a column named like a function, a parameter
/// named like the column, and a function of the same leaf name in another
/// schema. Each has a valid order (measured on 17.0), and the reader and
/// call tests find it instead of reporting a cycle.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn names_that_only_look_like_a_read_or_a_call_make_no_cycle() {
    case("alike_a", |mut conn| async move {
        // The column `f` and the function `f`, which reads only `id`.
        compile(
            &mut conn,
            vec![
                table("t", |t| {
                    t.computed.insert("f".into(), computed("app.f(id)"));
                }),
                reading("f", "id"),
            ],
        )
        .await
        .unwrap();
    })
    .await;
    case("alike_b", |mut conn| async move {
        // `c AS (v + 1)` uses the column `v`; the function `app.v` is another thing.
        compile(
            &mut conn,
            vec![
                table("t", |t| {
                    t.computed.insert("c".into(), computed("v + 1"));
                }),
                reading("v", "c"),
            ],
        )
        .await
        .unwrap();
    })
    .await;
    case("alike_c", |mut conn| async move {
        // A parameter `@c` is no column `c`.
        compile(
            &mut conn,
            vec![
                table("t", |t| {
                    t.computed.insert("c".into(), computed("app.g(id)"));
                }),
                module(
                    ModuleKind::Function,
                    "g",
                    "(@c int) RETURNS int AS BEGIN RETURN (SELECT COUNT(id) FROM app.t WHERE id = @c); END",
                ),
            ],
        )
        .await
        .unwrap();
    })
    .await;
    case("alike_d", |mut conn| async move {
        // `c` calls `other.f`; `app.f` shares the leaf name and reads `c`.
        conn.execute("CREATE SCHEMA other;").await.unwrap();
        let other = Change::CreateModule {
            id: ModuleId::Routine(RoutineId::new(TableName::new("other", "f"), Vec::new())),
            module: Box::new(Module {
                kind: ModuleKind::Function,
                definition: FUNCTION.into(),
                description: None,
                options: Default::default(),
            }),
        };
        compile(
            &mut conn,
            vec![
                table("t", |t| {
                    t.computed.insert("c".into(), computed("other.f(id)"));
                }),
                reading("f", "c"),
                other,
            ],
        )
        .await
        .unwrap();
    })
    .await;
}

/// A mid-table computed column keeps its position. The model writes a table's
/// plain columns, then its computed ones in name order, and the split adds the
/// late ones in that order: the columns of the table the reconstruction makes
/// are the target's, position for position (`column_id` is compared as
/// reported). `b` calls a planned function and `a` before it does not, so `a`
/// stays in the CREATE, `b` and `c` are added in order after the function.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_mid_table_computed_column_keeps_the_target_s_column_positions() {
    case("positions", |mut conn| async move {
        let wanted = |t: &mut Table| {
            t.columns.insert("w".into(), int());
            t.computed.insert("a".into(), computed("id + 1"));
            t.computed.insert("b".into(), computed("app.f(id)"));
            t.computed.insert("c".into(), computed("v + 1"));
        };
        compile(
            &mut conn,
            vec![
                table("t", wanted),
                module(ModuleKind::Function, "f", FUNCTION),
            ],
        )
        .await
        .unwrap();
        // The target: the same declarations created inline, in one CREATE.
        conn.execute(
            "CREATE TABLE app.target (id int, v int, w int, a AS (id + 1), b AS (app.f(id)), c AS (v + 1));",
        )
        .await
        .unwrap();
        let columns = async |conn: &mut Conn, table: &str| -> Vec<(String, i32)> {
            conn.query(&format!(
                "SELECT name, column_id FROM sys.columns WHERE object_id = OBJECT_ID(N'app.{table}') ORDER BY column_id"
            ))
            .await
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row.try_get::<&str>("name").unwrap().unwrap().to_owned(),
                    row.try_get::<i32>("column_id").unwrap().unwrap(),
                )
            })
            .collect()
        };
        let scratch = columns(&mut conn, "t").await;
        assert_eq!(scratch, columns(&mut conn, "target").await);
        assert_eq!(
            scratch.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            ["id", "v", "w", "a", "b", "c"]
        );
    })
    .await;
}

// Creation trials (DEC-2143.9). A text only guesses what a module needs, so a
// group the engine refuses for a missing object waits for another's success.
// In each case the plan's own order puts the caller first, so the first
// attempt fails and only the engine's answer corrects it.

fn bound_view(id: &str, definition: &str) -> Change {
    let mut change = module(ModuleKind::View, id, definition);
    if let Change::CreateModule { module, .. } = &mut change {
        module.options.schemabinding = true;
    }
    change
}

async fn views_named(conn: &mut Conn, name: &str) -> i32 {
    count(
        conn,
        &format!("SELECT COUNT(*) FROM sys.views WHERE name = N'{name}'"),
    )
    .await
}

/// 208, the invalid object name of a view: `a_caller` reads `b_callee`, whose
/// text spells the column `a_caller` of a table aliased `app`, so each text
/// names the other and neither edge can be told from a real one.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_view_that_reads_a_view_its_text_only_names_is_created_after_it_on_208() {
    case("trial208", |mut conn| async move {
        compile(
            &mut conn,
            vec![
                table("t", |t| {
                    t.columns.insert("a_caller".into(), int());
                }),
                module(ModuleKind::View, "a_caller", "SELECT id FROM app.b_callee"),
                module(
                    ModuleKind::View,
                    "b_callee",
                    "SELECT app.a_caller AS id FROM app.t AS app",
                ),
            ],
        )
        .await
        .unwrap();
        assert_eq!(views_named(&mut conn, "a_caller").await, 1);
        assert_eq!(views_named(&mut conn, "b_callee").await, 1);
    })
    .await;
}

/// 4121, a function the engine cannot find: the same pair with a view that
/// calls a function whose body spells the view's name as an alias column.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_view_that_calls_a_function_its_text_only_names_is_created_after_it_on_4121() {
    case("trial4121", |mut conn| async move {
        compile(
            &mut conn,
            vec![
                table("t", |t| {
                    t.columns.insert("view_c".into(), int());
                }),
                module(ModuleKind::View, "view_c", "SELECT app.fn_d(1) AS id"),
                module(
                    ModuleKind::Function,
                    "fn_d",
                    "(@x int) RETURNS int AS BEGIN RETURN (SELECT TOP 1 app.view_c FROM app.t AS app) END",
                ),
            ],
        )
        .await
        .unwrap();
        assert_eq!(views_named(&mut conn, "view_c").await, 1);
    })
    .await;
}

/// 4513 is the engine's refusal of a schema-bound caller of a function that is
/// not schema-bound. Measured on 17.0 it needs the callee to exist, and a
/// callee that exists does not change in a bootstrap, so no retry can cure
/// it: the group is deferred like the other classes and the pass that makes
/// no progress refuses by name with the engine's sentence.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_schema_bound_view_over_an_unbound_function_is_deferred_and_refused_on_4513() {
    case("trial4513", |mut conn| async move {
        let refused = compile(
            &mut conn,
            vec![
                table("t", |_| {}),
                module(
                    ModuleKind::Function,
                    "plain_f",
                    "(@x int) RETURNS int AS BEGIN RETURN @x END",
                ),
                bound_view("bound_v", "SELECT app.plain_f(id) AS x FROM app.t"),
            ],
        )
        .await
        .unwrap_err();
        let ReconstructError::Stalled(stalled) = &refused else {
            panic!("4513 is deferred, then refused as a stall: {refused}");
        };
        assert!(
            stalled.contains("view app.bound_v") && stalled.contains("code: 4513"),
            "{refused}"
        );
        assert_eq!(views_named(&mut conn, "bound_v").await, 0);
    })
    .await;
}

/// A syntax error is a verdict on the declaration. It is surfaced at once and
/// never retried, whatever else is still waiting.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn an_error_that_is_not_a_missing_object_is_not_retried() {
    case("trial_syntax", |mut conn| async move {
        let refused = compile(
            &mut conn,
            vec![
                table("t", |_| {}),
                module(ModuleKind::View, "bad", "SELECT id FROM"),
                module(ModuleKind::View, "good", "SELECT id FROM app.t"),
            ],
        )
        .await
        .unwrap_err();
        let ReconstructError::Compile {
            declaration,
            reason,
        } = &refused
        else {
            panic!("a syntax error is a verdict on the view: {refused}");
        };
        assert!(
            declaration == "view app.bad" && !reason.contains("code: 208"),
            "{refused}"
        );
    })
    .await;
}

/// A stated edge is hard. `b` would compile at once, but it declares that it
/// needs `a`, which can never be created, so a trial never tries it.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_stated_edge_is_never_overtaken_by_a_trial() {
    case("trial_stated", |mut conn| async move {
        let mut deps = ModuleDeps::new();
        deps.insert(
            ModuleId::Named(name("b")),
            [ModuleId::Named(name("a"))].into(),
        );
        let refused = compile_with(
            &mut conn,
            vec![
                table("t", |_| {}),
                module(ModuleKind::View, "a", "SELECT i FROM app.nowhere"),
                module(ModuleKind::View, "b", "SELECT id FROM app.t"),
            ],
            &deps,
        )
        .await
        .unwrap_err();
        let ReconstructError::Stalled(stalled) = &refused else {
            panic!("`a` never compiles, so the pass stalls: {refused}");
        };
        assert!(
            stalled.contains("view app.a") && stalled.contains("view app.b (waits for view app.a)"),
            "{refused}"
        );
        assert_eq!(views_named(&mut conn, "b").await, 0, "{refused}");
    })
    .await;
}

/// Two views that really read each other have no order: the pass that makes
/// no progress refuses, and names both.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn two_views_that_really_read_each_other_are_refused_naming_both() {
    case("trial_cycle", |mut conn| async move {
        let refused = compile(
            &mut conn,
            vec![
                table("t", |_| {}),
                module(ModuleKind::View, "m1", "SELECT id FROM app.m2"),
                module(ModuleKind::View, "m2", "SELECT id FROM app.m1"),
            ],
        )
        .await
        .unwrap_err();
        let ReconstructError::Stalled(stalled) = &refused else {
            panic!("a real cycle is a stall: {refused}");
        };
        assert!(
            stalled.contains("view app.m1") && stalled.contains("view app.m2"),
            "{refused}"
        );
    })
    .await;
}

/// The function that only names the computed column it is called by (through
/// an alias, or a table's own column of that name) used to be read as reading
/// it and refused; the engine says it does not, and it compiles.
#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_function_that_only_names_the_column_calling_it_compiles_on_the_engine() {
    for (tag, text) in [
        (
            "alias_a",
            "SELECT COUNT(o.c) AS c FROM app.other AS o JOIN app.t ON 1 = 1",
        ),
        ("alias_b", "SELECT id AS c FROM app.t"),
    ] {
        case(tag, move |mut conn| async move {
            conn.execute("CREATE TABLE app.other (c int);")
                .await
                .unwrap();
            compile(
                &mut conn,
                vec![
                    table("t", |t| {
                        t.computed.insert("c".into(), computed("app.g(id)"));
                    }),
                    module(
                        ModuleKind::Function,
                        "g",
                        &format!(
                            "(@x int) RETURNS int AS BEGIN RETURN (SELECT COUNT(*) FROM ({text}) AS q); END"
                        ),
                    ),
                ],
            )
            .await
            .unwrap_or_else(|refused| panic!("{text}: {refused}"));
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
}
