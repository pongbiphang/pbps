//! The index method policy against the engine (#2448 PR A, DEC-2448.1): what
//! the table says a method can do, what an in-place parameter change costs, how
//! a schema's own operator class is spelled, and what the pull says today about
//! every index shape that arrives with a later part of #2448. Each case runs on
//! the 16 and the 18 fixture, each in a database of its own.
use super::{sqlstate, text, truth};
use crate::certified_fixture::FixtureEmission as _;
use pbps_db::{Conn, Driver};
use pbps_model::storage::{IndexLock, ParameterChange};
use pbps_model::{Change, Index, IndexColumn, IndexMethod, OpClassRef, Strategy, TableName};
use pbps_pg::Postgres;

const SERVERS: [&str; 2] = ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"];

/// A database of its own on one fixture, so a pull sees nothing but this case.
struct Private {
    admin: Conn,
    name: String,
    conn: Option<Conn>,
    version: String,
}

impl Private {
    async fn on(variable: &str, case: &str) -> Private {
        let url = std::env::var(variable).expect("the PostgreSQL 16/18 fixture");
        let mut admin = Conn::connect(Driver::Postgres, &url).await.unwrap();
        let version = text(&mut admin, "SHOW server_version").await;
        let name = format!("pbps_a2448_{case}_{}", std::process::id());
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
        println!("server {version}: {name}");
        let mut db = Private {
            admin,
            name,
            conn: Some(conn),
            version,
        };
        // The connection starts with an empty `search_path`, as every pbps one does.
        db.run("CREATE SCHEMA app; SET search_path = app, pg_catalog")
            .await;
        db
    }

    fn conn(&mut self) -> &mut Conn {
        self.conn.as_mut().unwrap()
    }

    async fn run(&mut self, sql: &str) {
        println!("SQL: {sql}");
        self.conn()
            .execute(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }

    /// The engine's own answer, as the SQLSTATE of the refusal.
    async fn refused(&mut self, sql: &str) -> String {
        println!("SQL (refused): {sql}");
        let Err(error) = self.conn().execute(sql).await else {
            panic!("the engine accepted: {sql}")
        };
        sqlstate(&error).to_owned()
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
        println!("done on {}", self.version);
    }
}

/// The policy's `unique`, `include` and `ordered` are what the engine's own
/// property table says, and what an actual `CREATE INDEX` does (M5).
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn the_policy_capabilities_are_the_engines_own() {
    let _live = super::live_ledger().await;
    for variable in SERVERS {
        let mut db = Private::on(variable, "caps").await;
        db.run("CREATE TABLE t (id int PRIMARY KEY, body jsonb, n int)")
            .await;
        for method in IndexMethod::ALL {
            let policy = method.policy();
            let am = method.as_str();
            let property = |p: &'static str| {
                format!(
                    "SELECT pg_indexam_has_property(oid, '{p}') FROM pg_am WHERE amname = '{am}'"
                )
            };
            let column = key_of(policy);
            for (what, claimed, property_name, clause) in [
                ("unique", policy.unique, "can_unique", String::new()),
                (
                    "include",
                    policy.include,
                    "can_include",
                    " INCLUDE (id)".to_owned(),
                ),
                ("order", policy.ordered, "can_order", String::new()),
            ] {
                let engine = truth(db.conn(), &property(property_name)).await;
                assert_eq!(
                    claimed, engine,
                    "{am} {what}: the table against the engine's property"
                );
                let key = if what == "order" {
                    format!("{column} DESC")
                } else {
                    column.to_owned()
                };
                let unique = if what == "unique" { "UNIQUE " } else { "" };
                let sql = format!("CREATE {unique}INDEX ix ON t USING {am} ({key}){clause}");
                if claimed {
                    db.run(&sql).await;
                    db.run("DROP INDEX ix").await;
                } else {
                    // Named, not just refused: the engine says it does not
                    // support the feature, and nothing else is the reason.
                    assert_eq!(db.refused(&sql).await, "0A000", "{am} {what}");
                }
            }
        }
        // Negative: a method the table does not hold has no row, and the
        // engine's own list has it (so the absence is the model's choice).
        for other in ["brin", "spgist", "hash"] {
            assert_eq!(IndexMethod::from_am_name(other), None);
            assert!(
                truth(
                    db.conn(),
                    &format!(
                        "SELECT count(*) = 1 FROM pg_am WHERE amname = '{other}' AND amtype = 'i'"
                    )
                )
                .await,
                "{other}"
            );
        }
        db.finish().await;
    }
}

/// Every parameter the policy holds changes in place with its verdict's
/// meaning, and takes the lock the policy names: the relation file is the same
/// after `SET` and `RESET` exactly where the verdict is `Alter`, and the lock
/// on the index is the policy's (M6, DEC-1442.1 as a regression).
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn every_parameter_verdict_and_lock_is_what_the_engine_does() {
    let _live = super::live_ledger().await;
    let value = |name: &str| match name {
        "fillfactor" => "70",
        "gin_pending_list_limit" => "1024",
        _ => "off",
    };
    for variable in SERVERS {
        let mut db = Private::on(variable, "params").await;
        db.run("CREATE TABLE t (id int PRIMARY KEY, body jsonb, n int)")
            .await;
        for method in IndexMethod::ALL {
            let policy = method.policy();
            let am = method.as_str();
            db.run(&format!(
                "CREATE INDEX ix ON t USING {am} ({})",
                key_of(policy)
            ))
            .await;
            for parameter in policy.parameters {
                let before = file(db.conn()).await;
                db.run(&format!(
                    "BEGIN; ALTER INDEX ix SET ({} = {})",
                    parameter.name,
                    value(parameter.name)
                ))
                .await;
                let modes = text(
                    db.conn(),
                    "SELECT string_agg(DISTINCT mode, ',' ORDER BY mode) FROM pg_locks \
                     WHERE locktype = 'relation' AND relation = 'ix'::regclass AND pid = pg_backend_pid()",
                )
                .await;
                db.run("COMMIT").await;
                let expected = match policy.lock_of([parameter.name]) {
                    IndexLock::ShareUpdateExclusive => "ShareUpdateExclusiveLock",
                    IndexLock::AccessExclusive => "AccessExclusiveLock",
                };
                assert_eq!(
                    modes, expected,
                    "{am} {}: the lock on the index",
                    parameter.name
                );
                db.run(&format!("ALTER INDEX ix RESET ({})", parameter.name))
                    .await;
                let in_place = before == file(db.conn()).await;
                assert_eq!(
                    in_place,
                    parameter.change == ParameterChange::Alter,
                    "{am} {}: unchanged relation file means `Alter`",
                    parameter.name
                );
            }
            // Negative: the other method's parameter is refused by the engine.
            let other = IndexMethod::ALL.into_iter().find(|m| *m != method).unwrap();
            let foreign = other.policy().parameters[0].name;
            assert_eq!(
                db.refused(&format!(
                    "ALTER INDEX ix SET ({foreign} = {})",
                    value(foreign)
                ))
                .await,
                "22023",
                "{am} refuses {foreign}"
            );
            db.run("DROP INDEX ix").await;
        }
        db.finish().await;
    }
}

/// A key every method can be built over: `n` for a B-tree, `body` for GIN, and
/// an expression for GiST, which has no default class for an integer.
fn key_of(policy: &pbps_model::storage::MethodPolicy) -> &'static str {
    if policy.key_type.is_some() {
        "body"
    } else if policy.typed_columns.is_empty() {
        "n"
    } else {
        "(int4range(n, n))"
    }
}

/// The relation file of index `ix`: the same after `SET` and `RESET` when
/// nothing was rebuilt.
async fn file(db: &mut Conn) -> String {
    text(
        db,
        "SELECT relfilenode::text FROM pg_class WHERE oid = 'ix'::regclass",
    )
    .await
}

/// The schema and name of the class `ix_own` was built with.
async fn class_of(db: &mut Conn) -> String {
    text(
        db,
        "SELECT n.nspname || '.' || oc.opcname FROM pg_index i \
         JOIN pg_opclass oc ON oc.oid = i.indclass[0] \
         JOIN pg_namespace n ON n.oid = oc.opcnamespace \
         WHERE i.indexrelid = 'app.ix_own'::regclass",
    )
    .await
}

fn add_index(
    table: &TableName,
    class: Option<OpClassRef>,
    key: &str,
    method: IndexMethod,
) -> Change {
    Change::AddIndex {
        table: table.clone(),
        name: "ix_own".into(),
        index: Box::new(Index {
            columns: vec![IndexColumn {
                key: pbps_model::IndexKey::Column(key.into()),
                descending: false,
                opclass: class,
                collation: None,
            }],
            include: Vec::new(),
            unique: false,
            filter: None,
            method,
            storage_parameters: Default::default(),
        }),
        clustered: false,
    }
}

/// A class a schema owns is spelled with its schema and found there; a bare
/// class is `pg_catalog`'s whatever the `search_path` holds; and `pg_catalog.x`
/// is the same class as the bare `x` (M4, DEC-2448.1).
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_qualified_class_is_spelled_by_its_own_schema_and_a_bare_one_by_pg_catalog() {
    let _live = super::live_ledger().await;
    let pg = Postgres::new();
    for variable in SERVERS {
        let mut db = Private::on(variable, "class").await;
        db.run(
            "CREATE SCHEMA ext; CREATE TABLE t (c text);
             CREATE OPERATOR CLASS ext.text_ops2 FOR TYPE text USING btree AS
               OPERATOR 1 <, OPERATOR 2 <=, OPERATOR 3 =, OPERATOR 4 >=, OPERATOR 5 >,
               FUNCTION 1 bttextcmp(text, text);
             CREATE OPERATOR CLASS ext.text_pattern_ops FOR TYPE text USING btree AS
               OPERATOR 1 <, OPERATOR 2 <=, OPERATOR 3 =, OPERATOR 4 >=, OPERATOR 5 >,
               FUNCTION 1 bttextcmp(text, text)",
        )
        .await;
        let table: TableName = "app.t".parse().unwrap();
        for (class, expected) in [
            (OpClassRef::in_schema("ext", "text_ops2"), "ext.text_ops2"),
            ("ext.text_ops2".parse().unwrap(), "ext.text_ops2"),
            // The namesake in `ext` does not stand in for the bare class, even
            // with `ext` first on the path: the statement qualifies it.
            (
                "pg_catalog.text_pattern_ops".parse().unwrap(),
                "pg_catalog.text_pattern_ops",
            ),
            (
                OpClassRef::bare("text_pattern_ops"),
                "pg_catalog.text_pattern_ops",
            ),
        ] {
            db.run("SET search_path = ext, pg_catalog").await;
            for statement in pg
                .fixture_emit(
                    &add_index(&table, Some(class), "c", IndexMethod::Btree),
                    Strategy::default(),
                )
                .expect("emit")
            {
                db.run(&statement.sql).await;
            }
            assert_eq!(class_of(db.conn()).await, expected);
            db.run("SET search_path = app, pg_catalog; DROP INDEX ix_own")
                .await;
        }
        db.finish().await;
    }
}

/// What the pull says about each index shape that still arrives with a later
/// part of #2448, #2449 or #2447 (B holds GiST and a `pg_catalog` key
/// collation): each is left out with a note, nothing else is noted, and the
/// ordinary index beside them is read as it always was.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn each_index_shape_of_a_later_part_is_left_out_of_the_pull_with_a_named_note() {
    let _live = super::live_ledger().await;
    for variable in SERVERS {
        let mut db = Private::on(variable, "pull").await;
        db.run(
            "CREATE SCHEMA ext;
             CREATE OPERATOR CLASS ext.text_ops2 FOR TYPE text USING btree AS
               OPERATOR 1 <, OPERATOR 2 <=, OPERATOR 3 =, OPERATOR 4 >=, OPERATOR 5 >,
               FUNCTION 1 bttextcmp(text, text);
             CREATE TABLE t (id int PRIMARY KEY, c text, r inet, e text, body jsonb);
             CREATE INDEX ix_ok ON t (c);
             CREATE INDEX ix_collated ON t (c COLLATE \"C\");
             CREATE COLLATION ext.mine (provider = libc, locale = 'C');
             CREATE INDEX ix_user_collation ON t (c COLLATE ext.mine);
             CREATE INDEX ix_gist ON t USING gist (r inet_ops);
             CREATE INDEX ix_brin ON t USING brin (id);
             CREATE INDEX ix_pattern ON t (e text_pattern_ops);
             CREATE INDEX ix_ext ON t (c ext.text_ops2);
             CREATE INDEX ix_ext_gin ON t USING gin (body);
             ALTER TABLE t ADD CONSTRAINT ex_r EXCLUDE USING gist (r inet_ops WITH &&)",
        )
        .await;
        let pulled = pbps_pg::catalog::introspect(db.conn()).await.expect("pull");
        let table = &pulled.schema.tables[&"app.t".parse::<TableName>().unwrap()];
        let held: Vec<&str> = table.indexes.keys().map(|n| n.raw()).collect();
        assert_eq!(
            held,
            ["ix_collated", "ix_ext_gin", "ix_gist", "ix_ok"],
            "only the held shapes are read: {:?}",
            pulled.warnings
        );
        // Part B holds these two: a `pg_catalog` key collation and GiST.
        assert_eq!(
            table.indexes[&pbps_model::ObjectLocalName::from("ix_collated")].columns[0]
                .collation
                .as_deref(),
            Some("C")
        );
        assert_eq!(
            table.indexes[&pbps_model::ObjectLocalName::from("ix_gist")].method,
            IndexMethod::Gist
        );
        for (what, expected) in [
            ("ix_user_collation", "collation this model does not hold"),
            (
                "ix_brin",
                "`brin` method, and this model holds only `btree`, `gin` and `gist`",
            ),
            ("ix_pattern", "B-tree key's default class"),
            ("ix_ext", "#2447 PR F"),
        ] {
            let note = pulled
                .warnings
                .iter()
                .find(|w| w.contains(&format!("`{what}`")))
                .unwrap_or_else(|| panic!("{what} is named: {:?}", pulled.warnings));
            assert!(note.contains(expected), "{what}: {note}");
        }
        // The exclusion constraint is held since C1a (DEC-2448.3): read whole, as
        // a constraint and not as an index, with no note.
        let held = &table.exclude[&pbps_model::ObjectLocalName::from("ex_r")];
        assert_eq!(held.method, IndexMethod::Gist);
        assert_eq!(held.keys.len(), 1);
        assert_eq!(held.keys[0].operator.raw().to_string(), "&&");
        // The notes about this table are these four and no other; the two about
        // the fixture's superuser and `PUBLIC` are the cluster's, not the table's.
        let own = pulled
            .warnings
            .iter()
            .filter(|w| w.contains("`app.t`"))
            .count();
        assert_eq!(own, 4, "{:#?}", pulled.warnings);
        db.finish().await;
    }
}
