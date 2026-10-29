use super::*;

/// Native catalog identity is independent of both the old and final name:
/// an intermediate DROP belongs to its input UID, while its FK target is
/// read from the separate baseline snapshot (DEC-2296.9).
#[tokio::test]
#[ignore = "needs a live PostgreSQL; run scripts/live-tests-pg.sh"]
async fn a_renamed_dependent_keeps_its_old_target_between_address_versions() {
    use pbps_model::operation::{
        self, AddressChain, ModuleEdits, ObjectKey, Operations, Origin, Owners, PlanOwners,
        Snapshot, Source, Spelling,
    };
    use pbps_model::plan_names::PlanNames;
    use pbps_model::{Change, ChangeSet, IdsFile, PlannedChange, Schema, TableName, Uid};
    let mut db = TestDb::create("address_chain2296").await;
    db.conn.execute("CREATE SCHEMA app;
        CREATE TABLE app.u2(k integer PRIMARY KEY);
        CREATE TABLE app.u(k integer PRIMARY KEY, CONSTRAINT fk FOREIGN KEY(k) REFERENCES app.u2(k));
        INSERT INTO app.u2 VALUES(1); INSERT INTO app.u VALUES(1);").await.unwrap();
    let before = pbps_pg::catalog::introspect(&mut db.conn)
        .await
        .unwrap()
        .schema;
    let ids=db.conn.query("SELECT 'app.u'::regclass::oid::bigint AS own, 'app.u2'::regclass::oid::bigint AS target").await.unwrap();
    let own_oid = ids[0].try_get::<i64>("own").unwrap().unwrap();
    let target_oid = ids[0].try_get::<i64>("target").unwrap().unwrap();
    assert_ne!(own_oid, target_oid);
    let table = |s: &str| s.parse::<TableName>().unwrap();
    let uid = |s: &str| s.parse::<Uid>().unwrap();
    let base = IdsFile {
        tables: [
            (uid("t_aaaaaa"), table("app.u")),
            (uid("t_bbbbbb"), table("app.u2")),
        ]
        .into(),
        columns: [
            (uid("c_aaaaaa"), table("app.u").column("k")),
            (uid("c_bbbbbb"), table("app.u2").column("k")),
        ]
        .into(),
        ..Default::default()
    };
    let goal = IdsFile {
        tables: [
            (uid("t_aaaaaa"), table("app.u2")),
            (uid("t_bbbbbb"), table("app.v")),
        ]
        .into(),
        columns: [
            (uid("c_aaaaaa"), table("app.u2").column("k")),
            (uid("c_bbbbbb"), table("app.v").column("k")),
        ]
        .into(),
        ..Default::default()
    };
    let mut source = before.tables[&table("app.u")].clone();
    source.foreign_keys.clear();
    let after = Schema {
        tables: [
            (table("app.u2"), source),
            (table("app.v"), before.tables[&table("app.u2")].clone()),
        ]
        .into(),
        ..Default::default()
    };
    let steps = ChangeSet {
        changes: vec![
            Change::RenameTable {
                uid: uid("t_bbbbbb"),
                from: table("app.u2"),
                to: table("app.v"),
                defaults: vec![],
            },
            Change::RenameTable {
                uid: uid("t_aaaaaa"),
                from: table("app.u"),
                to: table("app.mid"),
                defaults: vec![],
            },
            Change::DropForeignKey {
                table: table("app.mid"),
                name: "fk".into(),
            },
            Change::RenameTable {
                uid: uid("t_aaaaaa"),
                from: table("app.mid"),
                to: table("app.u2"),
                defaults: vec![],
            },
        ]
        .into_iter()
        .map(PlannedChange::new)
        .collect(),
    };
    let raw = Operations::expanded(
        4,
        steps.changes.iter().enumerate().map(|(i, p)| {
            (
                if i == 2 {
                    Origin::Teardown(Source::Change(i))
                } else {
                    Origin::Whole(i)
                },
                p.clone(),
            )
        }),
        ModuleEdits::Rebuilt,
    );
    let names = PlanNames::of(&steps.changes, &goal).unwrap();
    let chain = AddressChain::bind(
        &names,
        &raw,
        &base,
        &goal,
        &Default::default(),
        &[raw.as_slice()[2].id()].into(),
    );
    let operations = raw.with_address_chain(chain);
    let op = &operations.as_slice()[2];
    assert!(matches!(op.origin().spelling(), Spelling::Input(_)));
    let owners = PlanOwners::new(PlanNames::of(&steps.changes, &goal).unwrap(), &base, &goal);
    let fx = operation::effects(
        op,
        &owners,
        Snapshot {
            schema: &before,
            ids: &base,
        },
        Snapshot {
            schema: &after,
            ids: &goal,
        },
    )
    .unwrap();
    let ObjectKey::Part { table: owner, .. } = op.key(&owners).unwrap() else {
        panic!()
    };
    assert_eq!(owner.uid(), &uid("t_aaaaaa"));
    assert_eq!(
        fx.bindings
            .iter()
            .map(|b| b.target.uid().clone())
            .collect::<Vec<_>>(),
        [uid("t_bbbbbb")]
    );
    assert_eq!(
        owners.table(&table("app.u2"), Spelling::Base).unwrap(),
        uid("t_bbbbbb")
    );
    db.conn
        .execute("ALTER TABLE app.u2 RENAME TO v; ALTER TABLE app.u RENAME TO mid;")
        .await
        .unwrap();
    let binding=db.conn.query("SELECT conrelid::bigint AS own, confrelid::bigint AS target FROM pg_catalog.pg_constraint
        WHERE conrelid='app.mid'::regclass AND conname='fk'").await.unwrap();
    assert_eq!(binding[0].try_get::<i64>("own").unwrap(), Some(own_oid));
    assert_eq!(
        binding[0].try_get::<i64>("target").unwrap(),
        Some(target_oid)
    );
    db.conn
        .execute("ALTER TABLE app.mid DROP CONSTRAINT fk; ALTER TABLE app.mid RENAME TO u2;")
        .await
        .unwrap();
    assert_eq!(
        number(&mut db.conn, "SELECT count(*)::integer FROM app.u2").await,
        1
    );
    let final_ids=db.conn.query("SELECT 'app.u2'::regclass::oid::bigint AS own, 'app.v'::regclass::oid::bigint AS target").await.unwrap();
    assert_eq!(final_ids[0].try_get::<i64>("own").unwrap(), Some(own_oid));
    assert_eq!(
        final_ids[0].try_get::<i64>("target").unwrap(),
        Some(target_oid)
    );
    db.drop().await;
}
