//! Shared slot-qualified inputs and value lifetimes within one logical expansion.
use pbps_diff::{
    Side,
    order::{self, Baseline, Checks, Schedule, Source},
};
use pbps_model::{operation::*, plan_names::PlanNames, *};

#[test]
fn one_split_origin_orders_source_temp_and_backup_reads_through_shared_slots() {
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
        name: address(&old.uid).name,
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
    let parts = vec![
        create_parent,
        create_leaf,
        copy,
        analyze,
        old_swap,
        new_swap,
        backup_read,
        drop_old,
    ];
    let ops = Operations::expanded(
        1,
        parts
            .into_iter()
            .map(|c| (Origin::SplitPart(0), PlannedChange::new(c))),
        ModuleEdits::Rebuilt,
    );
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
    let slots = order::slots::analyse(ops.as_slice(), &owners, before, &dialect);
    assert!(slots.unowned.is_empty(), "{:?}", slots.unowned);
    let slot_edges = order::slots::edges(&slots, None, &[]);
    for (a, b) in [(0, 2), (2, 4), (2, 5), (3, 5), (4, 6)] {
        assert!(
            slot_edges.iter().any(|e| e.before == a && e.after == b),
            "missing address edge {a} -> {b}"
        );
    }
    assert!(graph.edges.iter().any(|e| e.before == 2 && e.after == 3));
    assert!(graph.edges.iter().any(|e| e.before == 2 && e.after == 7));
    assert!(!graph.edges.iter().any(|e| e.before == 7 && e.after == 2));
    let mut schedule = Schedule::of_graph(&graph, ops.as_slice());
    order::slots::schedule(&mut schedule, &slot_edges);
    // These are completion obligations, not a new sorting stage or rank.
    for (a, b) in [(3, 4), (5, 6)] {
        schedule.edge(
            a,
            b,
            Source::Effect,
            pbps_model::resolver::OrderReason::Restoration,
        );
    }
    let settled = schedule
        .settle(order::propose(ChangeSet {
            changes: ops.iter().map(|o| o.planned().clone()).collect(),
        }))
        .unwrap();
    assert_eq!(settled.sequence, [0, 1, 2, 3, 4, 5, 6, 7]);
    let violations = order::verify::verify(ops.as_slice(), &fx, &baseline, &Default::default());
    assert!(violations.is_empty(), "{violations:?}");
    let (violations, skipped) =
        order::slots::verify(ops.as_slice(), &slots, &dialect, None, &owners, before);
    assert!(
        violations.is_empty() && skipped.is_empty(),
        "{violations:?} / {skipped:?}"
    );
    let reverse: Vec<_> = (0..ops.len()).rev().collect();
    let reversed = order::reordered_operations(&ops, &reverse);
    let reversed_fx: Vec<_> = reverse.iter().map(|&i| fx[i].clone()).collect();
    let reversed_graph = order::graph(
        reversed.as_slice(),
        &reversed_fx,
        &baseline,
        &Default::default(),
        Checks::ALL,
    );
    let reversed_slots = order::slots::analyse(reversed.as_slice(), &owners, before, &dialect);
    let mut schedule = Schedule::of_graph(&reversed_graph, reversed.as_slice());
    order::slots::schedule(
        &mut schedule,
        &order::slots::edges(&reversed_slots, None, &[]),
    );
    for (a, b) in [(3, 4), (5, 6)] {
        schedule.edge(
            7 - a,
            7 - b,
            Source::Effect,
            pbps_model::resolver::OrderReason::Restoration,
        );
    }
    let settled = schedule
        .settle(order::propose(ChangeSet {
            changes: reversed.iter().map(|o| o.planned().clone()).collect(),
        }))
        .unwrap();
    assert_eq!(settled.sequence, reverse);
    // Independent slot replay rejects both sides of each boundary, including
    // a release seen before its creator. It must not seed a phantom holder.
    for order in [
        vec![4, 0, 1, 2, 3, 5, 6, 7],
        vec![0, 1, 5, 2, 3, 4, 6, 7],
        vec![6, 0, 1, 2, 3, 4, 5, 7],
    ] {
        let reordered: Vec<_> = order.iter().map(|&i| ops.as_slice()[i].clone()).collect();
        let reordered_slots = order::slots::Slots {
            effects: order.iter().map(|&i| slots.effects[i].clone()).collect(),
            ..Default::default()
        };
        let (violations, _) = order::slots::verify(
            &reordered,
            &reordered_slots,
            &dialect,
            None,
            &owners,
            before,
        );
        assert!(
            violations
                .iter()
                .any(|v| matches!(v, order::slots::SlotViolation::MissingInput { .. })),
            "{order:?}: {violations:?}"
        );
    }
}
