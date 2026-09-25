use super::TestDb;
use pbps_model::{Change, ChangeSet, PlannedChange, TableName, UidKind};
use pbps_mssql::estimate::{self, Reads, Rewrite, RowStorage};

include!("estimate_types.rs");

fn retype(table: &str, column: &str, from: &str, to: &str) -> PlannedChange {
    PlannedChange::new(Change::AlterColumnType {
        uid: pbps_model::Uid::generate(UidKind::Column),
        column: table.parse::<TableName>().unwrap().column(column),
        from: from.parse().unwrap(),
        to: to.parse().unwrap(),
        from_nullable: true,
        to_nullable: true,
    })
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn the_estimate_agrees_with_the_catalogues_row_update_and_scan_paths() {
    let mut db = TestDb::create("estimate_matrix255").await;
    for batch in include_str!("../estimate_matrix.sql").split("\nGO\n") {
        db.conn.execute(batch).await.unwrap();
    }
    let mut sql = String::new();
    for compression in ["NONE", "ROW"] {
        for (from, sample) in ESTIMATE_TYPES {
            for (to, _) in ESTIMATE_TYPES {
                sql.push_str(&format!(
                    "EXEC dbo.matrix_measure N'{from}',N'{to}',N'{}',64,'{compression}';\n",
                    sample.replace('\'', "''")
                ));
            }
        }
    }
    // Empty/populated controls distinguish row work from mere log volume.
    for (from, to) in [("int", "bigint"), ("decimal(10,2)", "decimal(12,2)")] {
        for n in [0, 32] {
            sql.push_str(&format!(
                "EXEC dbo.matrix_measure N'{from}',N'{to}',N'1', {n}, 'NONE';\n"
            ));
        }
    }
    db.conn.execute(&sql).await.unwrap();
    let rows = db
        .conn
        .query("SELECT * FROM dbo.matrix_results")
        .await
        .unwrap();
    assert_eq!(rows.len(), ESTIMATE_TYPES.len().pow(2) * 2 + 4);
    let mut accepted = 0;
    let mut refused = 0;
    let mut metadata = 0;
    for row in &rows {
        let stage = row.try_get::<&str>("stage").unwrap().unwrap();
        assert!(
            matches!(stage, "accepted" | "alter"),
            "fixture failed at {stage}"
        );
        if stage == "alter" {
            refused += 1;
            continue;
        }
        if row.try_get::<i32>("n").unwrap() != Some(64) {
            continue;
        }
        accepted += 1;
        let from = row.try_get::<&str>("src").unwrap().unwrap();
        let to = row.try_get::<&str>("dst").unwrap().unwrap();
        let compressed = row.try_get::<&str>("compression").unwrap() == Some("ROW");
        let updates = row.try_get::<i64>("updates_after").unwrap().unwrap()
            - row.try_get::<i64>("updates_before").unwrap().unwrap();
        let scans = row.try_get::<i64>("scans_after").unwrap().unwrap()
            - row.try_get::<i64>("scans_before").unwrap().unwrap();
        assert!(
            matches!(updates, 0 | 64),
            "{from} -> {to}: {updates} updates"
        );
        let got = estimate::column_work(
            &from.parse().unwrap(),
            &to.parse().unwrap(),
            true,
            true,
            if compressed {
                RowStorage::Compressed
            } else {
                RowStorage::Uncompressed
            },
        );
        let expected = if updates == 0 {
            metadata += 1;
            (Rewrite::No, Reads::Nothing)
        } else {
            (Rewrite::Yes, Reads::EveryRow)
        };
        assert_eq!(got, expected, "{from} -> {to}, compressed={compressed}");
        assert_eq!(scans, i64::from(updates != 0), "{from} -> {to}");
        assert_eq!(row.try_get::<&str>("locks").unwrap(), Some("Sch-M"));
        assert_eq!(
            row.try_get::<i64>("hobt_before").unwrap(),
            row.try_get::<i64>("hobt_after").unwrap()
        );
        assert_eq!(
            row.try_get::<i64>("au_before").unwrap(),
            row.try_get::<i64>("au_after").unwrap()
        );
    }
    assert!(
        accepted > 2800 && metadata > 100 && refused > 1000,
        "accepted={accepted}, metadata={metadata}, refused={refused}"
    );
    for (src, grows) in [("int", true), ("decimal(10,2)", false)] {
        let bytes = |n| {
            rows.iter()
                .find(|r| {
                    r.try_get::<&str>("src").unwrap() == Some(src)
                        && r.try_get::<i32>("n").unwrap() == Some(n)
                })
                .unwrap()
                .try_get::<i64>("log_bytes")
                .unwrap()
                .unwrap()
        };
        assert_eq!(bytes(32) > bytes(0), grows, "log volume for {src}");
    }
    for ty in ["timestamp", "rowversion", "not_a_sql_server_type"] {
        assert!(matches!(
            estimate::column_work(
                &ty.parse().unwrap(),
                &"bigint".parse().unwrap(),
                true,
                true,
                RowStorage::Uncompressed
            )
            .0,
            Rewrite::Unknown(_)
        ));
    }
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn nullability_and_storage_boundaries_keep_the_measured_row_paths() {
    let mut db = TestDb::create("estimate_boundaries255").await;
    for batch in include_str!("../estimate_matrix.sql").split("\nGO\n") {
        db.conn.execute(batch).await.unwrap();
    }
    let cases = [
        ("int", "int", "1"),
        ("int", "bigint", "1"),
        ("tinyint", "smallint", "1"),
        ("varchar(5)", "varchar(10)", "N'1'"),
        ("char(5)", "char(10)", "N'1'"),
        ("varchar(5)", "varchar(max)", "N'1'"),
        ("varchar(4000)", "varchar(8000)", "N'1'"),
        ("nvarchar(2000)", "nvarchar(4000)", "N'1'"),
        ("varbinary(4000)", "varbinary(8000)", "0x31"),
        ("char(4000)", "char(8000)", "N'1'"),
        ("nchar(2000)", "nchar(4000)", "N'1'"),
        ("decimal(9,2)", "decimal(10,2)", "1.25"),
        ("decimal(19,2)", "decimal(20,2)", "1.25"),
        ("datetime2(2)", "datetime2(3)", "'2000-01-02T00:00:00.123'"),
        ("time(4)", "time(5)", "'12:13:14.12345'"),
        ("sysname", "nvarchar(128)", "N'1'"),
        ("nvarchar(128)", "sysname", "N'1'"),
        ("text", "varchar(max)", "N'1'"),
        ("geography", "geography", "geography::Point(1,2,4326)"),
        ("geometry", "geometry", "geometry::Point(1,2,0)"),
    ];
    let mut sql = String::new();
    for compression in ["NONE", "ROW", "PAGE"] {
        for (from, to, sample) in cases {
            for (a, b) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                for clustered in [0, 1] {
                    sql.push_str(&format!("EXEC dbo.matrix_measure N'{from}',N'{to}',N'{}',64,'{compression}',{a},{b},{clustered};\n", sample.replace('\'', "''")));
                }
            }
        }
    }
    db.conn.execute(&sql).await.unwrap();
    let rows = db
        .conn
        .query("SELECT * FROM dbo.matrix_results")
        .await
        .unwrap();
    assert_eq!(rows.len(), cases.len() * 24);
    let mut accepted = 0;
    for row in &rows {
        if row.try_get::<&str>("stage").unwrap() != Some("accepted") {
            assert_eq!(row.try_get::<&str>("stage").unwrap(), Some("alter"));
            assert_eq!(row.try_get::<i32>("error_number").unwrap(), Some(1701));
            continue;
        }
        accepted += 1;
        let from = row.try_get::<&str>("src").unwrap().unwrap();
        let to = row.try_get::<&str>("dst").unwrap().unwrap();
        let a = row.try_get::<bool>("from_null").unwrap().unwrap();
        let b = row.try_get::<bool>("to_null").unwrap().unwrap();
        let compression = row.try_get::<&str>("compression").unwrap().unwrap();
        let storage = if compression == "NONE" {
            RowStorage::Uncompressed
        } else {
            RowStorage::Compressed
        };
        let updates = row.try_get::<i64>("updates_after").unwrap().unwrap()
            - row.try_get::<i64>("updates_before").unwrap().unwrap();
        let scans = row.try_get::<i64>("scans_after").unwrap().unwrap()
            - row.try_get::<i64>("scans_before").unwrap().unwrap();
        assert!(matches!(updates, 0 | 64));
        let expected = if updates == 0 {
            (Rewrite::No, Reads::Nothing)
        } else {
            (Rewrite::Yes, Reads::EveryRow)
        };
        assert_eq!(
            estimate::column_work(&from.parse().unwrap(), &to.parse().unwrap(), a, b, storage),
            expected,
            "{from} -> {to}, nullable {a} -> {b}, {compression}"
        );
        assert_eq!(scans, i64::from(updates != 0));
        assert_eq!(row.try_get::<&str>("locks").unwrap(), Some("Sch-M"));
    }
    assert!(accepted > 400);
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs live SQL Server"]
async fn connected_estimates_keep_storage_and_identity_uncertainty_visible() {
    let mut db = TestDb::create("estimate_context255").await;
    db.conn
        .execute("CREATE TABLE dbo.t(v int NULL); INSERT dbo.t VALUES(1),(2),(3);")
        .await
        .unwrap();
    let cs = ChangeSet {
        changes: vec![retype("dbo.t", "v", "int", "bigint")],
    };
    let mut e = estimate::planned_estimates(&cs).pop().unwrap().1;
    estimate::against(&mut db.conn, &mut e).await.unwrap();
    assert_eq!(
        (e.rewrite, e.reads, e.rows),
        (Rewrite::Yes, Reads::EveryRow, Some(3))
    );
    db.conn
        .execute("ALTER TABLE dbo.t REBUILD WITH(DATA_COMPRESSION=ROW)")
        .await
        .unwrap();
    let mut e = estimate::planned_estimates(&cs).pop().unwrap().1;
    estimate::against(&mut db.conn, &mut e).await.unwrap();
    assert_eq!((e.rewrite, e.reads), (Rewrite::No, Reads::Nothing));

    let column = retype("dbo.renamed", "amount", "int", "bigint");
    let Change::AlterColumnType { uid, .. } = &column.change else {
        unreachable!()
    };
    let uid = uid.clone();
    let renamed = ChangeSet {
        changes: vec![
            column.clone(),
            PlannedChange::new(Change::RenameColumn {
                uid,
                table: "dbo.renamed".parse().unwrap(),
                from: "v".into(),
                to: "amount".into(),
            }),
            PlannedChange::new(Change::RenameTable {
                uid: pbps_model::Uid::generate(UidKind::Table),
                from: "dbo.t".parse().unwrap(),
                to: "dbo.renamed".parse().unwrap(),
                defaults: Vec::new(),
            }),
        ],
    };
    let mut e = estimate::planned_estimates(&renamed).pop().unwrap().1;
    estimate::against(&mut db.conn, &mut e).await.unwrap();
    assert_eq!(
        (e.table.to_string(), e.rewrite, e.rows),
        ("dbo.renamed".into(), Rewrite::No, Some(3))
    );

    let mut column = cs.changes[0].clone();
    column.strategy.online = true;
    let online = ChangeSet {
        changes: vec![column],
    };
    let mut e = estimate::planned_estimates(&online).pop().unwrap().1;
    estimate::against(&mut db.conn, &mut e).await.unwrap();
    assert!(matches!(e.rewrite, Rewrite::Unknown(_)));
    assert!(e.lock.contains("unknown"));
    assert_eq!(e.rows, Some(3));

    // A same-name current table cannot supply a future identity's row count.
    let created = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::CreateTable {
                uid: pbps_model::Uid::generate(UidKind::Table),
                name: "dbo.t".parse().unwrap(),
                table: Box::default(),
            }),
            cs.changes[0].clone(),
        ],
    };
    let mut e = estimate::planned_estimates(&created).pop().unwrap().1;
    estimate::against(&mut db.conn, &mut e).await.unwrap();
    assert!(matches!(e.rewrite, Rewrite::Unknown(_)));
    assert!(e.rows.is_none() && e.rows_unknown.is_some());

    for context in [
        "CREATE INDEX ix ON dbo.t(v)",
        "DROP INDEX ix ON dbo.t; ALTER TABLE dbo.t ADD CONSTRAINT ck CHECK(v>0)",
        "DROP TABLE dbo.t",
    ] {
        db.conn.execute(context).await.unwrap();
        let mut e = estimate::planned_estimates(&cs).pop().unwrap().1;
        estimate::against(&mut db.conn, &mut e).await.unwrap();
        assert!(matches!(e.rewrite, Rewrite::Unknown(_)), "{context}");
        assert!(matches!(e.reads, Reads::Unknown(_)), "{context}");
    }
    db.drop().await;
}
