use super::*;
use crate::resolver::{ObjectIdentity, Surface};
use crate::{ModuleKind, PlannedChange, TableName};

// The model treats catalog addresses as opaque adapter-owned identities. Reuse
// one fixture record to check every typed surface without inventing engine SQL.
fn drop_fixture(change: Change, surface: Surface) -> (ChangeSet, ResolverEvidence) {
    let plan = super::tests::plan();
    let PlanAnalysis::Resolved(mut evidence) = plan.analysis else {
        panic!("resolved")
    };
    let changes = ChangeSet {
        changes: vec![PlannedChange::new(change)],
    };
    std::mem::swap(&mut evidence.before, &mut evidence.after);
    let observation = &mut evidence.surfaces[0];
    std::mem::swap(&mut observation.current, &mut observation.desired);
    observation.surface = surface.clone();
    let transition = &mut evidence.transitions[0];
    std::mem::swap(&mut transition.before, &mut transition.after);
    transition.surface = surface;
    // Compilation's external hash differs from the target. Projection must
    // preserve the target's untouched record, even on an otherwise empty drop.
    evidence.after = evidence
        .before
        .project(&changes, &evidence.after, &evidence.transitions)
        .unwrap();
    evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
    (changes, *evidence)
}

fn dropped_surfaces() -> Vec<(Change, Surface)> {
    let table: TableName = "app.v".parse().unwrap();
    let uid = "c_000000".parse().unwrap();
    vec![
        (
            Change::DropModule {
                id: "app.v".parse().unwrap(),
                kind: ModuleKind::View,
            },
            Surface::Module("app.v".parse().unwrap()),
        ),
        (
            Change::AlterColumnDefault {
                uid,
                column: table.column("n"),
                from: Some("1".into()),
                to: None,
            },
            Surface::Default(table.column("n")),
        ),
        (
            Change::DropCheck {
                table: table.clone(),
                name: "positive".into(),
            },
            Surface::Check {
                table: table.clone(),
                name: "positive".into(),
            },
        ),
        (
            Change::DropIndex {
                table: table.clone(),
                name: "ix".into(),
            },
            Surface::Index {
                table: table.clone(),
                name: "ix".into(),
            },
        ),
        (
            Change::DropColumn {
                uid: "c_000000".parse().unwrap(),
                column: table.column("n"),
            },
            Surface::Column(table.column("n")),
        ),
        (
            Change::DropTable {
                uid: "t_000000".parse().unwrap(),
                name: table.clone(),
            },
            Surface::Table(table),
        ),
    ]
}

#[test]
fn dropped_surfaces_cannot_omit_their_closing_transitions() {
    for (change, surface) in dropped_surfaces() {
        let (changes, evidence) = drop_fixture(change, surface.clone());
        evidence.validate(&changes).unwrap();
        assert!(
            evidence
                .before
                .project(&changes, &evidence.after, &[])
                .is_err(),
            "projection accepted missing {surface:?}"
        );
        assert!(evidence.after.membership()[0].members.is_empty());
        assert_eq!(evidence.after.prerequisites().len(), 1);
        assert_eq!(
            evidence.after.prerequisites()[0],
            evidence.before.prerequisites()[1]
        );
        assert!(
            ResolverEvidence::new(
                &changes,
                evidence.qualification.clone(),
                evidence.authorization.clone(),
                evidence.before.clone(),
                &evidence.after,
                evidence.surfaces.clone(),
                vec![],
                evidence.ordering.clone(),
            )
            .is_err(),
            "constructor accepted missing {surface:?}"
        );
        let mut omitted = evidence;
        omitted.transitions.clear();
        omitted.after = omitted.before.clone();
        let restored: ResolverEvidence =
            serde_json::from_value(serde_json::to_value(omitted).unwrap()).unwrap();
        assert!(
            restored.validate(&changes).is_err(),
            "reader accepted missing {surface:?}"
        );
    }
}

#[test]
fn a_transition_must_cover_the_observed_object() {
    let (change, surface) = dropped_surfaces().remove(0);
    let (changes, mut wrong) = drop_fixture(change, surface);
    wrong.after = wrong.before.clone();
    wrong.transitions[0].before = BTreeSet::from([ObjectIdentity {
        class: "relation".into(),
        name: vec!["ext".into(), "input".into()],
        signature: vec![],
    }]);
    wrong.transitions[0].after = wrong.transitions[0].before.clone();
    assert!(
        wrong.validate(&changes).is_err(),
        "accepted wrong observed object"
    );
}

#[test]
fn changed_definitions_need_transitions_even_when_bindings_are_identical() {
    let plan = super::tests::plan();
    let PlanAnalysis::Resolved(mut evidence) = plan.analysis else {
        panic!("resolved")
    };
    let Change::CreateModule { id, module } = plan.changes.changes[0].change.clone() else {
        panic!("create")
    };
    let changes = ChangeSet {
        changes: vec![PlannedChange::new(Change::AlterModule { id, module })],
    };
    evidence.before = evidence.after.clone();
    evidence.surfaces[0].current = evidence.surfaces[0].desired.clone();
    evidence.transitions[0].before = evidence.transitions[0].after.clone();
    evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
    evidence.validate(&changes).unwrap();
    evidence.transitions.clear();
    assert!(evidence.validate(&changes).is_err());
}

#[test]
fn a_table_transition_can_own_its_removed_expression_records() {
    let (change, _) = dropped_surfaces().pop().unwrap();
    let (changes, mut evidence) = drop_fixture(change, Surface::Table("app.v".parse().unwrap()));
    evidence.surfaces[0].surface =
        Surface::Default("app.v".parse::<TableName>().unwrap().column("n"));
    evidence.validate(&changes).unwrap();
    evidence.transitions[0].before =
        BTreeSet::from([evidence.before.prerequisites()[1].object.clone()]);
    evidence.transitions[0].after = evidence.transitions[0].before.clone();
    evidence.after = evidence.before.clone();
    assert!(evidence.validate(&changes).is_err());
}

#[test]
fn unchanged_catalogs_do_not_need_a_transition_for_row_or_metadata_changes() {
    let plan = super::tests::plan();
    let PlanAnalysis::Resolved(mut evidence) = plan.analysis else {
        panic!("resolved")
    };
    let changes = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::SetColumnDeprecated {
                uid: "c_000000".parse().unwrap(),
                column: "app.v".parse::<TableName>().unwrap().column("n"),
                reason: Some("unused".into()),
            }),
            PlannedChange::new(Change::UpdateRow {
                table: "app.v".parse().unwrap(),
                key_column: "id".into(),
                key: crate::RowKey("1".into()),
                columns: std::collections::BTreeMap::from([(
                    "n".into(),
                    (
                        crate::Cell::Value(crate::Value::Text("before".into())),
                        crate::Cell::Value(crate::Value::Text("after".into())),
                    ),
                )]),
                unchanged: Default::default(),
                types: Default::default(),
                after_types: Default::default(),
            }),
        ],
    };
    evidence.before = evidence.after.clone();
    evidence.surfaces[0].current = evidence.surfaces[0].desired.clone();
    evidence.surfaces[0].surface =
        Surface::Column("app.v".parse::<TableName>().unwrap().column("n"));
    evidence.transitions.clear();
    evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
    evidence.validate(&changes).unwrap();
}

#[test]
fn renamed_surfaces_can_share_one_transition_between_their_old_and_new_names() {
    let (change, _) = dropped_surfaces().pop().unwrap();
    let (_, mut evidence) = drop_fixture(change, Surface::Table("app.v".parse().unwrap()));
    let changes = ChangeSet {
        changes: vec![PlannedChange::new(Change::RenameTable {
            uid: "t_000000".parse().unwrap(),
            from: "app.v".parse().unwrap(),
            to: "app.w".parse().unwrap(),
            defaults: vec![],
        })],
    };
    let mut desired = evidence.surfaces[0].current.clone().unwrap();
    desired.object.name[1] = "w".into();
    evidence.transitions[0].after = BTreeSet::from([desired.object.clone()]);
    let mut compiled = serde_json::to_value(&evidence.before).unwrap();
    compiled["prerequisites"][0]["object"] = serde_json::to_value(&desired.object).unwrap();
    compiled["membership"][0]["members"][0] = serde_json::to_value(&desired.object).unwrap();
    let compiled = serde_json::from_value(compiled).unwrap();
    evidence.surfaces.push(SurfaceResolution {
        surface: Surface::Table("app.w".parse().unwrap()),
        current: None,
        desired: Some(desired),
    });
    evidence.after = evidence
        .before
        .project(&changes, &compiled, &evidence.transitions)
        .unwrap();
    evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
    evidence.validate(&changes).unwrap();
    let mut missing = evidence;
    missing.transitions[0].after.clear();
    assert!(missing.validate(&changes).is_err());
}

#[test]
fn a_dropped_view_can_be_replaced_by_a_table_at_the_same_logical_address() {
    let (change, surface) = dropped_surfaces().remove(0);
    let (mut changes, mut evidence) = drop_fixture(change, surface);
    let table: crate::Table = serde_json::from_value(serde_json::json!({
        "columns": {"n": {"type": "integer", "nullable": true}}
    }))
    .unwrap();
    changes
        .changes
        .push(PlannedChange::new(Change::CreateTable {
            uid: "t_000000".parse().unwrap(),
            name: "app.v".parse().unwrap(),
            table: Box::new(table),
        }));
    let desired = evidence.surfaces[0].current.clone().unwrap();
    evidence.surfaces.insert(
        0,
        SurfaceResolution {
            surface: Surface::Table("app.v".parse().unwrap()),
            current: None,
            desired: Some(desired.clone()),
        },
    );
    evidence.transitions.insert(
        0,
        ObjectTransition {
            surface: Surface::Table("app.v".parse().unwrap()),
            before: BTreeSet::new(),
            after: BTreeSet::from([desired.object]),
        },
    );
    evidence.after = evidence
        .before
        .project(&changes, &evidence.before, &evidence.transitions)
        .unwrap();
    evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
    evidence.validate(&changes).unwrap();
    // A table without expressions contributes catalog records but need not
    // appear in the producer's binding-surface inventory.
    evidence.surfaces.remove(0);
    evidence.validate(&changes).unwrap();
    evidence.transitions.remove(0);
    assert!(evidence.validate(&changes).is_err());
}

#[test]
fn removing_a_predicate_can_retain_the_plain_index_catalog_record() {
    let (change, surface) = dropped_surfaces().remove(3);
    let (mut changes, mut evidence) = drop_fixture(change, surface);
    changes.changes.push(PlannedChange::new(Change::AddIndex {
        table: "app.v".parse().unwrap(),
        name: "ix".into(),
        index: Box::new(crate::Index {
            columns: vec![crate::IndexColumn {
                name: "n".into(),
                descending: false,
            }],
            include: vec![],
            unique: false,
            filter: None,
        }),
    }));
    evidence.transitions[0].after = evidence.transitions[0].before.clone();
    evidence.after = evidence.before.clone();
    evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
    evidence.validate(&changes).unwrap();
    evidence.transitions.clear();
    assert!(evidence.validate(&changes).is_err());
}
