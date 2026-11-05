//! GiST indexes and a key's collation against the engine (#2448 PR B,
//! DEC-2448.2): the statements the emitter writes build what the declaration
//! says, the pull reads it back as the same declaration, every column type and
//! collation rule the model holds is the engine's own answer, and an option-only
//! change is an in-place `ALTER INDEX`. Each case runs on the 16 and the 18
//! fixture, each in a database of its own.
use super::{sqlstate, text, truth};
use crate::certified_fixture::FixtureEmission as _;
use pbps_db::{Conn, Driver};
use pbps_model::{
    Change, Index, IndexColumn, IndexKey, IndexMethod, IndexPart, ObjectLocalName, OpClassRef,
    Strategy, TableName,
};
use pbps_pg::Postgres;
use std::collections::{BTreeMap, BTreeSet};

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
        let version = text(&mut admin, "SHOW server_version").await;
        let name = format!("pbps_b2448_{case}_{}", std::process::id());
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
        };
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

    /// The SQLSTATE of the engine's refusal; the statement must be refused.
    async fn refused(&mut self, sql: &str) -> String {
        println!("SQL (refused): {sql}");
        let Err(error) = self.conn().execute(sql).await else {
            panic!("the engine accepted: {sql}")
        };
        sqlstate(&error).to_owned()
    }

    async fn apply(&mut self, change: &Change) {
        for statement in Postgres::new()
            .fixture_emit(change, Strategy::default())
            .expect("emit")
        {
            self.run(&statement.sql).await;
        }
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

fn table() -> TableName {
    "app.booking".parse().unwrap()
}

fn key(key: IndexKey, class: Option<&str>, collation: Option<&str>) -> IndexColumn {
    IndexColumn {
        key,
        descending: false,
        opclass: class.map(|c| c.parse::<OpClassRef>().unwrap()),
        collation: collation.map(str::to_owned),
    }
}

fn index(method: IndexMethod, columns: Vec<IndexColumn>) -> Index {
    Index {
        columns,
        include: Vec::new(),
        unique: false,
        filter: None,
        method,
        storage_parameters: BTreeMap::new(),
    }
}

fn add(name: &str, index: Index) -> Change {
    Change::AddIndex {
        table: table(),
        name: name.into(),
        index: Box::new(index),
        clustered: false,
    }
}

async fn pull_indexes(db: &mut Conn) -> BTreeMap<ObjectLocalName, Index> {
    let pulled = pbps_pg::catalog::introspect(db).await.expect("pull");
    let table = &pulled.schema.tables[&table()];
    assert!(
        pulled.warnings.iter().all(|w| !w.contains("`app.booking`")),
        "nothing about the table is left out: {:?}",
        pulled.warnings
    );
    table.indexes.clone()
}

async fn file_of(db: &mut Conn, index: &str) -> String {
    text(
        db,
        &format!("SELECT relfilenode::text FROM pg_class WHERE oid = 'app.{index}'::regclass"),
    )
    .await
}

const BOOKING: &str = "CREATE TABLE booking (id int PRIMARY KEY, starts int NOT NULL, \
    ends int NOT NULL, note text, active boolean, ip inet, doc tsvector, label text)";

/// What the emitter writes for a GiST index and for a collated key builds an
/// index the pull reads back as the same declaration: method, keys (column and
/// expression), class, collation, `INCLUDE`, parameters and filter. The
/// expression and the filter come back as the engine renders them (M3, M8).
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_gist_index_and_a_key_collation_read_back_as_declared() {
    let _live = super::live_ledger().await;
    for variable in SERVERS {
        let mut db = Private::on(variable, "round").await;
        db.run(BOOKING).await;
        let span = IndexKey::Expression("int4range(starts, ends, '[)')".into());
        let mut ranged = index(IndexMethod::Gist, vec![key(span.clone(), None, None)]);
        ranged.include = vec!["note".into()];
        ranged.filter = Some("active".into());
        ranged.storage_parameters = [
            ("buffering".to_owned(), "on".to_owned()),
            ("fillfactor".to_owned(), "70".to_owned()),
        ]
        .into();
        db.apply(&add("ix_span", ranged)).await;
        db.apply(&add(
            "ix_ip",
            index(
                IndexMethod::Gist,
                vec![key(IndexKey::Column("ip".into()), Some("inet_ops"), None)],
            ),
        ))
        .await;
        db.apply(&add(
            "ix_doc",
            index(
                IndexMethod::Gist,
                vec![
                    key(IndexKey::Column("doc".into()), None, None),
                    key(span.clone(), None, None),
                ],
            ),
        ))
        .await;
        db.apply(&add(
            "ix_label",
            index(
                IndexMethod::Btree,
                vec![key(IndexKey::Column("label".into()), None, Some("C"))],
            ),
        ))
        .await;
        db.apply(&add(
            "ix_plain",
            index(
                IndexMethod::Btree,
                vec![key(IndexKey::Column("label".into()), None, None)],
            ),
        ))
        .await;
        let read = pull_indexes(db.conn()).await;
        let at = |name: &str| &read[&ObjectLocalName::from(name)];

        let ranged = at("ix_span");
        assert_eq!(ranged.method, IndexMethod::Gist);
        assert!(
            matches!(&ranged.columns[0].key, IndexKey::Expression(t) if t.starts_with("int4range("))
        );
        assert_eq!(ranged.include.len(), 1);
        assert_eq!(ranged.filter.as_deref(), Some("active"));
        assert_eq!(ranged.storage_parameters["buffering"], "on");
        assert_eq!(ranged.storage_parameters["fillfactor"], "70");
        assert_eq!(ranged.columns[0].opclass, None, "the default is no class");

        let ip = at("ix_ip");
        assert_eq!(ip.columns[0].opclass, Some(OpClassRef::bare("inet_ops")));
        let doc = at("ix_doc");
        assert_eq!(doc.columns.len(), 2);
        assert_eq!(doc.columns[0].opclass, None);

        assert_eq!(at("ix_label").columns[0].collation.as_deref(), Some("C"));
        // Negative: no collation declared, none read; and the column's own
        // collation is never invented.
        assert_eq!(at("ix_plain").columns[0].collation, None);
        assert!(
            read.values()
                .all(|i| i.columns.iter().all(|c| !c.descending))
        );
        db.finish().await;
    }
}

/// The typed-column table of the GiST policy is the engine's: over each column
/// type the model declares, an index built with the class the table names
/// succeeds exactly for the admitted types, and the engine refuses every other
/// one with `42704`, no default operator class (M4).
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn the_gist_column_types_are_the_ones_the_engine_has_a_class_for() {
    let _live = super::live_ledger().await;
    let policy = IndexMethod::Gist.policy();
    // Every declared type that is not an array, whose base is the spelling
    // the policy uses.
    let types = [
        ("tsvector", "tsvector"),
        ("tsquery", "tsquery"),
        ("inet", "inet"),
        ("cidr", "cidr"),
        ("int", "integer"),
        ("text", "text"),
        ("jsonb", "jsonb"),
        ("uuid", "uuid"),
        ("timestamp", "timestamp without time zone"),
        ("boolean", "boolean"),
        ("macaddr", "macaddr"),
        ("numeric", "numeric"),
        ("bytea", "bytea"),
    ];
    for variable in SERVERS {
        let mut db = Private::on(variable, "types").await;
        for (declared, base) in types {
            db.run(&format!("CREATE TABLE ty (c {declared})")).await;
            let admitted = policy.typed_columns.iter().find(|(b, _)| *b == base);
            let class = admitted.and_then(|(_, class)| *class);
            let sql = format!(
                "CREATE INDEX ix ON ty USING gist (c{})",
                class.map(|c| format!(" {c}")).unwrap_or_default()
            );
            if admitted.is_some() {
                db.run(&sql).await;
            } else {
                assert_eq!(db.refused(&sql).await, "42704", "{declared}");
                // Negative: the class of the nearest admitted type does not
                // make it indexable either.
                let with_class = db
                    .refused("CREATE INDEX ix ON ty USING gist (c inet_ops)")
                    .await;
                assert!(["42804", "42809", "42704"].contains(&with_class.as_str()));
            }
            db.run("DROP TABLE ty").await;
        }
        // A type that needs its class has no default: the table says so.
        db.run("CREATE TABLE ty (c inet)").await;
        assert_eq!(
            db.refused("CREATE INDEX ix ON ty USING gist (c)").await,
            "42704"
        );
        db.finish().await;
    }
}

/// A key collation is the engine's answer where the model refuses: held for a
/// string column (`text`, `varchar`, `char`, a domain over text), refused on a
/// key with none (`integer`, `inet`: `42804`), `default` is stored as nothing
/// and read as nothing, and a collation of another schema is read as a
/// limitation (M8, M11).
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_key_collation_is_the_engines_where_the_model_holds_it() {
    let _live = super::live_ledger().await;
    for variable in SERVERS {
        let mut db = Private::on(variable, "collation").await;
        db.run(
            "CREATE DOMAIN dtext AS text;
             CREATE TABLE booking (id int PRIMARY KEY, n int, ip inet, label text,
                                   v varchar(10), c char(5), d dtext, tv tsvector);
             CREATE INDEX ix_text ON booking (label COLLATE \"C\");
             CREATE INDEX ix_varchar ON booking (v COLLATE \"C\");
             CREATE INDEX ix_char ON booking (c COLLATE \"C\");
             CREATE INDEX ix_domain ON booking (d COLLATE \"C\");
             CREATE INDEX ix_default ON booking (label COLLATE \"default\");
             CREATE INDEX ix_expression ON booking (lower(label) COLLATE \"C\");
             CREATE COLLATION app.mine (provider = libc, locale = 'C');
             CREATE INDEX ix_mine ON booking (label COLLATE app.mine)",
        )
        .await;
        assert_eq!(
            db.refused("CREATE INDEX z ON booking (n COLLATE \"C\")")
                .await,
            "42804"
        );
        assert_eq!(
            db.refused("CREATE INDEX z ON booking USING gist (ip COLLATE \"C\" inet_ops)")
                .await,
            "42804",
            "a GiST key over inet has no collation either"
        );
        // The grammar puts COLLATE before the class: the other order is a
        // syntax error, so the emitter's order is the only one.
        assert_eq!(
            db.refused("CREATE INDEX z ON booking USING gist (ip inet_ops COLLATE \"C\")")
                .await,
            "42601"
        );
        let pulled = pbps_pg::catalog::introspect(db.conn()).await.expect("pull");
        let held = &pulled.schema.tables[&table()].indexes;
        let collation = |name: &str| {
            held.get(&ObjectLocalName::from(name))
                .map(|i| i.columns[0].collation.clone())
        };
        for name in ["ix_text", "ix_varchar", "ix_char", "ix_domain"] {
            assert_eq!(collation(name), Some(Some("C".to_owned())), "{name}");
        }
        assert_eq!(
            collation("ix_default"),
            Some(None),
            "default is the column's own"
        );
        // Negative: an expression key's collation and a collation of another
        // schema are left out of the pull, each named.
        assert_eq!(collation("ix_expression"), None);
        assert_eq!(collation("ix_mine"), None);
        for name in ["ix_expression", "ix_mine"] {
            assert!(
                pulled
                    .warnings
                    .iter()
                    .any(|w| w.contains(&format!("`{name}`")) && w.contains("left out")),
                "{name}: {:?}",
                pulled.warnings
            );
        }
        db.finish().await;
    }
}

/// A GiST option-only change is an in-place `ALTER INDEX`: the relation file is
/// the same after `SET` and `RESET`, the pull reads the new values, and a
/// value the engine bounds is refused by it (`22023`); `DROP INDEX` removes the
/// index and nothing else (M6, M15).
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_gist_option_change_alters_in_place_and_a_drop_needs_nothing_else() {
    let _live = super::live_ledger().await;
    for variable in SERVERS {
        let mut db = Private::on(variable, "alter").await;
        db.run(BOOKING).await;
        let mut ranged = index(
            IndexMethod::Gist,
            vec![key(
                IndexKey::Expression("int4range(starts, ends)".into()),
                None,
                None,
            )],
        );
        ranged.storage_parameters = [("fillfactor".to_owned(), "70".to_owned())].into();
        db.apply(&add("ix_span", ranged)).await;
        let before = file_of(db.conn(), "ix_span").await;
        db.apply(&Change::SetIndexStorageParameters {
            table: table(),
            target: IndexPart::Index("ix_span".into()),
            method: IndexMethod::Gist,
            set: [
                ("fillfactor".to_owned(), "80".to_owned()),
                ("buffering".to_owned(), "off".to_owned()),
            ]
            .into(),
            reset: BTreeSet::new(),
        })
        .await;
        assert_eq!(file_of(db.conn(), "ix_span").await, before, "no rebuild");
        let read = pull_indexes(db.conn()).await;
        let span = &read[&ObjectLocalName::from("ix_span")];
        assert_eq!(span.storage_parameters["fillfactor"], "80");
        assert_eq!(span.storage_parameters["buffering"], "off");
        db.apply(&Change::SetIndexStorageParameters {
            table: table(),
            target: IndexPart::Index("ix_span".into()),
            method: IndexMethod::Gist,
            set: BTreeMap::new(),
            reset: ["buffering".to_owned()].into(),
        })
        .await;
        assert_eq!(file_of(db.conn(), "ix_span").await, before);
        let read = pull_indexes(db.conn()).await;
        let span = &read[&ObjectLocalName::from("ix_span")];
        assert!(!span.storage_parameters.contains_key("buffering"));
        // Negative: out of bounds and another method's parameter.
        assert_eq!(
            db.refused("ALTER INDEX app.ix_span SET (fillfactor = 5)")
                .await,
            "22023"
        );
        assert_eq!(
            db.refused("ALTER INDEX app.ix_span SET (fastupdate = on)")
                .await,
            "22023"
        );
        db.apply(&Change::DropIndex {
            table: table(),
            name: "ix_span".into(),
        })
        .await;
        assert!(!truth(db.conn(), "SELECT to_regclass('app.ix_span') IS NOT NULL").await);
        db.finish().await;
    }
}

/// An array of a collatable element takes a key collation, and the round trip
/// is exact (declare, apply, pull, equal): a `text[]`, `varchar[]` and `char[]`
/// key under `COLLATE "C"` builds and is read as `collation: C`; an `integer[]`
/// key is the engine's `42804`, and a GiST over `text[]` has no class at all
/// (`42704`), so both stay refused (review round 1).
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn an_array_of_a_collatable_element_takes_a_key_collation_and_reads_back() {
    let _live = super::live_ledger().await;
    for variable in SERVERS {
        let mut db = Private::on(variable, "array").await;
        db.run(
            "CREATE TABLE booking (id int PRIMARY KEY, tags text[], vtags varchar(5)[], \
             ctags char(3)[], nums int[])",
        )
        .await;
        let collated = |column: &str| {
            index(
                IndexMethod::Btree,
                vec![key(IndexKey::Column(column.into()), None, Some("C"))],
            )
        };
        let declared = [
            ("ix_tags", "tags"),
            ("ix_vtags", "vtags"),
            ("ix_ctags", "ctags"),
        ];
        for (name, column) in declared {
            db.apply(&add(name, collated(column))).await;
        }
        let read = pull_indexes(db.conn()).await;
        for (name, column) in declared {
            assert_eq!(
                read[&ObjectLocalName::from(name)],
                collated(column),
                "{name}: what was declared is what is read"
            );
        }
        // Negative: an array of a type with no collation, and a GiST over a
        // collatable array, are the engine's refusals.
        assert_eq!(
            db.refused("CREATE INDEX z ON app.booking (nums COLLATE \"C\")")
                .await,
            "42804"
        );
        assert_eq!(
            db.refused("CREATE INDEX z ON app.booking USING gist (tags)")
                .await,
            "42704"
        );
        db.finish().await;
    }
}
