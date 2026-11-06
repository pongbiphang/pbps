//! A resolver plan orders schema-bound modules by the bindings the run read
//! from the engine, not by the lexer's guess at their text (#2143 c3a,
//! DEC-2143.8).

use super::reconstruct2374::{both, declared, exec, int, module};
use super::*;
use pbps_model::{Change, ModuleKind};
use std::pin::Pin;

const FUNCTION: &str = "() RETURNS int WITH SCHEMABINDING AS BEGIN \
     RETURN (SELECT COUNT(*) FROM dbo.t2143) END";

fn bound_view() -> (pbps_model::ModuleId, pbps_model::Module) {
    let (id, mut view) = module(ModuleKind::View, "dbo.v2143", "SELECT dbo.f2143() AS x");
    view.options.schemabinding = true;
    (id, view)
}

/// Case 24 through the resolver: the engine refuses to alter a function a
/// schema-bound view is bound to, so editing the function takes the view
/// down first and builds it again after, though the view is not edited. The
/// plan is ordered from the bindings the run captured, and the dependents
/// pass that follows the resolver reads the same ones.
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_a_changed_schema_bound_callee_is_rebuilt_around_by_its_bound_caller() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            exec(
                target,
                &[
                    "CREATE TABLE dbo.t2143(a int NOT NULL);",
                    &format!("CREATE FUNCTION dbo.f2143{FUNCTION};"),
                    "CREATE VIEW dbo.v2143 WITH SCHEMABINDING AS SELECT dbo.f2143() AS x;",
                ],
            )
            .await;
            let (function_id, function) = module(ModuleKind::Function, "dbo.f2143", FUNCTION);
            let (view_id, view) = bound_view();
            let mut declared = declared(&[
                (function_id.clone(), function.clone()),
                (view_id.clone(), view),
            ]);
            let mut table = pbps_model::Table::default();
            table.columns.insert("a".into(), int());
            declared
                .desired
                .tables
                .insert("dbo.t2143".parse().unwrap(), table);
            declared.base = declared.desired.clone();
            let mut edited = function;
            edited.definition = edited.definition.replace("COUNT(*)", "COUNT(*) + 1");
            declared.desired.modules.insert(function_id.clone(), edited);
            let (plan, _) = declared_plan(target, scratch, &declared, None)
                .await
                .unwrap_or_else(|error| panic!("the run should seal a plan: {error}"));
            let at = |wanted: &dyn Fn(&Change) -> bool| {
                plan.changes
                    .changes()
                    .changes
                    .iter()
                    .position(|planned| wanted(&planned.change))
            };
            let drop_view = at(&|c| matches!(c, Change::DropModule { id, .. } if id == &view_id));
            let alter = at(&|c| matches!(c, Change::AlterModule { id, .. } if id == &function_id));
            let create_view =
                at(&|c| matches!(c, Change::CreateModule { id, .. } if id == &view_id));
            let (Some(drop_view), Some(alter), Some(create_view)) = (drop_view, alter, create_view)
            else {
                panic!(
                    "the view is taken down and built again: {:?}",
                    plan.changes.changes()
                );
            };
            assert!(
                drop_view < alter && alter < create_view,
                "{drop_view} {alter} {create_view}"
            );
        })
    }
    both(case).await;
}

/// A schema-bound function reads a column of a table aliased `dbo`, spelled
/// `dbo.v2143` beside a view of that name that calls the function. The order
/// scratch settles creates the view first and the engine refuses it (4121);
/// the trial cursor creates the function and then the view, and the run seals
/// a plan that creates the function first (c3b, DEC-2143.9).
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_an_alias_column_spelled_like_a_view_is_no_call_of_it() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            exec(target, &["CREATE TABLE dbo.t2143(v2143 int NOT NULL);"]).await;
            let (function_id, function) = module(
                ModuleKind::Function,
                "dbo.f2143",
                "(@a int) RETURNS int WITH SCHEMABINDING AS BEGIN \
                 RETURN (SELECT TOP 1 dbo.v2143 FROM dbo.t2143 AS dbo) END",
            );
            let (view_id, mut view) =
                module(ModuleKind::View, "dbo.v2143", "SELECT dbo.f2143(1) AS x");
            view.options.schemabinding = true;
            let mut declared =
                declared(&[(function_id.clone(), function), (view_id.clone(), view)]);
            let mut table = pbps_model::Table::default();
            table.columns.insert("v2143".into(), int());
            declared
                .desired
                .tables
                .insert("dbo.t2143".parse().unwrap(), table);
            declared.base = declared.desired.clone();
            declared.base.modules.clear();
            let (plan, _) = declared_plan(target, scratch, &declared, None)
                .await
                .unwrap_or_else(|error| panic!("the run should seal a plan: {error}"));
            let at = |id: &pbps_model::ModuleId| {
                plan.changes.changes().changes.iter().position(|planned| {
                    matches!(&planned.change, Change::CreateModule { id: made, .. } if made == id)
                })
            };
            let (Some(function), Some(view)) = (at(&function_id), at(&view_id)) else {
                panic!("both are created: {:?}", plan.changes.changes());
            };
            assert!(function < view, "{function} {view}");
        })
    }
    both(case).await;
}

/// Negative: two modules that really read each other have no order on the
/// engine, however the guesses fall. The run refuses by name and seals nothing
/// (c3b, DEC-2143.9).
#[tokio::test]
#[ignore = "needs Docker and the owned SQL Server resolver fixtures"]
async fn mssql_two_modules_that_really_read_each_other_are_refused_by_name() {
    fn case<'a>(
        target: &'a str,
        scratch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let (first_id, first) = module(
                ModuleKind::View,
                "dbo.m1_2143",
                "SELECT 1 AS x FROM dbo.m2_2143",
            );
            let (second_id, second) = module(
                ModuleKind::View,
                "dbo.m2_2143",
                "SELECT 1 AS x FROM dbo.m1_2143",
            );
            let declared = declared(&[(first_id, first), (second_id, second)]);
            let Err(refused) = declared_plan(target, scratch, &declared, None).await else {
                panic!("no order creates both");
            };
            let refused = refused.to_string();
            assert!(
                refused.contains("m1_2143") && refused.contains("m2_2143"),
                "{refused}"
            );
        })
    }
    both(case).await;
}
