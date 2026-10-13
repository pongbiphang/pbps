use super::*;
use pbps_model::{Change, ChangeSet, PlannedChange, Uid, change::DeleteCause};

/// A removal names its table at that step, even if another table later takes
/// that name. Distinct child counts expose either identity being confused.
#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn a_removal_keeps_its_stored_table_when_names_change_hands() {
    let mut db = TestDb::create("removal1770").await;
    for sql in [
        "CREATE TABLE public.p (id int NOT NULL PRIMARY KEY);",
        "CREATE TABLE public.a (id int NOT NULL PRIMARY KEY, parent int,
            CONSTRAINT fk_a FOREIGN KEY (parent) REFERENCES public.p(id));",
        "CREATE TABLE public.b (id int NOT NULL PRIMARY KEY, parent int,
            CONSTRAINT fk_b FOREIGN KEY (parent) REFERENCES public.p(id));",
        "INSERT INTO public.p VALUES (1), (2);",
        "INSERT INTO public.a VALUES (1, 1), (2, 1), (3, 1);",
        "INSERT INTO public.b VALUES (1, 1), (2, 2);",
    ] {
        db.conn.execute(sql).await.unwrap();
    }
    let table = |name: &str| TableName::new("public", name);
    let rename = |from: &str, to: &str| Change::RenameTable {
        uid: Uid::generate(pbps_model::UidKind::Table),
        from: table(from),
        to: table(to),
        defaults: Vec::new(),
    };
    let drop_a = || Change::DropTable {
        uid: Uid::generate(pbps_model::UidKind::Table),
        name: table("a"),
        detach_from: None,
    };
    let drop_fk = |name: &str| Change::DropForeignKey {
        table: table(name),
        name: "fk_a".into(),
    };
    let deleting = Change::DeleteRow {
        table: table("p"),
        key_column: "id".into(),
        key: "1".into(),
        cause: DeleteCause::Undeclared,
        row: Default::default(),
        dropped: Default::default(),
        types: Default::default(),
        after_types: Default::default(),
        key_type: None,
    };
    let dialect = Postgres::new();
    for (label, prefix, arrivals) in [
        ("drop then handoff", vec![drop_a(), rename("b", "a")], 0),
        ("drop without handoff", vec![drop_a()], 0),
        (
            "rename then drop key",
            vec![rename("a", "c"), drop_fk("c")],
            0,
        ),
        (
            "drop key before handoff",
            vec![drop_fk("a"), rename("a", "c"), rename("b", "a")],
            0,
        ),
        (
            "handoff with a planned child",
            vec![
                drop_a(),
                rename("b", "a"),
                Change::InsertRow {
                    table: table("a"),
                    key_column: "id".into(),
                    key: "3".into(),
                    identity_key: false,
                    row: [("parent".into(), pbps_model::Value::Int(1))]
                        .into_iter()
                        .collect(),
                    defaults: Default::default(),
                    types: [("parent".into(), "int".parse().unwrap())].into(),
                },
            ],
            1,
        ),
    ] {
        let mut changes: Vec<_> = prefix.into_iter().map(PlannedChange::new).collect();
        changes.push(PlannedChange::new(deleting.clone()));
        let cs = ChangeSet { changes };
        let report = dialect.fixture_preflight(&cs);
        assert!(report.unchecked.is_empty(), "{label}: {report:?}");
        let probes: Vec<_> = report
            .probes
            .iter()
            .filter(|p| {
                p.description
                    .starts_with("rows in other tables that still reference")
            })
            .collect();
        assert_eq!(probes.len(), 1, "{label}: {report:?}");
        for (referencing, stored) in [(true, 1), (false, 0)] {
            let expected = stored + arrivals;
            if !referencing {
                db.conn
                    .execute("DELETE FROM public.b WHERE id = 1;")
                    .await
                    .unwrap();
            }
            let count = counted(&mut db.conn, &probes[0].sql).await;
            assert_eq!(
                count, expected,
                "{label}: only b's surviving references count"
            );

            // Check the engine's own constraint after exactly this prefix.
            // Roll back so every case starts with the same names and keys.
            db.conn
                .begin(&dialect.transaction_framing(&pbps_model::SessionZone::utc()))
                .await
                .unwrap();
            for p in &cs.changes[..cs.changes.len() - 1] {
                for statement in dialect.emit(&p.change, p.strategy).unwrap() {
                    db.conn.execute(&statement.sql).await.unwrap();
                }
            }
            let result = db.conn.execute("DELETE FROM public.p WHERE id = 1;").await;
            assert_eq!(result.is_err(), expected > 0, "{label}: {result:?}");
            db.conn
                .rollback(&dialect.transaction_framing(&pbps_model::SessionZone::utc()))
                .await
                .unwrap();
            if expected == 0 {
                // The emitted delete's guard agrees with the raw DELETE too.
                db.conn
                    .begin(&dialect.transaction_framing(&pbps_model::SessionZone::utc()))
                    .await
                    .unwrap();
                for p in &cs.changes {
                    for statement in dialect.emit(&p.change, p.strategy).unwrap() {
                        db.conn.execute(&statement.sql).await.unwrap();
                    }
                }
                db.conn
                    .rollback(&dialect.transaction_framing(&pbps_model::SessionZone::utc()))
                    .await
                    .unwrap();
            }
        }
        db.conn
            .execute("INSERT INTO public.b VALUES (1, 1);")
            .await
            .unwrap();
    }
    db.drop().await;
}
