//! Captured on unchanged master e7a290af before #2084 step 4's production edits.
//! Index and constraint identities retain the original artifact and SQL bytes.

use pbps_dialect::Dialect;
use pbps_model::*;

#[path = "../src/declaration_file.rs"]
mod declaration_file;

const NAME: &str = "local\nline\r\u{1b}[31m\u{9b}\\\u{202e}名前";
const SHOWN: &str = r"local\nline\r\u{1b}[31m\u{9b}\\\u{202e}名前";

fn artifacts(text: &str, dialect: &dyn Dialect) -> (Schema, SavedPlan, StateSnapshot) {
    let parent = TableName::new("app", "parent");
    let name = TableName::new("app", "child");
    let table_uid = "t_aaaaaa".parse::<Uid>().unwrap();
    let pk = PrimaryKey {
        name: Some(format!("{text}pk").into()),
        columns: vec!["id".into()],
        storage_parameters: Default::default(),
    };
    let unique = UniqueConstraint {
        columns: vec!["code".into()],
        storage_parameters: Default::default(),
    };
    let foreign = ForeignKey {
        columns: vec!["code".into()],
        references_table: parent.clone(),
        references_columns: vec!["id".into()],
        on_delete: Default::default(),
        on_update: Default::default(),
    };
    let check = CheckConstraint {
        expression: "code > 0".into(),
    };
    let index = Index {
        columns: vec![IndexColumn::column("code")],
        include: vec!["id".into()],
        unique: false,
        filter: Some("code > 0".into()),
        method: Default::default(),
        storage_parameters: Default::default(),
    };
    let mut table = Table {
        columns: [
            ("id".into(), Column::new("int".parse().unwrap()).not_null()),
            (
                "code".into(),
                Column::new("int".parse().unwrap()).not_null(),
            ),
        ]
        .into(),
        primary_key: Some(pk.clone()),
        unique: [(format!("{text}uq").into(), unique.clone())].into(),
        foreign_keys: [(format!("{text}fk").into(), foreign.clone())].into(),
        checks: [(format!("{text}ck").into(), check.clone())].into(),
        indexes: [(format!("{text}ix").into(), index.clone())].into(),
        ..Default::default()
    };
    if dialect.name() == "postgres" {
        let mut expression = index.clone();
        expression.include.clear();
        expression.columns[0].key = IndexKey::Expression("code + 1".into());
        table
            .indexes
            .insert(format!("{text}expr").into(), expression);
        table.replica_identity = Some(ReplicaIdentity::Unique(format!("{text}uq").into()));
    } else {
        table.clustered = Some(Clustered::Unique(format!("{text}uq").into()));
    }
    let schema = Schema {
        tables: [
            (
                parent.clone(),
                Table {
                    columns: [("id".into(), Column::new("int".parse().unwrap()).not_null())].into(),
                    primary_key: Some(PrimaryKey {
                        name: None,
                        columns: vec!["id".into()],
                        storage_parameters: Default::default(),
                    }),
                    ..Default::default()
                },
            ),
            (name.clone(), table.clone()),
        ]
        .into(),
        ..Default::default()
    };
    let ids = IdsFile {
        tables: [
            (table_uid.clone(), name.clone()),
            ("t_bbbbbb".parse().unwrap(), parent.clone()),
        ]
        .into(),
        ..Default::default()
    };
    let mut changes = vec![
        Change::CreateTable {
            uid: table_uid.clone(),
            name: name.clone(),
            table: Box::new(table.clone()),
            beside_default: None,
            moves_at_most: None,
        },
        Change::SetPrimaryKey {
            table: name.clone(),
            from: None,
            to: Some(pk),
            nonclustered: false,
        },
        Change::AddUnique {
            table: name.clone(),
            name: format!("{text}uq").into(),
            constraint: unique,
            clustered: false,
        },
        Change::DropUnique {
            table: name.clone(),
            name: format!("{text}uq").into(),
        },
        Change::AddForeignKey {
            table: name.clone(),
            name: format!("{text}fk").into(),
            constraint: Box::new(foreign),
        },
        Change::DropForeignKey {
            table: name.clone(),
            name: format!("{text}fk").into(),
        },
        Change::AddCheck {
            table: name.clone(),
            name: format!("{text}ck").into(),
            constraint: check,
        },
        Change::DropCheck {
            table: name.clone(),
            name: format!("{text}ck").into(),
        },
        Change::AddIndex {
            table: name.clone(),
            name: format!("{text}ix").into(),
            index: Box::new(index),
            clustered: false,
        },
        Change::DropIndex {
            table: name.clone(),
            name: format!("{text}ix").into(),
        },
    ];
    if dialect.name() == "postgres" {
        for target in [
            IndexPart::PrimaryKey,
            IndexPart::Unique(format!("{text}uq").into()),
            IndexPart::Index(format!("{text}ix").into()),
        ] {
            changes.push(Change::SetIndexStorageParameters {
                table: name.clone(),
                target,
                method: Default::default(),
                set: [("fillfactor".into(), "80".into())].into(),
                reset: Default::default(),
            });
        }
        for to in [
            ReplicaIdentity::Unique(format!("{text}uq").into()),
            ReplicaIdentity::Index(format!("{text}ix").into()),
        ] {
            changes.push(Change::SetReplicaIdentity {
                uid: table_uid.clone(),
                table: name.clone(),
                to: Some(to),
            });
        }
        changes.push(Change::DetachPartition {
            uid: table_uid,
            table: name.clone(),
            parent,
            names: [
                DetachedKind::PrimaryKey,
                DetachedKind::Unique,
                DetachedKind::ForeignKey,
                DetachedKind::Check,
                DetachedKind::Index,
            ]
            .into_iter()
            .enumerate()
            .map(|(i, kind)| DetachedName {
                kind,
                parent: format!("{text}parent{i}").into(),
                name: Some(format!("{text}child{i}").into()),
            })
            .collect(),
            shape: Box::new(table),
        });
    }
    let plan = SavedPlan::new(
        PlanOrigin::Database,
        dialect.name(),
        "2026-10-22T00:00:00Z",
        PlanBaseline {
            time_zone: None,
            description: "local-name master fixture".into(),
            checksum: state_checksum(&schema, &ids),
            database_collation: None,
        },
        ChangeSet {
            changes: changes.into_iter().map(PlannedChange::new).collect(),
        },
        ids.clone(),
    );
    let mut state = StateSnapshot::new(StateKind::Apply, schema.clone(), ids, "fixture");
    state.declared = Declared::from_schema(&schema);
    state.declared.bindings.checks.insert(
        name.clone(),
        [(format!("{text}ck").into(), Binding::default())].into(),
    );
    state.declared.bindings.filters.insert(
        name,
        [(format!("{text}ix").into(), Binding::default())].into(),
    );
    state.plan_checksum = Some(plan.wire_fingerprint());
    (schema, plan, state)
}

#[test]
fn local_artifacts_keep_master_bytes_checksums_sql_and_declarations() {
    check_local_artifacts(&pbps_pg::Postgres::new());
    check_local_artifacts(&pbps_mssql::Mssql);
}

fn check_local_artifacts<D>(dialect: &D)
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
    let dir = std::env::temp_dir().join(format!(
        "pbps-local-compatibility-{}",
        rand::random::<u64>()
    ));
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("plan.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
    let explain = std::process::Command::new(env!("CARGO_BIN_EXE_pbps"))
        .current_dir(&dir)
        .args(["explain", "--plan", "plan.json", "--format", "json"])
        .output()
        .unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert!(explain.status.success(), "{explain:?}");
    // Compare captured envelopes after explain has exercised the current
    // applicable version, following the column/role compatibility suites.
    plan.version = 44;
    state.version = 27;
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
        ("declarations.yml", yaml),
        ("filenames.txt", files),
        ("explain.json", String::from_utf8(explain.stdout).unwrap()),
    ];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/name-compatibility/local")
        .join(dialect.name());
    for (name, actual) in outputs {
        let expected = std::fs::read_to_string(root.join(name)).unwrap();
        let expected = if name == "explain.json" {
            expected.replace(&plan.wire_fingerprint(), &current_checksum)
        } else {
            expected
        };
        assert_eq!(actual, expected, "{name}");
    }
    assert_eq!(
        serde_json::from_str::<SavedPlan>(&serde_json::to_string(&plan).unwrap()).unwrap(),
        plan
    );
    assert_eq!(
        StateSnapshot::from_json(&serde_json::to_string(&state).unwrap()).unwrap(),
        state
    );
    let (_, mut other, _) = artifacts(SHOWN, dialect);
    other.version = plan.version;
    assert_ne!(plan.wire_fingerprint(), other.wire_fingerprint());
}

#[test]
fn local_refusals_escape_names_once_at_whole_and_individual_boundaries() {
    let table_name = TableName::new("app", "t");
    let table = Table {
        columns: [("id".into(), Column::new("int".parse().unwrap()))].into(),
        indexes: [(
            NAME.into(),
            Index {
                columns: vec![IndexColumn::column("id")],
                include: vec![],
                unique: false,
                filter: None,
                method: IndexMethod::Gin,
                storage_parameters: Default::default(),
            },
        )]
        .into(),
        ..Default::default()
    };
    assert_eq!(
        pbps_mssql::Mssql
            .validate_table(&table_name, &table)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec![format!(
            "mssql: index `{SHOWN}` uses the `gin` method, which SQL Server does not have"
        )]
    );
    let error = pbps_diff::DiffError::PermanentReferencesUnlogged {
        table: table_name,
        key: NAME.into(),
        target: TableName::new("app", "parent"),
    };
    assert_eq!(
        error.to_string(),
        format!(
            "app.t: its foreign key `{SHOWN}` references app.parent, which is unlogged, and PostgreSQL lets only an unlogged table reference an unlogged one. Make app.t unlogged too, or app.parent permanent."
        )
    );
}

#[test]
fn namespace_claims_distinguish_raw_names_from_their_shown_lookalikes() {
    let index = Index {
        columns: vec![IndexColumn::column("id")],
        include: vec![],
        unique: false,
        filter: None,
        method: Default::default(),
        storage_parameters: Default::default(),
    };
    let owner = TableName::new("app", "owner");
    let mut postgres = Schema {
        tables: [
            (
                owner.clone(),
                Table {
                    indexes: [(NAME.into(), index)].into(),
                    ..Default::default()
                },
            ),
            (TableName::new("app", SHOWN), Table::default()),
        ]
        .into(),
        ..Default::default()
    };
    assert!(pbps_dialect::check_index_names(&postgres, &pbps_pg::Postgres::new()).is_empty());
    postgres.tables.remove(&TableName::new("app", SHOWN));
    postgres
        .tables
        .insert(TableName::new("app", NAME), Table::default());
    assert_eq!(
        pbps_dialect::check_index_names(&postgres, &pbps_pg::Postgres::new()).len(),
        1
    );
    let mut mssql = Schema {
        tables: [
            (
                owner,
                Table {
                    checks: [(
                        NAME.into(),
                        CheckConstraint {
                            expression: "1 = 1".into(),
                        },
                    )]
                    .into(),
                    ..Default::default()
                },
            ),
            (TableName::new("app", SHOWN), Table::default()),
        ]
        .into(),
        ..Default::default()
    };
    assert!(pbps_dialect::check_constraint_names(&mssql, &pbps_mssql::Mssql).is_empty());
    mssql.tables.remove(&TableName::new("app", SHOWN));
    mssql
        .tables
        .insert(TableName::new("app", NAME), Table::default());
    assert_eq!(
        pbps_dialect::check_constraint_names(&mssql, &pbps_mssql::Mssql).len(),
        1
    );
}

#[test]
fn diff_matches_raw_local_names_and_keeps_their_shown_lookalikes_distinct() {
    for dialect in [
        &pbps_pg::Postgres::new() as &dyn Dialect,
        &pbps_mssql::Mssql,
    ] {
        let (before, _, state) = artifacts(NAME, dialect);
        let (after, _, _) = artifacts(SHOWN, dialect);
        let base = pbps_diff::Side {
            schema: &before,
            ids: &state.ids,
        };
        assert!(
            pbps_diff::diff(base, base, dialect, &Default::default())
                .unwrap()
                .changes
                .is_empty()
        );
        let changes = pbps_diff::diff(
            base,
            pbps_diff::Side {
                schema: &after,
                ids: &state.ids,
            },
            dialect,
            &Default::default(),
        )
        .unwrap();
        for (old, new) in [
            (format!("{NAME}uq"), format!("{SHOWN}uq")),
            (format!("{NAME}fk"), format!("{SHOWN}fk")),
            (format!("{NAME}ck"), format!("{SHOWN}ck")),
            (format!("{NAME}ix"), format!("{SHOWN}ix")),
        ] {
            assert!(changes.changes.iter().any(|p| matches!(&p.change,
                Change::DropUnique { name, .. } | Change::DropForeignKey { name, .. } |
                Change::DropCheck { name, .. } | Change::DropIndex { name, .. } if name.raw() == old
            )), "{changes:?}");
            assert!(changes.changes.iter().any(|p| matches!(&p.change,
                Change::AddUnique { name, .. } | Change::AddForeignKey { name, .. } |
                Change::AddCheck { name, .. } | Change::AddIndex { name, .. } if name.raw() == new
            )), "{changes:?}");
        }
    }
}
