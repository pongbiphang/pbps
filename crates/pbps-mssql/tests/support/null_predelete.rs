use super::*;
use pbps_model::{Cell, Change, ChangeSet, PlannedChange, RowKey, Value};

fn changes(changes: Vec<Change>) -> ChangeSet {
    ChangeSet {
        changes: changes.into_iter().map(PlannedChange::new).collect(),
    }
}

fn delete_parent() -> Change {
    Change::DeleteRow {
        table: TableName::new("dbo", "p"),
        key_column: "id".into(),
        key: "1".into(),
        cause: pbps_model::change::DeleteCause::Undeclared,
        row: Default::default(),
        dropped: Default::default(),
        types: Default::default(),
        after_types: Default::default(),
        key_type: None,
    }
}

async fn rolled_back(conn: &mut Conn, cs: &ChangeSet) -> Result<(), String> {
    conn.begin(&Mssql.transaction_framing(&pbps_model::SessionZone::utc()))
        .await
        .unwrap();
    let mut result = Ok(());
    'plan: for p in &cs.changes {
        for statement in Mssql.emit(&p.change, p.strategy).unwrap() {
            if let Err(error) = conn.execute(&statement.sql).await {
                result = Err(format!("{}\n{error}", statement.sql));
                break 'plan;
            }
        }
    }
    conn.rollback(&Mssql.transaction_framing(&pbps_model::SessionZone::utc()))
        .await
        .unwrap();
    result
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_null_default_moves_a_reference_off_the_deleted_parent() {
    let mut db = TestDb::create("null_default1771").await;
    db.conn
        .execute("CREATE TABLE dbo.p (id int PRIMARY KEY); INSERT dbo.p VALUES (1), (2);")
        .await
        .unwrap();
    let mut observations = Vec::new();
    for (default, expected) in [
        ("NULL", 0),
        ("((NULL))", 0),
        ("(NULL /* no reference */)", 0),
        ("1", 1),
        ("2", 0),
    ] {
        db.conn
            .execute(&format!(
                "CREATE TABLE dbo.c (id int PRIMARY KEY, parent int NULL DEFAULT {default},
             CONSTRAINT fk_c FOREIGN KEY (parent) REFERENCES dbo.p(id));
             INSERT dbo.c VALUES (1, 1);"
            ))
            .await
            .unwrap();
        for legacy in [false, true] {
            let update = Change::UpdateRow {
                table: TableName::new("dbo", "c"),
                key_column: "id".into(),
                key: RowKey::from("1"),
                columns: [(
                    "parent".into(),
                    (Cell::Value(Value::Int(1)), Cell::Default(default.into())),
                )]
                .into(),
                unchanged: Default::default(),
                types: if legacy {
                    Default::default()
                } else {
                    [("parent".into(), ty("int"))].into()
                },
                after_types: Default::default(),
                key_type: None,
            };
            let cs = changes(vec![update, delete_parent()]);
            let counts = probe_counts(&mut db.conn, &cs).await;
            let applied = rolled_back(&mut db.conn, &cs).await;
            observations.push((default, legacy, expected, counts, applied));
        }
        db.conn.execute("DROP TABLE dbo.c;").await.unwrap();
    }
    db.drop().await;
    for (default, legacy, expected, counts, applied) in observations {
        assert_eq!(
            applied.is_ok(),
            expected == 0,
            "{default}, legacy={legacy}: {applied:?}"
        );
        let count = counts
            .iter()
            .find(|(description, _)| {
                description.starts_with("rows in other tables that still reference")
            })
            .unwrap();
        assert_eq!(count.1, expected, "{default}, legacy={legacy}: {counts:?}");
        if expected == 0 {
            assert!(counts.iter().all(|(_, n)| *n == 0), "{default}: {counts:?}");
        }
    }
}

fn update_child(key: &str, after: Vec<(&str, Cell)>, unchanged: Vec<(&str, Cell)>) -> Change {
    Change::UpdateRow {
        table: TableName::new("dbo", "c"),
        key_column: "id".into(),
        key: key.into(),
        columns: after
            .into_iter()
            .map(|(c, v)| (c.into(), (Cell::Value(Value::Int(1)), v)))
            .collect(),
        unchanged: unchanged.into_iter().map(|(c, v)| (c.into(), v)).collect(),
        types: [
            ("a".into(), ty("int")),
            ("b".into(), ty("int")),
            ("extra".into(), ty("int")),
        ]
        .into(),
        after_types: Default::default(),
        key_type: None,
    }
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_known_null_decides_only_its_own_rows_foreign_key_tuple() {
    let mut db = TestDb::create("null_tuple1771").await;
    db.conn
        .execute("CREATE FUNCTION dbo.one() RETURNS int AS BEGIN RETURN 1 END;")
        .await
        .unwrap();
    db.conn.execute("CREATE TABLE dbo.p (id int PRIMARY KEY, a int, b int, UNIQUE(a,b)); INSERT dbo.p VALUES (1,1,1);").await.unwrap();
    let mut observations = Vec::new();
    for case in [
        "written",
        "default",
        "held",
        "held default",
        "insert",
        "omitted",
        "non-null",
        "two unknowns",
        "other row",
        "other key",
        "overridden default",
    ] {
        let a_default = if case == "two unknowns" {
            "dbo.one()"
        } else {
            "NULL"
        };
        db.conn.execute(&format!("CREATE TABLE dbo.c (id int PRIMARY KEY, a int NULL DEFAULT {a_default}, b int NULL DEFAULT dbo.one(), extra int NULL DEFAULT dbo.one(), CONSTRAINT fk_pair FOREIGN KEY (a,b) REFERENCES dbo.p(a,b));")).await.unwrap();
        let null = Cell::Value(Value::Null);
        let default = Cell::Default("NULL".into());
        let unknown = Cell::Default("dbo.one()".into());
        let mut steps = Vec::new();
        let valid = ![
            "non-null",
            "two unknowns",
            "other row",
            "other key",
            "overridden default",
        ]
        .contains(&case);
        if ["insert", "omitted", "overridden default"].contains(&case) {
            steps.push(Change::InsertRow {
                table: TableName::new("dbo", "c"),
                key_column: "id".into(),
                key: "1".into(),
                identity_key: false,
                row: pbps_model::Row(match case {
                    "insert" => [("a".into(), Value::Null)].into(),
                    "overridden default" => [("a".into(), Value::Int(1))].into(),
                    _ => Default::default(),
                }),
                defaults: [
                    ("a".into(), "NULL".into()),
                    ("b".into(), "dbo.one()".into()),
                    ("extra".into(), "dbo.one()".into()),
                ]
                .into(),
                types: [
                    ("a".into(), ty("int")),
                    ("b".into(), ty("int")),
                    ("extra".into(), ty("int")),
                ]
                .into(),
            });
        } else {
            let held = ["held", "held default", "other key"].contains(&case);
            db.conn
                .execute(if held {
                    "INSERT dbo.c VALUES (1,NULL,1,1);"
                } else {
                    "INSERT dbo.c VALUES (1,1,1,1);"
                })
                .await
                .unwrap();
            let unchanged = if held {
                vec![(
                    "a",
                    if case == "held default" {
                        default.clone()
                    } else {
                        null.clone()
                    },
                )]
            } else {
                vec![]
            };
            let mut after = vec![("b", unknown.clone())];
            if case == "two unknowns" {
                after.push(("a", unknown.clone()));
            }
            if ["written", "default", "other row"].contains(&case) {
                after.push(("a", if case == "default" { default } else { null }));
            }
            if case == "other key" {
                db.conn.execute("ALTER TABLE dbo.c ADD CONSTRAINT fk_extra FOREIGN KEY (extra) REFERENCES dbo.p(id);").await.unwrap();
                after.push(("extra", unknown.clone()));
            }
            steps.push(update_child("1", after, unchanged));
            if case == "other row" {
                db.conn
                    .execute("INSERT dbo.c VALUES (2,1,1,1);")
                    .await
                    .unwrap();
                steps.push(update_child("2", vec![("b", unknown)], vec![]));
            }
        }
        steps.push(delete_parent());
        let cs = changes(steps);
        let counts = probe_counts(&mut db.conn, &cs).await;
        let applied = rolled_back(&mut db.conn, &cs).await;
        observations.push((case, valid, counts, applied));
        db.conn.execute("DROP TABLE dbo.c;").await.unwrap();
    }
    db.drop().await;
    for (case, valid, _, applied) in &observations {
        assert_eq!(applied.is_ok(), *valid, "{case}: {applied:?}");
    }
    for (case, valid, counts, _) in observations {
        if case == "two unknowns" {
            assert_eq!(
                counts
                    .iter()
                    .find(|(description, _)| description
                        .contains("default the probe cannot evaluate"))
                    .unwrap()
                    .1,
                2,
                "{counts:?}"
            );
        }
        assert_eq!(
            counts.iter().all(|(_, n)| *n == 0),
            valid,
            "{case}: {counts:?}"
        );
    }
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_check_does_not_read_the_column_giving_up_its_name() {
    let mut db = TestDb::create("vacated_check1771").await;
    db.conn
        .execute(&format!(
            "ALTER DATABASE [{}] COLLATE Latin1_General_100_CI_AI;",
            db.name
        ))
        .await
        .unwrap();
    let table = TableName::new("dbo", "t");
    let rename = |from: &str, to: &str| Change::RenameColumn {
        uid: Uid::generate(pbps_model::UidKind::Column),
        table: table.clone(),
        from: from.into(),
        to: to.into(),
        table_was: None,
    };
    let computed = pbps_model::ComputedColumn {
        expression: "m".into(),
        persisted: true,
        not_null: false,
    };
    let mut observations = Vec::new();
    for case in [
        "drop rename",
        "drop rename case",
        "drop rename accent",
        "rename add",
        "rename computed",
        "drop computed rename",
        "alone",
        "unrelated rename",
        "rename case",
        "unrelated drop",
    ] {
        db.conn
            .execute(if case == "drop computed rename" {
                "CREATE TABLE dbo.t (m int, n AS (-m) PERSISTED); INSERT dbo.t(m) VALUES (1);"
            } else {
                "CREATE TABLE dbo.t (n int, m int); INSERT dbo.t VALUES (-1,1);"
            })
            .await
            .unwrap();
        let drop = |column: &str| Change::DropColumn {
            uid: Uid::generate(pbps_model::UidKind::Column),
            column: table.column(column),
        };
        let mut steps = match case {
            "drop rename" => vec![drop("n"), rename("m", "n")],
            "drop rename case" => vec![drop("n"), rename("m", "N")],
            "drop rename accent" => vec![drop("n"), rename("m", "ń")],
            "rename add" => vec![
                rename("n", "old_n"),
                Change::AddColumn {
                    uid: Uid::generate(pbps_model::UidKind::Column),
                    table: table.clone(),
                    name: "n".into(),
                    column: Box::new(Column {
                        ty: ty("int"),
                        nullable: false,
                        value_source: pbps_model::ValueSource::Default("1".into()),
                        ..Column::new(ty("int"))
                    }),
                },
            ],
            "rename computed" => vec![
                rename("n", "old_n"),
                Change::AddComputedColumn {
                    table: table.clone(),
                    name: "n".into(),
                    computed: computed.clone(),
                },
            ],
            "drop computed rename" => vec![
                Change::DropComputedColumn {
                    table: table.clone(),
                    name: "n".into(),
                    computed: pbps_model::ComputedColumn {
                        expression: "-m".into(),
                        ..computed.clone()
                    },
                },
                rename("m", "n"),
            ],
            "unrelated rename" => vec![rename("m", "m2")],
            "rename case" => vec![rename("n", "N")],
            "unrelated drop" => vec![drop("m")],
            _ => vec![],
        };
        steps.push(Change::AddCheck {
            table: table.clone(),
            name: "positive".into(),
            constraint: pbps_model::CheckConstraint {
                expression: match case {
                    "drop rename case" | "rename case" => "N > 0",
                    "drop rename accent" => "[ń] > 0",
                    _ => "n > 0",
                }
                .into(),
            },
        });
        let cs = changes(steps);
        let counts = probe_counts(&mut db.conn, &cs).await;
        let unchecked = Mssql.fixture_preflight(&cs).unchecked;
        let applied = rolled_back(&mut db.conn, &cs).await;
        observations.push((case, counts, unchecked, applied));
        db.conn.execute("DROP TABLE dbo.t;").await.unwrap();
    }
    db.drop().await;
    for (case, counts, unchecked, applied) in observations {
        let handoff = [
            "drop rename",
            "drop rename case",
            "drop rename accent",
            "rename add",
            "rename computed",
            "drop computed rename",
        ]
        .contains(&case);
        assert_eq!(applied.is_ok(), handoff, "{case}: {applied:?}");
        if handoff {
            assert!(counts.is_empty(), "{case}: {counts:?}");
            assert_eq!(
                unchecked
                    .iter()
                    .filter(|u| u.description.starts_with("new check"))
                    .count(),
                1,
                "{case}: {unchecked:?}"
            );
        } else {
            assert!(applied.unwrap_err().contains("CHECK constraint"), "{case}");
            assert_eq!(counts.len(), 1);
            assert_eq!(counts[0].1, 1, "{case}: {counts:?}");
            assert!(unchecked.is_empty());
        }
    }
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_planned_foreign_key_counts_references_after_parent_deletion() {
    let mut db = TestDb::create("planned_fk1771").await;
    db.conn.execute("CREATE TABLE dbo.p (id int PRIMARY KEY); INSERT dbo.p VALUES (1); CREATE TABLE dbo.c (id int PRIMARY KEY, a int NULL DEFAULT NULL); INSERT dbo.c VALUES (1,1);").await.unwrap();
    let mut observations = Vec::new();
    for action in ["stay", "null", "delete"] {
        let mut steps = match action {
            "null" => {
                let mut update =
                    update_child("1", vec![("a", Cell::Default("NULL".into()))], vec![]);
                if let Change::UpdateRow { types, .. } = &mut update {
                    types.retain(|name, _| name == "a");
                }
                vec![update]
            }
            "delete" => {
                let mut deleted = delete_parent();
                if let Change::DeleteRow { table, .. } = &mut deleted {
                    *table = TableName::new("dbo", "c");
                }
                vec![deleted]
            }
            _ => vec![],
        };
        steps.push(delete_parent());
        steps.push(Change::AddForeignKey {
            table: TableName::new("dbo", "c"),
            name: "fk_c".into(),
            constraint: Box::new(ForeignKey {
                columns: vec!["a".into()],
                references_table: TableName::new("dbo", "p"),
                references_columns: vec!["id".into()],
                on_delete: ReferentialAction::NoAction,
                on_update: ReferentialAction::NoAction,
            }),
        });
        let cs = changes(steps);
        observations.push((
            action,
            probe_counts(&mut db.conn, &cs).await,
            rolled_back(&mut db.conn, &cs).await,
        ));
    }
    db.drop().await;
    for (action, counts, applied) in observations {
        assert_eq!(applied.is_ok(), action != "stay", "{action}: {applied:?}");
        assert_eq!(
            counts.iter().all(|(_, n)| *n == 0),
            action != "stay",
            "{action}: {counts:?}"
        );
    }
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn a_legacy_null_default_needs_no_catalog_assignment_type() {
    let mut db = TestDb::create("null_clr1828").await;
    db.conn
        .execute(
            "CREATE TABLE dbo.p (id int PRIMARY KEY, h hierarchyid UNIQUE);
        INSERT dbo.p VALUES (1, hierarchyid::Parse('/1/'));
        CREATE TABLE dbo.c (id int PRIMARY KEY, h hierarchyid NULL DEFAULT NULL,
            CONSTRAINT fk_c FOREIGN KEY (h) REFERENCES dbo.p(h));
        INSERT dbo.c VALUES (1, hierarchyid::Parse('/1/'));",
        )
        .await
        .unwrap();
    let mut observations = Vec::new();
    for remaining in [false, true] {
        if remaining {
            db.conn
                .execute("INSERT dbo.c VALUES (2, hierarchyid::Parse('/1/'));")
                .await
                .unwrap();
        }
        for legacy in [false, true] {
            for clear in [false, true] {
                let value = Cell::Value(Value::Text("/1/".into()));
                let cs = changes(vec![
                    Change::UpdateRow {
                        table: TableName::new("dbo", "c"),
                        key_column: "id".into(),
                        key: "1".into(),
                        columns: [(
                            "h".into(),
                            (
                                value.clone(),
                                if clear {
                                    Cell::Default("NULL".into())
                                } else {
                                    value
                                },
                            ),
                        )]
                        .into(),
                        unchanged: Default::default(),
                        types: if legacy {
                            Default::default()
                        } else {
                            [("h".into(), ty("hierarchyid"))].into()
                        },
                        after_types: Default::default(),
                        key_type: None,
                    },
                    delete_parent(),
                ]);
                observations.push((
                    remaining,
                    legacy,
                    clear,
                    probe_counts(&mut db.conn, &cs).await,
                    rolled_back(&mut db.conn, &cs).await,
                ));
            }
        }
    }
    db.drop().await;
    for (remaining, legacy, clear, counts, applied) in observations {
        let expected = i32::from(!clear) + i32::from(remaining);
        assert_eq!(
            applied.is_ok(),
            expected == 0,
            "remaining={remaining}, legacy={legacy}, clear={clear}: {applied:?}"
        );
        let count = counts
            .iter()
            .find(|(description, _)| {
                description.starts_with("rows in other tables that still reference")
            })
            .unwrap()
            .1;
        assert_eq!(
            count, expected,
            "remaining={remaining}, legacy={legacy}, clear={clear}: {counts:?}"
        );
    }
}
