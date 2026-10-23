use super::*;

fn refusal(g: &Graph) -> String {
    g.closure(&root()).unwrap_err()
}
fn object(id: u64) -> Object {
    Object {
        catalog: "objects".into(),
        id,
        attribute: 0,
    }
}
fn root() -> Root {
    Root::Table("t_aaaaaa".parse().unwrap())
}
fn held(id: u64) -> Held {
    Held::Dependent(Holds::Module(ModuleId::Named(
        format!("app.v{id}").parse().unwrap(),
    )))
}
fn owned(id: u64, kind: Ownership) -> Held {
    Held::Owned {
        by: object(id),
        kind,
    }
}
fn node(g: &mut Graph, id: u64, held: Held, children: &[u64]) {
    g.nodes.insert(
        object(id),
        Fact::Present(Node {
            described: format!("node {id}"),
            held,
            dependents: Fact::Present(children.iter().map(|id| link(*id)).collect()),
        }),
    );
}
fn link(id: u64) -> Link {
    Link {
        dependent: object(id),
        kind: "n".into(),
        binding: Binding::Object,
    }
}
fn diamond() -> Graph {
    let mut g = Graph::default();
    g.roots.insert(root(), Fact::Present(object(1)));
    node(&mut g, 1, Held::Root(root()), &[2, 3, 2]);
    node(&mut g, 2, held(2), &[4]);
    node(&mut g, 3, held(3), &[4]);
    node(&mut g, 4, held(4), &[]);
    g
}
fn edit(g: &mut Graph, id: u64) -> &mut Node {
    let Fact::Present(node) = g.nodes.get_mut(&object(id)).unwrap() else {
        panic!("fixture node")
    };
    node
}

#[test]
fn dependency_diamonds_keep_every_direct_edge_and_parallel_binding() {
    let mut g = diamond();
    edit(&mut g, 2).described = "same description".into();
    edit(&mut g, 3).described = "same description".into();
    let Fact::Present(edges) = &mut edit(&mut g, 1).dependents else {
        unreachable!()
    };
    edges.insert(Link {
        binding: Binding::Argument {
            position: 0,
            array: true,
        },
        ..link(2)
    });
    let c = g.closure(&root()).unwrap();
    assert_eq!(c.nodes.len(), 4);
    assert_eq!(c.surfaces.len(), 3);
    assert_eq!(c.edges.len(), 5);
    assert!(c.edges.contains(&(object(2), link(4))));
    assert!(c.edges.contains(&(object(3), link(4))));
}

#[test]
fn dependency_cycles_and_ownership_cycles_are_named() {
    for next in [1, 4] {
        let mut g = diamond();
        edit(&mut g, 4).dependents = Fact::Present([link(next)].into());
        let why = refusal(&g);
        assert!(why.contains("cycle"));
        assert!(why.contains("node 4"));
    }
    let mut g = diamond();
    edit(&mut g, 2).held = owned(3, Ownership::PartitionClone);
    edit(&mut g, 3).held = owned(2, Ownership::PartitionClone);
    assert!(refusal(&g).contains("ownership cycle"));
    let mut g = diamond();
    node(&mut g, 5, owned(1, Ownership::ReturnRule), &[]);
    for binding in [Binding::Ownership(Ownership::ReturnRule), Binding::Column] {
        edit(&mut g, 1).dependents = Fact::Present(
            [Link {
                binding: binding.clone(),
                ..link(5)
            }]
            .into(),
        );
        assert_eq!(
            g.closure(&root()).is_ok(),
            matches!(binding, Binding::Ownership(_))
        );
    }
}

#[test]
fn owned_clones_keep_their_edges_without_duplicate_restorations() {
    let mut g = diamond();
    edit(&mut g, 1).dependents = Fact::Present([link(3)].into());
    edit(&mut g, 3).held = owned(2, Ownership::PartitionClone);
    let c = g.closure(&root()).unwrap();
    assert_eq!(c.edges.len(), 3);
    assert_eq!(c.surfaces.len(), 2);
    assert!(!c.surfaces.contains_key(&object(3)));
    assert!(c.surfaces.contains_key(&object(4)));
    assert_eq!(c.nodes[&object(3)].held, edit(&mut g, 3).held);
}

#[test]
fn missing_unreadable_and_unvisited_facts_never_mean_empty() {
    for fact in [Fact::Absent, Fact::Unreadable("denied".into())] {
        let mut g = diamond();
        edit(&mut g, 4).dependents = fact;
        assert!(refusal(&g).contains("node 4"));
    }
    for fact in [Fact::Absent, Fact::Unreadable("denied".into())] {
        let mut g = diamond();
        g.roots.insert(root(), fact);
        assert!(g.closure(&root()).is_err());
    }
    let mut g = diamond();
    g.nodes.remove(&object(4));
    assert!(refusal(&g).contains("absent"));
    g.nodes
        .insert(object(4), Fact::Unreadable("hidden object".into()));
    assert!(refusal(&g).contains("unreadable"));
    let mut g = diamond();
    edit(&mut g, 1).held = held(1);
    assert!(refusal(&g).contains("binding disagrees"));
    let invalid = Root::Table("r_aaaaaa".parse().unwrap());
    g.roots.insert(invalid.clone(), Fact::Present(object(1)));
    edit(&mut g, 1).held = Held::Root(invalid.clone());
    assert!(g.closure(&invalid).unwrap_err().contains("table UID"));
    let mut g = diamond();
    edit(&mut g, 1).dependents = Fact::Present(BTreeSet::new());
    assert!(g.closure(&root()).unwrap().surfaces.is_empty());
}

#[test]
fn a_bound_external_key_is_evidence_rather_than_a_rebuild_surface() {
    let mut g = diamond();
    edit(&mut g, 1).dependents = Fact::Present(
        [Link {
            binding: Binding::ForeignKey { key: object(5) },
            ..link(2)
        }]
        .into(),
    );
    node(&mut g, 5, owned(6, Ownership::KeyIndex), &[]);
    node(&mut g, 6, held(6), &[99]);
    let c = g.closure(&root()).unwrap();
    assert_eq!(c.bindings.len(), 2);
    assert!(!c.surfaces.contains_key(&object(6)));
    g.nodes.remove(&object(6));
    assert!(g.closure(&root()).is_err());
    node(&mut g, 6, owned(5, Ownership::KeyIndex), &[]);
    assert!(refusal(&g).contains("binding ownership cycle"));
}

#[test]
fn module_compatibility_keeps_the_original_lists_and_empty_reads() {
    let module: ModuleId = ModuleId::Named("app.root".parse().unwrap());
    let second = ModuleId::Named("app.empty".parse().unwrap());
    let mut found = BTreeMap::new();
    let a = Dependent {
        described: "second".into(),
        holds: Holds::Unrepresentable("known legacy refusal".into()),
    };
    let b = Dependent {
        described: "first".into(),
        holds: Holds::Module(module.clone()),
    };
    found.insert(module.clone(), vec![a.clone(), b, a]);
    found.insert(second.clone(), Vec::new());
    let adapter = ModuleAdapter::new(&found);
    assert!(std::ptr::eq(adapter.get(&module).unwrap(), &found[&module]));
    assert!(adapter.get(&second).unwrap().is_empty());
    assert!(
        adapter
            .get(&ModuleId::Named("app.absent".parse().unwrap()))
            .is_none()
    );
    let mut g = diamond();
    g.roots.clear();
    g.roots
        .insert(Root::Module(module.clone()), Fact::Present(object(1)));
    edit(&mut g, 1).held = Held::Root(Root::Module(module.clone()));
    assert_eq!(g.closure(&Root::Module(module)).unwrap().edges.len(), 4);
}

#[test]
fn catalog_publication_and_managed_facts_are_required_even_with_a_complete_graph() {
    let mut catalog = Catalog {
        graph: diamond(),
        ..Catalog::default()
    };
    let declared = pbps_model::Schema::default();
    for fact in [Fact::Absent, Fact::Unreadable("SQLSTATE 42501".into())] {
        catalog.publications = fact;
        assert!(
            catalog
                .checked(&root(), &declared)
                .unwrap_err()
                .contains("publication")
        );
    }
    catalog.publications = Fact::Present(BTreeSet::new());
    assert!(
        catalog
            .checked(&root(), &declared)
            .unwrap_err()
            .contains("node 2")
    );
    catalog.publications =
        Fact::Present([("replication".into(), "app.parent".parse().unwrap())].into());
    let why = catalog.checked(&root(), &declared).unwrap_err();
    assert!(why.contains("replication") && why.contains("app.parent"));
}
