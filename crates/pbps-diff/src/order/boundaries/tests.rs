use super::*;
use crate::order::{PlanningPlan, Schedule, plan};
use pbps_model::operation::{
    ModuleEdits, ObjectKey, Operations, PlanOwners,
    symbolic::{Boundary as TransferBoundary, Move},
};
use pbps_model::{PlannedChange, TableName};

fn fixture() -> (PlanningPlan, Operations, Vec<Effects>, State) {
    let parent: Uid = "t_aaaaaa".parse().unwrap();
    let child: Uid = "t_bbbbbb".parse().unwrap();
    let parent_name: TableName = "app.p".parse().unwrap();
    let mut ids = pbps_model::IdsFile::default();
    ids.tables.insert(parent.clone(), parent_name.clone());
    ids.tables.insert(child.clone(), "app.a".parse().unwrap());
    ids.roles
        .insert("r_aaaaaa".parse().unwrap(), "unused".into());
    let changes = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::AttachPartition {
                uid: child.clone(),
                table: "app.a".parse().unwrap(),
                parent: parent_name,
                bound: pbps_model::PartitionBound::Range {
                    from: vec![pbps_model::BoundDatum::MinValue],
                    to: vec![pbps_model::BoundDatum::MaxValue],
                },
                key: Default::default(),
                shape: Box::default(),
            }),
            PlannedChange::new(Change::CreateRole {
                uid: "r_aaaaaa".parse().unwrap(),
                name: "unused".into(),
            }),
        ],
    };
    let names = pbps_model::plan_names::PlanNames::of(&changes.changes, &ids).unwrap();
    let owners = PlanOwners::new(names, &ids, &ids);
    let operation = |uid, name| {
        Operations::of_plan(
            &[PlannedChange::new(Change::CreateTable {
                uid,
                name,
                table: Box::default(),
                beside_default: None,
                moves_at_most: None,
            })],
            ModuleEdits::InPlace,
        )
    };
    let parent_op = operation(parent.clone(), "app.p".parse().unwrap());
    let child_op = operation(child.clone(), "app.a".parse().unwrap());
    let ObjectKey::Table(p) = parent_op.as_slice()[0].key(&owners).unwrap() else {
        panic!()
    };
    let ObjectKey::Table(c) = child_op.as_slice()[0].key(&owners).unwrap() else {
        panic!()
    };
    let base = State {
        tables: [p.clone(), c.clone()].into_iter().collect(),
        ..Default::default()
    };
    let fx = vec![
        Effects {
            symbolic: vec![Clause::Move(Move {
                parent: p,
                child: c,
                kind: MoveKind::Attach,
                consumes: None,
                boundary: TransferBoundary::All(vec![]),
            })],
            ..Default::default()
        },
        Effects::default(),
    ];
    let ops = Operations::of_plan(&changes.changes, ModuleEdits::InPlace);
    let mut proposal = super::super::propose(changes);
    proposal.pending_boundaries([Boundary { parent, child }], ModuleEdits::InPlace);
    (proposal, ops, fx, base)
}
#[test]
fn pending_boundaries_survive_settlement_and_cannot_become_canonical() {
    let (p, ops, fx, base) = fixture();
    let settled = Schedule::new(p.changes.len()).settle(p).unwrap().changes;
    let intermediate: PlanningPlan<super::super::Differ> = plan::state(settled);
    let mut p: PlanningPlan = plan::state(intermediate);
    assert!(
        plan::canonical(p.clone(), &pbps_dialect::MinimalDialect)
            .unwrap_err()
            .contains("pending")
    );
    p.boundaries.discharge(ops.as_slice(), &fx, &base).unwrap();
    assert!(plan::canonical(p, &pbps_dialect::MinimalDialect).is_ok());
}
#[test]
fn a_changed_boundary_order_requires_replay_again() {
    let (mut p, ops, fx, base) = fixture();
    p.boundaries.discharge(ops.as_slice(), &fx, &base).unwrap();
    let mut schedule = Schedule::new(2);
    schedule.edge(
        1,
        0,
        crate::order::Source::Effect,
        pbps_model::resolver::OrderReason::Structural,
    );
    let mut settled = schedule.settle(p).unwrap();
    assert!(settled.changes.boundaries.ensure(&settled.changes).is_err());
    let ops = crate::order::reordered_operations(&ops, &settled.sequence);
    let fx: Vec<_> = settled.sequence.iter().map(|&i| fx[i].clone()).collect();
    settled
        .changes
        .boundaries
        .discharge(ops.as_slice(), &fx, &base)
        .unwrap();
    assert!(plan::canonical(settled.changes, &pbps_dialect::MinimalDialect).is_ok());
}
#[test]
fn an_empty_effect_cannot_discharge_a_missing_transfer() {
    let (mut p, ops, _, base) = fixture();
    let why = p
        .boundaries
        .discharge(
            ops.as_slice(),
            &[Effects::default(), Effects::default()],
            &base,
        )
        .unwrap_err();
    assert!(why.contains("no symbolic transfer") && why.contains(membership::REMEDY));
    assert!(plan::canonical(p, &pbps_dialect::MinimalDialect).is_err());
}

#[test]
fn expansion_and_logical_append_do_not_discard_pending_boundaries() {
    let (p, _, _, _) = fixture();
    let expanded = crate::order::expand::expand(p, &NoWork, 2).unwrap().changes;
    assert!(plan::canonical(expanded.clone(), &pbps_dialect::MinimalDialect).is_err());
    let appended = crate::order::writebacks::PlanningInput::new(vec![]).append_to(expanded);
    assert!(plan::canonical(appended, &pbps_dialect::MinimalDialect).is_err());
}

#[test]
fn boundary_discharge_binds_event_identity_as_well_as_payload() {
    let (mut p, ops, fx, base) = fixture();
    p.boundaries.discharge(ops.as_slice(), &fx, &base).unwrap();
    p.origins.swap(0, 1);
    assert!(plan::canonical(p, &pbps_dialect::MinimalDialect).is_err());
}
#[test]
fn a_detachs_parent_is_its_recorded_uid_even_when_a_rename_reuses_the_name() {
    let p: Uid = "t_aaaaaa".parse().unwrap();
    let other: Uid = "t_cccccc".parse().unwrap();
    let a: Uid = "t_bbbbbb".parse().unwrap();
    let name: TableName = "app.p".parse().unwrap();
    let new_name: TableName = "app.new_p".parse().unwrap();
    let mut before = pbps_model::IdsFile::default();
    before.tables.insert(p.clone(), name.clone());
    let mut after = pbps_model::IdsFile::default();
    after.tables.insert(p.clone(), new_name.clone());
    after.tables.insert(other, name.clone());
    let changes = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::RenameTable {
                uid: p.clone(),
                from: name.clone(),
                to: new_name,
                defaults: vec![],
            }),
            PlannedChange::new(Change::DetachPartition {
                uid: a.clone(),
                table: "app.a".parse().unwrap(),
                parent: name,
                names: vec![],
                shape: Box::default(),
            }),
        ],
    };
    let schema = pbps_model::Schema::default();
    let result = meetings(
        crate::Side {
            schema: &schema,
            ids: &before,
        },
        crate::Side {
            schema: &schema,
            ids: &after,
        },
        &changes,
    );
    assert_eq!(
        result,
        [Boundary {
            parent: p,
            child: a
        }]
        .into_iter()
        .collect()
    );
}

use crate::order::expand::{Expansion, Order, Rewrite};
struct NoWork;
impl Expansion for NoWork {
    type Obligation = String;
    fn rule(&self) -> &'static str {
        "no-work"
    }
    fn unmet(&self, _: &[PlannedChange]) -> Result<Vec<String>, String> {
        Ok(vec![])
    }
    fn expand(&self, _: &[PlannedChange], _: &String) -> Result<Rewrite, String> {
        unreachable!()
    }
    fn order(&self, _: &[PlannedChange]) -> Result<Vec<Order>, String> {
        Ok(vec![])
    }
}

#[test]
fn a_noop_expansion_retains_already_settled_split_event_ids() {
    use pbps_model::operation::{Origin, Source};
    let (mut p, _, fx, base) = fixture();
    p.logical = 1;
    p.origins = vec![
        Origin::Teardown(Source::Change(0)),
        Origin::Restoration(Source::Change(0)),
    ];
    let ops = Operations::expanded(
        1,
        p.origins.iter().cloned().zip(p.changes.iter().cloned()),
        ModuleEdits::InPlace,
    );
    p.identities = Some(ops.clone());
    let mut schedule = Schedule::new(2);
    schedule.edge(
        1,
        0,
        crate::order::Source::Effect,
        pbps_model::resolver::OrderReason::Structural,
    );
    let mut settled = schedule.settle(p).unwrap();
    let ops = crate::order::reordered_operations(&ops, &settled.sequence);
    let fx: Vec<_> = settled.sequence.iter().map(|&i| fx[i].clone()).collect();
    settled
        .changes
        .boundaries
        .discharge(ops.as_slice(), &fx, &base)
        .unwrap();
    assert!(plan::canonical(settled.changes.clone(), &pbps_dialect::MinimalDialect).is_ok());
    let result = crate::order::expand::expand(settled.changes, &NoWork, 2)
        .unwrap()
        .changes;
    assert!(plan::canonical(result, &pbps_dialect::MinimalDialect).is_ok());
}

#[test]
fn appending_a_step_retains_boundary_event_ids_and_requires_new_discharge() {
    let (mut p, ops, fx, base) = fixture();
    p.boundaries.discharge(ops.as_slice(), &fx, &base).unwrap();
    let addition = PlannedChange::new(Change::CreateRole {
        uid: "r_bbbbbb".parse().unwrap(),
        name: "another".into(),
    });
    let mut p = crate::order::writebacks::PlanningInput::new(vec![addition]).append_to(p);
    assert!(plan::canonical(p.clone(), &pbps_dialect::MinimalDialect).is_err());
    let events = p.identities.clone().unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(&events.as_slice()[..2], ops.as_slice());
    assert!(
        events.as_slice()[..2]
            .iter()
            .all(|op| op.id() != events.as_slice()[2].id())
    );
    let mut fx = fx;
    fx.push(Effects::default());
    p.boundaries
        .discharge(events.as_slice(), &fx, &base)
        .unwrap();
    assert!(plan::canonical(p, &pbps_dialect::MinimalDialect).is_ok());
}

#[test]
fn only_a_parent_whose_columns_change_beside_its_members_is_read() {
    use pbps_model::{Column, IdsFile, PartitionBound, PartitionBy, PartitionOf, Schema, Table};
    let side = |member: bool| {
        let (p, a): (TableName, TableName) = ("app.p".parse().unwrap(), "app.a".parse().unwrap());
        let mut parent = Table {
            partition_by: Some(PartitionBy {
                strategy: Default::default(),
                columns: vec!["d".into()],
                external: false,
            }),
            ..Default::default()
        };
        parent
            .columns
            .insert("d".into(), Column::new("integer".parse().unwrap()));
        let child = Table {
            partition_of: member.then(|| PartitionOf {
                parent: p.clone(),
                bound: PartitionBound::Default,
                columns: Default::default(),
            }),
            ..Default::default()
        };
        let mut ids = IdsFile::default();
        ids.tables.insert("t_aaaaaa".parse().unwrap(), p.clone());
        ids.tables.insert("t_bbbbbb".parse().unwrap(), a.clone());
        ids.columns
            .insert("c_aaaaaa".parse().unwrap(), p.column("d"));
        let mut schema = Schema::default();
        schema.tables.extend([(p, parent), (a, child)]);
        (schema, ids)
    };
    let attach = |column_change: bool| {
        let mut changes = vec![Change::AttachPartition {
            uid: "t_bbbbbb".parse().unwrap(),
            table: "app.a".parse().unwrap(),
            parent: "app.p".parse().unwrap(),
            bound: PartitionBound::Default,
            key: Default::default(),
            shape: Box::default(),
        }];
        if column_change {
            changes.push(Change::AlterColumnDefault {
                uid: "c_aaaaaa".parse().unwrap(),
                column: "app.p.d".parse().unwrap(),
                from: None,
                to: Some("1".into()),
            });
        }
        ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        }
    };
    let ((b, b_ids), (a, a_ids)) = (side(false), side(true));
    let (before, after) = (
        crate::Side {
            schema: &b,
            ids: &b_ids,
        },
        crate::Side {
            schema: &a,
            ids: &a_ids,
        },
    );
    let read = |changes: &ChangeSet, before, after| catalog_tables(changes, before, after);
    let tables: Vec<_> = read(&attach(true), before, after)
        .into_iter()
        .map(|u| u.to_string())
        .collect();
    assert_eq!(tables, ["t_aaaaaa", "t_bbbbbb"]);
    // A move alone, and columns alone, read nothing.
    assert!(read(&attach(false), before, after).is_empty());
    let (still, still_ids) = side(true);
    let held = crate::Side {
        schema: &still,
        ids: &still_ids,
    };
    assert!(read(&attach(true), held, held).is_empty());
}

#[test]
fn version_binding_requires_a_unique_existing_writer_chain() {
    use pbps_model::operation::{Incarnation, Resource, symbolic::*};
    let (_, ops, mut fx, mut state) = fixture();
    let Clause::Move(m) = &fx[0].symbolic[0] else {
        panic!()
    };
    let parent = m.parent.clone();
    let key = Key {
        parent: parent.clone(),
        property: Property::Address,
    };
    let value = |text: &str| {
        Fact::Known(Value {
            datum: Datum::Definition(text.into()),
            bindings: Default::default(),
            ownership: Ownership::Own,
        })
    };
    state.cells.insert(
        (parent.clone(), Property::Address),
        Cell {
            value: value("base"),
            version: Version::Base,
            origin: None,
        },
    );
    for (at, effect) in fx.iter_mut().enumerate() {
        effect.symbolic = vec![Clause::Recurse(RecursiveWrite {
            key: key.clone(),
            input: Version::Base,
            from: value("base"),
            to: value(if at == 0 { "a" } else { "b" }),
            scope: Scope::ParentOnly,
            policy: WritePolicy::PreserveOwnership,
            lifetime: Lifetime::Retain,
        })];
    }
    let baseline = crate::order::Baseline::empty().with_symbolic(state);
    let fork = fx.clone();
    bind(&ops, &mut fx, &baseline);
    assert_eq!(
        fx, fork,
        "incomparable writers must retain their explicit fork"
    );
    for effect in &mut fx {
        effect.rewrites.push(Resource::Object(
            ObjectKey::Table(parent.clone()),
            Incarnation::Base,
        ));
    }
    bind(&ops, &mut fx, &baseline);
    assert!(
        matches!(&fx[1].symbolic[..],[Clause::Recurse(w)] if w.input==Version::After(ops.as_slice()[0].id()) && w.from==value("a"))
    );
}

#[test]
fn logical_name_positions_survive_permutation_and_appended_input() {
    use pbps_model::operation::{Owners, Spelling};
    use pbps_model::plan_names::{At, PlanNames};
    let parent: Uid = "t_aaaaaa".parse().unwrap();
    let child: Uid = "t_bbbbbb".parse().unwrap();
    let mut base = pbps_model::IdsFile::default();
    base.tables.insert(parent.clone(), "app.p".parse().unwrap());
    base.tables.insert(child.clone(), "app.a".parse().unwrap());
    let mut after = base.clone();
    after
        .tables
        .insert(parent.clone(), "app.q".parse().unwrap());
    after
        .roles
        .insert("r_bbbbbb".parse().unwrap(), "newreader".into());
    let input = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::RenameTable {
                uid: parent.clone(),
                from: "app.p".parse().unwrap(),
                to: "app.q".parse().unwrap(),
                defaults: vec![],
            }),
            PlannedChange::new(Change::AttachPartition {
                uid: child.clone(),
                table: "app.a".parse().unwrap(),
                parent: "app.q".parse().unwrap(),
                bound: pbps_model::PartitionBound::Default,
                key: vec![],
                shape: Box::default(),
            }),
        ],
    };
    let mut plan = crate::order::propose(input);
    plan.pending_boundaries(
        [Boundary {
            parent: parent.clone(),
            child,
        }],
        ModuleEdits::InPlace,
    );
    let plan = crate::order::writebacks::PlanningInput::new(vec![PlannedChange::new(
        Change::CreateRole {
            uid: "r_bbbbbb".parse().unwrap(),
            name: "newreader".into(),
        },
    )])
    .append_to(plan);
    let mut schedule = Schedule::new(3);
    schedule.edge(
        2,
        0,
        crate::order::Source::Effect,
        pbps_model::resolver::OrderReason::Structural,
    );
    schedule.edge(
        0,
        1,
        crate::order::Source::Effect,
        pbps_model::resolver::OrderReason::Structural,
    );
    let plan = schedule.settle(plan).unwrap().changes;
    let attach = plan
        .identities
        .as_ref()
        .unwrap()
        .iter()
        .find(|op| matches!(op.change(), Change::AttachPartition { .. }))
        .unwrap();
    let history = plan.boundaries.names(&plan);
    let names = PlanNames::of(&history.changes, &after).unwrap();
    let owners = PlanOwners::new(names, &base, &after);
    assert_eq!(
        owners
            .table(&"app.q".parse().unwrap(), attach.origin().spelling())
            .unwrap(),
        parent
    );
    assert_eq!(
        owners
            .role(&"newreader".into(), Spelling::At(At::After(2)))
            .unwrap(),
        "r_bbbbbb".parse().unwrap()
    );
    let current = PlanOwners::new(PlanNames::of(&plan.changes, &after).unwrap(), &base, &after);
    assert!(
        current
            .table(&"app.q".parse().unwrap(), attach.origin().spelling())
            .is_err()
    );
}

#[test]
fn public_companions_keep_their_events_while_their_steps_are_out_of_the_window() {
    let (p, _, _, _) = fixture();
    let part = PlannedChange::new(Change::PublicExecution {
        routine: "app.f(integer)".parse().unwrap(),
        access: pbps_model::PublicAccess::Kept,
        origin: pbps_model::RoutineOrigin::Rebuilt,
    });
    let mut p = crate::order::writebacks::PlanningInput::new(vec![part]).append_to(p);
    let original = p.identities.clone().unwrap();
    let event = original.as_slice()[2].id();
    let decisions = crate::order::take_public_execution(&mut p);
    assert_eq!(p.changes.len(), 2);
    assert_eq!(p.identities.as_ref().unwrap().len(), 2);
    assert_eq!(
        p.identities.as_ref().unwrap().as_slice()[0].id(),
        original.as_slice()[0].id()
    );
    p = crate::order::writebacks::PlanningInput::new(vec![PlannedChange::new(
        Change::CreateRole {
            uid: "r_bbbbbb".parse().unwrap(),
            name: "later".into(),
        },
    )])
    .append_to(p);
    let added = p.identities.as_ref().unwrap().as_slice()[2].id();
    assert_ne!(added, event, "a held logical source stays reserved");
    crate::order::settle_public_execution(&mut p, decisions);
    let ids = p.identities.as_ref().unwrap().with_steps(&p.changes);
    assert_eq!(ids.as_slice()[1].id(), event);
    assert_eq!(
        ids.as_slice()[1].origin(),
        &pbps_model::operation::Origin::Whole(2)
    );
    assert_eq!(ids.as_slice()[2].id(), original.as_slice()[1].id());
    assert_eq!(ids.as_slice()[3].id(), added);
}

#[test]
fn retuning_a_parent_owned_sequence_does_not_claim_a_tree_alter_lock() {
    for retuning in [false, true] {
        let (mut plan, _, _, _) = fixture();
        let name: TableName = "app.p".parse().unwrap();
        let mut ids = pbps_model::IdsFile::default();
        ids.tables.insert("t_aaaaaa".parse().unwrap(), name.clone());
        ids.columns
            .insert("c_aaaaaa".parse().unwrap(), name.column("d"));
        let old = pbps_model::ValueSource::Sequence(pbps_model::OwnedSequence::serial());
        plan.payload
            .changes
            .push(PlannedChange::new(Change::AlterColumnNumbering {
                uid: "c_aaaaaa".parse().unwrap(),
                column: name.column("d"),
                ty: "integer".parse().unwrap(),
                from: old.clone(),
                to: if retuning {
                    old
                } else {
                    pbps_model::ValueSource::None
                },
            }));
        let schema = pbps_model::Schema::default();
        let side = crate::Side {
            schema: &schema,
            ids: &ids,
        };
        lock_window(&mut plan, side, side);
        let findings = plan
            .changes
            .iter()
            .flat_map(|p| &p.findings)
            .filter(|f| f.id == "plan.partition-alter-attach-lock")
            .count();
        assert_eq!(findings, usize::from(!retuning));
    }
}

#[test]
fn a_changed_physical_spelling_requires_boundary_recertification() {
    use pbps_model::operation::AddressChain;
    use pbps_model::plan_names::PlanNames;
    let (mut plan, ops, effects, state) = fixture();
    plan.boundaries
        .discharge(ops.as_slice(), &effects, &state)
        .unwrap();
    plan.identities = Some(ops.clone());
    assert!(plan.boundaries.ensure(&plan).is_ok());
    let parent: Uid = "t_aaaaaa".parse().unwrap();
    let mut before = pbps_model::IdsFile::default();
    before
        .tables
        .insert(parent.clone(), "app.p".parse().unwrap());
    before
        .tables
        .insert("t_bbbbbb".parse().unwrap(), "app.a".parse().unwrap());
    let mut after = before.clone();
    after
        .tables
        .insert(parent.clone(), "app.q".parse().unwrap());
    let mut history = plan.changes.clone();
    history[0] = PlannedChange::new(Change::RenameTable {
        uid: parent,
        from: "app.p".parse().unwrap(),
        to: "app.q".parse().unwrap(),
        defaults: vec![],
    });
    history[1] = PlannedChange::new(Change::AttachPartition {
        uid: "t_bbbbbb".parse().unwrap(),
        table: "app.a".parse().unwrap(),
        parent: "app.q".parse().unwrap(),
        bound: pbps_model::PartitionBound::Default,
        key: vec![],
        shape: Box::default(),
    });
    let ordered =
        Operations::from_operations([ops.as_slice()[1].clone(), ops.as_slice()[0].clone()]);
    let names = PlanNames::of(&history, &after).unwrap();
    let chain = AddressChain::bind(
        &names,
        &ordered,
        &before,
        &after,
        &BTreeSet::new(),
        &[ops.as_slice()[0].id()].into_iter().collect(),
    );
    let altered = ops.with_address_chain(chain);
    assert_eq!(altered.as_slice()[0].id(), ops.as_slice()[0].id());
    assert_eq!(altered.as_slice()[0].origin(), ops.as_slice()[0].origin());
    assert_ne!(
        altered.as_slice()[0].origin().spelling(),
        ops.as_slice()[0].origin().spelling()
    );
    plan.identities = Some(altered);
    assert!(plan.boundaries.ensure(&plan).is_err());
}

#[test]
fn unread_membership_and_known_empty_membership_have_different_obligations() {
    for unread in [false, true] {
        let (mut plan, _, _, _) = fixture();
        let mut schema = pbps_model::Schema::default();
        schema
            .tables
            .insert("app.a".parse().unwrap(), Default::default());
        if !unread {
            schema
                .tables
                .insert("app.p".parse().unwrap(), Default::default());
        }
        let mut ids = pbps_model::IdsFile::default();
        ids.tables
            .insert("t_aaaaaa".parse().unwrap(), "app.p".parse().unwrap());
        ids.tables
            .insert("t_bbbbbb".parse().unwrap(), "app.a".parse().unwrap());
        ids.roles
            .insert("r_aaaaaa".parse().unwrap(), "unused".into());
        let side = crate::Side {
            schema: &schema,
            ids: &ids,
        };
        let error = replay(&mut plan, side, side, &pbps_dialect::MinimalDialect, None).unwrap_err();
        // Known empty membership can advance to the independent missing-engine
        // transfer obligation. Unread membership cannot advance that far.
        assert!(
            error.contains(if unread {
                "no complete standalone membership"
            } else {
                "no symbolic transfer"
            }),
            "{error}"
        );
        assert!(error.contains(membership::REMEDY), "{error}");
    }
}
