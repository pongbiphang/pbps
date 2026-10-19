//! Captured on unchanged master c50c0db4 before #2084 step 2's production edits.
//! Collections exercise column identities independently of their printed spelling.

use pbps_dialect::Dialect;
use pbps_model::*;

const NAME: &str = "col\nline\r\u{1b}[31m\u{9b}\\\u{202e}名前";
const SHOWN: &str = r"col\nline\r\u{1b}[31m\u{9b}\\\u{202e}名前";

fn artifacts(text: &str, dialect: &dyn Dialect) -> (Schema, SavedPlan, StateSnapshot) {
    let parent = TableName::new("app", "parent");
    let name = TableName::new("app", "child");
    let payload = format!("{text}payload");
    let ordinary = || Column::new("int".parse().unwrap()).not_null();
    let mut table = Table {
        columns: [
            (text.into(), ordinary()),
            (payload.clone().into(), ordinary()),
        ]
        .into(),
        primary_key: Some(PrimaryKey {
            name: Some("pk_child".into()),
            columns: vec![text.into()],
            storage_parameters: Default::default(),
        }),
        unique: [(
            "uq_child".into(),
            UniqueConstraint {
                columns: vec![payload.clone().into()],
                storage_parameters: Default::default(),
            },
        )]
        .into(),
        indexes: [(
            "ix_child".into(),
            Index {
                columns: vec![IndexColumn::column(text)],
                include: vec![payload.clone().into()],
                unique: false,
                filter: None,
                method: Default::default(),
                storage_parameters: Default::default(),
            },
        )]
        .into(),
        foreign_keys: [(
            "fk_parent".into(),
            ForeignKey {
                columns: vec![payload.clone().into()],
                references_table: parent.clone(),
                references_columns: vec![text.into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        )]
        .into(),
        data: Some(TableData {
            mode: DataMode::Ensure,
            rows: [(
                "1".into(),
                Row([(payload.clone().into(), Value::Int(2))].into()),
            )]
            .into(),
        }),
        ..Default::default()
    };
    table
        .columns
        .get_mut(payload.as_str())
        .unwrap()
        .value_source = ValueSource::Default("2".into());
    if dialect.name() == "mssql" {
        table.computed.insert(
            format!("{text}computed").into(),
            ComputedColumn {
                expression: "1 + 1".into(),
                persisted: true,
                not_null: false,
            },
        );
    }
    let mut schema = Schema {
        tables: [
            (
                parent.clone(),
                Table {
                    columns: [(text.into(), ordinary())].into(),
                    ..Default::default()
                },
            ),
            (name.clone(), table),
        ]
        .into(),
        ..Default::default()
    };
    if dialect.name() == "postgres" {
        let partitioned = TableName::new("app", "partitioned");
        schema.tables.insert(
            partitioned.clone(),
            Table {
                columns: [(text.into(), ordinary())].into(),
                partition_by: Some(PartitionBy {
                    columns: vec![text.into()],
                    strategy: Default::default(),
                    external: false,
                }),
                ..Default::default()
            },
        );
        schema.tables.insert(
            TableName::new("app", "part"),
            Table {
                partition_of: Some(PartitionOf {
                    parent: partitioned,
                    bound: PartitionBound::Range {
                        from: vec![BoundDatum::Value("1".into())],
                        to: vec![BoundDatum::Value("9".into())],
                    },
                    columns: [(
                        text.into(),
                        PartitionColumn {
                            default: Some("3".into()),
                            not_null: true,
                        },
                    )]
                    .into(),
                }),
                ..Default::default()
            },
        );
    } else {
        let start = format!("{text}start");
        let end = format!("{text}end");
        schema.tables.insert(
            TableName::new("app", "temporal"),
            Table {
                columns: [
                    (
                        start.clone().into(),
                        Column::new("datetime2(7)".parse().unwrap()).not_null(),
                    ),
                    (
                        end.clone().into(),
                        Column::new("datetime2(7)".parse().unwrap()).not_null(),
                    ),
                ]
                .into(),
                system_time: Some(SystemTime {
                    start: start.into(),
                    end: end.into(),
                    hidden: false,
                    versioning: None,
                }),
                ..Default::default()
            },
        );
    }
    let ids = IdsFile {
        tables: schema
            .tables
            .keys()
            .enumerate()
            .map(|(i, name)| (format!("t_{i:06}").parse().unwrap(), name.clone()))
            .collect(),
        columns: [
            ("c_aaaaaa".parse().unwrap(), name.column(text)),
            ("c_bbbbbb".parse().unwrap(), name.column(&payload)),
        ]
        .into(),
        ..Default::default()
    };
    let mut changes: Vec<_> = schema
        .tables
        .iter()
        .enumerate()
        .map(|(i, (name, table))| {
            PlannedChange::new(Change::CreateTable {
                uid: format!("t_{i:06}").parse().unwrap(),
                name: name.clone(),
                table: Box::new(table.clone()),
                beside_default: None,
                moves_at_most: None,
            })
        })
        .collect();
    changes.push(PlannedChange::new(Change::UpdateRow {
        table: name,
        key_column: text.into(),
        key: "1".into(),
        columns: [(
            payload.clone().into(),
            (Cell::Value(Value::Int(1)), Cell::Value(Value::Int(2))),
        )]
        .into(),
        unchanged: Default::default(),
        types: [(payload.into(), "int".parse().unwrap())].into(),
        after_types: Default::default(),
        key_type: Some("int".parse().unwrap()),
    }));
    let plan = SavedPlan::new(
        PlanOrigin::Database,
        dialect.name(),
        "2026-10-19T00:00:00Z",
        PlanBaseline {
            time_zone: None,
            description: "column-name master fixture".into(),
            checksum: state_checksum(&schema, &ids),
            database_collation: None,
        },
        ChangeSet { changes },
        ids.clone(),
    );
    let mut state = StateSnapshot::new(StateKind::Apply, schema.clone(), ids, "fixture");
    state.declared = Declared::from_schema(&schema);
    state.plan_checksum = Some(plan.checksum());
    (schema, plan, state)
}

#[test]
fn column_collections_keep_master_bytes_checksums_sql_and_declarations() {
    for dialect in [
        &pbps_pg::Postgres::new() as &dyn Dialect,
        &pbps_mssql::Mssql,
    ] {
        let (schema, plan, state) = artifacts(NAME, dialect);
        let sql = plan
            .changes
            .changes
            .iter()
            .flat_map(|p| dialect.emit(&p.change, Default::default()).unwrap())
            .map(|s| s.sql)
            .collect::<Vec<_>>()
            .join("\n");
        let yaml = schema
            .tables
            .iter()
            .filter(|(_, t)| t.partition_of.is_none())
            .map(|(n, t)| {
                let partitions: Vec<_> = schema
                    .tables
                    .iter()
                    .filter(|(_, child)| {
                        child
                            .partition_of
                            .as_ref()
                            .is_some_and(|of| &of.parent == n)
                    })
                    .collect();
                pbps_load::fmt::render_partitioned(n, t, &partitions, &[], None)
            })
            .collect::<Vec<_>>()
            .join("\n---\n");
        let dir = std::env::temp_dir().join(format!(
            "pbps-column-compatibility-{}",
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
        let outputs = [
            ("schema.json", serde_json::to_string(&schema).unwrap()),
            ("ids.json", serde_json::to_string(&state.ids).unwrap()),
            ("plan.json", serde_json::to_string(&plan).unwrap()),
            ("state.json", serde_json::to_string(&state).unwrap()),
            ("plan.sha256", plan.checksum()),
            ("state.sha256", state_checksum(&schema, &state.ids)),
            ("statements.sql", sql),
            ("declarations.yml", yaml),
            ("explain.json", String::from_utf8(explain.stdout).unwrap()),
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/name-compatibility/columns")
            .join(dialect.name());
        for (name, actual) in outputs {
            assert_eq!(
                actual,
                std::fs::read_to_string(root.join(name)).unwrap(),
                "{name}"
            );
        }
        assert_eq!(
            serde_json::from_str::<SavedPlan>(&serde_json::to_string(&plan).unwrap()).unwrap(),
            plan
        );
        assert_eq!(
            StateSnapshot::from_json(&serde_json::to_string(&state).unwrap()).unwrap(),
            state
        );
        let (_, other, _) = artifacts(SHOWN, dialect);
        assert_ne!(plan.checksum(), other.checksum());
    }
}

#[test]
fn column_validation_preserves_the_whole_message_escape_boundary() {
    for dialect in [
        &pbps_pg::Postgres::new() as &dyn Dialect,
        &pbps_mssql::Mssql,
    ] {
        let table = Table {
            columns: [(NAME.into(), Column::new("int".parse().unwrap()))].into(),
            primary_key: Some(PrimaryKey {
                name: None,
                columns: vec![NAME.into()],
                storage_parameters: Default::default(),
            }),
            ..Default::default()
        };
        let found = dialect.validate_table(&TableName::new("app", "t"), &table);
        let explanation = if dialect.name() == "postgres" {
            ". This engine does not refuse the table -- it sets `NOT NULL` for you -- and then no plan can ever make the column match the declaration again."
        } else {
            ""
        };
        assert!(found.iter().any(|error| error.to_string() == format!(
            "{}: primary key column `{SHOWN}` is nullable; a primary key column must be NOT NULL{explanation}", dialect.name()
        )), "{found:?}");
        let mut ordinary = table;
        ordinary.columns.get_mut(NAME).unwrap().nullable = false;
        assert!(
            dialect
                .validate_table(&TableName::new("app", "t"), &ordinary)
                .is_empty()
        );
    }
}

#[test]
fn replica_identity_column_diagnostics_escape_once_at_dialect_validation() {
    let dialect = pbps_pg::Postgres::new();
    for (column, shown) in [
        (NAME, SHOWN),
        (r"path\part", r"path\\part"),
        ("ordinary", "ordinary"),
        ("名前", "名前"),
    ] {
        for kind in ["unique constraint", "index"] {
            let mut table = Table {
                columns: [(column.into(), Column::new("int".parse().unwrap()))].into(),
                ..Default::default()
            };
            if kind == "unique constraint" {
                table.unique.insert(
                    "ri".into(),
                    UniqueConstraint {
                        columns: vec![column.into()],
                        storage_parameters: Default::default(),
                    },
                );
                table.replica_identity = Some(ReplicaIdentity::Unique("ri".into()));
            } else {
                table.indexes.insert(
                    "ri".into(),
                    Index {
                        columns: vec![IndexColumn::column(column)],
                        include: Vec::new(),
                        unique: true,
                        filter: None,
                        method: Default::default(),
                        storage_parameters: Default::default(),
                    },
                );
                table.replica_identity = Some(ReplicaIdentity::Index("ri".into()));
            }
            let name = TableName::new("app", "t");
            let found: Vec<_> = dialect
                .validate_table(&name, &table)
                .into_iter()
                .map(|error| error.to_string())
                .collect();
            assert_eq!(
                found,
                [format!(
                    "postgres: `replica_identity` names {kind} `ri`, over nullable column(s) `{shown}`, which \
                     PostgreSQL does not take as a replica identity; declare them NOT NULL"
                )],
                "{kind}, {column:?}"
            );
            table.columns.get_mut(column).unwrap().nullable = false;
            assert!(dialect.validate_table(&name, &table).is_empty());
        }
    }
}

#[test]
fn row_read_refusals_escape_column_names_once() {
    let name = TableName::new("app", "t");
    let scope = RowScope::Every {
        known: Default::default(),
    };
    let mut table = Table {
        columns: [
            (NAME.into(), Column::new("int".parse().unwrap()).not_null()),
            ("ordinary".into(), Column::new("int".parse().unwrap())),
        ]
        .into(),
        primary_key: Some(PrimaryKey {
            name: None,
            columns: vec![NAME.into(), "ordinary".into()],
            storage_parameters: Default::default(),
        }),
        ..Default::default()
    };
    let expected = format!(
        "app.t: its rows cannot be read back -- its primary key has 2 columns ({SHOWN}, ordinary), and rows are keyed by one"
    );
    for error in [
        pbps_pg::rows::query(&name, &table, &scope)
            .unwrap_err()
            .to_string(),
        pbps_mssql::rows::query(&name, &table, &scope)
            .unwrap_err()
            .to_string(),
    ] {
        assert_eq!(error, expected);
    }
    table.primary_key.as_mut().unwrap().columns.pop();
    // Negative: the raw hostile identity still selects the supported key.
    assert!(pbps_pg::rows::query(&name, &table, &scope).is_ok());
    assert!(pbps_mssql::rows::query(&name, &table, &scope).is_ok());
    table.columns.get_mut(NAME).unwrap().ty = "integer[]".parse().unwrap();
    assert_eq!(
        pbps_pg::rows::query(&name, &table, &scope)
            .unwrap_err()
            .to_string(),
        format!(
            "app.t: its rows cannot be read back -- its column `{SHOWN}` is `integer[]`, an array, and pbps reads no rows of a table with an array column"
        )
    );
    table.columns.shift_remove(NAME);
    let expected = format!(
        "app.t: its rows cannot be read back -- its primary key column `{SHOWN}` is not among the columns read"
    );
    for error in [
        pbps_pg::rows::query(&name, &table, &scope)
            .unwrap_err()
            .to_string(),
        pbps_mssql::rows::query(&name, &table, &scope)
            .unwrap_err()
            .to_string(),
    ] {
        assert_eq!(error, expected);
    }
}

#[test]
fn cycling_expression_notes_keep_raw_column_identities_until_the_writer() {
    let mut column = Column::new("integer".parse().unwrap()).not_null();
    column.value_source = ValueSource::Identity(Identity {
        seed: 1,
        increment: 1,
        options: SequenceOptions {
            cycle: true,
            ..Default::default()
        },
    });
    let mut schema = Schema {
        tables: [(
            TableName::new("app", NAME),
            Table {
                columns: [(NAME.into(), column)].into(),
                indexes: [(
                    "expr".into(),
                    Index {
                        columns: vec![IndexColumn {
                            key: IndexKey::Expression(format!("(\"{NAME}\" + 0)")),
                            descending: false,
                            opclass: None,
                        }],
                        unique: true,
                        include: Vec::new(),
                        filter: None,
                        method: Default::default(),
                        storage_parameters: Default::default(),
                    },
                )]
                .into(),
                ..Default::default()
            },
        )]
        .into(),
        ..Default::default()
    };
    let dialect = pbps_pg::Postgres::new();
    let notes = dialect.declaration_notes(&schema);
    let reason = "has `cycle: true` and unique index `expr` has an expression over it: whether a number the sequence hands out again collides there depends on what the expression gives back, which is not checked";
    assert_eq!(notes, [format!("app.{NAME}.{NAME} {reason}")]);
    assert_eq!(
        pbps_model::escape::Escaped(&notes[0]).to_string(),
        format!("app.{SHOWN}.{SHOWN} {reason}")
    );
    let index = schema
        .tables
        .values_mut()
        .next()
        .unwrap()
        .indexes
        .get_mut("expr")
        .unwrap();
    index.columns.push(IndexColumn::column(SHOWN));
    assert_eq!(dialect.declaration_notes(&schema), notes);
    // Negative: a plain key on the same raw identity is refused separately.
    let index = schema
        .tables
        .values_mut()
        .next()
        .unwrap()
        .indexes
        .get_mut("expr")
        .unwrap();
    index.columns.last_mut().unwrap().key = IndexKey::Column(NAME.into());
    assert!(dialect.declaration_notes(&schema).is_empty());
}

#[test]
fn baseline_key_diagnostics_keep_the_previous_escaped_string_list() {
    let error = pbps_diff::DiffError::DataBaselineKeyNotSingle {
        table: TableName::new("app", "t"),
        columns: vec![NAME.into(), "ordinary".into()],
    };
    assert_eq!(
        error.to_string(),
        format!(
            "app.t has a baseline primary key with 2 columns ({:?}); pbps matches data rows only on a single-column key",
            [NAME, "ordinary"],
        )
    );
}
