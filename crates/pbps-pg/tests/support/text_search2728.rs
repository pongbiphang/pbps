//! Text search configurations and dictionaries against the engine (#2728 PR A,
//! DEC-2728.1): the facts the model rests on, asserted on the 16 and the 18 fixture, and
//! the pull's own query and decoder against what the catalog really holds. Each case runs in
//! a database of its own.
use super::{sqlstate, text, truth};
use pbps_db::{Conn, Driver};
use pbps_model::{SearchKind, SearchObject, SearchRef};
use pbps_pg::search::{TEMPLATES, TOKEN_ALIASES, decode, parse_options, query};

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
        let mut templates = TEMPLATES.to_vec();
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
