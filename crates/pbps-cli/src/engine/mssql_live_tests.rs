//! Connected function-call equality, through the real final-name pass (#2221).

use super::*;
use pbps_dialect::slots::{Namespace, SlotKey};
use pbps_diff::order::{self, Baseline};
use pbps_model::operation::{
    Incarnation, ModuleEdits, ObjectKey, Operations, PlanOwners, Requirement, Snapshot,
};
use pbps_model::{Change, Module, ModuleId, ModuleKind, PlannedChange};

#[tokio::test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB"]
async fn module_only_callers_use_catalog_collation_and_reversed_order_is_refused() {
    for (collation, managed, called, matches) in [
        ("Latin1_General_100_CI_AI", "café", "cafe", true),
        ("Latin1_General_100_CI_AS", "café", "cafe", false),
        ("Latin1_General_100_CI_AS", "cafe", "ｃａｆｅ", true),
        ("Latin1_General_100_CI_AS_WS", "cafe", "ｃａｆｅ", false),
        ("Latin1_General_100_CS_AS", "F", "f", false),
    ] {
        let server = std::env::var("PBPS_TEST_DB").expect("PBPS_TEST_DB");
        let mut conn = Conn::connect(Driver::Mssql, &server).await.unwrap();
        let db = format!("pbps_callees2221_{}_{}", std::process::id(), collation);
        conn.execute(&format!("CREATE DATABASE [{db}] COLLATE {collation}"))
            .await
            .unwrap();
        conn.execute(&format!("USE [{db}]")).await.unwrap();
        conn.execute("CREATE SCHEMA app").await.unwrap();
        let function = |body: &str| Module {
            kind: ModuleKind::Function,
            definition: body.into(),
            description: None,
            options: Default::default(),
        };
        let old = "(@x int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN @x END";
        let new = "(@x int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN @x + 1 END";
        let caller_body = format!(
            "(@x int) RETURNS int WITH SCHEMABINDING AS BEGIN RETURN app.[{called}](@x) END"
        );
        let callee = ModuleId::Named(TableName::new("app", managed));
        let caller = ModuleId::Named(TableName::new("app", "a"));
        let mut before = Schema::default();
        before.modules.insert(callee.clone(), function(old));
        conn.execute(&format!("CREATE FUNCTION app.[{managed}] {old}"))
            .await
            .unwrap();
        if !matches {
            // A valid sensitive-collation schema: the authored spelling calls
            // its own standing function, not the edited accent/width neighbour.
            before.modules.insert(
                ModuleId::Named(TableName::new("app", called)),
                function(old),
            );
            conn.execute(&format!("CREATE FUNCTION app.[{called}] {old}"))
                .await
                .unwrap();
        }
        let mut after = before.clone();
        after.modules.insert(callee.clone(), function(new));
        after.modules.insert(caller.clone(), function(&caller_body));
        let changes = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::CreateModule {
                    id: caller.clone(),
                    module: Box::new(function(&caller_body)),
                }),
                PlannedChange::new(Change::AlterModule {
                    id: callee.clone(),
                    module: Box::new(function(new)),
                }),
            ],
        };
        assert!(before.tables.is_empty() && after.tables.is_empty());
        let mut plan = order::propose(changes);
        let ids = IdsFile::default();
        let dependencies = Default::default();
        let declarations = crate::dependents::schedule::Declarations {
            schema: &after,
            sources: crate::dependents::Sources {
                before: pbps_diff::Side {
                    schema: &before,
                    ids: &ids,
                },
                after: &ids,
            },
            module_deps: &dependencies,
        };
        // This is the shipping connected pass. Neither computed-column read
        // runs; its existing object-name comparison must still cover calls.
        let facts = order_created_object_names(&mut conn, &mut plan, "test", &[], &declarations)
            .await
            .unwrap()
            .unwrap();
        let key = |name: &str| SlotKey {
            namespace: Namespace::Relations("app".into()),
            name: name.into(),
        };
        assert!(facts.compared.contains(&key(managed)) && facts.compared.contains(&key(called)));
        let ops = Operations::of_plan(&plan.changes, ModuleEdits::InPlace);
        let owners = PlanOwners::new(
            pbps_model::plan_names::PlanNames::of(&plan.changes, &ids).unwrap(),
            &ids,
            &ids,
        );
        let base = Snapshot {
            schema: &before,
            ids: &ids,
        };
        let end = Snapshot {
            schema: &after,
            ids: &ids,
        };
        let analysed =
            order::analyse_with_catalog(&ops, &owners, base, end, &pbps_mssql::Mssql, Some(&facts));
        assert!(analysed.unowned.is_empty());
        let g = ops
            .iter()
            .position(|op| matches!(op.change(), Change::CreateModule { id, .. } if id == &caller))
            .unwrap();
        let f = ops
            .iter()
            .position(|op| matches!(op.change(), Change::AlterModule { id, .. } if id == &callee))
            .unwrap();
        assert_eq!(
            f < g,
            matches,
            "caller-first input must be scheduled under {collation}"
        );
        let requirement = Requirement::Is(ObjectKey::Module(callee.clone()), Incarnation::Final);
        assert_eq!(
            analysed.effects[g].requires.contains(&requirement),
            matches,
            "{collation}"
        );
        let offline = order::analyse(&ops, &owners, base, end, &pbps_mssql::Mssql);
        assert!(
            !offline.effects[g].requires.contains(&requirement),
            "offline stays lexical"
        );
        let verify = |indices: &[usize]| {
            let ordered = order::reordered_operations(&ops, indices);
            let effects: Vec<_> = indices
                .iter()
                .map(|&i| analysed.effects[i].clone())
                .collect();
            order::verify(
                ordered.as_slice(),
                &effects,
                &Baseline::of(base, &owners),
                &Default::default(),
            )
        };
        assert!(verify(&[f, g]).is_empty());
        assert_eq!(!verify(&[g, f]).is_empty(), matches, "{collation}");
        // Also pin the final observer's delivery of the same facts, rather
        // than only a test that calls the fact-aware analysis directly.
        #[cfg(debug_assertions)]
        {
            let reversed = ChangeSet {
                changes: [g, f].map(|i| plan.changes[i].clone()).into(),
            };
            let refused = std::panic::catch_unwind(|| {
                order::shadow::observe_scheduled(
                    pbps_diff::Side {
                        schema: &before,
                        ids: &ids,
                    },
                    pbps_diff::Side {
                        schema: &after,
                        ids: &ids,
                    },
                    &pbps_mssql::Mssql,
                    &Default::default(),
                    &reversed,
                    order::shadow::Stage::Final,
                    Some(&facts),
                )
            })
            .is_err();
            assert_eq!(refused, matches, "{collation}");
        }
        conn.execute(&format!("CREATE FUNCTION app.a {caller_body}"))
            .await
            .unwrap();
        let altered = conn
            .execute(&format!("ALTER FUNCTION app.[{managed}] {new}"))
            .await;
        if matches {
            let error = altered.unwrap_err().to_string();
            assert!(error.contains("3729"), "{collation}: {error}");
        } else {
            altered.unwrap();
        }
        conn.execute("DROP FUNCTION app.a").await.unwrap();
        conn.execute(&format!("ALTER FUNCTION app.[{managed}] {new}"))
            .await
            .unwrap();
        conn.execute(&format!("CREATE FUNCTION app.a {caller_body}"))
            .await
            .unwrap();
        // A broken connection is an error, never an empty collation answer.
        conn.execute("RAISERROR ('2221 connection closed', 20, 1) WITH LOG")
            .await
            .unwrap_err();
        assert!(
            pbps_mssql::catalog::object_names_alike(
                &mut conn,
                &[
                    TableName::new("app", managed),
                    TableName::new("app", called)
                ],
            )
            .await
            .is_err()
        );
        assert!(
            order_created_object_names(&mut conn, &mut plan, "test", &[], &declarations)
                .await
                .is_err()
        );
        let mut cleanup = Conn::connect(Driver::Mssql, &server).await.unwrap();
        cleanup.execute(&format!("USE master; ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]")).await.unwrap();
    }
}

#[path = "mssql_live_tests/creation2469_tests.rs"]
mod creation2469;
