use super::*;
use pbps_model::operation::{
    AddressChain, ModuleEdits, Operations, Origin, PlanOwners, Snapshot, Source as LogicalSource,
    effects,
};
use pbps_model::plan_names::PlanNames;
use pbps_model::{Change, ChangeSet, IdsFile, PlannedChange, Schema, TableName, Uid};

#[test]
fn rename_dependencies_follow_consumed_versions_when_endpoint_names_are_equal() {
    let name = |s: &str| s.parse::<TableName>().unwrap();
    let uid: Uid = "t_aaaaaa".parse().unwrap();
    let before = Schema {
        tables: [(name("app.a"), Default::default())].into(),
        ..Default::default()
    };
    let ids = IdsFile {
        tables: [(uid.clone(), name("app.a"))].into(),
        ..Default::default()
    };
    let steps = ChangeSet {
        changes: vec![
            Change::RenameTable {
                uid: uid.clone(),
                from: name("app.a"),
                to: name("app.b"),
                defaults: vec![],
            },
            Change::RenameTable {
                uid,
                from: name("app.b"),
                to: name("app.a"),
                defaults: vec![],
            },
            Change::DropCheck {
                table: name("app.a"),
                name: "ck".into(),
            },
        ]
        .into_iter()
        .map(PlannedChange::new)
        .collect(),
    };
    let raw = Operations::expanded(
        3,
        steps.changes.iter().enumerate().map(|(at, p)| {
            (
                if at == 2 {
                    Origin::Teardown(LogicalSource::Change(at))
                } else {
                    Origin::Whole(at)
                },
                p.clone(),
            )
        }),
        ModuleEdits::Rebuilt,
    );
    let names = PlanNames::of(&steps.changes, &ids).unwrap();
    let chain = AddressChain::bind(
        &names,
        &raw,
        &ids,
        &ids,
        &Default::default(),
        &[raw.as_slice()[2].id()].into(),
    );
    let ops = raw.with_address_chain(chain);
    let owners = PlanOwners::new(names, &ids, &ids);
    let snapshot = Snapshot {
        schema: &before,
        ids: &ids,
    };
    let fx: Vec<_> = ops
        .iter()
        .map(|op| effects(op, &owners, snapshot, snapshot).unwrap())
        .collect();
    let graph = graph(
        ops.as_slice(),
        &fx,
        &Baseline::of(snapshot, &owners),
        &Default::default(),
        Checks::ALL,
    );
    assert!(graph.edges.iter().any(|e| e.before == 0 && e.after == 2));
    assert!(graph.edges.iter().any(|e| e.before == 1 && e.after == 2));
    assert!(
        !graph
            .edges
            .iter()
            .any(|e| e.before == 2 && (e.after == 0 || e.after == 1))
    );
    assert_eq!(
        ops.as_slice()[2].logical_origin(),
        &Origin::Teardown(LogicalSource::Change(2))
    );
}
