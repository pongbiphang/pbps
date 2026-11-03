//! A plan that creates an object a runtime-bound module's names will resolve
//! to reports that module's binding change, and one an unmanaged module would
//! bind refuses by name (#2143 c2, DEC-2143.7).

use super::reconstruct2374::{both, declared, exec, module, table};
use super::*;
use pbps_model::ModuleKind;
use std::pin::Pin;

const BODY: &str = "AS SELECT 1 AS id FROM ghost";

/// The target holds a managed procedure whose body names `ghost` with one
/// part, so it binds when it runs; no object holds the name yet.
async fn procedure_over_nothing(target: &str, extra: &[&str]) -> Declared {
    let mut statements = vec!["CREATE PROCEDURE dbo.p2143 AS SELECT 1 AS id FROM ghost;"];
    statements.extend_from_slice(extra);
    exec(target, &statements).await;
    let mut declared = declared(&[module(ModuleKind::Procedure, "dbo.p2143", BODY)]);
    declared.base = declared.desired.clone();
    declared
}

fn creates(declared: &mut Declared, name: &str) {
    let (name, table) = table(name);
    declared.desired.tables.insert(name, table);
}

/// Case 1: the table the plan creates takes the name the managed procedure
/// binds at run time. The procedure is reported, not rebuilt.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_an_object_the_plan_creates_that_captures_a_reference_is_reported() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let mut declared = procedure_over_nothing(target, &[]).await;
            creates(&mut declared, "dbo.ghost");
            let (plan, _) = declared_plan(target, scratch, &declared, None)
                .await
                .unwrap_or_else(|error| panic!("the run should seal a plan: {error}"));
            let limitations = &plan.evidence.qualification().limitations;
            let reported: Vec<_> = limitations
                .iter()
                .filter(|(name, _)| name.raw().starts_with("binding-change:"))
                .collect();
            assert_eq!(reported.len(), 1, "{limitations:?}");
            assert_eq!(reported[0].0.raw(), "binding-change:dbo.p2143");
            assert!(reported[0].1.raw().contains("dbo.ghost"));
            assert_eq!(
                plan.changes.changes().changes.len(),
                1,
                "the table is the plan's only change: no statement for the procedure"
            );
            pbps_mssql::resolver::validate_evidence(&plan.evidence).unwrap();
        })
    }
    both(case).await;
}

/// A module the plan creates binds afresh, so a name it creates beside it is
/// no change to report, and no claim that the module receives no statement.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_a_module_the_plan_creates_beside_the_object_is_not_reported_as_rebound() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let mut declared = declared(&[module(ModuleKind::Procedure, "dbo.new2143", BODY)]);
            creates(&mut declared, "dbo.ghost");
            let (plan, _) = declared_plan(target, scratch, &declared, None)
                .await
                .unwrap_or_else(|error| panic!("the run should seal a plan: {error}"));
            assert!(
                plan.evidence
                    .qualification()
                    .limitations
                    .keys()
                    .all(|name| !name.raw().starts_with("binding-change:")),
                "{:?}",
                plan.evidence.qualification().limitations
            );
            assert!(
                plan.changes.changes().changes.len() >= 2,
                "the plan creates both the module and the table"
            );
        })
    }
    both(case).await;
}

/// The negatives: a created name no module mentions reports nothing, and a
/// plan that creates nothing reports nothing.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_a_created_name_no_runtime_bound_module_uses_is_not_reported() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let mut declared = procedure_over_nothing(target, &[]).await;
            creates(&mut declared, "dbo.unrelated");
            let (plan, _) = declared_plan(target, scratch, &declared, None)
                .await
                .unwrap_or_else(|error| panic!("the run should seal a plan: {error}"));
            assert!(
                plan.evidence
                    .qualification()
                    .limitations
                    .keys()
                    .all(|name| !name.raw().starts_with("binding-change:")),
                "{:?}",
                plan.evidence.qualification().limitations
            );
        })
    }
    both(case).await;
}

/// An unmanaged module the target holds binds the created name too: the run
/// refuses and names the module and the object.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_an_unmanaged_module_that_would_bind_a_created_name_refuses_by_name() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let mut declared = procedure_over_nothing(
                target,
                &["CREATE PROCEDURE dbo.outside2143 AS SELECT 1 AS id FROM ghost;"],
            )
            .await;
            creates(&mut declared, "dbo.ghost");
            let Err(error) = declared_plan(target, scratch, &declared, None).await else {
                panic!("a plan sealed over an unmanaged module it would rebind");
            };
            let text = error.to_string();
            assert!(
                text.contains("outside2143") && text.contains("ghost"),
                "{text}"
            );
            // The same plan without the unmanaged module seals.
            exec(target, &["DROP PROCEDURE dbo.outside2143;"]).await;
            declared_plan(target, scratch, &declared, None)
                .await
                .unwrap_or_else(|error| panic!("the run should seal a plan: {error}"));
        })
    }
    both(case).await;
}

/// The engine's own lookup decides which unmanaged module a created name
/// reaches: a one-part reference from another schema falls back to `dbo`, and
/// a two-part reference reaches exactly the schema it spells.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_an_unmanaged_reference_is_matched_by_the_engines_own_lookup_path() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            // Two-part, spelled `app`: creating `dbo.ghost` does not reach it.
            let mut declared = procedure_over_nothing(
                target,
                &[
                    "CREATE SCHEMA app2143;",
                    "CREATE PROCEDURE dbo.spelled2143 AS SELECT 1 AS id FROM app2143.ghost;",
                ],
            )
            .await;
            creates(&mut declared, "dbo.ghost");
            declared_plan(target, scratch, &declared, None)
                .await
                .unwrap_or_else(|error| panic!("a spelled schema is exact: {error}"));
            // One-part from `app`: it falls back to `dbo`, so it is reached.
            exec(
                target,
                &["CREATE PROCEDURE app2143.fallback2143 AS SELECT 1 AS id FROM ghost;"],
            )
            .await;
            let Err(error) = declared_plan(target, scratch, &declared, None).await else {
                panic!("a plan sealed over a module whose fallback it takes");
            };
            assert!(error.to_string().contains("fallback2143"), "{error}");
        })
    }
    both(case).await;
}

/// A managed table the target and the plan both hold, with the id column the
/// fixtures use, so that the plan keeps it.
fn kept(declared: &mut Declared, name: &str) {
    let (name, mut table) = table(name);
    table.columns.get_mut("id").unwrap().nullable = false;
    declared.desired.tables.insert(name.clone(), table.clone());
    declared.base.tables.insert(name, table);
}

/// A resolved reference that a created object shadows: the module reads
/// `fb2143` from `dbo` by id, and a table of the same name in the module's own
/// schema becomes the first match on its path (DEC-619.2).
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_a_created_object_that_shadows_a_resolved_fallback_refuses_by_name() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            exec(
                target,
                &[
                    "CREATE SCHEMA app2143;",
                    "CREATE TABLE dbo.fb2143(id int NOT NULL);",
                    "CREATE PROCEDURE dbo.indbo2143 AS SELECT 1 AS id FROM fb2143;",
                ],
            )
            .await;
            let mut declared = declared(&[]);
            kept(&mut declared, "dbo.fb2143");
            creates(&mut declared, "app2143.fb2143");
            // A module in `dbo` already reads `dbo`: nothing shadows it.
            declared_plan(target, scratch, &declared, None)
                .await
                .unwrap_or_else(|error| panic!("a module in dbo is not shadowed: {error}"));
            // One in `app2143` reads the `dbo` fallback, which the new table
            // now precedes.
            exec(
                target,
                &["CREATE PROCEDURE app2143.shadow2143 AS SELECT 1 AS id FROM fb2143;"],
            )
            .await;
            let Err(error) = declared_plan(target, scratch, &declared, None).await else {
                panic!("a plan sealed over a module whose resolution it shadows");
            };
            let text = error.to_string();
            assert!(
                text.contains("shadow2143") && !text.contains("indbo2143"),
                "{text}"
            );
        })
    }
    both(case).await;
}

/// A plan that creates nothing takes no name: the roots the scratch
/// bootstrap creates again (the bootstrap plan creates every declared object)
/// are the ones the target already holds. An unmanaged module calling a kept
/// procedure by its one-part name must not refuse it.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_a_plan_that_creates_nothing_is_not_refused_for_the_roots_it_keeps() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            exec(
                target,
                &[
                    "CREATE PROCEDURE dbo.kept2143 AS SELECT 1 AS id;",
                    "CREATE PROCEDURE dbo.unmanaged2143 AS EXEC kept2143;",
                ],
            )
            .await;
            let mut declared = declared(&[module(
                ModuleKind::Procedure,
                "dbo.kept2143",
                "AS SELECT 1 AS id",
            )]);
            declared.base = declared.desired.clone();
            declared_plan(target, scratch, &declared, None)
                .await
                .unwrap_or_else(|error| panic!("a plan that creates nothing should seal: {error}"));
        })
    }
    both(case).await;
}

/// A call reaches the executing principal's default schema, then `dbo`: a
/// procedure created in `dbo` or in a principal's default schema is the first
/// match, one created in a schema no principal defaults to is not.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_an_unmanaged_call_is_matched_by_the_executing_principals_lookup_path() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let kept = procedure_over_nothing(
                target,
                &[
                    "CREATE SCHEMA off2143;",
                    "CREATE SCHEMA app2143;",
                    "CREATE PROCEDURE dbo.caller2143 AS EXEC ghostp2143;",
                ],
            )
            .await;
            let creating = |name: &str| {
                let (id, procedure) = module(ModuleKind::Procedure, name, "AS SELECT 1 AS id");
                let mut declared = Declared {
                    base: kept.base.clone(),
                    desired: kept.desired.clone(),
                };
                declared.desired.modules.insert(id, procedure);
                declared
            };
            // No principal defaults to `off2143`, and the call is in `dbo`.
            declared_plan(target, scratch, &creating("off2143.ghostp2143"), None)
                .await
                .unwrap_or_else(|error| panic!("a schema off every lookup path: {error}"));
            // `dbo` is always on the path.
            let Err(error) =
                declared_plan(target, scratch, &creating("dbo.ghostp2143"), None).await
            else {
                panic!("a plan sealed over a call it takes");
            };
            assert!(error.to_string().contains("caller2143"), "{error}");
            // So is the default schema of a principal that can run the call.
            exec(
                target,
                &["CREATE USER user2143 WITHOUT LOGIN WITH DEFAULT_SCHEMA = app2143;"],
            )
            .await;
            let Err(error) =
                declared_plan(target, scratch, &creating("app2143.ghostp2143"), None).await
            else {
                panic!("a plan sealed over a call in a principal's default schema");
            };
            assert!(error.to_string().contains("caller2143"), "{error}");
        })
    }
    both(case).await;
}
