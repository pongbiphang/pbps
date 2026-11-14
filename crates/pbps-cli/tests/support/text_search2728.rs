//! Text search configurations and dictionaries through the tool (#2728 PR A, DEC-2728.1):
//! pulled into declaration files, pulled again with no difference and an empty follow-up
//! plan, a copied configuration expanded, what is not held left out and named, and every
//! difference in a standing object refused by name. Part B (DEC-2728.2) creates and drops
//! them: the order, the recorded reason, the server-file advisory. Each case runs on both
//! pinned engines.

use super::rehearsal2379::{Fixture, Kind, Options};
use super::{
    Demo, OwnDatabase, apply_plan, approved_apply, code, on_server, scalar, server, stderr, stdout,
    succeeds,
};
use std::collections::BTreeMap;
use std::process::Output;

fn servers(mut check: impl FnMut(&str, &str)) {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        check(&std::env::var(variable).unwrap(), variable);
    }
}

fn said(output: &Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

fn fixture(server: &str, slug: &str) -> Fixture {
    Fixture::new(server, slug, Kind::Supplied, &Options::default())
}

/// What a user made: a dictionary, a configuration over it and the built-in one, a copied
/// configuration, a configuration and a dictionary of one name beside a domain of it, and
/// the objects that name a configuration by a string literal: a stored generated `tsvector`
/// column and a CHECK. An expression index over `to_tsvector('app.x', body)` is not here:
/// the model does not hold a GIN index over an expression (it is left out of a pull) until
/// the key types of #2448 arrive, so the index case waits for them.
const MADE: &str = "\
    CREATE TEXT SEARCH DICTIONARY app.english_nostop (TEMPLATE = snowball, Language = english); \
    CREATE TEXT SEARCH CONFIGURATION app.docs (PARSER = pg_catalog.\"default\"); \
    ALTER TEXT SEARCH CONFIGURATION app.docs ADD MAPPING FOR asciiword, word WITH app.english_nostop; \
    ALTER TEXT SEARCH CONFIGURATION app.docs ADD MAPPING FOR email WITH pg_catalog.simple; \
    CREATE TEXT SEARCH CONFIGURATION app.copied (COPY = pg_catalog.english); \
    CREATE TEXT SEARCH CONFIGURATION app.russian (PARSER = pg_catalog.\"default\"); \
    CREATE TEXT SEARCH DICTIONARY app.russian (TEMPLATE = pg_catalog.simple); \
    CREATE DOMAIN app.russian AS text; \
    CREATE TABLE app.articles (id integer PRIMARY KEY, body text, \
      tsv tsvector GENERATED ALWAYS AS (to_tsvector('app.docs', body)) STORED, \
      CONSTRAINT articles_indexable CHECK (to_tsvector('app.docs', body) IS NOT NULL OR body IS NULL))";

/// The text search files under `schema/`, by name, with their text.
fn search_files(f: &Fixture) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(f.d.dir.join("schema")).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.contains(".textsearch-") {
            out.insert(name, std::fs::read_to_string(&path).unwrap());
        }
    }
    out
}

fn no_changes(out: &Output) {
    assert_eq!(code(out), 0, "{}", said(out));
    assert!(stdout(out).contains("No changes"), "{}", said(out));
}

fn pull(f: &Fixture) -> Output {
    succeeds(f.d.run(&["pull", "--force", "--db", f.target()]))
}

/// A database with objects of both kinds, a copied configuration and an index and a
/// generated column over a configuration is pulled; pulled again, the files are the same
/// bytes; the plan against the database is empty and the recorded state verifies. The
/// system configurations and dictionaries the engine ships are not among the files.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_database_with_text_search_objects_pulls_and_pulls_again_with_no_difference() {
    servers(|server, slug| {
        let f = fixture(server, &format!("search2728-roundtrip-{slug}"));
        on_server(f.target(), MADE);
        pull(&f);
        let files = search_files(&f);
        let names: Vec<&str> = files.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            [
                "app.copied.textsearch-config.yml",
                "app.docs.textsearch-config.yml",
                "app.english_nostop.textsearch-dictionary.yml",
                "app.russian.textsearch-config.yml",
                "app.russian.textsearch-dictionary.yml",
            ],
            "the user's objects and nothing the engine ships"
        );
        let docs = &files["app.docs.textsearch-config.yml"];
        assert!(
            docs.contains("parser: pg_catalog.default")
                && docs.contains("asciiword: [app.english_nostop]")
                && docs.contains("email: [pg_catalog.simple]"),
            "{docs}"
        );
        let dictionary = &files["app.english_nostop.textsearch-dictionary.yml"];
        assert!(
            dictionary.contains("template: pg_catalog.snowball")
                && dictionary.contains("language: english"),
            "{dictionary}"
        );
        // A copy is expanded: it names mappings, and the word `copy` is nowhere.
        let copied = &files["app.copied.textsearch-config.yml"];
        assert!(
            copied.contains("asciiword: [pg_catalog.english_stem]") && !copied.contains("copy"),
            "{copied}"
        );
        // The same bytes the second time.
        pull(&f);
        assert_eq!(search_files(&f), files);
        f.d.commit();
        succeeds(f.d.run(&[
            "baseline",
            "--db",
            f.target(),
            "--reason",
            "adopt the text search objects",
        ]));
        no_changes(&succeeds(f.d.run(&["plan", "--db", f.target()])));
        succeeds(f.d.run(&["verify", "--db", f.target()]));
        // A text search object made outside the declarations after the baseline is not
        // drift (they have no identity yet, DEC-2728.1): verify and the plan are as before.
        on_server(
            f.target(),
            "CREATE TEXT SEARCH DICTIONARY app.made_later (TEMPLATE = pg_catalog.simple)",
        );
        succeeds(f.d.run(&["verify", "--db", f.target()]));
        no_changes(&succeeds(f.d.run(&["plan", "--db", f.target()])));
    });
}

/// What the model does not hold is left out and named, never read as less than it is:
/// a configuration over a custom parser, a dictionary over a custom template and the
/// configuration that maps it, and a configuration a package owns. A configuration that maps
/// a package's dictionary is held as that reference.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn what_the_model_does_not_hold_is_left_out_and_named() {
    servers(|server, slug| {
        let f = fixture(server, &format!("search2728-left-{slug}"));
        on_server(
            f.target(),
            "CREATE TEXT SEARCH PARSER app.myparser (START = prsd_start, GETTOKEN = prsd_nexttoken, END = prsd_end, LEXTYPES = prsd_lextype); \
             CREATE TEXT SEARCH CONFIGURATION app.overparser (PARSER = app.myparser); \
             CREATE TEXT SEARCH TEMPLATE app.mytmpl (INIT = dsimple_init, LEXIZE = dsimple_lexize); \
             CREATE TEXT SEARCH DICTIONARY app.overtmpl (TEMPLATE = app.mytmpl); \
             CREATE TEXT SEARCH CONFIGURATION app.mapsover (PARSER = pg_catalog.\"default\"); \
             ALTER TEXT SEARCH CONFIGURATION app.mapsover ADD MAPPING FOR word WITH app.overtmpl; \
             CREATE EXTENSION unaccent SCHEMA public; \
             CREATE TEXT SEARCH CONFIGURATION app.owned (COPY = pg_catalog.simple); \
             ALTER EXTENSION unaccent ADD TEXT SEARCH CONFIGURATION app.owned; \
             CREATE TEXT SEARCH CONFIGURATION app.usesunaccent (COPY = pg_catalog.simple); \
             ALTER TEXT SEARCH CONFIGURATION app.usesunaccent ALTER MAPPING FOR word WITH public.unaccent, pg_catalog.simple",
        );
        // A parser and a template made in `pg_catalog` itself, which shares the namespace of
        // the built-in ones and is no more held than a custom one anywhere else (the pull
        // tests the name, as `validate` does). Whether the engine lets a superuser make
        // one at all is measured here: the cases below follow when it does.
        let in_catalog = super::try_on_server(
            f.target(),
            "SET allow_system_table_mods = on; \
             CREATE TEXT SEARCH PARSER pg_catalog.pbps_parser (START = prsd_start, GETTOKEN = prsd_nexttoken, END = prsd_end, LEXTYPES = prsd_lextype); \
             CREATE TEXT SEARCH CONFIGURATION app.overcatparser (PARSER = pg_catalog.pbps_parser); \
             CREATE TEXT SEARCH TEMPLATE pg_catalog.pbps_tmpl (INIT = dsimple_init, LEXIZE = dsimple_lexize); \
             CREATE TEXT SEARCH DICTIONARY app.overcattmpl (TEMPLATE = pg_catalog.pbps_tmpl)",
        );
        eprintln!("measure: a parser and a template in pg_catalog on {slug}: {in_catalog:?}");
        let out = pull(&f);
        let notes = said(&out);
        let files = search_files(&f);
        let names: Vec<&str> = files.keys().map(String::as_str).collect();
        if in_catalog.is_ok() {
            for named in ["overcatparser", "overcattmpl"] {
                assert!(notes.contains(named), "{named} is named: {notes}");
            }
        }
        assert_eq!(
            names,
            ["app.usesunaccent.textsearch-config.yml"],
            "only the configuration over the built-in parser and held dictionaries"
        );
        assert!(
            files["app.usesunaccent.textsearch-config.yml"]
                .contains("word: [public.unaccent, pg_catalog.simple]"),
            "{files:?}"
        );
        for named in ["overparser", "overtmpl", "mapsover", "myparser", "mytmpl"] {
            assert!(
                notes.contains("text search") && notes.contains(named),
                "{named} is named: {notes}"
            );
        }
        // The package's own member is not this project's to name or hold.
        assert!(!notes.contains("`app.owned`"), "{notes}");
        // Negative: nothing of the system's own is among the files.
        assert!(files.keys().all(|n| !n.starts_with("pg_catalog.")));
    });
}

/// A declaration that differs from a standing object is refused by name (changing one is a
/// later part); an object only the target has is left alone.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn every_difference_is_refused_by_name_and_an_undeclared_object_is_left_alone() {
    servers(|server, slug| {
        let f = fixture(server, &format!("search2728-refused-{slug}"));
        on_server(f.target(), MADE);
        pull(&f);
        f.d.commit();
        succeeds(f.d.run(&[
            "baseline",
            "--db",
            f.target(),
            "--reason",
            "adopt the text search objects",
        ]));
        no_changes(&succeeds(f.d.run(&["plan", "--db", f.target()])));
        let dir = f.d.dir.join("schema");
        // An object the target holds differently.
        let path = dir.join("app.docs.textsearch-config.yml");
        let original = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{original}  url: [pg_catalog.simple]\n")).unwrap();
        let out = f.d.run(&["plan", "--db", f.target()]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("text search configuration `app.docs`")
                && said(&out).contains("changing one arrives"),
            "{}",
            said(&out)
        );
        std::fs::write(&path, &original).unwrap();
        // Negative: an object only the target has is not this project's.
        on_server(
            f.target(),
            "CREATE TEXT SEARCH CONFIGURATION app.extra (COPY = pg_catalog.simple)",
        );
        no_changes(&succeeds(f.d.run(&["plan", "--db", f.target()])));
        // And it does not stop a plan that touches only a table from being rehearsed on the
        // scratch, where the base is built from the target's managed objects alone (ready
        // round 2).
        let table = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                std::fs::read_to_string(p).is_ok_and(|t| t.starts_with("table: app.articles"))
            })
            .unwrap();
        let kept = std::fs::read_to_string(&table).unwrap();
        std::fs::write(
            &table,
            kept.replacen("primary_key", "  note: {type: text}\nprimary_key", 1),
        )
        .unwrap();
        // A new column is a new identity, and a plan against a database is refused while the
        // identity file lags the declarations; the offline plan records it first.
        succeeds(f.d.run(&["plan", "--no-dev"]));
        f.d.commit();
        let out = succeeds(f.d.run(&["plan", "--db", f.target()]));
        assert!(said(&out).contains("note"), "{}", said(&out));
        // The column stays declared: taking it back out is a drop, which the offline plan
        // would stop on for want of a reason, and the steps below do not depend on it.
        // A declaration that was recorded and is gone is a drop, and it needs its reason.
        std::fs::remove_file(dir.join("app.copied.textsearch-config.yml")).unwrap();
        let out = f.d.run(&["plan", "--db", f.target()]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("drop-search \"config app.copied\""),
            "{}",
            said(&out)
        );
    });
}

fn declare(d: &Demo, name: &str, text: &str) {
    std::fs::write(d.dir.join("schema").join(name), text).unwrap();
}

const WORDS: &str = "text_search_dictionary: app.words\ntemplate: pg_catalog.snowball\n\
                     options:\n  language: english\n";
// In the form a pull writes (the parser unquoted, the aliases sorted), so the round trip
// below compares like with like.
const DOCS: &str = "text_search_config: app.docs\nparser: pg_catalog.default\nmappings:\n  \
                    asciiword: [app.words]\n  email: [pg_catalog.simple]\n  word: [app.words]\n";
const ARTICLES: &str = "table: app.articles\ncolumns:\n  id: {type: integer, nullable: false}\n  \
                        body: {type: text}\n  tsv: {type: tsvector, generated: {expression: \
                        \"to_tsvector('app.docs', body)\", stored: true}}\nprimary_key: [id]\n";

fn subjects(plan: &std::path::Path) -> Vec<String> {
    let saved: pbps_model::SavedPlan =
        serde_json::from_slice(&std::fs::read(plan).unwrap()).unwrap();
    saved
        .changes
        .changes
        .iter()
        .map(|p| p.change.subject())
        .collect()
}

fn at(order: &[String], subject: &str) -> usize {
    order
        .iter()
        .position(|s| s == subject)
        .unwrap_or_else(|| panic!("{subject} not planned: {order:?}"))
}

/// A dictionary, the configuration that maps it and a table whose generated column names
/// the configuration by a literal are created in one plan, in that order, and the next plan
/// is empty; pulled back, they are the same files. Then they are given up: without a
/// recorded reason the plan is refused naming the command, and with one the table goes
/// first, then the configuration, then the dictionary it mapped.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn declared_text_search_objects_are_created_in_order_and_dropped_with_a_reason() {
    for server in [server(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()] {
        let own = OwnDatabase::new(&server, "search2728b");
        let db = own.connection();
        on_server(db, "CREATE SCHEMA app");
        let d = Demo::new("search2728b");
        d.commit();
        succeeds(d.run(&["baseline", "--db", db, "--reason", "empty database"]));
        // Listed so that a plain alphabetical order would put the configuration first.
        declare(&d, "app.docs.textsearch-config.yml", DOCS);
        declare(&d, "app.words.textsearch-dictionary.yml", WORDS);
        declare(&d, "app.articles.yml", ARTICLES);
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let path = d.dir.join("search.plan.json");
        succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        let order = subjects(&path);
        let (words, docs, table) = (
            at(&order, "dictionary app.words"),
            at(&order, "config app.docs"),
            at(&order, "app.articles"),
        );
        assert!(words < docs && docs < table, "{order:?}");
        succeeds(apply_plan(&d, db, &path, false));
        assert_eq!(
            scalar(
                db,
                "SELECT count(*) FROM pg_ts_config WHERE cfgname = 'docs' AND cfgnamespace = 'app'::regnamespace"
            ),
            1
        );
        no_changes(&succeeds(d.run(&["plan", "--db", db])));
        succeeds(d.run(&["verify", "--db", db]));
        // The round trip: a pull writes back the files that were declared.
        let before: Vec<String> = [
            "app.docs.textsearch-config.yml",
            "app.words.textsearch-dictionary.yml",
        ]
        .iter()
        .map(|n| std::fs::read_to_string(d.dir.join("schema").join(n)).unwrap())
        .collect();
        succeeds(d.run(&["pull", "--force", "--db", db]));
        let after: Vec<String> = [
            "app.docs.textsearch-config.yml",
            "app.words.textsearch-dictionary.yml",
        ]
        .iter()
        .map(|n| std::fs::read_to_string(d.dir.join("schema").join(n)).unwrap())
        .collect();
        assert_eq!(
            after
                .iter()
                .map(|t| t.split_whitespace().collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            before
                .iter()
                .map(|t| t.split_whitespace().collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            "pull writes the declared objects back"
        );
        d.commit();
        // Given up without a reason: refused, naming the command that records it.
        for name in [
            "app.docs.textsearch-config.yml",
            "app.words.textsearch-dictionary.yml",
            "app.articles.yml",
        ] {
            std::fs::remove_file(d.dir.join("schema").join(name)).unwrap();
        }
        let out = d.run(&["plan", "--db", db]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("drop-search") && said(&out).contains("drop-table"),
            "{}",
            said(&out)
        );
        // Identity decisions are recorded together: separate commands cannot record a
        // partial answer, so the three drops go in one batch.
        succeeds(d.run(&[
            "intent",
            "--drop-table",
            "app.articles",
            "retired",
            "--drop-search",
            "config app.docs",
            "retired",
            "--drop-search",
            "dictionary app.words",
            "retired",
        ]));
        d.commit();
        succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        let order = subjects(&path);
        let (table, docs, words) = (
            at(&order, "app.articles"),
            at(&order, "config app.docs"),
            at(&order, "dictionary app.words"),
        );
        assert!(table < docs && docs < words, "{order:?}");
        // Destructive is a gate of its own: without it nothing runs and nothing is dropped.
        let gated = approved_apply(&d, db, &path, &[]);
        assert_ne!(code(&gated), 0, "{}", said(&gated));
        assert!(said(&gated).contains("destructive"), "{}", said(&gated));
        assert_eq!(
            scalar(
                db,
                "SELECT count(*) FROM pg_ts_config WHERE cfgnamespace = 'app'::regnamespace"
            ),
            1
        );
        // A managed configuration changed after the plan was approved is drift: the baseline
        // guard refuses the apply before any statement (DEC-2728.2), and putting the
        // configuration back makes the same plan apply.
        on_server(
            db,
            "ALTER TEXT SEARCH CONFIGURATION app.docs ALTER MAPPING FOR email WITH app.words",
        );
        let drifted = approved_apply(&d, db, &path, &["--allow", "destructive"]);
        assert_ne!(code(&drifted), 0, "{}", said(&drifted));
        assert!(
            said(&drifted).contains("no longer the database this plan was computed against"),
            "{}",
            said(&drifted)
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*) FROM pg_ts_config WHERE cfgnamespace = 'app'::regnamespace"
            ),
            1
        );
        on_server(
            db,
            "ALTER TEXT SEARCH CONFIGURATION app.docs ALTER MAPPING FOR email WITH pg_catalog.simple",
        );
        succeeds(approved_apply(&d, db, &path, &["--allow", "destructive"]));
        assert_eq!(
            scalar(
                db,
                "SELECT count(*) FROM pg_ts_config WHERE cfgnamespace = 'app'::regnamespace"
            ),
            0
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*) FROM pg_ts_dict WHERE dictnamespace = 'app'::regnamespace"
            ),
            0
        );
    }
}

/// A mapping to a dictionary that exists nowhere is refused at plan, by name; a dictionary
/// that reads a server file is listed with an advisory, and one whose file is missing makes
/// the apply fail and leaves nothing behind.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_missing_dictionary_is_refused_and_a_server_file_is_named_and_checked_by_the_create() {
    for server in [server(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()] {
        let own = OwnDatabase::new(&server, "search2728c");
        let db = own.connection();
        on_server(db, "CREATE SCHEMA app");
        let d = Demo::new("search2728c");
        d.commit();
        succeeds(d.run(&["baseline", "--db", db, "--reason", "empty database"]));
        declare(
            &d,
            "app.docs.textsearch-config.yml",
            &DOCS.replace("app.words", "app.nope"),
        );
        // Offline the baseline only stands for the target, and a dictionary made on the
        // database outside pbps is not in it: the plan goes on and its line says the
        // database settles the dictionary. Against the database, which does not hold
        // `app.nope`, it is refused by name (DEC-2728.2).
        let offline = succeeds(d.run(&["plan", "--no-dev"]));
        assert!(
            stdout(&offline).contains("app.nope")
                && stdout(&offline).contains("resolved when planned against the database"),
            "{}",
            said(&offline)
        );
        d.commit();
        let out = d.run(&["plan", "--db", db]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("app.nope") && said(&out).contains("does not hold"),
            "{}",
            said(&out)
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*) FROM pg_ts_config WHERE cfgnamespace = 'app'::regnamespace"
            ),
            0
        );
        // Declaring the dictionary settles the mapping: the plans below create it and the
        // configuration with the one that reads a server file. (Giving the configuration up
        // instead would be a drop, which needs its recorded reason.)
        declare(
            &d,
            "app.nope.textsearch-dictionary.yml",
            "text_search_dictionary: app.nope\ntemplate: pg_catalog.simple\n",
        );
        // A file the server holds: named in the plan, and the create is the check.
        let syn = |file: &str| {
            format!(
                "text_search_dictionary: app.syn\ntemplate: pg_catalog.synonym\noptions:\n  synonyms: {file}\n"
            )
        };
        declare(
            &d,
            "app.syn.textsearch-dictionary.yml",
            &syn("synonym_sample"),
        );
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let path = d.dir.join("syn.plan.json");
        let planned = succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        assert!(
            stdout(&planned).contains("reads server file `synonym_sample` for synonyms"),
            "{}",
            said(&planned)
        );
        succeeds(apply_plan(&d, db, &path, false));
        assert_eq!(
            scalar(db, "SELECT count(*) FROM pg_ts_dict WHERE dictname = 'syn'"),
            1
        );
        // Negative: a new dictionary whose file is not there fails its create, and the apply
        // leaves nothing of it. (`app.syn` stays as it is: changing one is a later part.)
        let gone = syn("no_such_file").replace("app.syn", "app.gone");
        declare(&d, "app.gone.textsearch-dictionary.yml", &gone);
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        let out = apply_plan(&d, db, &path, false);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert_eq!(
            scalar(
                db,
                "SELECT count(*) FROM pg_ts_dict WHERE dictname = 'gone'"
            ),
            0
        );
        assert_eq!(
            scalar(db, "SELECT count(*) FROM pg_ts_dict WHERE dictname = 'syn'"),
            1
        );
    }
}

/// A package that ships a text search dictionary (`dict_int`, a contrib module of the pinned
/// images), a configuration of the project mapping it, and the two retired together: the
/// engine refuses the package's removal while the configuration maps its dictionary, so the
/// plan drops the configuration first and the package after it (DEC-2728.2). No resolver is
/// selected, as for every plan that drops a package.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_configuration_over_a_package_dictionary_is_retired_with_the_package() {
    for server in [server(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()] {
        let f = Fixture::new(
            &server,
            "search2728e",
            Kind::Supplied,
            &Options {
                externals: Some(
                    "CREATE SCHEMA IF NOT EXISTS ext; CREATE EXTENSION dict_int SCHEMA ext",
                ),
                unselected: true,
                ..Options::default()
            },
        );
        succeeds(f.d.run(&[
            "pull",
            "--force",
            "--db",
            f.target(),
            "--extension",
            "dict_int",
        ]));
        f.d.commit();
        succeeds(f.d.run(&[
            "baseline",
            "--db",
            f.target(),
            "--reason",
            "adopt the package",
        ]));
        declare(
            &f.d,
            "app.sdocs.textsearch-config.yml",
            "text_search_config: app.sdocs\nparser: pg_catalog.default\nmappings:\n  asciiword: [ext.intdict]\n",
        );
        succeeds(f.d.run(&["plan"]));
        f.d.commit();
        succeeds(f.plan(&[]));
        succeeds(f.apply("destructive"));
        assert_eq!(
            scalar(
                f.target(),
                "SELECT count(*) FROM pg_ts_config WHERE cfgname = 'sdocs'"
            ),
            1
        );
        // Both given up, with their reasons recorded together.
        std::fs::remove_file(f.d.dir.join("schema/app.sdocs.textsearch-config.yml")).unwrap();
        std::fs::remove_file(f.d.dir.join("schema/extensions/dict_int.yml")).unwrap();
        succeeds(f.d.run(&[
            "intent",
            "--drop-search",
            "config app.sdocs",
            "retired",
            "--drop-extension",
            "dict_int",
            "retired",
        ]));
        f.d.commit();
        let planned = f.plan(&[]);
        assert_eq!(code(&planned), 0, "{}", said(&planned));
        let order = subjects(&f.artifact());
        let kinds: Vec<String> = {
            let saved: pbps_model::SavedPlan =
                serde_json::from_slice(&std::fs::read(f.artifact()).unwrap()).unwrap();
            saved
                .changes
                .changes
                .iter()
                .map(|p| format!("{:?}", p.change))
                .collect()
        };
        let search = kinds
            .iter()
            .position(|k| k.starts_with("DropSearch"))
            .unwrap_or_else(|| panic!("no DropSearch in {order:?}"));
        let package = kinds
            .iter()
            .position(|k| k.starts_with("DropExtension"))
            .unwrap_or_else(|| panic!("no DropExtension in {order:?}"));
        assert!(search < package, "{order:?}");
        succeeds(f.apply("destructive"));
        assert_eq!(
            scalar(
                f.target(),
                "SELECT count(*) FROM pg_ts_config WHERE cfgname = 'sdocs'"
            ),
            0
        );
        assert_eq!(
            scalar(
                f.target(),
                "SELECT count(*) FROM pg_extension WHERE extname = 'dict_int'"
            ),
            0
        );
    }
}
