//! Text search configurations and dictionaries through the tool (#2728 PR A, DEC-2728.1):
//! pulled into declaration files, pulled again with no difference and an empty follow-up
//! plan, a copied configuration expanded, what is not held left out and named, and every
//! difference in a standing object refused by name. Part B (DEC-2728.2) creates and drops
//! them: the order, the recorded reason, the server-file advisory. Part C (DEC-2728.3) alters a
//! standing one in place and refuses what would go stale. Each case runs on both pinned
//! engines.

use super::rehearsal2379::{Fixture, Kind, Options};
use super::{
    Demo, OwnDatabase, apply_plan, approved_apply, code, on_server, scalar, server, stderr, stdout,
    succeeds, text_of,
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
             ALTER TEXT SEARCH CONFIGURATION app.usesunaccent ALTER MAPPING FOR word WITH public.unaccent, pg_catalog.simple; \
             CREATE TEXT SEARCH DICTIONARY app.thesbare (TEMPLATE = thesaurus, DICTFILE = thesaurus_sample, DICTIONARY = english_stem); \
             CREATE TEXT SEARCH DICTIONARY app.thesfull (TEMPLATE = thesaurus, DICTFILE = thesaurus_sample, DICTIONARY = pg_catalog.english_stem); \
             CREATE TEXT SEARCH CONFIGURATION app.overthes (PARSER = pg_catalog.\"default\"); \
             ALTER TEXT SEARCH CONFIGURATION app.overthes ADD MAPPING FOR word WITH app.thesbare",
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
            [
                "app.thesfull.textsearch-dictionary.yml",
                "app.usesunaccent.textsearch-config.yml"
            ],
            "only the configuration over the built-in parser and held dictionaries, and the \
             thesaurus whose sub-dictionary is qualified"
        );
        // A thesaurus naming its sub-dictionary without a schema has none that is true to
        // write: it is left out with the configuration over it, and the remedy is named. The
        // qualified one is written so, and what is written is read back.
        assert!(
            files["app.thesfull.textsearch-dictionary.yml"]
                .contains("dictionary: pg_catalog.english_stem"),
            "{files:?}"
        );
        assert!(
            notes.contains("thesbare")
                && notes.contains("schema.name")
                && notes.contains("overthes"),
            "{notes}"
        );
        assert!(!notes.contains("`app.thesfull`"), "{notes}");
        pull(&f);
        assert_eq!(
            search_files(&f),
            files,
            "a second pull reads its own files and changes none"
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
                && said(&out).contains("would keep the old behaviour")
                && said(&out).contains("app.articles"),
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

/// `pbps status` for the fixture's `prod` environment.
fn status(f: &Fixture) -> String {
    said(&f.d.run_with_env(&["status"], &[("PBPS_REH_TARGET", f.target())]))
}

fn baseline(f: &Fixture, reason: &str) {
    succeeds(f.d.run(&["baseline", "--db", f.target(), "--reason", reason]));
}

/// An object only the database holds, that no declaration names (#2855, DEC-2855.1), is
/// not recorded by `baseline` or `snapshot`, so taking it away is no drift for `verify`,
/// `status` or the `plan --db` gate; and while it stands it is no difference to `snapshot`.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_undeclared_text_search_object_is_not_recorded_and_dropping_it_is_no_drift() {
    servers(|server, slug| {
        let f = fixture(server, &format!("search2855-undeclared-{slug}"));
        on_server(
            f.target(),
            "CREATE TEXT SEARCH DICTIONARY app.undeclared (TEMPLATE = pg_catalog.simple)",
        );
        baseline(&f, "the database holds an object nothing declares");
        // Not a difference to the record, which never held it.
        succeeds(f.d.run(&["snapshot", "--db", f.target()]));
        succeeds(f.d.run(&["verify", "--db", f.target()]));
        on_server(f.target(), "DROP TEXT SEARCH DICTIONARY app.undeclared");
        succeeds(f.d.run(&["verify", "--db", f.target()]));
        succeeds(f.d.run(&["snapshot", "--db", f.target()]));
        // The fixture's declarations were not pulled from this database, so the plan has
        // changes of its own; what is pinned is that its drift gate lets it through.
        let planned = succeeds(f.d.run(&["plan", "--db", f.target()]));
        assert!(
            !said(&planned).contains("has drifted"),
            "{}",
            said(&planned)
        );
        let shown = status(&f);
        assert!(shown.contains("ok") && !shown.contains("drift"), "{shown}");
    });
}

/// A recorded object that is dropped or changed is drift, named the same way by `verify`,
/// `status` and the `plan --db` gate: no checksum sees the kind, and `status` and the gate
/// read clean where `verify` did not (#2846). Putting it back as it was clears all three.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_recorded_text_search_object_that_is_dropped_or_changed_is_drift_everywhere() {
    servers(|server, slug| {
        let f = fixture(server, &format!("search2855-drift-{slug}"));
        on_server(f.target(), MADE);
        pull(&f);
        f.d.commit();
        baseline(&f, "adopt the text search objects");
        no_changes(&succeeds(f.d.run(&["plan", "--db", f.target()])));
        succeeds(f.d.run(&["verify", "--db", f.target()]));
        assert!(!status(&f).contains("drift"), "{}", status(&f));
        // `app.russian` is a dictionary and a configuration and a domain; only the
        // dictionary goes.
        let drifted = |why: &str| {
            let verify = f.d.run(&["verify", "--db", f.target()]);
            assert_eq!(code(&verify), 2, "{why}: {}", said(&verify));
            assert!(said(&verify).contains(why), "{}", said(&verify));
            let shown = status(&f);
            assert!(shown.contains("drift"), "status: {shown}");
            let plan = f.d.run(&["plan", "--db", f.target()]);
            assert_ne!(code(&plan), 0, "{}", said(&plan));
            assert!(
                said(&plan).contains("has drifted from the state recorded"),
                "{}",
                said(&plan)
            );
            assert!(said(&plan).contains(why), "{}", said(&plan));
        };
        on_server(f.target(), "DROP TEXT SEARCH DICTIONARY app.russian");
        drifted("`app.russian` was recorded and is no longer in the database");
        on_server(
            f.target(),
            "CREATE TEXT SEARCH DICTIONARY app.russian (TEMPLATE = pg_catalog.simple, ACCEPT = false)",
        );
        drifted("differs from the definition recorded for it");
        // Back as it was: clean again.
        on_server(
            f.target(),
            "DROP TEXT SEARCH DICTIONARY app.russian; \
             CREATE TEXT SEARCH DICTIONARY app.russian (TEMPLATE = pg_catalog.simple)",
        );
        succeeds(f.d.run(&["verify", "--db", f.target()]));
        assert!(!status(&f).contains("drift"), "{}", status(&f));
        no_changes(&succeeds(f.d.run(&["plan", "--db", f.target()])));
    });
}

/// An apply carries the objects the previous record holds forward, so the record after it
/// still names them: one dropped afterwards is drift, and one the plan left alone is not
/// (#2855, DEC-2855.1).
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_apply_keeps_the_text_search_objects_the_record_names() {
    servers(|server, slug| {
        let f = fixture(server, &format!("search2855-apply-{slug}"));
        on_server(f.target(), MADE);
        pull(&f);
        f.d.commit();
        baseline(&f, "adopt the text search objects");
        // An object made outside the declarations, and a plan that touches neither kind.
        on_server(
            f.target(),
            "CREATE TEXT SEARCH DICTIONARY app.outside (TEMPLATE = pg_catalog.simple)",
        );
        f.revise_row();
        // Not rehearsed: a rehearsal builds the target's state without the text search
        // objects, and `app.articles` names one, so the scratch rejects the table until
        // creating them is planned (#2728 B).
        let planned = f.plan(&["--no-rehearse"]);
        assert_eq!(code(&planned), 0, "{}", said(&planned));
        let applied = f.apply("data-update");
        assert_eq!(code(&applied), 0, "{}", said(&applied));
        succeeds(f.d.run(&["verify", "--db", f.target()]));
        // The one nothing names was not recorded by the apply: taking it away is no drift.
        on_server(f.target(), "DROP TEXT SEARCH DICTIONARY app.outside");
        succeeds(f.d.run(&["verify", "--db", f.target()]));
        // A recorded one was carried: taking it away is.
        on_server(f.target(), "DROP TEXT SEARCH DICTIONARY app.russian");
        let verify = f.d.run(&["verify", "--db", f.target()]);
        assert_eq!(code(&verify), 2, "{}", said(&verify));
        assert!(
            said(&verify).contains("`app.russian` was recorded and is no longer"),
            "{}",
            said(&verify)
        );
    });
}

/// A rehearsal builds the target's managed state on a scratch before the plan is written. A
/// text search object no declaration names is not something it builds, and it no longer
/// stops the run ("the baseline cannot be expressed") (#2855, DEC-2855.1). Unmeasured
/// before this test: whether the run failed on an engine; what is asserted is the outcome.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn an_undeclared_text_search_object_does_not_stop_a_rehearsal() {
    servers(|server, slug| {
        let f = fixture(server, &format!("search2855-rehearsal-{slug}"));
        on_server(
            f.target(),
            "CREATE TEXT SEARCH DICTIONARY app.undeclared (TEMPLATE = pg_catalog.simple)",
        );
        f.add_column("true");
        let planned = f.plan(&[]);
        assert_eq!(code(&planned), 0, "{}", said(&planned));
        assert!(
            stdout(&planned).contains("Rehearsal: converged on scratch"),
            "{}",
            said(&planned)
        );
    });
}

/// A recorded object whose declaration is removed while the database keeps it is no
/// difference to `snapshot`: the database equals the record, as `verify` says. The new
/// record then stops naming it, so dropping it afterwards is no drift. Changed in the
/// database first, it is the difference `snapshot` exists to refuse.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_snapshot_after_a_declaration_is_removed_compares_the_unchanged_database_as_equal() {
    servers(|server, slug| {
        let f = fixture(server, &format!("search2855-undeclare-{slug}"));
        on_server(f.target(), MADE);
        pull(&f);
        f.d.commit();
        baseline(&f, "adopt the text search objects");
        let file = f.d.dir.join("schema/app.russian.textsearch-dictionary.yml");
        std::fs::remove_file(&file).unwrap();
        succeeds(f.d.run(&["verify", "--db", f.target()]));

        // Negative: changed outside pbps, it differs from the record whatever is declared.
        on_server(
            f.target(),
            "DROP TEXT SEARCH DICTIONARY app.russian; \
             CREATE TEXT SEARCH DICTIONARY app.russian (TEMPLATE = pg_catalog.simple, ACCEPT = false)",
        );
        let refused = f.d.run(&["snapshot", "--db", f.target()]);
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        assert!(
            said(&refused).contains("differs from the state recorded"),
            "{}",
            said(&refused)
        );
        on_server(
            f.target(),
            "DROP TEXT SEARCH DICTIONARY app.russian; \
             CREATE TEXT SEARCH DICTIONARY app.russian (TEMPLATE = pg_catalog.simple)",
        );

        succeeds(f.d.run(&["snapshot", "--db", f.target()]));
        // The record no longer names it.
        on_server(f.target(), "DROP TEXT SEARCH DICTIONARY app.russian");
        succeeds(f.d.run(&["verify", "--db", f.target()]));
        succeeds(f.d.run(&["snapshot", "--db", f.target()]));
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

/// A staged plan is one logical change, so a configuration's create is the case a stop can
/// split: `CREATE` and then one `ADD MAPPING` per group. Stopped after the `CREATE`, the
/// checkpoint holds the configuration as the database holds it, and `--resume` finishes the
/// mappings without refusing the database as moved (ready round 5, DEC-2728.2).
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_staged_create_stopped_after_the_configuration_exists_resumes() {
    for server in [server(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()] {
        let own = OwnDatabase::new(&server, "search2728r5");
        let db = own.connection();
        on_server(db, "CREATE SCHEMA app");
        let d = Demo::new("search2728r5");
        d.commit();
        succeeds(d.run(&["baseline", "--db", db, "--reason", "empty database"]));
        declare(&d, "app.words.textsearch-dictionary.yml", WORDS);
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let path = d.dir.join("search.plan.json");
        succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        succeeds(apply_plan(&d, db, &path, false));
        // The configuration is the one logical change of the staged plan.
        declare(&d, "app.docs.textsearch-config.yml", DOCS);
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        succeeds(d.run(&[
            "plan",
            "--db",
            db,
            "--staged",
            "--out",
            path.to_str().unwrap(),
        ]));
        // The first `ADD MAPPING` is refused by the engine, so the run stops after the
        // configuration's `CREATE` checkpoint and not before it.
        on_server(
            db,
            "CREATE SCHEMA witness; \
             CREATE FUNCTION witness.stop_before_the_mappings() RETURNS event_trigger \
             LANGUAGE plpgsql AS $$ BEGIN \
             RAISE EXCEPTION 'stop before the mapping commits'; END $$; \
             CREATE EVENT TRIGGER stop_mapping ON ddl_command_end \
             WHEN TAG IN ('ALTER TEXT SEARCH CONFIGURATION') \
             EXECUTE FUNCTION witness.stop_before_the_mappings()",
        );
        let stopped = approved_apply(&d, db, &path, &["--staged"]);
        assert_ne!(code(&stopped), 0, "{}", said(&stopped));
        let checkpoint = super::latest_snapshot(db);
        let progress = checkpoint
            .staged
            .as_ref()
            .unwrap_or_else(|| panic!("no checkpoint after the stop: {}", said(&stopped)));
        assert_eq!(progress.completed, 1, "{}", said(&stopped));
        // The configuration, which exists, is recorded; the mappings it lacks are not claimed.
        let held: Vec<String> = checkpoint
            .schema
            .search
            .keys()
            .map(|name| name.shown().to_string())
            .collect();
        assert!(
            held.iter().any(|name| name.contains("app.docs")),
            "{held:?}"
        );
        on_server(db, "DROP EVENT TRIGGER stop_mapping");
        succeeds(approved_apply(&d, db, &path, &["--staged", "--resume"]));
        no_changes(&succeeds(d.run(&["plan", "--db", db])));
        succeeds(d.run(&["verify", "--db", db]));
    }
}

/// A declaration that matches an object already on the database adopts it: identity
/// resolution gives it a uid and the plan makes no change for it, so the record has to
/// take the object from the read-back. Recorded without it, the apply left an identity
/// for an object the state did not hold, and `verify` read the live object as drift
/// (ready round 5, DEC-2728.2). An object no declaration names stays out of the record.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_declaration_that_adopts_an_existing_object_is_recorded_and_verifies() {
    for server in [server(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()] {
        let own = OwnDatabase::new(&server, "search2728adopt");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app; \
             CREATE TEXT SEARCH DICTIONARY app.words (TEMPLATE = pg_catalog.snowball, language = english); \
             CREATE TEXT SEARCH CONFIGURATION app.docs (PARSER = pg_catalog.\"default\"); \
             ALTER TEXT SEARCH CONFIGURATION app.docs ADD MAPPING FOR asciiword, word WITH app.words; \
             ALTER TEXT SEARCH CONFIGURATION app.docs ADD MAPPING FOR email WITH pg_catalog.simple; \
             CREATE TEXT SEARCH DICTIONARY app.undeclared (TEMPLATE = pg_catalog.simple)",
        );
        let d = Demo::new("search2728adopt");
        d.commit();
        succeeds(d.run(&["baseline", "--db", db, "--reason", "objects made by hand"]));
        declare(&d, "app.docs.textsearch-config.yml", DOCS);
        declare(&d, "app.words.textsearch-dictionary.yml", WORDS);
        declare(&d, "app.articles.yml", ARTICLES);
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let path = d.dir.join("search.plan.json");
        succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        // The adopted objects are not in the plan: only the table is made.
        let order = subjects(&path);
        assert!(
            !order
                .iter()
                .any(|s| s.contains("dictionary app.words") || s.contains("config app.docs")),
            "{order:?}"
        );
        succeeds(apply_plan(&d, db, &path, false));
        let record = super::latest_snapshot(db);
        let held: Vec<String> = record
            .schema
            .search
            .keys()
            .map(|name| name.shown().to_string())
            .collect();
        // Recorded with their identities, and the object nobody declared is not.
        assert!(held.iter().any(|n| n.contains("app.words")), "{held:?}");
        assert!(held.iter().any(|n| n.contains("app.docs")), "{held:?}");
        assert!(
            !held.iter().any(|n| n.contains("app.undeclared")),
            "{held:?}"
        );
        assert_eq!(record.ids.search.len(), 2, "{:?}", record.ids.search);
        succeeds(d.run(&["verify", "--db", db]));
        no_changes(&succeeds(d.run(&["plan", "--db", db])));
    }
}

/// The mappings a configuration holds in the catalog, in the engine's order.
fn mappings_of(db: &str, config: &str) -> String {
    text_of(
        db,
        &format!(
            "SELECT string_agg(tt.alias || '=' || d.dictname, ',' ORDER BY m.maptokentype, m.mapseqno) \
               FROM pg_ts_config_map m \
               JOIN pg_ts_dict d ON d.oid = m.mapdict \
               JOIN pg_ts_config c ON c.oid = m.mapcfg \
               JOIN ts_token_type(c.cfgparser) tt ON tt.tokid = m.maptokentype \
              WHERE c.cfgname = '{config}'"
        ),
    )
}

fn options_of(db: &str, dictionary: &str) -> String {
    text_of(
        db,
        &format!(
            "SELECT coalesce(dictinitoption, '') FROM pg_ts_dict WHERE dictname = '{dictionary}'"
        ),
    )
}

const SOLO: &str = "text_search_dictionary: app.solo\ntemplate: pg_catalog.simple\n\
                    options:\n  stopwords: english\n";

/// A standing configuration whose token types are added, changed and given up, and a standing
/// dictionary whose options are changed and removed, are altered in place with nothing
/// standing on them: the plan says what it alters, the catalog ends in the declared rows in
/// the declared order, and the next plan is empty and verifies. The same change planned
/// without the database is refused, since only the target can say what stands on the object.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_standing_configuration_and_dictionary_are_altered_in_place_and_the_replan_is_empty() {
    for server in [server(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()] {
        let own = OwnDatabase::new(&server, "search2728c");
        let db = own.connection();
        on_server(db, "CREATE SCHEMA app");
        let d = Demo::new("search2728c");
        d.commit();
        succeeds(d.run(&["baseline", "--db", db, "--reason", "empty database"]));
        declare(&d, "app.words.textsearch-dictionary.yml", WORDS);
        declare(&d, "app.docs.textsearch-config.yml", DOCS);
        declare(&d, "app.solo.textsearch-dictionary.yml", SOLO);
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let path = d.dir.join("alter.plan.json");
        succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        succeeds(apply_plan(&d, db, &path, false));
        assert_eq!(
            mappings_of(db, "docs"),
            "asciiword=words,word=words,email=simple"
        );
        // Token types added (`url`), changed (a second dictionary in a new order, `word`) and
        // given up (`email`); an option given up (`stopwords`) and one written (`accept`).
        declare(
            &d,
            "app.docs.textsearch-config.yml",
            "text_search_config: app.docs\nparser: pg_catalog.default\nmappings:\n  \
             asciiword: [pg_catalog.simple, app.words]\n  url: [pg_catalog.simple]\n  \
             word: [pg_catalog.simple]\n",
        );
        declare(
            &d,
            "app.solo.textsearch-dictionary.yml",
            "text_search_dictionary: app.solo\ntemplate: pg_catalog.simple\noptions:\n  \
             accept: \"false\"\n",
        );
        // Planned without the database, the dependents are unknown: refused, naming the way.
        let offline = d.run(&["plan", "--no-dev"]);
        assert_ne!(code(&offline), 0, "{}", said(&offline));
        assert!(said(&offline).contains("--db"), "{}", said(&offline));
        let out = succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        let text = said(&out);
        assert!(
            text.contains("~ alter text search configuration `app.docs` (mappings: asciiword, email, url, word)")
                && text.contains("~ alter text search dictionary `app.solo` (options: accept, stopwords)"),
            "{text}"
        );
        // Nothing is dropped or created: the identities stay.
        assert_eq!(subjects(&path).len(), 2, "{:?}", subjects(&path));
        succeeds(apply_plan(&d, db, &path, false));
        assert_eq!(
            mappings_of(db, "docs"),
            "asciiword=simple,asciiword=words,word=simple,url=simple"
        );
        assert_eq!(options_of(db, "solo"), "accept = 'false'");
        no_changes(&succeeds(d.run(&["plan", "--db", db])));
        succeeds(d.run(&["verify", "--db", db]));
        // The same files pulled back are the declared ones: the alteration left no residue.
        let before =
            std::fs::read_to_string(d.dir.join("schema/app.docs.textsearch-config.yml")).unwrap();
        succeeds(d.run(&["pull", "--force", "--db", db]));
        let after =
            std::fs::read_to_string(d.dir.join("schema/app.docs.textsearch-config.yml")).unwrap();
        assert_eq!(
            after.split_whitespace().collect::<Vec<_>>(),
            before.split_whitespace().collect::<Vec<_>>()
        );
        d.commit();
        // A dictionary retired with the configuration kept: the configuration stops mapping it
        // first (an alteration, class 7), then the dictionary is dropped (class 18); the drop
        // does not wait for a configuration that stays.
        declare(
            &d,
            "app.docs.textsearch-config.yml",
            "text_search_config: app.docs\nparser: pg_catalog.default\nmappings:\n  \
             asciiword: [pg_catalog.simple]\n  url: [pg_catalog.simple]\n  \
             word: [pg_catalog.simple]\n",
        );
        std::fs::remove_file(d.dir.join("schema/app.words.textsearch-dictionary.yml")).unwrap();
        succeeds(d.run(&["intent", "--drop-search", "dictionary app.words", "retired"]));
        d.commit();
        succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        let order = subjects(&path);
        assert!(
            at(&order, "config app.docs") < at(&order, "dictionary app.words"),
            "{order:?}"
        );
        succeeds(approved_apply(&d, db, &path, &["--allow", "destructive"]));
        assert_eq!(
            mappings_of(db, "docs"),
            "asciiword=simple,word=simple,url=simple"
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*) FROM pg_ts_dict WHERE dictname = 'words' AND dictnamespace = 'app'::regnamespace"
            ),
            0
        );
        no_changes(&succeeds(d.run(&["plan", "--db", db])));
    }
}

/// What would keep the old behaviour stops the change, by name, before any statement: a stored
/// generated column over a configuration stops a change of its mappings and, through the
/// configuration that maps it, a change of a dictionary's options; a parser or a template that
/// differs is refused in place of a drop and create. The refusal names the remedy and the
/// database is as it was. Negative: a dictionary nothing maps, in the same project, is
/// altered in place.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn what_would_keep_the_old_behaviour_stops_an_alteration_and_a_parser_or_template_change_is_refused()
 {
    for server in [server(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()] {
        let own = OwnDatabase::new(&server, "search2728d");
        let db = own.connection();
        on_server(db, "CREATE SCHEMA app");
        let d = Demo::new("search2728d");
        d.commit();
        succeeds(d.run(&["baseline", "--db", db, "--reason", "empty database"]));
        declare(&d, "app.words.textsearch-dictionary.yml", WORDS);
        declare(&d, "app.docs.textsearch-config.yml", DOCS);
        declare(&d, "app.solo.textsearch-dictionary.yml", SOLO);
        declare(&d, "app.articles.yml", ARTICLES);
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let path = d.dir.join("stale.plan.json");
        succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        succeeds(apply_plan(&d, db, &path, false));
        let refused = |what: &str, needles: &[&str]| {
            let out = d.run(&["plan", "--db", db]);
            assert_ne!(code(&out), 0, "{what}: {}", said(&out));
            for needle in needles {
                assert!(
                    said(&out).contains(needle),
                    "{what}: {needle}: {}",
                    said(&out)
                );
            }
        };
        // A mapping changed under a stored generated column.
        let docs =
            std::fs::read_to_string(d.dir.join("schema/app.docs.textsearch-config.yml")).unwrap();
        declare(
            &d,
            "app.docs.textsearch-config.yml",
            &docs.replace(
                "email: [pg_catalog.simple]",
                "email: [app.words, pg_catalog.simple]",
            ),
        );
        refused(
            "a mapping",
            &["app.docs", "default value for app.articles.tsv", "new name"],
        );
        // An option of a dictionary that configuration maps: the dependents are the
        // configuration's.
        declare(&d, "app.docs.textsearch-config.yml", &docs);
        declare(
            &d,
            "app.words.textsearch-dictionary.yml",
            &WORDS.replace("english", "german"),
        );
        refused("an option", &["app.words", "app.articles.tsv", "new name"]);
        // A template that differs, and a parser that is not the built-in one.
        declare(
            &d,
            "app.words.textsearch-dictionary.yml",
            &WORDS.replace("snowball", "simple"),
        );
        refused("a template", &["app.words", "template differs"]);
        declare(&d, "app.words.textsearch-dictionary.yml", WORDS);
        declare(
            &d,
            "app.docs.textsearch-config.yml",
            &docs.replace("pg_catalog.default", "pg_catalog.other"),
        );
        refused("a parser", &["app.docs", "parser"]);
        declare(&d, "app.docs.textsearch-config.yml", &docs);
        no_changes(&succeeds(d.run(&["plan", "--db", db])));
        assert_eq!(
            mappings_of(db, "docs"),
            "asciiword=words,word=words,email=simple"
        );
        assert_eq!(options_of(db, "words"), "language = 'english'");
        // Negative: a dictionary nothing maps has nothing stale on it.
        declare(
            &d,
            "app.solo.textsearch-dictionary.yml",
            &SOLO.replace("english", "german"),
        );
        succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        succeeds(apply_plan(&d, db, &path, false));
        assert_eq!(options_of(db, "solo"), "stopwords = 'german'");
        no_changes(&succeeds(d.run(&["plan", "--db", db])));
    }
}
