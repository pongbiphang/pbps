use super::*;
use pbps_model::resolver::OrderReason;
use pbps_model::{Change, PlannedChange};

fn example() -> ChangeSet {
    ChangeSet {
        changes: ["a", "b", "c"]
            .into_iter()
            .map(|name| {
                PlannedChange::new(Change::CreateRole {
                    uid: "r_aaaaaa".parse().unwrap(),
                    name: name.into(),
                })
            })
            .collect(),
    }
}

#[test]
fn a_sealed_order_is_deterministic_and_detects_a_reordered_artifact() {
    let result = order(
        example(),
        BTreeSet::from([OrderEdge {
            before: 2,
            after: 0,
            reason: OrderReason::Binding,
        }]),
    )
    .unwrap();
    assert_eq!(
        result
            .changes
            .changes
            .iter()
            .map(|c| c.change.subject())
            .collect::<Vec<_>>(),
        ["role b", "role c", "role a"]
    );
    result.proof.validate(&result.changes).unwrap();
    let mut edited = result.changes.clone();
    edited.changes.swap(0, 2);
    assert!(result.proof.validate(&edited).is_err());
}

#[test]
fn cycles_and_edges_outside_the_change_set_refuse_before_output() {
    let edges = BTreeSet::from([
        OrderEdge {
            before: 0,
            after: 1,
            reason: OrderReason::Binding,
        },
        OrderEdge {
            before: 1,
            after: 0,
            reason: OrderReason::Binding,
        },
    ]);
    assert!(matches!(order(example(), edges), Err(Error::Cycle)));
    assert!(matches!(
        order(
            example(),
            BTreeSet::from([OrderEdge {
                before: 9,
                after: 0,
                reason: OrderReason::Binding
            }])
        ),
        Err(Error::Edge)
    ));
}

fn identity(name: &str) -> pbps_model::resolver::ObjectIdentity {
    pbps_model::resolver::ObjectIdentity {
        class: "fixture".into(),
        name: vec![name.into()],
        signature: vec![],
    }
}
fn observation(surface: Surface, inputs: BTreeSet<Surface>) -> SurfaceResolution {
    SurfaceResolution {
        surface,
        current: None,
        desired: Some(pbps_model::resolver::BoundSurface {
            object: identity("observed"),
            bindings: vec![],
            managed_inputs: inputs,
        }),
    }
}
fn ids(schema: &pbps_model::Schema, old: &pbps_model::IdsFile) -> pbps_model::IdsFile {
    crate::resolve(
        schema,
        old,
        &[],
        &crate::Context {
            operator: "614".into(),
            today: "2026-09-28".into(),
        },
    )
    .unwrap()
    .ids
}
fn tables() -> pbps_model::Schema {
    let mut schema = pbps_model::Schema::default();
    for name in ["app.a", "app.z"] {
        let mut table = pbps_model::Table::default();
        table
            .columns
            .insert("id".into(), pbps_model::Column::new("int".parse().unwrap()));
        schema.tables.insert(name.parse().unwrap(), table);
    }
    schema
}

#[test]
fn independent_table_changes_can_cross_the_old_name_order_without_a_false_cycle() {
    use pbps_model::{Column, Hints, IdsFile, Module, ModuleKind, Strategy};
    let base = tables();
    let old_ids = ids(&base, &IdsFile::default());
    let mut desired = base.clone();
    let column: pbps_model::ColumnRef = "app.a.v".parse().unwrap();
    let input: pbps_model::ColumnRef = "app.z.x".parse().unwrap();
    let routine: pbps_model::ModuleId = "app.f".parse().unwrap();
    let mut spec = Column::new("int".parse().unwrap());
    spec.default = Some("app.f()".into());
    desired
        .tables
        .get_mut(&column.table)
        .unwrap()
        .columns
        .insert(column.name.clone(), spec);
    desired
        .tables
        .get_mut(&input.table)
        .unwrap()
        .columns
        .insert(input.name.clone(), Column::new("int".parse().unwrap()));
    desired.modules.insert(
        routine.clone(),
        Module {
            kind: ModuleKind::Function,
            description: None,
            definition: "fixture supplied by engine".into(),
        },
    );
    let wanted_ids = ids(&desired, &old_ids);
    let base_side = crate::Side {
        schema: &base,
        ids: &old_ids,
    };
    let desired_side = crate::Side {
        schema: &desired,
        ids: &wanted_ids,
    };
    let mut hints = Hints::default();
    hints
        .strategies
        .insert(column.table.clone(), Strategy { online: true });
    let mut observations = vec![
        observation(
            Surface::Default(column.clone()),
            BTreeSet::from([Surface::Module(routine.clone())]),
        ),
        observation(
            Surface::Module(routine.clone()),
            BTreeSet::from([Surface::Column(input.clone())]),
        ),
    ];
    let ordered = plan(
        base_side,
        desired_side,
        &hints,
        &observations,
        &pbps_dialect::MinimalDialect,
    )
    .unwrap();
    let steps = &ordered.changes.changes;
    let at = |predicate: &dyn Fn(&Change) -> bool| {
        steps.iter().position(|p| predicate(&p.change)).unwrap()
    };
    let input_at = at(
        &|c| matches!(c, Change::AddColumn { table, name, .. } if table == &input.table && name == &input.name),
    );
    let routine_at = at(&|c| matches!(c, Change::CreateModule { id, .. } if id == &routine));
    let default_at = at(
        &|c| matches!(c, Change::AddColumn { table, name, .. } if table == &column.table && name == &column.name),
    );
    assert!(input_at < routine_at && routine_at < default_at);
    assert_eq!(steps[default_at].strategy, Strategy { online: true });
    assert!(
        matches!(&steps[default_at].change, Change::AddColumn { column, .. } if column.default.as_deref() == Some("app.f()"))
    );
    // ADD COLUMN's default backfills existing rows; it stays one typed step.
    assert!(
        !steps
            .iter()
            .any(|p| matches!(p.change, Change::AlterColumnDefault { .. }))
    );
    observations[1].desired.as_mut().unwrap().managed_inputs =
        BTreeSet::from([Surface::Column(column.clone())]);
    assert!(matches!(
        plan(
            base_side,
            desired_side,
            &hints,
            &observations,
            &pbps_dialect::MinimalDialect
        ),
        Err(Error::Cycle)
    ));
    assert!(matches!(
        plan(
            base_side,
            desired_side,
            &hints,
            &observations[..1],
            &pbps_dialect::MinimalDialect
        ),
        Err(Error::Coverage(_))
    ));
}

#[test]
fn explicit_module_dependencies_and_ordinary_orders_survive_resolution() {
    use pbps_model::{Hints, IdsFile, Module, ModuleKind, Schema};
    let base = Schema::default();
    let old_ids = IdsFile::default();
    let mut desired = tables();
    for name in ["app.va", "app.vz"] {
        desired.modules.insert(
            name.parse().unwrap(),
            Module {
                kind: ModuleKind::View,
                description: None,
                definition: "SELECT 1 AS id".into(),
            },
        );
    }
    let wanted_ids = ids(&desired, &old_ids);
    let base_side = crate::Side {
        schema: &base,
        ids: &old_ids,
    };
    let desired_side = crate::Side {
        schema: &desired,
        ids: &wanted_ids,
    };
    let mut hints = Hints::default();
    hints.module_deps.insert(
        "app.va".parse().unwrap(),
        BTreeSet::from(["app.vz".parse().unwrap()]),
    );
    let observations = ["app.va", "app.vz"]
        .map(|n| observation(Surface::Module(n.parse().unwrap()), BTreeSet::new()));
    let ordered = plan(
        base_side,
        desired_side,
        &hints,
        &observations,
        &pbps_dialect::MinimalDialect,
    )
    .unwrap();
    let modules: Vec<_> = ordered
        .changes
        .changes
        .iter()
        .filter_map(|p| p.change.module_id())
        .map(ToString::to_string)
        .collect();
    assert_eq!(modules, ["app.vz", "app.va"]);
    let before = crate::diff(
        base_side,
        desired_side,
        &pbps_dialect::MinimalDialect,
        &hints,
    )
    .unwrap();
    let after = crate::diff(
        base_side,
        desired_side,
        &pbps_dialect::MinimalDialect,
        &hints,
    )
    .unwrap();
    assert_eq!(before, after);
    assert_eq!(ordered.changes, before);
    hints.module_deps.insert(
        "app.vz".parse().unwrap(),
        BTreeSet::from(["app.va".parse().unwrap()]),
    );
    assert!(
        plan(
            base_side,
            desired_side,
            &hints,
            &observations,
            &pbps_dialect::MinimalDialect
        )
        .is_err()
    );
}

#[test]
fn schema_authorization_precedes_binding_ddl_and_keeps_principal_creation() {
    use pbps_model::{GrantTarget, Hints, IdsFile, Module, ModuleKind, Permission, Role, Schema};
    let base = Schema::default();
    let old_ids = IdsFile::default();
    let mut desired = Schema::default();
    desired.modules.insert(
        "app.f".parse().unwrap(),
        Module {
            kind: ModuleKind::Function,
            description: None,
            definition: "fixture supplied by engine".into(),
        },
    );
    desired.roles.insert(
        "deployer".into(),
        Role {
            description: None,
            grants: std::collections::BTreeMap::from([(
                GrantTarget::Schema("app".into()),
                BTreeSet::from([Permission::Usage]),
            )]),
        },
    );
    let wanted_ids = ids(&desired, &old_ids);
    let ordered = plan(
        crate::Side {
            schema: &base,
            ids: &old_ids,
        },
        crate::Side {
            schema: &desired,
            ids: &wanted_ids,
        },
        &Hints::default(),
        &[observation(
            Surface::Module("app.f".parse().unwrap()),
            BTreeSet::new(),
        )],
        &pbps_dialect::MinimalDialect,
    )
    .unwrap();
    let steps = &ordered.changes.changes;
    let role = steps
        .iter()
        .position(|p| matches!(p.change, Change::CreateRole { .. }))
        .unwrap();
    let grant = steps
        .iter()
        .position(|p| matches!(p.change, Change::Grant { .. }))
        .unwrap();
    let create = steps
        .iter()
        .position(|p| matches!(p.change, Change::CreateModule { .. }))
        .unwrap();
    assert!(role < grant && grant < create);
    let mut missing = ordered.changes.clone();
    missing.changes.swap(grant, create);
    assert!(ordered.proof.validate(&missing).is_err());
}

#[test]
fn a_delayed_child_rename_does_not_lose_its_foreign_key_drop_dependency() {
    use pbps_model::{ColumnType, ForeignKey, IdsFile, Module, ModuleKind, Schema};
    let mut base = tables();
    let parent: pbps_model::TableName = "app.a".parse().unwrap();
    let old: pbps_model::TableName = "app.z".parse().unwrap();
    let new: pbps_model::TableName = "app.u".parse().unwrap();
    let routine: pbps_model::ModuleId = "app.f()".parse().unwrap();
    base.tables.get_mut(&old).unwrap().foreign_keys.insert(
        "fk".into(),
        ForeignKey {
            columns: vec!["id".into()],
            references_table: parent.clone(),
            references_columns: vec!["id".into()],
            on_delete: Default::default(),
            on_update: Default::default(),
        },
    );
    let old_ids = ids(&base, &IdsFile::default());
    let mut after_ids = old_ids.clone();
    after_ids.rename_table(&old, &new);
    let mut desired: Schema = base.clone();
    let mut child = desired.tables.remove(&old).unwrap();
    child.foreign_keys.clear();
    desired.tables.insert(new.clone(), child);
    let changes = ChangeSet { changes: vec![
        PlannedChange::new(Change::RenameTable { uid: old_ids.table_uid(&old).unwrap().clone(), from: old.clone(), to: new.clone(), defaults: vec![] }),
        PlannedChange::new(Change::DropForeignKey { table: new.clone(), name: "fk".into() }),
        PlannedChange::new(Change::AlterColumnType { uid: old_ids.column_uid(&parent.column("id")).unwrap().clone(), column: parent.column("id"), from: "int".parse::<ColumnType>().unwrap(), to: "text".parse().unwrap(), from_nullable: false, to_nullable: false, from_collation: None, to_collation: None }),
        PlannedChange::new(Change::DropModule { id: routine.clone(), kind: ModuleKind::Function }),
        PlannedChange::new(Change::CreateModule { id: routine.clone(), module: Box::new(Module { kind: ModuleKind::Function, description: None, definition: "() RETURNS integer LANGUAGE SQL RETURN (SELECT count(*)::integer FROM app.u)".into() }) }),
    ] };
    let mut observed = observation(
        Surface::Module(routine),
        BTreeSet::from([Surface::Table(new)]),
    );
    let mut current = observed.desired.clone().unwrap();
    current.managed_inputs = BTreeSet::from([Surface::Table(old)]);
    observed.current = Some(current);
    let edges = graph::constraints(
        &changes,
        &[observed],
        &Default::default(),
        crate::Side {
            schema: &base,
            ids: &old_ids,
        },
        crate::Side {
            schema: &desired,
            ids: &after_ids,
        },
    );
    let ordered = order(changes, edges).unwrap();
    let steps = &ordered.changes.changes;
    let drop = steps
        .iter()
        .position(|p| matches!(p.change, Change::DropForeignKey { .. }))
        .unwrap();
    let alter = steps
        .iter()
        .position(|p| matches!(p.change, Change::AlterColumnType { .. }))
        .unwrap();
    assert!(
        drop < alter,
        "a renamed child must release the parent before its incompatible type change"
    );
    let rename = steps
        .iter()
        .position(|p| matches!(p.change, Change::RenameTable { .. }))
        .unwrap();
    assert!(rename < drop, "the drop uses the child's approved new name");
}

#[test]
fn deprecation_does_not_rebuild_unchanged_column_dependents() {
    use pbps_model::resolver::Binding;
    use pbps_model::{IdsFile, Module, ModuleKind};
    let mut base = tables();
    let input: pbps_model::ColumnRef = "app.a.id".parse().unwrap();
    for (name, kind) in [
        ("app.v", ModuleKind::View),
        ("app.f()", ModuleKind::Function),
    ] {
        base.modules.insert(
            name.parse().unwrap(),
            Module {
                kind,
                description: None,
                definition: "fixture supplied by engine".into(),
            },
        );
    }
    let identities = ids(&base, &IdsFile::default());
    let observations: Vec<_> = base
        .modules
        .keys()
        .map(|id| {
            let mut o = observation(
                Surface::Module(id.clone()),
                BTreeSet::from([Surface::Column(input.clone())]),
            );
            o.desired.as_mut().unwrap().bindings.push(Binding {
                node: "column".into(),
                path: vec!["input".into()],
                target: identity("original"),
            });
            o.current = o.desired.clone();
            o
        })
        .collect();
    for (from, to) in [
        (None, Some("old API")),
        (Some("old API"), Some("use new API")),
        (Some("old API"), None),
    ] {
        base.tables
            .get_mut(&input.table)
            .unwrap()
            .columns
            .get_mut(&input.name)
            .unwrap()
            .deprecated = from.map(str::to_owned);
        let mut desired = base.clone();
        desired
            .tables
            .get_mut(&input.table)
            .unwrap()
            .columns
            .get_mut(&input.name)
            .unwrap()
            .deprecated = to.map(str::to_owned);
        let old = crate::Side {
            schema: &base,
            ids: &identities,
        };
        let wanted = crate::Side {
            schema: &desired,
            ids: &identities,
        };
        let hints = Default::default();
        let ordinary = crate::diff(old, wanted, &pbps_dialect::MinimalDialect, &hints).unwrap();
        assert!(
            matches!(&ordinary.changes[..], [p] if matches!(p.change, Change::SetColumnDeprecated { .. }))
        );
        let resolved = plan(
            old,
            wanted,
            &hints,
            &observations,
            &pbps_dialect::MinimalDialect,
        )
        .unwrap();
        assert_eq!(
            resolved.changes, ordinary,
            "metadata must not invent a dependent rebuild"
        );
        resolved.proof.validate(&resolved.changes).unwrap();
        // The metadata exemption must not hide an independently observed binding change.
        let mut rebound = observations.clone();
        rebound[0].desired.as_mut().unwrap().bindings[0].target = identity("replacement");
        let resolved = plan(old, wanted, &hints, &rebound, &pbps_dialect::MinimalDialect).unwrap();
        assert!(resolved.changes.changes.iter().any(|p| {
            matches!(&rebound[0].surface, Surface::Module(id) if p.change.module_id() == Some(id))
        }));
        // A real column type change still invalidates dependents even if the logical binding stays.
        desired
            .tables
            .get_mut(&input.table)
            .unwrap()
            .columns
            .get_mut(&input.name)
            .unwrap()
            .ty = "bigint".parse().unwrap();
        let resolved = plan(
            old,
            crate::Side {
                schema: &desired,
                ids: &identities,
            },
            &hints,
            &observations,
            &pbps_dialect::MinimalDialect,
        )
        .unwrap();
        assert!(
            resolved
                .changes
                .changes
                .iter()
                .any(|p| p.change.module_id().is_some())
        );
    }
}

#[test]
fn splitting_table_creation_preserves_the_declared_index_layout() {
    use pbps_model::{Clustered, Hints, IdsFile, Index, IndexColumn, Schema};
    for clustered in [false, true] {
        let base = Schema::default();
        let old_ids = IdsFile::default();
        let mut desired = tables();
        for table in desired.tables.values_mut() {
            table.indexes.insert(
                "ix".into(),
                Index {
                    columns: vec![IndexColumn {
                        key: pbps_model::IndexKey::Column("id".into()),
                        descending: false,
                        opclass: None,
                    }],
                    include: vec![],
                    unique: false,
                    filter: None,
                    method: Default::default(),
                },
            );
            table.clustered = clustered.then(|| Clustered::Index("ix".into()));
        }
        let wanted_ids = ids(&desired, &old_ids);
        let ordered = plan(
            crate::Side {
                schema: &base,
                ids: &old_ids,
            },
            crate::Side {
                schema: &desired,
                ids: &wanted_ids,
            },
            &Hints::default(),
            &[],
            &pbps_dialect::MinimalDialect,
        )
        .unwrap();
        let indexes: Vec<_> = ordered
            .changes
            .changes
            .iter()
            .filter_map(|step| {
                if let Change::AddIndex { clustered, .. } = &step.change {
                    Some(*clustered)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(indexes, vec![clustered; 2]);
        ordered.proof.validate(&ordered.changes).unwrap();
        let mut changed = ordered.changes.clone();
        for step in &mut changed.changes {
            if let Change::AddIndex { clustered, .. } = &mut step.change {
                *clustered = !*clustered;
            }
        }
        assert!(ordered.proof.validate(&changed).is_err());
    }
}

/// A created table's identity on one of its indexes is set after that index,
/// which the split takes out of the `CREATE`; one on no index stays in the
/// `CREATE` (#1444).
#[test]
fn splitting_table_creation_sets_an_index_identity_after_its_index() {
    use pbps_model::{Hints, IdsFile, Index, IndexColumn, ReplicaIdentity, Schema};
    for identity in [ReplicaIdentity::Index("ix".into()), ReplicaIdentity::Full] {
        let base = Schema::default();
        let old_ids = IdsFile::default();
        let mut desired = tables();
        for table in desired.tables.values_mut() {
            table.columns.get_mut("id").unwrap().nullable = false;
            table.indexes.insert(
                "ix".into(),
                Index {
                    columns: vec![IndexColumn {
                        key: pbps_model::IndexKey::Column("id".into()),
                        descending: false,
                        opclass: None,
                    }],
                    include: vec![],
                    unique: true,
                    filter: None,
                    method: Default::default(),
                },
            );
            table.replica_identity = Some(identity.clone());
        }
        let wanted_ids = ids(&desired, &old_ids);
        let ordered = plan(
            crate::Side {
                schema: &base,
                ids: &old_ids,
            },
            crate::Side {
                schema: &desired,
                ids: &wanted_ids,
            },
            &Hints::default(),
            &[],
            &pbps_dialect::MinimalDialect,
        )
        .unwrap();
        let steps = &ordered.changes.changes;
        for name in ["app.a", "app.z"] {
            let name: pbps_model::TableName = name.parse().unwrap();
            let created = steps.iter().find_map(|p| {
                if let Change::CreateTable { name: n, table, .. } = &p.change
                    && *n == name
                {
                    Some(table)
                } else {
                    None
                }
            });
            let set = steps.iter().position(
                |p| matches!(&p.change, Change::SetReplicaIdentity { table, .. } if *table == name),
            );
            let index = steps.iter().position(
                |p| matches!(&p.change, Change::AddIndex { table, .. } if *table == name),
            );
            if identity == ReplicaIdentity::Full {
                assert_eq!(
                    created.unwrap().replica_identity,
                    Some(ReplicaIdentity::Full)
                );
                assert_eq!(set, None, "{steps:#?}");
            } else {
                assert_eq!(created.unwrap().replica_identity, None);
                assert!(index.unwrap() < set.unwrap(), "{steps:#?}");
            }
        }
        ordered.proof.validate(&ordered.changes).unwrap();
    }
}

/// An existing table's identity moved off an index the plan drops is set
/// before the drop, as the differ ordered it, and one moved to an index the
/// plan adds after the add: the resolver keeps both (#1444).
#[test]
fn a_replica_identity_keeps_its_order_against_its_tables_indexes() {
    use pbps_model::{Hints, Index, IndexColumn, ReplicaIdentity, Schema};
    let unique_on_id = || Index {
        columns: vec![IndexColumn {
            key: pbps_model::IndexKey::Column("id".into()),
            descending: false,
            opclass: None,
        }],
        include: vec![],
        unique: true,
        filter: None,
        method: Default::default(),
    };
    let with = |index: &str, identity: Option<ReplicaIdentity>| {
        let mut schema = tables();
        for table in schema.tables.values_mut() {
            table.columns.get_mut("id").unwrap().nullable = false;
            table.indexes.insert(index.into(), unique_on_id());
            table.replica_identity = identity.clone();
        }
        schema
    };
    let order = |base: &Schema, desired: &Schema| -> Vec<String> {
        let base_ids = ids(base, &pbps_model::IdsFile::default());
        let wanted_ids = ids(desired, &base_ids);
        let ordered = plan(
            crate::Side {
                schema: base,
                ids: &base_ids,
            },
            crate::Side {
                schema: desired,
                ids: &wanted_ids,
            },
            &Hints::default(),
            &[],
            &pbps_dialect::MinimalDialect,
        )
        .unwrap();
        ordered.proof.validate(&ordered.changes).unwrap();
        ordered
            .changes
            .changes
            .iter()
            .filter(|p| p.change.table() == Some(&"app.a".parse().unwrap()))
            .map(|p| {
                if let Change::AddIndex { name, .. } = &p.change {
                    format!("add {name}")
                } else if let Change::DropIndex { name, .. } = &p.change {
                    format!("drop {name}")
                } else if matches!(p.change, Change::SetReplicaIdentity { .. }) {
                    "identity".to_owned()
                } else {
                    format!("{:?}", p.change)
                }
            })
            .collect()
    };
    let old = with("ix_old", Some(ReplicaIdentity::Index("ix_old".into())));
    assert_eq!(
        order(&old, &with("ix_new", Some(ReplicaIdentity::Full))),
        ["identity", "drop ix_old", "add ix_new"]
    );
    assert_eq!(
        order(
            &old,
            &with("ix_new", Some(ReplicaIdentity::Index("ix_new".into())))
        ),
        // FULL in between, while neither index is there.
        ["identity", "drop ix_old", "add ix_new", "identity"]
    );
}

#[test]
fn new_table_indexes_are_offline_while_existing_table_indexes_keep_the_requested_strategy() {
    use pbps_model::{Hints, IdsFile, Index, IndexColumn, Schema, Strategy};
    for existing in [false, true] {
        for online in [false, true] {
            let base = if existing {
                tables()
            } else {
                Schema::default()
            };
            let before_ids = ids(&base, &IdsFile::default());
            let mut desired = tables();
            let mut hints = Hints::default();
            for (name, table) in &mut desired.tables {
                table.indexes.insert(
                    "ix".into(),
                    Index {
                        columns: vec![IndexColumn {
                            key: pbps_model::IndexKey::Column("id".into()),
                            descending: false,
                            opclass: None,
                        }],
                        include: vec![],
                        unique: false,
                        filter: None,
                        method: Default::default(),
                    },
                );
                hints.strategies.insert(name.clone(), Strategy { online });
            }
            let after_ids = ids(&desired, &before_ids);
            let ordered = plan(
                crate::Side {
                    schema: &base,
                    ids: &before_ids,
                },
                crate::Side {
                    schema: &desired,
                    ids: &after_ids,
                },
                &hints,
                &[],
                &pbps_dialect::MinimalDialect,
            )
            .unwrap();
            let indexes: Vec<_> = ordered
                .changes
                .changes
                .iter()
                .filter(|p| matches!(p.change, Change::AddIndex { .. }))
                .collect();
            assert_eq!(indexes.len(), 2);
            for index in indexes {
                assert_eq!(index.strategy.online, existing && online);
            }
            ordered.proof.validate(&ordered.changes).unwrap();
        }
    }
}
