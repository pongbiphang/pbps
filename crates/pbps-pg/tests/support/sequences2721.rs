//! Standalone sequences against the engine (#2721 PR A, DEC-2721.1): the facts the model rests
//! on, asserted on the 16 and the 18 fixture, and the pull's own query and decoder against what
//! the catalog really holds. Each case runs in a database of its own.
use super::{sqlstate, text, truth};
use pbps_db::{Conn, Driver};
use pbps_pg::sequence::{decode, held_predicate, query};

const SERVERS: [&str; 2] = ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"];

struct Private {
    admin: Conn,
    name: String,
    conn: Option<Conn>,
}

impl Private {
    async fn on(variable: &str, case: &str) -> Private {
        let url = std::env::var(variable).expect("the PostgreSQL 16/18 fixture");
        let mut admin = Conn::connect(Driver::Postgres, &url).await.unwrap();
        let name = format!("pbps_seq2721_{case}_{}", std::process::id());
        admin
            .execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
            .await
            .unwrap();
        admin
            .execute(&format!("CREATE DATABASE {name}"))
            .await
            .unwrap();
        let own = url.replace("dbname=pbps_test", &format!("dbname={name}"));
        assert_ne!(own, url, "the fixture URL names its database");
        let conn = Conn::connect(Driver::Postgres, &own).await.unwrap();
        let mut db = Private {
            admin,
            name,
            conn: Some(conn),
        };
        db.run("CREATE SCHEMA app; SET search_path = app, pg_catalog")
            .await;
        db
    }

    fn conn(&mut self) -> &mut Conn {
        self.conn.as_mut().unwrap()
    }

    async fn run(&mut self, sql: &str) {
        self.conn()
            .execute(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }

    async fn refused(&mut self, sql: &str) -> String {
        let Err(error) = self.conn().execute(sql).await else {
            panic!("the engine accepted: {sql}")
        };
        sqlstate(&error).to_owned()
    }

    async fn one(&mut self, sql: &str) -> String {
        text(self.conn(), sql).await
    }

    /// The pull's own query, decoded.
    async fn held(
        &mut self,
    ) -> std::collections::BTreeMap<pbps_model::ObjectName, pbps_model::Sequence> {
        let rows = self
            .one(&format!(
                "SELECT COALESCE(jsonb_agg(to_jsonb(q)), '[]'::jsonb)::text FROM ({}) q",
                query()
            ))
            .await;
        let rows: Vec<serde_json::Value> = serde_json::from_str(&rows).unwrap();
        decode(&rows).unwrap()
    }

    async fn finish(mut self) {
        drop(self.conn.take());
        let _ = self
            .admin
            .execute(&format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                self.name
            ))
            .await;
    }
}

/// What the model rests on, measured: `pg_sequence` has no column for where the counter stands,
/// and `nextval`, `setval` and a restart leave every one of its columns as they were while
/// `pg_sequences.last_value` moves; an `UNLOGGED` sequence is `relpersistence = 'u'`; a column
/// ownership is a `pg_depend` row with a column number and type `a` (`serial`, `OWNED BY`) or
/// `i` (an identity); a package's member has type `e`; a sequence and a table of one name
/// refuse each other; a descending sequence starts at its maximum unless told otherwise.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn measure() {
    let _live = super::live_ledger().await;
    for variable in SERVERS {
        let mut db = Private::on(variable, "measure").await;
        // No column of `pg_sequence` holds the counter.
        assert!(
            !truth(
                db.conn(),
                "SELECT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid = 'pg_sequence'::regclass \
                 AND attnum > 0 AND attname IN ('last_value', 'is_called'))"
            )
            .await
        );
        assert_eq!(
            db.one("SELECT string_agg(attname, ',' ORDER BY attnum) FROM pg_attribute WHERE attrelid = 'pg_sequence'::regclass AND attnum > 0").await,
            "seqrelid,seqtypid,seqstart,seqincrement,seqmax,seqmin,seqcache,seqcycle"
        );
        // The counter moves and the row the pull reads does not.
        db.run("CREATE SEQUENCE s AS integer START 10 INCREMENT 5")
            .await;
        let row = "SELECT seqtypid::text || ',' || seqstart || ',' || seqincrement || ',' || seqmin || ',' || seqmax || ',' || seqcache || ',' || seqcycle FROM pg_sequence WHERE seqrelid = 'app.s'::regclass";
        let before = db.one(row).await;
        assert!(
            !truth(
                db.conn(),
                "SELECT last_value IS NOT NULL FROM pg_sequences WHERE schemaname = 'app' AND sequencename = 's'"
            )
            .await
        );
        db.run("SELECT nextval('app.s'); SELECT nextval('app.s'); SELECT setval('app.s', 500)")
            .await;
        assert!(
            truth(
                db.conn(),
                "SELECT last_value = 500 FROM pg_sequences WHERE schemaname = 'app' AND sequencename = 's'"
            )
            .await
        );
        assert_eq!(
            db.one(row).await,
            before,
            "the counter is not in pg_sequence"
        );
        db.run("ALTER SEQUENCE s RESTART WITH 12").await;
        assert_eq!(
            db.one(row).await,
            before,
            "a restart is not in pg_sequence either"
        );
        // Persistence, ownership and membership.
        db.run("CREATE UNLOGGED SEQUENCE u; CREATE TABLE t (id serial, n integer GENERATED ALWAYS AS IDENTITY); CREATE SEQUENCE o OWNED BY t.id").await;
        assert_eq!(
            db.one("SELECT relpersistence::text FROM pg_class WHERE oid = 'app.u'::regclass")
                .await,
            "u"
        );
        assert_eq!(
            db.one("SELECT relpersistence::text FROM pg_class WHERE oid = 'app.s'::regclass")
                .await,
            "p"
        );
        for (sequence, deptype) in [("t_id_seq", "a"), ("o", "a"), ("t_n_seq", "i")] {
            assert_eq!(
                db.one(&format!("SELECT d.deptype::text || d.refobjsubid FROM pg_depend d WHERE d.classid = 'pg_class'::regclass AND d.objid = 'app.{sequence}'::regclass AND d.refclassid = 'pg_class'::regclass AND d.refobjsubid > 0")).await,
                format!("{deptype}{}", if sequence == "t_n_seq" { 2 } else { 1 }),
                "{sequence}"
            );
        }
        db.run("CREATE EXTENSION unaccent SCHEMA public; CREATE SEQUENCE pkg; ALTER EXTENSION unaccent ADD SEQUENCE pkg").await;
        assert_eq!(db.one("SELECT d.deptype::text FROM pg_depend d WHERE d.classid = 'pg_class'::regclass AND d.objid = 'app.pkg'::regclass AND d.refclassid = 'pg_extension'::regclass").await, "e");
        // One namespace with tables.
        assert_eq!(db.refused("CREATE TABLE s (x integer)").await, "42P07");
        assert_eq!(db.refused("CREATE SEQUENCE t").await, "42P07");
        // A descending sequence starts at its maximum, a bound the model leaves as `None`.
        db.run("CREATE SEQUENCE down AS smallint INCREMENT -1")
            .await;
        assert_eq!(
            db.one("SELECT seqstart || ',' || seqmin || ',' || seqmax FROM pg_sequence WHERE seqrelid = 'app.down'::regclass").await,
            "-1,-32768,-1"
        );
        db.finish().await;
    }
}

/// The pull's own query and decoder against the catalog: a permanent sequence no column owns
/// is read with its type, start, increment and only the options that differ from the engine's
/// defaults; a sequence a `serial`, an identity or `OWNED BY` owns, an `UNLOGGED` one, a
/// package's, a temporary one and one with a dot in its name are not.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn the_pull_reads_standalone_sequences_and_nothing_else() {
    let _live = super::live_ledger().await;
    for variable in SERVERS {
        let mut db = Private::on(variable, "pull").await;
        db.run("CREATE SEQUENCE plain; \
                CREATE SEQUENCE counted AS integer START 100 INCREMENT 5 MINVALUE 100 MAXVALUE 999999 CYCLE CACHE 20; \
                CREATE SEQUENCE countdown AS smallint INCREMENT -1; \
                CREATE SEQUENCE big START 7; \
                CREATE SEQUENCE \"Mixed Case\"").await;
        // Not held: owned by a column in each of the three ways, unlogged, a package's, a
        // temporary one (this connection's own session), and one the model cannot spell.
        db.run("CREATE TABLE t (id serial, n integer GENERATED ALWAYS AS IDENTITY); \
                CREATE SEQUENCE owned_by_col OWNED BY t.id; \
                CREATE UNLOGGED SEQUENCE unlogged_s; \
                CREATE EXTENSION unaccent SCHEMA public; CREATE SEQUENCE pkg; ALTER EXTENSION unaccent ADD SEQUENCE pkg; \
                CREATE TEMP SEQUENCE temp_s; \
                CREATE SEQUENCE \"dotted.name\"; \
                CREATE SCHEMA \"dotted.schema\"; CREATE SEQUENCE \"dotted.schema\".in_dotted").await;
        let held = db.held().await;
        // The predicate the census and the default-sequence check share calls a sequence held
        // exactly when the pull declares it: system schemas, `pg_*` ones and dotted ones
        // included in the question, since the pull's query has no filter of its own.
        let asked = db
            .one(&format!(
                "SELECT COALESCE(string_agg(n.nspname || '.' || c.relname, ','), '') \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relkind = 'S' AND {}",
                held_predicate()
            ))
            .await;
        let mut by_predicate: Vec<&str> = asked.split(',').filter(|n| !n.is_empty()).collect();
        by_predicate.sort_unstable();
        let mut by_pull: Vec<String> = held.keys().map(|k| k.raw().to_string()).collect();
        by_pull.sort_unstable();
        assert_eq!(by_predicate, by_pull, "the predicate and the pull agree");
        // Sorted as text here, so the expectation does not depend on the map's key order.
        let mut names: Vec<String> = held.keys().map(|k| k.raw().to_string()).collect();
        names.sort();
        assert_eq!(
            names,
            [
                "app.Mixed Case",
                "app.big",
                "app.countdown",
                "app.counted",
                "app.plain"
            ],
            "the model's sequences and nothing a column or a package owns"
        );
        let get = |name: &str| &held[&pbps_model::ObjectName::new("app", name)];
        assert_eq!(get("plain").ty.raw().to_string(), "bigint");
        assert!((get("plain").start, get("plain").increment) == (1, 1));
        assert!(get("plain").options.is_default());
        let counted = get("counted");
        assert_eq!(counted.ty.raw().to_string(), "integer");
        assert_eq!((counted.start, counted.increment), (100, 5));
        assert_eq!(counted.options.min, Some(100));
        assert_eq!(counted.options.max, Some(999_999));
        assert!(counted.options.cycle);
        assert_eq!(counted.options.cache, Some(20));
        let countdown = get("countdown");
        assert_eq!((countdown.start, countdown.increment), (-1, -1));
        assert!(
            countdown.options.is_default(),
            "a descending default is no bound"
        );
        assert_eq!(get("big").start, 7);
        db.finish().await;
    }
}

/// Advancing a sequence is not a change anywhere the model looks: the pull's read is equal
/// before and after `nextval`, `setval` and a restart (DEC-2721.1). Negative: a change to what
/// the model does hold is a difference.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn advancing_a_sequence_changes_nothing_the_pull_reads() {
    let _live = super::live_ledger().await;
    for variable in SERVERS {
        let mut db = Private::on(variable, "advance").await;
        db.run("CREATE SEQUENCE a START 10 INCREMENT 5; CREATE SEQUENCE b AS smallint CYCLE MAXVALUE 100")
            .await;
        let before = db.held().await;
        assert_eq!(before.len(), 2);
        db.run("SELECT nextval('app.a'); SELECT nextval('app.a'); SELECT setval('app.a', 777); SELECT nextval('app.b'); SELECT setval('app.b', 50, false)").await;
        assert_eq!(
            db.held().await,
            before,
            "the counter moved and the read did not"
        );
        db.run("ALTER SEQUENCE a RESTART WITH 12").await;
        assert_eq!(
            db.held().await,
            before,
            "a restart moves the counter, not the definition"
        );
        // Negative: what the model holds is a difference.
        db.run("ALTER SEQUENCE a INCREMENT BY 6").await;
        assert_ne!(db.held().await, before);
        db.finish().await;
    }
}
