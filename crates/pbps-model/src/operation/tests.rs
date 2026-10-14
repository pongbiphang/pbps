use super::*;
use crate::{CheckConstraint, Column, Index, IndexColumn, IndexKey, Module, ModuleKind, Table};
use crate::{UidKind, ValueSource};

fn uid(kind: UidKind, name: &str) -> Uid {
    Uid::derived(kind, name, 0)
}
fn t(name: &str) -> TableName {
    TableName::new("app", name)
}
fn table(name: &str) -> Uid {
    uid(UidKind::Table, name)
}
fn column(name: &str) -> Uid {
    uid(UidKind::Column, name)
}
fn p(change: Change) -> PlannedChange {
    PlannedChange::new(change)
}
fn ids(tables: &[(&Uid, TableName)], columns: &[(&Uid, ColumnRef)]) -> IdsFile {
    IdsFile {
        tables: tables
            .iter()
            .map(|(u, n)| ((*u).clone(), n.clone()))
            .collect(),
        columns: columns
            .iter()
            .map(|(u, n)| ((*u).clone(), n.clone()))
            .collect(),
        ..IdsFile::default()
    }
}
fn rename(uid: &Uid, from: TableName, to: TableName) -> PlannedChange {
    p(Change::RenameTable {
        uid: uid.clone(),
        from,
        to,
        defaults: vec![],
    })
}
fn drop_check(on: TableName) -> Change {
    Change::DropCheck {
        table: on,
        name: "k".into(),
    }
}
fn add_check(on: TableName) -> Change {
    Change::AddCheck {
        table: on,
        name: "k".into(),
        constraint: CheckConstraint {
            expression: "id > 0".into(),
        },
    }
}
fn check_of(owner: &Uid) -> ObjectKey {
    ObjectKey::Part {
        table: Recorded(owner.clone()),
        part: PartKey::Check("k".into()),
    }
}
fn function() -> (ModuleId, Module) {
    (
        "app.f(integer)".parse().unwrap(),
        Module {
            kind: ModuleKind::Function,
            definition: "CREATE FUNCTION app.f(a integer) RETURNS integer AS 'SELECT a' \
                         LANGUAGE sql;"
                .into(),
            description: None,
        },
    )
}

/// The chain the resolver's own test pins (DEC-1498.1): `app.b` moves to
/// `app.c`, then `app.a` takes `app.b`. Two tables answer to `app.b` in one
/// plan, and only the step's spelling says which.
struct Chain {
    a: Uid,
    b: Uid,
    logical: Vec<PlannedChange>,
    base: IdsFile,
    desired: IdsFile,
}

fn chain() -> Chain {
    let (a, b) = (table("a"), table("b"));
    Chain {
        logical: vec![
            rename(&b, t("b"), t("c")),
            rename(&a, t("a"), t("b")),
            // The ordinary differ's own drop of a check on the table now
            // called app.b: the old app.a.
            p(drop_check(t("b"))),
        ],
        base: ids(&[(&a, t("a")), (&b, t("b"))], &[]),
        desired: ids(&[(&a, t("b")), (&b, t("c"))], &[]),
        a,
        b,
    }
}

#[test]
fn a_teardown_and_its_restoration_share_a_key_across_the_rename_their_names_straddle() {
    let c = chain();
    let names = PlanNames::of(&c.logical, &c.desired).unwrap();
    let owners = PlanOwners::new(names, &c.base, &c.desired);
    let source = Source::Rebinding(Surface::Check {
        table: t("c"),
        name: "k".into(),
    });
    let ops = Operations::expanded(
        c.logical.len(),
        c.logical
            .iter()
            .enumerate()
            .map(|(i, step)| (Origin::Whole(i), step.clone()))
            .chain([
                (Origin::Teardown(source.clone()), p(drop_check(t("b")))),
                (Origin::Restoration(source), p(add_check(t("c")))),
            ]),
        ModuleEdits::Rebuilt,
    );
    let key = |i: usize| ops.as_slice()[i].key(&owners).unwrap();
    // Spelled `app.b` by its base name and `app.c` by its final one: the
    // same table, so the same check.
    assert_eq!(key(3), check_of(&c.b));
    assert_eq!(key(4), check_of(&c.b));
    // The differ's drop, spelled `app.b` after both renames, is the other
    // table's check. A key made of the spelled name would have merged them.
    assert_eq!(key(2), check_of(&c.a));
    assert_ne!(key(2), key(3));
}

#[test]
fn a_name_the_models_do_not_record_is_an_error_and_never_a_key() {
    let c = chain();
    let owners = PlanOwners::new(
        PlanNames::of(&c.logical, &c.desired).unwrap(),
        &c.base,
        &c.desired,
    );
    let unmanaged = Operations::of_plan(&[p(drop_check(t("nobody")))], ModuleEdits::InPlace);
    assert!(matches!(
        unmanaged.as_slice()[0].key(&owners),
        Err(OwnerError::Unmanaged { .. })
    ));
    let torn_down = Operations::expanded(
        1,
        [(
            Origin::Teardown(Source::Change(0)),
            p(drop_check(t("nobody"))),
        )],
        ModuleEdits::InPlace,
    );
    assert!(matches!(
        torn_down.as_slice()[0].key(&owners),
        Err(OwnerError::Unmanaged { .. })
    ));
    // A position past the indexed plan is refused, not read at its end.
    let outside = Operations::expanded(
        8,
        [(Origin::Whole(7), p(drop_check(t("b"))))],
        ModuleEdits::InPlace,
    );
    assert!(matches!(
        outside.as_slice()[0].key(&owners),
        Err(OwnerError::Outside { .. })
    ));
    // `app.c` is held only after the first rename: before it, the name is a
    // known address nobody holds yet, which is not "unmanaged".
    let early = Operations::of_plan(&[p(drop_check(t("c")))], ModuleEdits::InPlace);
    assert!(matches!(
        early.as_slice()[0].key(&owners),
        Err(OwnerError::Vacant { .. })
    ));
}

/// Every object the model gives no uid of its own is keyed by its owner and
/// name, never by a uid of its own: there is no constructor to make one up
/// with (the `Recorded` doctest), and the key a change gets is decided here.
#[test]
fn keyless_objects_are_keyed_by_their_owner_and_name() {
    let owner = table("t");
    let base = ids(&[(&owner, t("t"))], &[]);
    let (id, module) = function();
    let keyless = [
        add_check(t("t")),
        Change::DropIndex {
            table: t("t"),
            name: "ix".into(),
        },
        Change::DropUnique {
            table: t("t"),
            name: "uq".into(),
        },
        Change::DropForeignKey {
            table: t("t"),
            name: "fk".into(),
        },
        Change::SetPrimaryKey {
            table: t("t"),
            from: None,
            to: None,
            nonclustered: false,
        },
        Change::SetIndexStorageParameters {
            table: t("t"),
            target: IndexPart::Index("ix".into()),
            method: Default::default(),
            set: Default::default(),
            reset: Default::default(),
        },
        Change::DeleteRow {
            table: t("t"),
            key_column: "id".into(),
            key: RowKey("1".into()),
            cause: crate::change::DeleteCause::Undeclared,
            row: Default::default(),
            dropped: Default::default(),
            types: Default::default(),
            after_types: Default::default(),
            key_type: None,
        },
        Change::CreateModule {
            id: id.clone(),
            module: Box::new(module),
        },
        Change::Grant {
            role: "reader".into(),
            target: GrantTarget::Object(t("t")),
            permissions: Default::default(),
        },
        Change::PublicExecution {
            routine: match &id {
                ModuleId::Routine(r) => r.clone(),
                ModuleId::Named(_) | ModuleId::Trigger { .. } => unreachable!(),
            },
            access: crate::PublicAccess::Revoked,
            origin: crate::RoutineOrigin::Created,
        },
        Change::SetPartitionDefault {
            uid: owner.clone(),
            table: t("t"),
            parent: t("parent"),
            column: "c".into(),
            from: None,
            to: Some("1".into()),
            fallback: None,
        },
    ];
    let logical: Vec<PlannedChange> = keyless.iter().cloned().map(p).collect();
    let owners = PlanOwners::new(PlanNames::of(&logical, &base).unwrap(), &base, &base);
    let ops = Operations::of_plan(&logical, ModuleEdits::InPlace);
    for op in &ops {
        let key = op.key(&owners).unwrap();
        // None of these is a table, column or role of its own; where a uid
        // appears it is the owning table's, which the identity file records.
        match &key {
            ObjectKey::Part { table, .. } | ObjectKey::Row { table, .. } => {
                assert_eq!(table.uid(), &owner, "{:?}", op.change());
            }
            ObjectKey::Default(ColumnKey::Partition { table, column }) => {
                assert_eq!((table.uid(), column.as_str()), (&owner, "c"));
            }
            ObjectKey::Module(m) => assert_eq!(m, &id),
            ObjectKey::Grant { .. } => {}
            ObjectKey::Table(_)
            | ObjectKey::Column(_)
            | ObjectKey::Role(_)
            | ObjectKey::NotNull(_)
            | ObjectKey::Default(ColumnKey::Recorded(_)) => {
                panic!("{:?} keyed by a uid of its own: {key:?}", op.change())
            }
        }
    }
    // PUBLIC is a grantee of its own, not a role that happens to be called
    // `PUBLIC`: a declared role of that name is another key.
    let public = ops.as_slice()[9].key(&owners).unwrap();
    let named_public = Operations::of_plan(
        &[p(Change::Grant {
            role: "PUBLIC".into(),
            target: public_target(&public),
            permissions: Default::default(),
        })],
        ModuleEdits::InPlace,
    );
    assert_ne!(
        named_public.as_slice()[0].key(&owners).unwrap(),
        public,
        "a role named PUBLIC is not the PUBLIC grantee"
    );
}

fn public_target(key: &ObjectKey) -> GrantTarget {
    let ObjectKey::Grant { target, .. } = key else {
        unreachable!()
    };
    target.clone()
}

#[test]
fn operation_ids_follow_the_origins_alone() {
    let source = Source::Rebinding(Surface::Check {
        table: t("c"),
        name: "k".into(),
    });
    let steps = || {
        vec![
            (Origin::SplitPart(0), p(drop_check(t("x")))),
            (Origin::SplitPart(0), p(add_check(t("x")))),
            (Origin::Whole(1), p(add_check(t("y")))),
            (Origin::Teardown(source.clone()), p(drop_check(t("b")))),
            (Origin::Restoration(source.clone()), p(add_check(t("c")))),
        ]
    };
    let once = Operations::expanded(2, steps(), ModuleEdits::InPlace);
    let again = Operations::expanded(2, steps(), ModuleEdits::InPlace);
    let ids: Vec<String> = once.iter().map(|o| o.id().to_string()).collect();
    assert_eq!(ids, ["0.0", "0.1", "1.0", "2.0", "2.1"]);
    assert_eq!(
        ids,
        again.iter().map(|o| o.id().to_string()).collect::<Vec<_>>()
    );
    // Reordering keeps each operation's id: a scheduler moves operations,
    // it does not renumber them.
    let moved = once.reordered(&[4, 2, 0, 3, 1]);
    assert_eq!(
        moved.iter().map(|o| o.id().to_string()).collect::<Vec<_>>(),
        ["2.1", "1.0", "0.0", "2.0", "0.1"]
    );
}

#[test]
#[should_panic(expected = "not a permutation")]
fn reordering_refuses_a_sequence_that_drops_an_operation() {
    let ops = Operations::of_plan(
        &[p(add_check(t("x"))), p(add_check(t("y")))],
        ModuleEdits::InPlace,
    );
    ops.reordered(&[0, 0]);
}

/// The step-local rule: what a step defines is read from its payload, so a
/// bare table defines no part its split-off steps add, and a drop says
/// nothing of what goes with it.
#[test]
fn a_step_defines_only_what_its_own_payload_holds() {
    let owner = table("t");
    let id = column("id");
    let after = ids(&[(&owner, t("t"))], &[(&id, t("t").column("id"))]);
    let mut full = Table::default();
    let mut c = Column::new("int".parse().unwrap());
    c.value_source = ValueSource::Default("1".into());
    full.columns.insert("id".into(), c);
    full.checks.insert(
        "k".into(),
        CheckConstraint {
            expression: "id > 0".into(),
        },
    );
    full.indexes.insert(
        "ix".into(),
        Index {
            columns: vec![IndexColumn {
                key: IndexKey::Column("id".into()),
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
    let mut bare = full.clone();
    bare.checks.clear();
    bare.indexes.clear();
    bare.columns.get_mut("id").unwrap().value_source = ValueSource::None;
    let create = |table: Table| {
        p(Change::CreateTable {
            uid: owner.clone(),
            name: t("t"),
            table: Box::new(table),
            beside_default: None,
            moves_at_most: None,
        })
    };
    let defined_against = |logical: &[PlannedChange], after: &IdsFile| {
        let owners = PlanOwners::new(PlanNames::of(logical, after).unwrap(), after, after);
        let ops = Operations::of_plan(logical, ModuleEdits::InPlace);
        ops.as_slice()[0]
            .defines(&owners)
            .unwrap()
            .into_iter()
            .map(|d| d.key)
            .collect::<Vec<_>>()
    };
    let defined = |logical: &[PlannedChange]| defined_against(logical, &after);
    let column_key = ColumnKey::Recorded(Recorded(id.clone()));
    let part = |part| ObjectKey::Part {
        table: Recorded(owner.clone()),
        part,
    };
    assert_eq!(
        defined(&[create(bare)]),
        [
            ObjectKey::Table(Recorded(owner.clone())),
            ObjectKey::Column(column_key.clone()),
        ]
    );
    assert_eq!(
        defined(&[create(full)]),
        [
            ObjectKey::Table(Recorded(owner.clone())),
            ObjectKey::Column(column_key.clone()),
            ObjectKey::Default(column_key),
            part(PartKey::Check("k".into())),
            part(PartKey::Index("ix".into())),
        ]
    );
    let dropped = defined_against(
        &[p(Change::DropTable {
            uid: owner.clone(),
            name: t("t"),
            detach_from: None,
        })],
        &IdsFile::default(),
    );
    assert_eq!(dropped, [ObjectKey::Table(Recorded(owner.clone()))]);
}

/// A module edit is a new instance only where the engine drops and creates
/// it; `CREATE OR ALTER` keeps the object and rewrites it.
#[test]
fn a_module_edit_is_a_new_instance_only_where_the_engine_rebuilds_it() {
    let (id, module) = function();
    let logical = [p(Change::AlterModule {
        id: id.clone(),
        module: Box::new(module),
    })];
    let none = IdsFile::default();
    let owners = PlanOwners::new(PlanNames::of(&logical, &none).unwrap(), &none, &none);
    let transitions = |modules| {
        Operations::of_plan(&logical, modules).as_slice()[0]
            .defines(&owners)
            .unwrap()
            .into_iter()
            .map(|d| (d.key, d.transition))
            .collect::<Vec<_>>()
    };
    let key = ObjectKey::Module(id);
    assert_eq!(
        transitions(ModuleEdits::Rebuilt),
        [(key.clone(), Transition::Ends), (key, Transition::Produces)]
    );
    assert_eq!(transitions(ModuleEdits::InPlace), []);
}

/// A rebuilt function with a view over it, as the connected plan weaves it:
/// the view torn down, the function dropped and created, the view restored.
fn rebuild() -> (Operations, ObjectKey, ObjectKey) {
    let (f, function) = function();
    let v: ModuleId = "app.v".parse().unwrap();
    let view = Module {
        kind: ModuleKind::View,
        definition: "CREATE VIEW app.v AS SELECT app.f(1) AS x;".into(),
        description: None,
    };
    let root = Source::Change(0);
    let ops = Operations::expanded(
        1,
        [
            (
                Origin::Teardown(root.clone()),
                p(Change::DropModule {
                    id: v.clone(),
                    kind: ModuleKind::View,
                }),
            ),
            (
                Origin::SplitPart(0),
                p(Change::DropModule {
                    id: f.clone(),
                    kind: ModuleKind::Function,
                }),
            ),
            (
                Origin::SplitPart(0),
                p(Change::CreateModule {
                    id: f.clone(),
                    module: Box::new(function),
                }),
            ),
            (
                Origin::Restoration(root),
                p(Change::CreateModule {
                    id: v.clone(),
                    module: Box::new(view),
                }),
            ),
        ],
        ModuleEdits::Rebuilt,
    );
    (ops, ObjectKey::Module(f), ObjectKey::Module(v))
}

#[test]
fn a_teardown_ends_the_base_and_its_restoration_produces_the_final_instance() {
    let (ops, f, v) = rebuild();
    let none = IdsFile::default();
    let logical = [p(Change::DropModule {
        id: "app.f(integer)".parse().unwrap(),
        kind: ModuleKind::Function,
    })];
    let owners = PlanOwners::new(PlanNames::of(&logical, &none).unwrap(), &none, &none);
    let roles: Vec<(Role, Vec<Incarnation>)> = ops
        .iter()
        .map(|op| {
            (
                op.role(),
                op.defines(&owners)
                    .unwrap()
                    .iter()
                    .map(|d| d.transition.incarnation())
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        roles,
        [
            (Role::Teardown, vec![Incarnation::Base]),
            (Role::SplitPart, vec![Incarnation::Base]),
            (Role::SplitPart, vec![Incarnation::Final]),
            (Role::Restoration, vec![Incarnation::Final]),
        ]
    );
    let replay = Incarnations::replay(ops.as_slice(), &owners).unwrap();
    let in_force = |key: &ObjectKey| (0..=4).map(|i| replay.in_force(key, i)).collect::<Vec<_>>();
    // The view: the baseline's until its teardown has run, nothing until
    // its restoration has, the plan's after it, for everything that follows.
    assert_eq!(
        in_force(&v),
        [
            InForce::Base,
            InForce::Absent,
            InForce::Absent,
            InForce::Absent,
            InForce::Final
        ]
    );
    // The restoration is created over the plan's function, never the
    // baseline's: at its position the function in force is the final one.
    assert_eq!(replay.in_force(&f, 3), InForce::Final);
    assert_eq!(replay.in_force(&f, 1), InForce::Base);
    // At no boundary are both instances of one key in force.
    assert_eq!(
        replay.in_force(&ObjectKey::Module("app.other".parse().unwrap()), 2),
        InForce::Untouched
    );
}

#[test]
fn a_replay_refuses_a_key_read_as_a_third_instance() {
    let (ops, f, v) = rebuild();
    let none = IdsFile::default();
    let logical = [p(Change::DropModule {
        id: "app.f(integer)".parse().unwrap(),
        kind: ModuleKind::Function,
    })];
    let owners = PlanOwners::new(PlanNames::of(&logical, &none).unwrap(), &none, &none);
    let third =
        |order: &[usize]| match Incarnations::replay(ops.reordered(order).as_slice(), &owners) {
            Err(ReplayError::Third { key, .. }) => *key,
            other => panic!("{order:?} replayed: {other:?}"),
        };
    // Restored before it is torn down: the teardown would end the plan's
    // own view, not the baseline's.
    assert_eq!(third(&[3, 1, 2, 0]), v);
    // The function created before its drop: the drop ends the new one.
    assert_eq!(third(&[0, 2, 1, 3]), f);
    // Twice restored is a third instance too.
    let twice = Operations::expanded(
        1,
        ops.iter()
            .map(|op| (op.origin().clone(), op.planned().clone()))
            .chain(std::iter::once((
                ops.as_slice()[3].origin().clone(),
                ops.as_slice()[3].planned().clone(),
            ))),
        ModuleEdits::Rebuilt,
    );
    assert!(matches!(
        Incarnations::replay(twice.as_slice(), &owners),
        Err(ReplayError::Third { key, .. }) if *key == v
    ));
    // The order the plan runs in is accepted.
    Incarnations::replay(ops.as_slice(), &owners).unwrap();
}
