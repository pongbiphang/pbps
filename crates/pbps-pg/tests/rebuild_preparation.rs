use pbps_diff::{
    Side,
    order::{
        catalog::{Holds, graph::Binding, graph::*},
        rebuild::*,
    },
};
use pbps_model::*;
use std::collections::BTreeMap;
fn obj(id: u64) -> Object {
    Object {
        catalog: "pg_class".into(),
        id,
        attribute: 0,
    }
}
fn side<'a>(schema: &'a Schema, ids: &'a IdsFile) -> Side<'a> {
    Side { schema, ids }
}
struct Fixture {
    base: Schema,
    ids: IdsFile,
    desired: Schema,
    recorded: IdsFile,
    root: RebuildPartitionedTable,
    catalog: Catalog,
    naming: Naming,
    hints: Hints,
}
impl Fixture {
    fn new() -> Self {
        let base = pbps_diff::rebuild::member_cases::source();
        let table = pbps_diff::rebuild::member_cases::root();
        let ctx = pbps_diff::Context {
            operator: "maintainer".into(),
            today: "2026-10-24".into(),
        };
        let ids = pbps_diff::resolve(&base, &IdsFile::default(), &[], &ctx)
            .unwrap()
            .ids;
        let mut desired = base.clone();
        let parent = desired.tables.get_mut(&table).unwrap();
        parent.columns.get_mut("k").unwrap().ty = "bigint".parse().unwrap();
        parent
            .columns
            .insert("added".into(), Column::new("text".parse().unwrap()));
        parent.indexes.insert(
            "p_idx".into(),
            serde_json::from_str(r#"{"columns":[{"name":"k"}]}"#).unwrap(),
        );
        let pg = pbps_pg::Postgres::new();
        let recorded = pbps_diff::rebuild::record(
            side(&base, &ids),
            &desired,
            &ids,
            &table,
            10,
            &"widen key".parse().unwrap(),
            &ctx,
            &pg,
        )
        .unwrap();
        let root =
            pbps_diff::rebuild::qualify(side(&base, &ids), side(&desired, &recorded), &table, &pg)
                .unwrap();
        let mut f = Self {
            base,
            ids,
            desired,
            recorded,
            root,
            catalog: Catalog {
                spellings: Fact::Present(BTreeMap::new()),
                publications: Fact::Present(Default::default()),
                ..Default::default()
            },
            naming: Fact::Present(
                [(
                    "app".into(),
                    NamespaceNames {
                        exists: true,
                        ..Default::default()
                    },
                )]
                .into(),
            ),
            hints: Hints::default(),
        };
        let root = Root::Table(f.ids.table_uid(&table).unwrap().clone());
        f.catalog
            .graph
            .roots
            .insert(root.clone(), Fact::Present(obj(1)));
        f.node(1, Held::Root(root));
        for (i, name) in [table.clone(), TableName::new("app", "p1")]
            .into_iter()
            .enumerate()
        {
            let id = obj(i as u64 + 1);
            if i != 0 {
                f.node(
                    id.id,
                    Held::Owned {
                        by: obj(1),
                        kind: Ownership::Storage,
                    },
                );
                f.link(1, id.id);
            }
            f.catalog.relations.insert(
                id.clone(),
                Relation {
                    name: name.clone(),
                    row_type: obj(100 + id.id),
                    array_type: obj(200 + id.id),
                    attributes: Default::default(),
                },
            );
            let Fact::Present(names) = &mut f.naming else {
                unreachable!()
            };
            names
                .get_mut("app")
                .unwrap()
                .relations
                .insert(name.name.into(), id);
        }
        f
    }
    fn node(&mut self, id: u64, held: Held) {
        self.catalog.graph.nodes.insert(
            obj(id),
            Fact::Present(Node {
                described: format!("dependent {id}"),
                held,
                dependents: Fact::Present(Default::default()),
            }),
        );
    }
    fn link(&mut self, from: u64, to: u64) {
        let Fact::Present(target) = &self.catalog.graph.nodes[&obj(to)] else {
            unreachable!()
        };
        let binding = match &target.held {
            Held::Owned { kind, .. } => Binding::Ownership(kind.clone()),
            Held::Root(_) | Held::Dependent(_) => Binding::Object,
        };
        let Fact::Present(n) = self.catalog.graph.nodes.get_mut(&obj(from)).unwrap() else {
            unreachable!()
        };
        let Fact::Present(d) = &mut n.dependents else {
            unreachable!()
        };
        d.insert(Link {
            dependent: obj(to),
            kind: "n".into(),
            binding,
        });
    }
    fn input(&self) -> Input<'_> {
        Input {
            root: &self.root,
            before: side(&self.base, &self.ids),
            desired: side(&self.desired, &self.recorded),
            catalog: &self.catalog,
            naming: &self.naming,
            hints: &self.hints,
            mode: PlanMode::Transactional,
        }
    }
    fn recipes(
        &self,
        slots: &pbps_diff::order::slots::Slots,
        ids: &IdsFile,
        released: &std::collections::BTreeSet<pbps_model::operation::ObjectKey>,
    ) -> Result<Vec<BuildPartitionedIndex>, String> {
        let mut allocation = pbps_pg::partition_index::Allocation::new(
            &self.root,
            &self.catalog,
            &self.naming,
            slots,
            ids,
            released,
        );
        let definition = PartitionIndexDefinition::Index(
            self.root.after[&self.root.table].indexes["p_idx"].clone(),
        );
        let part = IndexPart::Index("p_idx".into());
        Ok(vec![BuildPartitionedIndex {
            parent: RebuildRelation {
                uid: ids.table_uid(&self.root.table).unwrap().clone(),
                name: self.root.table.clone(),
            },
            name: "p_idx".into(),
            clones: allocation.clones(&part, &definition)?,
            definition,
        }])
    }
    fn prepare(&self) -> Result<Prepared, String> {
        prepare(&self.input(), &pbps_pg::Postgres::new(), &mut |s, i, r| {
            self.recipes(s, i, r)
        })
    }
}
fn part(p: &Prepared, purpose: Purpose) -> &Change {
    let part = p.parts().iter().find(|p| p.purpose == purpose).unwrap();
    &part.change.change
}
#[test]
fn preparation_keeps_typed_parts_reserved_uids_and_readonly_target_state() {
    let f = Fixture::new();
    let p = f.prepare().unwrap();
    assert!(p.target().rebuilds.is_empty());
    assert_eq!(f.recorded.rebuilds.len(), 1);
    assert_eq!(p.target().tables, f.root.intent.replacement.tables);
    assert!(
        f.root
            .intent
            .source
            .tables
            .keys()
            .all(|u| p.target().tombstones.contains_key(u))
    );
    assert_eq!(p.addresses().len(), 4);
    let ops = p.operations();
    assert_eq!(ops.len(), p.slots().effects.len());
    assert!(p.slots().unowned.is_empty() && p.slots().unlowered.is_empty());
    assert!(matches!(
        part(&p, Purpose::Tree),
        Change::CreateTable { table, .. } if table.indexes.is_empty()
    ));
    let Change::CopyTableRows(copy) = part(&p, Purpose::Copy) else {
        panic!("copy")
    };
    assert_eq!(copy.source.name, f.root.table);
    assert_eq!(copy.max_rows, 10);
    assert!(
        copy.columns
            .iter()
            .any(|c| c.source.is_none() && c.target.name.raw() == "added")
    );
    assert!(ops.iter().all(|o| o.planned().risks
        == pbps_dialect::Dialect::change_risks(&pbps_pg::Postgres::new(), o.change())));
    let Change::BuildPartitionedIndex(recipe) = part(&p, Purpose::Own) else {
        panic!("recipe")
    };
    assert!(
        p.addresses()
            .iter()
            .any(|a| a.uid == recipe.parent.uid && a.address == recipe.parent.name)
    );
    assert_eq!(recipe.clones[0].name.raw(), "p1_k_idx");
}
#[test]
fn temporary_addresses_avoid_catalog_types_deterministically() {
    let mut f = Fixture::new();
    let uid = f.root.intent.source.tables.keys().next().unwrap();
    let name = format!("__pbps_rebuild_{uid}");
    let Fact::Present(n) = &mut f.naming else {
        unreachable!()
    };
    n.get_mut("app")
        .unwrap()
        .types
        .insert(name.clone().into(), obj(90));
    let p = f.prepare().unwrap();
    assert!(p.addresses().iter().all(|a| a.address.name != name));
    assert_eq!(p.addresses(), f.prepare().unwrap().addresses());
}
#[test]
fn callback_cannot_change_approved_definitions_coverage_or_reserved_members() {
    let f = Fixture::new();
    for case in 0..5 {
        let error = prepare(&f.input(), &pbps_pg::Postgres::new(), &mut |s, i, r| {
            let mut recipes = f.recipes(s, i, r)?;
            match case {
                0 => {
                    recipes.pop();
                }
                1 => {
                    recipes.push(recipes[0].clone());
                }
                2 => recipes[0].parent.uid = "t_zzzzzz".parse().unwrap(),
                3 => recipes[0].clones.clear(),
                4 => {
                    recipes[0].definition = PartitionIndexDefinition::Index(
                        serde_json::from_str(r#"{"columns":[{"name":"payload"}]}"#).unwrap(),
                    );
                }
                _ => unreachable!(),
            }
            Ok(recipes)
        })
        .unwrap_err();
        assert!(error.contains("recipe"), "{case}: {error}");
    }
}
#[test]
fn partial_unreadable_unmanaged_and_published_facts_are_named_before_callback() {
    for case in 0..8 {
        let mut f = Fixture::new();
        let expected = match case {
            0 => {
                f.naming = Fact::Absent;
                "occupancy"
            }
            1 => {
                f.naming = Fact::Unreadable("SQLSTATE 42501".into());
                "42501"
            }
            2 => {
                f.catalog.relations.clear();
                "app.p"
            }
            3 => {
                f.catalog.spellings = Fact::Absent;
                "spellings"
            }
            4 => {
                f.catalog.publications =
                    Fact::Present([("pub_all".into(), f.root.table.clone())].into());
                "pub_all"
            }
            5 => {
                f.node(
                    8,
                    Held::Dependent(Holds::Unrepresentable("extension dependent".into())),
                );
                f.link(1, 8);
                "dependent 8"
            }
            6 => {
                let Fact::Present(n) = &mut f.naming else {
                    unreachable!()
                };
                n.get_mut("app").unwrap().exists = false;
                "namespace"
            }
            7 => {
                f.root.intent.target_checksum = "stale".into();
                "payload"
            }
            _ => unreachable!(),
        };
        let mut called = false;
        let error = prepare(&f.input(), &pbps_pg::Postgres::new(), &mut |_, _, _| {
            called = true;
            Ok(vec![])
        })
        .unwrap_err();
        assert!(!called, "{error}");
        assert!(error.contains(expected), "{case}: {error}");
    }
    let f = Fixture::new();
    let mut input = f.input();
    input.mode = PlanMode::Staged;
    let pg = pbps_pg::Postgres::new();
    let minimal = pbps_dialect::MinimalDialect;
    assert!(
        prepare(&input, &pg, &mut |_, _, _| unreachable!())
            .unwrap_err()
            .contains("transactional")
    );
    assert!(
        prepare(&f.input(), &minimal, &mut |_, _, _| unreachable!())
            .unwrap_err()
            .contains("PostgreSQL")
    );
}
