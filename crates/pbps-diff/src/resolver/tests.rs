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
        changes.changes.len(),
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
                    storage_parameters: Default::default(),
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
                    storage_parameters: Default::default(),
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
        storage_parameters: Default::default(),
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
                        storage_parameters: Default::default(),
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

mod generated_surface_coverage {
    use super::*;
    use pbps_model::resolver::{Binding, BoundSurface, ObjectIdentity};
    use pbps_model::{Column, Generated, Hints, IdsFile, Schema, Table};

    struct Fixture {
        base: Schema,
        desired: Schema,
        before_ids: IdsFile,
        after_ids: IdsFile,
        resolution: Vec<SurfaceResolution>,
    }

    fn generated(expression: &str) -> Column {
        let mut column = Column::new("int".parse().unwrap());
        column.generated = Some(Generated {
            expression: expression.into(),
            stored: true,
        });
        column
    }

    fn attrdef(name: &str, present_before: bool, reads_input: bool) -> SurfaceResolution {
        let input = ObjectIdentity {
            class: "pg_attribute".into(),
            name: vec!["app".into(), "t".into(), "a".into()],
            signature: vec![],
        };
        let bound = BoundSurface {
            object: ObjectIdentity {
                class: "pg_attrdef".into(),
                name: vec!["app".into(), "t".into(), name.into()],
                signature: vec![],
            },
            bindings: if reads_input {
                vec![Binding {
                    node: "Var".into(),
                    path: vec!["a".into()],
                    target: input,
                }]
            } else {
                vec![]
            },
            managed_inputs: if reads_input {
                BTreeSet::from([Surface::Column("app.t.a".parse().unwrap())])
            } else {
                BTreeSet::new()
            },
        };
        SurfaceResolution {
            surface: Surface::Default(format!("app.t.{name}").parse().unwrap()),
            current: present_before.then(|| bound.clone()),
            desired: Some(bound),
        }
    }

    // These are the two connected generation shapes, not engine admission or
    // version qualification. The pure planner uses the existing minimal dialect.
    fn fixture(add_stored: bool) -> Fixture {
        let table_name = "app.t".parse().unwrap();
        let mut table = Table::default();
        table
            .columns
            .insert("a".into(), Column::new("int".parse().unwrap()));
        table.columns.insert("g".into(), generated("a * 2 + 1"));
        let mut ordinary = Column::new("int".parse().unwrap());
        ordinary.default = Some("7".into());
        table.columns.insert("d".into(), ordinary);
        let mut base = Schema::default();
        base.tables.insert(table_name, table);
        let before_ids = ids(&base, &IdsFile::default());
        let mut desired = base.clone();
        let columns = &mut desired
            .tables
            .get_mut(&"app.t".parse().unwrap())
            .unwrap()
            .columns;
        let mut resolution = vec![attrdef("g", true, true), attrdef("d", true, false)];
        if add_stored {
            columns.insert("h".into(), generated("a + 4"));
            resolution.push(attrdef("h", false, true));
        } else {
            columns.insert("g".into(), generated("a * 3"));
        }
        let after_ids = ids(&desired, &before_ids);
        Fixture {
            base,
            desired,
            before_ids,
            after_ids,
            resolution,
        }
    }

    impl Fixture {
        fn plan(&self, resolution: &[SurfaceResolution]) -> Result<Ordered, Error> {
            super::plan(
                crate::Side {
                    schema: &self.base,
                    ids: &self.before_ids,
                },
                crate::Side {
                    schema: &self.desired,
                    ids: &self.after_ids,
                },
                &Hints::default(),
                resolution,
                &pbps_dialect::MinimalDialect,
            )
        }

        fn retained_column_uid(&self, name: &str) -> pbps_model::Uid {
            let column = format!("app.t.{name}").parse().unwrap();
            let before = self.before_ids.column_uid(&column).unwrap();
            assert_eq!(Some(before), self.after_ids.column_uid(&column));
            before.clone()
        }
    }

    #[test]
    fn replacing_a_stored_expression_keeps_its_uid_and_ordinary_default() {
        let fixture = fixture(false);
        assert_eq!(fixture.before_ids.tables, fixture.after_ids.tables);
        let uid = fixture.retained_column_uid("g");
        fixture.retained_column_uid("a");
        fixture.retained_column_uid("d");
        let ordered = fixture
            .plan(&fixture.resolution)
            .expect("generated attrdef evidence must admit the expression replacement");
        assert_eq!(ordered.changes.changes.len(), 1);
        assert_eq!(
            ordered.changes.changes[0].change,
            Change::AlterColumnExpression {
                uid,
                column: "app.t.g".parse().unwrap(),
                from: "a * 2 + 1".into(),
                to: "a * 3".into(),
            }
        );
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    #[test]
    fn adding_stored_generation_retains_existing_generation_and_ordinary_default() {
        let fixture = fixture(true);
        assert_eq!(fixture.before_ids.tables, fixture.after_ids.tables);
        for name in ["a", "g", "d"] {
            fixture.retained_column_uid(name);
        }
        let column = "app.t.h".parse().unwrap();
        assert!(fixture.before_ids.column_uid(&column).is_none());
        let uid = fixture.after_ids.column_uid(&column).unwrap().clone();
        let ordered = fixture
            .plan(&fixture.resolution)
            .expect("generated attrdef evidence must admit ADD STORED beside retained generation");
        assert_eq!(ordered.changes.changes.len(), 1);
        assert_eq!(
            ordered.changes.changes[0].change,
            Change::AddColumn {
                uid,
                table: "app.t".parse().unwrap(),
                name: "h".into(),
                column: Box::new(generated("a + 4")),
            }
        );
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    #[test]
    fn generated_and_ordinary_attrdefs_require_exact_coverage_and_per_side_presence() {
        for add_stored in [false, true] {
            let fixture = fixture(add_stored);
            for (index, record) in fixture.resolution.iter().enumerate() {
                let mut missing = fixture.resolution.clone();
                missing.remove(index);
                assert!(matches!(
                    fixture.plan(&missing),
                    Err(Error::Coverage(surface)) if surface == record.surface
                ));

                let mut wrong_current = fixture.resolution.clone();
                wrong_current[index].current = if record.current.is_some() {
                    None
                } else {
                    record.desired.clone()
                };
                assert!(matches!(
                    fixture.plan(&wrong_current),
                    Err(Error::Coverage(surface)) if surface == record.surface
                ));

                let mut missing_desired = fixture.resolution.clone();
                missing_desired[index].desired = None;
                assert!(matches!(
                    fixture.plan(&missing_desired),
                    Err(Error::Coverage(surface)) if surface == record.surface
                ));
            }

            let mut extra = fixture.resolution.clone();
            extra.push(attrdef("a", false, false));
            assert!(matches!(
                fixture.plan(&extra),
                Err(Error::Coverage(surface))
                    if surface == Surface::Default("app.t.a".parse().unwrap())
            ));

            let mut duplicate = fixture.resolution.clone();
            duplicate.push(fixture.resolution[0].clone());
            assert!(matches!(
                fixture.plan(&duplicate),
                Err(Error::Coverage(surface)) if surface == fixture.resolution[0].surface
            ));

            let mut reference_only = fixture.resolution.clone();
            reference_only[0].surface = Surface::Column("app.t.a".parse().unwrap());
            assert!(matches!(
                fixture.plan(&reference_only),
                Err(Error::Coverage(surface))
                    if surface == Surface::Column("app.t.a".parse().unwrap())
            ));

            let mut absent_desired_default = fixture;
            absent_desired_default
                .desired
                .tables
                .get_mut(&"app.t".parse().unwrap())
                .unwrap()
                .columns
                .get_mut("d")
                .unwrap()
                .default = None;
            assert!(matches!(
                absent_desired_default.plan(&absent_desired_default.resolution),
                Err(Error::Coverage(surface))
                    if surface == Surface::Default("app.t.d".parse().unwrap())
            ));
        }
    }

    /// A generation expression stays in its CREATE TABLE or ADD COLUMN, so
    /// that step provides its `pg_attrdef` surface: a function the plan
    /// creates and the expression calls is ordered before the table or
    /// column, not after it (DEC-1274.2).
    #[test]
    fn a_generation_expression_waits_for_the_function_it_calls() {
        let function: pbps_model::ModuleId = "app.f(integer)".parse().unwrap();
        let calls = |name: &str| SurfaceResolution {
            surface: Surface::Default(name.parse().unwrap()),
            current: None,
            desired: Some(BoundSurface {
                object: ObjectIdentity {
                    class: "pg_attrdef".into(),
                    name: name.split('.').map(str::to_owned).collect(),
                    signature: vec![],
                },
                bindings: vec![],
                // It reads its own table's column too, which the same
                // CREATE TABLE provides: no edge of a step to itself.
                managed_inputs: BTreeSet::from([
                    Surface::Module(function.clone()),
                    Surface::Column("app.t.a".parse().unwrap()),
                ]),
            }),
        };
        let module = SurfaceResolution {
            surface: Surface::Module(function.clone()),
            current: None,
            desired: Some(BoundSurface {
                object: ObjectIdentity {
                    class: "pg_proc".into(),
                    name: vec!["app".into(), "f".into()],
                    signature: vec![],
                },
                bindings: vec![],
                managed_inputs: BTreeSet::new(),
            }),
        };
        for new_table in [true, false] {
            let mut base = Schema::default();
            let mut table = Table::default();
            table
                .columns
                .insert("a".into(), Column::new("int".parse().unwrap()));
            if !new_table {
                base.tables.insert("app.t".parse().unwrap(), table.clone());
            }
            let before_ids = ids(&base, &IdsFile::default());
            let mut desired = base.clone();
            table.columns.insert("g".into(), generated("app.f(a)"));
            desired.tables.insert("app.t".parse().unwrap(), table);
            desired.modules.insert(
                function.clone(),
                pbps_model::Module {
                    kind: pbps_model::ModuleKind::Function,
                    description: None,
                    definition: "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1".into(),
                },
            );
            let after_ids = ids(&desired, &before_ids);
            let ordered = super::plan(
                crate::Side {
                    schema: &base,
                    ids: &before_ids,
                },
                crate::Side {
                    schema: &desired,
                    ids: &after_ids,
                },
                &Hints::default(),
                &[calls("app.t.g"), module.clone()],
                &pbps_dialect::MinimalDialect,
            )
            .unwrap();
            let at = |f: &dyn Fn(&Change) -> bool| {
                ordered
                    .changes
                    .changes
                    .iter()
                    .position(|p| f(&p.change))
                    .unwrap()
            };
            let created = at(&|c| matches!(c, Change::CreateModule { id, .. } if id == &function));
            let generation = at(&|c| {
                matches!(c, Change::CreateTable { .. })
                    || matches!(c, Change::AddColumn { name, .. } if name == "g")
            });
            assert!(created < generation, "new_table={new_table}");
            ordered.proof.validate(&ordered.changes).unwrap();
        }
    }

    /// A routine every case below calls from a generation expression.
    fn routine() -> (pbps_model::ModuleId, pbps_model::Module) {
        (
            "app.f(integer)".parse().unwrap(),
            pbps_model::Module {
                kind: pbps_model::ModuleKind::Function,
                description: None,
                definition: "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1".into(),
            },
        )
    }

    fn bound(class: &str, name: &str, inputs: &[Surface], binds: &str) -> BoundSurface {
        BoundSurface {
            object: ObjectIdentity {
                class: class.into(),
                name: name.split('.').map(str::to_owned).collect(),
                signature: vec![],
            },
            bindings: vec![Binding {
                node: "FuncExpr".into(),
                path: vec!["expr".into()],
                target: ObjectIdentity {
                    class: "pg_proc".into(),
                    name: vec!["app".into(), binds.into()],
                    signature: vec![],
                },
            }],
            managed_inputs: inputs.iter().cloned().collect(),
        }
    }

    fn sided(base: &Schema, desired: &Schema, observations: &[SurfaceResolution]) -> Ordered {
        let before_ids = ids(base, &IdsFile::default());
        let after_ids = ids(desired, &before_ids);
        super::plan(
            crate::Side {
                schema: base,
                ids: &before_ids,
            },
            crate::Side {
                schema: desired,
                ids: &after_ids,
            },
            &Hints::default(),
            observations,
            &pbps_dialect::MinimalDialect,
        )
        .unwrap()
    }

    fn at(ordered: &Ordered, f: &dyn Fn(&Change) -> bool) -> usize {
        ordered
            .changes
            .changes
            .iter()
            .position(|p| f(&p.change))
            .unwrap_or_else(|| panic!("missing from {:?}", ordered.changes.changes))
    }

    fn table(expression: Option<&str>) -> Table {
        let mut table = Table::default();
        table
            .columns
            .insert("a".into(), Column::new("int".parse().unwrap()));
        if let Some(expression) = expression {
            table.columns.insert("g".into(), generated(expression));
        }
        table
    }

    /// A rewritten generation expression lets go of the routine its old text
    /// called, so it precedes that routine's drop.
    #[test]
    fn a_rewritten_generation_expression_precedes_the_drop_of_what_it_called() {
        let (id, module) = routine();
        let mut base = Schema::default();
        base.modules.insert(id.clone(), module);
        base.tables
            .insert("app.t".parse().unwrap(), table(Some("app.f(a)")));
        let mut desired = Schema::default();
        desired
            .tables
            .insert("app.t".parse().unwrap(), table(Some("a * 2")));
        let function = Surface::Module(id.clone());
        let ordered = sided(
            &base,
            &desired,
            &[
                SurfaceResolution {
                    surface: Surface::Default("app.t.g".parse().unwrap()),
                    current: Some(bound(
                        "pg_attrdef",
                        "app.t.g",
                        std::slice::from_ref(&function),
                        "f",
                    )),
                    desired: Some(bound("pg_attrdef", "app.t.g", &[], "int4mul")),
                },
                SurfaceResolution {
                    surface: function,
                    current: Some(bound("pg_proc", "app.f", &[], "int4in")),
                    desired: None,
                },
            ],
        );
        let rewrite = at(&ordered, &|c| {
            matches!(c, Change::AlterColumnExpression { .. })
        });
        let drop = at(
            &ordered,
            &|c| matches!(c, Change::DropModule { id: d, .. } if d == &id),
        );
        assert!(rewrite < drop, "{:?}", ordered.changes.changes);
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    /// A generated column computes its rows as it is added, so an execute
    /// grant on the routine it calls goes first.
    #[test]
    fn an_execute_grant_precedes_the_generated_column_that_calls_the_routine() {
        let (id, module) = routine();
        let mut base = Schema::default();
        base.modules.insert(id.clone(), module);
        base.tables.insert("app.t".parse().unwrap(), table(None));
        base.roles
            .insert("reader".into(), pbps_model::Role::default());
        let mut desired = base.clone();
        desired
            .tables
            .insert("app.t".parse().unwrap(), table(Some("app.f(a)")));
        desired.roles.get_mut("reader").unwrap().grants.insert(
            pbps_model::GrantTarget::Routine(match &id {
                pbps_model::ModuleId::Routine(routine) => routine.clone(),
                other @ (pbps_model::ModuleId::Named(_) | pbps_model::ModuleId::Trigger { .. }) => {
                    panic!("{other:?}")
                }
            }),
            BTreeSet::from([pbps_model::Permission::Execute]),
        );
        let function = Surface::Module(id.clone());
        let ordered = sided(
            &base,
            &desired,
            &[
                SurfaceResolution {
                    surface: Surface::Default("app.t.g".parse().unwrap()),
                    current: None,
                    desired: Some(bound(
                        "pg_attrdef",
                        "app.t.g",
                        std::slice::from_ref(&function),
                        "f",
                    )),
                },
                SurfaceResolution {
                    surface: function,
                    current: Some(bound("pg_proc", "app.f", &[], "int4in")),
                    desired: Some(bound("pg_proc", "app.f", &[], "int4in")),
                },
            ],
        );
        let grant = at(&ordered, &|c| matches!(c, Change::Grant { .. }));
        let column = at(
            &ordered,
            &|c| matches!(c, Change::AddColumn { name, .. } if name == "g"),
        );
        assert!(grant < column, "{:?}", ordered.changes.changes);
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    /// An unchanged generation expression that binds differently is rewritten
    /// with its own text, so the new binding is installed.
    #[test]
    fn a_rebinding_generation_expression_is_rewritten_with_its_own_text() {
        let (id, module) = routine();
        let mut base = Schema::default();
        base.modules.insert(id.clone(), module);
        base.tables
            .insert("app.t".parse().unwrap(), table(Some("app.f(a)")));
        let desired = base.clone();
        let function = Surface::Module(id);
        let ordered = sided(
            &base,
            &desired,
            &[
                SurfaceResolution {
                    surface: Surface::Default("app.t.g".parse().unwrap()),
                    current: Some(bound(
                        "pg_attrdef",
                        "app.t.g",
                        std::slice::from_ref(&function),
                        "f",
                    )),
                    desired: Some(bound(
                        "pg_attrdef",
                        "app.t.g",
                        std::slice::from_ref(&function),
                        "f_exact",
                    )),
                },
                SurfaceResolution {
                    surface: function,
                    current: Some(bound("pg_proc", "app.f", &[], "int4in")),
                    desired: Some(bound("pg_proc", "app.f", &[], "int4in")),
                },
            ],
        );
        assert!(ordered.changes.changes.iter().any(|p| matches!(
            &p.change,
            Change::AlterColumnExpression { from, to, .. } if from == "app.f(a)" && to == "app.f(a)"
        )));
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    /// A check removed after its table's rename is covered under the spelling
    /// its removal uses, as the producer names it; the opening spelling no
    /// longer covers it.
    #[test]
    fn a_surface_removed_after_a_rename_is_covered_under_its_removal_spelling() {
        let mut table = table(None);
        table.checks.insert(
            "c".into(),
            pbps_model::CheckConstraint {
                expression: "a > 0".into(),
            },
        );
        let mut base = Schema::default();
        base.tables.insert("app.t".parse().unwrap(), table);
        let before_ids = ids(&base, &IdsFile::default());
        let mut desired = Schema::default();
        desired
            .tables
            .insert("app.u".parse().unwrap(), self::table(None));
        let mut after_ids = before_ids.clone();
        after_ids.rename_table(&"app.t".parse().unwrap(), &"app.u".parse().unwrap());
        let removed = |table: &str| SurfaceResolution {
            surface: Surface::Check {
                table: table.parse().unwrap(),
                name: "c".into(),
            },
            current: Some(bound("pg_constraint", "app.t.c", &[], "int4gt")),
            desired: None,
        };
        let plan = |observation: SurfaceResolution| {
            super::plan(
                crate::Side {
                    schema: &base,
                    ids: &before_ids,
                },
                crate::Side {
                    schema: &desired,
                    ids: &after_ids,
                },
                &Hints::default(),
                &[observation],
                &pbps_dialect::MinimalDialect,
            )
        };
        let ordered = plan(removed("app.u")).unwrap();
        assert!(
            ordered
                .changes
                .changes
                .iter()
                .any(|p| matches!(p.change, Change::DropCheck { .. }))
        );
        ordered.proof.validate(&ordered.changes).unwrap();
        assert!(matches!(
            plan(removed("app.t")),
            Err(super::Error::Coverage(_))
        ));
    }

    /// Dropping `app.a` and renaming `app.b` to it, with each table's check
    /// removed: the dropped table's check holds the shared spelling, so the
    /// renamed table's is covered under its opening one, and each removal
    /// still needs its own resolution.
    #[test]
    fn removals_sharing_a_reused_name_are_covered_apart() {
        let checked = || {
            let mut t = table(None);
            t.checks.insert(
                "c".into(),
                pbps_model::CheckConstraint {
                    expression: "a > 0".into(),
                },
            );
            t
        };
        let a: pbps_model::TableName = "app.a".parse().unwrap();
        let b: pbps_model::TableName = "app.b".parse().unwrap();
        let mut base = Schema::default();
        base.tables.insert(a.clone(), checked());
        base.tables.insert(b.clone(), checked());
        let before_ids = ids(&base, &IdsFile::default());
        let mut desired = Schema::default();
        desired.tables.insert(a.clone(), table(None));
        let mut after_ids = before_ids.clone();
        let dropped = before_ids.table_uid(&a).unwrap().clone();
        after_ids.tables.remove(&dropped);
        after_ids.columns.retain(|_, column| column.table != a);
        after_ids.rename_table(&b, &a);
        let removed = |table: &pbps_model::TableName| SurfaceResolution {
            surface: Surface::Check {
                table: table.clone(),
                name: "c".into(),
            },
            current: Some(bound("pg_constraint", "app.x.c", &[], "int4gt")),
            desired: None,
        };
        let base_side = crate::Side {
            schema: &base,
            ids: &before_ids,
        };
        let desired_side = crate::Side {
            schema: &desired,
            ids: &after_ids,
        };
        super::super::prepare::coverage(base_side, desired_side, &[removed(&a), removed(&b)])
            .unwrap();
        assert!(matches!(
            super::super::prepare::coverage(base_side, desired_side, &[removed(&a)]),
            Err(super::super::Error::Coverage(_))
        ));

        // The ordering graph tells the two tables apart by recorded UID, not
        // by the name they share in turn: the dropped table goes first, with
        // its check, then the rename, then the renamed table's check.
        let ordered = super::plan(
            base_side,
            desired_side,
            &Hints::default(),
            &[removed(&a), removed(&b)],
            &pbps_dialect::MinimalDialect,
        )
        .unwrap();
        let steps = &ordered.changes.changes;
        let position = |f: &dyn Fn(&Change) -> bool| {
            steps
                .iter()
                .position(|p| f(&p.change))
                .unwrap_or_else(|| panic!("missing from {steps:?}"))
        };
        let dropped_table =
            position(&|c| matches!(c, Change::DropTable { uid, .. } if uid == &dropped));
        let rename = position(&|c| matches!(c, Change::RenameTable { .. }));
        let check = position(&|c| matches!(c, Change::DropCheck { table, .. } if table == &a));
        assert!(dropped_table < rename, "{steps:?}");
        assert!(rename < check, "{steps:?}");
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    /// Two tables swap names while each generation expression keeps its own
    /// binding. Each observation holds the opening record under its base
    /// spelling and the compiled one under its desired spelling, so one
    /// spelling pairs two different tables; the rebuild check pairs them
    /// through recorded UIDs, and a rename-only swap rewrites nothing
    /// (DEC-1498.1).
    #[test]
    fn swapped_table_names_pair_bindings_through_recorded_uids() {
        let (id, module) = routine();
        let a: pbps_model::TableName = "app.a".parse().unwrap();
        let b: pbps_model::TableName = "app.b".parse().unwrap();
        let mut base = Schema::default();
        base.modules.insert(id.clone(), module);
        base.tables.insert(a.clone(), table(Some("app.f(a)")));
        base.tables.insert(b.clone(), table(Some("app.f(a)")));
        let before_ids = ids(&base, &IdsFile::default());
        let desired = base.clone();
        let mut after_ids = before_ids.clone();
        let (ua, ub) = (
            before_ids.table_uid(&a).unwrap().clone(),
            before_ids.table_uid(&b).unwrap().clone(),
        );
        after_ids.tables.insert(ua.clone(), b.clone());
        after_ids.tables.insert(ub.clone(), a.clone());
        for column in after_ids.columns.values_mut() {
            column.table = if column.table == a {
                b.clone()
            } else {
                a.clone()
            };
        }
        let function = Surface::Module(id);
        let g = |table: &str| Surface::Default(format!("{table}.g").parse().unwrap());
        let attrdef = |binds: &str| {
            Some(bound(
                "pg_attrdef",
                "app.x.g",
                std::slice::from_ref(&function),
                binds,
            ))
        };
        // The old app.a binds f and the old app.b binds f_exact. `old_a_binds`
        // is what the old app.a binds under its new name, app.b.
        let plan = |old_a_binds: &str| {
            let observations = [
                SurfaceResolution {
                    surface: g("app.a"),
                    current: attrdef("f"),
                    desired: attrdef("f_exact"),
                },
                SurfaceResolution {
                    surface: g("app.b"),
                    current: attrdef("f_exact"),
                    desired: attrdef(old_a_binds),
                },
                SurfaceResolution {
                    surface: function.clone(),
                    current: Some(bound("pg_proc", "app.f", &[], "int4in")),
                    desired: Some(bound("pg_proc", "app.f", &[], "int4in")),
                },
            ];
            super::plan(
                crate::Side {
                    schema: &base,
                    ids: &before_ids,
                },
                crate::Side {
                    schema: &desired,
                    ids: &after_ids,
                },
                &Hints::default(),
                &observations,
                &pbps_dialect::MinimalDialect,
            )
            .unwrap()
        };
        let rewritten = |ordered: &Ordered| -> Vec<pbps_model::ColumnRef> {
            ordered
                .changes
                .changes
                .iter()
                .filter_map(|p| {
                    if let Change::AlterColumnExpression { column, .. } = &p.change {
                        Some(column.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };
        let unchanged = plan("f");
        assert!(
            unchanged
                .changes
                .changes
                .iter()
                .any(|p| matches!(p.change, Change::RenameTable { .. }))
        );
        assert_eq!(rewritten(&unchanged), Vec::new(), "a rename-only swap");
        unchanged.proof.validate(&unchanged.changes).unwrap();
        // The old app.a rebinds: only it is rebuilt, under its new name.
        let rebound = plan("f_exact");
        assert_eq!(rewritten(&rebound), vec!["app.b.g".parse().unwrap()]);
        rebound.proof.validate(&rebound.changes).unwrap();
    }

    /// A rebuild the resolver appends after the ordinary plan spells its
    /// teardown by the base name and its restoration by the final one, not
    /// by where it sits. In a rename chain, app.b to app.c and then app.a to
    /// app.b, the old app.b's check is dropped before its rename, never
    /// after it, which would hit the table that took the name (DEC-1498.1).
    #[test]
    fn an_appended_teardown_belongs_to_its_base_table_in_a_rename_chain() {
        let (id, module) = routine();
        let names =
            ["app.a", "app.b", "app.c"].map(|n| n.parse::<pbps_model::TableName>().unwrap());
        let [a, b, c] = names.clone();
        let mut checked = table(None);
        checked.checks.insert(
            "k".into(),
            pbps_model::CheckConstraint {
                expression: "app.f(a) > 0".into(),
            },
        );
        let mut base = Schema::default();
        base.modules.insert(id.clone(), module);
        base.tables.insert(a.clone(), table(None));
        base.tables.insert(b.clone(), checked.clone());
        let mut desired = Schema {
            modules: base.modules.clone(),
            ..Default::default()
        };
        desired.tables.insert(b.clone(), table(None));
        desired.tables.insert(c.clone(), checked);
        let before_ids = ids(&base, &IdsFile::default());
        let mut after_ids = before_ids.clone();
        after_ids.rename_table(&b, &c);
        after_ids.rename_table(&a, &b);
        let function = Surface::Module(id);
        let check = |table: &pbps_model::TableName| Surface::Check {
            table: table.clone(),
            name: "k".into(),
        };
        let constraint = |binds: &str| {
            Some(bound(
                "pg_constraint",
                "app.x.k",
                std::slice::from_ref(&function),
                binds,
            ))
        };
        let ordered = super::plan(
            crate::Side {
                schema: &base,
                ids: &before_ids,
            },
            crate::Side {
                schema: &desired,
                ids: &after_ids,
            },
            &Hints::default(),
            &[
                SurfaceResolution {
                    surface: check(&b),
                    current: constraint("f"),
                    desired: None,
                },
                SurfaceResolution {
                    surface: check(&c),
                    current: None,
                    desired: constraint("f_exact"),
                },
                SurfaceResolution {
                    surface: function.clone(),
                    current: Some(bound("pg_proc", "app.f", &[], "int4in")),
                    desired: Some(bound("pg_proc", "app.f", &[], "int4in")),
                },
            ],
            &pbps_dialect::MinimalDialect,
        )
        .unwrap();
        let steps = &ordered.changes.changes;
        let position = |f: &dyn Fn(&Change) -> bool| {
            steps
                .iter()
                .position(|p| f(&p.change))
                .unwrap_or_else(|| panic!("missing from {steps:?}"))
        };
        let teardown = position(&|ch| matches!(ch, Change::DropCheck { table, .. } if table == &b));
        let away = position(&|ch| matches!(ch, Change::RenameTable { from, .. } if from == &b));
        let into = position(&|ch| matches!(ch, Change::RenameTable { to, .. } if to == &b));
        let restore = position(&|ch| matches!(ch, Change::AddCheck { table, .. } if table == &c));
        assert!(teardown < away, "{steps:?}");
        assert!(away < into, "{steps:?}");
        assert!(away < restore, "{steps:?}");
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    /// Releases are read through the table that owns them, not the spelling
    /// they share. `app.a` is dropped and `app.b` renamed to it, and each
    /// table's check calls its own function, which the plan also drops. The
    /// renamed table's check removal is spelled `app.a` but releases the
    /// `app.b` observation, so the function it called waits for it
    /// (DEC-1498.1).
    #[test]
    fn a_reused_name_releases_each_tables_own_bindings() {
        let module = |name: &str| -> (pbps_model::ModuleId, pbps_model::Module) {
            (
                format!("app.{name}(integer)").parse().unwrap(),
                pbps_model::Module {
                    kind: pbps_model::ModuleKind::Function,
                    description: None,
                    definition: "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1".into(),
                },
            )
        };
        let (f1, m1) = module("f1");
        let (f2, m2) = module("f2");
        let checked = |function: &str| {
            let mut t = table(None);
            t.checks.insert(
                "c".into(),
                pbps_model::CheckConstraint {
                    expression: format!("app.{function}(a) > 0"),
                },
            );
            t
        };
        let a: pbps_model::TableName = "app.a".parse().unwrap();
        let b: pbps_model::TableName = "app.b".parse().unwrap();
        let mut base = Schema::default();
        base.modules.insert(f1.clone(), m1);
        base.modules.insert(f2.clone(), m2);
        base.tables.insert(a.clone(), checked("f1"));
        base.tables.insert(b.clone(), checked("f2"));
        let before_ids = ids(&base, &IdsFile::default());
        let mut desired = Schema::default();
        desired.tables.insert(a.clone(), table(None));
        let mut after_ids = before_ids.clone();
        let dropped = before_ids.table_uid(&a).unwrap().clone();
        after_ids.tables.remove(&dropped);
        after_ids.columns.retain(|_, column| column.table != a);
        after_ids.rename_table(&b, &a);
        let (s1, s2) = (Surface::Module(f1.clone()), Surface::Module(f2.clone()));
        let check =
            |table: &pbps_model::TableName, input: &Surface, binds: &str| SurfaceResolution {
                surface: Surface::Check {
                    table: table.clone(),
                    name: "c".into(),
                },
                current: Some(bound(
                    "pg_constraint",
                    "app.x.c",
                    std::slice::from_ref(input),
                    binds,
                )),
                desired: None,
            };
        let routine = |surface: &Surface| SurfaceResolution {
            surface: surface.clone(),
            current: Some(bound("pg_proc", "app.f", &[], "int4in")),
            desired: None,
        };
        let ordered = super::plan(
            crate::Side {
                schema: &base,
                ids: &before_ids,
            },
            crate::Side {
                schema: &desired,
                ids: &after_ids,
            },
            &Hints::default(),
            &[
                check(&a, &s1, "f1"),
                check(&b, &s2, "f2"),
                routine(&s1),
                routine(&s2),
            ],
            &pbps_dialect::MinimalDialect,
        )
        .unwrap();
        let steps = &ordered.changes.changes;
        let position = |f: &dyn Fn(&Change) -> bool| {
            steps
                .iter()
                .position(|p| f(&p.change))
                .unwrap_or_else(|| panic!("missing from {steps:?}"))
        };
        let removal = position(&|c| matches!(c, Change::DropCheck { table, .. } if table == &a));
        let table = position(&|c| matches!(c, Change::DropTable { uid, .. } if uid == &dropped));
        let drop = |id: &pbps_model::ModuleId| {
            position(&|c| matches!(c, Change::DropModule { id: d, .. } if d == id))
        };
        assert!(removal < drop(&f2), "{steps:?}");
        assert!(table < drop(&f1), "{steps:?}");
        // The dropped table's observation does not claim the renamed
        // table's removal: no edge ties that removal to the other function.
        let f1_drop = drop(&f1);
        assert!(
            !ordered
                .proof
                .edges()
                .iter()
                .any(|e| e.before == removal && e.after == f1_drop),
            "{steps:?}"
        );
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    /// A default removal the differ spells by the column's final address
    /// follows that column's recorded rename (#1292): `app.t.id DEFAULT 1`
    /// becomes `app.u.n` with no default. A table-only rename and a kept
    /// default are the controls.
    #[test]
    fn a_default_removal_at_the_final_address_follows_the_column_rename() {
        let t: pbps_model::TableName = "app.t".parse().unwrap();
        let u: pbps_model::TableName = "app.u".parse().unwrap();
        let column = |default: Option<&str>| {
            let mut c = Column::new("int".parse().unwrap());
            c.default = default.map(Into::into);
            c
        };
        let schema = |table: &pbps_model::TableName, name: &str, default: Option<&str>| {
            let mut t = Table::default();
            t.columns.insert(name.into(), column(default));
            let mut schema = Schema::default();
            schema.tables.insert(table.clone(), t);
            schema
        };
        let base = schema(&t, "id", Some("1"));
        let before_ids = ids(&base, &IdsFile::default());
        let plan = |name: &str, default: Option<&str>| {
            let desired = schema(&u, name, default);
            let mut after_ids = before_ids.clone();
            after_ids.rename_table(&t, &u);
            let uid = before_ids.column_uid(&t.column("id")).unwrap();
            after_ids.columns.get_mut(uid).unwrap().name = name.into();
            let attrdef = || Some(bound("pg_attrdef", "app.x.d", &[], "int4in"));
            // A removed default is covered under the spelling its removal
            // uses; a kept one under its base and final spellings.
            let observations = if default.is_some() {
                vec![
                    SurfaceResolution {
                        surface: Surface::Default(t.column("id")),
                        current: attrdef(),
                        desired: None,
                    },
                    SurfaceResolution {
                        surface: Surface::Default(u.column(name)),
                        current: None,
                        desired: attrdef(),
                    },
                ]
            } else {
                vec![SurfaceResolution {
                    surface: Surface::Default(u.column(name)),
                    current: attrdef(),
                    desired: None,
                }]
            };
            super::plan(
                crate::Side {
                    schema: &base,
                    ids: &before_ids,
                },
                crate::Side {
                    schema: &desired,
                    ids: &after_ids,
                },
                &Hints::default(),
                &observations,
                &pbps_dialect::MinimalDialect,
            )
            .unwrap()
        };
        let removal = |ordered: &Ordered| {
            ordered
                .changes
                .changes
                .iter()
                .position(|p| matches!(p.change, Change::AlterColumnDefault { to: None, .. }))
        };
        let renames = |ordered: &Ordered| -> Vec<usize> {
            ordered
                .changes
                .changes
                .iter()
                .enumerate()
                .filter(|(_, p)| {
                    matches!(
                        p.change,
                        Change::RenameTable { .. } | Change::RenameColumn { .. }
                    )
                })
                .map(|(i, _)| i)
                .collect()
        };
        // Combined table and column rename.
        let both = plan("n", None);
        let at = removal(&both).expect("the default is removed");
        assert_eq!(renames(&both).len(), 2, "{:?}", both.changes.changes);
        assert!(
            renames(&both).iter().all(|&r| r < at),
            "{:?}",
            both.changes.changes
        );
        both.proof.validate(&both.changes).unwrap();
        // Table-only rename.
        let table_only = plan("id", None);
        let at = removal(&table_only).expect("the default is removed");
        assert!(
            renames(&table_only).iter().all(|&r| r < at),
            "{:?}",
            table_only.changes.changes
        );
        table_only.proof.validate(&table_only.changes).unwrap();
        // A kept default is not removed at all.
        let kept = plan("n", Some("1"));
        assert_eq!(removal(&kept), None, "{:?}", kept.changes.changes);
        kept.proof.validate(&kept.changes).unwrap();
    }
}
