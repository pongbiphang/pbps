use super::*;
use crate::{Context, Side};
use pbps_model::operation::{self, Incarnation, Origin, Source};
use pbps_model::*;

fn fragment() -> (Operations, Vec<Effects>, Baseline, ChangeSet) {
    let p = TableName::new("app", "p");
    let r = TableName::new("app", "ref");
    let tmp = TableName::new("app", "tmp_p");
    let old = TableName::new("app", "old_p");
    let pk = PrimaryKey {
        name: Some("p_pk".into()),
        columns: vec!["k".into()],
        storage_parameters: Default::default(),
    };
    let fk = ForeignKey {
        columns: vec!["k".into()],
        references_table: p.clone(),
        references_columns: vec!["k".into()],
        on_delete: ReferentialAction::NoAction,
        on_update: ReferentialAction::NoAction,
    };
    let mut column = Column::new("integer".parse().unwrap());
    column.nullable = false;
    let mut before = Schema::default();
    before.tables.insert(
        p.clone(),
        Table {
            columns: [("k".into(), column.clone())].into(),
            primary_key: Some(pk.clone()),
            partition_by: Some(PartitionBy {
                strategy: PartitionStrategy::Range,
                columns: vec!["k".into()],
                external: false,
            }),
            ..Table::default()
        },
    );
    before.tables.insert(
        r.clone(),
        Table {
            columns: [("k".into(), column)].into(),
            foreign_keys: [("ref_p_fk".into(), fk.clone())].into(),
            ..Table::default()
        },
    );
    let ctx = Context {
        operator: "PR2c audit probe".into(),
        today: "2026-10-24".into(),
    };
    let source = crate::resolve(&before, &IdsFile::default(), &[], &ctx)
        .unwrap()
        .ids;
    let mut after = before.clone();
    after
        .tables
        .get_mut(&p)
        .unwrap()
        .columns
        .get_mut("k")
        .unwrap()
        .ty = "bigint".parse().unwrap();
    let dialect = pbps_dialect::MinimalDialect;
    let recorded = crate::rebuild::record(
        Side {
            schema: &before,
            ids: &source,
        },
        &after,
        &source,
        &p,
        10,
        &Reason::new("Rebuild approved for audit probe").unwrap(),
        &ctx,
        &dialect,
    )
    .unwrap();
    let root = crate::rebuild::qualify(
        Side {
            schema: &before,
            ids: &source,
        },
        Side {
            schema: &after,
            ids: &recorded,
        },
        &p,
        &dialect,
    )
    .unwrap();
    let source_uid = root.intent.source.as_ids().table_uid(&p).unwrap().clone();
    let new_uid = root
        .intent
        .replacement
        .as_ids()
        .table_uid(&p)
        .unwrap()
        .clone();
    assert_ne!(source_uid, new_uid);
    // Endpoint fixture; the audited transition belongs to the expansion commit.
    let mut target = source.clone();
    for uid in root.intent.source.columns.keys() {
        target.columns.remove(uid);
    }
    for uid in root.intent.source.tables.keys() {
        target.tables.remove(uid);
    }
    target.tables.extend(root.intent.replacement.tables.clone());
    target
        .columns
        .extend(root.intent.replacement.columns.clone());
    target.validate().unwrap();
    let addresses = vec![
        IntermediateAddress {
            uid: source_uid.clone(),
            address: old.clone(),
        },
        IntermediateAddress {
            uid: new_uid.clone(),
            address: tmp.clone(),
        },
    ];
    let mut bare = after.tables[&p].clone();
    bare.primary_key = None;
    let changes = vec![
        Change::DropForeignKey {
            table: r.clone(),
            name: "ref_p_fk".into(),
        },
        Change::SetPrimaryKey {
            table: p.clone(),
            from: Some(pk.clone()),
            to: None,
            nonclustered: false,
        },
        Change::CreateTable {
            uid: new_uid.clone(),
            name: tmp.clone(),
            table: Box::new(bare),
            beside_default: None,
            moves_at_most: None,
        },
        Change::RenameTable {
            uid: source_uid.clone(),
            from: p.clone(),
            to: old.clone(),
            defaults: vec![],
        },
        Change::RenameTable {
            uid: new_uid.clone(),
            from: tmp,
            to: p.clone(),
            defaults: vec![],
        },
        Change::SetPrimaryKey {
            table: p,
            from: None,
            to: Some(pk),
            nonclustered: false,
        },
        Change::AddForeignKey {
            table: r,
            name: "ref_p_fk".into(),
            constraint: Box::new(fk),
        },
        Change::DropTable {
            uid: source_uid.clone(),
            name: old,
            detach_from: None,
        },
    ];
    let changes: Vec<_> = changes.into_iter().map(PlannedChange::new).collect();
    let names = PlanNames::of(&changes, &target).unwrap();
    let owners = PlanOwners::new(names, &source, &target)
        .with_rebuild_addresses(&root, &recorded, &addresses)
        .unwrap();
    let ops = Operations::expanded(
        1,
        changes.iter().cloned().enumerate().map(|(i, p)| {
            (
                if [0, 1, 3, 7].contains(&i) {
                    Origin::Teardown(Source::Change(0))
                } else {
                    Origin::Restoration(Source::Change(0))
                },
                p,
            )
        }),
        ModuleEdits::InPlace,
    );
    let base = Snapshot {
        schema: &before,
        ids: &source,
    };
    let end = Snapshot {
        schema: &after,
        ids: &target,
    };
    let effects = ops
        .iter()
        .map(|op| operation::effects(op, &owners, base, end).unwrap())
        .collect::<Vec<_>>();
    let baseline = Baseline::of(base, &owners);
    (ops, effects, baseline, ChangeSet { changes })
}

#[test]
fn restored_fk_binding_allows_the_eight_step_fragment_to_drop_the_old_table_last() {
    let (ops, fx, base, changes) = fragment();
    let g = graph(ops.as_slice(), &fx, &base, &ModuleDeps::new(), Checks::ALL);
    assert!(g.unmet.is_empty(), "{:?}", g.unmet);
    assert!(
        g.edges
            .iter()
            .any(|e| e.before == 0 && e.after == 7 && matches!(e.because, Because::Frees(_)))
    );
    assert!(!g.edges.iter().any(|e| e.before == 7 && e.after == 6));
    let mut schedule = super::super::Schedule::of_graph(&g, ops.as_slice());
    schedule.edge(
        6,
        7,
        super::super::Source::Effect,
        pbps_model::resolver::OrderReason::Restoration,
    );
    let result = schedule.settle(super::super::propose(changes)).unwrap();
    assert_eq!(result.sequence, [0, 1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(
        run(
            &ops,
            &fx,
            &base,
            &result.sequence,
            &ModuleDeps::new(),
            Checks::ALL
        ),
        []
    );
    // The old incarnation really blocks retirement until its release.
    assert!(
        run(
            &ops,
            &fx,
            &base,
            &[7, 0, 1, 2, 3, 4, 5, 6],
            &ModuleDeps::new(),
            Checks::ALL
        )
        .iter()
        .any(|v| matches!(v, Violation::StillPresent { .. }))
    );
}

#[test]
fn retirement_edges_and_binding_replay_agree_for_known_old_and_unknown_fk_targets() {
    let (ops, fx, base, _) = fragment();
    let Requirement::NotBoundTo { surface, target } = fx[7]
        .requires
        .iter()
        .find(|r| matches!(r, Requirement::NotBoundTo { .. }))
        .unwrap()
    else {
        panic!()
    };
    let orders = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    for binding in ["replacement", "old", "unknown", "conflicting"] {
        let chosen = [0, 6, 7];
        let ops = Operations::of_plan(
            &chosen
                .iter()
                .map(|&i| ops.as_slice()[i].planned().clone())
                .collect::<Vec<_>>(),
            ModuleEdits::InPlace,
        );
        let mut fx: Vec<_> = chosen.iter().map(|&i| fx[i].clone()).collect();
        // Isolate surface retirement from table/key existence and name slots,
        // so neither can hide disagreement in this contract's own edges.
        for f in &mut fx {
            f.creates.retain(|r| r.key() == surface);
            f.removes.retain(|r| r.key() == surface);
            f.rewrites.clear();
            f.requires
                .retain(|r| matches!(r, Requirement::NotBoundTo { .. }));
        }
        if binding == "old" {
            fx[1].bindings[0].target = target.clone();
        }
        if binding == "unknown" {
            fx[1].bindings.clear();
        }
        if binding == "conflicting" {
            let mut other = fx[1].bindings[0].clone();
            other.target = target.clone();
            fx[1].bindings.push(other);
        }
        let g = graph(ops.as_slice(), &fx, &base, &ModuleDeps::new(), Checks::ALL);
        for order in orders {
            let pos = |i| order.iter().position(|&j| j == i).unwrap();
            let edges_hold =
                g.unmet.is_empty() && g.edges.iter().all(|e| pos(e.before) < pos(e.after));
            let violations = run(&ops, &fx, &base, &order, &ModuleDeps::new(), Checks::ALL);
            // Creating the same surface before releasing it is a name
            // collision, independently of the new retirement requirement.
            let concrete_holds = pos(0) < pos(1)
                && !violations
                    .iter()
                    .any(|v| !matches!(v, Violation::Edge { .. }));
            assert_eq!(
                edges_hold, concrete_holds,
                "binding={binding} order={order:?}: {violations:?}"
            );
            if order == [0, 1, 2] {
                assert_eq!(concrete_holds, binding == "replacement");
            }
        }
        // Unknown baseline bindings retain the existing release obligation.
        let unknown = Baseline::from_keys([surface.clone()]);
        let mut drop_only = fx[2].clone();
        drop_only.bindings.clear();
        assert!(
            !graph(
                &ops.as_slice()[2..],
                &[drop_only.clone()],
                &unknown,
                &ModuleDeps::new(),
                Checks::ALL
            )
            .unmet
            .is_empty()
        );
        assert!(
            verify(
                &ops.as_slice()[2..],
                &[drop_only],
                &unknown,
                &ModuleDeps::new()
            )
            .iter()
            .any(|v| matches!(v, Violation::StillPresent { .. }))
        );
    }
    // Effects merge cannot turn contradictory binding facts into certainty.
    let mut conflicting = fx[6].clone();
    conflicting.bindings[0].target = target.clone();
    assert!(
        fx[6]
            .clone()
            .merged(conflicting)
            .bound_target(surface, Incarnation::Final)
            .is_none()
    );
}

#[test]
fn final_fk_still_bound_to_the_retiring_table_or_unknown_refuses_drop_last() {
    let (ops, fx, baseline, changes) = fragment();
    let Requirement::NotBoundTo { target, .. } = &fx[7].requires[0] else {
        panic!()
    };
    for known in [true, false] {
        let mut fx = fx.clone();
        if known {
            fx[6].bindings[0].target = target.clone();
        } else {
            fx[6].bindings.clear();
        }
        let graph = graph(
            ops.as_slice(),
            &fx,
            &baseline,
            &ModuleDeps::new(),
            Checks::ALL,
        );
        let mut schedule = crate::order::Schedule::of_graph(&graph, ops.as_slice());
        schedule.edge(
            6,
            7,
            crate::order::Source::Effect,
            pbps_model::resolver::OrderReason::Restoration,
        );
        assert!(matches!(
            schedule.settle(crate::order::propose(changes.clone())),
            Err(crate::order::Refusal::Cycle { .. })
        ));
        assert!(
            run(
                &ops,
                &fx,
                &baseline,
                &[0, 1, 2, 3, 4, 5, 6, 7],
                &ModuleDeps::new(),
                Checks::ALL
            )
            .iter()
            .any(|v| matches!(v, Violation::StillPresent { .. }))
        );
    }
}

#[test]
fn ordinary_same_name_replacement_with_a_new_uid_keeps_its_existing_order() {
    let (mut ops, _, _, _) = fragment();
    // The ordinary drop/create route uses no rebuild root or address overlay.
    let drops = ops.as_slice()[0].planned().clone();
    let old = if let Change::DropTable { uid, .. } = ops.as_slice()[7].change() {
        uid.clone()
    } else {
        panic!()
    };
    let mut create = ops.as_slice()[2].planned().clone();
    let Change::CreateTable { name, table, .. } = &mut create.change else {
        panic!()
    };
    *name = t("p");
    table.primary_key = Some(PrimaryKey {
        name: Some("p_pk".into()),
        columns: vec!["k".into()],
        storage_parameters: Default::default(),
    });
    let restore = ops.as_slice()[6].planned().clone();
    let mut before = Schema::default();
    let mut parent = table_with(&["k"]);
    parent.primary_key = table.primary_key.clone();
    let Change::AddForeignKey { constraint, .. } = &restore.change else {
        panic!()
    };
    let mut child = table_with(&["k"]);
    child
        .foreign_keys
        .insert("ref_p_fk".into(), *constraint.clone());
    before.tables.insert(t("p"), parent);
    before.tables.insert(t("ref"), child);
    let mut after = before.clone();
    after.tables.insert(t("p"), *table.clone());
    let mut before_ids = Plan::ids(&["ref"], &[("ref", "k"), ("p", "k")]);
    before_ids.tables.insert(old.clone(), t("p"));
    let mut after_ids = before_ids.clone();
    after_ids.tables.remove(&old);
    let Change::CreateTable { uid: new, .. } = &create.change else {
        panic!()
    };
    after_ids.tables.insert(new.clone(), t("p"));
    after_ids.columns.retain(|_, c| c.table != t("p"));
    after_ids
        .columns
        .insert(uid(UidKind::Column, "new-p.k"), t("p").column("k"));
    let plan = Plan {
        logical: vec![
            drops,
            p(Change::DropTable {
                uid: old,
                name: t("p"),
                detach_from: None,
            }),
            create,
            restore,
        ],
        before,
        after,
        before_ids,
        after_ids,
    };
    let (ordinary, fx, baseline) = plan.analysed(ModuleEdits::InPlace);
    ops = ordinary;
    assert_eq!(
        settled(&ops, &fx, &baseline, &[0, 1, 2, 3], false),
        [0, 1, 2, 3]
    );
    assert_eq!(
        run(
            &ops,
            &fx,
            &baseline,
            &[0, 1, 2, 3],
            &ModuleDeps::new(),
            Checks::ALL
        ),
        []
    );
}

#[test]
fn an_inline_self_fk_binds_the_created_uid_even_when_the_old_table_has_its_name() {
    let old_ids = Plan::ids(&["p"], &[("p", "k")]);
    let new_uid = uid(UidKind::Table, "replacement-p");
    let mut new_ids = old_ids.clone();
    new_ids.tables.clear();
    new_ids.tables.insert(new_uid.clone(), t("p"));
    let mut parent = table_with(&["k"]);
    parent.foreign_keys.insert(
        "self_fk".into(),
        ForeignKey {
            columns: vec!["k".into()],
            references_table: t("p"),
            references_columns: vec!["k".into()],
            on_delete: ReferentialAction::NoAction,
            on_update: ReferentialAction::NoAction,
        },
    );
    let mut before = Schema::default();
    before.tables.insert(t("p"), table_with(&["k"]));
    let mut after = Schema::default();
    after.tables.insert(t("p"), parent.clone());
    let plan = Plan {
        logical: vec![
            p(Change::DropTable {
                uid: old_ids.table_uid(&t("p")).unwrap().clone(),
                name: t("p"),
                detach_from: None,
            }),
            p(Change::CreateTable {
                uid: new_uid.clone(),
                name: t("p"),
                table: Box::new(parent),
                beside_default: None,
                moves_at_most: None,
            }),
        ],
        before,
        after,
        before_ids: old_ids,
        after_ids: new_ids,
    };
    let (_, fx, _) = plan.analysed(ModuleEdits::InPlace);
    assert_eq!(fx[1].bindings.len(), 1);
    assert_eq!(fx[1].bindings[0].target.uid(), &new_uid);
}
