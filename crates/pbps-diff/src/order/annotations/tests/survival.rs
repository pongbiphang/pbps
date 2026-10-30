use super::*;
use pbps_model::operation::{ModuleEdits, Operations};

#[test]
fn provider_availability_uses_the_module_edit_policy_not_two_boundaries() {
    let mut f = Fixture::new();
    f.after
        .modules
        .insert("app.provider".parse().unwrap(), view());
    f.before = f.after.clone();
    f.base = f.ids.clone();
    f.after.modules.remove(&"app.f".parse().unwrap());
    let base = dependencies(&[("app.f", &["app.provider"])]);
    let changes = vec![
        Change::AlterModule {
            id: "app.provider".parse().unwrap(),
            module: Box::new(view()),
        },
        Change::DropModule {
            id: "app.f".parse().unwrap(),
            kind: ModuleKind::View,
        },
    ];
    for modules in [ModuleEdits::Rebuilt, ModuleEdits::InPlace] {
        for tracked in [false, true] {
            let mut plan = f
                .plan_with_modules(changes.clone(), &base, &ObjectDeps::default(), modules)
                .unwrap();
            if tracked {
                plan.identities = Some(Operations::of_plan(&plan.changes, modules));
            }
            let expected = if modules == ModuleEdits::Rebuilt {
                vec![(1, 0)]
            } else {
                Vec::new()
            };
            assert_eq!(pairs(&plan), expected, "{modules:?}, tracked={tracked}");
            let settled = Schedule::new(2).settle(plan).unwrap();
            assert_eq!(
                settled.sequence,
                if modules == ModuleEdits::Rebuilt {
                    [1, 0]
                } else {
                    [0, 1]
                }
            );
        }
    }
}

#[test]
fn in_place_availability_does_not_remove_install_or_consumer_release_edges() {
    let mut f = Fixture::new();
    f.after
        .modules
        .insert("app.provider".parse().unwrap(), view());
    f.before = f.after.clone();
    f.base = f.ids.clone();
    let deps = dependencies(&[("app.f", &["app.provider"])]);
    for modules in [ModuleEdits::InPlace, ModuleEdits::Rebuilt] {
        let plan = f
            .plan_with_modules(
                vec![
                    Change::AlterModule {
                        id: "app.provider".parse().unwrap(),
                        module: Box::new(view()),
                    },
                    Change::AlterModule {
                        id: "app.f".parse().unwrap(),
                        module: Box::new(view()),
                    },
                ],
                &ObjectDeps::default(),
                &deps,
                modules,
            )
            .unwrap();
        assert_eq!(pairs(&plan), [(0, 1)]);
    }
    // In-place consumers still release their old dependency. Only continuous
    // availability of the provider can suppress the baseline reverse edge.
    f.after.modules.remove(&"app.provider".parse().unwrap());
    let plan = f
        .plan_with_modules(
            vec![
                Change::DropModule {
                    id: "app.provider".parse().unwrap(),
                    kind: ModuleKind::View,
                },
                Change::AlterModule {
                    id: "app.f".parse().unwrap(),
                    module: Box::new(view()),
                },
            ],
            &deps,
            &ObjectDeps::default(),
            ModuleEdits::InPlace,
        )
        .unwrap();
    assert_eq!(pairs(&plan), [(1, 0)]);
}

#[test]
fn replacing_a_facet_does_not_prove_continuous_provider_availability() {
    for provider in [
        "module::app.provider",
        "table::app.t",
        "column::app.t.x",
        "default::app.t.x",
        "generation::app.t.x",
        "not_null::app.t.x",
    ] {
        let provider: ObjectRef = provider.parse().unwrap();
        let owner: ObjectRef = "app.f".parse().unwrap();
        let mut index = Index::default();
        index.record(provider.clone(), 0, Transition::Ends);
        index.record(provider.clone(), 0, Transition::Produces);
        index.record(owner.clone(), 1, Transition::Ends);
        let edges = index
            .project(&[Annotation { owner, provider }], &[])
            .unwrap();
        assert_eq!(
            edges
                .iter()
                .map(|e| (e.before, e.after))
                .collect::<Vec<_>>(),
            [(1, 0)]
        );
    }
    let mut f = Fixture::new();
    f.after
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .columns["x"]
        .value_source = pbps_model::ValueSource::Default("1".into());
    f.before = f.after.clone();
    f.base = f.ids.clone();
    f.after.modules.clear();
    f.after
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .columns["x"]
        .value_source = pbps_model::ValueSource::Default("2".into());
    let plan = f
        .plan_with_modules(
            vec![
                Change::AlterColumnDefault {
                    uid: "c_aaaaaa".parse().unwrap(),
                    column: "app.t.x".parse().unwrap(),
                    from: Some("1".into()),
                    to: Some("2".into()),
                },
                Change::DropModule {
                    id: "app.f".parse().unwrap(),
                    kind: ModuleKind::View,
                },
            ],
            &dependencies(&[("app.f", &["default::app.t.x"])]),
            &ObjectDeps::default(),
            ModuleEdits::InPlace,
        )
        .unwrap();
    assert_eq!(pairs(&plan), [(1, 0)]);
}

fn pairs(plan: &PlanningPlan) -> Vec<(usize, usize)> {
    plan.annotation_edges()
        .unwrap()
        .iter()
        .map(|e| (e.before, e.after))
        .collect()
}
