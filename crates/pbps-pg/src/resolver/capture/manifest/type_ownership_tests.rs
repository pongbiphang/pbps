//! A declared enum or domain owns its `pg_type` row, its array type, its
//! labels and its constraints, matched by the recorded UID (#2407).

use super::*;
use crate::resolver::capture::ownership::{RecordedOwnership, classify};
use pbps_model::resolver::{ObjectOwnership, Surface};
use pbps_model::{IdsFile, ManagedType, Schema, TypeRef, Uid, UidKind};
use serde_json::json;

fn identity(class: &str, name: &[&str], signature: Vec<ObjectIdentity>) -> ObjectIdentity {
    ObjectIdentity {
        class: class.into(),
        name: name.iter().map(|part| (*part).into()).collect(),
        signature,
    }
}

fn input(properties: BTreeMap<String, Value>) -> Input {
    Input {
        properties,
        bindings: Vec::new(),
    }
}

fn kind(value: &str) -> Input {
    input(BTreeMap::from([("typtype".into(), json!(value))]))
}

struct Fixture {
    captured: CapturedInputs,
    root: ObjectIdentity,
    array: ObjectIdentity,
    label: ObjectIdentity,
    dependency: ObjectIdentity,
}

fn fixture(typtype: &str) -> Fixture {
    let root = identity("pg_type", &["app", "mood"], vec![]);
    let array = identity("pg_type", &["app", "_mood"], vec![]);
    let label = identity("pg_enum", &["sad"], vec![root.clone()]);
    let dependency = identity("pg_depend", &["i"], vec![array.clone(), root.clone()]);
    let deployer = identity("pg_authid", &["deployer"], vec![]);
    let captured = CapturedInputs {
        baseline: super::super::baseline::Baseline::Absent,
        session: super::super::session::Facts {
            current_role: deployer.clone(),
            session_role: deployer,
            settings: BTreeMap::new(),
        },
        rule: properties::RULE,
        major: 18,
        scope: CaptureScope {
            retained: BTreeSet::new(),
            candidates: BTreeSet::new(),
        },
        inputs: BTreeMap::from([
            (root.clone(), kind(typtype)),
            (array.clone(), kind("b")),
            (label.clone(), input(BTreeMap::new())),
            (dependency.clone(), input(BTreeMap::new())),
        ]),
        attribute_numbers: BTreeMap::new(),
        candidates: BTreeMap::new(),
        limitations: BTreeSet::new(),
        dropped: BTreeMap::new(),
    };
    Fixture {
        captured,
        root,
        array,
        label,
        dependency,
    }
}

fn ownership(
    captured: &CapturedInputs,
    declared: &[&str],
    recorded: &[&str],
) -> std::result::Result<BTreeMap<ObjectIdentity, ObjectOwnership>, Uncovered> {
    let mut schema = Schema::default();
    let mut ids = IdsFile::default();
    for name in declared {
        let name: TypeRef = name.parse().unwrap();
        schema
            .types
            .insert(name, ManagedType::Enum { values: vec![] });
    }
    for name in recorded {
        ids.types
            .insert(Uid::generate(UidKind::Type), name.parse().unwrap());
    }
    let routines = BTreeMap::new();
    let dropped = BTreeMap::new();
    let namespaces = BTreeSet::new();
    classify(
        captured,
        &RecordedOwnership {
            schema: &schema,
            ids: &ids,
            routines: &routines,
            dropped: &dropped,
            namespaces: &namespaces,
        },
    )
}

#[test]
fn a_declared_type_owns_its_row_array_labels_and_dependency_rows() {
    let owner = ObjectOwnership::Surface(Surface::Type("app.mood".parse().unwrap()));
    for typtype in ["e", "d"] {
        let f = fixture(typtype);
        let found = ownership(&f.captured, &["app.mood"], &["app.mood"]).unwrap();
        for (what, object) in [
            ("row", &f.root),
            ("array", &f.array),
            ("label", &f.label),
            ("dependency row", &f.dependency),
        ] {
            assert_eq!(found.get(object), Some(&owner), "{typtype} {what}");
        }
    }
}

/// Catalog names alone never grant ownership: an undeclared type of that
/// name, a type whose declaration has no recorded UID, and a declared name
/// held by another kind of object all stay as they were or refuse.
#[test]
fn the_name_of_a_type_alone_grants_nothing() {
    let f = fixture("e");
    // Not declared: a read prerequisite, no more.
    let found = ownership(&f.captured, &[], &[]).unwrap();
    for object in [&f.root, &f.array, &f.label, &f.dependency] {
        assert_eq!(found.get(object), Some(&ObjectOwnership::Unqualified));
    }
    // Declared without its recorded UID.
    let error = ownership(&f.captured, &["app.mood"], &[]).unwrap_err();
    assert!(error.to_string().contains("recorded UID"), "{error}");
    // Declared, recorded, but the catalog holds another kind under the name.
    let base = fixture("b");
    let error = ownership(&base.captured, &["app.mood"], &["app.mood"]).unwrap_err();
    assert!(
        error.to_string().contains("different catalog kind"),
        "{error}"
    );
}

/// A label or a constraint belongs to the type it names, and only a type the
/// project declares: another type's rows are not claimed.
#[test]
fn another_types_labels_are_not_claimed() {
    let mut f = fixture("e");
    let other = identity("pg_type", &["app", "other"], vec![]);
    let label = identity("pg_enum", &["x"], vec![other.clone()]);
    f.captured.inputs.insert(other.clone(), kind("e"));
    f.captured
        .inputs
        .insert(label.clone(), input(BTreeMap::new()));
    let found = ownership(&f.captured, &["app.mood"], &["app.mood"]).unwrap();
    assert_eq!(found.get(&other), Some(&ObjectOwnership::Unqualified));
    assert_eq!(found.get(&label), Some(&ObjectOwnership::Unqualified));
    // A domain's constraint names its type as the third part.
    let constraint = identity(
        "pg_constraint",
        &["nonempty"],
        vec![
            identity("pg_namespace", &["app"], vec![]),
            identity("pg_class", &[], vec![]),
            f.root.clone(),
        ],
    );
    f.captured
        .inputs
        .insert(constraint.clone(), input(BTreeMap::new()));
    let found = ownership(&f.captured, &["app.mood"], &["app.mood"]).unwrap();
    assert_eq!(
        found.get(&constraint),
        Some(&ObjectOwnership::Surface(Surface::Type(
            "app.mood".parse().unwrap()
        )))
    );
    // Negative: a table's check names a table, not a type.
    let table_check = identity(
        "pg_constraint",
        &["ck"],
        vec![
            identity("pg_namespace", &["app"], vec![]),
            identity("pg_class", &["app", "t"], vec![]),
            identity("pg_type", &[], vec![]),
        ],
    );
    f.captured
        .inputs
        .insert(table_check.clone(), input(BTreeMap::new()));
    let found = ownership(&f.captured, &["app.mood"], &["app.mood"]).unwrap();
    assert_eq!(found.get(&table_check), Some(&ObjectOwnership::Unqualified));
}
