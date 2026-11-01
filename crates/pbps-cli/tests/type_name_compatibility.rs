//! Captured on unchanged master 770a4cc6 before #2084 step 1c's production edits.
//! Type references and canonical type syntax retain their artifact and SQL bytes.

use pbps_dialect::Dialect;
use pbps_model::*;

#[path = "../src/declaration_file.rs"]
mod declaration_file;

const NAME: &str = "type\nline\r\"'\u{1b}[31m\u{9b}\\\u{202e}名前";
const SHOWN: &str = r#"type\nline\r"'\u{1b}[31m\u{9b}\\\u{202e}名前"#;

fn artifacts(text: &str, dialect: &dyn Dialect) -> (Schema, SavedPlan, StateSnapshot) {
    let name = TableName::new("app", "typed");
    let table_uid: Uid = "t_aaaaaa".parse().unwrap();
    let column_uid: Uid = "c_aaaaaa".parse().unwrap();
    let mut table = Table {
        columns: [
            ("id".into(), Column::new("int".parse().unwrap()).not_null()),
            (
                "amount".into(),
                Column::new("decimal(18, 4)".parse().unwrap()),
            ),
        ]
        .into(),
        ..Default::default()
    };
    if dialect.name() == "postgres" {
        let domain = ColumnType::reference(TypeRef::new("app", format!("{text}domain")).unwrap());
        let enumeration =
            ColumnType::reference(TypeRef::new("app", format!("{text}enum")).unwrap());
        for (column, ty) in [
            ("domain", domain.clone()),
            ("domains", domain.into_array()),
            ("enumeration", enumeration.clone()),
            ("enumerations", enumeration.into_array()),
            ("at", "timestamp(3) with time zone".parse().unwrap()),
        ] {
            table.columns.insert(column.into(), Column::new(ty));
        }
    } else {
        table
            .columns
            .insert("body".into(), Column::new("nvarchar(max)".parse().unwrap()));
        table
            .columns
            .insert("at".into(), Column::new("datetime2(3)".parse().unwrap()));
    }
    let mut schema = Schema {
        tables: [(name.clone(), table.clone())].into(),
        ..Default::default()
    };
    let ids = IdsFile {
        tables: [(table_uid.clone(), name.clone())].into(),
        columns: [
            (column_uid.clone(), name.column("id")),
            ("c_bbbbbb".parse().unwrap(), name.column("added")),
        ]
        .into(),
        ..Default::default()
    };
    let added = table.columns[if dialect.name() == "postgres" {
        "domain"
    } else {
        "body"
    }]
    .clone();
    schema
        .tables
        .get_mut(&name)
        .unwrap()
        .columns
        .insert("added".into(), added.clone());
    let changes = vec![
        Change::CreateTable {
            uid: table_uid,
            name: name.clone(),
            table: Box::new(table.clone()),
            beside_default: None,
            moves_at_most: None,
        },
        Change::AddColumn {
            table: name.clone(),
            uid: "c_bbbbbb".parse().unwrap(),
            name: "added".into(),
            column: Box::new(added),
        },
        Change::AlterColumnType {
            column: name.column("id"),
            uid: column_uid,
            from: "int".parse().unwrap(),
            to: "bigint".parse().unwrap(),
            from_nullable: false,
            to_nullable: false,
            from_collation: None,
            to_collation: None,
            through_external_partitions: false,
            using: None,
        },
    ];
    let mut plan = SavedPlan::new(
        PlanOrigin::Database,
        dialect.name(),
        "2026-10-22T00:00:00Z",
        PlanBaseline {
            time_zone: None,
            description: "type-name master fixture".into(),
            checksum: state_checksum(&schema, &ids),
            database_collation: None,
        },
        ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        },
        ids.clone(),
    );
    if dialect.name() == "postgres" {
        plan.type_pins = ["domain", "enum"]
            .into_iter()
            .enumerate()
            .map(|(i, kind)| TypePin {
                schema: "app".into(),
                name: format!("{text}{kind}"),
                oid: 40000 + i as i64,
                kind: kind.into(),
                digest: "ab".repeat(32),
            })
            .collect();
        plan.pin_type_routines = true;
        plan.type_routines = vec![TypeRoutine {
            oid: 40002,
            schema: "app".into(),
            name: "checked".into(),
            arguments: vec!["integer".into()],
            definition: format!(
                "CREATE FUNCTION app.checked(integer) RETURNS boolean LANGUAGE sql RETURN $1 > 0; -- {text}domain"
            ),
        }];
        plan.type_routine_pins = Some(RoutinePins {
            key_id: "fixture".into(),
            server_version: "180006".into(),
            routines: Default::default(),
        });
    }
    let mut state = StateSnapshot::new(StateKind::Apply, schema.clone(), ids, "fixture");
    state.declared = Declared::from_schema(&schema);
    state.plan_checksum = Some(plan.wire_fingerprint());
    (schema, plan, state)
}

#[test]
fn type_artifacts_keep_master_bytes_checksums_sql_and_declarations() {
    check_type_artifacts(&pbps_pg::Postgres::new());
    check_type_artifacts(&pbps_mssql::Mssql);
}

fn check_type_artifacts<D>(dialect: &D)
where
    D: pbps_dialect::PlanDialect<Plan = dyn pbps_diff::order::PlanView<D>> + 'static,
{
    let (schema, mut plan, mut state) = artifacts(NAME, dialect);
    let current_checksum = plan.wire_fingerprint();
    let sql = plan
        .changes
        .changes
        .iter()
        .flat_map(|p| {
            let ordered = pbps_diff::order::singleton(p.clone(), PlanMode::Transactional, dialect);
            dialect.emit(&ordered, 0).unwrap()
        })
        .map(|s| s.sql)
        .collect::<Vec<_>>()
        .join("\n");
    let guard = if dialect.name() == "postgres" {
        pbps_pg::type_refs::bootstrap_guard(&schema, &Default::default())
            .unwrap()
            .unwrap()
    } else {
        String::new()
    };
    let yaml = schema
        .tables
        .iter()
        .map(|(n, t)| pbps_load::fmt::render(n, t, &[], None))
        .collect::<Vec<_>>()
        .join("\n---\n");
    let files = schema
        .tables
        .keys()
        .map(|name| {
            declaration_file::path(std::path::Path::new("schema"), name, None)
                .unwrap()
                .strip_prefix("schema")
                .unwrap()
                .components()
                .map(|part| part.as_os_str().to_str().unwrap())
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let dir =
        std::env::temp_dir().join(format!("pbps-type-compatibility-{}", rand::random::<u64>()));
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("plan.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
    let explain = |extra: &[&str]| {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_pbps"))
            .current_dir(&dir)
            .args(["explain", "--plan", "plan.json"])
            .args(extra)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    let json = explain(&["--format", "json"]);
    let human = explain(&[]);
    std::fs::remove_dir_all(dir).unwrap();
    // Pin the captured artifact versions while explain exercises this build's
    // current reader, as in the other hostile-name compatibility suites.
    plan.version = 46;
    state.version = 29;
    state.plan_checksum = Some(plan.wire_fingerprint());
    // Strip the type branches from set 54 (extended in sets 56 and 59) and the
    // dependency branch from set 58. Captured legacy branches must still
    // keep their exact contract and bytes.
    let mut declaration_schema =
        serde_json::to_value(schemars::schema_for!(pbps_load::dto::DeclarationFile)).unwrap();
    for name in [
        "EnumTypeDto",
        "EnumValueDto",
        "EnumValueIntentDto",
        "DomainTypeDto",
        "ExtensionDto",
        "DependencyDto",
        "DependencyName",
        "DependencyKind",
        "DependencyColumnDto",
    ] {
        let reference = format!("#/$defs/{name}");
        declaration_schema["anyOf"]
            .as_array_mut()
            .unwrap()
            .retain(|branch| branch["$ref"] != reference);
        declaration_schema["$defs"]
            .as_object_mut()
            .unwrap()
            // Keep the captured order: with preserve_order, remove() swaps
            // the last definition into the removed slot.
            .shift_remove(name);
    }
    // #2329 adds a view's `schemabinding:`, `indexes:` and `clustered:` (schema
    // set 62); the captured module branches predate it.
    let module = &mut declaration_schema["$defs"]["ModuleDto"];
    module["properties"]
        .as_object_mut()
        .unwrap()
        .shift_remove("schemabinding");
    for key in ["indexes", "clustered"] {
        module["properties"]
            .as_object_mut()
            .unwrap()
            .shift_remove(key);
    }
    for branch in module["oneOf"].as_array_mut().unwrap() {
        if let Some(properties) = branch["properties"].as_object_mut() {
            for key in ["schemabinding", "indexes", "clustered"] {
                properties.shift_remove(key);
            }
        }
    }
    declaration_schema["description"] = serde_json::json!(
        "One pbps declaration file: a table, one view, procedure, function or\ntrigger, or a database role. The leading key is both the kind and the name."
    );
    // These fixtures pin the legacy projection, before optional drop guards.
    assert!(
        plan.changes
            .changes
            .iter()
            .all(|step| step.drop_guard.is_none())
    );
    let changes_debug = format!("{:?}", plan.changes).replace(", drop_guard: None", "");
    let outputs = [
        ("schema.json", serde_json::to_string(&schema).unwrap()),
        (
            "declaration-schema.json",
            serde_json::to_string(&declaration_schema).unwrap(),
        ),
        ("changes-debug.txt", changes_debug),
        ("ids.json", serde_json::to_string(&state.ids).unwrap()),
        ("plan.json", serde_json::to_string(&plan).unwrap()),
        ("state.json", serde_json::to_string(&state).unwrap()),
        ("plan.sha256", plan.wire_fingerprint()),
        ("state.sha256", state_checksum(&schema, &state.ids)),
        ("statements.sql", sql),
        ("bootstrap.sql", guard),
        ("declarations.yml", yaml),
        ("filenames.txt", files),
        ("explain.json", json),
        ("explain.txt", human),
    ];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/name-compatibility/types")
        .join(dialect.name());
    for (name, actual) in outputs {
        let expected = std::fs::read_to_string(root.join(name)).unwrap();
        let expected = if name.starts_with("explain.") {
            expected.replace(&plan.wire_fingerprint(), &current_checksum)
        } else {
            expected
        };
        assert_eq!(actual, expected, "{}: {name}", dialect.name());
    }
    assert_eq!(
        serde_json::from_str::<SavedPlan>(&serde_json::to_string(&plan).unwrap()).unwrap(),
        plan
    );
    assert_eq!(
        StateSnapshot::from_json(&serde_json::to_string(&state).unwrap()).unwrap(),
        state
    );
    if dialect.name() == "postgres" {
        let (_, mut other, _) = artifacts(SHOWN, dialect);
        other.version = plan.version;
        assert_ne!(plan.wire_fingerprint(), other.wire_fingerprint());
    }
}

#[test]
fn type_refusals_escape_once_at_whole_and_individual_boundaries() {
    let reference = TypeRef::new("app", NAME).unwrap();
    let ty = ColumnType::reference(reference.clone());
    let shown = r#"app."type\nline\r""'\u{1b}[31m\u{9b}\\\u{202e}名前""#;
    assert_eq!(reference.shown().to_string(), shown);
    assert_eq!(
        pbps_mssql::Mssql
            .normalize_type(&ty)
            .unwrap_err()
            .to_string(),
        format!(
            "mssql: `{shown}` names a user-defined type, and references to user-defined types are not supported; declare a built-in type"
        )
    );
    let unknown = ColumnType::new("unknown", vec![TypeArg::Ident(NAME.into())]);
    let unknown_shown = r#"unknown(type\nline\r"'\u{1b}[31m\u{9b}\\\u{202e}名前)"#;
    for dialect in [
        &pbps_pg::Postgres::new() as &dyn Dialect,
        &pbps_mssql::Mssql,
    ] {
        assert_eq!(
            dialect.normalize_type(&unknown).unwrap_err().to_string(),
            format!("{} has no type `{unknown_shown}`", dialect.name())
        );
        assert!(dialect.normalize_type(&"int".parse().unwrap()).is_ok());
    }
    let absent = pbps_pg::user_types::TypeRead::Absent
        .refusal(&reference)
        .unwrap();
    assert_eq!(
        pbps_db::DbError::BadRow(absent).to_string(),
        format!(
            "unexpected row shape: the type `{shown}` does not exist in the target database, so no column can have it"
        )
    );
    let input = pbps_pg::input_path::InputPath::Unreadable(
        pbps_pg::input_path::Unreadable::Failed("unavailable".into()),
    )
    .refusal(&reference)
    .unwrap();
    assert_eq!(
        input,
        format!(
            "the code the type `{shown}` runs could not be determined (unavailable); this is not a type that runs no code"
        )
    );
    let name = TableName::new("app", "t");
    let mut table = Table {
        columns: [
            ("id".into(), Column::new("int".parse().unwrap()).not_null()),
            ("v".into(), Column::new(ty)),
        ]
        .into(),
        primary_key: Some(PrimaryKey {
            name: None,
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        }),
        data: Some(TableData {
            mode: DataMode::Exact,
            rows: Default::default(),
        }),
        ..Default::default()
    };
    let problems = pbps_pg::Postgres::new().validate_table(&name, &table);
    assert!(problems.iter().any(|e| e.to_string()==format!(
        "postgres: `data:` cannot be declared on a table with the user-defined type reference `{shown}` on column `v`"
    )), "{problems:?}");
    let scope = RowScope::Every {
        known: Default::default(),
    };
    let refusal = pbps_pg::rows::query(&name, &table, &scope).err().unwrap();
    assert_eq!(
        refusal.to_string(),
        format!(
            "app.t: its rows cannot be read back -- its column `v` is `{shown}`, a user-defined type reference, and pbps reads no rows of this type"
        )
    );
    table.columns.shift_remove("v");
    assert!(pbps_pg::rows::query(&name, &table, &scope).is_ok());
}

#[test]
fn estimate_type_descriptions_are_shown_prose() {
    let table = TableName::new("app", "t");
    let uid: Uid = "c_aaaaaa".parse().unwrap();
    let ty = ColumnType::reference(TypeRef::new("app", NAME).unwrap());
    let change = Change::AlterColumnType {
        uid: uid.clone(),
        column: table.column("v"),
        from: "int".parse().unwrap(),
        to: ty,
        from_nullable: true,
        to_nullable: true,
        from_collation: None,
        to_collation: None,
        through_external_partitions: false,
        using: None,
    };
    let changes = vec![PlannedChange::new(change)];
    let ids = IdsFile {
        tables: [("t_aaaaaa".parse().unwrap(), table.clone())].into(),
        columns: [(uid, table.column("v"))].into(),
        ..Default::default()
    };
    let names = plan_names::PlanNames::of(&changes, &ids).unwrap();
    let canonical = "app.\"type\nline\r\"\"'\u{1b}[31m\u{9b}\\\u{202e}名前\"";
    let pg = pbps_pg::estimate::planned_estimates(&names);
    let ms = pbps_mssql::estimate::planned_estimates(&names);
    let shown = pbps_model::escape::Escaped(canonical).to_string();
    assert_eq!(pg[0].1.about, format!("changing app.t.v to {shown}"));
    assert_eq!(ms[0].1.about, format!("changing app.t.v to {shown} NULL"));
    for about in [&pg[0].1.about, &ms[0].1.about] {
        assert!(!about.chars().any(char::is_control));
        assert!(about.contains(&shown));
        let json = serde_json::to_string(about).unwrap();
        assert_eq!(serde_json::from_str::<String>(&json).unwrap(), **about);
    }
}
