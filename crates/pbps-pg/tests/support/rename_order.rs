use super::*;
use pbps_model::{Change, Declared, ForeignKey, Index, IndexColumn, UniqueConstraint};

fn one_column() -> Table {
    let mut t = Table::default();
    t.columns
        .insert("id".into(), Column::new(ty("integer")).not_null());
    t
}

fn fixture(case: &str) -> (Schema, Schema, Vec<Intent>) {
    let mut base = Schema::default();
    let mut intents = Vec::new();
    if case == "owner_cycle" {
        let mut declared = Schema::default();
        for (old, new, blocked) in [
            ("app.old_a", "app.new_a", "new_b"),
            ("app.old_b", "app.new_b", "new_a"),
        ] {
            let mut t = one_column();
            t.unique.insert(
                blocked.into(),
                UniqueConstraint {
                    columns: vec!["id".into()],
                },
            );
            base.tables.insert(old.parse().unwrap(), t);
            declared.tables.insert(new.parse().unwrap(), one_column());
            intents.push(Intent::RenameTable {
                from: old.parse().unwrap(),
                to: new.parse().unwrap(),
            });
        }
        return (base, declared, intents);
    }
    if case.starts_with("index_") {
        let mut t = one_column();
        for name in ["target", "kept"] {
            t.indexes.insert(
                name.into(),
                Index {
                    columns: vec![IndexColumn {
                        name: "id".into(),
                        descending: false,
                    }],
                    include: vec![],
                    unique: false,
                    filter: Some("id > 0".into()),
                },
            );
        }
        base.tables.insert("app.old".parse().unwrap(), t);
        intents.push(Intent::RenameTable {
            from: "app.old".parse().unwrap(),
            to: if case == "index_cross" {
                "moved.target"
            } else {
                "app.target"
            }
            .parse()
            .unwrap(),
        });
    } else if case.starts_with("owner_") {
        let mut owner = one_column();
        if case == "owner_primary" {
            owner.primary_key = Some(PrimaryKey {
                name: Some("target".into()),
                columns: vec!["id".into()],
            });
        } else {
            owner.unique.insert(
                "target".into(),
                UniqueConstraint {
                    columns: vec!["id".into()],
                },
            );
        }
        base.tables.insert("app.owner_old".parse().unwrap(), owner);
        base.tables
            .insert("app.claimant_old".parse().unwrap(), one_column());
        intents.push(Intent::RenameTable {
            from: "app.owner_old".parse().unwrap(),
            to: "app.owner_new".parse().unwrap(),
        });
        intents.push(Intent::RenameTable {
            from: "app.claimant_old".parse().unwrap(),
            to: "app.target".parse().unwrap(),
        });
    } else {
        let mut owner = one_column();
        owner.unique.insert(
            "target".into(),
            UniqueConstraint {
                columns: vec!["id".into()],
            },
        );
        base.tables.insert("app.owner".parse().unwrap(), owner);
        let mut parent = one_column();
        parent.primary_key = Some(PrimaryKey {
            name: Some("pk_parent".into()),
            columns: vec!["id".into()],
        });
        base.tables.insert("app.parent".parse().unwrap(), parent);
        let mut child = one_column();
        child.foreign_keys.insert(
            "fk_child".into(),
            ForeignKey {
                columns: vec!["id".into()],
                references_table: if case == "fk_dependent" {
                    "app.owner"
                } else {
                    "app.parent"
                }
                .parse()
                .unwrap(),
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
        base.tables.insert("app.old".parse().unwrap(), child);
        intents.push(Intent::RenameTable {
            from: "app.old".parse().unwrap(),
            to: "app.target".parse().unwrap(),
        });
    }
    let mut declared = base.clone();
    for table in declared.tables.values_mut() {
        table.indexes.remove("target");
        table.unique.remove("target");
        table.foreign_keys.remove("fk_child");
        if table.primary_key.as_ref().and_then(|pk| pk.name.as_deref()) == Some("target") {
            table.primary_key = None;
        }
    }
    for intent in &intents {
        if let Intent::RenameTable { from, to } = intent {
            let table = declared.tables.remove(from).unwrap();
            declared.tables.insert(to.clone(), table);
        }
    }
    (base, declared, intents)
}

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn rename_dependencies_execute_with_their_statement_time_names() {
    let pg = Postgres::new();
    let mut results = Vec::new();
    for case in [
        "fk_unrelated",
        "fk_dependent",
        "index_same",
        "index_cross",
        "owner_unique",
        "owner_primary",
        "owner_cycle",
    ] {
        let (base, declared, intents) = fixture(case);
        let base_ids = mint_ids(&base, &IdsFile::default(), &[]);
        let declared_ids = mint_ids(&declared, &base_ids, &intents);
        let mut db = TestDb::create(&format!("rename_dependencies467_{case}")).await;
        db.conn
            .execute("CREATE SCHEMA app; CREATE SCHEMA moved")
            .await
            .unwrap();
        apply(
            &mut db.conn,
            &pg,
            &plan(&Schema::default(), &IdsFile::default(), &base, &base_ids),
        )
        .await;
        for name in base.tables.keys() {
            // Parents sort first in these fixtures. Preserve real rows across
            // all renames and drops instead of testing empty catalog objects.
            if name.name != "old" {
                db.conn
                    .execute(&format!(
                        "INSERT INTO {}.{} VALUES (7)",
                        name.schema, name.name
                    ))
                    .await
                    .unwrap();
            }
        }
        if base.tables.contains_key(&"app.old".parse().unwrap()) {
            db.conn
                .execute("INSERT INTO app.old VALUES (7)")
                .await
                .unwrap();
        }
        let pulled = pbps_pg::catalog::introspect(&mut db.conn).await.unwrap();
        let changes = plan(&pulled.schema, &base_ids, &declared, &declared_ids);
        let json = serde_json::to_string(&changes).unwrap();
        let replay: pbps_model::ChangeSet = serde_json::from_str(&json).unwrap();
        assert_eq!(replay, changes);
        let mut recorded = Declared::from_schema(&base);
        recorded.advance(&replay);
        let mut sql = Vec::new();
        let mut failure = None;
        db.conn.execute("BEGIN").await.unwrap();
        let blockers = pbps_pg::impact::drop_blockers(&mut db.conn, &replay).await;
        for p in &replay.changes {
            for stmt in pg.emit(&p.change, p.strategy).unwrap() {
                sql.push(stmt.sql.clone());
                if let Err(error) = db.conn.execute(&stmt.sql).await {
                    failure = Some((error.server_error_code(), error.to_string()));
                    break;
                }
            }
            if failure.is_some() {
                break;
            }
        }
        let mut converged = false;
        let mut empty_next = false;
        let mut rows_preserved = false;
        if failure.is_none() {
            db.conn.execute("COMMIT").await.unwrap();
            let after = pbps_pg::catalog::introspect(&mut db.conn).await.unwrap();
            let overlaid = recorded.overlay(&after.schema);
            converged = overlaid.tables == normalized(&declared).tables;
            empty_next = plan(&overlaid, &declared_ids, &declared, &declared_ids).is_empty();
            rows_preserved = true;
            for name in declared.tables.keys() {
                let rows = db
                    .conn
                    .query(&format!(
                        "SELECT count(*)::int8 AS n FROM {}.{} WHERE id=7",
                        name.schema, name.name
                    ))
                    .await
                    .unwrap();
                rows_preserved &= rows[0].try_get::<i64>("n").unwrap() == Some(1);
            }
        }
        if failure.is_some() {
            db.conn.execute("ROLLBACK").await.unwrap();
        }
        db.drop().await;
        results.push((
            case,
            replay,
            sql,
            failure,
            blockers,
            converged,
            empty_next,
            rows_preserved,
            recorded == Declared::from_schema(&declared),
        ));
    }
    // Drop every fixture even when the original sorter is restored for proof.
    let failures: Vec<_> = results
        .iter()
        .filter(|r| r.3.is_some() || !r.8)
        .map(|r| format!("{}: execution={:?}, record={}; {:?}", r.0, r.3, r.8, r.2))
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    for (case, changes, sql, failure, blockers, converged, empty_next, rows_preserved, recorded) in
        results
    {
        assert!(failure.is_none(), "{case}: {failure:?}; {sql:?}");
        assert!(
            blockers
                .as_ref()
                .is_ok_and(|reports| reports.iter().all(|r| r.blocking.is_empty())),
            "{case}: {blockers:?}"
        );
        assert!(
            converged && empty_next && rows_preserved && recorded,
            "{case}: convergence={converged}, next={empty_next}, rows={rows_preserved}, record={recorded}; {changes:?}"
        );
        if case == "fk_unrelated" {
            assert!(matches!(
                &changes.changes[0].change,
                Change::DropUnique { .. }
            ));
            assert!(
                matches!(&changes.changes[2].change, Change::DropForeignKey { table, .. } if table.to_string() == "app.target")
            );
        } else if case == "fk_dependent" {
            assert!(
                matches!(&changes.changes[0].change, Change::DropForeignKey { table, .. } if table.to_string() == "app.old")
            );
        } else if case == "index_cross" {
            // The freeing drop precedes the move, and so, since #969's review,
            // does the moved table's own dropped index: it has nothing to carry.
            let at = |needle: &str| sql.iter().position(|s| s.contains(needle)).unwrap();
            assert!(
                at("DROP INDEX \"app\".\"target\"") < at("SET SCHEMA"),
                "{sql:?}"
            );
            assert!(
                at("DROP INDEX \"app\".\"kept\"") < at("SET SCHEMA"),
                "{sql:?}"
            );
        } else if case == "owner_cycle" {
            assert!(
                matches!(&changes.changes[0].change, Change::DropUnique { table, .. } if table.name.starts_with("old_"))
            );
            assert!(
                matches!(&changes.changes[2].change, Change::DropUnique { table, .. } if table.name.starts_with("new_"))
            );
        } else if case.starts_with("owner_") {
            assert!(
                matches!(&changes.changes[0].change, Change::RenameTable { to, .. } if to.to_string() == "app.owner_new")
            );
            assert_eq!(
                changes.changes[1].change.table().unwrap().to_string(),
                "app.owner_new"
            );
        }
    }
}
