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
