use super::transition_scope_tests::{fixture, object, refuses, seal};
use super::*;
use crate::resolver::ObjectOwnership;
use crate::{GrantTarget, Permission, PlannedChange, TableName};

fn table() -> TableName {
    "app.v".parse().unwrap()
}

fn grant(revoke: bool) -> Change {
    let role = "reader".into();
    let target = GrantTarget::Object(table());
    let permissions = BTreeSet::from([Permission::Select]);
    if revoke {
        Change::Revoke {
            role,
            target,
            permissions,
        }
    } else {
        Change::Grant {
            role,
            target,
            permissions,
        }
    }
}

fn owner_fixture(change: Change, root: &str) -> (ChangeSet, ResolverEvidence) {
    let (_, mut evidence) = fixture();
    let changes = ChangeSet {
        changes: vec![PlannedChange::new(change)],
    };
    evidence.surfaces.clear();
    evidence.transitions[0].before = BTreeSet::from([object(root), object("internal")]);
    evidence.transitions[0].after = evidence.transitions[0].before.clone();
    evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
    if matches!(
        changes.changes[0].change,
        Change::Grant { .. } | Change::Revoke { .. }
    ) {
        evidence.authorization.changes = BTreeSet::from([0]);
    }
    // Opaque internal records belong to the owner's exact surface; separately
    // owned columns/defaults/checks/indexes in fixture() are independent inputs.
    for manifest in [&mut evidence.before, &mut evidence.after] {
        let mut records = manifest.prerequisites().to_vec();
        let mut internal = records
            .iter()
            .find(|p| p.object == object(root))
            .unwrap()
            .clone();
        internal.object = object("internal");
        records.push(internal);
        records.sort_by(|a, b| a.object.cmp(&b.object));
        let mut json = serde_json::to_value(&*manifest).unwrap();
        json["prerequisites"] = serde_json::to_value(records).unwrap();
        *manifest = serde_json::from_value(json).unwrap();
    }
    (changes, evidence)
}

fn preserves_children(changes: &ChangeSet, evidence: &ResolverEvidence) {
    let projected = seal(changes, evidence).expect("owner-only valid mutation was refused");
    projected.validate(changes).unwrap();
    for record in projected.after.prerequisites() {
        let source = if evidence.transitions[0].after.contains(&record.object) {
            &evidence.after
        } else {
            &evidence.before
        };
        assert_eq!(
            Some(record),
            source
                .prerequisites()
                .iter()
                .find(|p| p.object == record.object)
        );
    }
    for name in [
        "column",
        "changed-default",
        "sibling-column",
        "sibling-default",
        "check",
        "index",
    ] {
        if evidence.transitions[0].before.contains(&object(name)) {
            continue;
        }
        let mut wrong = evidence.clone();
        wrong.transitions[0].before.insert(object(name));
        wrong.transitions[0].after.insert(object(name));
        refuses(changes, &wrong);
    }
}

#[test]
fn table_authorization_mutations_preserve_unchanged_child_fingerprints() {
    for revoke in [false, true] {
        let (changes, evidence) = owner_fixture(grant(revoke), "table");
        preserves_children(&changes, &evidence);
    }
}

#[test]
fn table_constraint_mutations_preserve_unchanged_child_fingerprints() {
    let pk = serde_json::from_value(serde_json::json!({"columns":["n"]})).unwrap();
    for change in [
        Change::SetPrimaryKey { table: table(), from: None, to: Some(pk), nonclustered: false },
        Change::SetPrimaryKey { table: table(), from: Some(serde_json::from_value(serde_json::json!({"columns":["n"]})).unwrap()), to: None, nonclustered: false },
        Change::AddUnique { table: table(), name: "uq".into(), constraint: serde_json::from_value(serde_json::json!({"columns":["n"]})).unwrap(), clustered: false },
        Change::DropUnique { table: table(), name: "uq".into() },
        Change::AddForeignKey { table: table(), name: "fk".into(), constraint: Box::new(serde_json::from_value(serde_json::json!({"columns":["n"],"references_table":"app.parent","references_columns":["id"]})).unwrap()) },
        Change::DropForeignKey { table: table(), name: "fk".into() },
    ] {
        let (changes, evidence) = owner_fixture(change, "table");
        preserves_children(&changes, &evidence);
    }
}

#[test]
fn nullability_mutations_preserve_unchanged_default_fingerprints() {
    for to_nullable in [false, true] {
        let (changes, evidence) = owner_fixture(
            Change::AlterColumnNullability {
                uid: "c_000000".parse().unwrap(),
                column: table().column("n"),
                ty: "integer".parse().unwrap(),
                to_nullable,
                collation: None,
            },
            "column",
        );
        preserves_children(&changes, &evidence);
    }
}

#[test]
fn a_table_grant_can_share_inventory_with_an_explicit_default_change() {
    let (mut changes, mut evidence) = owner_fixture(grant(false), "table");
    let (defaults, observations) = fixture();
    changes.changes.extend(defaults.changes);
    evidence.surfaces = observations.surfaces;
    evidence.transitions[0]
        .before
        .insert(object("changed-default"));
    evidence.transitions[0]
        .after
        .insert(object("changed-default"));
    evidence.ordering = OrderingProof::new(&changes, BTreeSet::new()).unwrap();
    preserves_children(&changes, &evidence);
}

#[test]
fn saved_table_grants_cannot_authorize_sibling_fingerprint_changes() {
    let (changes, mut evidence) = owner_fixture(grant(false), "table");
    let mut json = serde_json::to_value(&evidence.before).unwrap();
    // Construct the old projection's wrong closing state without calling the
    // fixed projector: all owned children change, while external inputs stay.
    for record in json["prerequisites"].as_array_mut().unwrap() {
        let owner: ObjectOwnership = serde_json::from_value(record["ownership"].clone()).unwrap();
        if matches!(owner, ObjectOwnership::Surface(_)) {
            let id = serde_json::from_value(record["object"].clone()).unwrap();
            evidence.transitions[0].before.insert(id);
            record["properties"] = serde_json::json!("bb".repeat(32));
        }
    }
    evidence.transitions[0].after = evidence.transitions[0].before.clone();
    evidence.after = serde_json::from_value(json).unwrap();
    let decoded: ResolverEvidence =
        serde_json::from_value(serde_json::to_value(evidence).unwrap()).unwrap();
    assert!(
        decoded.validate(&changes).is_err(),
        "saved grant reader accepted unrelated child changes"
    );
}

#[test]
fn owner_mutations_still_require_every_directly_owned_internal_record() {
    let (changes, evidence) = owner_fixture(grant(false), "table");
    let valid = seal(&changes, &evidence).expect("owner-only valid mutation was refused");
    valid.validate(&changes).unwrap();
    for name in ["table", "internal"] {
        for before in [false, true] {
            let mut wrong = evidence.clone();
            if before {
                wrong.transitions[0].before.remove(&object(name));
            } else {
                wrong.transitions[0].after.remove(&object(name));
            }
            assert!(
                seal(&changes, &wrong).is_err(),
                "omitted exact-owner record was accepted"
            );
        }
    }
}
