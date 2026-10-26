use super::*;
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
        .position(|c| matches!(c, Change::CopyTableRows(_)))
        .unwrap();
    let own = parts
        .iter()
        .position(|c| matches!(c, Change::BuildPartitionedIndex(_)))
        .unwrap();
    let analyze = parts
        .iter()
        .position(|c| matches!(c, Change::AnalyzeTableTree(_)))
        .unwrap();
    assert!(copy < own && own < analyze);
    let swap = parts
        .iter()
        .position(|c| matches!(c, Change::RenameTable { .. }))
        .unwrap();
    assert!(analyze < swap);
    assert!(matches!(
        parts.last().unwrap(),
        Change::DropTable {
            detach_from: None,
            ..
        }
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
    assert!(
        !prepared
            .slots()
            .effects
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
    let baseline = order::Baseline::of(before, &owners);
    let canonical = prepared.operations();
    let mut ops = done.operations.as_slice().to_vec();
    let copy = ops
        .iter()
        .position(|o| matches!(o.change(), Change::CopyTableRows(_)))
        .unwrap();
    let analyze = ops
        .iter()
        .position(|o| matches!(o.change(), Change::AnalyzeTableTree(_)))
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
                )
                .unwrap(),
            )
        })
        .collect();
    assert!(!order::verify::verify(&ops, &fx, &baseline, &Default::default()).is_empty());
    ops = done.operations.as_slice().to_vec();
    let a = ops.iter().position(|o|matches!(o.change(),Change::RenameTable{uid,..} if f.root.intent.source.tables.contains_key(uid))).unwrap();
    let b = ops.iter().position(|o|matches!(o.change(),Change::RenameTable{uid,to,..} if f.root.intent.replacement.tables.contains_key(uid) && to==&f.root.table)).unwrap();
    ops.swap(a, b);
    let mut description = prepared.slots().clone();
    description.effects = ops
        .iter()
        .map(|op| {
            prepared.slots().effects[canonical.iter().position(|p| p.id() == op.id()).unwrap()]
                .clone()
        })
        .collect();
    let (violations, skipped) = slots::verify(
        &ops,
        &description,
        &pg,
        Some(prepared.names()),
        &owners,
        before,
    );
    assert!(skipped.is_empty());
    assert!(!violations.is_empty());
}
