//! Public refusals render each name once and preserve tool-authored SQL text.
use pbps_dialect::Dialect;
use pbps_model::*;

const NAME: &str = "type\nline\r\"'\u{1b}[31m\u{9b}\\\u{202e}名前";

fn messages(name: &str) -> serde_json::Value {
    let role = RoleName::from(name);
    let renamed = RoleName::from(format!("{name}next"));
    let table = TableName::new("app", name);
    let spec = Table {
        columns: [(
            name.into(),
            Column::new(ColumnType::reference(TypeRef::new("app", name).unwrap())),
        )]
        .into(),
        primary_key: Some(PrimaryKey {
            name: Some(name.into()),
            columns: vec!["absent".into()],
            storage_parameters: Default::default(),
        }),
        ..Default::default()
    };
    let validation = |d: &dyn Dialect| {
        d.validate_table(&table, &spec)
            .into_iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
    };
    serde_json::json!({
        "missing_role": pbps_pg::roles::refuse_missing(&role).to_string(),
        "rename_role": pbps_pg::roles::refuse_rename(&role, &renamed, pbps_pg::roles::RenameEvidence::NotRunYet).unwrap().to_string(),
        "rename_collision": pbps_pg::roles::refuse_rename(&role, &renamed, pbps_pg::roles::RenameEvidence::BothPresent).unwrap().to_string(),
        "postgres_validation": validation(&pbps_pg::Postgres::new()),
        "mssql_validation": validation(&pbps_mssql::Mssql),
        "unsupported_type": pbps_mssql::Mssql.normalize_type(&ColumnType::reference(TypeRef::new("app", name).unwrap())).unwrap_err().to_string()
    })
}

#[test]
fn public_message_fixture() {
    let outputs = serde_json::json!({
        "hostile": messages(NAME),
        "unicode": messages("名前_ünï"),
        "backslash": messages(r"one\backslash")
    });
    let expected = include_str!("fixtures/name-compatibility/public-messages.json");
    assert_eq!(
        serde_json::to_string_pretty(&outputs).unwrap() + "\n",
        expected
    );
}

#[test]
fn by_hand_sql_preserves_backslashes_and_quotes_and_labels_changed_identities() {
    for name in [
        r"one\backslash",
        r"literal\n",
        "double\"quote'",
        "名前_ünï❤️",
    ] {
        let role = RoleName::from(name);
        let text = pbps_pg::roles::refuse_missing(&role).to_string();
        let (_, statement) = text.split_once("\n\n    ").unwrap();
        assert_eq!(
            statement,
            format!("CREATE ROLE \"{}\";", name.replace('"', "\"\""))
        );
        assert!(
            text.contains("Run it by hand, then plan again:"),
            "{text:?}"
        );
        assert!(!text.contains("not pasteable"), "{text:?}");
    }
    let literal = pbps_pg::roles::refuse_missing(&RoleName::from(r"literal\n")).to_string();
    let control = pbps_pg::roles::refuse_missing(&RoleName::from("literal\n")).to_string();
    assert_ne!(literal, control);
    assert!(
        control.contains("Statement preview only (not pasteable:"),
        "{control:?}"
    );
    assert!(!control.contains("Run it by hand"), "{control:?}");
}

#[test]
fn string_carriers_are_escaped_at_the_value_instead_of_the_sentence() {
    for name in [NAME, r"literal\n", "名前_ünï❤️"] {
        let shown = pbps_model::escape::Escaped(name).to_string();
        assert_eq!(
            pbps_model::storage::canonical(name, "true").unwrap_err(),
            format!("`{shown}` is not a table storage parameter this model declares")
        );
        let mut column = Column::new("varchar(3)".parse().unwrap());
        column.value_source = ValueSource::Identity(Identity::new(1, 1));
        let table = Table {
            columns: [(name.into(), column)].into(),
            ..Default::default()
        };
        let errors = pbps_pg::Postgres::new().validate_table(&TableName::new("app", "t"), &table);
        assert!(
            errors.iter().any(|error| error
                .to_string()
                .starts_with(&format!("postgres: column `{shown}` has an `identity:`"))),
            "{errors:?}"
        );
        for error in errors {
            let text = error.to_string();
            assert!(!text.chars().any(char::is_control), "{text:?}");
            if shown != name {
                assert!(
                    !text.contains(&pbps_model::escape::Escaped(&shown).to_string()),
                    "{text:?}"
                );
            }
        }
        assert_eq!(
            serde_json::to_value(&table).unwrap()["columns"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            [&name.to_owned()]
        );
    }
}

#[test]
fn trigger_refusals_show_names_once_in_the_sentence_and_its_sql_example() {
    for name in [NAME, r"literal\n", "名前_ünï❤️"] {
        let shown = pbps_model::escape::Escaped(name).to_string();
        let id = ModuleId::Trigger {
            on: TableName::new("app", "t"),
            name: name.into(),
        };
        let mut module = Module {
            kind: ModuleKind::Trigger,
            description: None,
            definition: "AFTER INSERT ON app.other FOR EACH ROW EXECUTE FUNCTION app.f()".into(),
        };
        let errors = pbps_pg::Postgres::new().validate_module(&id, &module);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(
            errors[0].to_string(),
            format!(
                "postgres: trigger `{id}` is declared on `app.t`, and its definition puts it on `app.other`. On this engine the table is part of the statement the declaration holds -- `CREATE TRIGGER {shown} AFTER INSERT ON app.t ...` -- so a definition naming another table creates the trigger there, under this key, and the next plan cannot find it",
                id = id.shown()
            )
        );
        module.definition = "AFTER INSERT ON app.t FOR EACH ROW EXECUTE FUNCTION app.f()".into();
        assert!(
            pbps_pg::Postgres::new()
                .validate_module(&id, &module)
                .is_empty()
        );
    }
}

mod plan_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../pbps-dialect/tests/support/preflight.rs"
    ));
}

#[test]
fn foreign_key_probe_prose_shows_raw_row_keys_and_defaulted_columns_once() {
    use plan_fixture::FixturePreflight as _;
    for name in [NAME, r"literal\n", "名前_ünï❤️"] {
        let shown = pbps_model::escape::Escaped(name).to_string();
        let child = TableName::new("app", "child");
        let parent = TableName::new("app", "parent");
        for insert in [false, true] {
            let row = if insert {
                Change::InsertRow {
                    table: child.clone(),
                    key_column: "code".into(),
                    identity_key: false,
                    key: name.into(),
                    row: Default::default(),
                    defaults: [(name.into(), "lower('OLD')".into())].into(),
                    types: [(name.into(), "text".parse().unwrap())].into(),
                }
            } else {
                Change::UpdateRow {
                    table: child.clone(),
                    key_column: "code".into(),
                    key: name.into(),
                    columns: [(
                        name.into(),
                        (
                            Cell::Value(Value::Text("old".into())),
                            Cell::Default("lower('OLD')".into()),
                        ),
                    )]
                    .into(),
                    unchanged: Default::default(),
                    types: Default::default(),
                    after_types: Default::default(),
                    key_type: None,
                }
            };
            let changes = ChangeSet {
                changes: vec![
                    PlannedChange::new(Change::AddForeignKey {
                        table: child.clone(),
                        name: "fk".into(),
                        constraint: Box::new(ForeignKey {
                            columns: vec![name.into()],
                            references_table: parent.clone(),
                            references_columns: vec!["code".into()],
                            on_delete: ReferentialAction::NoAction,
                            on_update: ReferentialAction::NoAction,
                        }),
                    }),
                    PlannedChange::new(row),
                    PlannedChange::new(Change::DeleteRow {
                        table: parent.clone(),
                        key_column: "code".into(),
                        key: name.into(),
                        cause: pbps_model::change::DeleteCause::Undeclared,
                        dropped: Default::default(),
                        row: Default::default(),
                        types: Default::default(),
                        after_types: Default::default(),
                        key_type: None,
                    }),
                ],
            };
            for dialect in [
                &pbps_pg::Postgres::new() as &dyn Dialect,
                &pbps_mssql::Mssql,
            ] {
                let report = dialect.fixture_preflight(&changes);
                let catalog = report
                    .probes
                    .iter()
                    .find(|p| p.description.starts_with("foreign-key column(s)"))
                    .unwrap_or_else(|| panic!("{report:#?}"));
                let assigned = if insert && dialect.name() == "postgres" {
                    format!(
                        ", a column row `{shown}` leaves to the engine to assign, where the key reaches one"
                    )
                } else {
                    String::new()
                };
                assert_eq!(
                    catalog.description,
                    format!(
                        "foreign-key column(s) of app.child that reference app.parent and that this plan writes to a default the probe cannot evaluate, which may be row `{shown}` being deleted: {shown} (row `{shown}`){assigned}; spell the value"
                    )
                );
                assert!(!catalog.description.chars().any(char::is_control));
                if dialect.name() == "postgres" {
                    let planned = report
                        .probes
                        .iter()
                        .find(|p| {
                            p.description
                                .starts_with("column(s) of the foreign key this plan adds")
                        })
                        .unwrap_or_else(|| panic!("{report:#?}"));
                    assert_eq!(
                        planned.description,
                        format!(
                            "column(s) of the foreign key this plan adds from app.child into app.parent that hold a default the probe cannot evaluate, which may be row `{shown}` being deleted: {shown} (row `{shown}`); spell the value"
                        )
                    );
                    let ordinary = report
                        .probes
                        .iter()
                        .find(|p| {
                            p.description
                                .starts_with("rows of app.child that reference")
                        })
                        .unwrap();
                    assert_eq!(
                        ordinary.description,
                        format!(
                            "rows of app.child that reference app.parent row `{shown}` through the foreign key this plan adds on a column it also adds, whose default is what every stored row will hold there; the key cannot be added once the row is gone"
                        )
                    );
                }
                assert_eq!(
                    serde_json::to_value(&changes.changes[1].change).unwrap()["key"],
                    name
                );
            }
        }
    }
}

#[test]
fn row_policy_refusals_show_names_once_without_changing_the_policy_key() {
    for (name, shown) in [
        (
            "policy\n\u{1b}[31m\u{202e}名前",
            r"policy\n\u{1b}[31m\u{202e}名前",
        ),
        (
            r"policy\n\u{1b}[31m\u{202e}名前",
            r"policy\\n\\u{1b}[31m\\u{202e}名前",
        ),
        ("名前_ünï", "名前_ünï"),
    ] {
        let mut table = Table {
            columns: [("id".into(), Column::new("integer".parse().unwrap()))].into(),
            row_policies: [(
                name.to_owned(),
                RowPolicy {
                    command: PolicyCommand::Insert,
                    using: Some("true".into()),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        };
        let errors = pbps_pg::Postgres::new().validate_table(&TableName::new("app", "t"), &table);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(
            errors[0].to_string(),
            format!(
                "postgres: row policy `{shown}`: only WITH CHECK expression allowed for INSERT"
            )
        );
        assert_eq!(
            serde_json::to_value(&table).unwrap()["row_policies"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            [&name.to_owned()]
        );
        table.row_policies.get_mut(name).unwrap().using = None;
        assert!(
            pbps_pg::Postgres::new()
                .validate_table(&TableName::new("app", "t"), &table)
                .is_empty()
        );
    }
}

#[test]
fn range_until_refusals_show_opaque_values_once_for_dates_and_integers() {
    for (ty, every, from, valid_until, reason) in [
        (
            "date",
            "1day",
            "2027-01-01",
            "2027-01-02",
            "which is not how the engine prints a date bound (years 1 to 9999, as `2027-01-01`)",
        ),
        (
            "bigint",
            "10",
            "0",
            "10",
            "which is not how the engine prints a bigint bound, as `1000`",
        ),
    ] {
        let parent = TableName::new("app", "ev");
        let schema = Schema {
            tables: [(
                parent.clone(),
                Table {
                    columns: [("ts".into(), Column::new(ty.parse().unwrap()))].into(),
                    partition_by: Some(PartitionBy {
                        columns: vec!["ts".into()],
                        external: false,
                        strategy: Default::default(),
                    }),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        };
        let ask = |until| {
            pbps_pg::Postgres::new().range_series(&pbps_dialect::RangeSeriesRequest {
                schema: &schema,
                parent: &parent,
                every,
                until,
                from: Some(from),
            })
        };
        for (until, shown) in [
            (
                "bad\n\u{1b}[31m\u{202e}名前",
                r"bad\n\u{1b}[31m\u{202e}名前",
            ),
            (
                r"bad\n\u{1b}[31m\u{202e}名前",
                r"bad\\n\\u{1b}[31m\\u{202e}名前",
            ),
            ("名前_ünï", "名前_ünï"),
        ] {
            assert_eq!(
                ask(until).unwrap_err().to_string(),
                format!("postgres: --until `{shown}`, {reason}")
            );
        }
        assert_eq!(ask(valid_until).unwrap().partitions.len(), 1);
    }
}

#[test]
fn postgres_grant_refusals_show_schema_operands_once_without_changing_targets() {
    for (name, shown) in [
        (
            "schema\n\u{1b}[31m\u{202e}名前",
            r"schema\n\u{1b}[31m\u{202e}名前",
        ),
        (
            r"schema\n\u{1b}[31m\u{202e}名前",
            r"schema\\n\\u{1b}[31m\\u{202e}名前",
        ),
        ("名前_ünï", "名前_ünï"),
    ] {
        for reserved in [false, true] {
            let prefix = if reserved { "pg_" } else { "" };
            let schema_name = format!("{prefix}{name}");
            let shown = format!("{prefix}{shown}");
            let object = TableName::new(&schema_name, "t");
            let schema = Schema {
                tables: [(
                    object.clone(),
                    Table {
                        columns: [("id".into(), Column::new("integer".parse().unwrap()))].into(),
                        ..Default::default()
                    },
                )]
                .into(),
                ..Default::default()
            };
            let target = GrantTarget::Object(object);
            let mut role = Role {
                grants: [(target.clone(), [Permission::Select].into())].into(),
                ..Default::default()
            };
            let errors = pbps_pg::Postgres::new().validate_role(&"reader".into(), &role, &schema);
            assert_eq!(errors.len(), 1, "{errors:?}");
            let expected = if reserved {
                format!(
                    "postgres: role `reader`: `{shown}.t` is in schema `{shown}`, which is the engine's own and not a project's: `pg_catalog`, `information_schema` and every `pg_` schema are left out of the pull, so a grant there reads back as absent and every plan would propose it again (DECISIONS 385). Grant in a schema this project declares"
                )
            } else {
                format!(
                    "postgres: role `reader`: `{shown}.t` is granted in schema `{shown}`, which this role has no `usage` on -- PostgreSQL checks the schema before the object, so every permission here reaches nothing and the role's own query is `permission denied for schema {shown}` (ADR-0010 section 1, measured). Add `schema::{shown}: [usage]` to this role"
                )
            };
            assert_eq!(errors[0].to_string(), expected);
            assert_eq!(role.grants.keys().next().unwrap().schema(), schema_name);
            if !reserved {
                let schema_target = GrantTarget::Schema(schema_name.clone());
                let mut schema_role = Role {
                    grants: [(schema_target.clone(), [Permission::Select].into())].into(),
                    ..Default::default()
                };
                let errors =
                    pbps_pg::Postgres::new().validate_role(&"reader".into(), &schema_role, &schema);
                assert_eq!(errors.len(), 1, "{errors:?}");
                assert_eq!(
                    errors[0].to_string(),
                    format!(
                        "postgres: role `reader`: `select` on `schema::{shown}` is not a permission PostgreSQL defines on a schema (`invalid privilege type SELECT for schema`), which takes only usage and create. On this engine a schema grant does not carry to the objects in it: `GRANT ... ON ALL TABLES IN SCHEMA` applies once to what is there now, and `ALTER DEFAULT PRIVILEGES` covers only what one role creates afterwards (ADR-0010 section 2). Grant `select` on each object instead, and keep `schema::{shown}: [usage]` so the role can reach them"
                    )
                );
                assert_eq!(schema_role.grants.keys().next().unwrap(), &schema_target);
                schema_role
                    .grants
                    .insert(schema_target, [Permission::Usage].into());
                assert!(
                    pbps_pg::Postgres::new()
                        .validate_role(&"reader".into(), &schema_role, &schema)
                        .is_empty()
                );
                role.grants
                    .insert(GrantTarget::Schema(schema_name), [Permission::Usage].into());
                assert!(
                    pbps_pg::Postgres::new()
                        .validate_role(&"reader".into(), &role, &schema)
                        .is_empty()
                );
            }
        }
    }
}
