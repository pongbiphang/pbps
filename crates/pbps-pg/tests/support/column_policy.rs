//! Native observations for the staged column producer; guards remain enabled.
use super::{TestDb, on_public, sqlstate};

async fn version(db: &mut TestDb) -> i32 {
    db.conn
        .query("SELECT current_setting('server_version_num')::int AS v")
        .await
        .unwrap()[0]
        .try_get::<i32>("v")
        .unwrap()
        .unwrap()
}
async fn text(db: &mut TestDb, sql: &str) -> String {
    db.conn.query(sql).await.unwrap()[0]
        .try_get::<&str>("v")
        .unwrap()
        .unwrap()
        .to_owned()
}

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn column_statement_policies_match_native_recursion() {
    let mut db = TestDb::create("column_policy2296").await;
    on_public(&mut db.conn).await;
    let version = version(&mut db).await;
    for (setup, change, query, expected) in [
        (
            "",
            "ALTER TABLE p ADD COLUMN x integer DEFAULT 7",
            "SELECT column_default::text AS v FROM information_schema.columns WHERE table_name='a' AND column_name='x'",
            "7",
        ),
        (
            "",
            "ALTER TABLE p DROP COLUMN d",
            "SELECT count(*)::text AS v FROM pg_attribute WHERE attrelid='a'::regclass AND attname='d' AND NOT attisdropped",
            "0",
        ),
        (
            "",
            "ALTER TABLE p RENAME COLUMN d TO e",
            "SELECT count(*)::text AS v FROM pg_attribute WHERE attrelid='a'::regclass AND attname='e' AND NOT attisdropped",
            "1",
        ),
        (
            "",
            "ALTER TABLE p ALTER COLUMN d TYPE bigint",
            "SELECT format_type(atttypid,atttypmod) AS v FROM pg_attribute WHERE attrelid='a'::regclass AND attname='d'",
            "bigint",
        ),
        (
            "ALTER TABLE ONLY a ALTER COLUMN d SET DEFAULT 9",
            "ALTER TABLE p ALTER COLUMN d SET DEFAULT 7",
            "SELECT d::text AS v FROM a",
            "7",
        ),
        (
            "ALTER TABLE p ALTER COLUMN d SET DEFAULT 1; ALTER TABLE ONLY a ALTER COLUMN d SET DEFAULT 9",
            "ALTER TABLE p ALTER COLUMN d DROP DEFAULT",
            "SELECT count(*)::text AS v FROM pg_attrdef WHERE adrelid='a'::regclass",
            "0",
        ),
        (
            "",
            "CREATE SEQUENCE seq AS integer; ALTER SEQUENCE seq OWNED BY p.d; ALTER TABLE p ALTER COLUMN d SET DEFAULT nextval('seq')",
            "SELECT count(*)::text AS v FROM pg_attrdef WHERE adrelid='a'::regclass",
            "1",
        ),
        (
            "CREATE SEQUENCE seq AS integer OWNED BY p.d; ALTER TABLE p ALTER COLUMN d SET DEFAULT nextval('seq'); ALTER TABLE ONLY a ALTER COLUMN d SET DEFAULT 9",
            "ALTER SEQUENCE seq INCREMENT BY 2",
            "SELECT d::text AS v FROM a",
            "9",
        ),
        (
            "",
            "ALTER TABLE p RENAME TO q; ALTER TABLE q SET SCHEMA other",
            "SELECT inhparent::regclass::text AS v FROM pg_inherits WHERE inhrelid='a'::regclass",
            "other.q",
        ),
    ] {
        db.conn.execute("BEGIN; CREATE SCHEMA other; CREATE TABLE p(k integer,d integer) PARTITION BY RANGE(k); CREATE TABLE a PARTITION OF p FOR VALUES FROM(0) TO(10)").await.unwrap();
        if !setup.is_empty() {
            db.conn.execute(setup).await.unwrap();
        }
        db.conn.execute(change).await.unwrap();
        if query.contains("FROM a") {
            db.conn.execute("INSERT INTO a(k) VALUES(1)").await.unwrap();
        }
        assert_eq!(text(&mut db, query).await, expected, "{version}: {change}");
        db.conn.execute("ROLLBACK").await.unwrap();
    }
    // PG18 keeps the child's local NOT NULL through the parent's DROP;
    // PG16 has only the recursive effective-column flag.
    db.conn.execute("BEGIN; CREATE TABLE p(k integer,d integer NOT NULL) PARTITION BY RANGE(k); CREATE TABLE a PARTITION OF p FOR VALUES FROM(0) TO(10); ALTER TABLE ONLY a ALTER COLUMN d SET NOT NULL; ALTER TABLE p ALTER COLUMN d DROP NOT NULL").await.unwrap();
    assert_eq!(text(&mut db,"SELECT attnotnull::text AS v FROM pg_attribute WHERE attrelid='a'::regclass AND attname='d'").await, if version >= 180000 { "true" } else { "false" });
    db.conn.execute("ROLLBACK").await.unwrap();
    db.drop().await;
}

#[tokio::test]
#[ignore = "needs live PostgreSQL"]
async fn identity_capability_refusals_keep_their_native_sqlstates() {
    let mut db = TestDb::create("column_identity2296").await;
    on_public(&mut db.conn).await;
    let version = version(&mut db).await;
    db.conn.execute("BEGIN; CREATE TABLE p(k integer) PARTITION BY RANGE(k); CREATE TABLE a PARTITION OF p FOR VALUES FROM(0) TO(10)").await.unwrap();
    let add = db
        .conn
        .execute("ALTER TABLE p ADD COLUMN d integer GENERATED ALWAYS AS IDENTITY")
        .await;
    if version < 180000 {
        assert_eq!(sqlstate(&add.unwrap_err()), "42P16");
    } else {
        add.unwrap();
    }
    db.conn.execute("ROLLBACK").await.unwrap();
    db.conn.execute("BEGIN; CREATE TABLE p(k integer,d integer GENERATED BY DEFAULT AS IDENTITY) PARTITION BY RANGE(k); CREATE TABLE a(k integer,d integer GENERATED BY DEFAULT AS IDENTITY)").await.unwrap();
    let attach = db
        .conn
        .execute("ALTER TABLE p ATTACH PARTITION a FOR VALUES FROM(0) TO(10)")
        .await;
    if version >= 180000 {
        assert_eq!(sqlstate(&attach.unwrap_err()), "55000");
    } else {
        attach.unwrap();
        db.conn
            .execute("ALTER TABLE p ALTER COLUMN d DROP IDENTITY; CREATE SEQUENCE seq OWNED BY p.d")
            .await
            .unwrap();
        let refused = db
            .conn
            .execute("ALTER TABLE p ALTER COLUMN d SET DEFAULT nextval('seq')")
            .await
            .unwrap_err();
        assert_eq!(sqlstate(&refused), "42601");
    }
    db.conn.execute("ROLLBACK").await.unwrap();
    // Negative control: the parent's identity conversion itself is legal;
    // the producer's conservative refusal is about membership/readback.
    db.conn.execute("BEGIN; CREATE TABLE p(k integer,d integer NOT NULL) PARTITION BY RANGE(k); CREATE TABLE a PARTITION OF p FOR VALUES FROM(0) TO(10); ALTER TABLE p ALTER COLUMN d ADD GENERATED ALWAYS AS IDENTITY; ROLLBACK").await.unwrap();
    db.drop().await;
}
