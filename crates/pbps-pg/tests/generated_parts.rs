//! Creation parts must retain the column and stored-value effects (#1503).
use pbps_dialect::Dialect;
use pbps_diff::order::{Baseline, analyse, generated, verify};
use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};
use pbps_model::plan_names::PlanNames;
use pbps_model::{
    Change, Column, Generated, IdsFile, Module, ModuleKind, Schema, Table, TableName, ValueSource,
};
use pbps_pg::Postgres;

fn fixture() -> (Schema, IdsFile) {
    let mut table = Table::default();
    table
        .columns
        .insert("id".into(), Column::new("integer".parse().unwrap()));
    let mut g = Column::new("integer".parse().unwrap());
    g.value_source = ValueSource::Generated(Generated {
        expression: "app.f(id)".into(),
        stored: true,
    });
    table.columns.insert("g".into(), g);
    table
        .columns
        .insert("z".into(), Column::new("integer".parse().unwrap()));
    let mut schema = Schema::default();
    schema.tables.insert(TableName::new("app", "a"), table);
    schema.modules.insert(
        "app.f(integer)".parse().unwrap(),
        Module {
            kind: ModuleKind::Function,
            description: None,
            definition: "(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN $1 * 2".into(),
        },
    );
    let ids = identities(&schema);
    (schema, ids)
}

fn identities(schema: &Schema) -> IdsFile {
    pbps_diff::resolve(
        schema,
        &IdsFile::default(),
        &[],
        &pbps_diff::Context {
            operator: "issue-1503".into(),
            today: "2026-10-19".into(),
        },
    )
    .unwrap()
    .ids
}

#[test]
fn generated_creation_parts_provide_columns_before_rows_and_preserve_stored_value_effects() {
    for placeholder in [false, true] {
        let (mut schema, ids) = fixture();
        if placeholder {
            let g = &mut schema
                .tables
                .get_mut(&TableName::new("app", "a"))
                .unwrap()
                .columns
                .get_mut("g")
                .unwrap()
                .value_source;
            if let ValueSource::Generated(g) = g {
                g.expression = "app.f(z)".into();
            }
        }
        let empty = Schema::default();
        let no_ids = IdsFile::default();
        let pg = Postgres::new().with_server_version(180_000);
        let mut cs = pbps_diff::diff(
            pbps_diff::Side {
                schema: &empty,
                ids: &no_ids,
            },
            pbps_diff::Side {
                schema: &schema,
                ids: &ids,
            },
            &pg,
            &Default::default(),
        )
        .unwrap();
        generated::expand(&mut cs, &ids, &pg).unwrap();
        cs.changes
            .push(pbps_model::PlannedChange::new(Change::InsertRow {
                table: TableName::new("app", "a"),
                key_column: "id".into(),
                identity_key: false,
                key: "1".into(),
                row: pbps_model::Row([("z".into(), pbps_model::Value::Int(3))].into()),
                defaults: Default::default(),
                types: [("z".into(), "integer".parse().unwrap())].into(),
            }));
        generated::settle(&mut cs, &schema, &Default::default(), &pg).unwrap();
        let ops = Operations::of_plan(&cs.changes, ModuleEdits::InPlace);
        let owners = PlanOwners::new(PlanNames::of(&cs.changes, &ids).unwrap(), &no_ids, &ids);
        let before = Snapshot {
            schema: &empty,
            ids: &no_ids,
        };
        let after = Snapshot {
            schema: &schema,
            ids: &ids,
        };
        let baseline = Baseline::of(before, &owners);
        let facts = analyse(&ops, &owners, before, after, &pg);
        assert!(facts.unowned.is_empty(), "{:?}", facts.unowned);
        let violations = verify(
            ops.as_slice(),
            &facts.effects,
            &baseline,
            &Default::default(),
        );
        assert!(
            violations.is_empty(),
            "placeholder={placeholder}: {violations:?}"
        );
        let row = cs.changes.len() - 1;
        let before_columns = cs
            .changes
            .iter()
            .position(|p| {
                matches!(
                    p.change,
                    Change::AddColumn { .. } | Change::AlterColumnExpression { .. }
                )
            })
            .unwrap();
        let mut order: Vec<_> = (0..cs.changes.len()).collect();
        order.remove(row);
        order.insert(before_columns, row);
        let effects = order
            .iter()
            .map(|&i| facts.effects[i].clone())
            .collect::<Vec<_>>();
        if placeholder {
            let edges = generated::constraints(&cs, &schema, &Default::default(), &pg);
            assert!(
                edges
                    .iter()
                    .any(|e| e.before == before_columns && e.after == row),
                "seed rows wait for the real stored expression, even when the columns already exist"
            );
        } else {
            assert!(
                !verify(
                    ops.reordered(&order).as_slice(),
                    &effects,
                    &baseline,
                    &Default::default()
                )
                .is_empty(),
                "a row before its added columns violates their effects"
            );
        }
        let sql = cs
            .changes
            .iter()
            .flat_map(|p| pg.emit(&p.change, p.strategy).unwrap())
            .map(|s| s.sql)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(sql.contains("SET EXPRESSION"), placeholder);
    }
}

#[test]
fn generated_creation_requires_recorded_column_identities_and_leaves_uncalled_functions_alone() {
    let (schema, ids) = fixture();
    let pg = Postgres::new();
    let table = &schema.tables[&TableName::new("app", "a")];
    let change = Change::CreateTable {
        uid: ids.table_uid(&TableName::new("app", "a")).unwrap().clone(),
        name: TableName::new("app", "a"),
        table: Box::new(table.clone()),
        beside_default: None,
        moves_at_most: None,
    };
    let f = vec!["app.f(integer)".parse().unwrap()];
    assert!(
        pg.generated_table_parts(&change, &IdsFile::default(), &f, false)
            .unwrap_err()
            .to_string()
            .contains("app.a.g")
    );
    assert!(
        pg.generated_table_parts(&change, &ids, &[], false)
            .unwrap()
            .is_none()
    );
}

#[test]
fn indexes_split_from_a_new_generated_table_remain_transactional_with_an_online_hint() {
    let (mut schema, ids) = fixture();
    let name = TableName::new("app", "a");
    schema.tables.get_mut(&name).unwrap().indexes.insert(
        "g_ix".into(),
        pbps_model::Index {
            columns: vec![pbps_model::IndexColumn {
                key: pbps_model::IndexKey::Column("g".into()),
                descending: false,
                opclass: None,
            }],
            include: vec![],
            unique: false,
            filter: None,
            method: Default::default(),
            storage_parameters: Default::default(),
        },
    );
    let pg = Postgres::new();
    let mut create = pbps_model::PlannedChange::new(Change::CreateTable {
        uid: ids.table_uid(&name).unwrap().clone(),
        name: name.clone(),
        table: Box::new(schema.tables[&name].clone()),
        beside_default: None,
        moves_at_most: None,
    });
    create.strategy.online = true;
    let parts = generated::parts(&create, &ids, &planned_functions(&schema), &pg)
        .unwrap()
        .unwrap();
    let index = parts
        .iter()
        .find(|p| matches!(p.change, Change::AddIndex { .. }))
        .unwrap();
    assert!(!index.strategy.online);
    assert!(
        !pg.emit(&index.change, index.strategy)
            .unwrap()
            .iter()
            .any(|s| s.sql.contains("CONCURRENTLY"))
    );
    // Existing-table index operations are not creation parts and keep their
    // independently approved online strategy.
    let mut standing = index.clone();
    standing.strategy.online = true;
    assert!(
        generated::parts(&standing, &ids, &planned_functions(&schema), &pg)
            .unwrap()
            .is_none()
    );
    assert!(
        pg.emit(&standing.change, standing.strategy)
            .unwrap()
            .iter()
            .any(|s| s.sql.contains("CONCURRENTLY"))
    );
}

#[test]
fn prefix_keys_checks_and_indexes_stay_inline_while_suffix_constraints_wait() {
    let (mut schema, ids) = fixture();
    let name = TableName::new("app", "a");
    let table = schema.tables.get_mut(&name).unwrap();
    table.primary_key = Some(pbps_model::PrimaryKey {
        name: Some("a_pk".into()),
        columns: vec!["id".into()],
        storage_parameters: Default::default(),
    });
    for (key, column) in [
        ("prefix_unique", "id"),
        ("suffix_unique", "g"),
        ("suffix_input", "z"),
    ] {
        table.unique.insert(
            key.into(),
            pbps_model::UniqueConstraint {
                columns: vec![column.into()],
                storage_parameters: Default::default(),
            },
        );
        table.checks.insert(
            key.into(),
            pbps_model::CheckConstraint {
                expression: format!("{column} > 0"),
            },
        );
        table.indexes.insert(
            key.into(),
            pbps_model::Index {
                columns: vec![pbps_model::IndexColumn {
                    key: pbps_model::IndexKey::Column(column.into()),
                    descending: false,
                    opclass: None,
                }],
                include: vec![],
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
    }
    for (key, column) in [("prefix_fk", "id"), ("suffix_fk", "z")] {
        table.foreign_keys.insert(
            key.into(),
            pbps_model::ForeignKey {
                columns: vec![column.into()],
                references_table: name.clone(),
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
    }
    for (name, key, include, filter) in [
        (
            "suffix_expression",
            pbps_model::IndexKey::Expression("id + z".into()),
            vec![],
            None,
        ),
        (
            "suffix_include",
            pbps_model::IndexKey::Column("id".into()),
            vec!["z".into()],
            None,
        ),
        (
            "suffix_filter",
            pbps_model::IndexKey::Column("id".into()),
            vec![],
            Some("z > 0".into()),
        ),
    ] {
        table.indexes.insert(
            name.into(),
            pbps_model::Index {
                columns: vec![pbps_model::IndexColumn {
                    key,
                    descending: false,
                    opclass: None,
                }],
                include,
                unique: false,
                filter,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
    }
    let change = Change::CreateTable {
        uid: ids.table_uid(&name).unwrap().clone(),
        name,
        table: Box::new(table.clone()),
        beside_default: None,
        moves_at_most: None,
    };
    let parts = Postgres::new()
        .generated_table_parts(&change, &ids, &["app.f(integer)".parse().unwrap()], false)
        .unwrap()
        .unwrap();
    let Change::CreateTable { table, .. } = &parts[0] else {
        unreachable!()
    };
    assert!(
        table.primary_key.is_some(),
        "an unaffected prefix key must not acquire a separate constraint approval"
    );
    for (kind, contains_prefix, contains_suffix) in [
        (
            "unique",
            table.unique.contains_key("prefix_unique"),
            table.unique.contains_key("suffix_unique"),
        ),
        (
            "check",
            table.checks.contains_key("prefix_unique"),
            table.checks.contains_key("suffix_unique"),
        ),
        (
            "index",
            table.indexes.contains_key("prefix_unique"),
            table.indexes.contains_key("suffix_unique"),
        ),
    ] {
        assert!(contains_prefix && !contains_suffix, "{kind}");
    }
    assert!(
        parts
            .iter()
            .any(|p| matches!(p, Change::AddUnique { name, .. } if name == "suffix_unique"))
    );
    assert!(
        parts
            .iter()
            .any(|p| matches!(p, Change::AddCheck { name, .. } if name == "suffix_unique"))
    );
    assert!(
        parts
            .iter()
            .any(|p| matches!(p, Change::AddIndex { name, .. } if name == "suffix_unique"))
    );
    assert!(!table.unique.contains_key("suffix_input"));
    assert!(!table.checks.contains_key("suffix_input"));
    assert!(
        parts
            .iter()
            .any(|p| matches!(p, Change::AddUnique { name, .. } if name == "suffix_input"))
    );
    assert!(
        parts
            .iter()
            .any(|p| matches!(p, Change::AddCheck { name, .. } if name == "suffix_input"))
    );
    for name in [
        "suffix_input",
        "suffix_expression",
        "suffix_include",
        "suffix_filter",
    ] {
        assert!(!table.indexes.contains_key(name), "{name}");
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, Change::AddIndex { name: n, .. } if n == name)),
            "{name}"
        );
    }
    assert!(table.foreign_keys.is_empty());
    for name in ["prefix_fk", "suffix_fk"] {
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, Change::AddForeignKey { name: n, .. } if n == name)),
            "{name}"
        );
    }
}

fn prefix_generation_fixture(expression: &str, input_before_cut: bool) -> (Change, IdsFile) {
    let (mut schema, _) = fixture();
    let name = TableName::new("app", "a");
    let mut table = Table::default();
    let mut columns = vec![("id", None), ("a", Some(expression))];
    if input_before_cut {
        columns.push(("b", None));
    }
    columns.push(("g", Some("app.f(id)")));
    if !input_before_cut {
        columns.push(("b", None));
    }
    for (name, expression) in columns {
        let mut column = Column::new("integer".parse().unwrap());
        if let Some(expression) = expression {
            column.value_source = ValueSource::Generated(Generated {
                expression: expression.into(),
                stored: true,
            });
        }
        table.columns.insert(name.into(), column);
    }
    schema.tables.insert(name.clone(), table.clone());
    let ids = identities(&schema);
    let change = Change::CreateTable {
        uid: ids.table_uid(&name).unwrap().clone(),
        name,
        table: Box::new(table),
        beside_default: None,
        moves_at_most: None,
    };
    (change, ids)
}

#[test]
fn a_prefix_generation_reading_a_removed_column_requires_the_full_creation_shape() {
    let (change, ids) = prefix_generation_fixture("b * 2", false);
    let f = ["app.f(integer)".parse().unwrap()];
    for pg in [
        Postgres::new(),
        Postgres::new().with_server_version(160_000),
    ] {
        assert!(
            pg.generated_table_parts(&change, &ids, &f, false)
                .unwrap()
                .is_none()
        );
    }
    let parts = Postgres::new()
        .with_server_version(180_000)
        .generated_table_parts(&change, &ids, &f, false)
        .unwrap()
        .unwrap();
    let Change::CreateTable { table, .. } = &parts[0] else {
        panic!("{parts:?}")
    };
    assert_eq!(
        table
            .columns
            .keys()
            .map(pbps_model::ColumnName::raw)
            .collect::<Vec<_>>(),
        ["id", "a", "g", "b"]
    );
    assert!(
        matches!(&table.columns["a"].value_source, ValueSource::Generated(g) if g.expression == "b * 2")
    );
    assert_eq!(
        parts.len(),
        2,
        "only the function-calling expression is postponed"
    );
    assert!(
        matches!(&parts[1], Change::AlterColumnExpression { column, to, .. } if column.name.raw() == "g" && to == "app.f(id)")
    );
    let sql = Postgres::new().emit(&parts[0], Default::default()).unwrap();
    assert!(sql.iter().any(|s| s.sql.contains("b * 2")));
    assert!(!sql.iter().any(|s| s.sql.contains("app.f(id)")));
}

#[test]
fn prefix_generation_inputs_kept_in_create_do_not_prevent_the_suffix_split() {
    for (expression, input_before_cut, prefix) in [
        ("id * 2", false, vec!["id", "a"]),
        ("b * 2", true, vec!["id", "a", "b"]),
    ] {
        let (change, ids) = prefix_generation_fixture(expression, input_before_cut);
        let f = ["app.f(integer)".parse().unwrap()];
        let expected = Postgres::new()
            .generated_table_parts(&change, &ids, &f, false)
            .unwrap()
            .unwrap();
        for pg in [
            Postgres::new().with_server_version(160_000),
            Postgres::new().with_server_version(180_000),
        ] {
            assert_eq!(
                pg.generated_table_parts(&change, &ids, &f, false)
                    .unwrap()
                    .unwrap(),
                expected
            );
        }
        let Change::CreateTable { table, .. } = &expected[0] else {
            panic!("{expected:?}")
        };
        assert_eq!(
            table
                .columns
                .keys()
                .map(pbps_model::ColumnName::raw)
                .collect::<Vec<_>>(),
            prefix
        );
        assert!(
            matches!(&table.columns["a"].value_source, ValueSource::Generated(g) if g.expression == expression)
        );
        assert!(
            expected
                .iter()
                .any(|c| matches!(c, Change::AddColumn { name, .. } if name.raw() == "g"))
        );
        assert!(
            !expected
                .iter()
                .any(|c| matches!(c, Change::AlterColumnExpression { .. }))
        );
    }
}

#[test]
fn a_primary_key_using_a_suffix_input_waits_for_its_column() {
    let (schema, ids) = fixture();
    let name = TableName::new("app", "a");
    for column in ["g", "z"] {
        let mut table = schema.tables[&name].clone();
        table.primary_key = Some(pbps_model::PrimaryKey {
            name: Some("a_pk".into()),
            columns: vec![column.into()],
            storage_parameters: Default::default(),
        });
        let change = Change::CreateTable {
            uid: ids.table_uid(&name).unwrap().clone(),
            name: name.clone(),
            table: Box::new(table),
            beside_default: None,
            moves_at_most: None,
        };
        let parts = Postgres::new()
            .generated_table_parts(&change, &ids, &["app.f(integer)".parse().unwrap()], false)
            .unwrap()
            .unwrap();
        let Change::CreateTable { table, .. } = &parts[0] else {
            panic!("{parts:?}")
        };
        assert!(table.primary_key.is_none());
        assert!(parts.iter().any(
            |p| matches!(p, Change::SetPrimaryKey { to: Some(key), .. } if key.columns == [pbps_model::ColumnName::from(column)])
        ));
    }
}

#[test]
fn generated_expression_installations_keep_their_creation_risks() {
    for partition_key in [false, true] {
        let (mut schema, _) = fixture();
        let name = TableName::new("app", "a");
        let table = schema.tables.get_mut(&name).unwrap();
        if partition_key {
            table.partition_by = Some(pbps_model::PartitionBy {
                strategy: Default::default(),
                columns: vec!["z".into()],
                external: false,
            });
        } else if let ValueSource::Generated(g) = &mut table.columns["g"].value_source {
            g.expression = "app.f(z)".into();
        }
        let ids = identities(&schema);
        let pg = Postgres::new().with_server_version(180_000);
        let create = pbps_model::PlannedChange::new(Change::CreateTable {
            uid: ids.table_uid(&name).unwrap().clone(),
            name: name.clone(),
            table: Box::new(schema.tables[&name].clone()),
            beside_default: None,
            moves_at_most: None,
        });
        assert!(create.risks.is_empty());
        let parts = generated::parts(&create, &ids, &planned_functions(&schema), &pg)
            .unwrap()
            .unwrap();
        assert!(
            parts
                .iter()
                .any(|p| matches!(p.change, Change::AlterColumnExpression { .. }))
        );
        for part in &parts {
            assert_eq!(part.risks, create.risks, "{:?}", part.change);
        }
        let cs = pbps_model::ChangeSet { changes: parts };
        assert_eq!(
            generated::risks(&cs, &ids, &pg),
            cs.changes
                .iter()
                .map(|p| p.risks.clone())
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn generated_columns_and_validators_keep_their_creation_risks() {
    let (mut schema, _) = fixture();
    let name = TableName::new("app", "a");
    let table = schema.tables.get_mut(&name).unwrap();
    table.columns["g"].nullable = false;
    table.columns["z"].nullable = false;
    table.primary_key = Some(pbps_model::PrimaryKey {
        name: Some("a_pk".into()),
        columns: vec!["z".into()],
        storage_parameters: Default::default(),
    });
    table.unique.insert(
        "g_unique".into(),
        pbps_model::UniqueConstraint {
            columns: vec!["g".into()],
            storage_parameters: Default::default(),
        },
    );
    table.checks.insert(
        "g_check".into(),
        pbps_model::CheckConstraint {
            expression: "g > 0".into(),
        },
    );
    table.foreign_keys.insert(
        "g_fk".into(),
        pbps_model::ForeignKey {
            columns: vec!["g".into()],
            references_table: name.clone(),
            references_columns: vec!["z".into()],
            on_delete: Default::default(),
            on_update: Default::default(),
        },
    );
    for (index, unique) in [("g_index", false), ("g_unique_index", true)] {
        table.indexes.insert(
            index.into(),
            pbps_model::Index {
                columns: vec![pbps_model::IndexColumn {
                    key: pbps_model::IndexKey::Column("g".into()),
                    descending: false,
                    opclass: None,
                }],
                include: Vec::new(),
                unique,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        );
    }
    let ids = identities(&schema);
    for pg in [
        Postgres::new(),
        Postgres::new().with_server_version(160_000),
        Postgres::new().with_server_version(180_000),
    ] {
        let create = pbps_model::PlannedChange::new(Change::CreateTable {
            uid: ids.table_uid(&name).unwrap().clone(),
            name: name.clone(),
            table: Box::new(schema.tables[&name].clone()),
            beside_default: None,
            moves_at_most: None,
        });
        let parts = generated::parts(&create, &ids, &planned_functions(&schema), &pg)
            .unwrap()
            .unwrap();
        for part in &parts {
            assert_eq!(part.risks, create.risks, "{:?}", part.change);
        }
        for kind in ["column", "key", "unique", "foreign key", "check", "index"] {
            assert!(
                parts.iter().any(|p| match kind {
                    "column" => matches!(p.change, Change::AddColumn { .. }),
                    "key" => matches!(p.change, Change::SetPrimaryKey { .. }),
                    "unique" => matches!(p.change, Change::AddUnique { .. }),
                    "foreign key" => matches!(p.change, Change::AddForeignKey { .. }),
                    "check" => matches!(p.change, Change::AddCheck { .. }),
                    "index" => matches!(p.change, Change::AddIndex { .. }),
                    _ => unreachable!(),
                }),
                "{kind}"
            );
        }
        let cs = pbps_model::ChangeSet { changes: parts };
        assert_eq!(
            generated::risks(&cs, &ids, &pg),
            cs.changes
                .iter()
                .map(|p| p.risks.clone())
                .collect::<Vec<_>>()
        );
    }
}

fn planned_functions(schema: &Schema) -> Vec<pbps_model::PlannedChange> {
    schema
        .modules
        .iter()
        .map(|(id, module)| {
            pbps_model::PlannedChange::new(Change::CreateModule {
                id: id.clone(),
                module: Box::new(module.clone()),
            })
        })
        .collect()
}

#[test]
fn creation_risks_require_all_installations_before_every_row_write() {
    let (mut schema, _) = fixture();
    let name = TableName::new("app", "a");
    if let ValueSource::Generated(g) =
        &mut schema.tables.get_mut(&name).unwrap().columns["g"].value_source
    {
        g.expression = "app.f(z)".into();
    }
    let ids = identities(&schema);
    let pg = Postgres::new().with_server_version(180_000);
    let create = pbps_model::PlannedChange::new(Change::CreateTable {
        uid: ids.table_uid(&name).unwrap().clone(),
        name: name.clone(),
        table: Box::new(schema.tables[&name].clone()),
        beside_default: None,
        moves_at_most: None,
    });
    let parts = generated::parts(&create, &ids, &planned_functions(&schema), &pg)
        .unwrap()
        .unwrap();
    let install = parts
        .iter()
        .position(|p| matches!(p.change, Change::AlterColumnExpression { .. }))
        .unwrap();
    let write = pbps_model::PlannedChange::new(Change::InsertRow {
        table: name.clone(),
        key_column: "id".into(),
        identity_key: false,
        key: "1".into(),
        row: Default::default(),
        defaults: Default::default(),
        types: Default::default(),
    });
    let mut cs = pbps_model::ChangeSet {
        changes: parts.clone(),
    };
    cs.changes.insert(install, write.clone());
    assert!(
        generated::risks(&cs, &ids, &pg)[install + 1].contains(&pbps_model::RiskClass::Narrowing)
    );
    let mut safe = pbps_model::ChangeSet {
        changes: parts.clone(),
    };
    safe.changes.push(write);
    assert!(generated::risks(&safe, &ids, &pg)[install].is_empty());
    // A second installation cannot borrow the first one's empty-table proof.
    safe.changes.push(parts[install].clone());
    assert!(
        generated::risks(&safe, &ids, &pg)
            .last()
            .unwrap()
            .contains(&pbps_model::RiskClass::Narrowing)
    );
    let mut unrelated = cs;
    if let Change::InsertRow { table, .. } = &mut unrelated.changes[install].change {
        *table = TableName::new("app", "other");
    }
    assert!(generated::risks(&unrelated, &ids, &pg)[install + 1].is_empty());
}
