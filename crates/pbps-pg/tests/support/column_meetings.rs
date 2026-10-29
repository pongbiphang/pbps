//! Native membership observations; no production schema guard is bypassed.
use super::{TestDb, on_public, sqlstate};

async fn text(db: &mut TestDb, sql: &str) -> String {
    db.conn.query(sql).await.unwrap()[0]
        .try_get::<&str>("v")
        .unwrap()
        .unwrap()
        .to_owned()
}

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn default_transfers_and_restorations_match_both_native_cuts() {
    let mut db = TestDb::create("meeting_defaults2296").await;
    on_public(&mut db.conn).await;
    for parent_first in [false, true] {
        db.conn.execute("BEGIN; CREATE TABLE p(k integer,d integer DEFAULT 1) PARTITION BY RANGE(k); CREATE TABLE a(k integer,d integer DEFAULT 9)").await.unwrap();
        let change = "ALTER TABLE p ALTER COLUMN d SET DEFAULT 7";
        let movement = "ALTER TABLE p ATTACH PARTITION a FOR VALUES FROM(10) TO(20)";
        for sql in if parent_first {
            [change, movement]
        } else {
            [movement, change]
        } {
            db.conn.execute(sql).await.unwrap();
        }
        assert_eq!(
            text(
                &mut db,
                "SELECT pg_get_expr(adbin,adrelid) AS v FROM pg_attrdef WHERE adrelid='a'::regclass"
            )
            .await,
            if parent_first { "9" } else { "7" }
        );
        // A restoration's fallback is the value it writes, even when the
        // parent's declared goal is already different (#2504).
        db.conn
            .execute("ALTER TABLE a ALTER COLUMN d SET DEFAULT 3")
            .await
            .unwrap();
        assert_eq!(
            text(
                &mut db,
                "SELECT pg_get_expr(adbin,adrelid) AS v FROM pg_attrdef WHERE adrelid='a'::regclass"
            )
            .await,
            "3"
        );
        assert_eq!(
            text(
                &mut db,
                "SELECT pg_get_expr(adbin,adrelid) AS v FROM pg_attrdef WHERE adrelid='p'::regclass"
            )
            .await,
            "7"
        );
        db.conn
            .execute("ALTER TABLE ONLY a ALTER COLUMN d SET DEFAULT 9")
            .await
            .unwrap();
        assert_eq!(
            text(
                &mut db,
                "INSERT INTO a(k) VALUES(11) RETURNING d::text AS v"
            )
            .await,
            "9"
        );
        db.conn.execute("ALTER TABLE p RENAME TO q; ALTER TABLE q DETACH PARTITION a; ALTER TABLE q ALTER COLUMN d SET DEFAULT 13").await.unwrap();
        assert_eq!(
            text(
                &mut db,
                "INSERT INTO a(k) VALUES(12) RETURNING d::text AS v"
            )
            .await,
            "9"
        );
        db.conn.execute("ROLLBACK").await.unwrap();
    }
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn not_null_transfer_ownership_matches_the_versioned_policy() {
    let mut db = TestDb::create("meeting_nn2296").await;
    on_public(&mut db.conn).await;
    let version = text(&mut db, "SELECT current_setting('server_version_num') AS v")
        .await
        .parse::<i64>()
        .unwrap();
    for parent_first in [false, true] {
        db.conn.execute("BEGIN; CREATE TABLE p(k integer,d integer) PARTITION BY RANGE(k); CREATE TABLE a(k integer,d integer NOT NULL)").await.unwrap();
        let change = "ALTER TABLE p ALTER COLUMN d SET NOT NULL";
        let movement = "ALTER TABLE p ATTACH PARTITION a FOR VALUES FROM(10) TO(20)";
        for sql in if parent_first {
            [change, movement]
        } else {
            [movement, change]
        } {
            db.conn.execute(sql).await.unwrap();
        }
        assert_eq!(text(&mut db,"SELECT attnotnull::text AS v FROM pg_attribute WHERE attrelid='a'::regclass AND attname='d'").await,"true");
        if version >= 180000 {
            assert_eq!(text(&mut db,"SELECT conislocal::text AS v FROM pg_constraint WHERE conrelid='a'::regclass AND contype='n'").await, if parent_first {"false"} else {"true"});
        }
        db.conn.execute("ROLLBACK").await.unwrap();
    }
    db.conn.execute("BEGIN; CREATE TABLE p(k integer,d integer NOT NULL) PARTITION BY RANGE(k); CREATE TABLE a(k integer,d integer)").await.unwrap();
    let error = db
        .conn
        .execute("ALTER TABLE p ATTACH PARTITION a FOR VALUES FROM(10) TO(20)")
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&error), "42804");
    db.conn.execute("ROLLBACK").await.unwrap();
    db.drop().await;
}
