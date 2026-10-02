//! Ownership of generated expressions and of a foreign key's internal
//! triggers must be justified by the recorded catalog facts. These private
//! fixtures exercise the same identities carried by PG16/18 reads.

use super::*;
use pbps_model::resolver::{ObjectOwnership, Surface};
use pbps_model::{IdsFile, TableName};
use serde_json::json;

fn relation_identity(table: &pbps_model::TableName) -> ObjectIdentity {
    ObjectIdentity {
        class: "pg_class".into(),
        name: vec![table.schema.clone(), table.name.clone()],
        signature: Vec::new(),
    }
}

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

fn capture(major: u32, inputs: BTreeMap<ObjectIdentity, Input>) -> CapturedInputs {
    let deployer = identity("pg_authid", &["deployer"], vec![]);
    CapturedInputs {
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
        inputs,
        attribute_numbers: BTreeMap::new(),
        candidates: BTreeMap::new(),
        limitations: BTreeSet::new(),
        dropped: BTreeMap::new(),
    }
}

fn generated_attrdef_fixture(major: u32) -> (CapturedInputs, pbps_model::Schema, IdsFile) {
    use pbps_model::{Column, Generated, Schema, Table};

    let table = TableName::new("app", "t");
    let relation = relation_identity(&table);
    let mut definition = Table::default();
    let mut inputs = BTreeMap::from([(
        relation.clone(),
        input(BTreeMap::from([("relkind".into(), json!("r"))])),
    )]);
    let mut ids = IdsFile::default();
    ids.tables
        .insert("t_000000".parse().unwrap(), table.clone());
    let mut numbers = BTreeMap::new();
    for (name, number, uid) in [
        ("id", 1, "c_000000"),
        ("a", 2, "c_000001"),
        ("g", 3, "c_000002"),
        ("d", 4, "c_000003"),
    ] {
        let mut column = Column::new("integer".parse().unwrap());
        if name == "g" {
            column.generated = Some(Generated {
                expression: "a * 2 + 1".into(),
                stored: true,
            });
        } else if name == "d" {
            column.default = Some("7".into());
        }
        definition.columns.insert(name.into(), column);
        ids.columns.insert(uid.parse().unwrap(), table.column(name));
        let object = identity("column", &[name], vec![relation.clone()]);
        numbers.insert(object.clone(), number);
        inputs.insert(
            object,
            input(BTreeMap::from([
                ("attrelid".into(), json!(&relation)),
                ("attname".into(), json!(name)),
                ("attisdropped".into(), json!(false)),
            ])),
        );
    }
    let source = identity("column", &["a"], vec![relation.clone()]);
    for (name, kind) in [("g", "i"), ("d", "a")] {
        let column = identity("column", &[name], vec![relation.clone()]);
        let attrdef = identity("pg_attrdef", &[], vec![column.clone()]);
        inputs.insert(attrdef.clone(), input(BTreeMap::new()));
        inputs.insert(
            identity("pg_depend", &[kind], vec![attrdef.clone(), column]),
            input(BTreeMap::new()),
        );
        if name == "g" {
            inputs.insert(
                identity("pg_depend", &["n"], vec![attrdef, source.clone()]),
                input(BTreeMap::new()),
            );
        }
    }
    // An undeclared attrdef merely referencing the managed input gains no
    // ownership from that reference, even when the referenced column is owned.
    let foreign = identity(
        "pg_attrdef",
        &[],
        vec![identity("column", &["foreign"], vec![relation])],
    );
    inputs.insert(foreign.clone(), input(BTreeMap::new()));
    inputs.insert(
        identity("pg_depend", &["n"], vec![foreign, source]),
        input(BTreeMap::new()),
    );
    let mut schema = Schema::default();
    schema.tables.insert(table, definition);
    let mut captured = capture(major, inputs);
    captured.attribute_numbers = numbers;
    (captured, schema, ids)
}

fn classify_attrdefs(
    captured: &CapturedInputs,
    schema: &pbps_model::Schema,
    ids: &IdsFile,
) -> Result<BTreeMap<ObjectIdentity, ObjectOwnership>, super::super::Uncovered> {
    super::super::ownership::classify(
        captured,
        &super::super::RecordedOwnership {
            schema,
            ids,
            routines: &BTreeMap::new(),
            dropped: &BTreeMap::new(),
            namespaces: &BTreeSet::new(),
        },
    )
}

/// A foreign key's internal RI triggers carry the constraint in their logical
/// identity and an internal dependency on it, so they change with the table
/// surface that owns the key. A declared trigger, and an internal trigger of
/// an unowned constraint, gain nothing from that rule.
#[test]
fn a_foreign_keys_internal_triggers_belong_to_its_table_surface() {
    for major in [16, 18] {
        let (mut captured, mut schema, ids) = generated_attrdef_fixture(major);
        let table = TableName::new("app", "t");
        let relation = relation_identity(&table);
        schema.tables.get_mut(&table).unwrap().foreign_keys.insert(
            "t_fk".into(),
            pbps_model::ForeignKey {
                columns: vec!["a".into()],
                references_table: table.clone(),
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
        let namespace = identity("pg_namespace", &["app"], vec![]);
        let no_type = identity("pg_type", &[], vec![]);
        let constraint = |name: &str| {
            identity(
                "pg_constraint",
                &[name],
                vec![namespace.clone(), relation.clone(), no_type.clone()],
            )
        };
        let declared = constraint("t_fk");
        let unowned = constraint("undeclared_fk");
        for key in [&declared, &unowned] {
            captured.inputs.insert(
                key.clone(),
                input(BTreeMap::from([("contype".into(), json!("f"))])),
            );
        }
        let trigger = |key: &ObjectIdentity, function: &str| {
            identity(
                "pg_trigger",
                &[],
                vec![
                    relation.clone(),
                    key.clone(),
                    identity("pg_proc", &["pg_catalog", function], vec![]),
                ],
            )
        };
        let check = trigger(&declared, "RI_FKey_check_ins");
        let action = trigger(&declared, "RI_FKey_noaction_del");
        let stray = trigger(&unowned, "RI_FKey_check_ins");
        let user = identity("pg_trigger", &["audit"], vec![relation.clone()]);
        for (made, maker) in [
            (&check, &declared),
            (&action, &declared),
            (&stray, &unowned),
        ] {
            captured.inputs.insert(made.clone(), input(BTreeMap::new()));
            captured.inputs.insert(
                identity("pg_depend", &["i"], vec![made.clone(), maker.clone()]),
                input(BTreeMap::new()),
            );
        }
        captured.inputs.insert(user.clone(), input(BTreeMap::new()));
        let owned = classify_attrdefs(&captured, &schema, &ids).unwrap();
        let owner = ObjectOwnership::Surface(Surface::Table(table.clone()));
        assert_eq!(owned.get(&declared), Some(&owner), "PG{major} key");
        assert_eq!(owned.get(&check), Some(&owner), "PG{major} check trigger");
        assert_eq!(owned.get(&action), Some(&owner), "PG{major} action trigger");
        for object in [&unowned, &stray, &user] {
            assert_eq!(
                owned.get(object),
                Some(&ObjectOwnership::Unqualified),
                "PG{major} {object:?}"
            );
        }
    }
}

#[test]
fn generated_attrdefs_and_their_edges_have_exact_default_ownership() {
    let table = TableName::new("app", "t");
    let relation = relation_identity(&table);
    for major in [16, 18] {
        let (captured, schema, ids) = generated_attrdef_fixture(major);
        let owners = classify_attrdefs(&captured, &schema, &ids).unwrap();
        for (name, kind) in [("g", "i"), ("d", "a")] {
            let column = identity("column", &[name], vec![relation.clone()]);
            let attrdef = identity("pg_attrdef", &[], vec![column.clone()]);
            let owner = ObjectOwnership::Surface(Surface::Default(table.column(name)));
            assert_eq!(owners[&attrdef], owner, "major {major}, column {name}");
            assert_eq!(
                owners[&identity("pg_depend", &[kind], vec![attrdef.clone(), column.clone()])],
                owner
            );
            assert_eq!(
                owners[&column],
                ObjectOwnership::Surface(Surface::Column(table.column(name)))
            );
            if name == "g" {
                let source = identity("column", &["a"], vec![relation.clone()]);
                assert_eq!(
                    owners[&identity("pg_depend", &["n"], vec![attrdef, source])],
                    owner,
                    "a dependency belongs to its subject, not the referenced input"
                );
            }
        }
        let source = identity("column", &["a"], vec![relation.clone()]);
        assert_eq!(
            owners[&source],
            ObjectOwnership::Surface(Surface::Column(table.column("a")))
        );
        let foreign = identity(
            "pg_attrdef",
            &[],
            vec![identity("column", &["foreign"], vec![relation.clone()])],
        );
        assert_eq!(owners[&foreign], ObjectOwnership::Unqualified);
        assert_eq!(
            owners[&identity("pg_depend", &["n"], vec![foreign, source])],
            ObjectOwnership::Unqualified
        );

        let mut undeclared = schema.clone();
        undeclared
            .tables
            .get_mut(&table)
            .unwrap()
            .columns
            .shift_remove("g");
        let owners = classify_attrdefs(&captured, &undeclared, &ids).unwrap();
        let column = identity("column", &["g"], vec![relation.clone()]);
        let attrdef = identity("pg_attrdef", &[], vec![column.clone()]);
        assert_eq!(owners[&attrdef], ObjectOwnership::Unqualified);
        assert_eq!(
            owners[&identity("pg_depend", &["i"], vec![attrdef, column])],
            ObjectOwnership::Unqualified,
            "an edge to an undeclared column cannot supply a declared attrdef root"
        );
    }
}

#[test]
fn generated_attrdef_ownership_refuses_missing_or_invalid_recorded_roots() {
    let (captured, schema, ids) = generated_attrdef_fixture(18);
    let table = TableName::new("app", "t");
    let generated_uid = ids.column_uid(&table.column("g")).unwrap().clone();
    let mut missing_uid = ids.clone();
    missing_uid.columns.remove(&generated_uid);
    assert!(classify_attrdefs(&captured, &schema, &missing_uid).is_err());

    let mut wrong_coordinate = ids.clone();
    wrong_coordinate
        .columns
        .insert(generated_uid.clone(), table.column("unrecorded"));
    assert!(classify_attrdefs(&captured, &schema, &wrong_coordinate).is_err());

    let mut wrong_kind = ids.clone();
    let reference = wrong_kind.columns.remove(&generated_uid).unwrap();
    wrong_kind
        .columns
        .insert("t_000001".parse().unwrap(), reference);
    assert!(classify_attrdefs(&captured, &schema, &wrong_kind).is_err());

    let mut wrong_root = captured;
    wrong_root
        .inputs
        .get_mut(&relation_identity(&table))
        .unwrap()
        .properties
        .insert("relkind".into(), json!("v"));
    assert!(classify_attrdefs(&wrong_root, &schema, &ids).is_err());
}
