use super::*;
#[path = "../../../pbps-diff/tests/fixtures/creation2469.rs"]
pub(crate) mod fixture;
use fixture::Fixture;

fn observed(plan: &ChangeSet, schema: &pbps_model::Schema) -> Alike {
    Alike::from_pairs(
        added_call_spellings(plan, schema, &pbps_mssql::Mssql)
            .into_iter()
            .map(|s| (s.clone(), s)),
    )
}
#[test]
fn only_checked_current_annotation_order_discharges_the_class_order_guard() {
    let mut f = Fixture::new("generation", "mssql");
    f.schema
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .computed
        .get_mut("g")
        .unwrap()
        .expression = "app.f()".into();
    let plan = f.compile(&pbps_mssql::Mssql).unwrap();
    let facts = observed(&plan, &f.schema);
    assert!(refuse_added_calls_in(&plan, &f.schema, &pbps_mssql::Mssql, &facts).is_ok());
    assert!(
        refuse_added_calls_in(&plan, &f.schema, &pbps_mssql::Mssql, &Alike::default()).is_err()
    );
    assert!(
        f.compile_from(
            &Default::default(),
            &Default::default(),
            &Default::default(),
            &pbps_mssql::Mssql
        )
        .is_err()
    );
    let mut methods = f.schema.clone();
    methods
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .columns
        .insert(
            "app".into(),
            pbps_model::Column::new("geography".parse().unwrap()),
        );
    assert!(refuse_added_calls_in(&plan, &methods, &pbps_mssql::Mssql, &facts).is_err());
    let input = pbps_diff::order::propose(plan.changes().clone())
        .with_annotations(
            pbps_model::operation::Snapshot {
                schema: &Default::default(),
                ids: &Default::default(),
            },
            pbps_model::operation::Snapshot {
                schema: &f.schema,
                ids: &f.ids,
            },
            &Default::default(),
            &f.deps,
            pbps_model::operation::ModuleEdits::InPlace,
        )
        .unwrap();
    let mut reverse = pbps_diff::order::Schedule::new(input.changes().changes.len());
    let provider = input
        .changes()
        .changes
        .iter()
        .position(|p| matches!(p.change, Change::CreateModule { .. }))
        .unwrap();
    let owner = input
        .changes()
        .changes
        .iter()
        .position(|p| matches!(p.change, Change::AddComputedColumn { .. }))
        .unwrap();
    reverse.edge(
        owner,
        provider,
        pbps_diff::order::Source::Catalog,
        pbps_model::resolver::OrderReason::Binding,
    );
    assert!(reverse.settle(input).is_err());
}
#[test]
fn a_changed_alias_or_a_removed_function_never_gains_the_new_capability() {
    let mut f = Fixture::new("generation", "mssql");
    f.schema
        .tables
        .get_mut(&"app.t".parse().unwrap())
        .unwrap()
        .computed
        .get_mut("g")
        .unwrap()
        .expression = "app.f()".into();
    let plan = f.compile(&pbps_mssql::Mssql).unwrap();
    let mut steps = plan.changes().clone();
    let module = steps
        .changes
        .iter()
        .find_map(|p| {
            if let Change::CreateModule { module, .. } = &p.change {
                Some(module.clone())
            } else {
                None
            }
        })
        .unwrap();
    steps.changes.push(PlannedChange::new(Change::CreateModule {
        id: "app.F".parse().unwrap(),
        module,
    }));
    let mut alike = observed(&steps, &f.schema);
    alike.0.insert(("f".into(), "F".into()));
    let owner = steps
        .changes
        .iter()
        .position(|p| matches!(p.change, Change::AddComputedColumn { .. }))
        .unwrap();
    assert!(
        refuse_added_calls_with(
            &steps,
            &f.schema,
            &pbps_mssql::Mssql,
            &alike,
            &[(owner, "app.f".parse().unwrap())].into()
        )
        .is_err()
    );
    let mut removed = plan.changes().clone();
    let install = removed
        .changes
        .iter_mut()
        .find(|p| matches!(p.change, Change::CreateModule { .. }))
        .unwrap();
    install.change = Change::DropModule {
        id: "app.f".parse().unwrap(),
        kind: pbps_model::ModuleKind::Function,
    };
    assert!(
        refuse_added_calls(
            &removed,
            &f.schema,
            &pbps_mssql::Mssql,
            &observed(&removed, &f.schema)
        )
        .is_err()
    );
}
