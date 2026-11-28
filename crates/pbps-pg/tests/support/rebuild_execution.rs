use super::*;
use pbps_diff::order::{CanonicalPlan, Schedule, Source, finish};
use pbps_model::IndexChange;
use pbps_model::ModuleChange;
use pbps_model::PartitionChange;
use pbps_model::TableChange;
#[test]
fn prepared_inventory_executes_through_the_existing_driver_with_stable_ids() {
    let f = Fixture::new();
    let prepared = f.prepare().unwrap();
    let canonical = prepared.operations();
    let result = expand(&f.input(), prepared, &pbps_pg::Postgres::new()).unwrap();
    assert_eq!(result.operations.len(), canonical.len());
    for op in result.operations.iter() {
        let original = canonical
            .iter()
            .find(|other| other.change() == op.change())
            .unwrap();
        assert_eq!(op.id(), original.id());
        assert_eq!(op.origin(), original.origin());
    }
    let parts: Vec<_> = result.operations.iter().map(|op| op.change()).collect();
    let copy = parts
        .iter()
        .position(|c| matches!(c, Change::Partition(PartitionChange::CopyTableRows(_))))
        .unwrap();
    let own = parts
        .iter()
        .position(|c| matches!(c, Change::Index(IndexChange::BuildPartitionedIndex(_))))
        .unwrap();
    let analyze = parts
        .iter()
        .position(|c| matches!(c, Change::Partition(PartitionChange::AnalyzeTableTree(_))))
        .unwrap();
    assert!(copy < own && own < analyze);
    let swap = parts
        .iter()
        .position(|c| matches!(c, Change::Table(TableChange::RenameTable { .. })))
        .unwrap();
    assert!(analyze < swap);
    assert!(matches!(
        parts.last().unwrap(),
        Change::Table(TableChange::DropTable {
            detach_from: None,
            ..
        })
    ));
    assert_eq!(result.rewrites, 1);
    assert_eq!(result.target.rebuilds, f.recorded.rebuilds);
    assert_eq!(f.recorded.rebuilds.len(), 1);
}

#[test]
fn only_complete_storage_ownership_excludes_a_spelling_without_freeing_it() {
    let mut f = Fixture::new();
    for (id, by) in [(3, 1), (4, 3)] {
        f.node(
            id,
            Held::Owned {
                by: obj(by),
                kind: Ownership::Storage,
            },
        );
        f.link(by, id);
    }
    let Fact::Present(spellings) = &mut f.catalog.spellings else {
        unreachable!()
    };
    spellings.insert(
        obj(4),
        PartSpelling {
            table: TableName::new("unrelated", "arbitrary"),
            name: "opaque_index".into(),
            kind: pbps_model::operation::NameKind::Index,
            index_attributes: None,
        },
    );
    let owner = f.catalog.graph.nodes.remove(&obj(3)).unwrap();
    assert!(f.prepare().unwrap_err().contains("app.p:"));
    f.catalog.graph.nodes.insert(obj(3), owner);
    let prepared = f.prepare().unwrap();
    assert!(prepared.closure().nodes.contains_key(&obj(4)));
    assert!(
        prepared
            .closure()
            .edges
            .iter()
            .any(|(from, link)| *from == obj(3) && link.dependent == obj(4))
    );
    assert!(prepared.slots().catalog_owned.is_empty());
    let (rows, _) = prepared.slots().effects_with_unread();
    assert!(
        !rows
            .iter()
            .flat_map(|fx| &fx.releases)
            .any(|(slot, _)| slot.name == "opaque_index")
    );
    for fact in [Fact::Absent, Fact::Unreadable("42501 storage read".into())] {
        let Fact::Present(node) = f.catalog.graph.nodes.get_mut(&obj(3)).unwrap() else {
            unreachable!()
        };
        node.dependents = fact;
        let error = f.prepare().unwrap_err();
        assert!(error.contains("dependent 3"), "{error}");
    }
    let Fact::Present(node) = f.catalog.graph.nodes.get_mut(&obj(3)).unwrap() else {
        unreachable!()
    };
    node.dependents = Fact::Present(
        [Link {
            dependent: obj(4),
            kind: "internal".into(),
            binding: Binding::Ownership(Ownership::Storage),
        }]
        .into(),
    );
    for id in [3, 4] {
        let Fact::Present(node) = f.catalog.graph.nodes.get_mut(&obj(id)).unwrap() else {
            unreachable!()
        };
        node.held = Held::Owned {
            by: obj(if id == 3 { 1 } else { 3 }),
            kind: Ownership::Attribute,
        };
    }
    assert!(
        f.prepare()
            .unwrap_err()
            .contains("unrelated.arbitrary: unsupported part ownership")
    );
}

#[test]
fn a_prepared_rebuild_refuses_changed_mode_or_stale_approval() {
    let f = Fixture::new();
    let mut input = f.input();
    input.mode = PlanMode::Staged;
    assert!(
        expand(&input, f.prepare().unwrap(), &pbps_pg::Postgres::new())
            .unwrap_err()
            .contains("app.p: rebuild requires PostgreSQL transactional mode")
    );
    let mut f = Fixture::new();
    let prepared = f.prepare().unwrap();
    f.desired
        .tables
        .get_mut(&f.root.table)
        .unwrap()
        .columns
        .get_mut("k")
        .unwrap()
        .ty = "numeric".parse().unwrap();
    assert!(
        expand(&f.input(), prepared, &pbps_pg::Postgres::new())
            .unwrap_err()
            .contains("re-approve it with")
    );
}

#[test]
fn independent_replay_rejects_premature_analysis_and_final_name_claims() {
    use pbps_diff::order::{self, slots};
    use pbps_model::operation::*;
    let f = Fixture::new();
    let pg = pbps_pg::Postgres::new();
    let prepared = f.prepare().unwrap();
    let input = f.input();
    let owners = prepared.owners(&input).unwrap();
    let before = Snapshot {
        schema: &f.base,
        ids: &f.ids,
    };
    let after = Snapshot {
        schema: &f.desired,
        ids: prepared.target(),
    };
    let done = expand(&input, f.prepare().unwrap(), &pg).unwrap();
    let baseline = order::Baseline::of(before, &owners).expect("read the test baseline");
    let canonical = prepared.operations();
    let mut ops = done.operations.as_slice().to_vec();
    let copy = ops
        .iter()
        .position(|o| {
            matches!(
                o.change(),
                Change::Partition(PartitionChange::CopyTableRows(_))
            )
        })
        .unwrap();
    let analyze = ops
        .iter()
        .position(|o| {
            matches!(
                o.change(),
                Change::Partition(PartitionChange::AnalyzeTableTree(_))
            )
        })
        .unwrap();
    ops.swap(copy, analyze);
    let fx: Vec<_> = ops
        .iter()
        .map(|op| {
            effects(op, &owners, before, after).unwrap().merged(
                pbps_dialect::Dialect::engine_effects(
                    &pg,
                    op,
                    &owners,
                    before,
                    after,
                    Some(prepared.names()),
                    &Default::default(),
                )
                .unwrap(),
            )
        })
        .collect();
    assert!(!order::verify::verify(&ops, &fx, &baseline, &Default::default()).is_empty());
    ops = done.operations.as_slice().to_vec();
    let a = ops.iter().position(|o|matches!(o.change(),Change::Table(TableChange::RenameTable{uid,..}) if f.root.intent.source.tables.contains_key(uid))).unwrap();
    let b = ops.iter().position(|o|matches!(o.change(),Change::Table(TableChange::RenameTable{uid,to,..}) if f.root.intent.replacement.tables.contains_key(uid) && to==&f.root.table)).unwrap();
    ops.swap(a, b);
    let (rows, _) = prepared.slots().effects_with_unread();
    let reordered_rows = ops
        .iter()
        .map(|op| rows[canonical.iter().position(|p| p.id() == op.id()).unwrap()].clone())
        .collect();
    let description = prepared.slots().clone().with_effects(reordered_rows);
    let (violations, skipped) = slots::verify(
        &ops,
        &description,
        &pg,
        Some(prepared.names()),
        &owners,
        before,
    )
    .unwrap();
    assert!(skipped.is_empty());
    assert!(!violations.is_empty());
}

#[test]
fn final_verification_keeps_every_part_and_the_expansions_completion_order() {
    let f = Fixture::new();
    let pg = pbps_pg::Postgres::new();
    let prepared = f.prepare().unwrap();
    let done = expand(&f.input(), prepared.clone(), &pg).unwrap();
    let valid = done.changes.changes().clone();
    verify_plan(&f.input(), &prepared, &valid, &pg).unwrap();
    let mut missing = valid.clone();
    missing.changes.pop();
    assert!(verify_plan(&f.input(), &prepared, &missing, &pg).is_err());
    let mut reordered = valid.clone();
    let own = reordered
        .changes
        .iter()
        .position(|p| {
            matches!(
                p.change,
                Change::Index(IndexChange::BuildPartitionedIndex(_))
            )
        })
        .unwrap();
    let analyze = reordered
        .changes
        .iter()
        .position(|p| {
            matches!(
                p.change,
                Change::Partition(PartitionChange::AnalyzeTableTree(_))
            )
        })
        .unwrap();
    reordered.changes.swap(own, analyze);
    assert!(verify_plan(&f.input(), &prepared, &reordered, &pg).is_err());
    let mut edited = valid;
    for p in &mut edited.changes {
        if let Change::Partition(PartitionChange::CopyTableRows(c)) = &mut p.change {
            c.max_rows += 1;
        }
    }
    assert!(verify_plan(&f.input(), &prepared, &edited, &pg).is_err());
}

fn expanded_plan(
    input: &Input<'_>,
    prepared: &Prepared,
    pg: &pbps_pg::Postgres,
) -> CanonicalPlan<pbps_pg::Postgres> {
    let root = pbps_diff::diff(input.before, input.desired, pg, input.hints).unwrap();
    continue_plan(root, input, prepared.clone(), pg).unwrap()
}

#[test]
fn a_rebuild_finishes_only_with_its_prepared_catalog() {
    let mut f = Fixture::new();
    let pg = pbps_pg::Postgres::new();
    let prepared = f.prepare().unwrap();
    let plan = expanded_plan(&f.input(), &prepared, &pg);
    let finished = finish::rebuild(plan.clone(), &f.input(), &prepared, &pg).unwrap();
    assert_eq!(finished.changes(), plan.changes());

    // A second catalog read reserves an address the original sequence uses.
    let occupied = &prepared.addresses()[0].address;
    let Fact::Present(names) = &mut f.naming else {
        unreachable!()
    };
    names
        .get_mut(&occupied.schema)
        .unwrap()
        .types
        .insert(occupied.name.clone().into(), obj(90));
    let other_prepared = f.prepare().unwrap();
    let other_input = f.input();
    assert_ne!(prepared.addresses(), other_prepared.addresses());
    let reason = verify_plan(&other_input, &other_prepared, plan.changes(), &pg).unwrap_err();
    let Err(error) = finish::rebuild(plan, &other_input, &other_prepared, &pg) else {
        panic!("the rebuild finished in a different catalog context");
    };
    assert!(matches!(
        &error,
        finish::FinishError::Rebuild(detail) if detail == &reason
    ));
    assert_eq!(error.to_string(), reason);
}

#[test]
fn rebuild_finish_refuses_a_changed_sequence_in_its_prepared_catalog() {
    use pbps_model::resolver::OrderReason;
    let f = Fixture::new();
    let pg = pbps_pg::Postgres::new();
    let input = f.input();
    let prepared = f.prepare().unwrap();
    let plan = expanded_plan(&input, &prepared, &pg);
    let steps = &plan.changes().changes;
    let own = steps
        .iter()
        .position(|p| {
            matches!(
                p.change,
                Change::Index(IndexChange::BuildPartitionedIndex(_))
            )
        })
        .unwrap();
    let analyze = steps
        .iter()
        .position(|p| {
            matches!(
                p.change,
                Change::Partition(PartitionChange::AnalyzeTableTree(_))
            )
        })
        .unwrap();
    let mut schedule = Schedule::new(steps.len());
    schedule.edge(analyze, own, Source::Effect, OrderReason::Structural);
    let reordered = schedule.settle(plan).unwrap().into_planning();
    let reason = verify_plan(&input, &prepared, reordered.changes(), &pg).unwrap_err();
    let Err(error) = finish::rebuild(reordered, &input, &prepared, &pg) else {
        panic!("the reordered rebuild finished");
    };
    assert_eq!(error.to_string(), reason);
}

#[test]
fn recertification_requires_readable_managed_and_reviewed_dependency_removals() {
    let mut f = Fixture::new();
    let prepared = f.prepare().unwrap();
    let done = expand(&f.input(), prepared, &pbps_pg::Postgres::new()).unwrap();
    let mut changes = done.changes.changes().clone();
    let copy = changes
        .changes
        .iter()
        .find_map(|p| {
            if let Change::Partition(PartitionChange::CopyTableRows(c)) = &p.change {
                Some((**c).clone())
            } else {
                None
            }
        })
        .unwrap();
    recertify(&f.catalog, &copy, side(&f.base, &f.ids), &changes).unwrap();
    let id = ModuleId::Named(ObjectName::new("app", "late_view"));
    f.node(8, Held::Dependent(Holds::Module(id.clone())));
    f.link(1, 8);
    assert!(
        recertify(&f.catalog, &copy, side(&f.base, &f.ids), &changes)
            .unwrap_err()
            .contains("unmanaged")
    );
    f.base.modules.insert(
        id.clone(),
        Module {
            kind: ModuleKind::View,
            definition: "SELECT k FROM app.p".into(),
            description: None,
            options: Default::default(),
        },
    );
    assert!(
        recertify(&f.catalog, &copy, side(&f.base, &f.ids), &changes)
            .unwrap_err()
            .contains("does not release")
    );
    changes.changes.push(PlannedChange::new(Change::Module(
        ModuleChange::DropModule {
            id,
            kind: ModuleKind::View,
        },
    )));
    recertify(&f.catalog, &copy, side(&f.base, &f.ids), &changes).unwrap();
    f.catalog.publications = Fact::Unreadable("42501 membership".into());
    assert!(
        recertify(&f.catalog, &copy, side(&f.base, &f.ids), &changes)
            .unwrap_err()
            .contains("42501")
    );
    f.catalog.publications =
        Fact::Present([("late_publication".into(), f.root.table.clone())].into());
    assert!(
        recertify(&f.catalog, &copy, side(&f.base, &f.ids), &changes)
            .unwrap_err()
            .contains("late_publication")
    );
}
