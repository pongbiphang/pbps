//! Text search configurations and dictionaries against the engine (#2728 PR A,
//! DEC-2728.1): the facts the model rests on, asserted on the 16 and the 18 fixture, and
//! the pull's own query and decoder against what the catalog really holds. Each case runs in
//! a database of its own.
use super::{sqlstate, text, truth};
use pbps_db::{Conn, Driver};
use pbps_model::{SearchKind, SearchObject, SearchRef};
use pbps_pg::search::{TOKEN_ALIASES, decode, parse_options, query, templates};

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
        let name = format!("pbps_ts2728_{case}_{}", std::process::id());
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

/// What the model rests on, measured: the built-in parser, templates and token types are the
/// closed lists the validator holds; the options text is stored as the reader expects, and an
/// option written with no value is removed; a mapping keeps the order it was written; a
/// copied configuration is its result; two kinds and a domain of one name coexist; a
/// file-backed dictionary's files are opened at `CREATE`; a dependent index blocks a drop
/// and a changed mapping does not rebuild it (the premise of the stale-dependents refusal).
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn measure() {
    let _live = super::live_ledger().await;
    for variable in SERVERS {
        let mut db = Private::on(variable, "measure").await;
        // The closed lists.
        assert_eq!(
            db.one("SELECT string_agg(prsname, ',') FROM pg_ts_parser WHERE prsnamespace = 'pg_catalog'::regnamespace").await,
            "default"
        );
        let mut templates = templates();
        templates.sort_unstable();
        assert_eq!(
            db.one("SELECT string_agg(tmplname, ',' ORDER BY tmplname) FROM pg_ts_template WHERE tmplnamespace = 'pg_catalog'::regnamespace").await,
            templates.join(",")
        );
        assert_eq!(
            db.one("SELECT string_agg(alias, ',' ORDER BY tokid) FROM ts_token_type((SELECT oid FROM pg_ts_parser WHERE prsname = 'default'))").await,
            TOKEN_ALIASES.join(",")
        );
        // Options as stored, and removed by writing the name alone.
        db.run("CREATE TEXT SEARCH DICTIONARY d1 (TEMPLATE = snowball, Language = english, StopWords = english); CREATE TEXT SEARCH DICTIONARY d2 (TEMPLATE = simple, Accept = false)").await;
        assert_eq!(
            db.one("SELECT dictinitoption FROM pg_ts_dict WHERE dictname = 'd1'")
                .await,
            "language = 'english', stopwords = 'english'"
        );
        assert_eq!(
            db.one("SELECT dictinitoption FROM pg_ts_dict WHERE dictname = 'd2'")
                .await,
            "accept = 'false'"
        );
        assert_eq!(
            parse_options("language = 'english', stopwords = 'english'")
                .unwrap()
                .len(),
            2
        );
        db.run("ALTER TEXT SEARCH DICTIONARY d1 (StopWords)").await;
        assert_eq!(
            db.one("SELECT dictinitoption FROM pg_ts_dict WHERE dictname = 'd1'")
                .await,
            "language = 'english'"
        );
        // Mapping order, by alias, as written and as altered.
        db.run("CREATE TEXT SEARCH CONFIGURATION c1 (PARSER = default); ALTER TEXT SEARCH CONFIGURATION c1 ADD MAPPING FOR asciiword, word WITH d1, pg_catalog.simple; ALTER TEXT SEARCH CONFIGURATION c1 ADD MAPPING FOR email WITH pg_catalog.simple; ALTER TEXT SEARCH CONFIGURATION c1 ALTER MAPPING FOR asciiword WITH pg_catalog.simple, d1").await;
        let order = db
            .one("SELECT string_agg(tt.alias || ':' || d.dictname, ',' ORDER BY m.maptokentype, m.mapseqno) FROM pg_ts_config_map m JOIN pg_ts_config c ON c.oid = m.mapcfg JOIN pg_ts_dict d ON d.oid = m.mapdict JOIN ts_token_type(c.cfgparser) tt ON tt.tokid = m.maptokentype WHERE c.cfgname = 'c1'")
            .await;
        assert_eq!(
            order,
            "asciiword:simple,asciiword:d1,word:d1,word:simple,email:simple"
        );
        // `ADD MAPPING` on a mapped token type fails, and `ALTER` or `DROP MAPPING` on an
        // unmapped one does nothing (DEC-2728.3): each token type needs the one verb that
        // says what it means.
        assert_eq!(
            db.refused("ALTER TEXT SEARCH CONFIGURATION c1 ADD MAPPING FOR email WITH d1")
                .await,
            "23505"
        );
        db.run("ALTER TEXT SEARCH CONFIGURATION c1 ALTER MAPPING FOR url WITH d1; ALTER TEXT SEARCH CONFIGURATION c1 DROP MAPPING FOR url").await;
        assert_eq!(
            db.one("SELECT count(*)::text FROM pg_ts_config_map WHERE mapcfg = 'app.c1'::regconfig AND maptokentype = 5").await,
            "0"
        );
        // A copy is its result: the catalog keeps no trace of `COPY`.
        db.run("CREATE TEXT SEARCH CONFIGURATION copied (COPY = pg_catalog.english)")
            .await;
        assert!(
            truth(db.conn(), "SELECT (SELECT count(*) FROM pg_ts_config_map WHERE mapcfg = 'app.copied'::regconfig) = (SELECT count(*) FROM pg_ts_config_map WHERE mapcfg = 'pg_catalog.english'::regconfig)").await
        );
        // One name, three namespaces.
        db.run("CREATE TEXT SEARCH CONFIGURATION russian (PARSER = default); CREATE TEXT SEARCH DICTIONARY russian (TEMPLATE = simple); CREATE DOMAIN russian AS text").await;
        // A file-backed dictionary opens its files when it is created.
        for sql in [
            "CREATE TEXT SEARCH DICTIONARY syn (TEMPLATE = synonym, SYNONYMS = nonexistent_file)",
            "CREATE TEXT SEARCH DICTIONARY thes (TEMPLATE = thesaurus, DICTFILE = missingthes, DICTIONARY = pg_catalog.english_stem)",
        ] {
            db.refused(sql).await;
        }
        assert!(
            !truth(
                db.conn(),
                "SELECT EXISTS (SELECT 1 FROM pg_ts_dict WHERE dictname IN ('syn', 'thes'))"
            )
            .await
        );
        // A dependent index blocks the drop, and a changed mapping leaves it unbuilt.
        db.run("CREATE TABLE docs (body text); CREATE INDEX docs_ix ON docs USING gin (to_tsvector('app.c1', body))").await;
        assert_eq!(
            db.refused("DROP TEXT SEARCH CONFIGURATION c1").await,
            "2BP01"
        );
        let before = db
            .one("SELECT relfilenode::text FROM pg_class WHERE relname = 'docs_ix'")
            .await;
        db.run(
            "ALTER TEXT SEARCH CONFIGURATION c1 ALTER MAPPING FOR asciiword WITH pg_catalog.simple",
        )
        .await;
        assert_eq!(
            db.one("SELECT relfilenode::text FROM pg_class WHERE relname = 'docs_ix'")
                .await,
            before
        );
        // What the census reads from `pg_depend` (DEC-2728.3): the index, a stored generated
        // column's default and a check go stale with the configuration; a view and a plain
        // default are evaluated on use and are not listed. A dictionary answers for the
        // configuration that maps it, and a dictionary nothing uses for nothing.
        db.run("CREATE VIEW v AS SELECT to_tsvector('app.c1', body) FROM docs; CREATE TABLE gen (body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('app.c1', body)) STORED, plain tsvector DEFAULT to_tsvector('app.c1', 'x'), CONSTRAINT k CHECK (to_tsvector('app.c1', body) IS NOT NULL)); CREATE MATERIALIZED VIEW mv AS SELECT to_tsvector('app.c1', body) AS x FROM docs").await;
        let subject = |text: &str| -> std::collections::BTreeSet<SearchRef> {
            [text.parse().unwrap()].into()
        };
        let census = |found: std::collections::BTreeMap<SearchRef, pbps_model::SearchCensus>| {
            found.into_values().next().unwrap().stored.join("; ")
        };
        let standing = Default::default();
        let by_config = census(
            pbps_pg::search::stale_dependents(db.conn(), &standing, &subject("config app.c1"))
                .await
                .unwrap(),
        );
        for stale in [
            "index app.docs_ix",
            "default value for app.gen.tsv",
            "table constraint k on app.gen",
            "materialized view app.mv",
        ] {
            assert!(by_config.contains(stale), "{stale}: {by_config}");
        }
        for fine in ["view app.v", "app.gen.plain"] {
            assert!(!by_config.contains(fine), "{fine}: {by_config}");
        }
        let by_dictionary = census(
            pbps_pg::search::stale_dependents(db.conn(), &standing, &subject("dictionary app.d1"))
                .await
                .unwrap(),
        );
        assert_eq!(by_dictionary, by_config);
        assert!(
            census(
                pbps_pg::search::stale_dependents(
                    db.conn(),
                    &standing,
                    &subject("dictionary app.d2")
                )
                .await
                .unwrap()
            )
            .is_empty()
        );
        db.finish().await;
    }
}

/// The census follows what only reads at run time down to what stores (DEC-2728.3): a
/// materialized view over a view over the configuration, through one view or two or a
/// function, is as stale as one over the configuration, an index on that materialized view
/// leaves the line on the materialized view, and a chain of views and functions that reaches
/// no stored dependent lists nothing. The dictionary path walks the same way.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_stored_dependent_behind_a_view_or_a_function_is_found_and_a_read_only_chain_is_not() {
    let _live = super::live_ledger().await;
    for variable in SERVERS {
        let mut db = Private::on(variable, "chain").await;
        db.run("CREATE TEXT SEARCH DICTIONARY d1 (TEMPLATE = simple); CREATE TEXT SEARCH CONFIGURATION c1 (PARSER = default); ALTER TEXT SEARCH CONFIGURATION c1 ADD MAPPING FOR asciiword WITH d1; CREATE TABLE docs (body text)").await;
        // Three configurations, one chain each, so every census line is one chain's.
        db.run("CREATE TEXT SEARCH CONFIGURATION c_one (COPY = app.c1); CREATE TEXT SEARCH CONFIGURATION c_two (COPY = app.c1); CREATE TEXT SEARCH CONFIGURATION c_fn (COPY = app.c1); CREATE TEXT SEARCH CONFIGURATION c_read (COPY = app.c1); CREATE TEXT SEARCH CONFIGURATION c_cycle (COPY = app.c1)").await;
        db.run("CREATE VIEW one_v AS SELECT to_tsvector('app.c_one', body) AS x FROM docs; CREATE MATERIALIZED VIEW one_mv AS SELECT x FROM one_v").await;
        db.run("CREATE VIEW two_v1 AS SELECT to_tsvector('app.c_two', body) AS x FROM docs; CREATE VIEW two_v2 AS SELECT x FROM two_v1; CREATE MATERIALIZED VIEW two_mv AS SELECT x FROM two_v2; CREATE INDEX two_ix ON two_mv USING gin (x)").await;
        db.run("CREATE FUNCTION fn_ts(t text) RETURNS tsvector LANGUAGE sql BEGIN ATOMIC SELECT to_tsvector('app.c_fn', t); END; CREATE VIEW fn_v AS SELECT fn_ts(body) AS x FROM docs; CREATE MATERIALIZED VIEW fn_mv AS SELECT x FROM fn_v").await;
        db.run("CREATE VIEW read_v1 AS SELECT to_tsvector('app.c_read', body) AS x FROM docs; CREATE VIEW read_v2 AS SELECT x FROM read_v1; CREATE FUNCTION read_fn() RETURNS bigint LANGUAGE sql BEGIN ATOMIC SELECT count(*) FROM read_v2; END").await;
        db.run("CREATE VIEW cyc_v AS SELECT to_tsvector('app.c_cycle', body) AS x FROM docs; CREATE FUNCTION cyc_fn() RETURNS bigint LANGUAGE sql BEGIN ATOMIC SELECT count(*) FROM cyc_v; END; CREATE OR REPLACE VIEW cyc_v AS SELECT to_tsvector('app.c_cycle', body) AS x FROM docs WHERE cyc_fn() >= 0").await;
        let census = |found: std::collections::BTreeMap<SearchRef, pbps_model::SearchCensus>| {
            found.into_values().next().unwrap().stored
        };
        let standing = Default::default();
        for (config, stored) in [
            ("c_one", vec!["materialized view app.one_mv"]),
            ("c_two", vec!["materialized view app.two_mv"]),
            ("c_fn", vec!["materialized view app.fn_mv"]),
            ("c_read", vec![]),
            ("c_cycle", vec![]),
        ] {
            let subject: std::collections::BTreeSet<SearchRef> =
                [format!("config app.{config}").parse().unwrap()].into();
            let found = census(
                pbps_pg::search::stale_dependents(db.conn(), &standing, &subject)
                    .await
                    .unwrap(),
            );
            assert_eq!(found, stored, "{config}");
        }
        // The dictionary path: a view that names the dictionary, then a materialized view over it.
        db.run("CREATE VIEW dict_v AS SELECT ts_lexize('app.d1'::regdictionary, body) AS x FROM docs; CREATE MATERIALIZED VIEW dict_mv AS SELECT x FROM dict_v").await;
        let subject: std::collections::BTreeSet<SearchRef> =
            ["dictionary app.d1".parse().unwrap()].into();
        let found = census(
            pbps_pg::search::stale_dependents(db.conn(), &standing, &subject)
                .await
                .unwrap(),
        );
        assert!(
            found.contains(&"materialized view app.dict_mv".to_owned()),
            "{found:?}"
        );
        assert!(
            !found.iter().any(|l| l.contains("_v") && !l.contains("mv")),
            "a view is not stale: {found:?}"
        );
        db.finish().await;
    }
}

/// The catalog has no edge from a function's text body to what it names (measured on 16 and 18:
/// a `LANGUAGE sql` string body and a PL/pgSQL body, called by an index, a stored generated
/// column, a check and a materialized view, each leave only the edge from the stored dependent
/// to the function), so the census does not guess (DEC-2728.4): a stored dependent that reaches
/// such a function, directly, through a view or through a `BEGIN ATOMIC` wrapper, is listed as
/// opaque with that function, whatever the body says, and nothing reads the body. Negatives: a
/// function no stored dependent reaches, a `BEGIN ATOMIC` function (the catalog proves its
/// edge, so its dependent is a stored one and not an opaque one) and a built-in function list
/// nothing opaque; a dependent the catalog proves stale is listed once, as stored.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_stored_dependent_behind_a_text_bodied_function_is_opaque_and_nothing_else_is() {
    let _live = super::live_ledger().await;
    for variable in SERVERS {
        let mut db = Private::on(variable, "opaque").await;
        db.run("CREATE TEXT SEARCH DICTIONARY d1 (TEMPLATE = simple); CREATE TEXT SEARCH CONFIGURATION c1 (PARSER = default); ALTER TEXT SEARCH CONFIGURATION c1 ADD MAPPING FOR asciiword WITH d1; CREATE TABLE docs (body text)").await;
        db.run("CREATE FUNCTION f_sql(t text) RETURNS tsvector LANGUAGE sql IMMUTABLE AS $$ SELECT to_tsvector('app.c1', t) $$; \
                CREATE FUNCTION f_pl(t text) RETURNS tsvector LANGUAGE plpgsql IMMUTABLE AS $$ BEGIN RETURN to_tsvector('app.c1', t); END $$; \
                CREATE FUNCTION f_other(t text) RETURNS tsvector LANGUAGE sql IMMUTABLE AS $$ SELECT to_tsvector('simple', t) $$; \
                CREATE FUNCTION f_unused(t text) RETURNS tsvector LANGUAGE sql IMMUTABLE AS $$ SELECT to_tsvector('app.c1', t) $$; \
                CREATE FUNCTION f_wrap(t text) RETURNS tsvector LANGUAGE sql IMMUTABLE BEGIN ATOMIC SELECT f_sql(t); END; \
                CREATE FUNCTION f_atomic(t text) RETURNS tsvector LANGUAGE sql IMMUTABLE BEGIN ATOMIC SELECT to_tsvector('app.c1', t); END").await;
        db.run("CREATE INDEX ix_sql ON docs USING gin (f_sql(body)); \
                CREATE INDEX ix_other ON docs USING gin (f_other(body)); \
                CREATE INDEX ix_wrap ON docs USING gin (f_wrap(body)); \
                CREATE INDEX ix_atomic ON docs USING gin (f_atomic(body)); \
                CREATE TABLE gen (body text, tsv tsvector GENERATED ALWAYS AS (f_pl(body)) STORED, CONSTRAINT k CHECK (f_pl(body) IS NOT NULL)); \
                CREATE VIEW v_pl AS SELECT f_pl(body) AS x FROM docs; \
                CREATE MATERIALIZED VIEW mv_pl AS SELECT x FROM v_pl; \
                CREATE INDEX ix_builtin ON docs USING gin (to_tsvector('simple', body))").await;
        let subject: std::collections::BTreeSet<SearchRef> =
            ["config app.c1".parse().unwrap()].into();
        let found = pbps_pg::search::stale_dependents(db.conn(), &Default::default(), &subject)
            .await
            .unwrap()
            .into_values()
            .next()
            .unwrap();
        let behind = |what: &str, function: &str| {
            format!(
                "{what} (behind function app.{function}(pg_catalog.text), whose body the catalog cannot read)"
            )
        };
        // The catalog proves the `BEGIN ATOMIC` function's edge to the configuration, so its index
        // is a stored dependent, and the wrapper over a string body is not one.
        assert_eq!(found.stored, ["index app.ix_atomic"], "{found:?}");
        let mut opaque = vec![
            behind("index app.ix_other", "f_other"),
            behind("index app.ix_sql", "f_sql"),
            behind("index app.ix_wrap", "f_sql"),
            behind("materialized view app.mv_pl", "f_pl"),
            behind("default value for app.gen.tsv", "f_pl"),
            behind("table constraint k on app.gen", "f_pl"),
        ];
        opaque.sort();
        assert_eq!(found.opaque, opaque, "{found:?}");
        // A body is never read: the dependent of a function that names another configuration is
        // listed too, and the unused function and the built-in one are not.
        assert!(
            !found
                .opaque
                .iter()
                .any(|l| l.contains("f_unused") || l.contains("ix_builtin"))
        );
        // Nothing stands behind a function for a project that has none: the same database with
        // those dependents gone lists nothing opaque.
        db.run("DROP MATERIALIZED VIEW mv_pl; DROP VIEW v_pl; DROP TABLE gen; DROP INDEX ix_sql, ix_other, ix_wrap").await;
        let found = pbps_pg::search::stale_dependents(db.conn(), &Default::default(), &subject)
            .await
            .unwrap()
            .into_values()
            .next()
            .unwrap();
        assert!(found.opaque.is_empty(), "{found:?}");
        db.finish().await;
    }
}

/// A dictionary the model does not hold is an opaque node when its options cannot be read as
/// written (a `dictionary` option with no schema), or name the subject, or another such
/// dictionary (a thesaurus's sub-dictionary leaves no catalog edge, measured on 16 and 18);
/// the stored dependents behind it are listed with it (DEC-2728.4). Negatives: the same
/// dictionary is a held one, and so a proven member of the closure and not an opaque node, when
/// the model holds it; a dictionary that names another dictionary, and one that names
/// nothing, are not nodes; and an opaque dictionary nothing stored stands behind lists nothing.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn a_dictionary_the_model_does_not_hold_that_may_name_the_object_is_an_opaque_node() {
    let _live = super::live_ledger().await;
    for variable in SERVERS {
        let mut db = Private::on(variable, "unheld").await;
        db.run("CREATE TEXT SEARCH DICTIONARY d1 (TEMPLATE = simple); CREATE TEXT SEARCH DICTIONARY d2 (TEMPLATE = simple); CREATE TABLE docs (body text)").await;
        // `th_named` names d1 with its schema, `th_bare` names it without one, `th_plain` names a
        // dictionary that is not d1, and `th_idle` names d1 but nothing maps it.
        db.run("CREATE TEXT SEARCH DICTIONARY th_named (TEMPLATE = thesaurus, DICTFILE = thesaurus_sample, DICTIONARY = app.d1); \
                CREATE TEXT SEARCH DICTIONARY th_bare (TEMPLATE = thesaurus, DICTFILE = thesaurus_sample, DICTIONARY = d2); \
                CREATE TEXT SEARCH DICTIONARY th_plain (TEMPLATE = thesaurus, DICTFILE = thesaurus_sample, DICTIONARY = app.d2); \
                CREATE TEXT SEARCH DICTIONARY th_idle (TEMPLATE = thesaurus, DICTFILE = thesaurus_sample, DICTIONARY = app.d1)").await;
        db.run("CREATE TEXT SEARCH CONFIGURATION c_named (PARSER = default); ALTER TEXT SEARCH CONFIGURATION c_named ADD MAPPING FOR asciiword WITH th_named; \
                CREATE TEXT SEARCH CONFIGURATION c_bare (PARSER = default); ALTER TEXT SEARCH CONFIGURATION c_bare ADD MAPPING FOR asciiword WITH th_bare; \
                CREATE TEXT SEARCH CONFIGURATION c_plain (PARSER = default); ALTER TEXT SEARCH CONFIGURATION c_plain ADD MAPPING FOR asciiword WITH th_plain; \
                CREATE INDEX ix_named ON docs USING gin (to_tsvector('app.c_named', body)); \
                CREATE INDEX ix_bare ON docs USING gin (to_tsvector('app.c_bare', body)); \
                CREATE INDEX ix_plain ON docs USING gin (to_tsvector('app.c_plain', body))").await;
        let subject = |text: &str| -> std::collections::BTreeSet<SearchRef> {
            [text.parse().unwrap()].into()
        };
        let census = |found: std::collections::BTreeMap<SearchRef, pbps_model::SearchCensus>| {
            found.into_values().next().unwrap()
        };
        // None of them is held: the census reads them from the catalog.
        let found = census(
            pbps_pg::search::stale_dependents(
                db.conn(),
                &Default::default(),
                &subject("dictionary app.d1"),
            )
            .await
            .unwrap(),
        );
        assert!(found.stored.is_empty(), "{found:?}");
        let named = "its options name text search dictionary `app.d1`";
        let bare =
            "its options could not be read: its option `dictionary` names `d2` without a schema";
        assert_eq!(
            found.opaque,
            [
                format!(
                    "index app.ix_bare (behind dictionary app.th_bare, which the model does not hold: {bare})"
                ),
                format!(
                    "index app.ix_named (behind dictionary app.th_named, which the model does not hold: {named})"
                ),
            ],
            "{found:?}"
        );
        // A configuration is never named by a dictionary's options: nothing opaque for it.
        let found = census(
            pbps_pg::search::stale_dependents(
                db.conn(),
                &Default::default(),
                &subject("config app.c_plain"),
            )
            .await
            .unwrap(),
        );
        assert_eq!(found.stored, ["index app.ix_plain"]);
        assert!(found.opaque.is_empty(), "{found:?}");
        // Held by the model: a named qualified sub-dictionary is a proven member of the closure,
        // so its configuration's dependent is a stored one. The bare one stays unreadable.
        let rows = db
            .one(&format!(
                "SELECT jsonb_agg(to_jsonb(q))::text FROM ({}) q",
                query()
            ))
            .await;
        let rows: Vec<serde_json::Value> = serde_json::from_str(&rows).unwrap();
        let (held, _) = decode(&rows).unwrap();
        let found = census(
            pbps_pg::search::stale_dependents(db.conn(), &held, &subject("dictionary app.d1"))
                .await
                .unwrap(),
        );
        assert_eq!(found.stored, ["index app.ix_named"], "{found:?}");
        assert!(
            !found.opaque.iter().any(|l| l.contains("th_named")),
            "{found:?}"
        );
        // An opaque dictionary with nothing stored behind it carries nothing.
        db.run("DROP INDEX ix_bare").await;
        let found = census(
            pbps_pg::search::stale_dependents(db.conn(), &held, &subject("dictionary app.d1"))
                .await
                .unwrap(),
        );
        assert!(found.opaque.is_empty(), "{found:?}");
        db.finish().await;
    }
}

/// The pull's own query and decoder against the catalog: what a user made is read, in the
/// engine's order and with a copy expanded; what the engine ships is not (the system-object
/// exclusion), nor a package's member, nor anything over a parser or a template outside
/// `pg_catalog`, nor a configuration that maps a dictionary left out.
#[tokio::test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
async fn the_pull_reads_what_a_user_made_and_nothing_else() {
    let _live = super::live_ledger().await;
    for variable in SERVERS {
        let mut db = Private::on(variable, "pull").await;
        db.run("CREATE TEXT SEARCH DICTIONARY d1 (TEMPLATE = snowball, Language = english); CREATE TEXT SEARCH CONFIGURATION c1 (PARSER = default); ALTER TEXT SEARCH CONFIGURATION c1 ADD MAPPING FOR asciiword WITH d1, pg_catalog.simple; ALTER TEXT SEARCH CONFIGURATION c1 ADD MAPPING FOR email WITH pg_catalog.simple; CREATE TEXT SEARCH CONFIGURATION copied (COPY = pg_catalog.english); CREATE TEXT SEARCH CONFIGURATION russian (PARSER = default); CREATE TEXT SEARCH DICTIONARY russian (TEMPLATE = simple)").await;
        // Left out: a custom parser, a custom template, a dictionary over it, a configuration
        // that maps that dictionary, and a configuration a package owns.
        db.run("CREATE TEXT SEARCH PARSER myparser (START = prsd_start, GETTOKEN = prsd_nexttoken, END = prsd_end, LEXTYPES = prsd_lextype); CREATE TEXT SEARCH CONFIGURATION overparser (PARSER = myparser); CREATE TEXT SEARCH TEMPLATE mytmpl (INIT = dsimple_init, LEXIZE = dsimple_lexize); CREATE TEXT SEARCH DICTIONARY overtmpl (TEMPLATE = mytmpl); CREATE TEXT SEARCH CONFIGURATION mapsover (PARSER = default); ALTER TEXT SEARCH CONFIGURATION mapsover ADD MAPPING FOR word WITH overtmpl").await;
        db.run("CREATE EXTENSION unaccent SCHEMA public; CREATE TEXT SEARCH CONFIGURATION owned (COPY = pg_catalog.simple); ALTER EXTENSION unaccent ADD TEXT SEARCH CONFIGURATION owned; CREATE TEXT SEARCH CONFIGURATION usesunaccent (COPY = pg_catalog.simple); ALTER TEXT SEARCH CONFIGURATION usesunaccent ALTER MAPPING FOR word WITH public.unaccent, pg_catalog.simple").await;
        let rows = db
            .one(&format!(
                "SELECT jsonb_agg(to_jsonb(q))::text FROM ({}) q",
                query()
            ))
            .await;
        let rows: Vec<serde_json::Value> = serde_json::from_str(&rows).unwrap();
        let (held, left_out) = decode(&rows).unwrap();
        let held_names: Vec<String> = held.keys().map(|k| k.raw().to_string()).collect();
        assert_eq!(
            held_names,
            [
                "config app.c1",
                "config app.copied",
                "config app.russian",
                "config app.usesunaccent",
                "dictionary app.d1",
                "dictionary app.russian",
            ],
            "{left_out:?}"
        );
        assert!(left_out.is_empty(), "{left_out:?}");
        // The order the engine kept, by alias; a mapping to a system or a package dictionary
        // is held as that reference.
        let config =
            |name: &str| SearchRef::new(SearchKind::Config, format!("app.{name}").parse().unwrap());
        let SearchObject::Config { parser, mappings } = &held[&config("c1")] else {
            panic!("a configuration")
        };
        // The canonical spelling quotes nothing it need not: `default` is a name here.
        assert_eq!(parser.raw().to_string(), "pg_catalog.default");
        let listed = |name: &str| -> Vec<String> {
            let SearchObject::Config { mappings, .. } = &held[&config("usesunaccent")] else {
                panic!("a configuration")
            };
            mappings[name].iter().map(|d| d.raw().to_string()).collect()
        };
        assert_eq!(
            mappings["asciiword"]
                .iter()
                .map(|d| d.raw().to_string())
                .collect::<Vec<_>>(),
            ["app.d1", "pg_catalog.simple"]
        );
        assert_eq!(listed("word"), ["public.unaccent", "pg_catalog.simple"]);
        // A copy is expanded to its mappings.
        let SearchObject::Config {
            mappings: copied, ..
        } = &held[&config("copied")]
        else {
            panic!("a configuration")
        };
        assert!(
            copied.len() > 2 && copied.contains_key("asciiword"),
            "{copied:?}"
        );
        // The options are held verbatim.
        let SearchObject::Dictionary { template, options } =
            &held[&SearchRef::new(SearchKind::Dictionary, "app.d1".parse().unwrap())]
        else {
            panic!("a dictionary")
        };
        assert_eq!(template.raw().to_string(), "pg_catalog.snowball");
        assert_eq!(options["language"], "english");
        db.finish().await;
    }
}
