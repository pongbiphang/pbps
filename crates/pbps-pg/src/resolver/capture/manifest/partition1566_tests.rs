use super::*;
use pbps_model::{Column, PartitionBound, PartitionBy, PartitionOf, Table};

fn fixture(major: u32) -> (CapturedInputs, pbps_model::Schema, IdsFile) {
    let root = TableName::new("app", "root");
    let leaf = TableName::new("app", "leaf");
    let mut parent = Table::default();
    parent
        .columns
        .insert("id".into(), Column::new("integer".parse().unwrap()));
    parent.partition_by = Some(PartitionBy {
        strategy: Default::default(),
        columns: vec!["id".into()],
        external: false,
    });
    let child = Table {
        partition_of: Some(PartitionOf {
            parent: root.clone(),
            bound: PartitionBound::Default,
            columns: BTreeMap::new(),
        }),
        ..Default::default()
    };
    let schema = pbps_model::Schema {
        tables: BTreeMap::from([(root.clone(), parent), (leaf.clone(), child)]),
        ..Default::default()
    };
    let mut ids = IdsFile::default();
    ids.tables.insert("t_000000".parse().unwrap(), root.clone());
    ids.tables.insert("t_000001".parse().unwrap(), leaf.clone());
    ids.columns
        .insert("c_000000".parse().unwrap(), root.column("id"));
    let mut inputs = BTreeMap::new();
    let mut numbers = BTreeMap::new();
    for (table, kind) in [(&root, "p"), (&leaf, "r")] {
        let relation = relation_identity(table);
        inputs.insert(
            relation.clone(),
            input(BTreeMap::from([("relkind".into(), json!(kind))])),
        );
        let column = identity("column", &["id"], vec![relation.clone()]);
        inputs.insert(
            column.clone(),
            input(BTreeMap::from([
                ("attrelid".into(), json!(relation)),
                ("attname".into(), json!("id")),
                ("attisdropped".into(), json!(false)),
            ])),
        );
        numbers.insert(column, 1);
    }
    let mut captured = capture(major, inputs);
    captured.attribute_numbers = numbers;
    (captured, schema, ids)
}

#[test]
fn inherited_columns_keep_the_partition_owner_before_detach() {
    for major in [16, 18] {
        let (captured, schema, ids) = fixture(major);
        let leaf = TableName::new("app", "leaf");
        let column = identity("column", &["id"], vec![relation_identity(&leaf)]);
        let owned = classify_attrdefs(&captured, &schema, &ids).unwrap();
        assert_eq!(
            owned[&column],
            ObjectOwnership::Surface(Surface::Table(leaf.clone())),
            "PG{major}: an inherited column belongs to the partition's opening table UID"
        );

        let mut detached = schema.clone();
        detached.tables.get_mut(&leaf).unwrap().partition_of = None;
        detached.tables.get_mut(&leaf).unwrap().columns = schema.tables
            [&TableName::new("app", "root")]
            .columns
            .clone();
        let mut after_ids = ids.clone();
        after_ids
            .columns
            .insert("c_000001".parse().unwrap(), leaf.column("id"));
        let after = classify_attrdefs(&captured, &detached, &after_ids).unwrap();
        assert_eq!(
            after[&column],
            ObjectOwnership::Surface(Surface::Column(leaf.column("id"))),
            "detach changes ownership kind, not the catalog address"
        );
    }
}

#[test]
fn an_extra_physical_column_does_not_gain_inherited_ownership() {
    for major in [16, 18] {
        let (mut captured, schema, ids) = fixture(major);
        let relation = relation_identity(&TableName::new("app", "leaf"));
        let extra = identity("column", &["unapproved"], vec![relation.clone()]);
        captured.inputs.insert(
            extra.clone(),
            input(BTreeMap::from([
                ("attrelid".into(), json!(relation)),
                ("attname".into(), json!("unapproved")),
                ("attisdropped".into(), json!(false)),
            ])),
        );
        captured.attribute_numbers.insert(extra.clone(), 2);
        assert_eq!(
            classify_attrdefs(&captured, &schema, &ids).unwrap()[&extra],
            ObjectOwnership::Unqualified,
            "a table-wide physical scan must not adopt a genuinely new column"
        );
    }
}

#[test]
fn an_inherited_column_requires_its_actual_physical_parent() {
    for major in [16, 18] {
        let (mut captured, schema, ids) = fixture(major);
        let column = identity(
            "column",
            &["id"],
            vec![relation_identity(&TableName::new("app", "leaf"))],
        );
        captured.inputs.get_mut(&column).unwrap().properties.insert(
            "attrelid".into(),
            json!(relation_identity(&TableName::new("app", "other"))),
        );
        assert!(classify_attrdefs(&captured, &schema, &ids).is_err());
    }
}

#[test]
fn inherited_columns_follow_arbitrary_depth_and_require_recorded_ancestry() {
    for major in [16, 18] {
        let (captured, mut schema, mut ids) = fixture(major);
        let root = TableName::new("app", "root");
        let leaf = TableName::new("app", "leaf");
        let middle = TableName::new("app", "middle");
        let mut table = schema.tables[&leaf].clone();
        table.partition_by = schema.tables[&root].partition_by.clone();
        schema.tables.insert(middle.clone(), table);
        schema
            .tables
            .get_mut(&leaf)
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap()
            .parent = middle.clone();
        ids.tables
            .insert("t_000002".parse().unwrap(), middle.clone());
        let column = identity("column", &["id"], vec![relation_identity(&leaf)]);
        assert_eq!(
            classify_attrdefs(&captured, &schema, &ids).unwrap()[&column],
            ObjectOwnership::Surface(Surface::Table(leaf.clone()))
        );
        let mut absent = schema.clone();
        absent.tables.remove(&root);
        assert!(classify_attrdefs(&captured, &absent, &ids).is_err());
        let mut cyclic = schema.clone();
        cyclic
            .tables
            .get_mut(&middle)
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap()
            .parent = leaf;
        assert!(classify_attrdefs(&captured, &cyclic, &ids).is_err());
        ids.columns.clear();
        assert!(classify_attrdefs(&captured, &schema, &ids).is_err());
    }
}

#[test]
fn inherited_not_null_ownership_requires_the_exact_qualified_user_column() {
    let (mut captured, schema, ids) = fixture(18);
    let leaf = TableName::new("app", "leaf");
    let relation = relation_identity(&leaf);
    let column = identity("column", &["id"], vec![relation.clone()]);
    let owner = ObjectOwnership::Surface(Surface::Table(leaf));
    for (name, kind, dep, parent) in [
        ("not_null", "n", "a", relation.clone()),
        ("check", "c", "a", relation.clone()),
        ("normal", "n", "n", relation.clone()),
        (
            "wrong_parent",
            "n",
            "a",
            relation_identity(&TableName::new("app", "other")),
        ),
    ] {
        let constraint = identity(
            "pg_constraint",
            &[name],
            vec![identity("pg_namespace", &["app"], vec![]), parent],
        );
        captured.inputs.insert(
            constraint.clone(),
            input(BTreeMap::from([("contype".into(), json!(kind))])),
        );
        captured.inputs.insert(
            identity(
                "pg_depend",
                &[dep],
                vec![constraint.clone(), column.clone()],
            ),
            input(BTreeMap::new()),
        );
        let found = classify_attrdefs(&captured, &schema, &ids).unwrap();
        assert_eq!(
            found[&constraint],
            if name == "not_null" {
                owner.clone()
            } else {
                ObjectOwnership::Unqualified
            }
        );
    }
    // A system attribute's table ownership is not inherited user-column authority.
    let system = identity("column", &["ctid"], vec![relation.clone()]);
    captured.inputs.insert(
        system.clone(),
        input(BTreeMap::from([
            ("attrelid".into(), json!(relation)),
            ("attname".into(), json!("ctid")),
            ("attisdropped".into(), json!(false)),
        ])),
    );
    captured.attribute_numbers.insert(system.clone(), -1);
    let constraint = identity(
        "pg_constraint",
        &["system"],
        vec![
            identity("pg_namespace", &["app"], vec![]),
            relation_identity(&TableName::new("app", "leaf")),
        ],
    );
    captured.inputs.insert(
        constraint.clone(),
        input(BTreeMap::from([("contype".into(), json!("n"))])),
    );
    captured.inputs.insert(
        identity("pg_depend", &["a"], vec![constraint.clone(), system]),
        input(BTreeMap::new()),
    );
    assert_eq!(
        classify_attrdefs(&captured, &schema, &ids).unwrap()[&constraint],
        ObjectOwnership::Unqualified
    );
}

fn default_record(captured: &mut CapturedInputs, table: &TableName) -> ObjectIdentity {
    let column = identity("column", &["id"], vec![relation_identity(table)]);
    let attrdef = identity("pg_attrdef", &[], vec![column.clone()]);
    captured
        .inputs
        .insert(attrdef.clone(), input(BTreeMap::new()));
    captured.inputs.insert(
        identity("pg_depend", &["a"], vec![attrdef.clone(), column]),
        input(BTreeMap::new()),
    );
    attrdef
}

#[test]
fn partition_own_defaults_have_independent_surface_ownership() {
    for major in [16, 18] {
        let (mut captured, mut schema, ids) = fixture(major);
        let root = TableName::new("app", "root");
        let leaf = TableName::new("app", "leaf");
        schema
            .tables
            .get_mut(&root)
            .unwrap()
            .columns
            .get_mut("id")
            .unwrap()
            .value_source = pbps_model::ValueSource::Default("4".into());
        schema
            .tables
            .get_mut(&leaf)
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap()
            .columns
            .entry("id".into())
            .or_default()
            .default = Some("app.f(1)".into());
        let parent_default = default_record(&mut captured, &root);
        let own_default = default_record(&mut captured, &leaf);
        let owners = classify_attrdefs(&captured, &schema, &ids).unwrap();
        assert_eq!(
            owners[&parent_default],
            ObjectOwnership::Surface(Surface::Default(root.column("id")))
        );
        let owner = ObjectOwnership::Surface(Surface::Default(leaf.column("id")));
        assert_eq!(owners[&own_default], owner);
        let column = identity("column", &["id"], vec![relation_identity(&leaf)]);
        assert_eq!(
            owners[&identity("pg_depend", &["a"], vec![own_default, column.clone()])],
            owner
        );
        assert_eq!(
            owners[&column],
            ObjectOwnership::Surface(Surface::Table(leaf))
        );
    }
}

#[test]
fn inherited_default_copies_keep_the_partition_table_owner_without_an_own_surface() {
    for major in [16, 18] {
        let (mut captured, mut schema, ids) = fixture(major);
        let root = TableName::new("app", "root");
        let leaf = TableName::new("app", "leaf");
        schema
            .tables
            .get_mut(&root)
            .unwrap()
            .columns
            .get_mut("id")
            .unwrap()
            .value_source = pbps_model::ValueSource::Default("4".into());
        let copied = default_record(&mut captured, &leaf);
        let owners = classify_attrdefs(&captured, &schema, &ids).unwrap();
        assert_eq!(
            owners[&copied],
            ObjectOwnership::Surface(Surface::Table(leaf.clone()))
        );
        assert!(
            !owners.values().any(
                |owner| *owner == ObjectOwnership::Surface(Surface::Default(leaf.column("id")))
            )
        );

        // No declared expression in the ancestry means no authority for an
        // extra catalog default, even on a qualified inherited column.
        schema
            .tables
            .get_mut(&root)
            .unwrap()
            .columns
            .get_mut("id")
            .unwrap()
            .value_source = pbps_model::ValueSource::None;
        assert_eq!(
            classify_attrdefs(&captured, &schema, &ids).unwrap()[&copied],
            ObjectOwnership::Unqualified
        );
    }
}

#[test]
fn partition_defaults_require_the_recorded_and_physical_column_root() {
    for major in [16, 18] {
        let (mut captured, mut schema, mut ids) = fixture(major);
        let leaf = TableName::new("app", "leaf");
        schema
            .tables
            .get_mut(&leaf)
            .unwrap()
            .partition_of
            .as_mut()
            .unwrap()
            .columns
            .entry("id".into())
            .or_default()
            .default = Some("3".into());
        default_record(&mut captured, &leaf);
        let column = identity("column", &["id"], vec![relation_identity(&leaf)]);
        let physical = captured.inputs.remove(&column).unwrap();
        assert!(classify_attrdefs(&captured, &schema, &ids).is_err());
        captured.inputs.insert(column.clone(), physical);
        captured.inputs.get_mut(&column).unwrap().properties.insert(
            "attrelid".into(),
            json!(relation_identity(&TableName::new("app", "foreign"))),
        );
        assert!(classify_attrdefs(&captured, &schema, &ids).is_err());
        ids.columns.clear();
        assert!(classify_attrdefs(&captured, &schema, &ids).is_err());
    }
}
