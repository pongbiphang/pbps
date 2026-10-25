//! The factory is exercised against complete physical binding/ownership facts.
use super::*;
use pbps_diff::order::rebuild::*;
use pbps_model::*;

#[tokio::test]
#[ignore = "needs a live PostgreSQL; set PBPS_TEST_PG_DB"]
async fn expansion_replays_own_parts_external_bindings_and_member_lifetimes() {
    let mut db = TestDb::create("factory2298").await;
    db.conn.execute(TREE).await.unwrap();
    db.conn.execute("CREATE INDEX parent_plain ON app.parent(id); CREATE INDEX child_memo ON app.child(memo)").await.unwrap();
    let role = super::super::least_privilege_role(&mut db, "factory2298").await;
    db.conn.execute(&format!("GRANT SELECT ON app.parent,app.left_view TO {role}; GRANT EXECUTE ON FUNCTION app.composite(app.parent) TO {role}")).await.unwrap();
    let base = pbps_pg::catalog::introspect(&mut db.conn)
        .await
        .unwrap()
        .schema;
    let ctx = pbps_diff::Context {
        operator: "maintainer".into(),
        today: "2026-10-24".into(),
    };
    let ids = pbps_diff::resolve(&base, &IdsFile::default(), &[], &ctx)
        .unwrap()
        .ids;
    let table: TableName = "app.parent".parse().unwrap();
    db.conn.execute("BEGIN").await.unwrap();
    let catalog_root = Root::Table(ids.table_uid(&table).unwrap().clone());
    let catalog = collect(&mut db.conn, &catalog_root, source(&base, &ids))
        .await
        .unwrap();
    let mut unread = catalog.clone();
    unread.publications = Fact::Unreadable("42501 publication membership".into());
    assert!(
        unread
            .checked(&catalog_root, &base)
            .unwrap_err()
            .contains("42501")
    );
    let naming = pbps_pg::partition_index::collect_names(&mut db.conn, &["app".into()].into())
        .await
        .unwrap();
    let hints = Hints {
        public_execute: base
            .modules
            .keys()
            .filter(|m| matches!(m, ModuleId::Routine(_)))
            .cloned()
            .collect(),
        ..Hints::default()
    };
    let pg = pbps_pg::Postgres::new();
    for case in 0..3 {
        let mut desired = base.clone();
        desired
            .tables
            .get_mut(&table)
            .unwrap()
            .columns
            .get_mut("id")
            .unwrap()
            .ty = "bigint".parse().unwrap();
        if case == 1 {
            desired.tables.remove(&"app.child".parse().unwrap());
            desired
                .tables
                .get_mut(&table)
                .unwrap()
                .columns
                .shift_remove("memo");
        }
        if case == 2 {
            let mut leaf = base.tables[&"app.child".parse::<TableName>().unwrap()].clone();
            leaf.indexes.clear();
            leaf.partition_of.as_mut().unwrap().bound = PartitionBound::Range {
                from: vec![BoundDatum::Value("100".into())],
                to: vec![BoundDatum::Value("200".into())],
            };
            desired.tables.insert("app.added".parse().unwrap(), leaf);
            desired
                .tables
                .get_mut(&table)
                .unwrap()
                .columns
                .insert("added".into(), Column::new("text".parse().unwrap()));
        }
        let recorded = pbps_diff::rebuild::record(
            source(&base, &ids),
            &desired,
            &ids,
            &table,
            10,
            &"widen key".parse().unwrap(),
            &ctx,
            &pg,
        )
        .unwrap();
        let root = pbps_diff::rebuild::qualify(
            source(&base, &ids),
            source(&desired, &recorded),
            &table,
            &pg,
        )
        .unwrap();
        let input = Input {
            root: &root,
            before: source(&base, &ids),
            desired: source(&desired, &recorded),
            catalog: &catalog,
            naming: &naming,
            hints: &hints,
            mode: PlanMode::Transactional,
        };
        let prepared = prepare(&input, &pg, &mut |slots, target, released| {
            let mut allocation = pbps_pg::partition_index::Allocation::new(
                &root, &catalog, &naming, slots, target, released,
            );
            index_definitions(&desired.tables[&table])
                .into_iter()
                .map(|(part, mut definition)| {
                    let name = match &mut definition {
                        PartitionIndexDefinition::PrimaryKey(key) => {
                            let name = key.name.clone().unwrap_or_else(|| {
                                pbps_pg::partition_index::clone_name(
                                    &table,
                                    &part,
                                    &Fact::Absent,
                                    &Default::default(),
                                )
                                .unwrap()
                            });
                            key.name = Some(name.clone());
                            name
                        }
                        PartitionIndexDefinition::Unique(_) => {
                            let IndexPart::Unique(n) = &part else {
                                unreachable!()
                            };
                            n.clone()
                        }
                        PartitionIndexDefinition::Index(_) => {
                            let IndexPart::Index(n) = &part else {
                                unreachable!()
                            };
                            n.clone()
                        }
                    };
                    Ok(BuildPartitionedIndex {
                        parent: RebuildRelation {
                            uid: target.table_uid(&table).unwrap().clone(),
                            name: table.clone(),
                        },
                        name,
                        clones: allocation.clones(&part, &definition)?,
                        definition,
                    })
                })
                .collect()
        })
        .unwrap();
        let canonical = prepared.operations();
        let purposes: std::collections::BTreeMap<_, _> = canonical
            .iter()
            .zip(prepared.parts())
            .map(|(op, p)| (op.id(), p.purpose))
            .collect();
        let done = expand(&input, prepared, &pg).unwrap_or_else(|e| panic!("case {case}: {e}"));
        assert_eq!(done.operations.len(), canonical.len());
        done.changes
            .proof()
            .validate(done.changes.changes())
            .unwrap();
        let ops = done.operations.as_slice();
        for op in ops {
            let original = canonical.iter().find(|p| p.id() == op.id()).unwrap();
            assert_eq!(op.change(), original.change());
            assert_eq!(op.origin(), original.origin());
        }
        let positions = |purpose| {
            ops.iter()
                .enumerate()
                .filter_map(|(i, op)| (purposes[&op.id()] == purpose).then_some(i))
                .collect::<Vec<_>>()
        };
        let copy = positions(Purpose::Copy)[0];
        let analyze = positions(Purpose::Analyze)[0];
        let swaps = positions(Purpose::Swap);
        for own in positions(Purpose::Own) {
            assert!(copy < own && own < analyze);
        }
        for swap in swaps {
            assert!(analyze < swap);
            for restore in positions(Purpose::Restoration) {
                assert!(swap < restore);
            }
            for release in positions(Purpose::Teardown) {
                assert!(release < swap);
            }
        }
        for retire in positions(Purpose::Retire) {
            assert!(
                ops.iter()
                    .enumerate()
                    .all(|(i, op)| purposes[&op.id()] == Purpose::Retire || i < retire)
            );
        }
        assert!(
            matches!(ops.last().unwrap().change(),Change::DropTable{uid,..} if uid==ids.table_uid(&table).unwrap())
        );
        assert!(done.rewrites > 1);
        assert_eq!(
            ops.iter()
                .filter(|op| matches!(op.change(),Change::Grant{role:r,..} if r.raw()==role))
                .count(),
            3
        );
        for name in ["app.left_view", "app.right_view"] {
            let position = |n: &str, create| {
                ops.iter()
                    .position(|o| {
                        matches!(o.change(),Change::CreateModule{id,..} if create && id==&n.parse().unwrap())
                            || matches!(o.change(),Change::DropModule{id,..} if !create && id==&n.parse().unwrap())
                    })
                    .unwrap()
            };
            assert!(position("app.diamond", false) < position(name, false));
            assert!(position(name, true) < position("app.diamond", true));
        }
        done.target.validate().unwrap();
        assert!(done.target.rebuilds.is_empty());
        assert_eq!(recorded.rebuilds.len(), 1);
        assert_eq!(
            done.target.tables.len() + root.intent.source.tables.len(),
            ids.tables.len() + root.intent.replacement.tables.len()
        );
        assert!(ops.iter().any(|op|matches!(op.change(),Change::AddForeignKey{table,..} if table==&"app.incoming".parse().unwrap())));
        assert_eq!(
            ops.iter()
                .filter(|op| matches!(op.change(), Change::BuildPartitionedIndex(_)))
                .count(),
            3
        );
        assert!(
            ops.iter()
                .any(|op| matches!(op.change(), Change::CreateModule { .. }))
        );
    }
    db.conn.execute("ROLLBACK").await.unwrap();
    assert_eq!(
        pbps_pg::catalog::introspect(&mut db.conn)
            .await
            .unwrap()
            .schema,
        base
    );
    db.conn
        .execute(&format!("DROP OWNED BY {role}; DROP ROLE {role}"))
        .await
        .unwrap();
    db.drop().await;
}
