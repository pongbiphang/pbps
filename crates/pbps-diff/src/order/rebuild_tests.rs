use super::*;
use pbps_dialect::MinimalDialect;
use pbps_model::operation::{
    ModuleEdits, Operations, Origin, PlanOwners, Snapshot, Source as OriginSource, effects,
};
use pbps_model::plan_names::PlanNames;
use pbps_model::{Change, ChangeSet, Column, ModuleDeps, PlannedChange, Schema, Table};
#[path = "../../../pbps-model/tests/support/rebuild.rs"]
pub(super) mod fixture;

#[test]
fn rebuild_copy_precedes_analyze_by_effects_in_adversarial_input_order() {
    let (root, base, desired, addresses) = fixture::approval();
    let owners = PlanOwners::new(PlanNames::of(&[], &base).unwrap(), &base, &desired)
        .with_rebuild_addresses(&root, &base, &addresses)
        .unwrap();
    let schema = Schema::default();
    let before = Snapshot {
        schema: &schema,
        ids: &base,
    };
    let after = Snapshot {
        schema: &schema,
        ids: &desired,
    };
    let mut parts = fixture::changes();
    parts.remove(0);
    parts.reverse();
    let Change::CopyTableRows(copy) = &parts[1] else {
        panic!()
    };
    let target = copy.target.clone();
    let leaf = copy.target_partitions[0].clone();
    for relation in [target, leaf] {
        let mut table = Table::default();
        if relation.name.name == "tmp_p" {
            table
                .columns
                .insert("k".into(), Column::new("bigint".parse().unwrap()));
        }
        parts.push(Change::CreateTable {
            uid: relation.uid,
            name: relation.name,
            table: Box::new(table),
            beside_default: None,
            moves_at_most: None,
        });
    }
    let changes = ChangeSet {
        changes: parts.into_iter().map(PlannedChange::new).collect(),
    };
    let ops = Operations::expanded(
        4,
        changes
            .changes
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, p)| (Origin::Restoration(OriginSource::Change(i)), p)),
        ModuleEdits::InPlace,
    );
    let fx = ops
        .iter()
        .map(|op| effects(op, &owners, before, after).unwrap())
        .collect::<Vec<_>>();
    let baseline = Baseline::from_keys(fx[1].requires.iter().filter_map(|r| {
        if let pbps_model::operation::Requirement::Is(k, pbps_model::operation::Incarnation::Base) =
            r
        {
            Some(k.clone())
        } else {
            None
        }
    }));
    let graph = graph(
        ops.as_slice(),
        &fx,
        &baseline,
        &ModuleDeps::new(),
        Checks::ALL,
    );
    assert!(
        graph
            .edges
            .iter()
            .any(|e| e.before == 1 && e.after == 0 && matches!(e.because, Because::Writes(_)))
    );
    for creator in [2, 3] {
        assert!(graph.edges.iter().any(|e| e.before == creator
            && e.after == 1
            && matches!(e.because, Because::Creates(_))));
    }
    let settled = Schedule::of_graph(&graph, ops.as_slice())
        .settle(propose(changes))
        .unwrap();
    assert_eq!(settled.sequence, [2, 3, 1, 0]);
}

#[test]
fn rebuild_roots_and_partial_parts_cannot_gain_canonical_authority() {
    for change in
        fixture::changes()
            .into_iter()
            .chain(std::iter::once(Change::BuildPartitionedIndex(Box::new(
                fixture::index_recipe(),
            ))))
    {
        let name = change.table().unwrap().shown().to_string();
        let plan = propose(ChangeSet {
            changes: vec![PlannedChange::new(change)],
        });
        assert!(
            super::plan::canonical(plan, &MinimalDialect)
                .unwrap_err()
                .contains(&name)
        );
    }
}

#[test]
fn rebuild_swap_uses_shared_slots_with_distinct_reserved_holders() {
    let (root, base, desired, addresses) = fixture::approval();
    let owners = PlanOwners::new(PlanNames::of(&[], &base).unwrap(), &base, &desired)
        .with_rebuild_addresses(&root, &base, &addresses)
        .unwrap();
    let old = base.index().table_uid(&root.table).unwrap().clone();
    let new = desired.index().table_uid(&root.table).unwrap().clone();
    let rename = |uid, from, to| {
        PlannedChange::new(Change::RenameTable {
            uid,
            from,
            to,
            defaults: vec![],
        })
    };
    // The premature final claim must wait for the source's release, despite equal logical names.
    let parts = vec![
        rename(
            new,
            pbps_model::TableName::new("app", "tmp_p"),
            root.table.clone(),
        ),
        rename(old, root.table, pbps_model::TableName::new("app", "old_p")),
    ];
    let ops = Operations::of_plan(&parts, ModuleEdits::InPlace);
    let schema = Schema::default();
    let slots = super::slots::analyse(
        ops.as_slice(),
        &owners,
        Snapshot {
            schema: &schema,
            ids: &base,
        },
        &MinimalDialect,
    );
    assert!(slots.unowned.is_empty() && slots.unlowered.is_empty());
    let edges = super::slots::edges(&slots, None, &[]);
    assert!(edges.iter().any(|e| e.before == 1 && e.after == 0));
    assert_ne!(slots.effects[0].claims[0].1, slots.effects[1].releases[0].1);
}

#[test]
fn rebuild_roots_and_partial_parts_are_refused_by_the_shared_route() {
    let schema = Schema::default();
    let ids = pbps_model::IdsFile::default();
    let deps = ModuleDeps::new();
    let declared = deployment::schedule::Declarations {
        schema: &schema,
        sources: deployment::Sources {
            before: crate::Side {
                schema: &schema,
                ids: &ids,
            },
            after: &ids,
        },
        module_deps: &deps,
    };
    for change in
        fixture::changes()
            .into_iter()
            .chain(std::iter::once(Change::BuildPartitionedIndex(Box::new(
                fixture::index_recipe(),
            ))))
    {
        let name = change.table().unwrap().shown().to_string();
        let draft = propose(ChangeSet {
            changes: vec![PlannedChange::new(change)],
        });
        let error = deployment::schedule::schedule(
            draft,
            &declared,
            &deployment::schedule::Facts::default(),
            &MinimalDialect,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("cannot be certified") && error.contains(&name),
            "{error}"
        );
    }
}
