//! A declared exclusion constraint owns its constraint row and the index
//! PostgreSQL builds for it, as one index surface of the constraint's name.
//! Without that, the plan's own new records have no owner and the closing
//! evidence cannot be sealed (#2448 C1b).

use super::*;
use crate::resolver::capture::ownership::{RecordedOwnership, classify};
use pbps_model::resolver::{ObjectOwnership, Surface};
use pbps_model::{Column, IdsFile, Schema, Table, TableName};
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

struct Fixture {
    captured: CapturedInputs,
    schema: Schema,
    ids: IdsFile,
    index: ObjectIdentity,
    index_column: ObjectIdentity,
    constraint: ObjectIdentity,
}

fn fixture(index_kind: &str, constraint_kind: &str) -> Fixture {
    let name = TableName::new("app", "t");
    let table_id = identity("pg_class", &["app", "t"], vec![]);
    let column = identity("column", &["room"], vec![table_id.clone()]);
    let index = identity("pg_class", &["app", "no_overlap"], vec![]);
    let index_column = identity("column", &["room"], vec![index.clone()]);
    let constraint = identity(
        "pg_constraint",
        &["no_overlap"],
        vec![identity("pg_namespace", &["app"], vec![]), table_id.clone()],
    );
    let mut table = Table::default();
    table
        .columns
        .insert("room".into(), Column::new("integer".parse().unwrap()));
    table.exclude.insert(
        "no_overlap".into(),
        pbps_model::ExclusionConstraint {
            method: pbps_model::IndexMethod::Btree,
            keys: vec![],
            include: vec![],
            filter: None,
            storage_parameters: Default::default(),
            timing: pbps_model::ExclusionTiming::Immediate,
        },
    );
    let schema = Schema {
        tables: BTreeMap::from([(name.clone(), table)]),
        ..Default::default()
    };
    let mut ids = IdsFile::default();
    ids.tables.insert("t_000000".parse().unwrap(), name.clone());
    ids.columns
        .insert("c_000000".parse().unwrap(), name.column("room"));
    let deployer = identity("pg_authid", &["deployer"], vec![]);
    let captured = CapturedInputs {
        baseline: super::super::baseline::Baseline::Absent,
        session: super::super::session::Facts {
            current_role: deployer.clone(),
            session_role: deployer,
            settings: BTreeMap::new(),
        },
        rule: properties::RULE,
        major: 16,
        scope: CaptureScope {
            retained: BTreeSet::new(),
            candidates: BTreeSet::new(),
        },
        inputs: BTreeMap::from([
            (
                table_id.clone(),
                input(BTreeMap::from([("relkind".into(), json!("r"))])),
            ),
            (
                column.clone(),
                input(BTreeMap::from([
                    ("attrelid".into(), serde_json::to_value(&table_id).unwrap()),
                    ("attname".into(), json!("room")),
                    ("attisdropped".into(), json!(false)),
                ])),
            ),
            (
                index.clone(),
                input(BTreeMap::from([("relkind".into(), json!(index_kind))])),
            ),
            (
                index_column.clone(),
                input(BTreeMap::from([
                    ("attrelid".into(), serde_json::to_value(&index).unwrap()),
                    ("attname".into(), json!("room")),
                    ("attisdropped".into(), json!(false)),
                ])),
            ),
            (
                constraint.clone(),
                input(BTreeMap::from([("contype".into(), json!(constraint_kind))])),
            ),
        ]),
        attribute_numbers: BTreeMap::from([(column, 1), (index_column.clone(), 1)]),
        candidates: BTreeMap::new(),
        limitations: BTreeSet::new(),
        dropped: BTreeMap::new(),
    };
    Fixture {
        captured,
        schema,
        ids,
        index,
        index_column,
        constraint,
    }
}

fn ownership(
    f: &Fixture,
) -> std::result::Result<BTreeMap<ObjectIdentity, ObjectOwnership>, Uncovered> {
    let (routines, dropped, namespaces) = (BTreeMap::new(), BTreeMap::new(), BTreeSet::new());
    classify(
        &f.captured,
        &RecordedOwnership {
            schema: &f.schema,
            ids: &f.ids,
            routines: &routines,
            dropped: &dropped,
            namespaces: &namespaces,
        },
    )
}

#[test]
fn an_exclusion_owns_its_constraint_row_and_its_index_as_one_index_surface() {
    let f = fixture("i", "x");
    let surface = Surface::Index {
        table: TableName::new("app", "t"),
        name: "no_overlap".into(),
    };
    let owned = ownership(&f).unwrap();
    assert_eq!(owned[&f.index], ObjectOwnership::Surface(surface.clone()));
    assert_eq!(
        owned[&f.constraint],
        ObjectOwnership::Surface(surface.clone())
    );
    // The columns of the index relation arrive with it, as a plain index's do.
    assert_eq!(owned[&f.index_column], ObjectOwnership::Surface(surface));
}

#[test]
fn a_different_kind_under_the_declared_name_is_refused_not_adopted() {
    let wrong_index = fixture("v", "x");
    assert!(ownership(&wrong_index).is_err());
    let wrong_constraint = fixture("i", "u");
    assert!(ownership(&wrong_constraint).is_err());
}
