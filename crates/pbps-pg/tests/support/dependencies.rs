//! New root evidence never changes the legacy module route's bytes.
use super::TestDb;
use pbps_diff::{
    Side,
    order::catalog::{Holds, Part, graph::*},
};
use pbps_model::{IdsFile, ModuleId, Schema};
use pbps_pg::modules::dependencies::collect;

fn source<'a>(schema: &'a Schema, ids: &'a IdsFile) -> Side<'a> {
    Side { schema, ids }
}
fn root() -> Root {
    Root::Table("t_aaaaaa".parse().unwrap())
}
fn ids() -> IdsFile {
    let mut ids = IdsFile::default();
    ids.tables
        .insert("t_aaaaaa".parse().unwrap(), "app.parent".parse().unwrap());
    ids
}
const TREE: &str = include_str!("rebuild_tree.sql");

#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
async fn table_catalog_keeps_physical_edges_bindings_and_clone_owners() {
    let mut db = TestDb::create("catalog2298b2").await;
    db.conn.execute(TREE).await.unwrap();
    let pulled = pbps_pg::catalog::introspect(&mut db.conn).await.unwrap();
    let ids = ids();
    db.conn.execute("BEGIN").await.unwrap();
    let found = collect(&mut db.conn, &root(), source(&pulled.schema, &ids))
        .await
        .unwrap();
    let closure = found.checked(&root(), &pulled.schema).unwrap();
    let spellings = found.spellings.require("typed source spellings").unwrap();
    let (clone, spelling) = spellings
        .iter()
        .find(|(_, s)| {
            s.table == "app.child".parse().unwrap()
                && s.name.raw() == "child_pkey"
                && s.kind == pbps_model::operation::NameKind::Index
        })
        .unwrap();
    assert_eq!(spelling.index_attributes, Some(vec!["id".into()]));
    assert!(matches!(&closure.nodes[&closure.owners[clone]].held,
        Held::Dependent(Holds::TablePart { table, part: Part::PrimaryKey(_) })
        if table == &"app.parent".parse().unwrap()));
    assert!(spellings.values().any(|s| s.table == spelling.table
        && s.name == spelling.name
        && s.kind == pbps_model::operation::NameKind::Constraint));
    assert!(found.graph.nodes.values().all(
        |n| !matches!(n, Fact::Present(Node { described, .. }) if described.contains("unrelated"))
    ));
    assert!(
        found
            .relations
            .values()
            .any(|r| r.name == "app.parent".parse().unwrap()
                && r.attributes.values().any(|(n, _)| n.raw() == "id")
                && r.row_type.id != 0
                && r.array_type.id != 0)
    );
    assert!(closure.nodes.values().any(|n| matches!(
        n.held,
        Held::Owned {
            kind: Ownership::ConstraintTrigger,
            ..
        }
    )));
    assert!(closure.nodes.values().any(|n| matches!(
        n.held,
        Held::Owned {
            kind: Ownership::PartitionClone,
            ..
        }
    )));
    let version = db
        .conn
        .query("SELECT current_setting('server_version_num')::int AS version")
        .await
        .unwrap()[0]
        .try_get::<i32>("version")
        .unwrap()
        .unwrap();
    if version >= 180000 {
        assert!(
            closure
                .nodes
                .values()
                .any(|n| n.described.contains("not_null")
                    && matches!(
                        n.held,
                        Held::Owned {
                            kind: Ownership::Attribute,
                            ..
                        }
                    ))
        );
    }
    for predicate in [
        Binding::Argument {
            position: 0,
            array: false,
        },
        Binding::Result { array: false },
        Binding::Argument {
            position: 0,
            array: true,
        },
        Binding::Result { array: true },
    ] {
        assert!(
            closure.edges.iter().any(|(_, e)| e.binding == predicate),
            "{predicate:?}"
        );
    }
    assert!(
        closure
            .edges
            .iter()
            .any(|(_, e)| matches!(e.binding, Binding::ForeignKey { .. }))
    );
    assert!(
        found
            .keys
            .values()
            .any(|key| !key.referenced_columns.is_empty() && key.columns.iter().all(|c| c.id != 0))
    );
    assert!(
        !closure
            .surfaces
            .values()
            .any(|d| d.described.contains("incoming_child") || d.described.contains("unrelated")),
        "clones are not independent restorations"
    );
    assert!(closure.surfaces.values().any(|d| matches!(
        d.holds,
        Holds::TablePart {
            part: Part::ForeignKey(_),
            ..
        }
    ) && d.described.contains("incoming")));
    assert!(closure.nodes.values().any(|n| matches!(
        n.held,
        Held::Owned {
            kind: Ownership::Storage,
            ..
        }
    )));
    assert_eq!(
        closure
            .surfaces
            .values()
            .filter(|d| matches!(
                d.holds,
                Holds::TablePart {
                    part: Part::Check(_),
                    ..
                }
            ))
            .count(),
        1
    );
    let diamond = closure
        .nodes
        .iter()
        .find(|(_, n)| n.described == "view app.diamond")
        .unwrap()
        .0;
    let rule = closure
        .nodes
        .iter()
        .find(|(_, n)| matches!(&n.held,Held::Owned{by,kind:Ownership::ReturnRule} if by==diamond))
        .unwrap()
        .0;
    assert!(
        closure
            .edges
            .iter()
            .filter(|(_, e)| &e.dependent == rule)
            .count()
            >= 2
    );
    let mut unmanaged = pulled.schema.clone();
    unmanaged
        .modules
        .remove(&"app.diamond".parse::<ModuleId>().unwrap());
    assert!(
        found
            .checked(&root(), &unmanaged)
            .unwrap_err()
            .contains("app.diamond")
    );
    let module: ModuleId = "app.left_view".parse().unwrap();
    let before = pbps_pg::modules::dependents(&mut db.conn, &module, pbps_model::ModuleKind::View)
        .await
        .unwrap();
    let module_root = Root::Module(module.clone());
    let module_facts = collect(&mut db.conn, &module_root, source(&pulled.schema, &ids))
        .await
        .unwrap();
    assert!(module_facts.checked(&module_root, &pulled.schema).is_ok());
    assert_eq!(
        before,
        pbps_pg::modules::dependents(&mut db.conn, &module, pbps_model::ModuleKind::View)
            .await
            .unwrap()
    );
    db.conn
        .execute("SAVEPOINT missing; DROP TABLE app.child CASCADE")
        .await
        .unwrap();
    assert!(
        collect(&mut db.conn, &root(), source(&pulled.schema, &ids))
            .await
            .unwrap_err()
            .to_string()
            .contains("app.child")
    );
    db.conn.execute("ROLLBACK TO missing").await.unwrap();
    db.conn
        .execute("CREATE MATERIALIZED VIEW app.unsupported AS SELECT id FROM app.parent; CREATE STATISTICS app.unsupported_stats ON id,code FROM app.parent")
        .await
        .unwrap();
    let unknown = collect(&mut db.conn, &root(), source(&pulled.schema, &ids))
        .await
        .unwrap();
    assert!(unknown.graph.nodes.values().any(|n| matches!(n,Fact::Present(Node{described,held:Held::Dependent(Holds::Unrepresentable(_)),..}) if described.contains("app.unsupported_stats"))));
    assert!(
        unknown
            .checked(&root(), &pulled.schema)
            .unwrap_err()
            .contains("app.unsupported")
    );
    db.conn.execute("DROP MATERIALIZED VIEW app.unsupported; DROP STATISTICS app.unsupported_stats; CREATE OR REPLACE VIEW app.left_view AS SELECT id, 1 AS code FROM app.diamond").await.unwrap();
    let cyclic = collect(&mut db.conn, &root(), source(&pulled.schema, &ids))
        .await
        .unwrap();
    assert!(
        cyclic
            .checked(&root(), &pulled.schema)
            .unwrap_err()
            .contains("cycle")
    );
    db.conn.execute("ROLLBACK").await.unwrap();
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
async fn outgoing_foreign_keys_keep_undeclared_candidate_keys_as_binding_evidence() {
    let mut db = TestDb::create("catalog2298binding").await;
    db.conn.execute("CREATE SCHEMA app;
        CREATE TABLE app.lookup(id integer PRIMARY KEY, code integer UNIQUE, memo text DEFAULT 'kept', CONSTRAINT lookup_check CHECK(id>=0));
        CREATE TABLE app.indexed(id integer NOT NULL) PARTITION BY RANGE(id);
        CREATE UNIQUE INDEX indexed_key ON app.indexed(id);
        CREATE TABLE app.indexed_child PARTITION OF app.indexed FOR VALUES FROM(0) TO(100);
        CREATE TABLE app.unrelated(id integer REFERENCES app.lookup(id), code integer REFERENCES app.lookup(code));
        CREATE TABLE app.parent(id integer PRIMARY KEY, pk integer REFERENCES app.lookup(id), uq integer REFERENCES app.lookup(code), ix integer REFERENCES app.indexed_child(id)) PARTITION BY RANGE(id);
        CREATE TABLE app.child PARTITION OF app.parent FOR VALUES FROM(0) TO(100)").await.unwrap();
    let mut schema = pbps_pg::catalog::introspect(&mut db.conn)
        .await
        .unwrap()
        .schema;
    schema.tables.retain(|name, _| {
        name == &"app.parent".parse().unwrap() || name == &"app.child".parse().unwrap()
    });
    // Declare this FK explicitly so collection does not depend on the pull's
    // support for a standalone candidate index on a partition leaf.
    schema
        .tables
        .get_mut(&"app.parent".parse().unwrap())
        .unwrap()
        .foreign_keys
        .insert(
            "parent_ix_fkey".into(),
            pbps_model::ForeignKey {
                columns: vec!["ix".into()],
                references_table: "app.indexed_child".parse().unwrap(),
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
    let ids = ids();
    db.conn.execute("BEGIN").await.unwrap();
    let found = collect(&mut db.conn, &root(), source(&schema, &ids)).await;
    db.conn.execute("ROLLBACK").await.unwrap();
    db.drop().await;
    let found = found.expect("outgoing FKs may reference undeclared candidate keys");
    let closure = found.checked(&root(), &schema).unwrap();
    for (foreign, candidate) in [
        ("parent_pk_fkey", "lookup_pkey"),
        ("parent_uq_fkey", "lookup_code_key"),
        ("parent_ix_fkey", "indexed_child_id_idx"),
    ] {
        let (foreign_id, _) = closure
            .nodes
            .iter()
            .find(|(_, n)| {
                matches!(
                    &n.held,
                    Held::Dependent(Holds::TablePart { part: Part::ForeignKey(name), .. })
                        if name.raw() == foreign
                )
            })
            .unwrap_or_else(|| panic!("missing FK {foreign}"));
        let key = &found.keys[foreign_id];
        assert_eq!(key.columns.len(), 1);
        assert_eq!(key.referenced_columns.len(), 1);
        assert!(closure.edges.iter().any(|(_, link)| {
            &link.dependent == foreign_id
                && link.binding
                    == Binding::ForeignKey {
                        key: key.index.clone(),
                    }
        }));
        let binding = &closure.bindings[&key.index];
        assert!(binding.described.contains(candidate), "{binding:?}");
        assert!(matches!(binding.dependents, Fact::Absent));
        assert!(!closure.nodes.contains_key(&key.index));
        assert!(!closure.surfaces.contains_key(&key.index));
    }
    for candidate in ["lookup_pkey", "lookup_code_key"] {
        let (id, _) = closure
            .bindings
            .iter()
            .find(|(id, n)| id.catalog == "pg_constraint" && n.described.contains(candidate))
            .unwrap_or_else(|| panic!("missing constraint evidence for {candidate}"));
        let key = &found.keys[id];
        assert_eq!(key.columns.len(), 1);
        assert!(key.referenced_columns.is_empty());
        assert!(found.keys.values().any(|foreign| {
            foreign.index == key.index && foreign.referenced_columns == key.columns
        }));
        assert!(closure.bindings.contains_key(&key.index));
        assert!(!closure.surfaces.contains_key(id));
    }
    assert!(found.graph.nodes.values().all(|fact| !matches!(
        fact, Fact::Present(Node { described, .. })
            if described.contains("unrelated") || described.contains("lookup_check")
                || described.contains("memo")
    )));
}

#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
async fn an_undeclared_primary_key_in_the_rebuilt_tree_is_refused() {
    let mut db = TestDb::create("catalog2298unmanagedpk").await;
    db.conn
        .execute(
            "CREATE SCHEMA app;
        CREATE TABLE app.parent(id integer PRIMARY KEY) PARTITION BY RANGE(id);
        CREATE TABLE app.child PARTITION OF app.parent FOR VALUES FROM(0) TO(100)",
        )
        .await
        .unwrap();
    let mut schema = pbps_pg::catalog::introspect(&mut db.conn)
        .await
        .unwrap()
        .schema;
    schema
        .tables
        .get_mut(&"app.parent".parse().unwrap())
        .unwrap()
        .primary_key = None;
    let ids = ids();
    db.conn.execute("BEGIN").await.unwrap();
    let error = collect(&mut db.conn, &root(), source(&schema, &ids))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("parent_pkey") && error.contains("source primary key is unmanaged"),
        "{error}"
    );
    db.conn.execute("ROLLBACK").await.unwrap();
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
async fn publication_modes_missing_trees_and_unreadable_catalogs_are_named() {
    let mut db = TestDb::create("catalog2298pub").await;
    db.conn.execute("CREATE SCHEMA app; CREATE TABLE app.parent(id integer NOT NULL) PARTITION BY RANGE(id)").await.unwrap();
    let pulled = pbps_pg::catalog::introspect(&mut db.conn).await.unwrap();
    let ids = ids();
    assert!(
        collect(&mut db.conn, &root(), source(&pulled.schema, &ids))
            .await
            .unwrap_err()
            .to_string()
            .contains("transaction")
    );
    db.conn.execute("BEGIN").await.unwrap();
    assert!(
        collect(&mut db.conn, &root(), source(&pulled.schema, &ids))
            .await
            .unwrap()
            .checked(&root(), &pulled.schema)
            .is_ok()
    );
    db.conn.execute("CREATE SCHEMA empty_names; CREATE DOMAIN app.d AS integer CONSTRAINT shared_name CHECK(VALUE>0); CREATE TABLE app.other(k integer CONSTRAINT shared_name CHECK(k>0)); CREATE SEQUENCE app.seq; CREATE VIEW app.v AS SELECT 1 AS k").await.unwrap();
    let schemas = ["app".into(), "empty_names".into(), "missing_names".into()].into();
    let names = pbps_pg::partition_index::collect_names(&mut db.conn, &schemas)
        .await
        .unwrap();
    let names = names.require("all schemas").unwrap();
    assert!(names["app"].types.contains_key("d"));
    assert!(names["app"].types.contains_key("_other"));
    assert!(names["app"].relations.contains_key("seq"));
    assert!(names["app"].relations.contains_key("v"));
    assert_eq!(names["app"].constraints["shared_name"].len(), 2);
    assert!(names["empty_names"].exists && names["empty_names"].relations.is_empty());
    assert!(!names["missing_names"].exists);
    for definition in [
        "FOR TABLE app.parent WITH (publish_via_partition_root=true)",
        "FOR ALL TABLES",
        "FOR TABLES IN SCHEMA app",
    ] {
        db.conn
            .execute(&format!("CREATE PUBLICATION pub2298 {definition}"))
            .await
            .unwrap();
        let found = collect(&mut db.conn, &root(), source(&pulled.schema, &ids))
            .await
            .unwrap();
        let error = found.checked(&root(), &pulled.schema).unwrap_err();
        assert!(
            error.contains("pub2298") && error.contains("app.parent"),
            "{error}"
        );
        db.conn.execute("DROP PUBLICATION pub2298").await.unwrap();
    }
    db.conn.execute("CREATE TABLE app.child PARTITION OF app.parent FOR VALUES FROM(0) TO(100); CREATE PUBLICATION pub2298 FOR TABLE app.parent WITH (publish_via_partition_root=true)").await.unwrap();
    let mut child_ids = IdsFile::default();
    child_ids
        .tables
        .insert("t_aaaaaa".parse().unwrap(), "app.child".parse().unwrap());
    let child_schema = pbps_pg::catalog::introspect_within_transaction(&mut db.conn)
        .await
        .unwrap()
        .schema;
    assert!(
        collect(&mut db.conn, &root(), source(&child_schema, &child_ids))
            .await
            .unwrap()
            .checked(&root(), &child_schema)
            .unwrap_err()
            .contains("pub2298")
    );
    db.conn.execute("DROP PUBLICATION pub2298").await.unwrap();
    let role = format!("catalog2298_reader_{}", std::process::id());
    for catalog in [
        "pg_depend",
        "pg_publication",
        "pg_class",
        "pg_type",
        "pg_constraint",
    ] {
        db.conn.execute("SAVEPOINT permissions").await.unwrap();
        db.conn.execute(&format!("CREATE ROLE {role}; GRANT USAGE ON SCHEMA app TO {role}; REVOKE SELECT ON pg_catalog.{catalog} FROM PUBLIC; SET LOCAL ROLE {role}")).await.unwrap();
        let error = if ["pg_class", "pg_type", "pg_constraint"].contains(&catalog) {
            pbps_pg::partition_index::collect_names(&mut db.conn, &schemas)
                .await
                .unwrap_err()
        } else {
            collect(&mut db.conn, &root(), source(&child_schema, &ids))
                .await
                .unwrap_err()
        };
        assert!(error.to_string().contains("42501"), "{error:?}");
        let mut cause = &error;
        while let pbps_db::DbError::Context { source, .. } = cause {
            cause = source;
        }
        assert!(
            matches!(cause,pbps_db::DbError::Driver{code:Some(code),..} if code=="42501"),
            "{error:?}"
        );
        if ["pg_class", "pg_type", "pg_constraint"].contains(&catalog) {
            assert!(error.to_string().contains("unreadable"), "{error}");
            assert!(format!("{error:?}").contains("42501"));
        }
        db.conn.execute("ROLLBACK TO permissions").await.unwrap();
    }
    db.conn
        .execute("DROP TABLE app.parent CASCADE")
        .await
        .unwrap();
    assert!(
        collect(&mut db.conn, &root(), source(&pulled.schema, &ids))
            .await
            .unwrap_err()
            .to_string()
            .contains("app.parent")
    );
    db.conn.execute("ROLLBACK").await.unwrap();
    db.drop().await;
}

#[test]
fn owned_clone_releases_feed_the_same_slot_edges_and_replay() {
    use pbps_diff::order::catalog::graph::Binding;
    use pbps_diff::order::{
        catalog::{Holds, Part, graph::*},
        slots,
    };
    use pbps_model::operation::{
        ModuleEdits, Operations, Origin, PlanOwners, Snapshot, Source as OriginSource,
    };
    use pbps_model::plan_names::PlanNames;
    use pbps_model::*;
    #[path = "../../../pbps-model/tests/support/rebuild.rs"]
    mod fixture;
    let dialect = pbps_pg::Postgres::new();
    use pbps_dialect::slots::{CatalogFacts, Namespace, Occupant, SlotKey};
    use pbps_model::operation::NameKind;
    let (mut root, base, desired, addresses) = fixture::approval();
    let mut schema = Schema {
        tables: root.before.clone(),
        ..Default::default()
    };
    let leaf = root
        .before
        .keys()
        .find(|n| **n != root.table)
        .unwrap()
        .clone();
    root.after.get_mut(&leaf).unwrap().partition_of = Some(PartitionOf {
        parent: root.table.clone(),
        bound: PartitionBound::Default,
        columns: Default::default(),
    });
    let r = fixture::index_recipe();
    let pbps_model::PartitionIndexDefinition::Index(index) = r.definition.clone() else {
        panic!()
    };
    schema
        .tables
        .get_mut(&root.table)
        .unwrap()
        .indexes
        .insert("p_idx".into(), index.clone());
    let ids = base.index();
    let old_leaf = ids.table_uid(&leaf).unwrap().clone();
    let obj = |id| Object {
        catalog: "pg_class".into(),
        id,
        attribute: 0,
    };
    let link = |dependent| Link {
        binding: if dependent == obj(3) {
            Binding::Ownership(Ownership::PartitionClone)
        } else {
            Binding::Object
        },
        dependent,
        kind: "n".into(),
    };
    let node = |held, children: Vec<Object>| {
        Fact::Present(Node {
            described: "typed index fact".into(),
            held,
            dependents: Fact::Present(children.into_iter().map(link).collect()),
        })
    };
    let key = Root::Table(ids.table_uid(&root.table).unwrap().clone());
    let mut catalog = Catalog::default();
    catalog
        .graph
        .roots
        .insert(key.clone(), Fact::Present(obj(1)));
    catalog.graph.nodes = [
        (obj(1), node(Held::Root(key.clone()), vec![obj(2)])),
        (
            obj(2),
            node(
                Held::Dependent(Holds::TablePart {
                    table: root.table.clone(),
                    part: Part::Index("p_idx".into()),
                }),
                vec![obj(3)],
            ),
        ),
        (
            obj(3),
            node(
                Held::Owned {
                    by: obj(2),
                    kind: Ownership::PartitionClone,
                },
                vec![],
            ),
        ),
    ]
    .into();
    catalog.spellings = Fact::Present(
        [(
            obj(3),
            PartSpelling {
                table: leaf.clone(),
                name: "p1_k_idx".into(),
                kind: NameKind::Index,
                index_attributes: Some(vec!["k".into()]),
            },
        )]
        .into(),
    );
    let slot = SlotKey {
        namespace: Namespace::Relations("app".into()),
        name: "p1_k_idx".into(),
    };
    let facts = CatalogFacts {
        occupants: vec![Occupant {
            slot: slot.clone(),
            kind: "inherited index".into(),
            owner: Some(leaf),
            column: None,
        }],
        ..Default::default()
    };
    let logical = [PlannedChange::new(Change::RebuildPartitionedTable(
        Box::new(root.clone()),
    ))];
    let owners = PlanOwners::new(PlanNames::of(&logical, &desired).unwrap(), &base, &desired)
        .with_rebuild_addresses(&root, &desired, &addresses)
        .unwrap();
    let before = Snapshot {
        schema: &schema,
        ids: &base,
    };
    let parts = [
        Change::AddIndex {
            table: "app.tmp_p1".parse().unwrap(),
            name: "p1_k_idx".into(),
            index: Box::new(index),
            clustered: false,
        },
        Change::DropIndex {
            table: root.table.clone(),
            name: "p_idx".into(),
        },
        Change::DropTable {
            uid: old_leaf,
            name: "app.old_p1".parse().unwrap(),
            detach_from: None,
        },
        Change::DropTable {
            uid: base.table_uid(&root.table).unwrap().clone(),
            name: "app.old_p".parse().unwrap(),
            detach_from: None,
        },
    ];
    for order in [[0, 1, 2, 3], [1, 0, 2, 3]] {
        let ops = Operations::expanded(
            1,
            order.map(|i| {
                (
                    match i {
                        0 => Origin::Restoration(OriginSource::Change(0)),
                        1 => Origin::Teardown(OriginSource::Change(0)),
                        2 | 3 => Origin::SplitPart(0),
                        _ => unreachable!(),
                    },
                    PlannedChange::new(parts[i].clone()),
                )
            }),
            ModuleEdits::InPlace,
        );
        let plain = slots::analyse(ops.as_slice(), &owners, before, &dialect);
        let enriched =
            slots::with_catalog_parts(plain, &catalog, &key, &owners, before, &dialect).unwrap();
        let holder = enriched.catalog_owned.values().next().unwrap().clone();
        let release = order.iter().position(|i| *i == 1).unwrap();
        let carried = vec![slots::Carried {
            occupant: 0,
            slot: slot.clone(),
            step: 2,
            lands: None,
        }];
        let edges = slots::edges(&enriched, Some(&facts), &carried);
        let claim = order.iter().position(|i| *i == 0).unwrap();
        assert!(
            edges
                .iter()
                .any(|e| e.before == release && e.after == claim)
        );
        assert!(!edges.iter().any(|e| e.before >= 2 && e.after == claim));
        let mut allocation_slots = enriched.clone();
        allocation_slots.effects[claim].claims.clear();
        let occupancy = Fact::Present(
            [(
                "app".into(),
                NamespaceNames {
                    exists: true,
                    relations: [("p1_k_idx".into(), obj(3))].into(),
                    ..Default::default()
                },
            )]
            .into(),
        );
        let freed = allocation_slots.effects[release]
            .releases
            .iter()
            .map(|(_, h)| h.clone())
            .collect();
        let definition = fixture::index_recipe().definition;
        let mut allocator = pbps_pg::partition_index::Allocation::new(
            &root,
            &catalog,
            &occupancy,
            &allocation_slots,
            &desired,
            &freed,
        );
        assert_eq!(
            allocator.clones(&r.part(), &definition).unwrap()[0]
                .name
                .raw(),
            "p1_k_idx"
        );
        assert_eq!(
            allocator.clones(&r.part(), &definition).unwrap()[0]
                .name
                .raw(),
            "p1_k_idx1"
        );
        let allocate = |root: &RebuildPartitionedTable,
                        catalog: &Catalog,
                        naming: &Naming,
                        definition: &PartitionIndexDefinition| {
            pbps_pg::partition_index::Allocation::new(
                root,
                catalog,
                naming,
                &allocation_slots,
                &desired,
                &freed,
            )
            .clones(&r.part(), definition)
        };
        let Fact::Present(mut occupied) = occupancy.clone() else {
            panic!()
        };
        occupied
            .get_mut("app")
            .unwrap()
            .constraints
            .insert("p1_k_idx".into(), [obj(21), obj(22)].into());
        let occupied = Fact::Present(occupied);
        assert_eq!(
            allocate(&root, &catalog, &occupied, &definition).unwrap()[0]
                .name
                .raw(),
            "p1_k_idx1"
        );
        for (missing, expected) in [
            (Fact::Absent, "absent"),
            (Fact::Unreadable("42501".into()), "42501"),
            (Fact::Present(Default::default()), "app.p1"),
        ] {
            assert!(
                allocate(&root, &catalog, &missing, &definition)
                    .unwrap_err()
                    .contains(expected)
            );
        }
        let PartitionIndexDefinition::Index(mut duplicates) = definition.clone() else {
            panic!()
        };
        duplicates.columns.push(duplicates.columns[0].clone());
        assert_eq!(
            allocate(
                &root,
                &catalog,
                &occupancy,
                &PartitionIndexDefinition::Index(duplicates)
            )
            .unwrap()[0]
                .name
                .raw(),
            "p1_k_k1_idx"
        );
        let PartitionIndexDefinition::Index(mut expression) = definition.clone() else {
            panic!()
        };
        expression.columns[0].key = IndexKey::Expression("lower(k::text)".into());
        let expression = PartitionIndexDefinition::Index(expression);
        let mut approved = root.clone();
        let PartitionIndexDefinition::Index(index) = &expression else {
            panic!()
        };
        approved
            .before
            .get_mut(&root.table)
            .unwrap()
            .indexes
            .insert("p_idx".into(), index.clone());
        let mut labels = catalog.clone();
        let Fact::Present(spellings) = &mut labels.spellings else {
            panic!()
        };
        spellings.insert(
            obj(2),
            PartSpelling {
                table: root.table.clone(),
                name: "p_idx".into(),
                kind: NameKind::Index,
                index_attributes: Some(vec!["lower".into()]),
            },
        );
        assert_eq!(
            allocate(&approved, &labels, &occupancy, &expression).unwrap()[0]
                .name
                .raw(),
            "p1_lower_idx"
        );
        assert!(
            allocate(&root, &labels, &occupancy, &expression)
                .unwrap_err()
                .contains("new or changed expression")
        );
        labels.spellings = Fact::Unreadable("42501".into());
        assert!(
            allocate(&approved, &labels, &occupancy, &expression)
                .unwrap_err()
                .contains("42501")
        );

        let mut schedule = pbps_diff::order::Schedule::new(4);
        slots::schedule(&mut schedule, &edges);
        for (before, after) in [(claim, 2), (2, 3)] {
            schedule.edge(
                before,
                after,
                pbps_diff::order::Source::Effect,
                pbps_model::resolver::OrderReason::Restoration,
            );
        }
        let settled = schedule
            .settle(pbps_diff::order::propose(ChangeSet {
                changes: ops.iter().map(|op| op.planned().clone()).collect(),
            }))
            .unwrap();
        assert_eq!(settled.sequence, [release, claim, 2, 3]);
        // This clone-slot fragment omits temp-tree creation and name swaps.
        // Keep its original claim/release oracle independent of input-lifetime
        // coverage, which the complete rebuild_read_boundary proof supplies.
        let mut clone_slots = enriched.clone();
        for fx in &mut clone_slots.effects {
            fx.reads.clear();
        }
        let (violations, _) = slots::verify(
            ops.as_slice(),
            &clone_slots,
            &dialect,
            Some(&facts),
            &owners,
            before,
        );
        assert_eq!(violations.is_empty(), release < claim);
        let released = [holder].into();
        let mut vacant = enriched.clone();
        for fx in &mut vacant.effects {
            fx.claims.clear();
        }
        assert!(
            !slots::unavailable_names(
                &vacant,
                &Fact::Present(facts.clone()),
                std::slice::from_ref(&slot.namespace),
                &released
            )
            .unwrap()
            .contains("p1_k_idx")
        );
        assert!(
            slots::unavailable_names(
                &enriched,
                &Fact::Present(facts.clone()),
                std::slice::from_ref(&slot.namespace),
                &released
            )
            .unwrap()
            .contains("p1_k_idx"),
            "the new independent claim reserves its name"
        );
    }
    // A reintroduced incarnation still belongs to its eventual container drop.
    let ordinary = PlanOwners::new(PlanNames::of(&[], &base).unwrap(), &base, &base);
    let Change::AddIndex { index, .. } = &parts[0] else {
        panic!()
    };
    let returned = Operations::expanded(
        1,
        [
            (Origin::Teardown(OriginSource::Change(0)), parts[1].clone()),
            (
                Origin::Restoration(OriginSource::Change(0)),
                Change::AddIndex {
                    table: root.table.clone(),
                    name: "p_idx".into(),
                    index: index.clone(),
                    clustered: false,
                },
            ),
            (
                Origin::SplitPart(0),
                Change::DropTable {
                    uid: base.table_uid(&root.table).unwrap().clone(),
                    name: root.table.clone(),
                    detach_from: None,
                },
            ),
        ]
        .map(|(origin, change)| (origin, PlannedChange::new(change))),
        ModuleEdits::InPlace,
    );
    let returned = slots::with_catalog_parts(
        slots::analyse(returned.as_slice(), &ordinary, before, &dialect),
        &catalog,
        &key,
        &ordinary,
        before,
        &dialect,
    )
    .unwrap();
    assert!(
        returned.effects[2]
            .releases
            .contains(&(slot.clone(), returned.catalog_owned[&slot].clone()))
    );
    catalog.spellings = Fact::Unreadable("42501".into());
    assert!(
        slots::with_catalog_parts(
            slots::Slots::default(),
            &catalog,
            &key,
            &owners,
            before,
            &dialect
        )
        .unwrap_err()
        .contains("42501")
    );
}

#[path = "rebuild_live.rs"]
mod execution;

#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
async fn references_sort_what_a_stored_default_binds_and_what_the_facts_cannot_hold() {
    use pbps_model::operation::Snapshot;
    use pbps_pg::modules::dependencies::references;
    let mut db = TestDb::create("references2296").await;
    db.conn
        .execute(
            "CREATE SCHEMA app;
        CREATE FUNCTION app.f(integer) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT $1';
        CREATE FUNCTION app.f2(integer, integer) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT $1 + $2';
        CREATE TABLE app.other(id integer);
        CREATE SEQUENCE app.free;
        CREATE OPERATOR app.|+| (LEFTARG = integer, RIGHTARG = integer, FUNCTION = app.f2);
        CREATE TYPE app.mood AS ENUM ('ok');
        CREATE TABLE app.parent(k integer NOT NULL, a integer DEFAULT 7, b integer DEFAULT app.f(1),
          c serial, e regclass DEFAULT 'app.other'::regclass, f integer DEFAULT nextval('app.free'),
          g integer GENERATED ALWAYS AS (a * 2) STORED,
          h integer DEFAULT 1 OPERATOR(app.|+|) 2, i app.mood DEFAULT 'ok',
          j integer DEFAULT 1 + 2, l text DEFAULT 'x' || 'y')",
        )
        .await
        .unwrap();
    let version = db
        .conn
        .query("SELECT current_setting('server_version_num')::int AS version")
        .await
        .unwrap()[0]
        .try_get::<i32>("version")
        .unwrap()
        .unwrap();
    let schema = Schema::default();
    let ids = ids();
    let before = Snapshot {
        schema: &schema,
        ids: &ids,
    };
    let tables = [ids.tables.keys().next().unwrap().clone()].into();
    // The read is the caller's transaction's, or it is refused.
    assert!(references(&mut db.conn, before, &tables).await.is_err());
    db.conn.execute("BEGIN").await.unwrap();
    let found = references(&mut db.conn, before, &tables).await;
    let missing = references(&mut db.conn, before, &["t_bbbbbb".parse().unwrap()].into()).await;
    db.conn.execute("ROLLBACK").await.unwrap();
    db.drop().await;
    assert!(
        missing.is_err(),
        "a table the baseline does not name is not empty"
    );
    let found = found.unwrap();
    let at = |name: &str| {
        &found.columns[&"app.parent"
            .parse::<pbps_model::TableName>()
            .unwrap()
            .column(name)]
    };
    let routine = ModuleId::Routine(pbps_model::RoutineId::new(
        pbps_model::ObjectName::new("app", "f"),
        vec!["integer".parse().unwrap()],
    ));
    assert_eq!(at("a").default.as_deref(), Some("7"));
    assert!(at("a").routines.is_empty() && !at("a").unrepresented);
    assert_eq!(at("b").routines, [routine].into());
    assert!(!at("b").unrepresented);
    assert_eq!(
        at("c").sequences,
        ["app.parent.c".parse().unwrap()].into(),
        "a serial's sequence is owned by its column"
    );
    assert!(!at("c").unrepresented);
    assert!(
        at("e").unrepresented,
        "a relation is not a representable binding"
    );
    assert!(
        at("f").unrepresented,
        "a sequence nobody owns is not representable"
    );
    assert!(!at("g").unrepresented && at("g").default.is_some());
    // A normal dependency of any other catalog is just as unrepresented: a
    // user-defined operator and a user-defined type. Built-in operators are pinned and bind nothing.
    assert!(
        at("h").unrepresented,
        "a user-defined operator is not a representable binding"
    );
    assert!(
        at("i").unrepresented,
        "a user-defined type is not a representable binding"
    );
    for built_in in ["j", "l"] {
        assert!(
            !at(built_in).unrepresented && at(built_in).routines.is_empty(),
            "{built_in}: built-in operators and types are not dependencies"
        );
    }
    assert_eq!(at("k").default, None);
    assert_eq!(at("k").local_not_null, version >= 180000);
    assert!(!at("a").local_not_null);
}
