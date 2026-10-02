use super::*;
use crate::resolver::{BoundSurface, ObjectIdentity, ObjectOwnership, Prerequisite, Surface};
use crate::{PlannedChange, TableName};

pub(super) fn object(name: &str) -> ObjectIdentity {
    ObjectIdentity {
        class: "opaque-fixture-record".into(),
        name: vec![name.into()],
        signature: vec![],
    }
}

pub(super) fn fixture() -> (ChangeSet, ResolverEvidence) {
    let PlanAnalysis::Resolved(mut evidence) = super::tests::plan().analysis else {
        panic!("resolved")
    };
    let table: TableName = "app.v".parse().unwrap();
    let owner = Surface::Default(table.column("n"));
    let mut records = evidence.before.prerequisites().to_vec();
    for (name, surface) in [
        ("changed-default", owner.clone()),
        ("table", Surface::Table(table.clone())),
        ("column", Surface::Column(table.column("n"))),
        ("sibling-column", Surface::Column(table.column("other"))),
        ("sibling-default", Surface::Default(table.column("other"))),
        (
            "check",
            Surface::Check {
                table: table.clone(),
                name: "positive".into(),
            },
        ),
        (
            "index",
            Surface::Index {
                table: table.clone(),
                name: "ix".into(),
            },
        ),
    ] {
        records.push(Prerequisite {
            object: object(name),
            ownership: ObjectOwnership::Surface(surface),
            canonicalization: "fixture-v1".into(),
            properties: "aa".repeat(32),
            bindings: vec![],
        });
    }
    records.sort_by(|a, b| a.object.cmp(&b.object));
    let mut json = serde_json::to_value(&evidence.before).unwrap();
    json["prerequisites"] = serde_json::to_value(&records).unwrap();
    evidence.before = serde_json::from_value(json.clone()).unwrap();
    // Desired compilation differs even for records the plan does not change.
    // Those differences must never become authority to rewrite target inputs.
    for record in json["prerequisites"].as_array_mut().unwrap() {
        record["properties"] = serde_json::json!("bb".repeat(32));
    }
    evidence.after = serde_json::from_value(json).unwrap();
    let changes = ChangeSet {
        changes: vec![PlannedChange::new(Change::AlterColumnDefault {
            uid: "c_000000".parse().unwrap(),
            column: table.column("n"),
            from: Some("1".into()),
            to: Some("2".into()),
        })],
    };
    let bound = BoundSurface {
        object: object("changed-default"),
        bindings: vec![],
        managed_inputs: BTreeSet::new(),
    };
    evidence.surfaces = vec![SurfaceResolution {
        surface: owner,
        current: Some(bound.clone()),
        desired: Some(bound),
    }];
    evidence.transitions = vec![ObjectTransition {
        surface: Surface::Table(table),
        before: BTreeSet::from([object("changed-default")]),
        after: BTreeSet::from([object("changed-default")]),
    }];
    evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
    (changes, *evidence)
}

pub(super) fn seal(
    changes: &ChangeSet,
    evidence: &ResolverEvidence,
) -> Result<ResolverEvidence, EvidenceError> {
    ResolverEvidence::new(
        changes,
        evidence.qualification.clone(),
        evidence.authorization.clone(),
        evidence.before.clone(),
        &evidence.after,
        evidence.surfaces.clone(),
        evidence.transitions.clone(),
        evidence.ordering.clone(),
    )
}

pub(super) fn refuses(changes: &ChangeSet, evidence: &ResolverEvidence) {
    assert!(
        evidence
            .before
            .project(changes, &evidence.after, &evidence.transitions)
            .is_err(),
        "projection accepted an unaffected record"
    );
    assert!(
        seal(changes, evidence).is_err(),
        "constructor accepted an unaffected record"
    );
    let decoded: ResolverEvidence =
        serde_json::from_value(serde_json::to_value(evidence).unwrap()).unwrap();
    assert!(
        decoded.validate(changes).is_err(),
        "reader accepted an unaffected record"
    );
}

#[test]
fn aggregate_default_changes_preserve_every_untouched_target_fingerprint() {
    let (changes, evidence) = fixture();
    let projected = seal(&changes, &evidence).unwrap();
    projected.validate(&changes).unwrap();
    // `evidence.after` is the compiled capture, whose "bb" fingerprints
    // differ for every record; none of them may reach the closing manifest.
    super::tests::assert_closing(&projected, &evidence.after);
    for name in [
        "table",
        "column",
        "sibling-column",
        "sibling-default",
        "check",
        "index",
    ] {
        let mut wrong = evidence.clone();
        wrong.transitions[0].before.insert(object(name));
        wrong.transitions[0].after.insert(object(name));
        refuses(&changes, &wrong);
    }
}

#[test]
fn column_aggregation_does_not_authorize_the_parent_of_a_changed_default() {
    let (changes, mut evidence) = fixture();
    evidence.transitions[0].surface =
        Surface::Column("app.v".parse::<TableName>().unwrap().column("n"));
    seal(&changes, &evidence)
        .unwrap()
        .validate(&changes)
        .unwrap();
    evidence.transitions[0].before.insert(object("column"));
    evidence.transitions[0].after.insert(object("column"));
    refuses(&changes, &evidence);
}

#[test]
fn aggregate_inventories_cannot_add_or_remove_unaffected_siblings() {
    let (changes, evidence) = fixture();
    for adding in [false, true] {
        let mut wrong = evidence.clone();
        let absent = if adding {
            &mut wrong.before
        } else {
            &mut wrong.after
        };
        let mut json = serde_json::to_value(&*absent).unwrap();
        json["prerequisites"]
            .as_array_mut()
            .unwrap()
            .retain(|p| p["object"] != serde_json::to_value(object("sibling-default")).unwrap());
        *absent = serde_json::from_value(json).unwrap();
        if adding {
            wrong.transitions[0].after.insert(object("sibling-default"));
        } else {
            wrong.transitions[0]
                .before
                .insert(object("sibling-default"));
        }
        refuses(&changes, &wrong);
    }
}

#[test]
fn one_aggregate_can_account_for_multiple_explicit_default_changes() {
    let (mut changes, mut evidence) = fixture();
    let column = "app.v".parse::<TableName>().unwrap().column("other");
    changes
        .changes
        .push(PlannedChange::new(Change::AlterColumnDefault {
            uid: "c_000001".parse().unwrap(),
            column: column.clone(),
            from: Some("7".into()),
            to: Some("8".into()),
        }));
    let bound = BoundSurface {
        object: object("sibling-default"),
        bindings: vec![],
        managed_inputs: BTreeSet::new(),
    };
    evidence.surfaces.push(SurfaceResolution {
        surface: Surface::Default(column),
        current: Some(bound.clone()),
        desired: Some(bound),
    });
    evidence.surfaces.sort_by(|a, b| a.surface.cmp(&b.surface));
    evidence.transitions[0]
        .before
        .insert(object("sibling-default"));
    evidence.transitions[0]
        .after
        .insert(object("sibling-default"));
    evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
    let projected = seal(&changes, &evidence).unwrap();
    projected.validate(&changes).unwrap();
    // Both changed defaults are the plan's own records and nothing names
    // them: neither is predicted, while every untouched record is kept.
    for name in ["changed-default", "sibling-default"] {
        assert!(
            !projected
                .after
                .prerequisites()
                .iter()
                .any(|p| p.object == object(name))
        );
    }
    super::tests::assert_closing(&projected, &evidence.after);
    evidence.transitions[0].before.insert(object("column"));
    evidence.transitions[0].after.insert(object("column"));
    refuses(&changes, &evidence);
}

#[test]
fn saved_aggregate_inventory_cannot_rewrite_an_untouched_sibling() {
    let (changes, evidence) = fixture();
    let valid = seal(&changes, &evidence).unwrap();
    let mut json = serde_json::to_value(&valid).unwrap();
    // Start with the correctly projected artifact, so the sole invalid fact
    // is replacing this untouched record through an overbroad inventory.
    for record in json["after"]["prerequisites"].as_array_mut().unwrap() {
        if record["object"] == serde_json::to_value(object("sibling-default")).unwrap() {
            record["properties"] = serde_json::json!("bb".repeat(32));
        }
    }
    let mut wrong: ResolverEvidence = serde_json::from_value(json).unwrap();
    wrong.transitions[0]
        .before
        .insert(object("sibling-default"));
    wrong.transitions[0].after.insert(object("sibling-default"));
    let decoded: ResolverEvidence =
        serde_json::from_value(serde_json::to_value(wrong).unwrap()).unwrap();
    assert!(
        decoded.validate(&changes).is_err(),
        "saved reader accepted an unaffected sibling"
    );
}

#[test]
fn default_inventories_respect_creation_and_removal_endpoints() {
    for creating in [false, true] {
        let (mut changes, mut evidence) = fixture();
        if let Change::AlterColumnDefault { from, to, .. } = &mut changes.changes[0].change {
            if creating {
                *from = None;
            } else {
                *to = None;
            }
        }
        evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
        // The fully populated endpoints contradict this ADD/DROP even though
        // both records have the exact changed owner and aggregate label.
        refuses(&changes, &evidence);
        let absent = if creating {
            &mut evidence.before
        } else {
            &mut evidence.after
        };
        let mut json = serde_json::to_value(&*absent).unwrap();
        json["prerequisites"]
            .as_array_mut()
            .unwrap()
            .retain(|p| p["object"] != serde_json::to_value(object("changed-default")).unwrap());
        *absent = serde_json::from_value(json).unwrap();
        if creating {
            evidence.transitions[0].before.clear();
            evidence.surfaces[0].current = None;
        } else {
            evidence.transitions[0].after.clear();
            evidence.surfaces[0].desired = None;
        }
        seal(&changes, &evidence)
            .unwrap()
            .validate(&changes)
            .unwrap();
    }
}

/// A generation expression is the column's `pg_attrdef` row, as a default is
/// (DEC-1168.1): a plan that changes one seals over that record and no other,
/// just as a default change does.
#[test]
fn a_generation_expression_change_accounts_for_its_record_and_nothing_else() {
    let (mut changes, mut evidence) = fixture();
    changes.changes[0] = PlannedChange::new(Change::AlterColumnExpression {
        uid: "c_000000".parse().unwrap(),
        column: "app.v".parse::<TableName>().unwrap().column("n"),
        from: "(a * 2)".into(),
        to: "a * 3".into(),
    });
    evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
    // Aggregated under the table, and named exactly as the record's own
    // surface, which only a change to that surface authorizes.
    let exact = {
        let mut exact = evidence.clone();
        exact.transitions[0].surface =
            Surface::Default("app.v".parse::<TableName>().unwrap().column("n"));
        exact
    };
    for sealed in [&evidence, &exact] {
        seal(&changes, sealed).unwrap().validate(&changes).unwrap();
    }
    // The record's resolution is required, as a changed default's is.
    let mut unresolved = evidence.clone();
    unresolved.surfaces.clear();
    assert!(seal(&changes, &unresolved).is_err());
    for name in ["sibling-default", "column", "check"] {
        let mut wrong = evidence.clone();
        wrong.transitions[0].before.insert(object(name));
        wrong.transitions[0].after.insert(object(name));
        refuses(&changes, &wrong);
    }
}
