//! Captured child metadata, generated-expression ownership and in-place parent
//! column vectors must be justified by the recorded catalog facts. These
//! private fixtures exercise the same identities carried by PG16/18 reads.

use super::*;
use crate::resolver::authorization::{AuthorizationContext, RoleMap};
use pbps_db::fingerprint::EnvironmentFingerprintKey;
use pbps_db::resolver::environment::DeploymentPrincipal;
use pbps_model::resolver::{ManifestError, ObjectOwnership, Surface};
use pbps_model::{Change, ChangeSet, IdsFile, PlannedChange, TableName};
use serde_json::json;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicU64, Ordering};

static KEY_FILE: AtomicU64 = AtomicU64::new(0);

fn key() -> EnvironmentFingerprintKey {
    let path = std::env::temp_dir().join(format!(
        "pbps-1274-manifest-guards-{}-{}.key",
        std::process::id(),
        KEY_FILE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    file.write_all(b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        .unwrap();
    drop(file);
    let key = EnvironmentFingerprintKey::from_file(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    key
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

fn rename_without_children(
    major: u32,
    not_null: bool,
) -> (CompiledCapture, CapturedInputs, ChangeSet, IdsFile, IdsFile) {
    let table = TableName::new("app", "t");
    let before = table.column("id");
    let after = table.column("renamed");
    let relation = relation_identity(&table);
    let old_column = identity("column", &["id"], vec![relation.clone()]);
    let new_column = identity("column", &["renamed"], vec![relation]);
    let table_uid = "t_000000".parse().unwrap();
    let column_uid: pbps_model::Uid = "c_000000".parse().unwrap();
    let mut base_ids = IdsFile::default();
    base_ids.tables.insert(table_uid, table.clone());
    base_ids.columns.insert(column_uid.clone(), before);
    let mut desired_ids = base_ids.clone();
    desired_ids.columns.insert(column_uid.clone(), after);
    let changes = ChangeSet {
        changes: vec![PlannedChange::new(Change::RenameColumn {
            uid: column_uid,
            table,
            from: "id".into(),
            to: "renamed".into(),
            table_was: None,
        })],
    };
    let column_input = || input(BTreeMap::from([("attnotnull".into(), json!(not_null))]));
    let opening = capture(major, BTreeMap::from([(old_column, column_input())]));
    let compiled = capture(major, BTreeMap::from([(new_column, column_input())]));
    let context = AuthorizationContext {
        principal: DeploymentPrincipal {
            login: "deployer".into(),
            effective: "deployer".into(),
            superuser: false,
        },
        schemas: BTreeMap::new(),
        roles: BTreeMap::new(),
        settings: BTreeMap::new(),
    };
    let roles = RoleMap::generate(&context, &[], "deployer", "guardtest");
    let compiled = CompiledCapture::new(compiled, &key(), &roles, BTreeMap::new());
    (compiled, opening, changes, base_ids, desired_ids)
}

#[test]
fn pg18_nonnullable_rename_refuses_missing_automatic_children() {
    let (mut compiled, opening, changes, base_ids, desired_ids) = rename_without_children(18, true);
    assert_eq!(
        compiled.retain_renamed_not_null(&opening, &changes, &[], &base_ids, &desired_ids),
        Err(ManifestError::Incomplete),
        "attnotnull=true cannot be treated as a nullable column when both PG18 children are absent"
    );
}

#[test]
fn pg16_nonnullable_and_pg18_nullable_renames_need_no_child() {
    for (major, not_null) in [(16, true), (18, false)] {
        let (mut compiled, opening, changes, base_ids, desired_ids) =
            rename_without_children(major, not_null);
        assert_eq!(
            compiled.retain_renamed_not_null(&opening, &changes, &[], &base_ids, &desired_ids),
            Ok(()),
            "major {major}, attnotnull={not_null}"
        );
    }
}

fn child(column: &ObjectIdentity, name: &str) -> (ObjectIdentity, Input, ObjectIdentity) {
    let relation = column.signature[0].clone();
    let namespace = identity("pg_namespace", &["app"], vec![]);
    let no_type = identity("pg_type", &[], vec![]);
    let constraint = identity("pg_constraint", &[name], vec![namespace, relation, no_type]);
    let row = input(BTreeMap::from([
        ("contype".into(), json!("n")),
        ("conname".into(), json!(name)),
        ("conkey".into(), json!([column])),
    ]));
    let edge = identity(
        "pg_depend",
        &["a"],
        vec![constraint.clone(), column.clone()],
    );
    (constraint, row, edge)
}

#[test]
fn a_not_null_child_needs_one_exact_automatic_edge() {
    let table = TableName::new("app", "t");
    let column = identity("column", &["id"], vec![relation_identity(&table)]);
    let (first, row, edge) = child(&column, "t_id_not_null");
    let base = BTreeMap::from([
        (
            column.clone(),
            input(BTreeMap::from([("attnotnull".into(), json!(true))])),
        ),
        (first.clone(), row),
        (edge.clone(), input(BTreeMap::new())),
    ]);
    assert_eq!(
        not_null_child(&capture(18, base.clone()), &column),
        Ok(Some(first))
    );

    let mut missing_edge = base.clone();
    missing_edge.remove(&edge);
    assert_eq!(
        not_null_child(&capture(18, missing_edge), &column),
        Err(ManifestError::Incomplete)
    );

    let (second, row, edge) = child(&column, "other_not_null");
    let mut duplicate = base;
    duplicate.insert(second, row);
    duplicate.insert(edge, input(BTreeMap::new()));
    assert_eq!(
        not_null_child(&capture(18, duplicate), &column),
        Err(ManifestError::Invalid)
    );
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

/// Children PostgreSQL named when the table had another name, an unnamed
/// key, its index and an identity sequence, keep those names on the target.
fn auto_named_capture(
    major: u32,
    table: &str,
    children: &str,
) -> (
    CapturedInputs,
    ObjectIdentity,
    ObjectIdentity,
    ObjectIdentity,
) {
    let relation = identity("pg_class", &["app", table], vec![]);
    let column = identity("column", &["id"], vec![relation.clone()]);
    let key = identity(
        "pg_constraint",
        &[&format!("{children}_pkey")],
        vec![
            identity("pg_namespace", &["app"], vec![]),
            relation.clone(),
            identity("pg_type", &[], vec![]),
        ],
    );
    let index = identity("pg_class", &["app", &format!("{children}_pkey")], vec![]);
    let sequence = identity("pg_class", &["app", &format!("{children}_id_seq")], vec![]);
    let inputs = BTreeMap::from([
        (
            relation.clone(),
            input(BTreeMap::from([("relkind".into(), json!("r"))])),
        ),
        (
            column.clone(),
            input(BTreeMap::from([("attname".into(), json!("id"))])),
        ),
        (
            key.clone(),
            input(BTreeMap::from([
                ("contype".into(), json!("p")),
                ("conname".into(), json!(format!("{children}_pkey"))),
                ("conindid".into(), json!(&index)),
            ])),
        ),
        (
            index.clone(),
            input(BTreeMap::from([
                ("relkind".into(), json!("i")),
                ("relname".into(), json!(format!("{children}_pkey"))),
            ])),
        ),
        (
            sequence.clone(),
            input(BTreeMap::from([
                ("relkind".into(), json!("S")),
                ("relname".into(), json!(format!("{children}_id_seq"))),
            ])),
        ),
        (
            identity("pg_depend", &["i"], vec![sequence.clone(), column]),
            input(BTreeMap::new()),
        ),
    ]);
    (capture(major, inputs), key, index, sequence)
}

#[test]
fn a_kept_tables_auto_named_children_keep_the_targets_names() {
    use pbps_model::{Column, Identity, PrimaryKey, Schema, Table};
    let context = AuthorizationContext {
        principal: DeploymentPrincipal {
            login: "deployer".into(),
            effective: "deployer".into(),
            superuser: false,
        },
        schemas: BTreeMap::new(),
        roles: BTreeMap::new(),
        settings: BTreeMap::new(),
    };
    let roles = RoleMap::generate(&context, &[], "deployer", "autoname");
    let uid: pbps_model::Uid = "t_000000".parse().unwrap();
    for major in [16, 18] {
        // (opening table, its child spelling, final table, plan sets a key)
        // (opening table, its child spelling, final table, plan sets a key,
        // plan changes the identity column's type)
        for (old, children, new, key_change, retyped) in [
            ("old_t", "old_t", "new_t", false, false),
            ("t", "older", "t", false, false),
            ("old_t", "old_t", "new_t", true, false),
            // A type change keeps the sequence and its name (PG16/18).
            ("t", "older", "t", false, true),
        ] {
            let old_table = TableName::new("app", old);
            let new_table = TableName::new("app", new);
            let mut base_ids = IdsFile::default();
            base_ids.tables.insert(uid.clone(), old_table.clone());
            base_ids
                .columns
                .insert("c_000000".parse().unwrap(), old_table.column("id"));
            let mut desired_ids = IdsFile::default();
            desired_ids.tables.insert(uid.clone(), new_table.clone());
            desired_ids
                .columns
                .insert("c_000000".parse().unwrap(), new_table.column("id"));
            let mut column = Column::new("integer".parse().unwrap());
            column.identity = Some(Identity {
                seed: 1,
                increment: 1,
            });
            let mut table = Table::default();
            table.columns.insert("id".into(), column);
            table.primary_key = Some(PrimaryKey {
                name: None,
                columns: vec!["id".into()],
            });
            let mut desired = Schema::default();
            desired.tables.insert(new_table.clone(), table);
            let mut steps = Vec::new();
            if old != new {
                steps.push(Change::RenameTable {
                    uid: uid.clone(),
                    from: old_table.clone(),
                    to: new_table.clone(),
                    defaults: Vec::new(),
                });
            }
            if key_change {
                steps.push(Change::SetPrimaryKey {
                    table: new_table.clone(),
                    from: None,
                    to: Some(PrimaryKey {
                        name: None,
                        columns: vec!["id".into()],
                    }),
                    nonclustered: false,
                });
            }
            if retyped {
                steps.push(Change::AlterColumnType {
                    uid: "c_000000".parse().unwrap(),
                    column: new_table.column("id"),
                    from: "integer".parse().unwrap(),
                    to: "bigint".parse().unwrap(),
                    from_nullable: false,
                    to_nullable: false,
                    from_collation: None,
                    to_collation: None,
                });
            }
            let changes = ChangeSet {
                changes: steps.into_iter().map(PlannedChange::new).collect(),
            };
            let (opening, ..) = auto_named_capture(major, old, children);
            let (compiled, new_key, new_index, new_sequence) = auto_named_capture(major, new, new);
            let mut producer = CompiledCapture::new(compiled, &key(), &roles, BTreeMap::new());
            producer
                .retain_auto_named_children(&opening, &changes, &base_ids, &desired_ids, &desired)
                .unwrap();
            let names: BTreeSet<_> = producer.captured.inputs.keys().cloned().collect();
            let kept = |object: &ObjectIdentity, name: String| {
                let mut kept = object.clone();
                *kept.name.last_mut().unwrap() = name;
                kept
            };
            let key_name = if key_change { new } else { children };
            let label = format!("PG{major} {old}->{new} key_change={key_change} retyped={retyped}");
            assert!(
                names.contains(&kept(&new_key, format!("{key_name}_pkey"))),
                "{label}"
            );
            assert!(
                names.contains(&kept(&new_index, format!("{key_name}_pkey"))),
                "{label}"
            );
            assert!(
                names.contains(&kept(&new_sequence, format!("{children}_id_seq"))),
                "{label}"
            );
            let key_record = &producer.captured.inputs[&kept(&new_key, format!("{key_name}_pkey"))];
            assert_eq!(
                key_record.properties["conindid"],
                json!(kept(&new_index, format!("{key_name}_pkey"))),
                "{label}"
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

mod column_vector_parent {
    use super::*;
    use pbps_model::resolver::ObjectTransition;
    use pbps_model::{Column, Generated, Uid};

    fn uid(value: &str) -> Uid {
        value.parse().unwrap()
    }

    fn column(table: &TableName, name: &str) -> ObjectIdentity {
        identity("column", &[name], vec![relation_identity(table)])
    }

    fn ordered_capture(major: u32, table: &TableName, names: &[&str]) -> CapturedInputs {
        let relation = relation_identity(table);
        let columns: Vec<_> = names.iter().map(|name| column(table, name)).collect();
        let mut inputs = BTreeMap::from([(
            relation.clone(),
            input(BTreeMap::from([
                ("relkind".into(), json!("r")),
                ("column_order".into(), json!(&columns)),
            ])),
        )]);
        let mut numbers = BTreeMap::new();
        for (position, object) in columns.into_iter().enumerate() {
            // Dropped physical slots are not logical vector members.
            numbers.insert(object.clone(), (position as i32) * 2 + 1);
            inputs.insert(
                object.clone(),
                input(BTreeMap::from([
                    ("attrelid".into(), json!(&relation)),
                    ("attname".into(), json!(&object.name[0])),
                    ("attisdropped".into(), json!(false)),
                    ("attnotnull".into(), json!(false)),
                    ("attacl".into(), Value::Null),
                ])),
            );
        }
        let mut result = capture(major, inputs);
        result.attribute_numbers = numbers;
        result
    }

    fn add(table: &TableName, name: &str, recorded: &str, kind: &str) -> Change {
        let mut definition = Column::new("integer".parse().unwrap());
        if kind == "default" {
            definition.default = Some("7".into());
        } else if kind == "generated" {
            definition.generated = Some(Generated {
                expression: "a + 4".into(),
                stored: true,
            });
        }
        Change::AddColumn {
            uid: uid(recorded),
            table: table.clone(),
            name: name.into(),
            column: Box::new(definition),
        }
    }

    fn rename(table: &TableName, from: &str, to: &str, recorded: &str) -> Change {
        Change::RenameColumn {
            uid: uid(recorded),
            table: table.clone(),
            from: from.into(),
            to: to.into(),
            table_was: None,
        }
    }

    struct Fixture {
        table: TableName,
        opening: CapturedInputs,
        compiled: CapturedInputs,
        changes: ChangeSet,
        base_ids: IdsFile,
        desired_ids: IdsFile,
    }

    impl Fixture {
        fn new(major: u32, changes: Vec<Change>, scratch: &[(&str, &str)]) -> Self {
            let table = TableName::new("app", "t");
            let mut base_ids = IdsFile::default();
            base_ids.tables.insert(uid("t_000000"), table.clone());
            for (name, recorded) in [("a", "c_000000"), ("g", "c_000001"), ("d", "c_000002")] {
                base_ids.columns.insert(uid(recorded), table.column(name));
            }
            let mut desired_ids = IdsFile {
                tables: base_ids.tables.clone(),
                ..IdsFile::default()
            };
            for (name, recorded) in scratch {
                desired_ids
                    .columns
                    .insert(uid(recorded), table.column(*name));
            }
            Self {
                opening: ordered_capture(major, &table, &["a", "g", "d"]),
                compiled: ordered_capture(
                    major,
                    &table,
                    &scratch.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
                ),
                table,
                changes: ChangeSet {
                    changes: changes.into_iter().map(PlannedChange::new).collect(),
                },
                base_ids,
                desired_ids,
            }
        }

        fn addition(major: u32, kind: &str) -> Self {
            let table = TableName::new("app", "t");
            Self::new(
                major,
                vec![add(&table, "h", "c_000003", kind)],
                &[
                    ("a", "c_000000"),
                    ("d", "c_000002"),
                    ("g", "c_000001"),
                    ("h", "c_000003"),
                ],
            )
        }

        fn project(&self) -> Result<Option<ParentColumnOrder>, ManifestError> {
            projected_column_order(
                &self.opening,
                &self.compiled,
                &self.changes,
                &self.base_ids,
                &self.desired_ids,
                &self.table,
            )
        }

        fn assert_order(&self, names: &[&str]) {
            let projected = self.project().unwrap().unwrap();
            assert_eq!(
                projected.source,
                relation_identity(&TableName::new("app", "t"))
            );
            assert_eq!(
                projected.columns,
                names
                    .iter()
                    .map(|name| column(&self.table, name))
                    .collect::<Vec<_>>()
            );
        }
    }

    /// A key, constraint or grant runs in place and keeps the target's
    /// physical order, which can differ from the declaration order scratch
    /// created. Without a column-vector edit the opening order still holds.
    #[test]
    fn in_place_table_changes_without_column_edits_keep_the_opening_order() {
        let scratch = [("a", "c_000000"), ("d", "c_000002"), ("g", "c_000001")];
        let table = TableName::new("app", "t");
        for major in [16, 18] {
            let key = Fixture::new(
                major,
                vec![Change::SetPrimaryKey {
                    table: table.clone(),
                    from: None,
                    to: Some(pbps_model::PrimaryKey {
                        name: None,
                        columns: vec!["a".into()],
                    }),
                    nonclustered: false,
                }],
                &scratch,
            );
            key.assert_order(&["a", "g", "d"]);
            let projected = key.project().unwrap().unwrap();
            assert!(projected.separate_table_mutation && projected.key_change);
            let grant = Fixture::new(
                major,
                vec![Change::Grant {
                    role: "reader".into(),
                    target: "app.t".parse().unwrap(),
                    permissions: BTreeSet::from([pbps_model::Permission::Select]),
                }],
                &scratch,
            );
            grant.assert_order(&["a", "g", "d"]);
            let projected = grant.project().unwrap().unwrap();
            assert!(!projected.separate_table_mutation && !projected.key_change);
        }
    }

    #[test]
    fn parent_column_order_follows_opening_positions_and_ordered_typed_changes() {
        let table = TableName::new("app", "t");
        for major in [16, 18] {
            for kind in ["plain", "default", "generated"] {
                Fixture::addition(major, kind).assert_order(&["a", "g", "d", "h"]);
            }
            Fixture::new(
                major,
                vec![
                    add(&table, "h", "c_000003", "generated"),
                    add(&table, "z", "c_000004", "plain"),
                    rename(&table, "h", "renamed_h", "c_000003"),
                    Change::DropColumn {
                        uid: uid("c_000003"),
                        column: table.column("renamed_h"),
                    },
                ],
                &[
                    ("a", "c_000000"),
                    ("d", "c_000002"),
                    ("g", "c_000001"),
                    ("z", "c_000004"),
                ],
            )
            .assert_order(&["a", "g", "d", "z"]);
            Fixture::new(
                major,
                vec![Change::DropColumn {
                    uid: uid("c_000001"),
                    column: table.column("g"),
                }],
                &[("a", "c_000000"), ("d", "c_000002")],
            )
            .assert_order(&["a", "d"]);
            Fixture::new(
                major,
                vec![rename(&table, "g", "m", "c_000001")],
                &[("a", "c_000000"), ("d", "c_000002"), ("m", "c_000001")],
            )
            .assert_order(&["a", "m", "d"]);
        }
    }

    #[test]
    fn parent_column_order_refuses_incomplete_or_malformed_catalog_vectors() {
        for fault in [
            "missing-parent",
            "missing-vector",
            "malformed",
            "wrong-relkind",
            "duplicate",
            "wrong-class",
            "empty-name",
            "wrong-parent",
            "missing-column",
            "missing-live-member",
            "missing-attnums",
            "duplicate-attnum",
            "wrong-attnum-order",
            "compiled-missing",
            "compiled-extra",
            "missing-base-uid",
            "duplicate-base-uid",
            "missing-desired-uid",
            "wrong-desired-name",
            "duplicate-desired-name",
            "duplicate-table-uid",
        ] {
            let mut fixture = Fixture::addition(16, "plain");
            let relation = relation_identity(&fixture.table);
            let a = column(&fixture.table, "a");
            let g = column(&fixture.table, "g");
            let d = column(&fixture.table, "d");
            match fault {
                "missing-parent" => {
                    fixture.opening.inputs.remove(&relation);
                }
                "missing-vector" => {
                    fixture
                        .opening
                        .inputs
                        .get_mut(&relation)
                        .unwrap()
                        .properties
                        .remove("column_order");
                }
                "malformed" => {
                    fixture
                        .opening
                        .inputs
                        .get_mut(&relation)
                        .unwrap()
                        .properties
                        .insert("column_order".into(), Value::Null);
                }
                "wrong-relkind" => {
                    fixture
                        .opening
                        .inputs
                        .get_mut(&relation)
                        .unwrap()
                        .properties
                        .insert("relkind".into(), json!("v"));
                }
                "duplicate" => {
                    fixture
                        .opening
                        .inputs
                        .get_mut(&relation)
                        .unwrap()
                        .properties
                        .insert("column_order".into(), json!([&a, &g, &d, &g]));
                }
                "wrong-class" | "empty-name" | "wrong-parent" => {
                    let mut wrong = g.clone();
                    if fault == "wrong-class" {
                        wrong.class = "pg_attribute".into();
                    }
                    if fault == "empty-name" {
                        wrong.name.clear();
                    }
                    if fault == "wrong-parent" {
                        wrong.signature = vec![relation_identity(&TableName::new("app", "other"))];
                    }
                    fixture
                        .opening
                        .inputs
                        .get_mut(&relation)
                        .unwrap()
                        .properties
                        .insert("column_order".into(), json!([&a, wrong, &d]));
                }
                "missing-column" => {
                    fixture.opening.inputs.remove(&g);
                }
                "missing-live-member" => {
                    fixture
                        .opening
                        .inputs
                        .get_mut(&relation)
                        .unwrap()
                        .properties
                        .insert("column_order".into(), json!([&a, &d]));
                }
                "missing-attnums" => {
                    fixture.opening.attribute_numbers.clear();
                }
                "duplicate-attnum" => {
                    fixture.opening.attribute_numbers.insert(g.clone(), 1);
                }
                "wrong-attnum-order" => {
                    fixture.opening.attribute_numbers.insert(g.clone(), 5);
                    fixture.opening.attribute_numbers.insert(d, 3);
                }
                "compiled-missing" => {
                    fixture.compiled = ordered_capture(16, &fixture.table, &["a", "d", "g"]);
                }
                "compiled-extra" => {
                    fixture.compiled =
                        ordered_capture(16, &fixture.table, &["a", "d", "g", "h", "unplanned"]);
                }
                "missing-base-uid" => {
                    fixture.base_ids.columns.remove(&uid("c_000001"));
                }
                "duplicate-base-uid" => {
                    fixture
                        .base_ids
                        .columns
                        .insert(uid("c_000005"), fixture.table.column("g"));
                }
                "missing-desired-uid" => {
                    fixture.desired_ids.columns.remove(&uid("c_000003"));
                }
                "wrong-desired-name" => {
                    fixture
                        .desired_ids
                        .columns
                        .insert(uid("c_000003"), fixture.table.column("wrong"));
                }
                "duplicate-desired-name" => {
                    fixture
                        .desired_ids
                        .columns
                        .insert(uid("c_000005"), fixture.table.column("h"));
                }
                "duplicate-table-uid" => {
                    fixture
                        .desired_ids
                        .tables
                        .insert(uid("t_000001"), fixture.table.clone());
                }
                _ => unreachable!("unlisted fixture fault"),
            }
            let expected = match fault {
                "missing-parent"
                | "missing-vector"
                | "missing-live-member"
                | "missing-attnums"
                | "compiled-missing"
                | "compiled-extra" => ManifestError::Incomplete,
                _ => ManifestError::Invalid,
            };
            assert!(
                matches!(fixture.project(), Err(error) if error == expected),
                "{fault}"
            );
        }
    }

    #[test]
    fn parent_column_order_tracks_recorded_uids_across_renames_and_name_reuse() {
        let before = TableName::new("app", "t");
        let after = TableName::new("app", "renamed");
        let mut moved = Fixture::new(
            16,
            vec![
                Change::RenameTable {
                    uid: uid("t_000000"),
                    from: before.clone(),
                    to: after.clone(),
                    defaults: vec![],
                },
                rename(&after, "g", "m", "c_000001"),
                add(&after, "h", "c_000003", "plain"),
            ],
            &[
                ("a", "c_000000"),
                ("d", "c_000002"),
                ("m", "c_000001"),
                ("h", "c_000003"),
            ],
        );
        moved.table = after.clone();
        moved.compiled = ordered_capture(16, &after, &["a", "d", "m", "h"]);
        moved
            .desired_ids
            .tables
            .insert(uid("t_000000"), after.clone());
        for endpoint in moved.desired_ids.columns.values_mut() {
            endpoint.table = after.clone();
        }
        moved.assert_order(&["a", "m", "d", "h"]);

        let reused = Fixture::new(
            16,
            vec![
                Change::DropColumn {
                    uid: uid("c_000001"),
                    column: before.column("g"),
                },
                add(&before, "g", "c_000003", "plain"),
            ],
            &[("a", "c_000000"), ("g", "c_000003"), ("d", "c_000002")],
        );
        reused.assert_order(&["a", "d", "g"]);
        for fault in [
            "historical-uid",
            "wrong-drop-uid",
            "wrong-rename-uid",
            "occupied-name",
            "empty-name",
            "unrecorded-table",
        ] {
            let mut fixture = Fixture::addition(16, "plain");
            fixture.changes.changes = match fault {
                "historical-uid" => vec![
                    PlannedChange::new(Change::DropColumn {
                        uid: uid("c_000001"),
                        column: before.column("g"),
                    }),
                    PlannedChange::new(add(&before, "g", "c_000001", "plain")),
                ],
                "wrong-drop-uid" => vec![PlannedChange::new(Change::DropColumn {
                    uid: uid("c_000000"),
                    column: before.column("g"),
                })],
                "wrong-rename-uid" => {
                    vec![PlannedChange::new(rename(&before, "g", "m", "c_000000"))]
                }
                "occupied-name" => vec![PlannedChange::new(rename(&before, "g", "d", "c_000001"))],
                "empty-name" => vec![PlannedChange::new(add(&before, "", "c_000003", "plain"))],
                "unrecorded-table" => vec![
                    PlannedChange::new(Change::RenameTable {
                        uid: uid("t_000001"),
                        from: before.clone(),
                        to: after.clone(),
                        defaults: vec![],
                    }),
                    PlannedChange::new(add(&before, "h", "c_000003", "plain")),
                ],
                _ => unreachable!("unlisted UID fault"),
            };
            if fault == "unrecorded-table" {
                fixture.table = after.clone();
                fixture
                    .desired_ids
                    .tables
                    .insert(uid("t_000000"), after.clone());
            }
            assert!(
                matches!(fixture.project(), Err(ManifestError::Invalid)),
                "{fault}"
            );
        }
    }

    #[test]
    fn preseal_vector_changes_preserve_complete_opening_parent_metadata_and_bindings() {
        for major in [16, 18] {
            let mut fixture = Fixture::addition(major, "plain");
            let table = fixture.table.clone();
            let relation = relation_identity(&table);
            let row_type = identity("pg_type", &["app", "t"], vec![]);
            let context = AuthorizationContext {
                principal: DeploymentPrincipal {
                    login: "deployer".into(),
                    effective: "deployer".into(),
                    superuser: false,
                },
                schemas: BTreeMap::new(),
                roles: BTreeMap::new(),
                settings: BTreeMap::new(),
            };
            let roles = RoleMap::generate(&context, &[], "deployer", "vectorparent");
            let before = fixture.opening.inputs.get_mut(&relation).unwrap();
            before.properties.extend(BTreeMap::from([
                ("relname".into(), json!("t")),
                ("reloptions".into(), json!(["fillfactor=70"])),
                ("relreplident".into(), json!("d")),
                ("relchecks".into(), json!(2)),
                ("reltype".into(), json!(&row_type)),
            ]));
            before.bindings.push(Binding {
                node: "relation".into(),
                path: vec!["reltype".into()],
                target: row_type.clone(),
            });
            let after = fixture.compiled.inputs.get_mut(&relation).unwrap();
            after.properties.extend(BTreeMap::from([
                ("relname".into(), json!("t")),
                ("reloptions".into(), json!(["fillfactor=90"])),
                ("relreplident".into(), json!("f")),
                ("relchecks".into(), json!(0)),
                ("reltype".into(), json!(&row_type)),
            ]));
            // A row type belongs to the table, while a default and an unrelated
            // prerequisite remain independently retained by model projection.
            let metadata = Input {
                properties: BTreeMap::from([
                    ("typrelid".into(), json!(&relation)),
                    ("typalign".into(), json!("i")),
                ]),
                bindings: vec![Binding {
                    node: "row_type".into(),
                    path: vec!["typrelid".into()],
                    target: relation.clone(),
                }],
            };
            fixture
                .opening
                .inputs
                .insert(row_type.clone(), metadata.clone());
            let mut scratch_metadata = metadata;
            scratch_metadata
                .properties
                .insert("typalign".into(), json!("d"));
            scratch_metadata.bindings.clear();
            fixture
                .compiled
                .inputs
                .insert(row_type.clone(), scratch_metadata);
            let attrdef = identity("pg_attrdef", &[], vec![column(&table, "g")]);
            let unrelated = identity("pg_proc", &["app", "helper"], vec![]);
            for object in [&attrdef, &unrelated] {
                let original = Input {
                    properties: BTreeMap::from([("engine_definition".into(), json!("a + 1"))]),
                    bindings: vec![Binding {
                        node: "expression".into(),
                        path: vec!["input".into()],
                        target: column(&table, "a"),
                    }],
                };
                fixture.opening.inputs.insert(object.clone(), original);
                fixture.compiled.inputs.insert(
                    object.clone(),
                    Input {
                        properties: BTreeMap::from([("engine_definition".into(), json!("d + 1"))]),
                        bindings: vec![Binding {
                            node: "expression".into(),
                            path: vec!["input".into()],
                            target: column(&table, "d"),
                        }],
                    },
                );
            }
            let parent_objects = BTreeSet::from([relation.clone(), row_type.clone()]);
            let mut ownership = BTreeMap::new();
            for object in fixture
                .opening
                .inputs
                .keys()
                .chain(fixture.compiled.inputs.keys())
            {
                let owner = if object == &relation || object == &row_type {
                    ObjectOwnership::Surface(Surface::Table(table.clone()))
                } else if object.class == "column" {
                    ObjectOwnership::Surface(Surface::Column(table.column(&object.name[0])))
                } else if object == &attrdef {
                    ObjectOwnership::Surface(Surface::Default(table.column("g")))
                } else {
                    ObjectOwnership::Unqualified
                };
                ownership.insert(object.clone(), owner);
            }
            let h = column(&table, "h");
            let transitions = vec![
                ObjectTransition {
                    surface: Surface::Table(table.clone()),
                    before: parent_objects.clone(),
                    after: parent_objects.clone(),
                },
                ObjectTransition {
                    surface: Surface::Column(table.column("h")),
                    before: BTreeSet::new(),
                    after: BTreeSet::from([h.clone()]),
                },
            ];
            let fingerprint_key = key();
            let opening = fixture
                .opening
                .seal_with_roles(&fingerprint_key, None, Some(&ownership))
                .unwrap();
            let mut expected_inputs = fixture.opening.inputs.clone();
            expected_inputs.insert(h.clone(), fixture.compiled.inputs[&h].clone());
            expected_inputs
                .get_mut(&relation)
                .unwrap()
                .properties
                .insert(
                    "column_order".into(),
                    json!([
                        column(&table, "a"),
                        column(&table, "g"),
                        column(&table, "d"),
                        &h,
                    ]),
                );
            let expected = capture(major, expected_inputs)
                .seal_with_roles(&fingerprint_key, None, Some(&ownership))
                .unwrap();
            let compiled_inputs = fixture.compiled.inputs.clone();
            let compiled_numbers = fixture.compiled.attribute_numbers.clone();
            let qualified_ownership = ownership.clone();

            // The actual private sealer must restore every parent property and
            // binding; the public projection independently retains nonowners.
            let sealed =
                CompiledCapture::new(fixture.compiled, &fingerprint_key, &roles, ownership)
                    .seal_for_plan(
                        &fixture.opening,
                        &fixture.changes,
                        &transitions,
                        &fixture.base_ids,
                        &fixture.desired_ids,
                    )
                    .unwrap();
            for object in &parent_objects {
                assert_eq!(
                    sealed
                        .prerequisites()
                        .iter()
                        .find(|record| &record.object == object),
                    expected
                        .prerequisites()
                        .iter()
                        .find(|record| &record.object == object),
                    "major {major}, parent record {object:?}",
                );
            }
            let projected = opening
                .project(&fixture.changes, &sealed, &transitions)
                .unwrap();
            assert_eq!(
                projected.prerequisites(),
                expected.prerequisites(),
                "major {major}, complete properties, bindings and identities"
            );
            assert_eq!(projected.membership(), expected.membership());
            for fault in ["wrong-owner", "missing-opening", "missing-closing"] {
                let mut compiled = capture(major, compiled_inputs.clone());
                compiled.attribute_numbers = compiled_numbers.clone();
                let mut owners = qualified_ownership.clone();
                let mut incomplete = transitions.clone();
                match fault {
                    "wrong-owner" => {
                        owners.insert(
                            relation.clone(),
                            ObjectOwnership::Surface(Surface::Column(table.column("g"))),
                        );
                    }
                    "missing-opening" => {
                        incomplete[0].before.remove(&relation);
                    }
                    "missing-closing" => {
                        incomplete[0].after.remove(&relation);
                    }
                    _ => unreachable!("unlisted parent inventory fault"),
                }
                assert!(
                    matches!(
                        CompiledCapture::new(compiled, &fingerprint_key, &roles, owners)
                            .seal_for_plan(
                                &fixture.opening,
                                &fixture.changes,
                                &incomplete,
                                &fixture.base_ids,
                                &fixture.desired_ids,
                            ),
                        Err(SealError::Manifest(ManifestError::Incomplete))
                    ),
                    "major {major}, {fault}"
                );
            }
        }
    }
}
