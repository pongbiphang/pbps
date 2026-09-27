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

// A default and its owner are distinct catalog records. Keeping only the
// child's transition must not leave a dropped owner, or omit a created one,
// even when the binding-surface inventory contains only the expression.
fn owner_coverage(change: Change, owner: Surface, child: Surface, creating: bool) {
    use crate::resolver::{BoundSurface, Prerequisite};
    let table: TableName = "app.v".parse().unwrap();
    let (_, mut evidence) = drop_fixture(
        Change::DropTable {
            uid: "t_000000".parse().unwrap(),
            name: table.clone(),
        },
        Surface::Table(table.clone()),
    );
    let table_object = evidence.surfaces[0]
        .current
        .as_ref()
        .unwrap()
        .object
        .clone();
    let column_object = ObjectIdentity {
        class: "column".into(),
        name: vec!["n".into()],
        signature: vec![table_object.clone()],
    };
    let owner_object = if matches!(owner, Surface::Table(_)) {
        table_object.clone()
    } else {
        column_object.clone()
    };
    let child_object = if matches!(child, Surface::Column(_)) {
        column_object
    } else {
        ObjectIdentity {
            class: "default".into(),
            name: vec!["n".into()],
            signature: vec![table_object],
        }
    };
    let mut records = evidence.before.prerequisites().to_vec();
    for object in [&owner_object, &child_object] {
        if !records.iter().any(|p| &p.object == object) {
            records.push(Prerequisite {
                object: object.clone(),
                canonicalization: "fixture-v1".into(),
                properties: "dd".repeat(32),
                bindings: vec![],
            });
        }
    }
    records.sort_by(|a, b| a.object.cmp(&b.object));
    let manifest = |owner_present: bool, child_present: bool| {
        let records: Vec<_> = records
            .iter()
            .filter(|p| {
                (p.object != owner_object || owner_present)
                    && (p.object != child_object || child_present)
            })
            .collect();
        let mut membership = evidence.before.membership().to_vec();
        for m in &mut membership {
            m.members
                .retain(|id| records.iter().any(|p| &p.object == id));
        }
        let mut json = serde_json::to_value(&evidence.before).unwrap();
        json["prerequisites"] = serde_json::to_value(records).unwrap();
        json["membership"] = serde_json::to_value(membership).unwrap();
        serde_json::from_value::<InputManifest>(json).unwrap()
    };
    let before = manifest(!creating, !creating);
    let compiled = manifest(creating, creating);
    let wrong_after = manifest(!creating, creating);
    let changes = ChangeSet {
        changes: vec![PlannedChange::new(change)],
    };
    let observation = BoundSurface {
        object: child_object.clone(),
        bindings: vec![],
        managed_inputs: BTreeSet::new(),
    };
    evidence.before = before;
    evidence.surfaces = vec![SurfaceResolution {
        surface: child.clone(),
        current: (!creating).then(|| observation.clone()),
        desired: creating.then_some(observation),
    }];
    let owned = BTreeSet::from([owner_object.clone(), child_object.clone()]);
    evidence.transitions = vec![ObjectTransition {
        surface: owner,
        before: if creating {
            BTreeSet::new()
        } else {
            owned.clone()
        },
        after: if creating { owned } else { BTreeSet::new() },
    }];
    evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
    evidence.after = evidence
        .before
        .project(&changes, &compiled, &evidence.transitions)
        .unwrap();
    evidence.validate(&changes).unwrap();
    assert_eq!(evidence.after, compiled);
    // Aggregate table ownership remains valid for ADD COLUMN plus its default.
    if matches!(evidence.transitions[0].surface, Surface::Column(_)) {
        let mut aggregate = evidence.clone();
        aggregate.transitions[0].surface = Surface::Table(table);
        aggregate.validate(&changes).unwrap();
    }
    let child_only = BTreeSet::from([child_object]);
    evidence.transitions = vec![ObjectTransition {
        surface: child,
        before: if creating {
            BTreeSet::new()
        } else {
            child_only.clone()
        },
        after: if creating {
            child_only
        } else {
            BTreeSet::new()
        },
    }];
    assert!(
        evidence
            .before
            .project(&changes, &compiled, &evidence.transitions)
            .is_err(),
        "child-only transition must not substitute for the changed owner"
    );
    assert!(
        ResolverEvidence::new(
            &changes,
            evidence.qualification.clone(),
            evidence.authorization.clone(),
            evidence.before.clone(),
            &compiled,
            evidence.surfaces.clone(),
            evidence.transitions.clone(),
            evidence.ordering.clone(),
        )
        .is_err(),
        "constructor accepted a missing owner transition"
    );
    evidence.after = wrong_after;
    assert_eq!(
        evidence
            .after
            .prerequisites()
            .iter()
            .any(|p| p.object == owner_object),
        !creating
    );
    let decoded: ResolverEvidence =
        serde_json::from_value(serde_json::to_value(evidence).unwrap()).unwrap();
    assert!(
        decoded.validate(&changes).is_err(),
        "reader accepted a stale owner record"
    );
}

#[test]
fn table_drops_require_the_owner_even_with_a_child_transition() {
    let table: TableName = "app.v".parse().unwrap();
    for child in [
        Surface::Default(table.column("n")),
        Surface::Column(table.column("n")),
    ] {
        owner_coverage(
            Change::DropTable {
                uid: "t_000000".parse().unwrap(),
                name: table.clone(),
            },
            Surface::Table(table.clone()),
            child,
            false,
        );
    }
}

#[test]
fn table_creation_requires_the_owner_even_with_a_child_transition() {
    let table: TableName = "app.v".parse().unwrap();
    let definition: crate::Table = serde_json::from_value(serde_json::json!({
        "columns": {"n": {"type": "integer", "nullable": true, "default": "7"}}
    }))
    .unwrap();
    for child in [
        Surface::Default(table.column("n")),
        Surface::Column(table.column("n")),
    ] {
        owner_coverage(
            Change::CreateTable {
                uid: "t_000000".parse().unwrap(),
                name: table.clone(),
                table: Box::new(definition.clone()),
            },
            Surface::Table(table.clone()),
            child,
            true,
        );
    }
}

#[test]
fn adding_a_column_requires_more_than_its_default_transition() {
    let table: TableName = "app.v".parse().unwrap();
    let column =
        serde_json::from_value(serde_json::json!({"type":"integer","nullable":true,"default":"7"}))
            .unwrap();
    owner_coverage(
        Change::AddColumn {
            uid: "c_000000".parse().unwrap(),
            table: table.clone(),
            name: "n".into(),
            column: Box::new(column),
        },
        Surface::Column(table.column("n")),
        Surface::Default(table.column("n")),
        true,
    );
}
