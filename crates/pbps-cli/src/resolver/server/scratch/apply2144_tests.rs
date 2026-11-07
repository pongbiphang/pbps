//! A resolved SQL Server plan is applied under its sealed checks (#2144,
//! DEC-2144.1): the key and the cluster before the lock, the opening manifest
//! before the first statement, the closing manifest before anything is
//! recorded. Each case seals a plan on the scratch fixture, then drives the
//! apply's own guard on the target alone, in the apply's own framing: apply
//! starts no resolver and opens no scratch.

use super::reconstruct2374::{both, declared, deployer, exec, module};
use super::*;
use crate::certified_fixture::FixtureEmission as _;
use crate::resolver::apply::Keyed;
use pbps_model::ModuleKind;
use std::pin::Pin;

const OPENING: &str = "before its first statement";
const CLOSING: &str = "after its last statement";
const APPLY_FRAMING: &str = "SET XACT_ABORT ON; BEGIN TRANSACTION;";
const ROLLBACK: &str = "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;";

/// The target holds a table and a managed view over it; the plan adds a
/// function, which is the plan's only change.
async fn target_with_a_view(target: &str) -> (Declared, pbps_model::ModuleId) {
    exec(
        target,
        &[
            "CREATE TABLE dbo.t2144(id int NOT NULL);",
            "CREATE VIEW dbo.v2144 AS SELECT id FROM t2144;",
        ],
    )
    .await;
    let (view_id, view) = module(ModuleKind::View, "dbo.v2144", "SELECT id FROM t2144");
    let mut declared = declared(&[(view_id, view)]);
    let (table_name, table) = super::reconstruct2374::table("dbo.t2144");
    declared.desired.tables.insert(table_name, table);
    declared.base = declared.desired.clone();
    let (function_id, function) = module(
        ModuleKind::Function,
        "dbo.f2144",
        "() RETURNS int AS BEGIN RETURN 1 END",
    );
    declared
        .desired
        .modules
        .insert(function_id.clone(), function);
    (declared, function_id)
}

/// The sealed plan, as the apply reads it back from its file.
async fn sealed(
    target: &str,
    scratch: &str,
    declared: &Declared,
) -> (pbps_model::SavedPlan, Begun) {
    sealed_with(target, scratch, declared, None).await
}

/// The same, with the externals file the objects the managed modules read
/// need on scratch: a synonym and a function have no rendered template.
async fn sealed_with(
    target: &str,
    scratch: &str,
    declared: &Declared,
    externals: Option<&str>,
) -> (pbps_model::SavedPlan, Begun) {
    let run = begin(target, declared).await;
    let (base, desired) = run.sides(declared);
    let plan = super::super::produce::<pbps_mssql::Mssql>(
        Driver::Mssql,
        scratch,
        target,
        &run.identity,
        &run.binding(declared),
        base,
        desired,
        &Hints::default(),
        &[],
        &run.key.project,
        Some(ENVIRONMENT),
        Standard::Mssql,
        externals,
        &provenance(),
    )
    .await
    .unwrap_or_else(|error| panic!("the run should seal a plan: {error}"));
    plan.evidence.validate(&plan.changes).unwrap();
    let saved = pbps_model::SavedPlan::new(
        pbps_model::PlanOrigin::Database,
        "mssql",
        "2026-11-07",
        pbps_model::PlanBaseline {
            description: "apply fixture".into(),
            checksum: String::new(),
            database_collation: None,
            time_zone: None,
        },
        plan.changes.changes().clone(),
        run.desired_ids.clone(),
    )
    .with_resolution(plan.evidence)
    .unwrap();
    let saved = serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
    (saved, run)
}

fn keyed<'a>(saved: &'a pbps_model::SavedPlan, run: &Begun) -> Keyed<'a> {
    Keyed::check(
        saved,
        crate::resolver::sealing::environment_key(&run.key.project, Some(ENVIRONMENT)),
        "issue-2144",
    )
    .unwrap()
    .expect("a resolved plan")
}

/// What the apply runs for the plan's changes, in its order.
fn statements(saved: &pbps_model::SavedPlan) -> Vec<String> {
    saved
        .changes
        .changes
        .iter()
        .flat_map(|planned| {
            pbps_mssql::Mssql
                .fixture_emit(&planned.change, planned.strategy)
                .unwrap()
        })
        .map(|statement| statement.sql)
        .collect()
}

async fn count(conn: &mut Conn, name: &str) -> i64 {
    conn.query(&format!(
        "SELECT CONVERT(bigint, COUNT(*)) AS n FROM sys.objects WHERE name = N'{name}';"
    ))
    .await
    .unwrap()[0]
        .try_get::<i64>("n")
        .unwrap()
        .unwrap()
}

/// Opens the guard on its own connection, as the apply does before its lock.
async fn open<'a>(keyed: &'a Keyed<'_>, conn: &mut Conn) -> crate::resolver::apply::Guard<'a> {
    keyed.open(conn).await.unwrap()
}

/// Case 8: a resolved plan applies on a target alone, and its checks pass at
/// both ends. The apply's guard takes the target's connection and nothing
/// else: no scratch URL, no Docker.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_a_resolved_plan_applies_without_docker() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let (declared, function) = target_with_a_view(target).await;
            let (saved, run) = sealed(target, scratch, &declared).await;
            let keyed = keyed(&saved, &run);
            let mut conn = connection(target).await;
            let guard = open(&keyed, &mut conn).await;
            conn.execute(APPLY_FRAMING).await.unwrap();
            guard.before_statements(&mut conn).await.unwrap();
            for sql in statements(&saved) {
                conn.execute(&sql).await.unwrap();
            }
            guard
                .before_recording(&mut conn, &declared.desired, &run.desired_ids)
                .await
                .unwrap();
            conn.execute("COMMIT;").await.unwrap();
            assert_eq!(count(&mut conn, "f2144").await, 1, "{function:?}");
        })
    }
    both(case).await;
}

/// The wrong-binding rollback: after planning, a second session creates the
/// table that a managed view's one-part name now resolves to first. The apply
/// refuses before its first statement and the target is unchanged. The same
/// arrival where no managed name looks is no refusal.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_a_name_that_arrived_after_planning_refuses_before_the_first_statement() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            exec(
                target,
                &[
                    "CREATE SCHEMA app2144;",
                    "CREATE SCHEMA elsewhere2144;",
                    "CREATE TABLE dbo.price2144(id int NOT NULL);",
                    "CREATE VIEW app2144.one_part AS SELECT id FROM price2144;",
                ],
            )
            .await;
            let (view_id, view) = module(
                ModuleKind::View,
                "app2144.one_part",
                "SELECT id FROM price2144",
            );
            let mut declared = declared(&[(view_id, view)]);
            let (table_name, table) = super::reconstruct2374::table("dbo.price2144");
            declared.desired.tables.insert(table_name, table);
            declared.base = declared.desired.clone();
            let (function_id, function) = module(
                ModuleKind::Function,
                "dbo.f2144",
                "() RETURNS int AS BEGIN RETURN 1 END",
            );
            declared.desired.modules.insert(function_id, function);
            let (saved, run) = sealed(target, scratch, &declared).await;
            let keyed = keyed(&saved, &run);
            let mut conn = connection(target).await;
            let guard = open(&keyed, &mut conn).await;

            // Off the path: another schema's table of the name captures nothing.
            exec(target, &["CREATE TABLE elsewhere2144.price2144(id int);"]).await;
            conn.execute(APPLY_FRAMING).await.unwrap();
            guard.before_statements(&mut conn).await.unwrap();
            conn.execute(ROLLBACK).await.unwrap();

            // On the path: the view's own schema is searched first.
            exec(target, &["CREATE TABLE app2144.price2144(id int);"]).await;
            conn.execute(APPLY_FRAMING).await.unwrap();
            let error = guard
                .before_statements(&mut conn)
                .await
                .expect_err("a captured name must refuse")
                .to_string();
            assert!(error.contains(OPENING), "{error}");
            assert!(error.contains("price2144"), "{error}");
            assert!(error.contains("Nothing has been applied"), "{error}");
            conn.execute(ROLLBACK).await.unwrap();
            assert_eq!(count(&mut conn, "f2144").await, 0, "nothing was applied");

            // The same arrival after the plan's last statement is found by the
            // closing check instead: a record the seal does not hold, and no
            // change of the plan owns it.
            exec(target, &["DROP TABLE app2144.price2144;"]).await;
            conn.execute(APPLY_FRAMING).await.unwrap();
            guard.before_statements(&mut conn).await.unwrap();
            for sql in statements(&saved) {
                conn.execute(&sql).await.unwrap();
            }
            exec(target, &["CREATE TABLE app2144.price2144(id int);"]).await;
            let error = guard
                .before_recording(&mut conn, &declared.desired, &run.desired_ids)
                .await
                .expect_err("an object nothing in the plan owns must refuse")
                .to_string();
            assert!(error.contains(CLOSING), "{error}");
            assert!(error.contains("price2144"), "{error}");
            conn.execute(ROLLBACK).await.unwrap();
            assert_eq!(
                count(&mut conn, "f2144").await,
                0,
                "the plan was rolled back"
            );
        })
    }
    both(case).await;
}

/// What the plan's views read, for scratch: the objects the target holds
/// and the plan does not manage.
const EXTERNALS: &str = "\
CREATE TABLE dbo.ta2144(id int NOT NULL, v decimal(10,2) NOT NULL);\n\
CREATE TABLE dbo.tb2144(id int NOT NULL, v decimal(10,2) NOT NULL);\n\
CREATE SYNONYM dbo.syn2144 FOR dbo.ta2144;\n\
GO\n\
CREATE FUNCTION dbo.one2144() RETURNS int AS BEGIN RETURN 1 END;\n";

/// Case 15: a property of what the plan's modules read, changed on the
/// target between planning and apply, refuses at the opening check and names
/// the object: a synonym retargeted, a function made schema-bound, a column's
/// precision. A change to an object outside everything the plan reads does not.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_an_external_property_change_invalidates_the_plan() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            exec(
                target,
                &[
                    "CREATE TABLE dbo.ta2144(id int NOT NULL, v decimal(10,2) NOT NULL);",
                    "CREATE TABLE dbo.tb2144(id int NOT NULL, v decimal(10,2) NOT NULL);",
                    "CREATE TABLE dbo.outside2144(id int NOT NULL);",
                    "CREATE SYNONYM dbo.syn2144 FOR dbo.ta2144;",
                    "CREATE FUNCTION dbo.one2144() RETURNS int AS BEGIN RETURN 1 END;",
                    "CREATE VIEW dbo.vs2144 AS SELECT id, v FROM dbo.syn2144;",
                    "CREATE VIEW dbo.vf2144 AS SELECT dbo.one2144() AS one;",
                ],
            )
            .await;
            let (synonym_view_id, synonym_view) = module(
                ModuleKind::View,
                "dbo.vs2144",
                "SELECT id, v FROM dbo.syn2144",
            );
            let (function_view_id, function_view) = module(
                ModuleKind::View,
                "dbo.vf2144",
                "SELECT dbo.one2144() AS one",
            );
            let mut declared = declared(&[
                (synonym_view_id, synonym_view),
                (function_view_id, function_view),
            ]);
            declared.base = declared.desired.clone();
            let (added_id, added) = module(
                ModuleKind::Function,
                "dbo.f2144",
                "() RETURNS int AS BEGIN RETURN 1 END",
            );
            declared.desired.modules.insert(added_id, added);
            let (saved, run) = sealed_with(target, scratch, &declared, Some(EXTERNALS)).await;
            let keyed = keyed(&saved, &run);
            let mut conn = connection(target).await;
            let guard = open(&keyed, &mut conn).await;

            conn.execute(APPLY_FRAMING).await.unwrap();
            // Outside everything the plan reads: no refusal.
            exec(target, &["ALTER TABLE dbo.outside2144 ADD extra int NULL;"]).await;
            guard.before_statements(&mut conn).await.unwrap();
            conn.execute(ROLLBACK).await.unwrap();

            for (what, change, named) in [
                (
                    "a synonym retargeted",
                    "DROP SYNONYM dbo.syn2144; CREATE SYNONYM dbo.syn2144 FOR dbo.tb2144;",
                    "syn2144",
                ),
                (
                    "a column's precision",
                    "ALTER TABLE dbo.tb2144 ALTER COLUMN v decimal(12,4) NOT NULL;",
                    "v(dbo.tb2144)",
                ),
            ] {
                exec(target, &[change]).await;
                conn.execute(APPLY_FRAMING).await.unwrap();
                let error = guard
                    .before_statements(&mut conn)
                    .await
                    .expect_err(what)
                    .to_string();
                assert!(error.contains(OPENING), "{what}: {error}");
                assert!(error.contains(named), "{what}: {error}");
                conn.execute(ROLLBACK).await.unwrap();
            }
            exec(
                target,
                &["ALTER FUNCTION dbo.one2144() RETURNS int WITH SCHEMABINDING AS BEGIN RETURN 1 END;"],
            )
            .await;
            conn.execute(APPLY_FRAMING).await.unwrap();
            let error = guard
                .before_statements(&mut conn)
                .await
                .expect_err("a function made schema-bound")
                .to_string();
            assert!(
                error.contains(OPENING) && error.contains("one2144"),
                "{error}"
            );
            conn.execute(ROLLBACK).await.unwrap();
            assert_eq!(count(&mut conn, "f2144").await, 0, "nothing was applied");
        })
    }
    both(case).await;
}

/// Case 22 and case 9: after the plan's last statement and before the closing
/// capture, another session's lasting change to what the plan read rolls the
/// apply back and leaves nothing of the plan; one made and restored is
/// outside what a plain read can see (DEC-619.1) and does not refuse.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_a_resolved_plan_rolls_back_when_its_closing_check_finds_another_writer() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let (declared, _) = target_with_a_view(target).await;
            let (saved, run) = sealed(target, scratch, &declared).await;
            let keyed = keyed(&saved, &run);
            let mut conn = connection(target).await;
            let guard = open(&keyed, &mut conn).await;

            // Case 9: made and restored between the opening and closing reads.
            conn.execute(APPLY_FRAMING).await.unwrap();
            guard.before_statements(&mut conn).await.unwrap();
            for sql in statements(&saved) {
                conn.execute(&sql).await.unwrap();
            }
            exec(
                target,
                &[
                    "CREATE TABLE dbo.price2144(v int);",
                    "DROP TABLE dbo.price2144;",
                ],
            )
            .await;
            guard
                .before_recording(&mut conn, &declared.desired, &run.desired_ids)
                .await
                .expect("a change restored before the closing read leaves nothing");
            conn.execute(ROLLBACK).await.unwrap();

            // Case 22: a lasting change to a column the view binds.
            conn.execute(APPLY_FRAMING).await.unwrap();
            guard.before_statements(&mut conn).await.unwrap();
            for sql in statements(&saved) {
                conn.execute(&sql).await.unwrap();
            }
            exec(
                target,
                &["ALTER TABLE dbo.t2144 ALTER COLUMN id bigint NOT NULL;"],
            )
            .await;
            let error = guard
                .before_recording(&mut conn, &declared.desired, &run.desired_ids)
                .await
                .expect_err("another writer's change must refuse")
                .to_string();
            assert!(error.contains(CLOSING), "{error}");
            assert!(error.contains("Nothing has been applied"), "{error}");
            conn.execute(ROLLBACK).await.unwrap();
            assert_eq!(
                count(&mut conn, "f2144").await,
                0,
                "the target holds nothing of the plan"
            );
        })
    }
    both(case).await;
}

/// Before the lock: evidence sealed under another key is no evidence; a
/// connection that reaches another instance is refused by the cluster check.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_another_key_or_another_instance_refuses_before_anything_is_read() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let (declared, _) = target_with_a_view(target).await;
            let (saved, run) = sealed(target, scratch, &declared).await;

            let other = ProjectKey::new(true);
            let error = Keyed::check(
                &saved,
                crate::resolver::sealing::environment_key(&other.project, Some(ENVIRONMENT)),
                "issue-2144",
            )
            .err()
            .expect("another key must refuse")
            .to_string();
            assert!(error.contains("fingerprint key"), "{error}");

            let keyed = keyed(&saved, &run);
            let mut elsewhere = connection(scratch).await;
            let error = keyed
                .open(&mut elsewhere)
                .await
                .err()
                .expect("another instance must refuse")
                .to_string();
            assert!(error.contains("another cluster"), "{error}");
            let mut here = connection(target).await;
            keyed
                .open(&mut here)
                .await
                .expect("the planned instance opens");
        })
    }
    both(case).await;
}

/// The deployer's authorization is part of what the plan was sealed against:
/// a role the deployer held when it planned and does not hold when it applies
/// refuses at the opening check, and holding it again does not.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_a_role_removed_after_planning_refuses_at_the_opening_check() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let primary = server("PBPS_TEST_DB");
            exec(target, &["CREATE SCHEMA app2144", "CREATE ROLE r2144"]).await;
            let login = deployer(
                &primary,
                target,
                "role2144",
                "WITH DEFAULT_SCHEMA = [app2144]",
            )
            .await;
            exec(
                target,
                &[
                    "GRANT ALTER ON SCHEMA::app2144 TO r2144",
                    "GRANT CREATE FUNCTION TO r2144",
                    "GRANT REFERENCES ON SCHEMA::app2144 TO r2144",
                    &format!("ALTER ROLE r2144 ADD MEMBER [{login}]"),
                ],
            )
            .await;
            let as_deployer = format!("{target};User Id={login};Password=Pbps!Test12345");
            let (function_id, function) = module(
                ModuleKind::Function,
                "app2144.f2144",
                "() RETURNS int AS BEGIN RETURN 1 END",
            );
            let mut declared = declared(&[]);
            declared.desired.modules.insert(function_id, function);
            let (saved, run) = sealed(&as_deployer, scratch, &declared).await;
            let keyed = keyed(&saved, &run);
            let mut conn = connection(&as_deployer).await;
            let guard = open(&keyed, &mut conn).await;

            conn.execute(APPLY_FRAMING).await.unwrap();
            guard
                .before_statements(&mut conn)
                .await
                .expect("the deployer still holds what it planned with");
            conn.execute(ROLLBACK).await.unwrap();

            exec(
                target,
                &[&format!("ALTER ROLE r2144 DROP MEMBER [{login}]")],
            )
            .await;
            conn.execute(APPLY_FRAMING).await.unwrap();
            let error = guard
                .before_statements(&mut conn)
                .await
                .expect_err("a role removed after planning must refuse")
                .to_string();
            assert!(error.contains(OPENING), "{error}");
            assert!(
                error.contains("authorization") || error.contains("roles"),
                "{error}"
            );
            conn.execute(ROLLBACK).await.unwrap();
        })
    }
    both(case).await;
}

/// Planning builds its authorization inventory with the function apply now
/// shares (DEC-2144.1), and what it seals is byte for byte what it sealed
/// before: the deployer's scope read is 3221 bytes and the inventory built
/// over it, which holds every schema the staging names, 6577. A change to
/// either number is a change to what every SQL Server plan seals, and moves
/// apply's recomputation with it unnoticed.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_the_planned_authorization_inventory_is_what_planning_sealed_before() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let (declared, _) = target_with_a_view(target).await;
            let Ok(unsealed) =
                declared_outcome(target, scratch, &declared, None, &provenance()).await
            else {
                panic!("the run should complete");
            };
            let crate::resolver::scope::Authorization::Mssql(read) =
                &unsealed.sealing.authorization
            else {
                panic!("a SQL Server run reads SQL Server's context");
            };
            assert_eq!(read.canonical().len(), 3221, "the scope read");
            let condition = unsealed
                .sealing
                .authorization_condition
                .as_ref()
                .expect("a SQL Server run measures its authorization condition");
            let before = String::from_utf8(condition.before().canonical()).unwrap();
            assert_eq!(
                before.len(),
                6577,
                "the inventory over the staging's schemas"
            );
            for schema in ["INFORMATION_SCHEMA", "db_owner", "dbo", "guest", "sys"] {
                assert!(
                    before.contains(&format!("\"{schema}\":\"present\"")),
                    "{schema}"
                );
            }
        })
    }
    both(case).await;
}

/// What the plan itself creates under names the engine chooses applies, and
/// its closing check passes: the target's generated names do not matter.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_a_plan_that_creates_unnamed_keys_and_defaults_applies_and_closes() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            use pbps_model::{Column, ForeignKey, PrimaryKey, ValueSource};
            let (parent_name, mut parent) = super::reconstruct2374::table("dbo.p2144");
            parent.primary_key = Some(PrimaryKey {
                name: None,
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            });
            let (child_name, mut child) = super::reconstruct2374::table("dbo.c2144");
            child
                .columns
                .insert("pid".into(), super::reconstruct2374::int());
            let mut defaulted = Column::new("int".parse().unwrap());
            defaulted.value_source = ValueSource::Default("5".into());
            child.columns.insert("v".into(), defaulted);
            child.primary_key = Some(PrimaryKey {
                name: None,
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            });
            child.foreign_keys.insert(
                "fk_c2144_p".into(),
                ForeignKey {
                    columns: vec!["pid".into()],
                    references_table: parent_name.clone(),
                    references_columns: vec!["id".into()],
                    on_delete: Default::default(),
                    on_update: Default::default(),
                },
            );
            for table in [&mut parent, &mut child] {
                table.columns.get_mut("id").unwrap().nullable = false;
            }
            child.columns.get_mut("pid").unwrap().nullable = false;
            let mut declared = declared(&[]);
            declared.desired.tables.insert(parent_name, parent);
            declared.desired.tables.insert(child_name, child);
            let (saved, run) = sealed(target, scratch, &declared).await;
            let keyed = keyed(&saved, &run);
            let mut conn = connection(target).await;
            let guard = open(&keyed, &mut conn).await;
            conn.execute(APPLY_FRAMING).await.unwrap();
            guard.before_statements(&mut conn).await.unwrap();
            for sql in statements(&saved) {
                conn.execute(&sql).await.unwrap();
            }
            guard
                .before_recording(&mut conn, &declared.desired, &run.desired_ids)
                .await
                .expect("the keys and the default the engine named are the plan's own");
            conn.execute("COMMIT;").await.unwrap();
            assert_eq!(count(&mut conn, "c2144").await, 1);
        })
    }
    both(case).await;
}
