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
