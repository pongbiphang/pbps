//! Text search configurations and dictionaries through the tool (#2728 PR A, DEC-2728.1):
//! pulled into declaration files, pulled again with no difference and an empty follow-up
//! plan, a copied configuration expanded, what is not held left out and named, and every
//! difference refused by name because nothing plans them yet. Each case runs on both pinned
//! engines.

use super::rehearsal2379::{Fixture, Kind, Options};
use super::{code, on_server, stderr, stdout, succeeds};
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
        let out = pull(&f);
        let notes = said(&out);
        let files = search_files(&f);
        let names: Vec<&str> = files.keys().map(String::as_str).collect();
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

/// Nothing plans them yet, so a declaration that differs from the target is refused by
/// name, whichever way it differs; an object only the target has is left alone.
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
        // An object the target lacks: refused as arriving with a later part.
        std::fs::write(
            dir.join("app.fresh.textsearch-dictionary.yml"),
            "text_search_dictionary: app.fresh\ntemplate: pg_catalog.simple\n",
        )
        .unwrap();
        let out = f.d.run(&["plan", "--db", f.target()]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("text search dictionary `app.fresh`")
                && said(&out).contains("creating one arrives"),
            "{}",
            said(&out)
        );
        std::fs::remove_file(dir.join("app.fresh.textsearch-dictionary.yml")).unwrap();
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
        std::fs::remove_file(dir.join("app.copied.textsearch-config.yml")).unwrap();
        no_changes(&succeeds(f.d.run(&["plan", "--db", f.target()])));
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
