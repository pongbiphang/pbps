//! A managed view's exact user columns change when the typed plan rebuilds it.
//! Catalog names and reference edges alone never grant that ownership.

use super::*;
use crate::resolver::capture::ownership::{RecordedOwnership, classify};
use pbps_model::resolver::{ObjectOwnership, Surface};
use pbps_model::{IdsFile, Module, ModuleId, ModuleKind, Schema};
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

fn attribute(parent: &ObjectIdentity, name: &str) -> Input {
    input(BTreeMap::from([
        ("attrelid".into(), serde_json::to_value(parent).unwrap()),
        ("attname".into(), json!(name)),
        ("attisdropped".into(), json!(false)),
    ]))
}

fn snapshot(major: u32) -> (CapturedInputs, ObjectIdentity, ObjectIdentity) {
    let view = identity("pg_class", &["app", "v"], vec![]);
    let column = identity("column", &["x"], vec![view.clone()]);
    let deployer = identity("pg_authid", &["deployer"], vec![]);
    let captured = CapturedInputs {
        baseline: super::super::baseline::Baseline::Absent,
        session: super::super::session::Facts {
            current_role: deployer.clone(),
            session_role: deployer,
            settings: BTreeMap::new(),
        },
        rule: properties::RULE,
        major,
        scope: CaptureScope {
            retained: BTreeSet::new(),
            candidates: BTreeSet::new(),
        },
        inputs: BTreeMap::from([
            (
                view.clone(),
                input(BTreeMap::from([("relkind".into(), json!("v"))])),
            ),
            (column.clone(), attribute(&view, "x")),
        ]),
        role_pinned: BTreeMap::new(),
        attribute_numbers: BTreeMap::from([(column.clone(), 1)]),
        candidates: BTreeMap::new(),
        limitations: BTreeSet::new(),
        dropped: BTreeMap::new(),
    };
    (captured, view, column)
}

fn ownership(
    captured: &CapturedInputs,
) -> std::result::Result<BTreeMap<ObjectIdentity, ObjectOwnership>, Uncovered> {
    let id: ModuleId = "app.v".parse().unwrap();
    let mut schema = Schema::default();
    schema.modules.insert(
        id,
        Module {
            kind: ModuleKind::View,
            description: None,
            definition: "SELECT 1 AS x".into(),
        },
    );
    let ids = IdsFile::default();
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
fn exact_view_root_owns_its_user_column_in_both_supported_catalogs() {
    let owner = ObjectOwnership::Surface(Surface::Module("app.v".parse().unwrap()));
    for major in [16, 18] {
        let (captured, view, column) = snapshot(major);
        let found = ownership(&captured).unwrap();
        assert_eq!(found.get(&view), Some(&owner), "PG{major} view root");
        assert_eq!(found.get(&column), Some(&owner), "PG{major} user column");
    }
}

#[test]
fn system_and_referenced_columns_do_not_inherit_view_ownership() {
    let (mut captured, view, column) = snapshot(18);
    let system = identity("column", &["ctid"], vec![view.clone()]);
    captured
        .inputs
        .insert(system.clone(), attribute(&view, "ctid"));
    captured.attribute_numbers.insert(system.clone(), -1);

    let foreign = identity("pg_class", &["app", "foreign"], vec![]);
    let foreign_column = identity("column", &["x"], vec![foreign.clone()]);
    captured.inputs.insert(
        foreign.clone(),
        input(BTreeMap::from([("relkind".into(), json!("r"))])),
    );
    captured
        .inputs
        .insert(foreign_column.clone(), attribute(&foreign, "x"));
    captured.attribute_numbers.insert(foreign_column.clone(), 1);
    captured.inputs.insert(
        identity(
            "pg_depend",
            &["n"],
            vec![view.clone(), foreign_column.clone()],
        ),
        input(BTreeMap::new()),
    );

    let found = ownership(&captured).unwrap();
    let owner = ObjectOwnership::Surface(Surface::Module("app.v".parse().unwrap()));
    assert_eq!(found.get(&column), Some(&owner));
    assert_eq!(found.get(&system), Some(&ObjectOwnership::Unqualified));
    assert_eq!(found.get(&foreign), Some(&ObjectOwnership::Unqualified));
    assert_eq!(
        found.get(&foreign_column),
        Some(&ObjectOwnership::Unqualified),
        "a normal dependency on another column is a reference, not ownership"
    );
}

#[test]
fn absent_or_wrong_kind_view_root_cannot_own_a_same_named_column() {
    let (mut missing, view, column) = snapshot(18);
    missing.inputs.remove(&view);
    match ownership(&missing) {
        Ok(found) => assert_eq!(found.get(&column), Some(&ObjectOwnership::Unqualified)),
        Err(_) => {}
    }

    let (mut wrong_kind, view, _) = snapshot(18);
    wrong_kind
        .inputs
        .get_mut(&view)
        .unwrap()
        .properties
        .insert("relkind".into(), json!("r"));
    assert!(ownership(&wrong_kind).is_err());
}

#[test]
fn corrupt_parent_or_missing_attribute_number_refuses_view_column_authority() {
    let (mut corrupt_parent, _, column) = snapshot(18);
    let other = identity("pg_class", &["app", "other"], vec![]);
    corrupt_parent
        .inputs
        .get_mut(&column)
        .unwrap()
        .properties
        .insert("attrelid".into(), serde_json::to_value(other).unwrap());
    assert!(ownership(&corrupt_parent).is_err());

    let (mut missing_number, _, column) = snapshot(18);
    missing_number.attribute_numbers.remove(&column);
    assert!(ownership(&missing_number).is_err());
}
