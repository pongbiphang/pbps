use super::*;
use crate::resolver::{ObjectIdentity, ObjectOwnership, Surface};
use crate::{ModuleKind, PlannedChange, TableName};

fn own(manifest: &mut InputManifest, object: &ObjectIdentity, owner: Surface) {
    let mut json = serde_json::to_value(&*manifest).unwrap();
    let record = json["prerequisites"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|p| p["object"] == serde_json::to_value(object).unwrap())
        .unwrap();
    record["ownership"] = serde_json::to_value(ObjectOwnership::Surface(owner)).unwrap();
    *manifest = serde_json::from_value(json).unwrap();
}

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
    // The view the fixture plan created opens this plan observed with its
    // real fingerprint: an opening manifest never holds a placeholder.
    evidence.after = super::tests::as_opening(
        &super::tests::observed(&evidence, &super::tests::compiled()),
        &evidence.before,
    );
    std::mem::swap(&mut evidence.before, &mut evidence.after);
    let observation = &mut evidence.surfaces[0];
    std::mem::swap(&mut observation.current, &mut observation.desired);
    observation.surface = surface.clone();
    let transition = &mut evidence.transitions[0];
    std::mem::swap(&mut transition.before, &mut transition.after);
    transition.surface = surface.clone();
    let object = transition.before.iter().next().unwrap().clone();
    own(&mut evidence.before, &object, surface);
    if let Change::DropColumn { column, .. } = &changes.changes[0].change {
        // A removed column still changes its surviving parent's vector. Keep
        // that obligation separate from the existing opaque column fixture.
        let parent = crate::resolver::Prerequisite {
            object: ObjectIdentity {
                class: "table-fixture".into(),
                name: vec![column.table.schema.clone(), column.table.name.clone()],
                signature: vec![],
            },
            ownership: ObjectOwnership::Surface(Surface::Table(column.table.clone())),
            canonicalization: "fixture-v1".into(),
            properties: "bc".repeat(32),
            bindings: vec![],
        };
        for manifest in [&mut evidence.before, &mut evidence.after] {
            let mut json = serde_json::to_value(&*manifest).unwrap();
            let mut records = manifest.prerequisites().to_vec();
            records.push(parent.clone());
            records.sort_by(|a, b| a.object.cmp(&b.object));
            json["prerequisites"] = serde_json::to_value(records).unwrap();
            *manifest = serde_json::from_value(json).unwrap();
        }
        evidence.transitions.push(ObjectTransition {
            references: BTreeSet::new(),
            surface: Surface::Table(column.table.clone()),
            before: BTreeSet::from([parent.object.clone()]),
            after: BTreeSet::from([parent.object]),
        });
    }
    evidence
        .transitions
        .sort_by(|a, b| a.surface.cmp(&b.surface));
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
                detach_from: None,
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
        // A dropped column's surviving parent is reinstalled by its own
        // transition and nothing names it, so it is not predicted either.
        assert_eq!(evidence.after.prerequisites().len(), 1);
        assert_eq!(
            evidence
                .after
                .prerequisites()
                .iter()
                .find(|p| p.object == evidence.before.prerequisites()[1].object)
                .unwrap(),
            &evidence.before.prerequisites()[1]
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
    evidence.before = super::tests::observed(&evidence, &super::tests::compiled());
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
    // Without a transition the closing manifest is the opening one, record
    // for record: both are the observed catalog the fixture plan left.
    evidence.before = super::tests::observed(&evidence, &super::tests::compiled());
    evidence.after = evidence.before.clone();
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
    let mut compiled = serde_json::from_value(compiled).unwrap();
    own(
        &mut compiled,
        &desired.object,
        Surface::Table("app.w".parse().unwrap()),
    );
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
            references: BTreeSet::new(),
            surface: Surface::Table("app.v".parse().unwrap()),
            before: BTreeSet::new(),
            after: BTreeSet::from([desired.object]),
        },
    );
    let mut compiled = evidence.before.clone();
    own(
        &mut compiled,
        &evidence.transitions[0].after.iter().next().unwrap().clone(),
        Surface::Table("app.v".parse().unwrap()),
    );
    evidence.after = evidence
        .before
        .project(&changes, &compiled, &evidence.transitions)
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
        clustered: false,
        index: Box::new(crate::Index {
            columns: vec![crate::IndexColumn {
                key: crate::IndexKey::Column("n".into()),
                descending: false,
                opclass: None,
            }],
            include: vec![],
            unique: false,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
        }),
    }));
    evidence.transitions[0].after = evidence.transitions[0].before.clone();
    // Scratch compiles the plain index as the same catalog record.
    let compiled = evidence.before.clone();
    evidence.after = evidence
        .before
        .project(&changes, &compiled, &evidence.transitions)
        .unwrap();
    evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
    evidence.validate(&changes).unwrap();
    evidence.transitions.clear();
    assert!(evidence.validate(&changes).is_err());
}

// A default and its owner are distinct catalog records. Keeping only the
// child's transition must not leave a dropped owner, or omit a created one,
// even when the binding-surface inventory contains only the expression.
// Returns the compiled manifest too: the closing manifest no longer carries
// the plan's own records, so it cannot stand in for compilation.
fn owner_coverage(
    change: Change,
    owner: Surface,
    child: Surface,
    creating: bool,
) -> (ChangeSet, ResolverEvidence, InputManifest) {
    use crate::resolver::{BoundSurface, Prerequisite};
    let vector_edit = matches!(
        &change,
        Change::AddColumn { .. } | Change::DropColumn { .. }
    );
    let table: TableName = "app.v".parse().unwrap();
    let (_, mut evidence) = drop_fixture(
        Change::DropTable {
            uid: "t_000000".parse().unwrap(),
            name: table.clone(),
            detach_from: None,
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
            signature: vec![table_object.clone()],
        }
    };
    let mut records = evidence.before.prerequisites().to_vec();
    for object in [&owner_object, &child_object] {
        if !records.iter().any(|p| &p.object == object) {
            records.push(Prerequisite {
                object: object.clone(),
                ownership: ObjectOwnership::Surface(if object == &owner_object {
                    owner.clone()
                } else {
                    child.clone()
                }),
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
        references: BTreeSet::new(),
        surface: owner.clone(),
        before: if creating {
            BTreeSet::new()
        } else {
            owned.clone()
        },
        after: if creating { owned } else { BTreeSet::new() },
    }];
    if vector_edit {
        // The table already exists on both sides of this fixture. Its exact
        // inventory supplements, and never replaces, the column inventory.
        evidence.transitions.push(ObjectTransition {
            references: BTreeSet::new(),
            surface: Surface::Table(table.clone()),
            before: BTreeSet::from([table_object.clone()]),
            after: BTreeSet::from([table_object]),
        });
    }
    evidence
        .transitions
        .sort_by(|a, b| a.surface.cmp(&b.surface));
    evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
    evidence.after = evidence
        .before
        .project(&changes, &compiled, &evidence.transitions)
        .unwrap();
    evidence.validate(&changes).unwrap();
    super::tests::assert_closing(&evidence, &compiled);
    // A table label alone does not prove a vector edit's independent parent
    // inventory. A computed column has no vector parent (#1174): a table
    // label over its records covers exactly the same records.
    if vector_edit && matches!(owner, Surface::Column(_)) {
        let mut aggregate = evidence.clone();
        let owner_transition = evidence
            .transitions
            .iter()
            .find(|transition| transition.surface == owner)
            .unwrap()
            .clone();
        aggregate.transitions = vec![ObjectTransition {
            references: BTreeSet::new(),
            surface: Surface::Table(table),
            ..owner_transition
        }];
        assert!(aggregate.validate(&changes).is_err());
    }
    let valid = evidence.clone();
    // A surface that is its own only record, as a computed column is (it has
    // no default under it, #1174), has no child to substitute for it.
    if owner_object == child_object {
        return (changes, valid, compiled);
    }
    let child_only = BTreeSet::from([child_object]);
    evidence.transitions = vec![ObjectTransition {
        references: BTreeSet::new(),
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
    // A created owner omitted from the closing inventory is a closing-side
    // defect: the reader has no compiled records to prove it, so only the
    // projection and constructor above refuse it.
    if !creating {
        evidence.after = wrong_after;
        assert!(
            evidence
                .after
                .prerequisites()
                .iter()
                .any(|p| p.object == owner_object)
        );
        let decoded: ResolverEvidence =
            serde_json::from_value(serde_json::to_value(evidence).unwrap()).unwrap();
        assert!(
            decoded.validate(&changes).is_err(),
            "reader accepted a stale owner record"
        );
    }
    (changes, valid, compiled)
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
                detach_from: None,
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
        let mut definition = definition.clone();
        if matches!(&child, Surface::Column(_)) {
            definition.columns.get_mut("n").unwrap().default = None;
        }
        owner_coverage(
            Change::CreateTable {
                uid: "t_000000".parse().unwrap(),
                name: table.clone(),
                table: Box::new(definition),
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

pub(super) fn rename_endpoints(column: bool) -> (ChangeSet, ResolverEvidence, InputManifest) {
    let table: TableName = "app.v".parse().unwrap();
    let (drop, surface) = dropped_surfaces().pop().unwrap();
    let (_, mut evidence) = drop_fixture(drop, surface);
    let table_object = evidence.before.prerequisites()[0].object.clone();
    let (change, from, to, old_object, new_object) = if column {
        let old_object = ObjectIdentity {
            class: "column".into(),
            name: vec!["n".into()],
            signature: vec![table_object],
        };
        let mut new_object = old_object.clone();
        new_object.name[0] = "m".into();
        let mut before = serde_json::to_value(&evidence.before).unwrap();
        let mut parent = before["prerequisites"][0].clone();
        parent["ownership"] =
            serde_json::to_value(ObjectOwnership::Surface(Surface::Table(table.clone()))).unwrap();
        before["prerequisites"][0]["object"] = serde_json::to_value(&old_object).unwrap();
        before["prerequisites"].as_array_mut().unwrap().push(parent);
        before["prerequisites"]
            .as_array_mut()
            .unwrap()
            .sort_by_key(|p| {
                serde_json::from_value::<ObjectIdentity>(p["object"].clone()).unwrap()
            });
        // Columns are prerequisites, not relation-name candidates.
        before["membership"][0]["members"] = serde_json::json!([]);
        evidence.before = serde_json::from_value(before).unwrap();
        (
            Change::RenameColumn {
                uid: "c_000000".parse().unwrap(),
                table: table.clone(),
                from: "n".into(),
                to: "m".into(),
                table_was: None,
            },
            Surface::Column(table.column("n")),
            Surface::Column(table.column("m")),
            old_object,
            new_object,
        )
    } else {
        let mut new_object = table_object.clone();
        new_object.name[1] = "w".into();
        (
            Change::RenameTable {
                uid: "t_000000".parse().unwrap(),
                from: table.clone(),
                to: "app.w".parse().unwrap(),
                defaults: vec![],
            },
            Surface::Table(table.clone()),
            Surface::Table("app.w".parse().unwrap()),
            table_object,
            new_object,
        )
    };
    let changes = ChangeSet {
        changes: vec![PlannedChange::new(change)],
    };
    let mut compiled = serde_json::to_value(&evidence.before).unwrap();
    compiled["prerequisites"][0]["object"] = serde_json::to_value(&new_object).unwrap();
    if !column {
        compiled["membership"][0]["members"][0] = serde_json::to_value(&new_object).unwrap();
    }
    let mut compiled: InputManifest = serde_json::from_value(compiled).unwrap();
    own(&mut evidence.before, &old_object, from.clone());
    own(&mut compiled, &new_object, to.clone());
    // Plain tables and columns need not occur in a binding-surface inventory.
    evidence.surfaces.clear();
    evidence.transitions = vec![ObjectTransition {
        references: BTreeSet::new(),
        surface: from.clone(),
        before: BTreeSet::from([old_object.clone()]),
        after: BTreeSet::from([new_object.clone()]),
    }];
    if column {
        let parent = evidence
            .before
            .prerequisites()
            .iter()
            .find(|p| p.ownership == ObjectOwnership::Surface(Surface::Table(table.clone())))
            .unwrap()
            .object
            .clone();
        evidence.transitions.push(ObjectTransition {
            references: BTreeSet::new(),
            surface: Surface::Table(table.clone()),
            before: BTreeSet::from([parent.clone()]),
            after: BTreeSet::from([parent]),
        });
    }
    evidence
        .transitions
        .sort_by(|a, b| a.surface.cmp(&b.surface));
    let rename_index = evidence
        .transitions
        .iter()
        .position(|transition| transition.surface == from)
        .unwrap();
    evidence.after = evidence
        .before
        .project(&changes, &compiled, &evidence.transitions)
        .unwrap();
    evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
    evidence.validate(&changes).unwrap();
    super::tests::assert_closing(&evidence, &compiled);
    // A producer may split the old/new column inventory. The independent
    // parent inventory cannot stand in for either column endpoint.
    let mut split = evidence.clone();
    split.transitions[rename_index].after.clear();
    split.transitions.push(ObjectTransition {
        references: BTreeSet::new(),
        surface: to,
        before: BTreeSet::new(),
        after: BTreeSet::from([new_object.clone()]),
    });
    split.transitions.sort_by(|a, b| a.surface.cmp(&b.surface));
    split.validate(&changes).unwrap();
    if column {
        let mut aggregate = evidence.clone();
        aggregate.transitions = vec![ObjectTransition {
            references: BTreeSet::new(),
            surface: Surface::Table(table),
            ..evidence.transitions[rename_index].clone()
        }];
        assert!(aggregate.validate(&changes).is_err());
    }
    for omit_before in [false, true] {
        let mut omitted = evidence.clone();
        if omit_before {
            omitted.transitions[rename_index].before.clear();
            let mut after = serde_json::to_value(&evidence.before).unwrap();
            let mut records = evidence.before.prerequisites().to_vec();
            // The renamed table is a candidate member, so its closing
            // record is the placeholder; the renamed column is named by
            // nothing and is absent.
            if !column {
                records.extend(
                    compiled
                        .prerequisites()
                        .iter()
                        .filter(|p| p.object == new_object)
                        .map(|p| p.managed_closing()),
                );
                after["membership"][0]["members"] =
                    serde_json::to_value(BTreeSet::from([old_object.clone(), new_object.clone()]))
                        .unwrap();
            }
            records.sort_by(|a, b| a.object.cmp(&b.object));
            after["prerequisites"] = serde_json::to_value(records).unwrap();
            omitted.after = serde_json::from_value(after).unwrap();
            let decoded: ResolverEvidence =
                serde_json::from_value(serde_json::to_value(&omitted).unwrap()).unwrap();
            assert!(
                decoded.validate(&changes).is_err(),
                "rename reader accepted missing opening endpoint"
            );
        } else {
            // The closing endpoint is a compiled record the reader cannot
            // see; only projection and the constructor can refuse its omission.
            omitted.transitions[rename_index].after.clear();
        }
        assert!(
            evidence
                .before
                .project(&changes, &compiled, &omitted.transitions)
                .is_err(),
            "projection accepted missing rename endpoint"
        );
        assert!(
            ResolverEvidence::new(
                &changes,
                evidence.qualification.clone(),
                evidence.authorization.clone(),
                evidence.before.clone(),
                &compiled,
                vec![],
                omitted.transitions,
                evidence.ordering.clone(),
            )
            .is_err(),
            "constructor accepted missing rename endpoint"
        );
    }
    (changes, evidence, compiled)
}

#[test]
fn plain_table_renames_require_opening_and_closing_inventories() {
    rename_endpoints(false);
}

/// A foreign key on another table that points at a renamed table is
/// rewritten in place by PostgreSQL. The touched table's transition may carry
/// it as a listed reference of a table-family owner; an unlisted, unqualified
/// or routine-owned record never rides along (#1466).
#[test]
fn a_touched_tables_transition_carries_only_listed_table_family_references() {
    for column in [false, true] {
        let (changes, evidence, compiled) = rename_endpoints(column);
        let rename = evidence
            .transitions
            .iter()
            .position(|t| {
                changes.changes.iter().any(|step| {
                    matches!(
                        step.change,
                        Change::RenameTable { .. } | Change::RenameColumn { .. }
                    ) && super::super::projection::touches(&step.change, &t.surface)
                })
            })
            .unwrap();
        let foreign = ObjectIdentity {
            class: "constraint-fixture".into(),
            name: vec!["child_fk".into()],
            signature: vec![],
        };
        let record = |ownership: ObjectOwnership, properties: &str| crate::resolver::Prerequisite {
            object: foreign.clone(),
            ownership,
            canonicalization: "fixture-v1".into(),
            properties: properties.repeat(32),
            bindings: vec![],
        };
        let child = ObjectOwnership::Surface(Surface::Table("app.child".parse().unwrap()));
        let add = |manifest: &InputManifest, p: crate::resolver::Prerequisite| {
            let mut records: Vec<_> = manifest.prerequisites().to_vec();
            records.push(p);
            records.sort_by(|a, b| a.object.cmp(&b.object));
            let mut json = serde_json::to_value(manifest).unwrap();
            json["prerequisites"] = serde_json::to_value(records).unwrap();
            serde_json::from_value::<InputManifest>(json).unwrap()
        };
        let carry = |ownership: ObjectOwnership, listed: bool| {
            let mut carried = evidence.clone();
            carried.before = add(&evidence.before, record(ownership.clone(), "c1"));
            let compiled = add(&compiled, record(ownership, "c2"));
            let transition = &mut carried.transitions[rename];
            transition.before.insert(foreign.clone());
            transition.after.insert(foreign.clone());
            if listed {
                transition.references.insert(foreign.clone());
            }
            (carried, compiled)
        };
        let (carried, carried_compiled) = carry(child.clone(), true);
        let sealed = carried
            .before
            .project(&changes, &carried_compiled, &carried.transitions)
            .unwrap();
        assert!(
            sealed.prerequisites().iter().all(|p| p.object != foreign),
            "a carried reference is not compared with a predicted fingerprint"
        );
        for (ownership, listed, case) in [
            (child.clone(), false, "an unlisted reference"),
            (
                ObjectOwnership::Unqualified,
                true,
                "an unqualified reference",
            ),
        ] {
            let (wrong, wrong_compiled) = carry(ownership, listed);
            assert!(
                wrong
                    .before
                    .project(&changes, &wrong_compiled, &wrong.transitions)
                    .is_err(),
                "{case} rode on the rename (column={column})"
            );
        }
        // Only the owner differs from the accepted case: a routine's record
        // is not part of any table's tree and confers no table authority.
        let routine = ObjectOwnership::Surface(Surface::Module("app.f()".parse().unwrap()));
        let (wrong, wrong_compiled) = carry(routine, true);
        assert!(
            wrong
                .before
                .project(&changes, &wrong_compiled, &wrong.transitions)
                .is_err(),
            "a routine-owned reference rode on the table (column={column})"
        );
    }
}

#[test]
fn plain_column_renames_require_opening_and_closing_inventories() {
    rename_endpoints(true);
}

#[test]
fn transitions_cannot_rewrite_an_unrelated_prerequisite() {
    let plan = super::tests::plan();
    let PlanAnalysis::Resolved(mut evidence) = plan.analysis else {
        panic!("resolved")
    };
    let external = evidence.before.prerequisites()[0].object.clone();
    let compiled = super::tests::compiled();
    assert_eq!(
        compiled
            .prerequisites()
            .iter()
            .find(|p| p.object == external)
            .unwrap()
            .properties,
        "aa".repeat(32)
    );
    evidence.transitions[0].before.insert(external.clone());
    evidence.transitions[0].after.insert(external);
    // The retained target record has a different fingerprint from scratch.
    // Claiming it under the changed view must never authorize replacing it.
    let projected = evidence
        .before
        .project(&plan.changes, &compiled, &evidence.transitions);
    assert!(
        projected.is_err(),
        "projection accepted an unrelated prerequisite"
    );
    assert!(
        ResolverEvidence::new(
            &plan.changes,
            evidence.qualification.clone(),
            evidence.authorization.clone(),
            evidence.before.clone(),
            &compiled,
            evidence.surfaces.clone(),
            evidence.transitions.clone(),
            evidence.ordering.clone(),
        )
        .is_err(),
        "constructor accepted an unrelated prerequisite"
    );
    evidence.after = compiled;
    let decoded: ResolverEvidence =
        serde_json::from_value(serde_json::to_value(evidence).unwrap()).unwrap();
    assert!(
        decoded.validate(&plan.changes).is_err(),
        "reader accepted an unrelated prerequisite"
    );
}

fn assert_projection_refuses(
    changes: &ChangeSet,
    evidence: &ResolverEvidence,
    compiled: &InputManifest,
) {
    assert!(
        evidence
            .before
            .project(changes, compiled, &evidence.transitions)
            .is_err(),
        "projection accepted unqualified or unrelated ownership"
    );
    assert!(
        ResolverEvidence::new(
            changes,
            evidence.qualification.clone(),
            evidence.authorization.clone(),
            evidence.before.clone(),
            compiled,
            evidence.surfaces.clone(),
            evidence.transitions.clone(),
            evidence.ordering.clone()
        )
        .is_err(),
        "constructor accepted unqualified or unrelated ownership"
    );
}

fn assert_reader_refuses(changes: &ChangeSet, evidence: ResolverEvidence) {
    let decoded: ResolverEvidence =
        serde_json::from_value(serde_json::to_value(evidence).unwrap()).unwrap();
    assert!(
        decoded.validate(changes).is_err(),
        "reader accepted unqualified or unrelated ownership"
    );
}

#[test]
fn opening_inventory_requires_its_own_qualified_ownership() {
    let (change, surface) = dropped_surfaces().remove(0);
    let (changes, evidence) = drop_fixture(change, surface);
    for ownership in [
        ObjectOwnership::Unqualified,
        ObjectOwnership::Surface(Surface::Module("other.v".parse().unwrap())),
    ] {
        let mut wrong = evidence.clone();
        let mut before = serde_json::to_value(&wrong.before).unwrap();
        before["prerequisites"][0]["ownership"] = serde_json::to_value(ownership).unwrap();
        wrong.before = serde_json::from_value(before).unwrap();
        // A drop installs nothing: the compiled manifest is the closing one.
        let compiled = wrong.after.clone();
        assert_projection_refuses(&changes, &wrong, &compiled);
        assert_reader_refuses(&changes, wrong);
    }
}

// Closing ownership is the compiled records' ownership. The reader holds
// none of them, so only projection and the constructor can refuse it.
#[test]
fn closing_inventory_requires_its_own_qualified_ownership() {
    let plan = super::tests::plan();
    let PlanAnalysis::Resolved(evidence) = plan.analysis else {
        panic!("resolved")
    };
    let view = super::tests::compiled()
        .prerequisites()
        .iter()
        .position(|p| evidence.transitions[0].after.contains(&p.object))
        .unwrap();
    for ownership in [
        ObjectOwnership::Unqualified,
        ObjectOwnership::Surface(Surface::Module("app.other".parse().unwrap())),
    ] {
        let mut compiled = serde_json::to_value(super::tests::compiled()).unwrap();
        compiled["prerequisites"][view]["ownership"] = serde_json::to_value(ownership).unwrap();
        let compiled: InputManifest = serde_json::from_value(compiled).unwrap();
        assert_projection_refuses(&plan.changes, &evidence, &compiled);
    }
}

#[test]
fn proved_internal_objects_are_allowed_but_referenced_objects_are_not_owned() {
    let plan = super::tests::plan();
    let PlanAnalysis::Resolved(mut evidence) = plan.analysis else {
        panic!("resolved")
    };
    let owner = evidence.transitions[0].surface.clone();
    // The pure contract accepts an adapter's opaque internal identity; neither
    // its spelling nor a dependency edge establishes ownership by itself.
    let internal = ObjectIdentity {
        class: "adapter-internal-type".into(),
        name: vec!["opaque".into()],
        signature: vec![],
    };
    let compiled = super::tests::compiled();
    let mut records = compiled.prerequisites().to_vec();
    records.push(crate::resolver::Prerequisite {
        object: internal.clone(),
        ownership: ObjectOwnership::Surface(owner),
        canonicalization: "fixture-v1".into(),
        properties: "ee".repeat(32),
        bindings: vec![],
    });
    records.sort_by(|a, b| a.object.cmp(&b.object));
    let mut json = serde_json::to_value(&compiled).unwrap();
    json["prerequisites"] = serde_json::to_value(records).unwrap();
    let mut compiled: InputManifest = serde_json::from_value(json).unwrap();
    evidence.transitions[0].after.insert(internal.clone());
    evidence.after = evidence
        .before
        .project(&plan.changes, &compiled, &evidence.transitions)
        .unwrap();
    evidence.validate(&plan.changes).unwrap();
    // A closing record's ownership is the compiled capture's, which only
    // sealing holds: the reader cannot refuse it.
    own(
        &mut compiled,
        &internal,
        Surface::Module("app.external".parse().unwrap()),
    );
    assert_projection_refuses(&plan.changes, &evidence, &compiled);
}

// Construct the wrong closing state caused by omitting exactly this removed
// record: it survives. Membership follows that state so the reader regression
// cannot pass merely because the fixture is malformed. An omitted installed
// record has no reader-side counterpart: the reader holds no compiled records.
fn omitted_closing_record(
    before: &InputManifest,
    after: &InputManifest,
    object: &ObjectIdentity,
) -> InputManifest {
    let mut records = after.prerequisites().to_vec();
    records.push(
        before
            .prerequisites()
            .iter()
            .find(|p| &p.object == object)
            .unwrap()
            .clone(),
    );
    records.sort_by(|a, b| a.object.cmp(&b.object));
    let mut membership = after.membership().to_vec();
    for (result, opening) in membership.iter_mut().zip(before.membership()) {
        if opening.members.contains(object) {
            result.members.insert(object.clone());
        }
    }
    let mut json = serde_json::to_value(after).unwrap();
    json["prerequisites"] = serde_json::to_value(records).unwrap();
    json["membership"] = serde_json::to_value(membership).unwrap();
    serde_json::from_value(json).unwrap()
}

fn aggregate_records(change: Change, owner: Surface, child: Surface, creating: bool) {
    let (changes, evidence, compiled) = owner_coverage(change, owner.clone(), child, creating);
    let owner_index = evidence
        .transitions
        .iter()
        .position(|transition| transition.surface == owner)
        .unwrap();
    let inventory = if creating {
        &evidence.transitions[owner_index].after
    } else {
        &evidence.transitions[owner_index].before
    };
    let manifest = if creating {
        &compiled
    } else {
        &evidence.before
    };
    let mut inventory: Vec<_> = inventory.iter().collect();
    inventory.sort_by_key(|object| {
        !manifest
            .prerequisites()
            .iter()
            .any(|p| &p.object == *object && p.ownership == ObjectOwnership::Surface(owner.clone()))
    });
    // Omit either the owner or its child. Binding observations are deliberately
    // absent: plain columns/defaults must not depend on an expression witness.
    for object in inventory {
        let mut omitted = evidence.clone();
        omitted.surfaces.clear();
        if creating {
            assert!(omitted.transitions[owner_index].after.remove(object));
        } else {
            assert!(omitted.transitions[owner_index].before.remove(object));
        }
        assert!(
            evidence
                .before
                .project(&changes, &compiled, &omitted.transitions)
                .is_err(),
            "aggregate projection accepted an omitted owned record"
        );
        assert!(
            ResolverEvidence::new(
                &changes,
                evidence.qualification.clone(),
                evidence.authorization.clone(),
                evidence.before.clone(),
                &compiled,
                vec![],
                omitted.transitions.clone(),
                evidence.ordering.clone(),
            )
            .is_err(),
            "aggregate constructor accepted an omitted owned record"
        );
        // The reader replays from the sealed closing manifest. Its typed
        // lifecycle change independently requires an owner even when that
        // owner was omitted from both the manifest and the transition. Child
        // capture completeness itself remains the adapter's qualification.
        // A created owner is a closing record, which the reader cannot see.
        if creating
            || !manifest.prerequisites().iter().any(|p| {
                &p.object == object && p.ownership == ObjectOwnership::Surface(owner.clone())
            })
        {
            continue;
        }
        omitted.after = omitted_closing_record(&evidence.before, &evidence.after, object);
        let decoded: ResolverEvidence =
            serde_json::from_value(serde_json::to_value(omitted).unwrap()).unwrap();
        assert!(
            decoded.validate(&changes).is_err(),
            "aggregate reader accepted an omitted owned record"
        );
    }
}

#[test]
fn aggregate_table_drops_require_all_owned_records() {
    let table: TableName = "app.v".parse().unwrap();
    aggregate_records(
        Change::DropTable {
            uid: "t_000000".parse().unwrap(),
            name: table.clone(),
            detach_from: None,
        },
        Surface::Table(table.clone()),
        Surface::Default(table.column("n")),
        false,
    );
}

#[test]
fn aggregate_table_creation_requires_all_owned_records() {
    let table: TableName = "app.v".parse().unwrap();
    let definition = serde_json::from_value(serde_json::json!({
        "columns": {"n": {"type":"integer","nullable":true,"default":"7"}}
    }))
    .unwrap();
    aggregate_records(
        Change::CreateTable {
            uid: "t_000000".parse().unwrap(),
            name: table.clone(),
            table: Box::new(definition),
        },
        Surface::Table(table.clone()),
        Surface::Default(table.column("n")),
        true,
    );
}

#[test]
fn aggregate_column_changes_require_all_owned_records() {
    let table: TableName = "app.v".parse().unwrap();
    let column =
        serde_json::from_value(serde_json::json!({"type":"integer","nullable":true,"default":"7"}))
            .unwrap();
    for (change, creating) in [
        (
            Change::AddColumn {
                uid: "c_000000".parse().unwrap(),
                table: table.clone(),
                name: "n".into(),
                column: Box::new(column),
            },
            true,
        ),
        (
            Change::DropColumn {
                uid: "c_000000".parse().unwrap(),
                column: table.column("n"),
            },
            false,
        ),
    ] {
        aggregate_records(
            change,
            Surface::Column(table.column("n")),
            Surface::Default(table.column("n")),
            creating,
        );
    }
}

#[test]
fn aggregate_renames_require_owned_records_at_both_endpoints() {
    for column in [false, true] {
        let (changes, mut evidence, mut compiled) = rename_endpoints(column);
        let source = match &changes.changes[0].change {
            Change::RenameColumn { table, from, .. } => Surface::Column(table.column(from)),
            Change::RenameTable { from, .. } => Surface::Table(from.clone()),
            Change::CreateTable { .. }
            | Change::DetachPartition { .. }
            | Change::DropTable { .. }
            | Change::AddColumn { .. }
            | Change::AddComputedColumn { .. }
            | Change::SetReplicaIdentity { .. }
            | Change::SetStorageParameters { .. }
            | Change::SetTablePersistence { .. }
            | Change::SetPartitionDefault { .. }
            | Change::SetPartitionNotNull { .. }
            | Change::SetIndexStorageParameters { .. }
            | Change::DropComputedColumn { .. }
            | Change::DropColumn { .. }
            | Change::AlterColumnType { .. }
            | Change::AlterColumnNullability { .. }
            | Change::AlterColumnDefault { .. }
            | Change::AlterColumnExpression { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::SetPrimaryKey { .. }
            | Change::AddUnique { .. }
            | Change::DropUnique { .. }
            | Change::AddForeignKey { .. }
            | Change::DropForeignKey { .. }
            | Change::AddCheck { .. }
            | Change::DropCheck { .. }
            | Change::AddIndex { .. }
            | Change::DropIndex { .. }
            | Change::InsertRow { .. }
            | Change::UpdateRow { .. }
            | Change::DeleteRow { .. }
            | Change::SetDataMode { .. }
            | Change::CreateModule { .. }
            | Change::AlterModule { .. }
            | Change::DropModule { .. }
            | Change::CreateRole { .. }
            | Change::DropRole { .. }
            | Change::RenameRole { .. }
            | Change::Grant { .. }
            | Change::Revoke { .. }
            | Change::PublicExecution { .. } => panic!("rename fixture"),
        };
        let rename_index = evidence
            .transitions
            .iter()
            .position(|transition| transition.surface == source)
            .unwrap();
        let old = evidence.transitions[rename_index]
            .before
            .iter()
            .next()
            .unwrap()
            .clone();
        let new = evidence.transitions[rename_index]
            .after
            .iter()
            .next()
            .unwrap()
            .clone();
        let mut children = Vec::new();
        for (manifest, object) in [(&mut evidence.before, &old), (&mut compiled, &new)] {
            let owner = manifest
                .prerequisites()
                .iter()
                .find(|p| &p.object == object)
                .unwrap()
                .ownership
                .clone();
            let ObjectOwnership::Surface(surface) = owner else {
                panic!("qualified")
            };
            let child_owner = match surface {
                Surface::Table(t) => Surface::Default(t.column("n")),
                Surface::Column(c) => Surface::Default(c),
                Surface::Namespace(_)
                | Surface::Default(_)
                | Surface::Check { .. }
                | Surface::Index { .. }
                | Surface::Module(_) => panic!("table or column"),
            };
            let child = ObjectIdentity {
                class: "adapter-owned-default".into(),
                name: vec![],
                signature: vec![object.clone()],
            };
            let mut records = manifest.prerequisites().to_vec();
            records.push(crate::resolver::Prerequisite {
                object: child.clone(),
                ownership: ObjectOwnership::Surface(child_owner),
                canonicalization: "fixture-v1".into(),
                properties: "ee".repeat(32),
                bindings: vec![],
            });
            records.sort_by(|a, b| a.object.cmp(&b.object));
            let mut json = serde_json::to_value(&*manifest).unwrap();
            json["prerequisites"] = serde_json::to_value(records).unwrap();
            *manifest = serde_json::from_value(json).unwrap();
            children.push(child);
        }
        evidence.transitions[rename_index]
            .before
            .insert(children[0].clone());
        evidence.transitions[rename_index]
            .after
            .insert(children[1].clone());
        evidence.after = evidence
            .before
            .project(&changes, &compiled, &evidence.transitions)
            .unwrap();
        evidence.validate(&changes).unwrap();
        for omit_before in [true, false] {
            let mut omitted = evidence.clone();
            if omit_before {
                omitted.transitions[rename_index].before.remove(&old);
            } else {
                omitted.transitions[rename_index].after.remove(&new);
            }
            assert!(
                evidence
                    .before
                    .project(&changes, &compiled, &omitted.transitions)
                    .is_err(),
                "aggregate rename accepted a child in place of its owner"
            );
            // The closing owner is a compiled record the reader cannot see.
            if !omit_before {
                continue;
            }
            omitted.after = omitted_closing_record(&evidence.before, &evidence.after, &old);
            let decoded: ResolverEvidence =
                serde_json::from_value(serde_json::to_value(omitted).unwrap()).unwrap();
            assert!(
                decoded.validate(&changes).is_err(),
                "aggregate rename reader accepted a missing owner"
            );
        }
    }
}

// Both opaque records belong to the changed scope: a directly owned internal
// record, or a child the mutation affects. Neither can hide an omitted owner.
fn mutation_inventory(change: Change, owner: Surface, child: Surface) {
    let table: TableName = "app.v".parse().unwrap();
    let (_, mut evidence, _) = owner_coverage(
        Change::DropTable {
            uid: "t_000000".parse().unwrap(),
            name: table.clone(),
            detach_from: None,
        },
        Surface::Table(table.clone()),
        Surface::Default(table.column("n")),
        false,
    );
    let root = evidence
        .before
        .prerequisites()
        .iter()
        .find(|p| p.ownership == ObjectOwnership::Surface(Surface::Table(table.clone())))
        .unwrap()
        .object
        .clone();
    let subordinate = evidence.transitions[0]
        .before
        .iter()
        .find(|id| **id != root)
        .unwrap()
        .clone();
    own(&mut evidence.before, &root, owner.clone());
    own(&mut evidence.before, &subordinate, child);
    let mut compiled = serde_json::to_value(&evidence.before).unwrap();
    for p in compiled["prerequisites"].as_array_mut().unwrap() {
        if p["object"] == serde_json::to_value(&root).unwrap() {
            p["properties"] = serde_json::json!("ab".repeat(32));
        }
    }
    let compiled: InputManifest = serde_json::from_value(compiled).unwrap();
    let changes = ChangeSet {
        changes: vec![PlannedChange::new(change)],
    };
    evidence.surfaces.clear();
    if matches!(owner, Surface::Module(_)) {
        let observed = crate::resolver::BoundSurface {
            object: root.clone(),
            bindings: vec![],
            managed_inputs: BTreeSet::new(),
        };
        evidence.surfaces.push(SurfaceResolution {
            surface: owner.clone(),
            current: Some(observed.clone()),
            desired: Some(observed),
        });
    }
    evidence.transitions[0].surface = owner;
    evidence.transitions[0].after = evidence.transitions[0].before.clone();
    evidence.after = evidence
        .before
        .project(&changes, &compiled, &evidence.transitions)
        .unwrap();
    evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
    if matches!(
        changes.changes[0].change,
        Change::Grant { .. } | Change::Revoke { .. }
    ) {
        evidence.authorization.changes = BTreeSet::from([0]);
    }
    evidence.validate(&changes).unwrap();
    super::tests::assert_closing(&evidence, &compiled);
    for missing in [&root, &subordinate] {
        let mut omitted = evidence.clone();
        omitted.transitions[0].before.remove(missing);
        omitted.transitions[0].after.remove(missing);
        assert!(
            evidence
                .before
                .project(&changes, &compiled, &omitted.transitions)
                .is_err(),
            "mutation projection accepted an omitted owned record"
        );
        assert!(
            ResolverEvidence::new(
                &changes,
                evidence.qualification.clone(),
                evidence.authorization.clone(),
                evidence.before.clone(),
                &compiled,
                evidence.surfaces.clone(),
                omitted.transitions.clone(),
                evidence.ordering.clone()
            )
            .is_err(),
            "mutation constructor accepted an omitted owned record"
        );
        // Retaining the old owner fingerprint is exactly what the bad
        // projection does; membership and identities remain well formed.
        omitted.after = evidence.before.clone();
        let decoded: ResolverEvidence =
            serde_json::from_value(serde_json::to_value(omitted).unwrap()).unwrap();
        assert!(
            decoded.validate(&changes).is_err(),
            "mutation reader accepted a stale owner"
        );
    }
}

#[test]
fn column_type_mutations_require_complete_owner_inventories() {
    let table: TableName = "app.v".parse().unwrap();
    mutation_inventory(
        Change::AlterColumnType {
            uid: "c_000000".parse().unwrap(),
            column: table.column("n"),
            from: "integer".parse().unwrap(),
            to: "bigint".parse().unwrap(),
            from_nullable: true,
            to_nullable: true,
            from_collation: None,
            to_collation: None,
        },
        Surface::Column(table.column("n")),
        Surface::Default(table.column("n")),
    );
}

#[test]
fn nullability_mutations_require_complete_owner_inventories() {
    let table: TableName = "app.v".parse().unwrap();
    for to_nullable in [false, true] {
        mutation_inventory(
            Change::AlterColumnNullability {
                uid: "c_000000".parse().unwrap(),
                column: table.column("n"),
                ty: "integer".parse().unwrap(),
                to_nullable,
                collation: None,
            },
            Surface::Column(table.column("n")),
            Surface::Column(table.column("n")),
        );
    }
}

#[test]
fn table_constraint_mutations_require_complete_owner_inventories() {
    let table: TableName = "app.v".parse().unwrap();
    for change in [
        Change::SetPrimaryKey {
            nonclustered: false,
            table: table.clone(),
            from: None,
            to: Some(serde_json::from_value(serde_json::json!({"columns":["n"]})).unwrap()),
        },
        Change::AddUnique {
            clustered: false,
            table: table.clone(),
            name: "uq".into(),
            constraint: serde_json::from_value(serde_json::json!({"columns":["n"]})).unwrap(),
        },
        Change::DropUnique {
            table: table.clone(),
            name: "uq".into(),
        },
        Change::DropForeignKey {
            table: table.clone(),
            name: "fk".into(),
        },
    ] {
        mutation_inventory(
            change,
            Surface::Table(table.clone()),
            Surface::Table(table.clone()),
        );
    }
}

#[test]
fn module_mutations_require_complete_owned_inventories() {
    let plan = super::tests::plan();
    let Change::CreateModule { id, module } = plan.changes.changes[0].change.clone() else {
        panic!("module")
    };
    let owner = Surface::Module(id.clone());
    mutation_inventory(Change::AlterModule { id, module }, owner.clone(), owner);
}

#[test]
fn mutations_of_created_or_removed_targets_use_the_planned_endpoint() {
    let table: TableName = "app.v".parse().unwrap();
    for creating in [false, true] {
        let change = if creating {
            Change::CreateTable { uid: "t_000000".parse().unwrap(), name: table.clone(),
                table: Box::new(serde_json::from_value(serde_json::json!({"columns":{"n":{"type":"integer","nullable":true,"default":"7"}}})).unwrap()) }
        } else {
            Change::DropTable {
                uid: "t_000000".parse().unwrap(),
                name: table.clone(),
                detach_from: None,
            }
        };
        let (mut changes, mut evidence, _) = owner_coverage(
            change,
            Surface::Table(table.clone()),
            Surface::Default(table.column("n")),
            creating,
        );
        let grant = Change::Grant {
            role: "reader".into(),
            target: crate::GrantTarget::Object(table.clone()),
            permissions: BTreeSet::from([crate::Permission::Select]),
        };
        if creating {
            changes.changes.push(PlannedChange::new(grant));
            changes
                .changes
                .push(PlannedChange::new(Change::SetPrimaryKey {
                    nonclustered: false,
                    table: table.clone(),
                    from: None,
                    to: Some(serde_json::from_value(serde_json::json!({"columns":["n"]})).unwrap()),
                }));
            evidence.authorization.changes = BTreeSet::from([1]);
        } else {
            changes.changes.insert(0, PlannedChange::new(grant));
            evidence.authorization.changes = BTreeSet::from([0]);
        }
        evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
        evidence.validate(&changes).unwrap();
        let mut unplanned = changes.clone();
        unplanned.changes.retain(|p| {
            !matches!(
                p.change,
                Change::CreateTable { .. } | Change::DropTable { .. }
            )
        });
        evidence.authorization.changes = BTreeSet::from([0]);
        evidence.ordering = OrderingProof::new(&unplanned, BTreeSet::new()).unwrap();
        assert!(
            evidence.validate(&unplanned).is_err(),
            "unplanned absent endpoint accepted"
        );
    }
}

/// A computed column's add or drop is a column lifecycle the projection
/// accepts as a column's is: the resolver-backed plan of a table that gains
/// or loses one is not refused as incomplete (#1174 review).
#[test]
fn a_computed_column_add_or_drop_is_a_projected_column_lifecycle() {
    let table: TableName = "app.v".parse().unwrap();
    let computed = crate::schema::ComputedColumn {
        expression: "a * 2".into(),
        persisted: false,
        not_null: false,
    };
    for (change, creating) in [
        (
            Change::AddComputedColumn {
                table: table.clone(),
                name: "n".into(),
                computed: computed.clone(),
            },
            true,
        ),
        (
            Change::DropComputedColumn {
                table: table.clone(),
                name: "n".into(),
                computed,
            },
            false,
        ),
    ] {
        let (changes, evidence, compiled) = owner_coverage(
            change,
            Surface::Column(table.column("n")),
            Surface::Column(table.column("n")),
            creating,
        );
        evidence
            .before
            .project(&changes, &compiled, &evidence.transitions)
            .expect("a computed column's lifecycle projects");
    }
}

mod inline_binding_floor {
    use super::*;
    use crate::resolver::{BoundSurface, Prerequisite};
    use crate::{Column, Generated, IdsFile, PlanOrigin, SavedPlan, Table};

    fn construct(
        changes: &ChangeSet,
        evidence: &ResolverEvidence,
        compiled: &InputManifest,
    ) -> Result<ResolverEvidence, EvidenceError> {
        ResolverEvidence::new(
            changes,
            evidence.qualification.clone(),
            evidence.authorization.clone(),
            evidence.before.clone(),
            compiled,
            evidence.surfaces.clone(),
            evidence.transitions.clone(),
            evidence.ordering.clone(),
        )
    }

    fn roundtrip(
        changes: &ChangeSet,
        evidence: &ResolverEvidence,
        compiled: &InputManifest,
    ) -> SavedPlan {
        let seed = super::super::tests::plan();
        let sealed =
            construct(changes, evidence, compiled).expect("complete inline bindings must seal");
        let plan = SavedPlan::new(
            PlanOrigin::Database,
            seed.dialect,
            seed.created_at,
            seed.baseline,
            changes.clone(),
            IdsFile::default(),
        )
        .with_resolution(sealed)
        .unwrap();
        plan.validate_analysis().unwrap();
        let decoded: SavedPlan =
            serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
        assert_eq!(decoded, plan);
        decoded.validate_analysis().unwrap();
        decoded
    }

    fn require_each(
        changes: &ChangeSet,
        evidence: &ResolverEvidence,
        compiled: &InputManifest,
        required: &[Surface],
    ) {
        let plan = roundtrip(changes, evidence, compiled);
        for missing in required {
            let mut omitted = evidence.clone();
            let at = omitted
                .surfaces
                .iter()
                .position(|s| &s.surface == missing)
                .unwrap();
            omitted.surfaces.remove(at);
            // Only the resolution is absent. Complete owner inventories,
            // transitions and the sealed order must not mask this omission.
            assert_eq!(
                omitted
                    .before
                    .project(changes, compiled, &omitted.transitions)
                    .unwrap(),
                evidence.after
            );
            assert!(
                matches!(
                    construct(changes, &omitted, compiled),
                    Err(EvidenceError::Incomplete)
                ),
                "constructor accepted missing inline resolution {missing:?}"
            );
            let mut unread = plan.clone();
            unread.analysis = PlanAnalysis::Resolved(Box::new(omitted));
            let decoded: SavedPlan =
                serde_json::from_str(&serde_json::to_string(&unread).unwrap()).unwrap();
            assert_eq!(
                decoded.validate_analysis(),
                Err(EvidenceError::Incomplete),
                "reader accepted missing inline resolution {missing:?}"
            );
        }
    }

    fn added_column(column: Column) -> (ChangeSet, ResolverEvidence, InputManifest) {
        let table: TableName = "app.v".parse().unwrap();
        owner_coverage(
            Change::AddColumn {
                uid: "c_000000".parse().unwrap(),
                table: table.clone(),
                name: "h".into(),
                column: Box::new(column),
            },
            Surface::Column(table.column("h")),
            Surface::Default(table.column("h")),
            true,
        )
    }

    #[test]
    fn generated_and_ordinary_additions_require_their_inline_attrdef_resolution() {
        for generated in [false, true] {
            let mut column = Column::new("integer".parse().unwrap());
            if generated {
                column.generated = Some(Generated {
                    expression: "1 + 4".into(),
                    stored: true,
                });
                assert!(column.default.is_none());
            } else {
                column.default = Some("7".into());
            }
            let (changes, evidence, compiled) = added_column(column);
            require_each(
                &changes,
                &evidence,
                &compiled,
                &[Surface::Default("app.v.h".parse().unwrap())],
            );
        }
    }

    fn plain_table() -> (ChangeSet, ResolverEvidence, InputManifest) {
        let name: TableName = "app.v".parse().unwrap();
        let mut table = Table::default();
        table
            .columns
            .insert("n".into(), Column::new("integer".parse().unwrap()));
        owner_coverage(
            Change::CreateTable {
                uid: "t_000000".parse().unwrap(),
                name: name.clone(),
                table: Box::new(table),
            },
            Surface::Table(name.clone()),
            Surface::Column(name.column("n")),
            true,
        )
    }

    #[test]
    fn table_creation_requires_every_distinct_inline_binding_resolution() {
        let name: TableName = "app.v".parse().unwrap();
        let (mut changes, mut evidence, compiled) = plain_table();
        let Change::CreateTable { table, .. } = &mut changes.changes[0].change else {
            panic!("create table fixture")
        };
        table.columns.get_mut("n").unwrap().default = Some("7".into());
        let mut generated = Column::new("integer".parse().unwrap());
        generated.generated = Some(Generated {
            expression: "1 + 4".into(),
            stored: true,
        });
        table.columns.insert("g".into(), generated);
        table.checks.insert(
            "positive".into(),
            crate::CheckConstraint {
                expression: "n > 0".into(),
            },
        );
        for (index_name, key, filter) in [
            (
                "expression",
                crate::IndexKey::Expression("n + 1".into()),
                None,
            ),
            (
                "filtered",
                crate::IndexKey::Column("n".into()),
                Some("n > 0".into()),
            ),
            ("plain", crate::IndexKey::Column("n".into()), None),
        ] {
            table.indexes.insert(
                index_name.into(),
                crate::Index {
                    columns: vec![crate::IndexColumn {
                        key,
                        descending: false,
                        opclass: None,
                    }],
                    include: vec![],
                    unique: false,
                    filter,
                    method: Default::default(),
                    storage_parameters: Default::default(),
                },
            );
        }
        let required = [
            Surface::Default(name.column("n")),
            Surface::Default(name.column("g")),
            Surface::Check {
                table: name.clone(),
                name: "positive".into(),
            },
            Surface::Index {
                table: name.clone(),
                name: "expression".into(),
            },
            Surface::Index {
                table: name.clone(),
                name: "filtered".into(),
            },
        ];
        let root = compiled
            .prerequisites()
            .iter()
            .find(|p| p.ownership == ObjectOwnership::Surface(Surface::Table(name.clone())))
            .unwrap()
            .object
            .clone();
        let mut records = compiled.prerequisites().to_vec();
        evidence.surfaces.clear();
        // Opaque model addresses carry explicit ownership. These records are
        // not PostgreSQL catalog facts or runtime qualification.
        for (ordinal, surface) in required
            .iter()
            .cloned()
            .chain([
                Surface::Column(name.column("g")),
                Surface::Index {
                    table: name,
                    name: "plain".into(),
                },
            ])
            .enumerate()
        {
            let object = ObjectIdentity {
                class: "fixture-inline-child".into(),
                name: vec![ordinal.to_string()],
                signature: vec![root.clone()],
            };
            records.push(Prerequisite {
                object: object.clone(),
                ownership: ObjectOwnership::Surface(surface.clone()),
                canonicalization: "fixture-v1".into(),
                properties: "de".repeat(32),
                bindings: vec![],
            });
            evidence.transitions[0].after.insert(object.clone());
            if required.contains(&surface) {
                evidence.surfaces.push(SurfaceResolution {
                    surface,
                    current: None,
                    desired: Some(BoundSurface {
                        object,
                        bindings: vec![],
                        managed_inputs: BTreeSet::new(),
                    }),
                });
            }
        }
        records.sort_by(|a, b| a.object.cmp(&b.object));
        let mut compiled = serde_json::to_value(&compiled).unwrap();
        compiled["prerequisites"] = serde_json::to_value(records).unwrap();
        let compiled: InputManifest = serde_json::from_value(compiled).unwrap();
        evidence.after = evidence
            .before
            .project(&changes, &compiled, &evidence.transitions)
            .unwrap();
        evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
        evidence.surfaces.sort_by(|a, b| a.surface.cmp(&b.surface));
        assert_eq!(evidence.surfaces.len(), 5);
        require_each(&changes, &evidence, &compiled, &required);
    }

    #[test]
    fn nonbinding_and_ordinary_plans_do_not_acquire_inline_resolution_requirements() {
        let (changes, mut evidence, compiled) = plain_table();
        evidence.surfaces.clear();
        roundtrip(&changes, &evidence, &compiled);

        let mut column = Column::new("integer".parse().unwrap());
        column.default = Some("7".into());
        let (mut changes, mut evidence, compiled) = added_column(column);
        let seed = super::super::tests::plan();
        let ordinary = SavedPlan::new(
            PlanOrigin::Database,
            seed.dialect,
            seed.created_at,
            seed.baseline,
            changes.clone(),
            IdsFile::default(),
        );
        let ordinary: SavedPlan =
            serde_json::from_str(&serde_json::to_string(&ordinary).unwrap()).unwrap();
        assert!(matches!(ordinary.analysis, PlanAnalysis::Ordinary));
        ordinary.validate_analysis().unwrap();

        let child = evidence.surfaces[0]
            .desired
            .as_ref()
            .unwrap()
            .object
            .clone();
        let Change::AddColumn {
            table,
            name,
            column,
            ..
        } = &mut changes.changes[0].change
        else {
            panic!("add column fixture")
        };
        let owner = Surface::Column(table.column(name.as_str()));
        column.default = None;
        let mut compiled = serde_json::to_value(&compiled).unwrap();
        compiled["prerequisites"]
            .as_array_mut()
            .unwrap()
            .retain(|p| p["object"] != serde_json::to_value(&child).unwrap());
        let compiled: InputManifest = serde_json::from_value(compiled).unwrap();
        let transition = evidence
            .transitions
            .iter_mut()
            .find(|transition| transition.surface == owner)
            .unwrap();
        assert!(transition.after.remove(&child));
        evidence.surfaces.clear();
        evidence.after = evidence
            .before
            .project(&changes, &compiled, &evidence.transitions)
            .unwrap();
        evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
        roundtrip(&changes, &evidence, &compiled);
    }
}

mod column_vector_parent {
    use super::*;
    use crate::{Column, Generated, IdsFile, PlanOrigin, SavedPlan};

    fn fixtures() -> Vec<(ChangeSet, ResolverEvidence, InputManifest)> {
        let table: TableName = "app.v".parse().unwrap();
        let mut fixtures = Vec::new();
        for kind in ["plain", "default", "generated"] {
            let mut column = Column::new("integer".parse().unwrap());
            let child = if kind == "plain" {
                Surface::Column(table.column("n"))
            } else {
                if kind == "default" {
                    column.default = Some("7".into());
                } else {
                    column.generated = Some(Generated {
                        expression: "1 + 4".into(),
                        stored: true,
                    });
                }
                Surface::Default(table.column("n"))
            };
            let (changes, mut evidence, compiled) = owner_coverage(
                Change::AddColumn {
                    uid: "c_000000".parse().unwrap(),
                    table: table.clone(),
                    name: "n".into(),
                    column: Box::new(column),
                },
                Surface::Column(table.column("n")),
                child,
                true,
            );
            if kind == "plain" {
                evidence.surfaces.clear();
            }
            fixtures.push((changes, evidence, compiled));
        }
        fixtures.push(owner_coverage(
            Change::DropColumn {
                uid: "c_000000".parse().unwrap(),
                column: table.column("n"),
            },
            Surface::Column(table.column("n")),
            Surface::Default(table.column("n")),
            false,
        ));
        fixtures.push(rename_endpoints(true));
        fixtures
    }

    fn construct(
        changes: &ChangeSet,
        evidence: &ResolverEvidence,
        compiled: &InputManifest,
    ) -> Result<ResolverEvidence, EvidenceError> {
        ResolverEvidence::new(
            changes,
            evidence.qualification.clone(),
            evidence.authorization.clone(),
            evidence.before.clone(),
            compiled,
            evidence.surfaces.clone(),
            evidence.transitions.clone(),
            evidence.ordering.clone(),
        )
    }

    // A closing-side defect is refused while sealing, where the compiled
    // records are at hand. The reader holds none of them, so it can refuse
    // only what the opening manifest and the typed plan prove.
    fn refuse_sealing(
        changes: &ChangeSet,
        evidence: &ResolverEvidence,
        compiled: &InputManifest,
        reason: &str,
    ) {
        assert!(
            evidence
                .before
                .project(changes, compiled, &evidence.transitions)
                .is_err(),
            "projection accepted {reason}"
        );
        assert!(
            construct(changes, evidence, compiled).is_err(),
            "constructor accepted {reason}"
        );
    }

    fn refuse(
        changes: &ChangeSet,
        evidence: &ResolverEvidence,
        compiled: &InputManifest,
        reason: &str,
    ) {
        refuse_sealing(changes, evidence, compiled, reason);
        let decoded: ResolverEvidence =
            serde_json::from_value(serde_json::to_value(evidence).unwrap()).unwrap();
        assert!(
            decoded.validate(changes).is_err(),
            "reader accepted {reason}"
        );
    }

    #[test]
    fn column_vector_edits_require_independent_parent_and_column_inventories() {
        for (changes, evidence, compiled) in fixtures() {
            let seed = super::super::tests::plan();
            let plan = SavedPlan::new(
                PlanOrigin::Database,
                seed.dialect,
                seed.created_at,
                seed.baseline,
                changes.clone(),
                IdsFile::default(),
            )
            .with_resolution(construct(&changes, &evidence, &compiled).unwrap())
            .unwrap();
            let decoded: SavedPlan =
                serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
            assert_eq!(decoded, plan);
            decoded.validate_analysis().unwrap();
            let parent = evidence
                .transitions
                .iter()
                .position(|t| matches!(t.surface, Surface::Table(_)))
                .unwrap();
            let column = evidence
                .transitions
                .iter()
                .position(|t| matches!(t.surface, Surface::Column(_)))
                .unwrap();
            assert_eq!(evidence.transitions[parent].before.len(), 1);
            assert_eq!(evidence.transitions[parent].after.len(), 1);
            // Identical parent fingerprints are deliberate: neither a hash
            // mismatch nor a binding failure can explain these refusals.
            for missing in [parent, column] {
                let mut omitted = evidence.clone();
                let removed = omitted.transitions.remove(missing);
                // A plain added column's inventory removes nothing and no
                // surface observes it: only its compiled records, which
                // the reader does not hold, prove it was omitted.
                if removed.before.is_empty() && evidence.surfaces.is_empty() {
                    refuse_sealing(
                        &changes,
                        &omitted,
                        &compiled,
                        "one independent inventory omitted",
                    );
                } else {
                    refuse(
                        &changes,
                        &omitted,
                        &compiled,
                        "one independent inventory omitted",
                    );
                }
            }
            for before in [true, false] {
                let mut omitted = evidence.clone();
                if before {
                    omitted.transitions[parent].before.clear();
                } else {
                    omitted.transitions[parent].after.clear();
                }
                // The reader sees an omitted closing parent only through
                // the placeholder a candidate membership keeps for it; a
                // renamed column's parent is named by nothing.
                let referenced = evidence
                    .after
                    .prerequisites()
                    .iter()
                    .any(|p| evidence.transitions[parent].after.contains(&p.object));
                if before || referenced {
                    refuse(
                        &changes,
                        &omitted,
                        &compiled,
                        "one surviving parent endpoint omitted",
                    );
                } else {
                    refuse_sealing(
                        &changes,
                        &omitted,
                        &compiled,
                        "one surviving parent endpoint omitted",
                    );
                }
            }
            let mut duplicate = evidence.clone();
            duplicate
                .transitions
                .push(evidence.transitions[parent].clone());
            refuse(
                &changes,
                &duplicate,
                &compiled,
                "duplicate parent transition",
            );
        }
    }

    #[test]
    fn a_column_vector_parent_cannot_authorize_unrelated_records() {
        let (changes, evidence, compiled) = fixtures().remove(0);
        let parent = evidence
            .transitions
            .iter()
            .position(|t| matches!(t.surface, Surface::Table(_)))
            .unwrap();
        let table: TableName = "app.v".parse().unwrap();
        for ownership in [
            ObjectOwnership::Surface(Surface::Column(table.column("unrelated"))),
            ObjectOwnership::Surface(Surface::Table("app.other".parse().unwrap())),
            ObjectOwnership::Unqualified,
        ] {
            let mut excessive = evidence.clone();
            let mut compiled = compiled.clone();
            let object = ObjectIdentity {
                class: "unrelated-fixture".into(),
                name: vec!["untouched".into()],
                signature: vec![],
            };
            for manifest in [&mut excessive.before, &mut excessive.after, &mut compiled] {
                let mut records = manifest.prerequisites().to_vec();
                records.push(crate::resolver::Prerequisite {
                    object: object.clone(),
                    ownership: ownership.clone(),
                    canonicalization: "fixture-v1".into(),
                    properties: "cf".repeat(32),
                    bindings: vec![],
                });
                records.sort_by(|a, b| a.object.cmp(&b.object));
                let mut json = serde_json::to_value(&*manifest).unwrap();
                json["prerequisites"] = serde_json::to_value(records).unwrap();
                *manifest = serde_json::from_value(json).unwrap();
            }
            // The closing read rereads every record its manifest lists.
            let mut json = serde_json::to_value(&excessive.after).unwrap();
            json["scope"]["retained"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::to_value(&object).unwrap());
            excessive.after = serde_json::from_value(json).unwrap();
            excessive.validate(&changes).unwrap();
            excessive.transitions[parent].before.insert(object.clone());
            excessive.transitions[parent].after.insert(object);
            refuse(
                &changes,
                &excessive,
                &compiled,
                "parent escalation to unrelated authority",
            );
        }
    }

    #[test]
    fn a_missing_vector_parent_endpoint_needs_its_own_table_lifecycle_change() {
        let table: TableName = "app.v".parse().unwrap();
        for creating in [true, false] {
            let (mut changes, mut evidence, mut compiled) = if creating {
                fixtures().remove(0)
            } else {
                fixtures().remove(3)
            };
            let parent = evidence
                .transitions
                .iter()
                .position(|t| matches!(t.surface, Surface::Table(_)))
                .unwrap();
            // The parent is absent at the opening snapshot, or compiles to
            // nothing at the closing one.
            let manifest = if creating {
                &mut evidence.before
            } else {
                &mut compiled
            };
            let object = evidence.transitions[parent]
                .after
                .iter()
                .next()
                .unwrap()
                .clone();
            let mut json = serde_json::to_value(&*manifest).unwrap();
            json["prerequisites"]
                .as_array_mut()
                .unwrap()
                .retain(|p| p["object"] != serde_json::to_value(&object).unwrap());
            for membership in json["membership"].as_array_mut().unwrap() {
                membership["members"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|member| member != &serde_json::to_value(&object).unwrap());
            }
            *manifest = serde_json::from_value(json).unwrap();
            if creating {
                evidence.transitions[parent].before.clear();
            } else {
                evidence.transitions[parent].after.clear();
            }
            if creating {
                refuse(
                    &changes,
                    &evidence,
                    &compiled,
                    "absent parent without its lifecycle change",
                );
            } else {
                refuse_sealing(
                    &changes,
                    &evidence,
                    &compiled,
                    "absent parent without its lifecycle change",
                );
            }
            let lifecycle = if creating {
                Change::CreateTable {
                    uid: "t_000000".parse().unwrap(),
                    name: table.clone(),
                    table: Box::default(),
                }
            } else {
                Change::DropTable {
                    uid: "t_000000".parse().unwrap(),
                    name: table.clone(),
                    detach_from: None,
                }
            };
            if creating {
                changes.changes.insert(0, PlannedChange::new(lifecycle));
            } else {
                changes.changes.push(PlannedChange::new(lifecycle));
            }
            evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
            construct(&changes, &evidence, &compiled)
                .unwrap()
                .validate(&changes)
                .unwrap();
        }
    }

    #[test]
    fn nonvector_edits_cannot_borrow_a_parent_transition() {
        let table: TableName = "app.v".parse().unwrap();
        let (_, fixture, _) = fixtures().remove(3);
        let parent = fixture
            .transitions
            .iter()
            .find(|t| matches!(t.surface, Surface::Table(_)))
            .unwrap()
            .clone();
        let column = fixture
            .before
            .prerequisites()
            .iter()
            .find(|p| p.ownership == ObjectOwnership::Surface(Surface::Column(table.column("n"))))
            .unwrap()
            .object
            .clone();
        let default = fixture
            .before
            .prerequisites()
            .iter()
            .find(|p| p.ownership == ObjectOwnership::Surface(Surface::Default(table.column("n"))))
            .unwrap()
            .object
            .clone();
        let uid: crate::Uid = "c_000000".parse().unwrap();
        for (change, surface, inventory) in [
            (
                Change::AlterColumnType {
                    uid: uid.clone(),
                    column: table.column("n"),
                    from: "integer".parse().unwrap(),
                    to: "bigint".parse().unwrap(),
                    from_nullable: true,
                    to_nullable: true,
                    from_collation: None,
                    to_collation: None,
                },
                Surface::Column(table.column("n")),
                BTreeSet::from([column.clone(), default.clone()]),
            ),
            (
                Change::AlterColumnNullability {
                    uid: uid.clone(),
                    column: table.column("n"),
                    ty: "integer".parse().unwrap(),
                    to_nullable: false,
                    collation: None,
                },
                Surface::Column(table.column("n")),
                BTreeSet::from([column.clone()]),
            ),
            (
                Change::AlterColumnDefault {
                    uid: uid.clone(),
                    column: table.column("n"),
                    from: Some("7".into()),
                    to: Some("8".into()),
                },
                Surface::Default(table.column("n")),
                BTreeSet::from([default.clone()]),
            ),
            (
                Change::AlterColumnExpression {
                    uid,
                    column: table.column("n"),
                    from: "1 + 4".into(),
                    to: "1 + 5".into(),
                },
                Surface::Default(table.column("n")),
                BTreeSet::from([default.clone()]),
            ),
        ] {
            let mut evidence = fixture.clone();
            // An in-place edit compiles to the same records it opens with.
            let compiled = evidence.before.clone();
            evidence.surfaces[0].desired = evidence.surfaces[0].current.clone();
            evidence.transitions = vec![ObjectTransition {
                references: BTreeSet::new(),
                surface,
                before: inventory.clone(),
                after: inventory,
            }];
            let changes = ChangeSet {
                changes: vec![PlannedChange::new(change)],
            };
            evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
            evidence = construct(&changes, &evidence, &compiled).unwrap();
            evidence.validate(&changes).unwrap();
            evidence.transitions.push(parent.clone());
            refuse(
                &changes,
                &evidence,
                &compiled,
                "parent authority for a nonvector edit",
            );
        }
    }
}
