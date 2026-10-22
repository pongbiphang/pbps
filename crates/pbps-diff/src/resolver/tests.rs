use super::*;
use pbps_model::resolver::OrderEdge;
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
    let mut edited = result.changes.changes().clone();
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
    assert!(matches!(order(example(), edges), Err(Error::Cycle(_))));
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

/// DEC-1968.1: an edge only the evidence states is an obligation like any
/// other. A cycle through one is refused by name, whichever source closes it.
#[test]
fn a_cycle_through_an_execution_or_binding_edge_is_refused_by_name_not_dropped() {
    let edge = |before, after, reason| OrderEdge {
        before,
        after,
        reason,
    };
    for edges in [
        // Execution alone.
        vec![
            edge(0, 1, OrderReason::Authorization),
            edge(1, 0, OrderReason::Authorization),
        ],
        // Execution closed by a binding.
        vec![
            edge(0, 1, OrderReason::Authorization),
            edge(1, 2, OrderReason::Binding),
            edge(2, 0, OrderReason::Binding),
        ],
    ] {
        match order(example(), edges.iter().cloned().collect()) {
            Err(Error::Cycle(text)) => {
                assert!(text.contains("role a") && text.contains("role b"), "{text}");
                assert!(text.contains("authorization"), "{text}");
            }
            other => panic!("{edges:?}: {:?}", other.map(|o| o.changes)),
        }
    }
    // Negative: the same edges without the closing one order.
    assert!(
        order(
            example(),
            BTreeSet::from([edge(0, 1, OrderReason::Authorization)])
        )
        .is_ok()
    );
}

/// The evidence orders what the effects leave free (DEC-1968.2): a view
/// reads a table, which no effect says, and the plan arrives with the view
/// first. Guard: without the binding edge the view stays first.
#[test]
fn a_binding_edge_is_respected_where_the_effects_allow_the_other_order() {
    use pbps_model::{Hints, IdsFile, Module, ModuleKind, Schema};
    let base = Schema::default();
    let old_ids = IdsFile::default();
    let mut desired = Schema::default();
    let table: pbps_model::TableName = "app.t".parse().unwrap();
    desired
        .tables
        .insert(table.clone(), pbps_model::Table::default());
    let view: pbps_model::ModuleId = "app.v".parse().unwrap();
    desired.modules.insert(
        view.clone(),
        Module {
            kind: ModuleKind::View,
            description: None,
            definition: "SELECT 1 AS id".into(),
        },
    );
    let wanted_ids = ids(&desired, &old_ids);
    let (base_side, desired_side) = (
        crate::Side {
            schema: &base,
            ids: &old_ids,
        },
        crate::Side {
            schema: &desired,
            ids: &wanted_ids,
        },
    );
    let observations = [observation(
        Surface::Module(view.clone()),
        BTreeSet::from([Surface::Table(table.clone())]),
    )];
    let steps = |ordered: &Ordered| {
        ordered
            .changes
            .changes
            .iter()
            .map(|p| p.change.subject())
            .collect::<Vec<_>>()
    };
    let ordered = plan(
        base_side,
        desired_side,
        &Hints::default(),
        &observations,
        &pbps_dialect::MinimalDialect,
    )
    .unwrap();
    assert_eq!(steps(&ordered), ["app.t", "app.v"]);
    let binding: Vec<_> = ordered
        .proof
        .edges()
        .iter()
        .filter(|e| e.reason == OrderReason::Binding)
        .collect();
    assert_eq!(binding.len(), 1, "{:?}", ordered.proof.edges());
    // The view first, as the arrival order in a hand-built plan, and the
    // table after it: the evidence alone moves them.
    let wrong = ChangeSet {
        changes: vec![
            ordered.changes.changes[1].clone(),
            ordered.changes.changes[0].clone(),
        ],
    };
    let with = order_logical(
        wrong.clone(),
        &observations,
        &Default::default(),
        base_side,
        desired_side,
        &pbps_dialect::MinimalDialect,
    )
    .unwrap();
    assert_eq!(steps(&with), ["app.t", "app.v"]);
    let without = order_logical(
        wrong,
        &[],
        &Default::default(),
        base_side,
        desired_side,
        &pbps_dialect::MinimalDialect,
    )
    .unwrap();
    assert_eq!(steps(&without), ["app.v", "app.t"]);
}

mod unowned_steps {
    use super::*;
    use pbps_model::operation::{Origin, OwnerError, Source};
    use pbps_model::{CheckConstraint, Column, IdsFile, Module, ModuleKind, Schema, Table};

    /// A PostgreSQL-shaped check rebuild waits for a function. The ordinary
    /// plan also drops a check on a cross-schema table neither model owns.
    /// `order` selects the teardown, restoration, unowned drop and function
    /// create; origins deliberately differ from the prepared positions.
    fn rebuild(order: &[usize]) -> (ChangeSet, Schedule) {
        named_rebuild(order, "t", "t")
    }

    fn named_rebuild(order: &[usize], managed: &str, unmanaged: &str) -> (ChangeSet, Schedule) {
        let table = pbps_model::TableName::new("app", managed);
        let external = pbps_model::TableName::new("external", unmanaged);
        let external_shown = external.shown().to_string();
        let function: pbps_model::ModuleId = "app.f()".parse().unwrap();
        let check = Surface::Check {
            table: table.clone(),
            name: "ck".into(),
        };
        let mut shape = Table::default();
        shape
            .columns
            .insert("id".into(), Column::new("int".parse().unwrap()));
        shape.checks.insert(
            "ck".into(),
            CheckConstraint {
                expression: "id > 0".into(),
            },
        );
        let mut base = Schema::default();
        base.tables.insert(table.clone(), shape);
        let mut desired = base.clone();
        let constraint = CheckConstraint {
            expression: "app.f() > 0".into(),
        };
        desired
            .tables
            .get_mut(&table)
            .unwrap()
            .checks
            .insert("ck".into(), constraint.clone());
        let module = Module {
            kind: ModuleKind::Function,
            description: None,
            definition: "CREATE FUNCTION app.f() RETURNS integer LANGUAGE sql AS $$ SELECT 1 $$"
                .into(),
        };
        desired.modules.insert(function.clone(), module.clone());
        let before_ids = ids(&base, &IdsFile::default());
        let after_ids = ids(&desired, &before_ids);
        let create = PlannedChange::new(Change::CreateModule {
            id: function.clone(),
            module: Box::new(module),
        });
        let unowned = PlannedChange::new(Change::DropCheck {
            table: external,
            name: "old_check".into(),
        });
        let mut logical = ChangeSet {
            changes: vec![create.clone()],
        };
        if order.contains(&2) {
            logical.changes.push(unowned.clone());
        }
        let rebinding = Source::Rebinding(check.clone());
        let candidates = [
            (
                Origin::Teardown(rebinding.clone()),
                PlannedChange::new(Change::DropCheck {
                    table: table.clone(),
                    name: "ck".into(),
                }),
            ),
            (
                Origin::Restoration(rebinding),
                PlannedChange::new(Change::AddCheck {
                    table,
                    name: "ck".into(),
                    constraint,
                }),
            ),
            (Origin::Whole(1), unowned),
            (Origin::Whole(0), create),
        ];
        let (origins, steps): (Vec<_>, Vec<_>) =
            order.iter().map(|&i| candidates[i].clone()).unzip();
        let changes = ChangeSet { changes: steps };
        let owners = PlanOwners::new(
            pbps_model::plan_names::PlanNames::of(&logical.changes, &after_ids).unwrap(),
            &before_ids,
            &after_ids,
        );
        let operations = Operations::expanded(
            logical.changes.len(),
            origins.iter().cloned().zip(changes.changes.iter().cloned()),
            ModuleEdits::Rebuilt,
        );
        let (before, after) = (
            Snapshot {
                schema: &base,
                ids: &before_ids,
            },
            Snapshot {
                schema: &desired,
                ids: &after_ids,
            },
        );
        let shadowed = analyse(&operations, &owners, before, after, &Candidates);
        if let Some(at) = order.iter().position(|&i| i == 2) {
            assert!(matches!(
                shadowed.unowned.as_slice(),
                [(id, OwnerError::Unmanaged { name, .. })]
                    if *id == operations.as_slice()[at].id() && name == &external_shown
            ));
            assert_ne!(operations.as_slice()[at].id().source() as usize, at);
        } else {
            assert!(shadowed.unowned.is_empty());
        }
        let schedule = scheduled(
            &changes,
            &origins,
            &operations,
            &owners,
            &[observation(
                check,
                BTreeSet::from([Surface::Module(function)]),
            )],
            &Default::default(),
            crate::Side {
                schema: &base,
                ids: &before_ids,
            },
            crate::Side {
                schema: &desired,
                ids: &after_ids,
            },
            &Candidates,
        );
        (changes, schedule)
    }

    #[test]
    fn an_unowned_step_refuses_a_rebuild_that_would_cross_it_by_name() {
        let (given, schedule) = rebuild(&[0, 1, 2, 3]);
        let crossing = given.changes[1].change.subject();
        let unowned = given.changes[2].change.subject();
        match settle(given, &schedule) {
            Err(Error::Cycle(text)) => {
                assert!(
                    text.contains(&crossing) && text.contains(&unowned),
                    "{text}"
                );
                assert!(text.contains("names have no recorded owner"), "{text}");
            }
            other => panic!("settled: {:?}", other.map(|(o, _)| o.changes)),
        }
    }

    #[test]
    fn an_unowned_steps_refusal_shows_each_name_once() {
        let (given, schedule) = named_rebuild(&[0, 1, 2, 3], "a\\\nb", "c\\\nd");
        match settle(given, &schedule) {
            Err(Error::Cycle(text)) => assert_eq!(
                text,
                r"cannot reorder `app.a\\\nb` across `external.c\\\nd`: the latter's names have no recorded owner",
            ),
            other => panic!("settled: {:?}", other.map(|(o, _)| o.changes)),
        }
    }

    #[test]
    fn an_unowned_step_allows_a_rebuild_that_stays_on_its_side() {
        let (given, schedule) = rebuild(&[0, 3, 1, 2]);
        let (ordered, sequence) = settle(given.clone(), &schedule).unwrap();
        assert_eq!(ordered.changes, given);
        assert_eq!(sequence, [0, 1, 2, 3]);
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    #[test]
    fn a_rebuild_without_an_unowned_step_keeps_its_order_and_proof() {
        let (given, schedule) = rebuild(&[0, 1, 3]);
        let (ordered, sequence) = settle(given.clone(), &schedule).unwrap();
        assert_eq!(sequence, [0, 2, 1]);
        assert_eq!(
            ordered.changes.changes,
            [0, 2, 1].map(|i| given.changes[i].clone()),
        );
        assert_eq!(
            ordered.proof.edges(),
            &BTreeSet::from([
                OrderEdge {
                    before: 0,
                    after: 2,
                    reason: OrderReason::Identity,
                },
                OrderEdge {
                    before: 1,
                    after: 2,
                    reason: OrderReason::Binding,
                },
            ]),
        );
        ordered.proof.validate(&ordered.changes).unwrap();
    }
}

/// A resolver plan with a cycle through its own edges is refused by name: a
/// declared dependency of a view on another, and the evidence binding the
/// other to the view.
#[test]
fn a_resolver_plan_with_a_cycle_through_the_evidence_is_refused_by_name() {
    use pbps_model::{Hints, IdsFile, Module, ModuleKind, Schema};
    let base = Schema::default();
    let old_ids = IdsFile::default();
    let mut desired = Schema::default();
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
    let mut hints = Hints::default();
    hints.module_deps.insert(
        "app.va".parse().unwrap(),
        BTreeSet::from(["app.vz".parse().unwrap()]),
    );
    let observations = [
        observation(Surface::Module("app.va".parse().unwrap()), BTreeSet::new()),
        // `app.vz` is bound to `app.va`, which `app.va` is declared to follow.
        observation(
            Surface::Module("app.vz".parse().unwrap()),
            BTreeSet::from([Surface::Module("app.va".parse().unwrap())]),
        ),
    ];
    match plan(
        crate::Side {
            schema: &base,
            ids: &old_ids,
        },
        crate::Side {
            schema: &desired,
            ids: &wanted_ids,
        },
        &hints,
        &observations,
        &pbps_dialect::MinimalDialect,
    ) {
        Err(Error::Cycle(text)) => {
            assert!(text.contains("app.va") && text.contains("app.vz"), "{text}");
            assert!(
                text.contains("binding") && text.contains("structural"),
                "{text}"
            );
        }
        other => panic!("{:?}", other.map(|o| o.changes)),
    }
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
    spec.value_source = pbps_model::ValueSource::Default("app.f()".into());
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
        matches!(&steps[default_at].change, Change::AddColumn { column, .. } if column.value_source == pbps_model::ValueSource::Default("app.f()".into()))
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
        Err(Error::Cycle(_))
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
        .map(|id| id.raw().to_string())
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
    // A cycle of annotations alone is no longer refused here: `settle` keeps
    // the order given and the replay names the broken annotation (DEC-1956.3,
    // #2083). A test build's observer refuses that order, so the cycle is
    // pinned in `schedule_tests`, and the cycle the resolver still refuses, one
    // through its own edges, below.
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
    let mut missing = ordered.changes.changes().clone();
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
        PlannedChange::new(Change::AlterColumnType { uid: old_ids.column_uid(&parent.column("id")).unwrap().clone(), column: parent.column("id"), from: "int".parse::<ColumnType>().unwrap(), to: "text".parse().unwrap(), from_nullable: false, to_nullable: false, from_collation: None, to_collation: None, through_external_partitions: false }),
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
    let ordered = order_logical(
        changes,
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
        &pbps_dialect::MinimalDialect,
    )
    .unwrap();
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
            .deprecated = from.map(pbps_model::Text::from);
        let mut desired = base.clone();
        desired
            .tables
            .get_mut(&input.table)
            .unwrap()
            .columns
            .get_mut(&input.name)
            .unwrap()
            .deprecated = to.map(pbps_model::Text::from);
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
        let mut changed = ordered.changes.changes().clone();
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
        column.value_source = pbps_model::ValueSource::Generated(Generated {
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
        ordinary.value_source = pbps_model::ValueSource::Default("7".into());
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
                .value_source = pbps_model::ValueSource::None;
            assert!(matches!(
                absent_desired_default.plan(&absent_desired_default.resolution),
                Err(Error::Coverage(surface))
                    if surface == Surface::Default("app.t.d".parse().unwrap())
            ));
        }
    }

    /// The step installing a generation expression provides its `pg_attrdef`
    /// surface and follows the function it calls. This MinimalDialect fixture
    /// keeps the expression in CREATE TABLE or ADD COLUMN; PostgreSQL expands
    /// new tables into creation parts first (DEC-1503.1).
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
                    || matches!(c, Change::AddColumn { name, .. } if name.raw() == "g")
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

    /// A check whose binding changes with an unchanged text is torn down and put
    /// back around the rows the plan writes (review of #2120): an update that
    /// the old binding refuses meets neither (a new overload, a row update).
    #[test]
    fn a_rebound_check_is_torn_down_before_the_row_the_plan_updates() {
        use pbps_model::{DataMode, PrimaryKey, Row, RowKey, TableData, Value};
        let t: pbps_model::TableName = "app.t".parse().unwrap();
        let table = |n: i64| {
            let mut table = Table::default();
            table
                .columns
                .insert("id".into(), Column::new("int".parse().unwrap()).not_null());
            table
                .columns
                .insert("n".into(), Column::new("int".parse().unwrap()));
            table.primary_key = Some(PrimaryKey {
                name: None,
                columns: vec!["id".into()],
                storage_parameters: Default::default(),
            });
            table.checks.insert(
                "ck".into(),
                pbps_model::CheckConstraint {
                    expression: "n > app.f(1)".into(),
                },
            );
            table.data = Some(TableData {
                mode: DataMode::Exact,
                rows: [(
                    RowKey::from("1"),
                    [("n".into(), Value::Int(n))].into_iter().collect::<Row>(),
                )]
                .into_iter()
                .collect(),
            });
            table
        };
        let mut base = Schema::default();
        base.tables.insert(t.clone(), table(11));
        let mut desired = Schema::default();
        desired.tables.insert(t.clone(), table(1));
        let observation = SurfaceResolution {
            surface: Surface::Check {
                table: t.clone(),
                name: "ck".into(),
            },
            current: Some(bound("pg_constraint", "app.t.ck", &[], "f_old")),
            desired: Some(bound("pg_constraint", "app.t.ck", &[], "f_new")),
        };
        let ordered = sided(&base, &desired, &[observation]);
        let drop = at(&ordered, &|c| matches!(c, Change::DropCheck { .. }));
        let update = at(&ordered, &|c| matches!(c, Change::UpdateRow { .. }));
        let add = at(&ordered, &|c| matches!(c, Change::AddCheck { .. }));
        assert!(
            drop < update && update < add,
            "{:?}",
            ordered.changes.changes
        );
        ordered.proof.validate(&ordered.changes).unwrap();
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

    /// An index the evidence rebuilds is created with its declared storage
    /// parameters, so the plan's own change to them goes, as the differ's
    /// and the dependents pass's rebuilds drop it (#1516 review). Negative:
    /// the same change on an index the evidence leaves alone stays.
    #[test]
    fn a_rebuilt_index_takes_over_its_storage_parameter_change() {
        let (id, module) = routine();
        let indexed = |fillfactor: &str| {
            let mut table = table(None);
            table.indexes.insert(
                "ix".into(),
                pbps_model::Index {
                    columns: vec![pbps_model::IndexColumn {
                        key: pbps_model::IndexKey::Column("a".into()),
                        descending: false,
                        opclass: None,
                    }],
                    include: Vec::new(),
                    unique: false,
                    filter: Some("a > app.f(1)".into()),
                    method: Default::default(),
                    storage_parameters: [("fillfactor".to_owned(), fillfactor.to_owned())].into(),
                },
            );
            table
        };
        let mut base = Schema::default();
        base.modules.insert(id.clone(), module);
        base.tables.insert("app.t".parse().unwrap(), indexed("90"));
        let mut desired = base.clone();
        desired
            .tables
            .insert("app.t".parse().unwrap(), indexed("80"));
        let function = Surface::Module(id.clone());
        let index = Surface::Index {
            table: "app.t".parse().unwrap(),
            name: "ix".into(),
        };
        let parameters = |c: &Change| matches!(c, Change::SetIndexStorageParameters { .. });
        for rebinds in [true, false] {
            let ordered = sided(
                &base,
                &desired,
                &[
                    SurfaceResolution {
                        surface: index.clone(),
                        current: Some(bound(
                            "pg_index",
                            "app.ix",
                            std::slice::from_ref(&function),
                            "f",
                        )),
                        desired: Some(bound(
                            "pg_index",
                            "app.ix",
                            std::slice::from_ref(&function),
                            if rebinds { "g" } else { "f" },
                        )),
                    },
                    SurfaceResolution {
                        surface: function.clone(),
                        current: Some(bound("pg_proc", "app.f", &[], "int4in")),
                        desired: Some(bound("pg_proc", "app.f", &[], "int4in")),
                    },
                ],
            );
            ordered.proof.validate(&ordered.changes).unwrap();
            let kept = ordered
                .changes
                .changes
                .iter()
                .any(|p| parameters(&p.change));
            if rebinds {
                assert!(!kept, "{:?}", ordered.changes.changes);
                let added = ordered.changes.changes.iter().find_map(|p| {
                    if let Change::AddIndex { index, .. } = &p.change {
                        Some(index.storage_parameters.clone())
                    } else {
                        None
                    }
                });
                assert_eq!(
                    added,
                    Some([("fillfactor".to_owned(), "80".to_owned())].into()),
                    "{:?}",
                    ordered.changes.changes
                );
            } else {
                assert!(kept, "{:?}", ordered.changes.changes);
            }
        }
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
            &|c| matches!(c, Change::AddColumn { name, .. } if name.raw() == "g"),
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

    /// Two tables swapping names never reach the resolver: the ordinary
    /// plan refuses the cycle by name (#1966, DEC-1966.1), so no observation
    /// is ever paired across one. The pairing a swap exercised is pinned on a
    /// chain below.
    #[test]
    fn a_table_swap_is_refused_before_the_resolver_binds_it() {
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
        after_ids.tables.insert(ua, b.clone());
        after_ids.tables.insert(ub, a.clone());
        for column in after_ids.columns.values_mut() {
            column.table = if column.table == a {
                b.clone()
            } else {
                a.clone()
            };
        }
        let function = Surface::Module(id);
        let g = |table: &str| Surface::Default(format!("{table}.g").parse().unwrap());
        let attrdef = || {
            Some(bound(
                "pg_attrdef",
                "app.x.g",
                std::slice::from_ref(&function),
                "f",
            ))
        };
        let observations = [
            SurfaceResolution {
                surface: g("app.a"),
                current: attrdef(),
                desired: attrdef(),
            },
            SurfaceResolution {
                surface: g("app.b"),
                current: attrdef(),
                desired: attrdef(),
            },
            SurfaceResolution {
                surface: function.clone(),
                current: Some(bound("pg_proc", "app.f", &[], "int4in")),
                desired: Some(bound("pg_proc", "app.f", &[], "int4in")),
            },
        ];
        let refused = super::plan(
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
        );
        let Err(super::Error::Diff(reasons)) = refused else {
            panic!("a swap must be refused by the ordinary plan");
        };
        assert!(
            reasons
                .iter()
                .any(|r| r.contains("`app.a -> app.b`") && r.contains("`app.b -> app.a`")),
            "{reasons:?}"
        );
    }

    /// Two tables in a rename chain, app.b to app.c and then app.a to app.b,
    /// while each generation expression keeps its own binding. The app.b
    /// observation holds the opening record under its base spelling and the
    /// compiled one under its desired spelling, so one spelling pairs two
    /// different tables; the rebuild check pairs them through recorded UIDs,
    /// and a rename-only chain rewrites nothing (DEC-1498.1).
    #[test]
    fn chained_table_names_pair_bindings_through_recorded_uids() {
        let (id, module) = routine();
        let names =
            ["app.a", "app.b", "app.c"].map(|n| n.parse::<pbps_model::TableName>().unwrap());
        let [a, b, c] = names.clone();
        let mut base = Schema::default();
        base.modules.insert(id.clone(), module);
        base.tables.insert(a.clone(), table(Some("app.f(a)")));
        base.tables.insert(b.clone(), table(Some("app.f(a)")));
        let before_ids = ids(&base, &IdsFile::default());
        let mut desired = Schema {
            modules: base.modules.clone(),
            ..Default::default()
        };
        desired.tables.insert(b.clone(), table(Some("app.f(a)")));
        desired.tables.insert(c.clone(), table(Some("app.f(a)")));
        let mut after_ids = before_ids.clone();
        after_ids.rename_table(&b, &c);
        after_ids.rename_table(&a, &b);
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
                    desired: None,
                },
                SurfaceResolution {
                    surface: g("app.b"),
                    current: attrdef("f_exact"),
                    desired: attrdef(old_a_binds),
                },
                SurfaceResolution {
                    surface: g("app.c"),
                    current: None,
                    desired: attrdef("f_exact"),
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
        assert_eq!(
            unchanged
                .changes
                .changes
                .iter()
                .filter(|p| matches!(p.change, Change::RenameTable { .. }))
                .count(),
            2
        );
        assert_eq!(rewritten(&unchanged), Vec::new(), "a rename-only chain");
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
    /// Rename chain app.b to app.c, then app.a to app.b; the moved table's
    /// check calls a routine whose binding moves, so the resolver appends its
    /// rebuild: (base, desired, base ids, final ids, evidence).
    fn chained_check() -> (Schema, Schema, IdsFile, IdsFile, Vec<SurfaceResolution>) {
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
        let observations = vec![
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
        ];
        (base, desired, before_ids, after_ids, observations)
    }

    #[test]
    fn an_appended_teardown_belongs_to_its_base_table_in_a_rename_chain() {
        let [b, c] = ["app.b", "app.c"].map(|n| n.parse::<pbps_model::TableName>().unwrap());
        let (base, desired, before_ids, after_ids, observations) = chained_check();
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
            &observations,
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

    /// An ordinary step spelled ahead of a rename chain belongs to the table
    /// holding its name there, not to the one holding it once the plan has
    /// run. The differ spells its removals after the renames today, so the
    /// plan is built by hand: drop app.b's check, then app.b to app.c and
    /// app.a to app.b. Read by the final name, the drop would belong to the
    /// old app.a, follow its rename into app.b and hit it (#1871).
    #[test]
    fn an_ordinary_removal_ahead_of_a_rename_chain_belongs_to_the_table_there() {
        let [a, b, c] =
            ["app.a", "app.b", "app.c"].map(|n| n.parse::<pbps_model::TableName>().unwrap());
        let mut checked = table(None);
        checked.checks.insert(
            "k".into(),
            pbps_model::CheckConstraint {
                expression: "a > 0".into(),
            },
        );
        let mut base = Schema::default();
        base.tables.insert(a.clone(), table(None));
        base.tables.insert(b.clone(), checked);
        let mut desired = Schema::default();
        desired.tables.insert(b.clone(), table(None));
        desired.tables.insert(c.clone(), table(None));
        let before_ids = ids(&base, &IdsFile::default());
        let mut after_ids = before_ids.clone();
        after_ids.rename_table(&b, &c);
        after_ids.rename_table(&a, &b);
        let rename = |from: &pbps_model::TableName, to: &pbps_model::TableName| {
            PlannedChange::new(Change::RenameTable {
                uid: before_ids.table_uid(from).unwrap().clone(),
                from: from.clone(),
                to: to.clone(),
                defaults: Vec::new(),
            })
        };
        let changes = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::DropCheck {
                    table: b.clone(),
                    name: "k".into(),
                }),
                rename(&b, &c),
                rename(&a, &b),
            ],
        };
        let ordered = order_logical(
            changes,
            &[],
            &Default::default(),
            crate::Side {
                schema: &base,
                ids: &before_ids,
            },
            crate::Side {
                schema: &desired,
                ids: &after_ids,
            },
            &pbps_dialect::MinimalDialect,
        )
        .unwrap();
        let edges = ordered.proof.edges();
        let between = |before, after| edges.iter().any(|e| e.before == before && e.after == after);
        assert!(
            between(0, 1),
            "the drop goes before its table's rename: {edges:?}"
        );
        // Negative: it does not wait for the other table's rename into app.b.
        assert!(!between(2, 0) && !between(0, 2), "{edges:?}");
        assert!(matches!(
            ordered.changes.changes[0].change,
            Change::DropCheck { .. }
        ));
    }

    /// A rebuild of the table that moves into a reused name: app.a's check
    /// rebinds while app.b goes to app.c and app.a to app.b. Its restoration
    /// is spelled app.b and belongs to the table holding that name once the
    /// plan has run, so it follows app.a's rename into it. Read by the base
    /// name, app.b would be the other table, and the restoration could run
    /// before the name is the right table's (#1871).
    #[test]
    fn a_restoration_into_a_reused_name_follows_the_rename_into_it() {
        let (id, module) = routine();
        let [a, b, c] =
            ["app.a", "app.b", "app.c"].map(|n| n.parse::<pbps_model::TableName>().unwrap());
        let mut checked = table(None);
        checked.checks.insert(
            "k".into(),
            pbps_model::CheckConstraint {
                expression: "app.f(a) > 0".into(),
            },
        );
        let mut base = Schema::default();
        base.modules.insert(id.clone(), module);
        base.tables.insert(a.clone(), checked.clone());
        base.tables.insert(b.clone(), table(None));
        let mut desired = Schema {
            modules: base.modules.clone(),
            ..Default::default()
        };
        desired.tables.insert(b.clone(), checked);
        desired.tables.insert(c.clone(), table(None));
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
        let observations = [
            SurfaceResolution {
                surface: check(&a),
                current: constraint("f"),
                desired: None,
            },
            SurfaceResolution {
                surface: check(&b),
                current: None,
                desired: constraint("f_exact"),
            },
            SurfaceResolution {
                surface: function.clone(),
                current: Some(bound("pg_proc", "app.f", &[], "int4in")),
                desired: Some(bound("pg_proc", "app.f", &[], "int4in")),
            },
        ];
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
            &observations,
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
        let teardown = position(&|ch| matches!(ch, Change::DropCheck { table, .. } if table == &a));
        let away = position(&|ch| matches!(ch, Change::RenameTable { from, .. } if from == &b));
        let into = position(&|ch| matches!(ch, Change::RenameTable { to, .. } if to == &b));
        let restore = position(&|ch| matches!(ch, Change::AddCheck { table, .. } if table == &b));
        assert!(teardown < into, "{steps:?}");
        assert!(away < into, "{steps:?}");
        assert!(into < restore, "{steps:?}");
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    /// Owners are read from the ordinary plan's own names, so a plan whose
    /// names cannot be followed is refused, not ordered by guessed owners.
    /// Final identities the index cannot read stand in for such a plan
    /// (#1871).
    #[test]
    fn a_plan_whose_names_cannot_be_followed_is_refused() {
        let (base, desired, before_ids, after_ids, observations) = chained_check();
        let plan = |after_ids: &IdsFile| {
            super::plan(
                crate::Side {
                    schema: &base,
                    ids: &before_ids,
                },
                crate::Side {
                    schema: &desired,
                    ids: after_ids,
                },
                &Hints::default(),
                &observations,
                &pbps_dialect::MinimalDialect,
            )
        };
        let mut unreadable = after_ids.clone();
        unreadable.version += 1;
        match plan(&unreadable) {
            Err(super::Error::Names(reason)) => {
                assert!(reason.contains("final identities"), "{reason}")
            }
            other => panic!("planned: {:?}", other.map(|o| o.changes)),
        }
        // Negative: the same plan with readable identities is ordered.
        assert!(plan(&after_ids).is_ok());
    }

    /// The edge behind that order, which the order alone does not show: the
    /// restoration waits for the rename into its name, and not for the old
    /// holder of the name, which it does not belong to (#1871).
    #[test]
    fn a_restoration_belongs_to_the_final_holder_of_its_name() {
        use pbps_model::operation::{Origin, PlanOwners, Source};
        let [a, b, c] =
            ["app.a", "app.b", "app.c"].map(|n| n.parse::<pbps_model::TableName>().unwrap());
        let mut checked = table(None);
        checked.checks.insert(
            "k".into(),
            pbps_model::CheckConstraint {
                expression: "a > 0".into(),
            },
        );
        let mut base = Schema::default();
        base.tables.insert(a.clone(), checked.clone());
        base.tables.insert(b.clone(), table(None));
        let mut desired = Schema::default();
        desired.tables.insert(b.clone(), checked);
        desired.tables.insert(c.clone(), table(None));
        let before_ids = ids(&base, &IdsFile::default());
        let mut after_ids = before_ids.clone();
        after_ids.rename_table(&b, &c);
        after_ids.rename_table(&a, &b);
        let rename = |from: &pbps_model::TableName, to: &pbps_model::TableName| {
            PlannedChange::new(Change::RenameTable {
                uid: before_ids.table_uid(from).unwrap().clone(),
                from: from.clone(),
                to: to.clone(),
                defaults: Vec::new(),
            })
        };
        let logical = ChangeSet {
            changes: vec![rename(&b, &c), rename(&a, &b)],
        };
        let mut steps = logical.clone();
        steps.changes.push(PlannedChange::new(Change::AddCheck {
            table: b.clone(),
            name: "k".into(),
            constraint: pbps_model::CheckConstraint {
                expression: "a > 0".into(),
            },
        }));
        let rebinding = Source::Rebinding(Surface::Check {
            table: b.clone(),
            name: "k".into(),
        });
        let origins = [
            Origin::Whole(0),
            Origin::Whole(1),
            Origin::Restoration(rebinding),
        ];
        let owners = PlanOwners::new(
            pbps_model::plan_names::PlanNames::of(&logical.changes, &after_ids).unwrap(),
            &before_ids,
            &after_ids,
        );
        let operations = Operations::expanded(
            logical.changes.len(),
            origins.iter().cloned().zip(steps.changes.iter().cloned()),
            pbps_model::operation::ModuleEdits::InPlace,
        );
        let schedule = scheduled(
            &steps,
            &origins,
            &operations,
            &owners,
            &[],
            &Default::default(),
            crate::Side {
                schema: &base,
                ids: &before_ids,
            },
            crate::Side {
                schema: &desired,
                ids: &after_ids,
            },
            &pbps_dialect::MinimalDialect,
        );
        let edges = schedule.constraints();
        let between = |before, after| edges.iter().any(|e| e.before == before && e.after == after);
        assert!(between(1, 2), "after the rename into app.b: {edges:?}");
        // Negative: the old app.b's rename away is not its table's.
        assert!(!between(0, 2), "{edges:?}");
    }

    /// The rebuild appended in that chain spells its teardown by the base
    /// name and its restoration by the final one: two names for one check.
    /// Keyed by the table recorded under each, they are one key, whose
    /// teardown ends the baseline's instance and whose restoration produces
    /// the plan's; the table that took the old name is another key (#1961).
    #[test]
    fn a_rebuilds_teardown_ends_the_base_and_its_restoration_produces_the_final() {
        use pbps_model::operation::{
            InForce, Incarnations, ObjectKey, PlanOwners, ReplayError, Role, Source, Spelling,
            Transition,
        };
        let [b, c] = ["app.b", "app.c"].map(|n| n.parse::<pbps_model::TableName>().unwrap());
        let (base, desired, before_ids, after_ids, observations) = chained_check();
        let planned = super::plan_operations(
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
        .unwrap();
        let owners = PlanOwners::new(
            pbps_model::plan_names::PlanNames::of(&planned.logical.changes, &after_ids).unwrap(),
            &before_ids,
            &after_ids,
        );
        let ops = planned.operations.as_slice();
        let position = |f: &dyn Fn(&Change) -> bool| {
            ops.iter()
                .position(|op| f(op.change()))
                .unwrap_or_else(|| panic!("missing from {ops:?}"))
        };
        let down = position(&|ch| matches!(ch, Change::DropCheck { table, .. } if table == &b));
        let up = position(&|ch| matches!(ch, Change::AddCheck { table, .. } if table == &c));
        let (teardown, restoration) = (&ops[down], &ops[up]);
        assert_eq!(teardown.role(), Role::Teardown);
        assert_eq!(restoration.role(), Role::Restoration);
        assert_eq!(teardown.source(), restoration.source());
        assert!(matches!(
            teardown.source(),
            Source::Rebinding(Surface::Check { .. })
        ));
        assert_eq!(teardown.origin().spelling(), Spelling::Base);
        assert_eq!(restoration.origin().spelling(), Spelling::Final);
        let key = teardown.key(&owners).unwrap();
        assert_eq!(restoration.key(&owners).unwrap(), key);
        let ObjectKey::Part { table, .. } = &key else {
            panic!("{key:?}")
        };
        assert_eq!(Some(table.uid()), before_ids.table_uid(&b));
        // Negative: not the table that holds app.b once the plan has run.
        assert_ne!(Some(table.uid()), after_ids.table_uid(&b));
        let transitions = |op: &pbps_model::operation::Operation| {
            op.defines(&owners)
                .unwrap()
                .into_iter()
                .map(|d| (d.key, d.transition))
                .collect::<Vec<_>>()
        };
        assert_eq!(transitions(teardown), [(key.clone(), Transition::Ends)]);
        assert_eq!(
            transitions(restoration),
            [(key.clone(), Transition::Produces)]
        );
        let replay = Incarnations::replay(ops, &owners).unwrap();
        assert_eq!(replay.in_force(&key, down), InForce::Base);
        assert_eq!(replay.in_force(&key, down + 1), InForce::Absent);
        assert_eq!(replay.in_force(&key, up), InForce::Absent);
        // Whatever follows the restoration reads the plan's check.
        for after in up + 1..=ops.len() {
            assert_eq!(replay.in_force(&key, after), InForce::Final);
        }
        // Negative: restored ahead of its teardown, the teardown would end the
        // plan's own check, which the replay refuses as a third instance.
        let mut order: Vec<usize> = (0..ops.len()).collect();
        order.swap(down, up);
        match Incarnations::replay(
            crate::order::reordered_operations(&planned.operations, &order).as_slice(),
            &owners,
        ) {
            Err(ReplayError::Third { key: third, .. }) => assert_eq!(*third, key),
            other => panic!("replayed: {other:?}"),
        }
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
            c.value_source = default.map_or(pbps_model::ValueSource::None, |text| {
                pbps_model::ValueSource::Default(text.into())
            });
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

    /// Column renames chain in one table, `b` to `c` and then `a` to `b`. A
    /// default removal spelled `b` by its column's final name follows both
    /// renames. A teardown of the old `b`'s default, spelled `b` by its base
    /// name, precedes both. Each removal is read through its column's UID,
    /// not the name the two columns share in turn (#1292).
    #[test]
    fn a_default_removal_in_a_column_rename_chain_reads_its_column_uid() {
        let t: pbps_model::TableName = "app.t".parse().unwrap();
        let column = |default: Option<&str>| {
            let mut c = Column::new("int".parse().unwrap());
            c.value_source = default.map_or(pbps_model::ValueSource::None, |text| {
                pbps_model::ValueSource::Default(text.into())
            });
            c
        };
        let schema = |columns: &[(&str, Option<&str>)]| {
            let mut table = Table::default();
            for (name, default) in columns {
                table.columns.insert((*name).into(), column(*default));
            }
            let mut schema = Schema::default();
            schema.tables.insert(t.clone(), table);
            schema
        };
        // `second` is the base name of the column renamed to `c`.
        let plan = |label: &str,
                    second: &str,
                    base: &Schema,
                    desired: &Schema,
                    observations: &[SurfaceResolution]| {
            let before_ids = ids(base, &IdsFile::default());
            let mut after_ids = before_ids.clone();
            let a = before_ids.column_uid(&t.column("a")).unwrap().clone();
            let b = before_ids.column_uid(&t.column(second)).unwrap().clone();
            after_ids.columns.get_mut(&b).unwrap().name = "c".into();
            after_ids.columns.get_mut(&a).unwrap().name = "b".into();
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
            .unwrap_or_else(|error| panic!("{label}: {error:?}"))
        };
        let position = |ordered: &Ordered, f: &dyn Fn(&Change) -> bool| {
            ordered
                .changes
                .changes
                .iter()
                .position(|p| f(&p.change))
                .unwrap_or_else(|| panic!("missing from {:?}", ordered.changes.changes))
        };
        let rename = |from: &'static str| move |c: &Change| matches!(c, Change::RenameColumn { from: f, .. } if f.raw() == from);
        let removal = |c: &Change| matches!(c, Change::AlterColumnDefault { to: None, .. });
        let attrdef = |binds: &str| Some(bound("pg_attrdef", "app.t.d", &[], binds));

        // The old `a` loses its default under its final name `b`.
        let removed = plan(
            "final-spelled removal",
            "b",
            &schema(&[("a", Some("1")), ("b", None)]),
            &schema(&[("b", None), ("c", None)]),
            &[SurfaceResolution {
                surface: Surface::Default(t.column("b")),
                current: attrdef("int4in"),
                desired: None,
            }],
        );
        let at = position(&removed, &removal);
        assert!(
            position(&removed, &rename("b")) < at,
            "{:?}",
            removed.changes.changes
        );
        assert!(
            position(&removed, &rename("a")) < at,
            "{:?}",
            removed.changes.changes
        );
        removed.proof.validate(&removed.changes).unwrap();

        // The old `b` keeps its default but rebinds: its teardown is spelled
        // `b` by its base name and runs before `b` is renamed to `c`.
        let rebound = plan(
            "base-spelled teardown",
            "b",
            &schema(&[("a", None), ("b", Some("1"))]),
            &schema(&[("b", None), ("c", Some("1"))]),
            &[
                SurfaceResolution {
                    surface: Surface::Default(t.column("b")),
                    current: attrdef("f"),
                    desired: None,
                },
                SurfaceResolution {
                    surface: Surface::Default(t.column("c")),
                    current: None,
                    desired: attrdef("f_exact"),
                },
            ],
        );
        let teardown = position(&rebound, &removal);
        assert!(
            teardown < position(&rebound, &rename("b")),
            "{:?}",
            rebound.changes.changes
        );
        rebound.proof.validate(&rebound.changes).unwrap();

        // The chain folds case: `B` frees `b`, as the differ's rename order
        // reads it, so it goes first and the removal spelled `b` follows.
        let folded = plan(
            "case-folded chain",
            "B",
            &schema(&[("a", Some("1")), ("B", None)]),
            &schema(&[("b", None), ("c", None)]),
            &[SurfaceResolution {
                surface: Surface::Default(t.column("b")),
                current: attrdef("int4in"),
                desired: None,
            }],
        );
        let at = position(&folded, &removal);
        assert!(
            position(&folded, &rename("B")) < at,
            "{:?}",
            folded.changes.changes
        );
        assert!(
            position(&folded, &rename("a")) < at,
            "{:?}",
            folded.changes.changes
        );
        folded.proof.validate(&folded.changes).unwrap();

        // Both at once (review of #2120): the old `a` loses its default under
        // its final name `b` while the old `b` rebinds. The appended teardown,
        // spelled `b`, holds `b` to `c` back, and with it `a` to `b`; the
        // removal spelled `b` follows that last rename, not the teardown's
        // position.
        let both = plan(
            "teardown and final-spelled removal",
            "b",
            &schema(&[("a", Some("1")), ("b", Some("1"))]),
            &schema(&[("b", None), ("c", Some("1"))]),
            &[
                SurfaceResolution {
                    surface: Surface::Default(t.column("a")),
                    current: attrdef("int4in"),
                    desired: None,
                },
                SurfaceResolution {
                    surface: Surface::Default(t.column("b")),
                    current: attrdef("f"),
                    desired: None,
                },
                SurfaceResolution {
                    surface: Surface::Default(t.column("c")),
                    current: None,
                    desired: attrdef("f_exact"),
                },
            ],
        );
        let mut removals: Vec<usize> = both
            .changes
            .changes
            .iter()
            .enumerate()
            .filter(|(_, p)| removal(&p.change))
            .map(|(i, _)| i)
            .collect();
        removals.sort_unstable();
        let (away, into) = (position(&both, &rename("b")), position(&both, &rename("a")));
        assert!(
            matches!(removals[..], [teardown, spelled] if teardown < away && away < into && into < spelled),
            "{:?}",
            both.changes.changes
        );
        both.proof.validate(&both.changes).unwrap();

        // A drop frees the name instead: the old `b` is dropped, `a` takes
        // its name and loses its default. The drop goes first, as the
        // ordinary plan orders it, then the rename, then the removal.
        let base = schema(&[("a", Some("1")), ("b", None)]);
        let desired = schema(&[("b", None)]);
        let before_ids = ids(&base, &IdsFile::default());
        let mut after_ids = before_ids.clone();
        let a = before_ids.column_uid(&t.column("a")).unwrap().clone();
        let dropped = before_ids.column_uid(&t.column("b")).unwrap().clone();
        after_ids.columns.remove(&dropped);
        after_ids.columns.get_mut(&a).unwrap().name = "b".into();
        let freed = super::plan(
            crate::Side {
                schema: &base,
                ids: &before_ids,
            },
            crate::Side {
                schema: &desired,
                ids: &after_ids,
            },
            &Hints::default(),
            &[SurfaceResolution {
                surface: Surface::Default(t.column("b")),
                current: attrdef("int4in"),
                desired: None,
            }],
            &pbps_dialect::MinimalDialect,
        )
        .unwrap_or_else(|error| panic!("freeing drop: {error:?}"));
        let drop = position(&freed, &|c| matches!(c, Change::DropColumn { .. }));
        let into = position(&freed, &rename("a"));
        let at = position(&freed, &removal);
        assert!(drop < into && into < at, "{:?}", freed.changes.changes);
        freed.proof.validate(&freed.changes).unwrap();
    }

    /// A partitioned `app.t(d)` and its partition `app.p`, which holds its
    /// own default on `d` when `own` is given (#1578), and the parent's
    /// default when `parent` is.
    fn partitioned(parent: Option<&str>, own: Option<&str>) -> Schema {
        use pbps_model::{PartitionBound, PartitionBy, PartitionColumn, PartitionOf};
        let mut d = Column::new("int".parse().unwrap());
        if let Some(parent) = parent {
            d.value_source = pbps_model::ValueSource::Default(parent.into());
        }
        let mut schema = Schema::default();
        schema.tables.insert(
            "app.t".parse().unwrap(),
            Table {
                columns: [("d".into(), d)].into(),
                partition_by: Some(PartitionBy {
                    external: false,
                    columns: vec!["d".into()],
                    strategy: Default::default(),
                }),
                ..Default::default()
            },
        );
        schema.tables.insert(
            "app.p".parse().unwrap(),
            Table {
                partition_of: Some(PartitionOf {
                    parent: "app.t".parse().unwrap(),
                    bound: PartitionBound::Default,
                    columns: own
                        .map(|own| {
                            (
                                "d".into(),
                                PartitionColumn {
                                    default: Some(own.into()),
                                    not_null: false,
                                },
                            )
                        })
                        .into_iter()
                        .collect(),
                }),
                ..Default::default()
            },
        );
        schema
    }

    /// The default on `table.d` binds `app.f` before the plan and `app.g`
    /// after it, so the resolver rebuilds it.
    fn rebinds(table: &str) -> SurfaceResolution {
        let column: pbps_model::TableName = table.parse().unwrap();
        SurfaceResolution {
            surface: Surface::Default(column.column("d")),
            current: Some(bound("pg_attrdef", &format!("{table}.d"), &[], "f")),
            desired: Some(bound("pg_attrdef", &format!("{table}.d"), &[], "g")),
        }
    }

    /// The default on `table.d` keeps its binding.
    fn keeps(table: &str) -> SurfaceResolution {
        let column: pbps_model::TableName = table.parse().unwrap();
        let attrdef = Some(bound("pg_attrdef", &format!("{table}.d"), &[], "f"));
        SurfaceResolution {
            surface: Surface::Default(column.column("d")),
            current: attrdef.clone(),
            desired: attrdef,
        }
    }

    /// A function the plan drops (`app.f`) or creates (`app.g`), with no
    /// bindings of its own.
    fn function(name: &str, before: bool) -> (pbps_model::ModuleId, SurfaceResolution) {
        let id: pbps_model::ModuleId = format!("app.{name}(integer)").parse().unwrap();
        let object = Some(BoundSurface {
            object: ObjectIdentity {
                class: "pg_proc".into(),
                name: vec!["app".into(), name.into()],
                signature: vec![],
            },
            bindings: vec![],
            managed_inputs: BTreeSet::new(),
        });
        let resolution = SurfaceResolution {
            surface: Surface::Module(id.clone()),
            current: if before { object.clone() } else { None },
            desired: if before { None } else { object },
        };
        (id, resolution)
    }

    /// #1610: a partition's own default whose bindings change is released
    /// and restored in the resolver plan, as the ordinary path's
    /// `SetPartitionDefault` pair (DEC-1581.1): taken off the partition
    /// alone, then set again to its own with the parent's as the fallback.
    /// The release precedes the drop of the function it bound, and the
    /// restoration follows the creation of the one it binds. It used to be
    /// skipped, leaving the default bound to the old input.
    #[test]
    fn a_partitions_own_default_is_released_and_restored_when_it_rebinds() {
        let (f, dropped) = function("f", true);
        let (g, created) = function("g", false);
        let module = pbps_model::Module {
            kind: pbps_model::ModuleKind::Function,
            description: None,
            definition: "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1".into(),
        };
        let mut base = partitioned(Some("0"), Some("app.f(1)"));
        base.modules.insert(f.clone(), module.clone());
        let mut desired = partitioned(Some("0"), Some("app.f(1)"));
        desired.modules.insert(g.clone(), module);
        let p: pbps_model::TableName = "app.p".parse().unwrap();
        let own = SurfaceResolution {
            surface: Surface::Default(p.column("d")),
            current: Some(bound(
                "pg_attrdef",
                "app.p.d",
                &[Surface::Module(f.clone())],
                "f",
            )),
            desired: Some(bound(
                "pg_attrdef",
                "app.p.d",
                &[Surface::Module(g.clone())],
                "g",
            )),
        };
        let ordered = sided(&base, &desired, &[keeps("app.t"), own, dropped, created]);
        let release = at(&ordered, &|c| {
            matches!(c, Change::SetPartitionDefault { table, column, from: Some(from), to: None, fallback: None, .. }
                if *table == p && column.raw() == "d" && from == "app.f(1)")
        });
        let restore = at(&ordered, &|c| {
            matches!(c, Change::SetPartitionDefault { table, column, from: None, to: Some(to), fallback: Some(fallback), .. }
                if *table == p && column.raw() == "d" && to == "app.f(1)" && fallback == "0")
        });
        assert!(release < restore, "{:?}", ordered.changes.changes);
        let drop = at(
            &ordered,
            &|c| matches!(c, Change::DropModule { id, .. } if *id == f),
        );
        let create = at(
            &ordered,
            &|c| matches!(c, Change::CreateModule { id, .. } if *id == g),
        );
        assert!(
            release < drop && create < restore,
            "{:?}",
            ordered.changes.changes
        );
        // Negative: the parent's default keeps its binding, so it is not
        // rebuilt.
        assert!(
            !ordered
                .changes
                .changes
                .iter()
                .any(|c| matches!(c.change, Change::AlterColumnDefault { .. })),
            "{:?}",
            ordered.changes.changes
        );
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    /// #1610: when a parent's default and a partition's own both rebind, the
    /// partition's restoration follows the parent's. The engine recurses the
    /// parent's `SET DEFAULT` over every partition's own default, so the
    /// other way round would leave the partition with its parent's
    /// (DEC-1687.1).
    #[test]
    fn a_partitions_own_default_is_restored_after_its_parents() {
        let schema = partitioned(Some("app.f(0)"), Some("app.f(1)"));
        let ordered = sided(&schema, &schema, &[rebinds("app.t"), rebinds("app.p")]);
        let t: pbps_model::TableName = "app.t".parse().unwrap();
        let p: pbps_model::TableName = "app.p".parse().unwrap();
        let parent = at(
            &ordered,
            &|c| matches!(c, Change::AlterColumnDefault { column, to: Some(_), .. } if column.table == t),
        );
        let own = at(
            &ordered,
            &|c| matches!(c, Change::SetPartitionDefault { table, to: Some(_), .. } if *table == p),
        );
        assert!(parent < own, "{:?}", ordered.changes.changes);
        assert!(
            ordered
                .proof
                .edges()
                .iter()
                .any(|e| e.before == parent && e.after == own),
            "the order is an edge, not an accident of position: {:?}",
            ordered.proof.edges()
        );
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    /// #1610 review: one routine both defaults call is rebuilt. Each default
    /// is released before the routine's drop and restored after its
    /// creation, and the partition's restoration follows the parent's. The
    /// parent-before-partition edge covers the partition's restoration only:
    /// on its release it would close a cycle through the routine's rebuild.
    #[test]
    fn a_shared_routine_rebuild_orders_both_defaults_without_a_cycle() {
        let f: pbps_model::ModuleId = "app.f(integer)".parse().unwrap();
        let module = |body: &str| pbps_model::Module {
            kind: pbps_model::ModuleKind::Function,
            description: None,
            definition: format!("(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN {body}"),
        };
        let mut base = partitioned(Some("app.f(0)"), Some("app.f(1)"));
        base.modules.insert(f.clone(), module("$1"));
        let mut desired = partitioned(Some("app.f(0)"), Some("app.f(1)"));
        desired.modules.insert(f.clone(), module("$1 + 1"));
        let object = |name: &str| BoundSurface {
            object: ObjectIdentity {
                class: "pg_proc".into(),
                name: vec!["app".into(), name.into()],
                signature: vec![],
            },
            bindings: vec![],
            managed_inputs: BTreeSet::new(),
        };
        let routine = SurfaceResolution {
            surface: Surface::Module(f.clone()),
            current: Some(object("f")),
            desired: Some(object("f")),
        };
        let default = |table: &str| {
            let name: pbps_model::TableName = table.parse().unwrap();
            let attrdef = format!("{table}.d");
            SurfaceResolution {
                surface: Surface::Default(name.column("d")),
                current: Some(bound(
                    "pg_attrdef",
                    &attrdef,
                    &[Surface::Module(f.clone())],
                    "f",
                )),
                desired: Some(bound(
                    "pg_attrdef",
                    &attrdef,
                    &[Surface::Module(f.clone())],
                    "f_v2",
                )),
            }
        };
        let before_ids = ids(&base, &IdsFile::default());
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
            &[default("app.t"), default("app.p"), routine],
            &Candidates,
        )
        .unwrap_or_else(|error| panic!("a schedulable rebuild is refused: {error:?}"));
        let t: pbps_model::TableName = "app.t".parse().unwrap();
        let p: pbps_model::TableName = "app.p".parse().unwrap();
        let release_parent = at(
            &ordered,
            &|c| matches!(c, Change::AlterColumnDefault { column, to: None, .. } if column.table == t),
        );
        let release_own = at(
            &ordered,
            &|c| matches!(c, Change::SetPartitionDefault { table, to: None, fallback: None, .. } if *table == p),
        );
        let drop = at(
            &ordered,
            &|c| matches!(c, Change::DropModule { id, .. } if *id == f),
        );
        let create = at(
            &ordered,
            &|c| matches!(c, Change::CreateModule { id, .. } if *id == f),
        );
        let restore_parent = at(
            &ordered,
            &|c| matches!(c, Change::AlterColumnDefault { column, to: Some(_), .. } if column.table == t),
        );
        let restore_own = at(
            &ordered,
            &|c| matches!(c, Change::SetPartitionDefault { table, to: Some(_), .. } if *table == p),
        );
        assert!(
            release_parent < drop
                && release_own < drop
                && drop < create
                && create < restore_parent
                && restore_parent < restore_own,
            "{:?}",
            ordered.changes.changes
        );
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    /// #1610 review: a generated column dropped and its name taken by a new
    /// column, under a new UID and with a default, is the ordinary plan's
    /// `DropColumn` and `AddColumn`. Its default surface rebinds, but the
    /// base column has no default to release: the replacement already
    /// rebuilds it, so the plan is not refused for a missing default.
    #[test]
    fn a_replaced_column_is_rebuilt_by_its_replacement_not_refused() {
        let t: pbps_model::TableName = "app.t".parse().unwrap();
        let mut base = Schema::default();
        let mut table = Table::default();
        table
            .columns
            .insert("a".into(), Column::new("int".parse().unwrap()));
        table.columns.insert("c".into(), generated("app.f(a)"));
        base.tables.insert(t.clone(), table.clone());
        let mut desired = base.clone();
        let mut c = Column::new("int".parse().unwrap());
        c.value_source = pbps_model::ValueSource::Default("app.f(1)".into());
        desired
            .tables
            .get_mut(&t)
            .unwrap()
            .columns
            .insert("c".into(), c);
        let before_ids = ids(&base, &IdsFile::default());
        let mut after_ids = before_ids.clone();
        let old = before_ids.column_uid(&t.column("c")).unwrap().clone();
        after_ids.columns.remove(&old);
        let new: pbps_model::Uid = "c_zzzzzz".parse().unwrap();
        after_ids.columns.insert(new.clone(), t.column("c"));
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
            &[rebinds("app.t").clone()].map(|r| SurfaceResolution {
                surface: Surface::Default(t.column("c")),
                ..r
            }),
            &pbps_dialect::MinimalDialect,
        )
        .unwrap_or_else(|error| panic!("a replaced column is refused: {error:?}"));
        let dropped = at(
            &ordered,
            &|ch| matches!(ch, Change::DropColumn { uid, .. } if *uid == old),
        );
        let added = at(
            &ordered,
            &|ch| matches!(ch, Change::AddColumn { uid, .. } if *uid == new),
        );
        assert!(dropped < added, "{:?}", ordered.changes.changes);
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    /// Plans `base` to `desired` under the given identities, as `sided` does
    /// under resolved ones.
    fn planned(
        base: &Schema,
        before_ids: &IdsFile,
        desired: &Schema,
        after_ids: &IdsFile,
        observations: &[SurfaceResolution],
    ) -> Result<Ordered, super::super::Error> {
        super::plan(
            crate::Side {
                schema: base,
                ids: before_ids,
            },
            crate::Side {
                schema: desired,
                ids: after_ids,
            },
            &Hints::default(),
            observations,
            &pbps_dialect::MinimalDialect,
        )
    }

    /// #1610 review: a parent drops `d` and renames `e` to `d`. Its
    /// partition's own default on `d` had no override on `e` before, so only
    /// the plan's side has one: the ordinary plan installs it, and the plan
    /// is not refused for an old override that never existed.
    #[test]
    fn a_replaced_parent_column_is_not_refused_for_its_partitions_override() {
        use pbps_model::{PartitionBound, PartitionBy, PartitionColumn, PartitionOf};
        let t: pbps_model::TableName = "app.t".parse().unwrap();
        let p: pbps_model::TableName = "app.p".parse().unwrap();
        let tree = |columns: &[&str]| {
            let mut schema = Schema::default();
            schema.tables.insert(
                t.clone(),
                Table {
                    columns: columns
                        .iter()
                        .map(|c| ((*c).into(), Column::new("int".parse().unwrap())))
                        .collect(),
                    partition_by: Some(PartitionBy {
                        external: false,
                        columns: vec!["k".into()],
                        strategy: Default::default(),
                    }),
                    ..Default::default()
                },
            );
            schema.tables.insert(
                p.clone(),
                Table {
                    partition_of: Some(PartitionOf {
                        parent: t.clone(),
                        bound: PartitionBound::Default,
                        columns: [(
                            "d".into(),
                            PartitionColumn {
                                default: Some("app.f(1)".into()),
                                not_null: false,
                            },
                        )]
                        .into(),
                    }),
                    ..Default::default()
                },
            );
            schema
        };
        let base = tree(&["k", "d", "e"]);
        let desired = tree(&["k", "d"]);
        let before_ids = ids(&base, &IdsFile::default());
        let mut after_ids = before_ids.clone();
        let d = before_ids.column_uid(&t.column("d")).unwrap().clone();
        let e = before_ids.column_uid(&t.column("e")).unwrap().clone();
        after_ids.columns.remove(&d);
        after_ids.columns.get_mut(&e).unwrap().name = "d".into();
        let ordered = planned(
            &base,
            &before_ids,
            &desired,
            &after_ids,
            &[rebinds("app.p")],
        )
        .unwrap_or_else(|error| panic!("a replaced parent column is refused: {error:?}"));
        assert!(
            ordered
                .changes
                .changes
                .iter()
                .any(|c| matches!(&c.change, Change::DropColumn { uid, .. } if *uid == d)),
            "{:?}",
            ordered.changes.changes
        );
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    /// #1610 review: as above, but the partition holds its own default on
    /// `e` too, which the rename keeps as `d`'s. That default rebinds: its
    /// teardown, spelled `p.e`, runs before the parent renames `e`, and its
    /// restoration, spelled `p.d`, after it. Either other way addresses a
    /// column that is not there.
    #[test]
    fn a_partitions_kept_override_is_rebuilt_around_its_parents_column_rename() {
        use pbps_model::{PartitionBound, PartitionBy, PartitionColumn, PartitionOf};
        let t: pbps_model::TableName = "app.t".parse().unwrap();
        let p: pbps_model::TableName = "app.p".parse().unwrap();
        let tree = |columns: &[&str], overrides: &[(&str, &str)]| {
            let mut schema = Schema::default();
            schema.tables.insert(
                t.clone(),
                Table {
                    columns: columns
                        .iter()
                        .map(|c| ((*c).into(), Column::new("int".parse().unwrap())))
                        .collect(),
                    partition_by: Some(PartitionBy {
                        external: false,
                        columns: vec!["k".into()],
                        strategy: Default::default(),
                    }),
                    ..Default::default()
                },
            );
            schema.tables.insert(
                p.clone(),
                Table {
                    partition_of: Some(PartitionOf {
                        parent: t.clone(),
                        bound: PartitionBound::Default,
                        columns: overrides
                            .iter()
                            .map(|(c, default)| {
                                (
                                    (*c).into(),
                                    PartitionColumn {
                                        default: Some((*default).into()),
                                        not_null: false,
                                    },
                                )
                            })
                            .collect(),
                    }),
                    ..Default::default()
                },
            );
            schema
        };
        let base = tree(&["k", "d", "e"], &[("d", "app.f(1)"), ("e", "app.f(2)")]);
        let desired = tree(&["k", "d"], &[("d", "app.f(2)")]);
        let before_ids = ids(&base, &IdsFile::default());
        let mut after_ids = before_ids.clone();
        let d = before_ids.column_uid(&t.column("d")).unwrap().clone();
        let e = before_ids.column_uid(&t.column("e")).unwrap().clone();
        after_ids.columns.remove(&d);
        after_ids.columns.get_mut(&e).unwrap().name = "d".into();
        let removed = SurfaceResolution {
            surface: Surface::Default(p.column("e")),
            current: Some(bound("pg_attrdef", "app.p.e", &[], "f")),
            desired: None,
        };
        let ordered = planned(
            &base,
            &before_ids,
            &desired,
            &after_ids,
            &[rebinds("app.p"), removed],
        )
        .unwrap_or_else(|error| panic!("a kept override is refused: {error:?}"));
        let rename = at(&ordered, &|c| {
            matches!(c, Change::RenameColumn { table, from, to, .. }
                if *table == t && from.raw() == "e" && to.raw() == "d")
        });
        let release = at(&ordered, &|c| {
            matches!(c, Change::SetPartitionDefault { table, column, to: None, fallback: None, .. }
                if *table == p && column.raw() == "e")
        });
        let restore = at(&ordered, &|c| {
            matches!(c, Change::SetPartitionDefault { table, column, to: Some(to), .. }
                if *table == p && column.raw() == "d" && to == "app.f(2)")
        });
        assert!(
            release < rename && rename < restore,
            "{:?}",
            ordered.changes.changes
        );
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    /// A partitioned `parent(k, columns...)` with `partition` holding the
    /// given own defaults.
    fn tree(parent: &str, columns: &[&str], partition: &str, overrides: &[(&str, &str)]) -> Schema {
        use pbps_model::{PartitionBound, PartitionBy, PartitionColumn, PartitionOf};
        let mut schema = Schema::default();
        schema.tables.insert(
            parent.parse().unwrap(),
            Table {
                columns: columns
                    .iter()
                    .map(|c| ((*c).into(), Column::new("int".parse().unwrap())))
                    .collect(),
                partition_by: Some(PartitionBy {
                    external: false,
                    columns: vec!["k".into()],
                    strategy: Default::default(),
                }),
                ..Default::default()
            },
        );
        schema.tables.insert(
            partition.parse().unwrap(),
            Table {
                partition_of: Some(PartitionOf {
                    parent: parent.parse().unwrap(),
                    bound: PartitionBound::Default,
                    columns: overrides
                        .iter()
                        .map(|(c, default)| {
                            (
                                (*c).into(),
                                PartitionColumn {
                                    default: Some((*default).into()),
                                    not_null: false,
                                },
                            )
                        })
                        .collect(),
                }),
                ..Default::default()
            },
        );
        schema
    }

    /// #1610 review: the parent renames `e` to `f` and `d` to `e`, and the
    /// plan removes the partition's own default on the original `d`. The
    /// ordinary plan spells that removal `p.e`, the name the column has once
    /// both renames have run, so it follows them: placed before `e -> f`, it
    /// would take the default off the old `e` and leave `d`'s in place.
    #[test]
    fn an_ordinary_partition_removal_follows_its_parents_rename_chain() {
        let t: pbps_model::TableName = "app.t".parse().unwrap();
        let p: pbps_model::TableName = "app.p".parse().unwrap();
        let base = tree("app.t", &["k", "d", "e"], "app.p", &[("d", "app.f(1)")]);
        let desired = tree("app.t", &["k", "e", "f"], "app.p", &[]);
        let before_ids = ids(&base, &IdsFile::default());
        let mut after_ids = before_ids.clone();
        let d = before_ids.column_uid(&t.column("d")).unwrap().clone();
        let e = before_ids.column_uid(&t.column("e")).unwrap().clone();
        after_ids.columns.get_mut(&e).unwrap().name = "f".into();
        after_ids.columns.get_mut(&d).unwrap().name = "e".into();
        let removed = SurfaceResolution {
            surface: Surface::Default(p.column("d")),
            current: Some(bound("pg_attrdef", "app.p.d", &[], "f")),
            desired: None,
        };
        let ordered = planned(&base, &before_ids, &desired, &after_ids, &[removed])
            .unwrap_or_else(|error| panic!("a rename chain is refused: {error:?}"));
        let renamed = |from: &str| {
            at(
                &ordered,
                &|c| matches!(c, Change::RenameColumn { table, from: f, .. } if *table == t && f.raw() == from),
            )
        };
        let removal = at(&ordered, &|c| {
            matches!(c, Change::SetPartitionDefault { table, column, from: Some(_), to: None, .. }
                if *table == p && column.raw() == "e")
        });
        assert!(
            renamed("e") < removal && renamed("d") < removal,
            "{:?}",
            ordered.changes.changes
        );
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    /// #1610 review: the plan renames the parent `app.t` to `app.u` as well
    /// as its column `e` to `d`. The rebuilt override's release names the
    /// parent `app.t` and the rename names `app.u`, so the parent is matched
    /// by its UID: the release still goes before the column rename.
    #[test]
    fn a_partitions_rebuild_follows_its_parents_column_rename_through_a_table_rename() {
        let t: pbps_model::TableName = "app.t".parse().unwrap();
        let u: pbps_model::TableName = "app.u".parse().unwrap();
        let p: pbps_model::TableName = "app.p".parse().unwrap();
        let base = tree(
            "app.t",
            &["k", "d", "e"],
            "app.p",
            &[("d", "app.f(1)"), ("e", "app.f(2)")],
        );
        let desired = tree("app.u", &["k", "d"], "app.p", &[("d", "app.f(2)")]);
        let before_ids = ids(&base, &IdsFile::default());
        let mut after_ids = before_ids.clone();
        let parent = before_ids.table_uid(&t).unwrap().clone();
        let d = before_ids.column_uid(&t.column("d")).unwrap().clone();
        let e = before_ids.column_uid(&t.column("e")).unwrap().clone();
        *after_ids.tables.get_mut(&parent).unwrap() = u.clone();
        for column in after_ids.columns.values_mut() {
            if column.table == t {
                column.table = u.clone();
            }
        }
        after_ids.columns.remove(&d);
        after_ids.columns.get_mut(&e).unwrap().name = "d".into();
        let removed = SurfaceResolution {
            surface: Surface::Default(p.column("e")),
            current: Some(bound("pg_attrdef", "app.p.e", &[], "f")),
            desired: None,
        };
        let ordered = planned(
            &base,
            &before_ids,
            &desired,
            &after_ids,
            &[rebinds("app.p"), removed],
        )
        .unwrap_or_else(|error| panic!("a renamed parent is refused: {error:?}"));
        let rename = at(
            &ordered,
            &|c| matches!(c, Change::RenameColumn { from, to, .. } if from.raw() == "e" && to.raw() == "d"),
        );
        let release = at(&ordered, &|c| {
            matches!(c, Change::SetPartitionDefault { table, column, to: None, fallback: None, .. }
                if *table == p && column.raw() == "e")
        });
        let restore = at(&ordered, &|c| {
            matches!(c, Change::SetPartitionDefault { table, column, to: Some(_), .. }
                if *table == p && column.raw() == "d")
        });
        assert!(
            release < rename && rename < restore,
            "{:?}",
            ordered.changes.changes
        );
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    /// The plan's names resolve at every step, as the CLI checks before it
    /// renders a plan: a change addressed by a UID the plan has not made, or
    /// one sharing an address with another UID, is refused there.
    fn names_resolve(ordered: &Ordered, after: &IdsFile) {
        if let Err(error) = pbps_model::plan_names::PlanNames::of(&ordered.changes.changes, after) {
            panic!("{error:?}: {:?}", ordered.changes.changes);
        }
    }

    /// #1610 review: a default whose owner the plan replaces under its name,
    /// with a new UID, is created with the replacement and binds anew. No
    /// pair is added for it: a teardown under the new UID, spelled at the
    /// shared name before the old owner is dropped, is an address collision.
    /// Covered for a partition, an ordinary table and a column.
    #[test]
    fn a_replaced_default_owner_gets_no_rebuild_pair() {
        let p: pbps_model::TableName = "app.p".parse().unwrap();
        let t: pbps_model::TableName = "app.t".parse().unwrap();
        let added = |ordered: &Ordered| {
            ordered
                .changes
                .changes
                .iter()
                .filter(|c| {
                    matches!(
                        &c.change,
                        Change::SetPartitionDefault { to: None, .. }
                            | Change::AlterColumnDefault { to: None, .. }
                    )
                })
                .count()
        };

        let base = partitioned(None, Some("app.f(1)"));
        let before_ids = ids(&base, &IdsFile::default());
        let mut after_ids = before_ids.clone();
        let old = before_ids.table_uid(&p).unwrap().clone();
        after_ids.tables.remove(&old);
        after_ids
            .tables
            .insert("t_zzzzzz".parse().unwrap(), p.clone());
        let ordered = planned(&base, &before_ids, &base, &after_ids, &[rebinds("app.p")])
            .unwrap_or_else(|error| panic!("a replaced partition is refused: {error:?}"));
        names_resolve(&ordered, &after_ids);
        assert_eq!(added(&ordered), 0, "{:?}", ordered.changes.changes);

        let mut base = Schema::default();
        let mut table = Table::default();
        table
            .columns
            .insert("a".into(), Column::new("int".parse().unwrap()));
        let mut c = Column::new("int".parse().unwrap());
        c.value_source = pbps_model::ValueSource::Default("app.f(1)".into());
        table.columns.insert("c".into(), c);
        base.tables.insert(t.clone(), table);
        let before_ids = ids(&base, &IdsFile::default());
        let observation = SurfaceResolution {
            surface: Surface::Default(t.column("c")),
            ..rebinds("app.t")
        };
        // The whole table, then the column alone.
        let mut whole = IdsFile {
            version: before_ids.version,
            ..Default::default()
        };
        whole.tables.insert("t_zzzzzz".parse().unwrap(), t.clone());
        whole
            .columns
            .insert("c_yyyyyy".parse().unwrap(), t.column("a"));
        whole
            .columns
            .insert("c_zzzzzz".parse().unwrap(), t.column("c"));
        let mut column = before_ids.clone();
        let old = before_ids.column_uid(&t.column("c")).unwrap().clone();
        column.columns.remove(&old);
        column
            .columns
            .insert("c_zzzzzz".parse().unwrap(), t.column("c"));
        for after_ids in [whole, column] {
            let ordered = planned(
                &base,
                &before_ids,
                &base,
                &after_ids,
                std::slice::from_ref(&observation),
            )
            .unwrap_or_else(|error| panic!("a replaced owner is refused: {error:?}"));
            names_resolve(&ordered, &after_ids);
            assert_eq!(added(&ordered), 0, "{:?}", ordered.changes.changes);
        }

        // Negative: the same default on an owner that stands through the
        // plan is rebuilt.
        let ordered = planned(&base, &before_ids, &base, &before_ids, &[observation])
            .expect("a standing column's default is rebuilt");
        names_resolve(&ordered, &before_ids);
        assert_eq!(added(&ordered), 1, "{:?}", ordered.changes.changes);
    }

    /// #1610 review: an ordinary table attached while its default rebinds.
    /// The attach keeps the column's default as the partition's own (#1578):
    /// it is released as the column's before the attach, and restored as the
    /// partition's own after it.
    #[test]
    fn an_attached_tables_default_is_released_and_restored() {
        let p: pbps_model::TableName = "app.p".parse().unwrap();
        let mut base = partitioned(None, None);
        let mut d = Column::new("int".parse().unwrap());
        d.value_source = pbps_model::ValueSource::Default("app.f(1)".into());
        base.tables.insert(
            p.clone(),
            Table {
                columns: [("d".into(), d)].into(),
                ..Default::default()
            },
        );
        // A range, which a table is attached as; a DEFAULT partition is
        // created, not attached.
        let mut desired = partitioned(None, Some("app.f(1)"));
        desired
            .tables
            .get_mut(&p)
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap()
            .bound = pbps_model::PartitionBound::Range {
            from: vec![pbps_model::BoundDatum::Value("0".into())],
            to: vec![pbps_model::BoundDatum::Value("10".into())],
        };
        let before_ids = ids(&base, &IdsFile::default());
        let after_ids = ids(&desired, &before_ids);
        let ordered = planned(
            &base,
            &before_ids,
            &desired,
            &after_ids,
            &[rebinds("app.p")],
        )
        .unwrap_or_else(|error| panic!("an attach is refused: {error:?}"));
        let release = at(&ordered, &|c| {
            matches!(c, Change::AlterColumnDefault { column, from: Some(from), to: None, .. }
                if column.table == p && from == "app.f(1)")
        });
        let attach = at(&ordered, &|c| matches!(c, Change::AttachPartition { .. }));
        let restore = at(&ordered, &|c| {
            matches!(c, Change::SetPartitionDefault { table, from: None, to: Some(to), .. }
                if *table == p && to == "app.f(1)")
        });
        assert!(
            release < attach && attach < restore,
            "{:?}",
            ordered.changes.changes
        );
        names_resolve(&ordered, &after_ids);
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    /// #1610 review: the plan takes a partition's own default off, leaving
    /// its parent's, and drops the routine the own default called. Taking it
    /// off lets go of that routine, so it precedes the drop.
    #[test]
    fn a_partition_default_given_back_to_its_parents_precedes_its_routines_drop() {
        let (f, dropped) = function("f", true);
        let module = pbps_model::Module {
            kind: pbps_model::ModuleKind::Function,
            description: None,
            definition: "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1".into(),
        };
        let mut base = partitioned(Some("0"), Some("app.f(1)"));
        base.modules.insert(f.clone(), module);
        let desired = partitioned(Some("0"), None);
        let p: pbps_model::TableName = "app.p".parse().unwrap();
        let own = SurfaceResolution {
            surface: Surface::Default(p.column("d")),
            current: Some(bound(
                "pg_attrdef",
                "app.p.d",
                &[Surface::Module(f.clone())],
                "f",
            )),
            desired: None,
        };
        let ordered = sided(&base, &desired, &[keeps("app.t"), own, dropped]);
        let given_back = at(&ordered, &|c| {
            matches!(c, Change::SetPartitionDefault { table, from: Some(_), to: None, fallback: Some(_), .. }
                if *table == p)
        });
        let drop = at(
            &ordered,
            &|c| matches!(c, Change::DropModule { id, .. } if *id == f),
        );
        assert!(given_back < drop, "{:?}", ordered.changes.changes);
        assert!(
            ordered
                .proof
                .edges()
                .iter()
                .any(|e| e.before == given_back && e.after == drop),
            "the order is an edge, not an accident of position: {:?}",
            ordered.proof.edges()
        );
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    /// #1610 review: the parent's default rebinds and its partition's own
    /// keeps its binding. The parent's restoration is recursed by the engine
    /// into the partition, over its own default, so that is set again after
    /// it (DEC-1687.1). Negative: a partition holding only its parent's
    /// default is not set again.
    #[test]
    fn a_partitions_standing_default_is_set_again_after_its_parents_rebuild() {
        let t: pbps_model::TableName = "app.t".parse().unwrap();
        let p: pbps_model::TableName = "app.p".parse().unwrap();
        let schema = partitioned(Some("app.f(0)"), Some("7"));
        let before_ids = ids(&schema, &IdsFile::default());
        let ordered = planned(
            &schema,
            &before_ids,
            &schema,
            &before_ids,
            &[rebinds("app.t"), keeps("app.p")],
        )
        .unwrap_or_else(|error| panic!("a parent's rebuild is refused: {error:?}"));
        let parent = at(
            &ordered,
            &|c| matches!(c, Change::AlterColumnDefault { column, to: Some(_), .. } if column.table == t),
        );
        let again = at(&ordered, &|c| {
            matches!(c, Change::SetPartitionDefault { table, from: Some(from), to: Some(to), fallback: Some(fallback), .. }
                if *table == p && from == "7" && to == "7" && fallback == "app.f(0)")
        });
        assert!(
            ordered
                .proof
                .edges()
                .iter()
                .any(|e| e.before == parent && e.after == again),
            "the order is an edge, not an accident of position: {:?}",
            ordered.proof.edges()
        );
        names_resolve(&ordered, &before_ids);
        ordered.proof.validate(&ordered.changes).unwrap();

        let schema = partitioned(Some("app.f(0)"), None);
        let before_ids = ids(&schema, &IdsFile::default());
        let ordered = planned(
            &schema,
            &before_ids,
            &schema,
            &before_ids,
            &[rebinds("app.t")],
        )
        .unwrap_or_else(|error| panic!("a parent's rebuild is refused: {error:?}"));
        assert!(
            !ordered
                .changes
                .changes
                .iter()
                .any(|c| matches!(c.change, Change::SetPartitionDefault { .. })),
            "{:?}",
            ordered.changes.changes
        );
    }

    /// #1610 review: a table replaced under its name, with new UIDs, whose
    /// generated column comes back as an ordinary one with a default. Only
    /// the plan's side has a default; the ordinary plan's `DropTable` and
    /// `CreateTable` rebuild it, and the plan is not refused.
    #[test]
    fn a_replaced_table_is_not_refused_for_its_new_default() {
        let t: pbps_model::TableName = "app.t".parse().unwrap();
        let mut base = Schema::default();
        let mut table = Table::default();
        table
            .columns
            .insert("a".into(), Column::new("int".parse().unwrap()));
        table.columns.insert("c".into(), generated("app.f(a)"));
        base.tables.insert(t.clone(), table);
        let mut desired = base.clone();
        let mut c = Column::new("int".parse().unwrap());
        c.value_source = pbps_model::ValueSource::Default("app.f(1)".into());
        desired
            .tables
            .get_mut(&t)
            .unwrap()
            .columns
            .insert("c".into(), c);
        let before_ids = ids(&base, &IdsFile::default());
        let mut after_ids = IdsFile {
            version: before_ids.version,
            ..Default::default()
        };
        let old = before_ids.table_uid(&t).unwrap().clone();
        after_ids
            .tables
            .insert("t_zzzzzz".parse().unwrap(), t.clone());
        after_ids
            .columns
            .insert("c_yyyyyy".parse().unwrap(), t.column("a"));
        after_ids
            .columns
            .insert("c_zzzzzz".parse().unwrap(), t.column("c"));
        let observation = SurfaceResolution {
            surface: Surface::Default(t.column("c")),
            ..rebinds("app.t")
        };
        let ordered = planned(&base, &before_ids, &desired, &after_ids, &[observation])
            .unwrap_or_else(|error| panic!("a replaced table is refused: {error:?}"));
        let dropped = at(
            &ordered,
            &|c| matches!(c, Change::DropTable { uid, .. } if *uid == old),
        );
        let created = at(
            &ordered,
            &|c| matches!(c, Change::CreateTable { name, .. } if *name == t),
        );
        assert!(dropped < created, "{:?}", ordered.changes.changes);
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    /// #1610 review: a partition detached while its own default rebinds. The
    /// detach keeps the override as the table's default (#1578): it is
    /// released as the partition's own before the detach, and restored as
    /// the column's after it, the detach being what makes the column.
    #[test]
    fn a_detached_partitions_default_is_released_and_restored() {
        let base = partitioned(None, Some("app.f(1)"));
        let mut desired = base.clone();
        let p: pbps_model::TableName = "app.p".parse().unwrap();
        let mut d = Column::new("int".parse().unwrap());
        d.value_source = pbps_model::ValueSource::Default("app.f(1)".into());
        desired.tables.insert(
            p.clone(),
            Table {
                columns: [("d".into(), d)].into(),
                ..Default::default()
            },
        );
        let before_ids = ids(&base, &IdsFile::default());
        let after_ids = ids(&desired, &before_ids);
        let ordered = planned(
            &base,
            &before_ids,
            &desired,
            &after_ids,
            &[rebinds("app.p")],
        )
        .unwrap_or_else(|error| panic!("a detach is refused: {error:?}"));
        let release = at(&ordered, &|c| {
            matches!(c, Change::SetPartitionDefault { table, from: Some(from), to: None, fallback: None, .. }
                if *table == p && from == "app.f(1)")
        });
        let detach = at(&ordered, &|c| matches!(c, Change::DetachPartition { .. }));
        let restore = at(&ordered, &|c| {
            matches!(c, Change::AlterColumnDefault { column, from: None, to: Some(to), .. }
                if column.table == p && to == "app.f(1)")
        });
        assert!(
            release < detach && detach < restore,
            "{:?}",
            ordered.changes.changes
        );
        names_resolve(&ordered, &after_ids);
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    /// Negative for #1610: a partition holding only its parent's copy has no
    /// default of its own to rebuild. The parent's default is rebuilt, and
    /// the partition gets no pair.
    #[test]
    fn a_partition_holding_its_parents_default_gets_no_pair_of_its_own() {
        let schema = partitioned(Some("app.f(1)"), None);
        let ordered = sided(&schema, &schema, &[rebinds("app.t")]);
        let t: pbps_model::TableName = "app.t".parse().unwrap();
        let release = at(
            &ordered,
            &|c| matches!(c, Change::AlterColumnDefault { column, to: None, .. } if column.table == t),
        );
        let restore = at(
            &ordered,
            &|c| matches!(c, Change::AlterColumnDefault { column, from: None, to: Some(_), .. } if column.table == t),
        );
        assert!(release < restore, "{:?}", ordered.changes.changes);
        assert!(
            !ordered
                .changes
                .changes
                .iter()
                .any(|c| matches!(c.change, Change::SetPartitionDefault { .. })),
            "{:?}",
            ordered.changes.changes
        );
        ordered.proof.validate(&ordered.changes).unwrap();
    }

    /// #1610: a rebuilt default surface with no default on either side, own
    /// or column, is refused by name rather than skipped: an absent default
    /// is a definition the rebuild cannot spell, not nothing to rebuild.
    #[test]
    fn a_rebuilt_default_with_no_default_anywhere_is_refused_by_name() {
        let schema = partitioned(None, None);
        let before_ids = ids(&schema, &IdsFile::default());
        let after_ids = ids(&schema, &before_ids);
        for table in ["app.t", "app.p"] {
            let name: pbps_model::TableName = table.parse().unwrap();
            let surface = Surface::Default(name.column("d"));
            let result = super::super::prepare::changes(
                &ChangeSet::default(),
                &schema,
                &schema,
                &after_ids,
                &before_ids,
                &[surface.clone()].into(),
                &Hints::default(),
                &pbps_dialect::MinimalDialect,
            );
            assert!(
                matches!(&result, Err(super::super::Error::Definition(s)) if *s == surface),
                "{table}: {:?}",
                result.map(|prepared| prepared.changes)
            );
        }
        // Negative: the same surface with a default is rebuilt, not refused:
        // its pair, and for the parent the partition's own set again after
        // it.
        let schema = partitioned(Some("0"), Some("1"));
        let before_ids = ids(&schema, &IdsFile::default());
        let after_ids = ids(&schema, &before_ids);
        for (table, expected) in [("app.t", 3), ("app.p", 2)] {
            let name: pbps_model::TableName = table.parse().unwrap();
            let changes = super::super::prepare::changes(
                &ChangeSet::default(),
                &schema,
                &schema,
                &after_ids,
                &before_ids,
                &[Surface::Default(name.column("d"))].into(),
                &Hints::default(),
                &pbps_dialect::MinimalDialect,
            )
            .unwrap_or_else(|error| panic!("{table}: {error:?}"))
            .changes;
            assert_eq!(
                changes.changes.len(),
                expected,
                "{table}: {:?}",
                changes.changes
            );
        }
    }
}

/// A dialect whose candidate test rebuilds every unchanged module whenever
/// anything arrives, and whose `AlterModule` rebuilds: the shape of
/// PostgreSQL's ADR-0013 rule at its most conservative. Everything else is
/// `MinimalDialect`'s.
struct Candidates;

impl pbps_dialect::Dialect for Candidates {
    fn name(&self) -> &'static str {
        "candidates"
    }
    fn rebuilds_modules(&self) -> bool {
        true
    }
    fn rebound_modules(
        &self,
        declared: &pbps_model::Schema,
        arriving: &[pbps_model::ModuleId],
        already_changed: &BTreeSet<pbps_model::ModuleId>,
    ) -> BTreeSet<pbps_model::ModuleId> {
        if arriving.is_empty() {
            return BTreeSet::new();
        }
        declared
            .modules
            .keys()
            .filter(|id| !arriving.contains(id) && !already_changed.contains(id))
            .cloned()
            .collect()
    }
    fn quote_ident(&self, ident: &str) -> Result<String, pbps_dialect::DialectError> {
        pbps_dialect::MinimalDialect.quote_ident(ident)
    }
    fn emit(
        &self,
        change: &Change,
        strategy: pbps_model::Strategy,
    ) -> Result<Vec<pbps_dialect::Statement>, pbps_dialect::DialectError> {
        pbps_dialect::MinimalDialect.emit(change, strategy)
    }
    fn normalize_type(
        &self,
        ty: &pbps_model::ColumnType,
    ) -> Result<pbps_model::ColumnType, pbps_dialect::DialectError> {
        pbps_dialect::MinimalDialect.normalize_type(ty)
    }
    fn type_change_risk(
        &self,
        from: &pbps_model::ColumnType,
        to: &pbps_model::ColumnType,
    ) -> pbps_dialect::TypeChangeRisk {
        pbps_dialect::MinimalDialect.type_change_risk(from, to)
    }
    fn fold_ident<'a>(&self, ident: &'a str) -> std::borrow::Cow<'a, str> {
        pbps_dialect::MinimalDialect.fold_ident(ident)
    }
    fn lexicon(&self) -> pbps_dialect::Lexicon {
        pbps_dialect::MinimalDialect.lexicon()
    }
    fn validate_table(
        &self,
        name: &pbps_model::TableName,
        table: &pbps_model::Table,
    ) -> Vec<pbps_dialect::DialectError> {
        pbps_dialect::MinimalDialect.validate_table(name, table)
    }
    fn transaction_framing(
        &self,
        zone: &pbps_model::SessionZone,
    ) -> pbps_dialect::TransactionFraming {
        pbps_dialect::MinimalDialect.transaction_framing(zone)
    }
    fn probe_framing(&self) -> Option<pbps_dialect::TransactionFraming> {
        pbps_dialect::MinimalDialect.probe_framing()
    }
}

/// DEC-1515.1: on the resolver's path the evidence, not the candidate test,
/// decides which unchanged module an arrival rebuilds. The ordinary plan of
/// the same pair rebuilds the view; the resolved plan rebuilds it only when
/// its binding moves.
#[test]
fn evidence_replaces_the_candidate_rebuild_of_an_unchanged_module() {
    use pbps_model::resolver::{Binding, BoundSurface};
    use pbps_model::{Hints, IdsFile, Module, ModuleKind, Schema};
    let mut base = Schema::default();
    base.modules.insert(
        "app.v".parse().unwrap(),
        Module {
            kind: ModuleKind::View,
            description: None,
            definition: "SELECT f() AS x".into(),
        },
    );
    let mut desired = base.clone();
    desired.modules.insert(
        "app.f()".parse().unwrap(),
        Module {
            kind: ModuleKind::Function,
            description: None,
            definition: "RETURNS int LANGUAGE sql RETURN 1".into(),
        },
    );
    let old_ids = ids(&base, &IdsFile::default());
    let new_ids = ids(&desired, &old_ids);
    let base_side = crate::Side {
        schema: &base,
        ids: &old_ids,
    };
    let desired_side = crate::Side {
        schema: &desired,
        ids: &new_ids,
    };
    let view: pbps_model::ModuleId = "app.v".parse().unwrap();
    let rebuilds = |changes: &ChangeSet| {
        changes
            .changes
            .iter()
            .any(|p| p.change.module_id() == Some(&view))
    };
    let ordinary = crate::diff(base_side, desired_side, &Candidates, &Hints::default()).unwrap();
    assert!(rebuilds(&ordinary), "the candidate test rebuilds the view");
    let bound = |target: &str| BoundSurface {
        object: identity("v"),
        bindings: vec![Binding {
            node: "FuncExpr".into(),
            path: vec!["x".into()],
            target: identity(target),
        }],
        managed_inputs: BTreeSet::new(),
    };
    let resolved = |now: &str| {
        let observations = vec![
            SurfaceResolution {
                surface: Surface::Module("app.f()".parse().unwrap()),
                current: None,
                desired: Some(BoundSurface {
                    object: identity("f"),
                    bindings: vec![],
                    managed_inputs: BTreeSet::new(),
                }),
            },
            SurfaceResolution {
                surface: Surface::Module(view.clone()),
                current: Some(bound("ext.f")),
                desired: Some(bound(now)),
            },
        ];
        plan(
            base_side,
            desired_side,
            &Hints::default(),
            &observations,
            &Candidates,
        )
        .unwrap()
    };
    let unchanged = resolved("ext.f");
    assert!(
        !rebuilds(&unchanged.changes),
        "an unchanged binding is not rebuilt: {:?}",
        unchanged.changes
    );
    unchanged.proof.validate(&unchanged.changes).unwrap();
    let moved = resolved("app.f");
    assert!(
        rebuilds(&moved.changes),
        "a moved binding is rebuilt: {:?}",
        moved.changes
    );
}

/// A partitioned parent's new index stays ahead of each partition's own
/// under the resolver's order (#1737 review, DEC-1688.1): the ordinary
/// plan's rank is no edge, and built second, the parent's would take the
/// partition's own as its clone. An unrelated table's index is not tied to
/// it.
#[test]
fn a_parents_new_index_precedes_its_partitions_own_under_the_resolver() {
    use pbps_model::{
        Index, IndexColumn, PartitionBound, PartitionBy, PartitionOf, Schema, Table, TableName,
    };
    let parent: TableName = "app.ev".parse().unwrap();
    let partition: TableName = "app.aa".parse().unwrap();
    let other: TableName = "app.zz".parse().unwrap();
    let mut desired = Schema::default();
    desired.tables.insert(
        parent.clone(),
        Table {
            partition_by: Some(PartitionBy {
                external: false,
                columns: vec!["ts".into()],
                strategy: Default::default(),
            }),
            ..Default::default()
        },
    );
    desired.tables.insert(
        partition.clone(),
        Table {
            partition_of: Some(PartitionOf {
                parent: parent.clone(),
                bound: PartitionBound::Default,
                columns: Default::default(),
            }),
            ..Default::default()
        },
    );
    desired.tables.insert(other.clone(), Table::default());
    let index = Index {
        columns: vec![IndexColumn::column("n")],
        include: Vec::new(),
        unique: false,
        filter: None,
        method: Default::default(),
        storage_parameters: Default::default(),
    };
    let add = |table: &TableName, name: &str| {
        PlannedChange::new(Change::AddIndex {
            table: table.clone(),
            name: name.into(),
            index: Box::new(index.clone()),
            clustered: false,
        })
    };
    // In the wrong order, as nothing else would keep them.
    let changes = ChangeSet {
        changes: vec![
            add(&partition, "own"),
            add(&other, "z_n"),
            add(&parent, "ev_n"),
        ],
    };
    // The tables stand and the plan adds the three indexes.
    let base = desired.clone();
    for (table, name) in [(&parent, "ev_n"), (&partition, "own"), (&other, "z_n")] {
        desired
            .tables
            .get_mut(table)
            .unwrap()
            .indexes
            .insert(name.into(), index.clone());
    }
    let ids = ids(&desired, &pbps_model::IdsFile::default());
    let ordered = order_logical(
        changes,
        &[],
        &Default::default(),
        crate::Side {
            schema: &base,
            ids: &ids,
        },
        crate::Side {
            schema: &desired,
            ids: &ids,
        },
        &pbps_dialect::MinimalDialect,
    )
    .unwrap();
    let edges = ordered.proof.edges();
    // The parent's index is ordered before the partition's own by an edge,
    // not by where the differ put it (DEC-1688.1).
    let at = |table: &TableName| {
        ordered
            .changes
            .changes
            .iter()
            .position(|p| p.change.table() == Some(table))
            .unwrap()
    };
    assert!(
        edges
            .iter()
            .any(|e| e.before == at(&parent) && e.after == at(&partition)),
        "{edges:?}"
    );
    // Negative: nothing ties the unrelated table's index to either.
    assert!(
        !edges
            .iter()
            .any(|e| e.before == at(&other) || e.after == at(&other)),
        "{edges:?}"
    );
    let tables: Vec<&TableName> = ordered
        .changes
        .changes
        .iter()
        .filter_map(|p| p.change.table())
        .filter(|t| **t != other)
        .collect();
    assert_eq!(tables, [&parent, &partition]);
}

/// The plan a resolver orders carries the move a human allowed into the
/// create it takes apart, as the ordinary plan does (#1788): the bound is
/// the moving statement's only source.
#[test]
fn a_resolver_plan_keeps_the_move_a_human_allowed() {
    use pbps_model::{BoundDatum, Hints, IdsFile, PartitionBound, PartitionBy, PartitionOf};
    let mut base = pbps_model::Schema::default();
    let mut parent = pbps_model::Table::default();
    parent.columns.insert(
        "ts".into(),
        pbps_model::Column::new("date".parse().unwrap()).not_null(),
    );
    parent.partition_by = Some(PartitionBy {
        external: false,
        columns: vec!["ts".into()],
        strategy: Default::default(),
    });
    base.tables.insert("app.ev".parse().unwrap(), parent);
    let partition = |bound: PartitionBound| pbps_model::Table {
        partition_of: Some(PartitionOf {
            parent: "app.ev".parse().unwrap(),
            bound,
            columns: Default::default(),
        }),
        ..Default::default()
    };
    base.tables.insert(
        "app.ev_rest".parse().unwrap(),
        partition(PartitionBound::Default),
    );
    let mut desired = base.clone();
    desired.tables.insert(
        "app.ev_2025".parse().unwrap(),
        partition(PartitionBound::Range {
            from: vec![BoundDatum::Value("2025-01-01".into())],
            to: vec![BoundDatum::Value("2026-01-01".into())],
        }),
    );
    let old_ids = ids(&base, &IdsFile::default());
    let bound = |intents: &[pbps_model::Intent]| {
        let new_ids = crate::resolve(
            &desired,
            &old_ids,
            intents,
            &crate::Context {
                operator: "1788".into(),
                today: "2026-10-11".into(),
            },
        )
        .unwrap()
        .ids;
        let ordered = plan(
            crate::Side {
                schema: &base,
                ids: &old_ids,
            },
            crate::Side {
                schema: &desired,
                ids: &new_ids,
            },
            &Hints::default(),
            &[],
            &pbps_dialect::MinimalDialect,
        )
        .unwrap();
        ordered
            .changes
            .changes
            .iter()
            .find_map(|p| {
                if let Change::CreateTable {
                    beside_default: Some(_),
                    moves_at_most,
                    ..
                } = &p.change
                {
                    Some(*moves_at_most)
                } else {
                    None
                }
            })
            .expect("a create beside the DEFAULT")
    };
    assert_eq!(
        bound(&[pbps_model::Intent::MoveRows {
            table: "app.ev_2025".parse().unwrap(),
            max_rows: 7,
            reason: "no CDC".into(),
        }]),
        Some(7)
    );
    // Negative: nothing allowed.
    assert_eq!(bound(&[]), None);
}

/// The operations the resolver orders beside its plan (#1961). Each reads
/// its own payload: the models supply who owns a name and nothing about what
/// a step does.
mod operations {
    use super::*;
    use pbps_model::operation::{
        InForce, Incarnations, ObjectKey, Operation, Origin, PartKey, PlanOwners, ReplayError,
        Role, Source, Transition,
    };
    use pbps_model::plan_names::PlanNames;
    use pbps_model::{
        CheckConstraint, Hints, IdsFile, Index, IndexColumn, Module, ModuleKind, Schema,
        ValueSource,
    };

    /// `app.a` with a default, a check and an index, which the resolver
    /// takes out of its `CREATE`, and `app.z` with nothing to take out.
    fn created() -> (Schema, Schema) {
        let mut desired = tables();
        let a = desired.tables.get_mut(&"app.a".parse().unwrap()).unwrap();
        a.columns.get_mut("id").unwrap().value_source = ValueSource::Default("0".into());
        a.checks.insert(
            "k".into(),
            CheckConstraint {
                expression: "id >= 0".into(),
            },
        );
        a.indexes.insert(
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
        (Schema::default(), desired)
    }

    /// Every surface of a schema the plan creates whole, observed.
    fn observed(desired: &Schema) -> Vec<SurfaceResolution> {
        super::super::prepare::surfaces(desired)
            .into_iter()
            .map(|surface| observation(surface, BTreeSet::new()))
            .collect()
    }

    fn planned(
        base: &Schema,
        desired: &Schema,
        observations: &[SurfaceResolution],
        dialect: &(impl pbps_dialect::Dialect + ?Sized),
    ) -> (Planned, IdsFile, IdsFile) {
        let old_ids = ids(base, &IdsFile::default());
        let new_ids = ids(desired, &old_ids);
        let planned = plan_operations(
            crate::Side {
                schema: base,
                ids: &old_ids,
            },
            crate::Side {
                schema: desired,
                ids: &new_ids,
            },
            &Hints::default(),
            observations,
            dialect,
        )
        .unwrap();
        (planned, old_ids, new_ids)
    }

    fn owners<'p>(planned: &'p Planned, old: &'p IdsFile, new: &'p IdsFile) -> PlanOwners<'p> {
        PlanOwners::new(
            PlanNames::of(&planned.logical.changes, new).unwrap(),
            old,
            new,
        )
    }

    fn find<'o>(planned: &'o Planned, f: &dyn Fn(&Change) -> bool) -> (usize, &'o Operation) {
        planned
            .operations
            .iter()
            .enumerate()
            .find(|(_, op)| f(op.change()))
            .unwrap_or_else(|| panic!("missing from {:?}", planned.operations))
    }

    /// The bare table the resolver splits off a `CREATE TABLE` creates the
    /// table and its columns and nothing else: its default, check and index
    /// are the later operations', which produce them. Read from the desired
    /// table instead, the bare table would claim all three.
    #[test]
    fn a_bare_table_operation_defines_no_index_check_or_default_and_the_later_ones_do() {
        let (base, desired) = created();
        let (planned, old, new) = planned(
            &base,
            &desired,
            &observed(&desired),
            &pbps_dialect::MinimalDialect,
        );
        let owners = owners(&planned, &old, &new);
        let a: pbps_model::TableName = "app.a".parse().unwrap();
        let z: pbps_model::TableName = "app.z".parse().unwrap();
        let (bare_at, bare) = find(
            &planned,
            &|c| matches!(c, Change::CreateTable { name, .. } if name == &a),
        );
        let defined = |op: &Operation| -> Vec<ObjectKey> {
            op.defines(&owners)
                .unwrap()
                .into_iter()
                .map(|d| {
                    assert_eq!(d.transition, Transition::Produces, "{op:?}");
                    d.key
                })
                .collect()
        };
        let bare_keys = defined(bare);
        assert_eq!(bare.role(), Role::SplitPart);
        assert!(
            bare_keys
                .iter()
                .all(|k| matches!(k, ObjectKey::Table(_) | ObjectKey::Column(_))),
            "{bare_keys:?}"
        );
        assert_eq!(bare_keys.len(), 2, "the table and its one column");
        let replay = Incarnations::replay(planned.operations.as_slice(), &owners).unwrap();
        let later = [
            find(&planned, &|c| {
                matches!(c, Change::AlterColumnDefault { .. })
            }),
            find(&planned, &|c| matches!(c, Change::AddCheck { .. })),
            find(&planned, &|c| matches!(c, Change::AddIndex { .. })),
        ];
        for (at, op) in later {
            assert!(at > bare_at, "{:?}", planned.operations);
            // One logical change: the same source, another part of it.
            assert_eq!(op.source(), bare.source());
            assert_eq!(op.role(), Role::SplitPart);
            let keys = defined(op);
            assert_eq!(keys.len(), 1, "{op:?}: {keys:?}");
            let key = &keys[0];
            assert!(
                matches!(
                    key,
                    ObjectKey::Default(_)
                        | ObjectKey::Part {
                            part: PartKey::Check(_) | PartKey::Index(_),
                            ..
                        }
                ),
                "{key:?}"
            );
            assert!(!bare_keys.contains(key), "{key:?} claimed by {bare:?}");
            // Nothing of it stands after the bare table; it does after its
            // own operation.
            assert_eq!(replay.in_force(key, bare_at + 1), InForce::Absent);
            assert_eq!(replay.in_force(key, at), InForce::Absent);
            assert_eq!(replay.in_force(key, at + 1), InForce::Final);
        }
        // A create with nothing to take out is one whole operation.
        let (_, whole) = find(
            &planned,
            &|c| matches!(c, Change::CreateTable { name, .. } if name == &z),
        );
        assert_eq!(whole.role(), Role::Whole);
        assert_ne!(whole.source(), bare.source());
        assert!(
            planned
                .operations
                .iter()
                .filter(|op| op.source() == whole.source())
                .count()
                == 1
        );
    }

    /// The operations are a layer beside the plan, not in it: the ordered
    /// plan and its proof are `plan`'s, and so the saved plan's bytes and
    /// checksum. The ids follow from the plan alone, so two runs give the
    /// same ones.
    #[test]
    fn operation_ids_repeat_across_runs_and_the_ordered_plan_is_unchanged() {
        let (base, desired) = created();
        let observations = observed(&desired);
        let old = super::ids(&base, &IdsFile::default());
        let new = super::ids(&desired, &old);
        let base = crate::Side {
            schema: &base,
            ids: &old,
        };
        let desired = crate::Side {
            schema: &desired,
            ids: &new,
        };
        let dialect = &pbps_dialect::MinimalDialect;
        let run =
            || plan_operations(base, desired, &Hints::default(), &observations, dialect).unwrap();
        let first = run();
        let second = run();
        let ids = |p: &Planned| {
            p.operations
                .iter()
                .map(|op| op.id().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&first), ids(&second));
        assert_eq!(ids(&first), ["0.0", "0.1", "0.2", "0.3", "1.0"]);
        let ordered = plan(base, desired, &Hints::default(), &observations, dialect).unwrap();
        assert_eq!(first.ordered.changes, ordered.changes);
        assert_eq!(first.ordered.proof, ordered.proof);
        // One operation per sealed step, carrying that step's change.
        let steps: Vec<&PlannedChange> = first.operations.iter().map(|op| op.planned()).collect();
        let sealed: Vec<&PlannedChange> = ordered.changes.changes.iter().collect();
        assert_eq!(steps, sealed);
        // Negative: an id is not a position. The second table's names its
        // logical change, the plan's second, while it runs fifth.
        let (at, whole) = find(
            &first,
            &|c| matches!(c, Change::CreateTable { name, .. } if name.name == "z"),
        );
        assert_eq!(at, 4);
        assert_eq!(whole.source(), Source::Change(1));
        assert!(matches!(whole.origin(), Origin::Whole(1)));
    }

    /// A view the evidence rebuilds on an engine that rebuilds modules is
    /// one logical change in two parts: its drop ends the instance the
    /// baseline holds and its create produces the one the plan leaves, under
    /// one key. The create is made over the plan's function, and everything
    /// after it reads the plan's view. Created before its drop, the view
    /// would be read as a third instance, which the replay refuses.
    #[test]
    fn a_rebuilt_views_drop_ends_the_base_and_its_create_produces_the_final() {
        use pbps_model::resolver::{Binding, BoundSurface};
        let view: pbps_model::ModuleId = "app.v".parse().unwrap();
        let function: pbps_model::ModuleId = "app.f()".parse().unwrap();
        let mut base = Schema::default();
        base.modules.insert(
            view.clone(),
            Module {
                kind: ModuleKind::View,
                description: None,
                definition: "SELECT f() AS x".into(),
            },
        );
        let mut desired = base.clone();
        desired.modules.insert(
            function.clone(),
            Module {
                kind: ModuleKind::Function,
                description: None,
                definition: "RETURNS int LANGUAGE sql RETURN 1".into(),
            },
        );
        let bound = |target: &str| BoundSurface {
            object: identity("v"),
            bindings: vec![Binding {
                node: "FuncExpr".into(),
                path: vec!["x".into()],
                target: identity(target),
            }],
            managed_inputs: BTreeSet::new(),
        };
        let observations = [
            SurfaceResolution {
                surface: Surface::Module(function.clone()),
                current: None,
                desired: Some(BoundSurface {
                    object: identity("f"),
                    bindings: vec![],
                    managed_inputs: BTreeSet::new(),
                }),
            },
            SurfaceResolution {
                surface: Surface::Module(view.clone()),
                current: Some(bound("ext.f")),
                desired: Some(bound("app.f")),
            },
        ];
        let (planned, old, new) = planned(&base, &desired, &observations, &Candidates);
        let owners = owners(&planned, &old, &new);
        let (down, teardown) = find(
            &planned,
            &|c| matches!(c, Change::DropModule { id, .. } if id == &view),
        );
        let (up, restoration) = find(
            &planned,
            &|c| matches!(c, Change::CreateModule { id, .. } if id == &view),
        );
        let (made, _) = find(
            &planned,
            &|c| matches!(c, Change::CreateModule { id, .. } if id == &function),
        );
        assert_eq!(teardown.role(), Role::SplitPart);
        assert_eq!(restoration.role(), Role::SplitPart);
        assert_eq!(teardown.source(), restoration.source());
        let Source::Change(logical) = teardown.source() else {
            panic!("{teardown:?}")
        };
        assert!(matches!(
            &planned.logical.changes[logical].change,
            Change::AlterModule { id, .. } if id == &view
        ));
        let key = teardown.key(&owners).unwrap();
        assert_eq!(restoration.key(&owners).unwrap(), key);
        assert_eq!(
            teardown.defines(&owners).unwrap()[0].transition,
            Transition::Ends
        );
        assert_eq!(
            restoration.defines(&owners).unwrap()[0].transition,
            Transition::Produces
        );
        let replay = Incarnations::replay(planned.operations.as_slice(), &owners).unwrap();
        assert!(down < up, "{:?}", planned.operations);
        assert_eq!(replay.in_force(&key, down), InForce::Base);
        assert_eq!(replay.in_force(&key, down + 1), InForce::Absent);
        assert_eq!(replay.in_force(&key, up), InForce::Absent);
        // Everything after the restoration reads the plan's view.
        for after in up + 1..=planned.operations.len() {
            assert_eq!(replay.in_force(&key, after), InForce::Final);
        }
        // And the restoration reads the plan's function, never a missing one.
        assert!(made < up, "{:?}", planned.operations);
        assert_eq!(
            replay.in_force(&ObjectKey::Module(function.clone()), up),
            InForce::Final
        );
        // Negative: the restoration moved ahead of its teardown reads one
        // view as both instances, and is refused.
        let mut order: Vec<usize> = (0..planned.operations.len()).collect();
        order.swap(down, up);
        match Incarnations::replay(
            crate::order::reordered_operations(&planned.operations, &order).as_slice(),
            &owners,
        ) {
            Err(ReplayError::Third { key: third, .. }) => assert_eq!(*third, key),
            other => panic!("replayed: {other:?}"),
        }
    }
}

/// Two reference tables that reference each other, rows in both: the differ
/// plans them in name order and does not refuse the cycle
/// (`data::supply_order`), and the resolver, which settles the same plan, does
/// not refuse it either and keeps the differ's order (DEC-1968.2).
#[test]
fn two_reference_tables_referencing_each_other_plan_and_settle_in_the_differs_order() {
    use pbps_model::{
        Column, DataMode, ForeignKey, Hints, IdsFile, PrimaryKey, Row, RowKey, Schema, Table,
        TableData, Value,
    };
    let reference = |to: &str, column: &str| ForeignKey {
        columns: vec![column.into()],
        references_table: to.parse().unwrap(),
        references_columns: vec!["id".into()],
        on_delete: Default::default(),
        on_update: Default::default(),
    };
    let table = |other: &str, column: &str, name: &str| {
        let mut table = Table::default();
        table
            .columns
            .insert("id".into(), Column::new("int".parse().unwrap()).not_null());
        table
            .columns
            .insert(column.into(), Column::new("int".parse().unwrap()));
        table.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        });
        table
            .foreign_keys
            .insert(format!("fk_{name}"), reference(other, column));
        table.data = Some(TableData {
            mode: DataMode::Exact,
            rows: [(
                RowKey::from("1"),
                [(column.into(), Value::Int(1))]
                    .into_iter()
                    .collect::<Row>(),
            )]
            .into_iter()
            .collect(),
        });
        table
    };
    let mut desired = Schema::default();
    desired
        .tables
        .insert("app.a".parse().unwrap(), table("app.b", "b_id", "a"));
    desired
        .tables
        .insert("app.b".parse().unwrap(), table("app.a", "a_id", "b"));
    // The tables and their keys stand; the plan inserts the rows.
    let mut base = desired.clone();
    for table in base.tables.values_mut() {
        table.data = None;
    }
    let wanted_ids = ids(&desired, &IdsFile::default());
    let (base_side, desired_side) = (
        crate::Side {
            schema: &base,
            ids: &wanted_ids,
        },
        crate::Side {
            schema: &desired,
            ids: &wanted_ids,
        },
    );
    let differ = crate::diff(
        base_side,
        desired_side,
        &pbps_dialect::MinimalDialect,
        &Hints::default(),
    )
    .unwrap();
    assert!(
        differ
            .changes
            .iter()
            .filter(|p| matches!(p.change, Change::InsertRow { .. }))
            .count()
            == 2,
        "{differ:?}"
    );
    let ordered = plan(
        base_side,
        desired_side,
        &Hints::default(),
        &[],
        &pbps_dialect::MinimalDialect,
    )
    .unwrap_or_else(|error| panic!("a cycle of reference tables is refused: {error:?}"));
    assert_eq!(ordered.changes, differ);
    ordered.proof.validate(&ordered.changes).unwrap();
}

#[test]
fn preparing_generated_creation_parts_preserves_their_creation_risks() {
    use pbps_model::{Column, Generated, IdsFile, Schema, Table, TableName, ValueSource};
    let name = TableName::new("app", "a");
    let mut table = Table::default();
    table
        .columns
        .insert("id".into(), Column::new("integer".parse().unwrap()));
    let mut g = Column::new("integer".parse().unwrap());
    g.value_source = ValueSource::Generated(Generated {
        expression: "id * 2".into(),
        stored: true,
    });
    table.columns.insert("g".into(), g);
    let z = Column::new("integer".parse().unwrap()).not_null();
    table.columns.insert("z".into(), z.clone());
    table.checks.insert(
        "positive".into(),
        pbps_model::CheckConstraint {
            expression: "g > 0".into(),
        },
    );
    let desired = Schema {
        tables: [(name.clone(), table.clone())].into(),
        ..Default::default()
    };
    let identities = ids(&desired, &IdsFile::default());
    table.columns.shift_remove("z");
    if let ValueSource::Generated(g) = &mut table.columns["g"].value_source {
        g.expression = "NULL::integer".into();
    }
    let mut ordinary = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::CreateTable {
                uid: identities.table_uid(&name).unwrap().clone(),
                name: name.clone(),
                table: Box::new(table),
                beside_default: None,
                moves_at_most: None,
            }),
            PlannedChange::new(Change::AddColumn {
                uid: identities.column_uid(&name.column("z")).unwrap().clone(),
                table: name.clone(),
                name: "z".into(),
                column: Box::new(z),
            }),
            PlannedChange::new(Change::AlterColumnExpression {
                uid: identities.column_uid(&name.column("g")).unwrap().clone(),
                column: name.column("g"),
                from: "NULL::integer".into(),
                to: "id * 2".into(),
            }),
        ],
    };
    for part in &mut ordinary.changes {
        part.risks.clear();
        part.provenance = Some(pbps_model::Provenance {
            rule: crate::order::generated::RULE,
            role: pbps_model::AddedRole::SplitPart,
            source: "app.a".into(),
            reason: "typed creation part".into(),
        });
    }
    let prepare = |cs: &ChangeSet| {
        super::prepare::changes(
            cs,
            &Schema::default(),
            &desired,
            &identities,
            &IdsFile::default(),
            &BTreeSet::new(),
            &Default::default(),
            &pbps_dialect::MinimalDialect,
        )
        .unwrap()
    };
    let prepared = prepare(&ordinary);
    assert!(
        prepared
            .changes
            .changes
            .iter()
            .any(|p| matches!(p.change, Change::AddCheck { .. }))
    );
    assert!(
        prepared.changes.gated_risks().is_empty(),
        "{:?}",
        prepared.changes
    );
    let mut late = ordinary.clone();
    let column = late.changes.remove(1);
    late.changes.push(PlannedChange::new(Change::InsertRow {
        table: name.clone(),
        key_column: "id".into(),
        identity_key: false,
        key: "1".into(),
        row: Default::default(),
        defaults: Default::default(),
        types: Default::default(),
    }));
    late.changes.push(column);
    let prepared_late = prepare(&late);
    assert!(
        prepared_late.changes.changes.iter().any(|p| {
            matches!(&p.change, Change::AddColumn { name, .. } if name.raw() == "z")
                && p.risks.contains(&pbps_model::RiskClass::NotNull)
        }),
        "a lowering cannot borrow the earlier installation's empty context"
    );
    let mut standing = prepared.changes;
    standing
        .changes
        .retain(|p| !matches!(p.change, Change::CreateTable { .. }));
    let unmarked = prepare(&standing);
    assert!(
        unmarked
            .changes
            .gated_risks()
            .contains(&pbps_model::RiskClass::Narrowing)
    );
    assert!(
        unmarked
            .changes
            .gated_risks()
            .contains(&pbps_model::RiskClass::NotNull)
    );
    assert!(
        unmarked
            .changes
            .gated_risks()
            .contains(&pbps_model::RiskClass::Constraint)
    );
}

#[test]
fn placeholder_creates_do_not_claim_the_installed_expressions_bindings() {
    use pbps_model::operation::{Origin, PlanOwners};
    use pbps_model::resolver::{BoundSurface, ObjectIdentity};
    use pbps_model::{
        Column, Generated, IdsFile, Module, ModuleKind, Schema, Table, TableName, ValueSource,
    };
    let name = TableName::new("app", "a");
    let column = name.column("g");
    let function: pbps_model::ModuleId = "app.f(integer)".parse().unwrap();
    let mut spec = Column::new("integer".parse().unwrap());
    spec.value_source = ValueSource::Generated(Generated {
        expression: "app.f(1)".into(),
        stored: true,
    });
    let mut table = Table::default();
    table.columns.insert("g".into(), spec);
    let module = Module {
        kind: ModuleKind::Function,
        description: None,
        definition: "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1".into(),
    };
    let desired = Schema {
        tables: [(name.clone(), table.clone())].into(),
        modules: [(function.clone(), module.clone())].into(),
        ..Default::default()
    };
    let before_ids = IdsFile::default();
    let ids = ids(&desired, &before_ids);
    let base = Schema::default();
    let observation = SurfaceResolution {
        surface: Surface::Default(column.clone()),
        current: None,
        desired: Some(BoundSurface {
            object: ObjectIdentity {
                class: "pg_attrdef".into(),
                name: vec!["app".into(), "a".into(), "g".into()],
                signature: Vec::new(),
            },
            bindings: Vec::new(),
            managed_inputs: [Surface::Module(function.clone())].into(),
        }),
    };
    for placeholder in [true, false] {
        let from = if placeholder {
            "NULL::integer"
        } else {
            "app.f(1)"
        };
        let mut created = table.clone();
        if let ValueSource::Generated(g) = &mut created.columns["g"].value_source {
            g.expression = from.into();
        }
        let changes = ChangeSet {
            changes: vec![
                PlannedChange::new(Change::CreateTable {
                    uid: ids.table_uid(&name).unwrap().clone(),
                    name: name.clone(),
                    table: Box::new(created),
                    beside_default: None,
                    moves_at_most: None,
                }),
                PlannedChange::new(Change::CreateModule {
                    id: function.clone(),
                    module: Box::new(module.clone()),
                }),
                PlannedChange::new(Change::AlterColumnExpression {
                    uid: ids.column_uid(&column).unwrap().clone(),
                    column: column.clone(),
                    from: from.into(),
                    to: "app.f(1)".into(),
                }),
            ],
        };
        let owners = PlanOwners::new(
            pbps_model::plan_names::PlanNames::of(&changes.changes, &ids).unwrap(),
            &before_ids,
            &ids,
        );
        for same_source in [true, false] {
            let origins = [
                Origin::SplitPart(0),
                Origin::Whole(1),
                Origin::SplitPart(if same_source { 0 } else { 2 }),
            ];
            let edges = super::graph::constraints(
                &changes,
                &origins,
                &owners,
                std::slice::from_ref(&observation),
                crate::Side {
                    schema: &base,
                    ids: &before_ids,
                },
                crate::Side {
                    schema: &desired,
                    ids: &ids,
                },
            );
            assert!(edges.iter().any(|e| e.before == 1 && e.after == 2));
            assert_eq!(
                edges.iter().any(|e| e.before == 1 && e.after == 0),
                !(placeholder && same_source)
            );
        }
    }
}
