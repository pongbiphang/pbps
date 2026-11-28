pub(super) fn final_order_refusal() -> pbps_diff::order::finish::FinishError {
    use pbps_diff::Side;
    use pbps_diff::order::{
        deployment::{Sources, schedule},
        finish,
    };
    use pbps_model::{Change, Module, ModuleKind, Schema};
    let dialect = pbps_pg::Postgres::new();
    let empty = Schema::default();
    let ids = Default::default();
    let mut schema = Schema::default();
    for id in ["app.first", "app.second"] {
        schema.modules.insert(
            id.parse().unwrap(),
            Module {
                kind: ModuleKind::View,
                definition: "SELECT 1".into(),
                description: None,
                options: Default::default(),
            },
        );
    }
    let before = Side {
        schema: &empty,
        ids: &ids,
    };
    let after = Side {
        schema: &schema,
        ids: &ids,
    };
    let draft = pbps_diff::diff(before, after, &dialect, &Default::default()).unwrap();
    let plan = schedule::schedule(
        finish::standalone(draft),
        &schedule::Declarations {
            schema: &schema,
            sources: Sources {
                before,
                after: &ids,
            },
            annotations: &Default::default(),
        },
        &Default::default(),
        &dialect,
    )
    .unwrap()
    .changes;
    let [first, second] = plan.changes().changes.as_slice() else {
        panic!("the fixture has two independent modules");
    };
    let module = |change: &Change| change.module_id().unwrap().clone();
    let modules = pbps_model::ModuleDeps::from([(
        module(&first.change),
        [module(&second.change)].into_iter().collect(),
    )]);
    finish::offline(
        plan,
        finish::Inputs {
            before,
            after,
            modules: &modules,
        },
        &dialect,
    )
    .err()
    .expect("the added stated leg runs backward")
}
