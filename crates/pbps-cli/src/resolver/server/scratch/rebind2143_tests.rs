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
