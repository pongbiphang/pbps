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
