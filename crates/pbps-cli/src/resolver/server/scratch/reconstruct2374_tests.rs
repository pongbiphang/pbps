//! The declarations compile on scratch, as the reproduced deployer (#2374).
//!
//! Every case runs on the warm separate-instance scratch fixture. A successful
//! run still ends in an error, the planning
//! boundary (#2143), so reaching exactly that error is the proof that the
//! compile and both captures passed; each negative names what refused.

use super::*;
use indexmap::IndexMap;
use pbps_model::{Column, Module, ModuleKind, Table};
use std::pin::Pin;

const PASSWORD: &str = "Pbps!Test12345";

type Case = for<'a> fn(&'a str, &'a str) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// A fresh target for the warm separate-instance scratch. The runner keeps
/// cases serial; the outer audit catches release leaks before recovering them.
pub(super) async fn both(case: Case) {
    let primary = server("PBPS_TEST_DB");
    let supplied = server("PBPS_TEST_MSSQL_SCRATCH_DB");
    let servers = [primary.clone(), supplied.clone()];
    warm2585::run(&[&servers[0], &servers[1]], async move {
        let (name, target) = create_database(&primary).await;
        let outcome = tokio::spawn(async move { case(&target, &supplied).await }).await;
        drop_database(&primary, &name).await;
        drop_logins(&primary, &name).await;
        outcome.unwrap();
    })
    .await
    .unwrap();
}

async fn run(target: &str, deployer: Option<&str>, scratch: &str, declared: &Declared) -> String {
    let target = deployer.map_or_else(
        || target.to_owned(),
        |login| format!("{target};User Id={login};Password={PASSWORD}"),
    );
    let outcome = Box::pin(declared_refusal(
        &target,
        scratch,
        declared,
        None,
        &provenance(),
    ))
    .await;
    outcome.to_string()
}

async fn run_with_file(target: &str, scratch: &str, declared: &Declared, file: &str) -> String {
    Box::pin(declared_refusal(
        target,
        scratch,
        declared,
        Some(file),
        &provenance(),
    ))
    .await
    .to_string()
}

/// The boundary a successful run ends at.
fn reached_the_boundary(outcome: &str) {
    assert!(
        outcome.contains("planning from evidence") && outcome.contains("#2143"),
        "{outcome}"
    );
}

fn int() -> Column {
    Column::new("int".parse().unwrap())
}

/// A view is named by itself; SQL Server's functions and procedures are
/// routines with no arguments, so `f()`: the capture refuses a function
/// declared under a view's identity.
fn module(kind: ModuleKind, id: &str, definition: &str) -> (pbps_model::ModuleId, Module) {
    let id = match kind {
        ModuleKind::View => id.to_owned(),
        ModuleKind::Function | ModuleKind::Procedure | ModuleKind::Trigger => format!("{id}()"),
    };
    (
        id.parse().unwrap(),
        Module {
            kind,
            description: None,
            options: Default::default(),
            definition: definition.into(),
        },
    )
}

fn declared(modules: &[(pbps_model::ModuleId, Module)]) -> Declared {
    let mut desired = Schema::default();
    desired.modules.extend(modules.iter().cloned());
    Declared {
        base: Schema::default(),
        desired,
    }
}

fn table(name: &str) -> (pbps_model::TableName, Table) {
    (
        name.parse().unwrap(),
        Table {
            columns: IndexMap::from([("id".into(), int())]),
            ..Table::default()
        },
    )
}

async fn exec(url: &str, statements: &[&str]) {
    let mut conn = connection(url).await;
    for statement in statements {
        conn.execute(statement).await.unwrap();
    }
}

/// A login and its user in the target database, with the rights the
/// declarations need and no more than each case names. The resolver reads
/// the target as this principal, so it may read the dependency catalog.
async fn deployer(server_url: &str, target: &str, purpose: &str, user_setup: &str) -> String {
    let database = sql::database(&mut connection(target).await)
        .await
        .unwrap()
        .name;
    let login = format!("dep2374_{database}_{purpose}");
    exec(
        server_url,
        &[&format!(
            "CREATE LOGIN [{login}] WITH PASSWORD=N'{PASSWORD}', CHECK_POLICY=OFF;"
        )],
    )
    .await;
    exec(
        target,
        &[
            &format!("CREATE USER [{login}] FOR LOGIN [{login}] {user_setup};"),
            &format!("GRANT VIEW DEFINITION TO [{login}];"),
            &format!("GRANT SELECT ON sys.sql_expression_dependencies TO [{login}];"),
        ],
    )
    .await;
    login
}

async fn drop_logins(server_url: &str, database: &str) {
    let prefix = format!("dep2374_{database}_");
    let mut admin = connection(server_url).await;
    let leftover = admin
        .query("SELECT name FROM sys.server_principals;")
        .await
        .unwrap();
    for row in &leftover {
        let login = row.try_get::<&str>("name").unwrap().unwrap().to_owned();
        if !login.starts_with(&prefix) {
            continue;
        }
        admin
            .execute(&format!("DROP LOGIN [{login}];"))
            .await
            .unwrap();
    }
}

/// Where `CREATE TYPE t_probe` lands for the connection's principal: the
/// schema the engine names for a one-part type, which is the principal's
/// default schema and no schema the statement or the table spells.
const PROBE: &str = "CREATE TYPE t_probe2374 FROM int;";
const PROBE_SCHEMA: &str =
    "SELECT SCHEMA_NAME(schema_id) AS home FROM sys.types WHERE name = N't_probe2374';";

async fn schema_of_probe(conn: &mut Conn) -> String {
    conn.execute(PROBE).await.unwrap();
    let rows = conn.query(PROBE_SCHEMA).await.unwrap();
    assert_eq!(rows.len(), 1);
    rows[0].try_get::<&str>("home").unwrap().unwrap().to_owned()
}

/// The prepared scratch's compile connection is the reproduced deployer, so
/// a one-part type it creates lands where the target's own deployer's does.
async fn on_scratch(target: &str, scratch: &str) -> String {
    use super::super::super::ScopeRequest;
    let mut target_conn = connection(target).await;
    let declared = Declared::default();
    let bootstrap = pbps_diff::diff(
        pbps_diff::Side {
            schema: &declared.desired,
            ids: &IdsFile::default(),
        },
        pbps_diff::Side {
            schema: &declared.desired,
            ids: &IdsFile::default(),
        },
        &pbps_mssql::Mssql,
        &Hints::default(),
    )
    .unwrap();
    let binding = BindingRequest {
        dependencies: &Default::default(),
        bootstrap: &bootstrap,
        base: &declared.base,
        desired: &declared.desired,
    };
    let schemas: Vec<String> = vec!["app".into(), "other".into()];
    let request = ScopeRequest {
        schemas: schemas.clone(),
        ..Default::default()
    };
    let tokens = super::super::Tokens::generate();
    let policy = provenance();
    let identity = sql::cluster_identity(&mut target_conn).await.unwrap();
    let mut admin = connection(scratch).await;
    let (provisioning, mut state) = super::super::engine::scratch(
        Driver::Mssql,
        &mut target_conn,
        &mut admin,
        &tokens,
        &identity,
        scratch,
        &request,
        &Standard::Mssql,
        None,
        &policy,
    )
    .await
    .unwrap();
    let (catalog, authorization) = crate::resolver::scope::read_target(
        &mut target_conn,
        Driver::Mssql,
        &schemas,
        &[],
        &schemas,
        &[],
    )
    .await
    .unwrap();
    let prepared = state
        .prepare(
            provisioning,
            &mut admin,
            &mut target_conn,
            &catalog,
            &authorization,
            &schemas,
            &binding,
            &[],
            Some(&crate::resolver::server::scratch::mssql_live_tests::capture_key()),
        )
        .await;
    let landed = async {
        let super::super::engine::Prepared::Mssql(mut prepared) = prepared? else {
            panic!("wrong engine");
        };
        Ok::<String, Error>(schema_of_probe(&mut prepared.compile).await)
    }
    .await;
    state.release(&mut admin).await.unwrap();
    landed.unwrap()
}

/// Case 16. A one-part type name binds in the deployer's default schema, so
/// scratch must reproduce it. Two deployers hold identical rights and differ
/// in the default schema only: each lands where it does on the target, and the
/// two land apart, so a scratch that fixed one schema for both would fail.
#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_scratch_reproduces_the_deployers_default_schema() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let primary = server("PBPS_TEST_DB");
            let home = deployer(&primary, target, "app", "WITH DEFAULT_SCHEMA = [app]").await;
            exec(
                target,
                &[&format!("CREATE SCHEMA app AUTHORIZATION [{home}]")],
            )
            .await;
            let other = deployer(&primary, target, "other", "WITH DEFAULT_SCHEMA = [other]").await;
            exec(
                target,
                &[
                    "CREATE SCHEMA other",
                    &format!("GRANT CREATE TYPE TO [{home}], [{other}]"),
                    &format!("GRANT ALTER ON SCHEMA::other TO [{other}]"),
                    &format!("GRANT REFERENCES ON SCHEMA::other TO [{other}]"),
                    &format!("GRANT ALTER ON SCHEMA::app TO [{other}]"),
                    &format!("GRANT REFERENCES ON SCHEMA::app TO [{other}]"),
                ],
            )
            .await;
            let mut landed = Vec::new();
            for login in [home.as_str(), other.as_str()] {
                let url = format!("{target};User Id={login};Password={PASSWORD}");
                let mut on_target = connection(&url).await;
                let there = schema_of_probe(&mut on_target).await;
                on_target.execute("DROP TYPE t_probe2374;").await.unwrap();
                drop(on_target);
                let here = on_scratch(&url, scratch).await;
                assert_eq!(here, there, "{login}");
                landed.push(here);
            }
            assert_eq!(landed, ["app", "other"]);
        })
    }
    both(case).await;
}

/// A managed view over an external table compiles, whether the file or the
/// renderer stages the table; neither is a refusal naming both objects.
#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_managed_view_over_an_external_table_compiles_and_a_missing_one_refuses() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            // The file stages the table; the target holds the same one.
            exec(target, &["CREATE SCHEMA ext", "CREATE TABLE ext.x(i int)"]).await;
            let by_file = run_with_file(
                target,
                scratch,
                &declared(&[module(
                    ModuleKind::View,
                    "dbo.v_file",
                    "SELECT i FROM ext.x",
                )]),
                "CREATE SCHEMA ext AUTHORIZATION dbo;\nGO\nCREATE TABLE ext.x(i int);",
            )
            .await;
            reached_the_boundary(&by_file);
            // The renderer stages it: the target already holds a managed view
            // over it, so the table is a standing external no file describes.
            exec(target, &["CREATE VIEW dbo.v_render AS SELECT i FROM ext.x"]).await;
            let mut rendered = declared(&[module(
                ModuleKind::View,
                "dbo.v_render",
                "SELECT i, i + 1 AS j FROM ext.x",
            )]);
            rendered.base.modules.insert(
                "dbo.v_render".parse().unwrap(),
                Module {
                    kind: ModuleKind::View,
                    description: None,
                    options: Default::default(),
                    definition: "SELECT i FROM ext.x".into(),
                },
            );
            reached_the_boundary(&run(target, None, scratch, &rendered).await);
            // Neither the file nor the target holds the table.
            let missing = run(
                target,
                None,
                scratch,
                &declared(&[module(
                    ModuleKind::View,
                    "dbo.v_missing",
                    "SELECT i FROM ext.nowhere",
                )]),
            )
            .await;
            assert!(
                missing.contains("view dbo.v_missing") && missing.contains("ext.nowhere"),
                "{missing}"
            );
        })
    }
    both(case).await;
}

/// A statement the deployer may not run is refused on scratch as on the
/// target, with the engine's own error: 262 for a view it may not create and
/// 2760 for a table in a schema it may not alter.
#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_a_statement_the_deployer_may_not_run_refuses_on_scratch_as_on_the_target() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let primary = server("PBPS_TEST_DB");
            exec(target, &["CREATE SCHEMA locked", "CREATE SCHEMA mine"]).await;
            let login = deployer(&primary, target, "limited", "WITH DEFAULT_SCHEMA = [mine]").await;
            exec(
                target,
                &[
                    &format!("GRANT ALTER ON SCHEMA::mine TO [{login}]"),
                    &format!("GRANT CREATE TABLE TO [{login}]"),
                ],
            )
            .await;
            let mut on_target =
                connection(&format!("{target};User Id={login};Password={PASSWORD}")).await;
            let (view, in_locked) = (
                on_target
                    .execute("CREATE VIEW mine.v AS SELECT 1 AS a;")
                    .await
                    .unwrap_err(),
                on_target
                    .execute("CREATE TABLE locked.t(id int);")
                    .await
                    .unwrap_err(),
            );
            assert_eq!(view.server_error_code().as_deref(), Some("262"));
            assert_eq!(in_locked.server_error_code().as_deref(), Some("2760"));
            let view_refused = run(
                target,
                Some(&login),
                scratch,
                &declared(&[module(ModuleKind::View, "mine.v", "SELECT 1 AS a")]),
            )
            .await;
            assert!(
                view_refused.contains("view mine.v") && view_refused.contains("(code: 262"),
                "{view_refused}"
            );
            let mut locked = Declared::default();
            locked.desired.tables.extend([table("locked.t")]);
            let table_refused = run(target, Some(&login), scratch, &locked).await;
            assert!(
                table_refused.contains("table locked.t") && table_refused.contains("(code: 2760"),
                "{table_refused}"
            );
        })
    }
    both(case).await;
}

/// Declarations that depend on each other, in an order the engine accepts
/// only one way round: a schema-bound function over a table, and a view over
/// both. The same declarations as the deployer who may run them reach the
/// boundary, so the refusals above are the deployer's and nothing else's.
#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_declarations_the_deployer_may_run_compile_in_dependency_order() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let mut wanted = declared(&[
                module(
                    ModuleKind::View,
                    "dbo.w",
                    "SELECT id, dbo.f(id) AS n FROM dbo.t",
                ),
                module(
                    ModuleKind::Function,
                    "dbo.f",
                    "(@x int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN (SELECT COUNT(*) FROM dbo.t WHERE id = @x); END",
                ),
            ]);
            wanted.desired.tables.extend([table("dbo.t")]);
            reached_the_boundary(&run(target, None, scratch, &wanted).await);
        })
    }
    both(case).await;
}

/// A module only the target holds, one the plan drops, binds names scratch
/// never reads. Its candidates must still be in the scope both captures are
/// sealed under, or the plan that drops it is refused by the capture.
#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn mssql_a_module_the_plan_drops_does_not_refuse_the_capture() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            exec(
                target,
                &[
                    "CREATE TABLE dbo.t_dropped2374(i int)",
                    "CREATE PROCEDURE dbo.p_dropped2374 AS SELECT i FROM t_dropped2374",
                ],
            )
            .await;
            let mut dropped = Declared::default();
            let (id, procedure) = module(
                ModuleKind::Procedure,
                "dbo.p_dropped2374",
                "AS SELECT i FROM t_dropped2374",
            );
            dropped.base.modules.insert(id, procedure);
            let outcome = run(target, None, scratch, &dropped).await;
            reached_the_boundary(&outcome);
        })
    }
    both(case).await;
}

#[tokio::test]
#[ignore = "needs the owned SQL Server resolver fixtures"]
async fn a_warm_case_removes_only_its_own_deployer_logins() {
    let primary = server("PBPS_TEST_DB");
    warm2585::run(&[&primary], async {}).await.unwrap();
    let baseline = format!("dep2374_another_case_{:032x}", rand::random::<u128>());
    exec(
        &primary,
        &[&format!(
            "CREATE LOGIN [{baseline}] WITH PASSWORD=N'{PASSWORD}',CHECK_POLICY=OFF;"
        )],
    )
    .await;
    let (database, target) = create_database(&primary).await;
    let owned = deployer(&primary, &target, "owned", "").await;
    drop_database(&primary, &database).await;
    drop_logins(&primary, &database).await;
    let rows = connection(&primary)
        .await
        .query("SELECT name FROM sys.server_principals;")
        .await
        .unwrap();
    let names = rows
        .iter()
        .map(|row| row.try_get::<&str>("name").unwrap().unwrap().to_owned())
        .collect::<Vec<_>>();
    for login in [&baseline, &owned] {
        if names.contains(login) {
            exec(&primary, &[&format!("DROP LOGIN [{login}];")]).await;
        }
    }
    assert!(
        names.contains(&baseline),
        "another case's deployer was swept"
    );
    assert!(
        !names.contains(&owned),
        "the owned deployer was not released"
    );
}
