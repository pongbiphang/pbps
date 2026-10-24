//! The first expansion blocker: origins do not encode per-input read boundaries.
use pbps_diff::{
    Side,
    order::{self, Baseline, Checks, Schedule, Source},
};
use pbps_model::{operation::*, plan_names::PlanNames, *};

#[test]
fn one_split_origin_cannot_order_source_temp_and_backup_reads() {
    let dialect = pbps_pg::Postgres::new();
    let schema = pbps_diff::rebuild::member_cases::source();
    let parent = pbps_diff::rebuild::member_cases::root();
    let leaf = TableName::new("app", "p1");
    let ctx = pbps_diff::Context {
        operator: "maintainer".into(),
        today: "2026-10-24".into(),
    };
    let base = pbps_diff::resolve(&schema, &IdsFile::default(), &[], &ctx)
        .unwrap()
        .ids;
    let mut desired = schema.clone();
    desired
        .tables
        .get_mut(&parent)
        .unwrap()
        .columns
        .get_mut("k")
        .unwrap()
        .ty = "bigint".parse().unwrap();
    let before = Side {
        schema: &schema,
        ids: &base,
    };
    let approval = pbps_diff::rebuild::record(
        before,
        &desired,
        &base,
        &parent,
        10,
        &"widen key".parse().unwrap(),
        &ctx,
        &dialect,
    )
    .unwrap();
    let after = Side {
        schema: &desired,
        ids: &approval,
    };
    let root = pbps_diff::rebuild::qualify(before, after, &parent, &dialect).unwrap();
    let target = pbps_diff::rebuild::target_identities(before, after, &root, &dialect).unwrap();
    let addresses: Vec<_> = root
        .intent
        .source
        .tables
        .iter()
        .chain(&root.intent.replacement.tables)
        .map(|(uid, name)| IntermediateAddress {
            uid: uid.clone(),
            address: TableName::new(&name.schema, format!("alias_{uid}")),
        })
        .collect();
    let address = |uid: &Uid| RebuildRelation {
        uid: uid.clone(),
        name: addresses
            .iter()
            .find(|a| &a.uid == uid)
            .unwrap()
            .address
            .clone(),
    };
    let old = RebuildRelation {
        uid: base.table_uid(&parent).unwrap().clone(),
        name: parent.clone(),
    };
    let new = address(target.table_uid(&parent).unwrap());
    let child = address(target.table_uid(&leaf).unwrap());
    let mut bare_leaf = desired.tables[&leaf].clone();
    bare_leaf.partition_of.as_mut().unwrap().parent = new.name.clone();
    let create_parent = Change::CreateTable {
        uid: new.uid.clone(),
        name: new.name.clone(),
        table: Box::new(desired.tables[&parent].clone()),
        beside_default: None,
        moves_at_most: None,
    };
    let create_leaf = Change::CreateTable {
        uid: child.uid.clone(),
        name: child.name.clone(),
        table: Box::new(bare_leaf),
        beside_default: None,
        moves_at_most: None,
    };
    let copy = Change::CopyTableRows(Box::new(CopyTableRows {
        source: old.clone(),
        target: new.clone(),
        max_rows: 10,
        source_partitions: vec![RebuildRelation {
            uid: base.table_uid(&leaf).unwrap().clone(),
            name: leaf.clone(),
        }],
        target_partitions: vec![child.clone()],
        columns: desired.tables[&parent]
            .columns
            .iter()
            .map(|(name, c)| CopyColumn {
                source: Some(CopySource {
                    column: parent.column(name),
                    ty: schema.tables[&parent].columns[name].ty.clone(),
                    collation: None,
                }),
                target: new.name.column(name),
                cast: c.ty.clone(),
                collation: None,
            })
            .collect(),
    }));
    let analyze = Change::AnalyzeTableTree(Box::new(AnalyzeTableTree {
        parent: new.clone(),
        partitions: vec![child],
    }));
    let old_swap = Change::RenameTable {
        uid: old.uid.clone(),
        from: parent.clone(),
        to: address(&old.uid).name.clone(),
        defaults: vec![],
    };
    let new_swap = Change::RenameTable {
        uid: new.uid.clone(),
        from: new.name.clone(),
        to: parent.clone(),
        defaults: vec![],
    };
    let drop_old = Change::DropTable {
        uid: old.uid.clone(),
        name: parent.clone(),
        detach_from: None,
    };
    let backup_read = Change::DropTable {
        uid: base.table_uid(&leaf).unwrap().clone(),
        name: leaf.clone(),
        detach_from: Some(address(&old.uid).name),
    };
    let logical = [PlannedChange::new(Change::RebuildPartitionedTable(
        Box::new(root.clone()),
    ))];
    let owners = PlanOwners::new(PlanNames::of(&logical, &target).unwrap(), &base, &target)
        .with_rebuild_addresses(&root, &approval, &addresses)
        .unwrap();
    let before = Snapshot {
        schema: &schema,
        ids: &base,
    };
    let after = Snapshot {
        schema: &desired,
        ids: &target,
    };
    let baseline = Baseline::of(before, &owners);
    let graph_for = |changes: Vec<Change>| {
        let ops = Operations::expanded(
            1,
            changes
                .into_iter()
                .map(|c| (Origin::SplitPart(0), PlannedChange::new(c))),
            ModuleEdits::Rebuilt,
        );
        // Portable edges alone already conflict; engine effects are additive.
        let fx = ops
            .iter()
            .map(|op| effects(op, &owners, before, after).unwrap())
            .collect::<Vec<_>>();
        let graph = order::graph(
            ops.as_slice(),
            &fx,
            &baseline,
            &Default::default(),
            Checks::ALL,
        );
        assert!(graph.unmet.is_empty(), "{:?}", graph.unmet);
        (ops, graph)
    };
    {
        let parts = vec![
            create_parent.clone(),
            create_leaf.clone(),
            copy.clone(),
            analyze.clone(),
            old_swap.clone(),
            new_swap.clone(),
        ];
        let (ops, graph) = graph_for(parts);
        let at = |change: &Change| ops.iter().position(|op| op.change() == change).unwrap();
        let copy = at(&copy);
        let analyze = at(&analyze);
        let old_swap = at(&old_swap);
        let new_swap = at(&new_swap);
        let has = |a, b| graph.edges.iter().any(|e| e.before == a && e.after == b);
        // These directions contradict the names carried by the typed payloads.
        assert!(
            has(old_swap, copy),
            "source-name read follows the source rename"
        );
        assert!(
            has(new_swap, copy),
            "temporary target read follows the final rename"
        );
        assert!(
            has(new_swap, analyze),
            "temporary ANALYZE follows the final rename"
        );
        assert!(
            has(copy, analyze),
            "the ordinary row-value edge is already correct"
        );
        assert!(!graph.has_cycle(ops.len()));
        let mut schedule = Schedule::of_graph(&graph, ops.as_slice());
        for (a, b) in [(copy, analyze), (analyze, old_swap), (analyze, new_swap)] {
            schedule.edge(
                a,
                b,
                Source::Effect,
                pbps_model::resolver::OrderReason::Restoration,
            );
        }
        assert!(matches!(
            schedule.settle(order::propose(ChangeSet {
                changes: ops.iter().map(|op| op.planned().clone()).collect()
            })),
            Err(order::Refusal::Cycle { .. })
        ));
    }
    let (ops, graph) = graph_for(vec![old_swap.clone(), backup_read]);
    assert!(graph.edges.iter().any(|e| e.before == 0 && e.after == 1));
    assert!(
        !graph.has_cycle(ops.len()),
        "the backup address correctly follows its rename"
    );
    let (_, graph) = graph_for(vec![create_parent, create_leaf, copy, drop_old]);
    assert!(graph.edges.iter().any(|e| e.before == 2 && e.after == 3));
    assert!(graph.edges.iter().any(|e| e.before == 3 && e.after == 2));
    assert!(
        graph.has_cycle(4),
        "source Reads waits for the same lifetime's value-removing DROP"
    );
}
