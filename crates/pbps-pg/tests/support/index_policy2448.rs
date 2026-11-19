//! The index method policy against the engine (#2448 PR A, DEC-2448.1): what
//! the table says a method can do, what an in-place parameter change costs, how
//! a schema's own operator class is spelled, and what the pull says today about
//! every index shape that arrives with a later part of #2448. Each case runs on
//! the 16 and the 18 fixture, each in a database of its own.
use super::{sqlstate, text, truth};
use crate::certified_fixture::FixtureEmission as _;
use pbps_db::{Conn, Driver};
use pbps_model::IndexChange;
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
        for other in ["spgist", "hash"] {
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
        "pages_per_range" => "4",
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
                if parameter.change == ParameterChange::Rebuild {
                    // BRIN's `pages_per_range` (DEC-2449.1): the native `ALTER`
                    // keeps the relation file and the metapage granularity, so
                    // the file proves nothing and the metapage is what is read.
                    assert!(in_place, "{am} {}: ALTER keeps the file", parameter.name);
                    db.run("CREATE EXTENSION IF NOT EXISTS pageinspect").await;
                    db.run(&format!(
                        "ALTER INDEX ix SET ({} = {})",
                        parameter.name,
                        value(parameter.name)
                    ))
                    .await;
                    let physical = text(
                        db.conn(),
                        "SELECT (brin_metapage_info(get_raw_page('ix', 0))).pagesperrange::text",
                    )
                    .await;
                    assert_ne!(
                        physical,
                        value(parameter.name),
                        "{am} {}: the index still uses the old granularity",
                        parameter.name
                    );
                    db.run(&format!("ALTER INDEX ix RESET ({})", parameter.name))
                        .await;
                } else {
                    assert!(
                        in_place,
                        "{am} {}: unchanged relation file means `Alter`",
                        parameter.name
                    );
                }
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
    // BRIN is held by key type too, and its default class covers an integer.
    if policy.method == IndexMethod::Brin {
        "n"
    } else if !policy.key_types.is_empty() {
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
    Change::Index(IndexChange::AddIndex {
        table: table.clone(),
        name: "ix_own".into(),
        index: Box::new(Index {
            columns: vec![IndexColumn {
                key: pbps_model::IndexKey::Column(key.into()),
                descending: false,
                opclass: class,
                collation: None,
                opclass_parameters: Default::default(),
            }],
            include: Vec::new(),
            unique: false,
            filter: None,
            method,
            storage_parameters: Default::default(),
            filegroup: None,
        }),
        clustered: false,
    })
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
             CREATE INDEX ix_brin ON t USING brin (id) WITH (pages_per_range = 8, autosummarize = on);
             CREATE INDEX ix_hash ON t USING hash (id);
             CREATE INDEX ix_brin_multi ON t USING brin (id int4_minmax_multi_ops (values_per_range = '040'));
             CREATE INDEX ix_brin_wrong ON t USING brin (c bpchar_bloom_ops);
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
            [
                "ix_brin",
                "ix_brin_multi",
                "ix_collated",
                "ix_ext_gin",
                "ix_gist",
                "ix_ok"
            ],
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
        // Part R1 holds BRIN with its two options, read back as declared.
        let brin = &table.indexes[&pbps_model::ObjectLocalName::from("ix_brin")];
        assert_eq!(brin.method, IndexMethod::Brin);
        assert_eq!(brin.storage_parameters["pages_per_range"], "8");
        assert_eq!(brin.storage_parameters["autosummarize"], "true");
        // Part R2 holds a minmax-multi class with its option in the canonical spelling
        // (`040` is octal 32); a class that is not the engine's for the key's type
        // (`bpchar_bloom_ops` over `text`, which the engine takes) is named.
        let multi = &table.indexes[&pbps_model::ObjectLocalName::from("ix_brin_multi")].columns[0];
        assert_eq!(multi.opclass_parameters["values_per_range"], "32");
        for (what, expected) in [
            ("ix_user_collation", "collation this model does not hold"),
            (
                "ix_hash",
                "`hash` method, and this model holds only `btree`, `gin`, `gist` and `brin`",
            ),
            (
                "ix_brin_wrong",
                "this model holds BRIN over `text` only with",
            ),
            ("ix_pattern", "B-tree key's default class"),
            ("ix_ext", "which no package owns"),
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
        // The notes about this table are these five and no other; the two about
        // the fixture's superuser and `PUBLIC` are the cluster's, not the table's.
        let own = pulled
            .warnings
            .iter()
            .filter(|w| w.contains("`app.t`"))
            .count();
        assert_eq!(own, 5, "{:#?}", pulled.warnings);
        db.finish().await;
    }
}

/// BRIN's classes and their options are what the engine holds (#2449 PR R2,
/// DEC-2449.2): the built-in inventory (72 classes) is told apart into its
/// families by the support function the engine gives each, which is the identity
/// the model's suffix rule stands for; each key type's default and other classes
/// are exactly the engine's classes of its input type, and each builds; the
/// options' bounds are the engine's, an endpoint accepted and the value past it
/// refused (`22023`); and a Bloom filter that does not fit fails at the build
/// (`XX000`) though its values are in bounds, so bounds alone never prove a
/// build.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn the_brin_classes_and_options_are_what_the_engine_holds() {
    let _live = super::live_ledger().await;
    let brin = IndexMethod::Brin.policy();
    for variable in SERVERS {
        let mut db = Private::on(variable, "brin_classes").await;
        let inventory = text(
            db.conn(),
            "SELECT string_agg(c.opcname || '|' || format_type(c.opcintype, NULL) || '|' || \
                    c.opcdefault::text || '|' || COALESCE((SELECT p.amproc::text FROM pg_amproc p \
                     WHERE p.amprocfamily = c.opcfamily AND p.amproclefttype = c.opcintype \
                       AND p.amprocnum = 5), ''), E'\\n' ORDER BY c.opcname) \
               FROM pg_opclass c JOIN pg_am a ON a.oid = c.opcmethod \
              WHERE a.amname = 'brin' AND c.opcnamespace = 'pg_catalog'::regnamespace",
        )
        .await;
        let classes: Vec<Vec<&str>> = inventory.lines().map(|l| l.split('|').collect()).collect();
        assert_eq!(classes.len(), 72, "the built-in inventory");
        // The family is the support function's, and the suffix says the same.
        for class in &classes {
            let (name, procedure) = (class[0], class[3]);
            assert_eq!(
                name.ends_with("_bloom_ops"),
                procedure == "brin_bloom_options",
                "{name}"
            );
            assert_eq!(
                name.ends_with("_minmax_multi_ops"),
                procedure == "brin_minmax_multi_options",
                "{name}"
            );
        }
        // Each key type: its default, and the other classes of its input type.
        for (position, key) in brin.key_types.iter().enumerate() {
            let input = match key.base {
                "character varying" => "text",
                "cidr" => "inet",
                other => other,
            };
            let of_input = |default: bool| -> Vec<&str> {
                let mut names: Vec<&str> = classes
                    .iter()
                    .filter(|c| c[1] == input && (c[2] == "true") == default)
                    .map(|c| c[0])
                    .collect();
                names.sort_unstable();
                names
            };
            assert_eq!(of_input(true), [key.default_class], "{} default", key.base);
            let mut held: Vec<&str> = key.classes.to_vec();
            held.sort_unstable();
            assert_eq!(of_input(false), held, "{} classes", key.base);
            // Each builds on a column of the model's type, and the default is
            // the class an index without one gets.
            db.run(&format!("CREATE TABLE k{position} (c {})", key.base))
                .await;
            db.run(&format!(
                "CREATE INDEX d{position} ON k{position} USING brin (c)"
            ))
            .await;
            let resolved = text(
                db.conn(),
                &format!(
                    "SELECT oc.opcname FROM pg_index i JOIN pg_opclass oc ON oc.oid = i.indclass[0] \
                      WHERE i.indexrelid = 'd{position}'::regclass"
                ),
            )
            .await;
            assert_eq!(resolved, key.default_class, "{}", key.base);
            for class in key.classes {
                db.run(&format!(
                    "CREATE INDEX x{position} ON k{position} USING brin (c pg_catalog.{class}); \
                     DROP INDEX x{position}"
                ))
                .await;
            }
        }
        // The bounds are the engine's: the endpoints build, the next value out does not.
        db.run("CREATE TABLE b (n int)").await;
        for (class, option, inside, outside) in [
            (
                "int4_minmax_multi_ops",
                "values_per_range",
                ["8", "256"],
                ["7", "257"],
            ),
            (
                "int4_bloom_ops",
                "n_distinct_per_range",
                ["-1", "2147483647"],
                ["-1.0001", "2147483648"],
            ),
            (
                "int4_bloom_ops",
                "false_positive_rate",
                ["0.0001", "0.25"],
                ["0.00009", "0.2501"],
            ),
        ] {
            let parameter = brin
                .class_parameters(&class.parse().unwrap())
                .iter()
                .find(|p| p.name == option)
                .expect(option);
            for value in inside {
                assert_eq!(
                    brin.canonical_class_parameter(&class.parse().unwrap(), option, value)
                        .as_deref()
                        .map(|c| c.parse::<f64>().unwrap()),
                    Ok(value.parse::<f64>().unwrap()),
                    "the model takes {option} {value}"
                );
                db.run(&format!(
                    "CREATE INDEX bx ON b USING brin (n pg_catalog.{class} ({option} = '{value}')) \
                     WITH (pages_per_range = 1); DROP INDEX bx"
                ))
                .await;
            }
            for value in outside {
                assert!(
                    brin.canonical_class_parameter(&class.parse().unwrap(), option, value)
                        .is_err(),
                    "the model refuses {option} {value}"
                );
                assert_eq!(
                    db.refused(&format!(
                        "CREATE INDEX bx ON b USING brin (n pg_catalog.{class} ({option} = '{value}'))"
                    ))
                    .await,
                    "22023",
                    "{option} {value}"
                );
            }
            assert_eq!(parameter.name, option);
        }
        // Values in bounds that the filter cannot hold fail at the build over rows
        // (`XX000`, "the bloom filter is too large"), and a smaller range or
        // estimate makes the same class build: bounds alone never prove a build.
        db.run("INSERT INTO b SELECT g FROM generate_series(1, 2000) g")
            .await;
        for (clause, fitting) in [
            (
                "(n_distinct_per_range = '-1')",
                "(n_distinct_per_range = '-1')) WITH (pages_per_range = 1",
            ),
            (
                "(false_positive_rate = '0.0001')",
                "(false_positive_rate = '0.0001', n_distinct_per_range = '100')",
            ),
        ] {
            assert_eq!(
                db.refused(&format!(
                    "CREATE INDEX bx ON b USING brin (n pg_catalog.int4_bloom_ops {clause})"
                ))
                .await,
                "XX000",
                "{clause}"
            );
            db.run(&format!(
                "CREATE INDEX bx ON b USING brin (n pg_catalog.int4_bloom_ops {fitting}); \
                 DROP INDEX bx"
            ))
            .await;
        }
        // Negatives: the other family's option, an option of a class with none, a
        // storage option inside a class, and a class option beside the storage ones.
        for sql in [
            "CREATE INDEX bx ON b USING brin (n pg_catalog.int4_minmax_multi_ops (false_positive_rate = '0.01'))",
            "CREATE INDEX bx ON b USING brin (n pg_catalog.int4_minmax_ops (values_per_range = '16'))",
            "CREATE INDEX bx ON b USING brin (n pg_catalog.int4_minmax_multi_ops (pages_per_range = '16'))",
            "CREATE INDEX bx ON b USING brin (n) WITH (values_per_range = '16')",
        ] {
            assert!(!db.refused(sql).await.is_empty(), "{sql}");
        }
        db.finish().await;
    }
}
