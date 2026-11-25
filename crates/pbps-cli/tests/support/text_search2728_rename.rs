//! Renaming a text search configuration or dictionary through the tool (#2728 PR D,
//! DEC-2728.5). The engine facts the design rests on are asserted here, each on both pinned
//! engines: the rename keeps the object's oid, so what names it as a parsed constant follows
//! it, and what names it as text does not. Each case runs on PostgreSQL 16 and 18.

use super::{
    Demo, OwnDatabase, approved_apply, code, on_server, scalar, server, stderr, stdout, succeeds,
    text_of, try_on_server,
};
use std::process::Output;

fn servers(mut check: impl FnMut(&str)) {
    for server in [server(), std::env::var("PBPS_TEST_PG_OLD_DB").unwrap()] {
        check(&server);
    }
}

fn said(output: &Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

fn declare(d: &Demo, name: &str, text: &str) {
    std::fs::write(d.dir.join("schema").join(name), text).unwrap();
}

fn undeclare(d: &Demo, name: &str) {
    std::fs::remove_file(d.dir.join("schema").join(name)).unwrap();
}

const WORDS: &str = "text_search_dictionary: app.words\ntemplate: pg_catalog.snowball\n\
                     options:\n  language: english\n";

fn docs(name: &str, from: Option<&str>, dictionary: &str) -> String {
    let renamed = from.map_or(String::new(), |from| format!("renamed_from: {from}\n"));
    format!(
        "text_search_config: {name}\n{renamed}parser: pg_catalog.default\nmappings:\n  \
         asciiword: [{dictionary}]\n  email: [pg_catalog.simple]\n  word: [{dictionary}]\n"
    )
}

/// A table whose generated column, check and expression index name the configuration `config`
/// by a string literal: what the engine stores as the object's oid, and renders by its name.
/// Before PostgreSQL 17 a generated column's expression cannot be changed in place (the plan
/// is refused by name, `generation_support`), so an older server has no generated column
/// to rename under.
fn articles(config: &str, db: &str) -> String {
    let generated = if scalar(db, "SELECT current_setting('server_version_num')::int8") >= 170000 {
        format!(
            "  tsv: {{type: tsvector, generated: {{expression: \"to_tsvector('{config}', body)\", \
             stored: true}}}}\n"
        )
    } else {
        String::new()
    };
    format!(
        "table: app.articles\ncolumns:\n  id: {{type: integer, nullable: false}}\n  \
         body: {{type: text}}\n{generated}primary_key: [id]\nchecks:\n  \
         articles_indexable: \"to_tsvector('{config}', body) IS NOT NULL\"\nindexes:\n  \
         articles_by_words:\n    keys:\n      - {{expression: \"to_tsvector('{config}', body)\"}}\n"
    )
}

/// The classes a plan asks to be approved for, from its own `--allow` line.
fn approval(out: &Output) -> Vec<String> {
    let text = stdout(out);
    text.lines()
        .find_map(|line| line.split("--allow ").nth(1))
        .map(|list| list.split(',').map(|c| c.trim().to_owned()).collect())
        .unwrap_or_default()
}

fn allow(classes: &[String]) -> Vec<&str> {
    let mut args = Vec::new();
    for class in classes {
        args.extend(["--allow", class.as_str()]);
    }
    args
}

fn subjects(plan: &std::path::Path) -> Vec<String> {
    let saved: pbps_model::SavedPlan =
        serde_json::from_slice(&std::fs::read(plan).unwrap()).unwrap();
    saved
        .changes
        .changes
        .iter()
        .map(|p| {
            let op = serde_json::to_value(&p.change).unwrap()["op"].clone();
            format!("{} {}", op.as_str().unwrap(), p.change.subject())
        })
        .collect()
}

fn setup(own: &OwnDatabase, slug: &str, table: bool) -> Demo {
    let db = own.connection();
    on_server(db, "CREATE SCHEMA app");
    let d = Demo::new(slug);
    d.commit();
    succeeds(d.run(&["baseline", "--db", db, "--reason", "empty database"]));
    declare(&d, "app.words.textsearch-dictionary.yml", WORDS);
    declare(
        &d,
        "app.docs.textsearch-config.yml",
        &docs("app.docs", None, "app.words"),
    );
    if table {
        declare(&d, "app.articles.yml", &articles("app.docs", db));
    }
    succeeds(d.run(&["plan", "--no-dev"]));
    d.commit();
    let path = d.dir.join("make.plan.json");
    succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
    succeeds(approved_apply(&d, db, &path, &[]));
    d
}

/// A configuration renamed through `renamed_from:` keeps its oid, so a stored generated column,
/// a check and an expression index that name it follow it and read back the new name. The
/// plan is the rename, class `rename`, ahead of the dependents whose declared text now
/// names the new spelling; those are rebuilt under their own classes (a literal that changed
/// spelling is a changed expression to the differ, which does not read through the
/// rename). The next plan is empty and the files pull back as declared. Negatives: the old
/// spelling stops resolving, and a plan that renames and leaves the dependents' text alone
/// leaves a difference the next plan cannot apply, which the engine refuses and rolls back.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_configuration_rename_keeps_its_oid_and_the_dependents_that_name_it_follow() {
    servers(|server| {
        let own = OwnDatabase::new(server, "search2728r1");
        let db = own.connection();
        let d = setup(&own, "search2728r1", true);
        let oid = scalar(
            db,
            "SELECT oid::int8 FROM pg_ts_config WHERE cfgname = 'docs'",
        );
        undeclare(&d, "app.docs.textsearch-config.yml");
        declare(
            &d,
            "app.notes.textsearch-config.yml",
            &docs("app.notes", Some("app.docs"), "app.words"),
        );
        declare(&d, "app.articles.yml", &articles("app.notes", db));
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let path = d.dir.join("rename.plan.json");
        let out = succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        let text = said(&out);
        assert!(
            text.contains("rename config app.docs -> app.notes")
                && text.contains("text that names app.docs is not followed"),
            "{text}"
        );
        let planned = subjects(&path);
        assert_eq!(planned[0], "rename_search config app.docs", "{planned:?}");
        assert_eq!(
            planned
                .iter()
                .filter(|s| s.starts_with("rename_search"))
                .count(),
            1,
            "{planned:?}"
        );
        let classes = approval(&out);
        assert!(classes.iter().any(|c| c == "rename"), "{text}");
        succeeds(approved_apply(&d, db, &path, &allow(&classes)));
        assert_eq!(
            scalar(
                db,
                "SELECT oid::int8 FROM pg_ts_config WHERE cfgname = 'notes'"
            ),
            oid
        );
        for (what, sql) in [
            (
                "the index",
                "SELECT indexdef FROM pg_indexes WHERE indexname = 'articles_by_words'",
            ),
            (
                "the check",
                "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'articles_indexable'",
            ),
        ] {
            let shown = text_of(db, sql);
            assert!(
                shown.contains("app.notes") && !shown.contains("app.docs"),
                "{what}: {shown}"
            );
        }
        if scalar(db, "SELECT current_setting('server_version_num')::int8") >= 170000 {
            let shown = text_of(
                db,
                "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef \
                  WHERE adrelid = 'app.articles'::regclass",
            );
            assert!(shown.contains("app.notes"), "the generated column: {shown}");
        }
        assert!(try_on_server(db, "SELECT to_tsvector('app.docs', 'a')").is_err());
        assert!(try_on_server(db, "SELECT to_tsvector('app.notes', 'a')").is_ok());
        let again = succeeds(d.run(&["plan", "--db", db]));
        assert!(stdout(&again).contains("No changes"), "{}", said(&again));
        succeeds(d.run(&["verify", "--db", db]));
        // Negative: a second rename that leaves the dependents' text at `app.notes`.
        d.commit();
        undeclare(&d, "app.notes.textsearch-config.yml");
        declare(
            &d,
            "app.memo.textsearch-config.yml",
            &docs("app.memo", Some("app.notes"), "app.words"),
        );
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let out = succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        assert_eq!(
            subjects(&path),
            ["rename_search config app.notes"],
            "{}",
            said(&out)
        );
        // The engine would follow it, but the plan holds the table to its approved text, and
        // the generated column, the check and the index would now read the new spelling: the
        // guard rolls the apply back, and nothing moved.
        let applied = approved_apply(&d, db, &path, &allow(&approval(&out)));
        assert_ne!(code(&applied), 0, "{}", said(&applied));
        assert!(said(&applied).contains("rolled back"), "{}", said(&applied));
        assert_eq!(
            scalar(
                db,
                "SELECT oid::int8 FROM pg_ts_config WHERE cfgname = 'notes'"
            ),
            oid
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*) FROM pg_ts_config WHERE cfgname = 'memo'"
            ),
            0
        );
        succeeds(d.run(&["pull", "--force", "--db", db]));
        assert_eq!(
            std::fs::read_to_string(d.dir.join("schema/app.notes.textsearch-config.yml"))
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>(),
            docs("app.notes", None, "app.words")
                .split_whitespace()
                .collect::<Vec<_>>()
        );
    });
}

/// The dictionaries each token type of `config` maps, in order, as the catalog holds them.
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

fn oid_of(db: &str, table: &str, column: &str, name: &str) -> i64 {
    scalar(
        db,
        &format!("SELECT oid::int8 FROM {table} WHERE {column} = '{name}'"),
    )
}

fn plan_to(d: &Demo, db: &str, out: &std::path::Path) -> Output {
    succeeds(d.run(&["plan", "--no-dev"]));
    d.commit();
    succeeds(d.run(&["plan", "--db", db, "--out", out.to_str().unwrap()]))
}

/// A dictionary renamed under the configuration that maps it is one change: the mapping
/// follows by oid and the configuration plans nothing of its own. The pair renamed together
/// is two changes, and a real mapping change beside a rename is planned from the new spelling.
/// Each plan is applied and the next one is empty; the catalog holds the same objects.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_renamed_dictionary_keeps_its_mappings_and_the_pair_renames_in_one_plan() {
    servers(|server| {
        let own = OwnDatabase::new(server, "search2728r2");
        let db = own.connection();
        let d = setup(&own, "search2728r2", false);
        let (dictionary, config) = (
            oid_of(db, "pg_ts_dict", "dictname", "words"),
            oid_of(db, "pg_ts_config", "cfgname", "docs"),
        );
        let path = d.dir.join("rename.plan.json");
        // The dictionary alone: the configuration declares the new spelling, and plans nothing.
        undeclare(&d, "app.words.textsearch-dictionary.yml");
        declare(
            &d,
            "app.terms.textsearch-dictionary.yml",
            &WORDS.replace("app.words", "app.terms").replace(
                "text_search_dictionary: app.terms\n",
                "text_search_dictionary: app.terms\nrenamed_from: app.words\n",
            ),
        );
        declare(
            &d,
            "app.docs.textsearch-config.yml",
            &docs("app.docs", None, "app.terms"),
        );
        let out = plan_to(&d, db, &path);
        assert_eq!(
            subjects(&path),
            ["rename_search dictionary app.words"],
            "{}",
            said(&out)
        );
        succeeds(approved_apply(&d, db, &path, &allow(&approval(&out))));
        assert_eq!(
            mappings_of(db, "docs"),
            "asciiword=terms,word=terms,email=simple"
        );
        assert_eq!(oid_of(db, "pg_ts_dict", "dictname", "terms"), dictionary);
        no_changes(&succeeds(d.run(&["plan", "--db", db])));
        succeeds(d.run(&["verify", "--db", db]));
        d.commit();
        // The pair together, the configuration over the dictionary.
        undeclare(&d, "app.terms.textsearch-dictionary.yml");
        undeclare(&d, "app.docs.textsearch-config.yml");
        declare(
            &d,
            "app.lexicon.textsearch-dictionary.yml",
            "text_search_dictionary: app.lexicon\nrenamed_from: app.terms\n\
             template: pg_catalog.snowball\noptions:\n  language: english\n",
        );
        declare(
            &d,
            "app.notes.textsearch-config.yml",
            &docs("app.notes", Some("app.docs"), "app.lexicon"),
        );
        let out = plan_to(&d, db, &path);
        let planned = subjects(&path);
        assert_eq!(
            {
                let mut sorted = planned.clone();
                sorted.sort();
                sorted
            },
            [
                "rename_search config app.docs",
                "rename_search dictionary app.terms"
            ],
            "{}",
            said(&out)
        );
        succeeds(approved_apply(&d, db, &path, &allow(&approval(&out))));
        assert_eq!(
            mappings_of(db, "notes"),
            "asciiword=lexicon,word=lexicon,email=simple"
        );
        assert_eq!(oid_of(db, "pg_ts_config", "cfgname", "notes"), config);
        assert_eq!(oid_of(db, "pg_ts_dict", "dictname", "lexicon"), dictionary);
        no_changes(&succeeds(d.run(&["plan", "--db", db])));
        d.commit();
        // A mapping that changes beside the dictionary's rename is its own alteration, over
        // the new spelling.
        undeclare(&d, "app.lexicon.textsearch-dictionary.yml");
        declare(
            &d,
            "app.words.textsearch-dictionary.yml",
            &WORDS.replace(
                "text_search_dictionary: app.words\n",
                "text_search_dictionary: app.words\nrenamed_from: app.lexicon\n",
            ),
        );
        declare(
            &d,
            "app.notes.textsearch-config.yml",
            &docs("app.notes", None, "app.words"),
        );
        // The identities are written offline, where a mapping change cannot be planned.
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        declare(
            &d,
            "app.notes.textsearch-config.yml",
            "text_search_config: app.notes\nparser: pg_catalog.default\nmappings:\n  \
             asciiword: [pg_catalog.simple, app.words]\n  email: [pg_catalog.simple]\n  \
             word: [app.words]\n",
        );
        let out = succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        let mut planned = subjects(&path);
        planned.sort();
        assert_eq!(
            planned,
            [
                "alter_search_mapping config app.notes",
                "rename_search dictionary app.lexicon"
            ],
            "{}",
            said(&out)
        );
        succeeds(approved_apply(&d, db, &path, &allow(&approval(&out))));
        assert_eq!(
            mappings_of(db, "notes"),
            "asciiword=simple,asciiword=words,word=words,email=simple"
        );
        assert_eq!(oid_of(db, "pg_ts_dict", "dictname", "words"), dictionary);
        no_changes(&succeeds(d.run(&["plan", "--db", db])));
        succeeds(d.run(&["verify", "--db", db]));
    });
}

fn no_changes(out: &Output) {
    assert_eq!(code(out), 0, "{}", said(out));
    assert!(stdout(out).contains("No changes"), "{}", said(out));
}

const TWO: &str = "text_search_dictionary: app.a\ntemplate: pg_catalog.simple\n";

fn two(name: &str, from: Option<&str>) -> String {
    let renamed = from.map_or(String::new(), |from| format!("renamed_from: {from}\n"));
    TWO.replace(
        "text_search_dictionary: app.a\n",
        &format!("text_search_dictionary: {name}\n{renamed}"),
    )
}

/// What a rename is refused for, by name, before anything runs: a chain (the target is a name
/// the files still declare; a swap of two declared names names nothing that disappeared, so there
/// is nothing to rename and the loop in the recorded identities is the differ's unit test), a name a dropped object holds (the same refusal: the
/// drop and the rename are two plans), a move to another schema, and a rename together with a
/// change. Then the legal plan beside them: a name the rename vacates is taken by a new object
/// in the plan after the rename, whose identity the files already hold, and the engine runs
/// the rename first (the other order fails 42710, measured).
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn a_swap_a_chain_a_move_and_a_change_are_refused_and_a_vacated_name_is_reused_in_order() {
    servers(|server| {
        let own = OwnDatabase::new(server, "search2728r3");
        let db = own.connection();
        on_server(db, "CREATE SCHEMA app; CREATE SCHEMA other");
        let d = Demo::new("search2728r3");
        d.commit();
        succeed_baseline(&d, db);
        declare(&d, "app.a.textsearch-dictionary.yml", &two("app.a", None));
        declare(&d, "app.b.textsearch-dictionary.yml", &two("app.b", None));
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let path = d.dir.join("make.plan.json");
        succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        succeeds(approved_apply(&d, db, &path, &[]));
        let (a, b) = (
            oid_of(db, "pg_ts_dict", "dictname", "a"),
            oid_of(db, "pg_ts_dict", "dictname", "b"),
        );
        d.commit();
        let files = [
            "app.a.textsearch-dictionary.yml",
            "app.b.textsearch-dictionary.yml",
        ];
        let reset = |d: &Demo| {
            declare(d, files[0], &two("app.a", None));
            declare(d, files[1], &two("app.b", None));
            let _ = std::fs::remove_file(d.dir.join("schema/app.c.textsearch-dictionary.yml"));
            let _ = std::fs::remove_file(d.dir.join("schema/other.a.textsearch-dictionary.yml"));
        };
        let said_by_plan = |d: &Demo| {
            let out = d.run(&["plan", "--no-dev"]);
            assert_ne!(code(&out), 0, "{}", said(&out));
            said(&out)
        };
        // A chain: `a` becomes `b` while `b` becomes `c`.
        reset(&d);
        declare(&d, files[0], &two("app.b", Some("app.a")));
        declare(&d, files[1], &two("app.c", Some("app.b")));
        let text = said_by_plan(&d);
        assert!(text.contains("app.b"), "{text}");
        // A name a dropped object holds: `b` goes and `a` becomes `b` in one plan.
        reset(&d);
        undeclare(&d, files[1]);
        declare(&d, files[0], &two("app.b", Some("app.a")));
        let out = d.run(&["intent", "--drop-search", "dictionary app.b", "retired"]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("rename target dictionary app.b already exists"),
            "{}",
            said(&out)
        );
        // Another schema.
        reset(&d);
        undeclare(&d, files[0]);
        declare(
            &d,
            "other.a.textsearch-dictionary.yml",
            &TWO.replace("app.a", "other.a").replace(
                "text_search_dictionary: other.a\n",
                "text_search_dictionary: other.a\nrenamed_from: app.a\n",
            ),
        );
        let text = said_by_plan(&d);
        assert!(
            text.contains("a text search rename keeps its kind and its schema"),
            "{text}"
        );
        // A rename together with a change of the object.
        reset(&d);
        undeclare(&d, files[0]);
        declare(
            &d,
            "app.c.textsearch-dictionary.yml",
            &format!(
                "{}options:\n  accept: \"false\"\n",
                two("app.c", Some("app.a"))
            ),
        );
        let text = said_by_plan(&d);
        assert!(text.contains("also changes"), "{text}");
        // The legal plan: `a` becomes `c` (the files hold the rename), and a new `a` is made.
        reset(&d);
        undeclare(&d, files[0]);
        declare(
            &d,
            "app.c.textsearch-dictionary.yml",
            &two("app.c", Some("app.a")),
        );
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        declare(&d, files[0], &two("app.a", None));
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        let out = succeeds(d.run(&["plan", "--db", db, "--out", path.to_str().unwrap()]));
        let planned = subjects(&path);
        assert_eq!(
            planned,
            [
                "rename_search dictionary app.a",
                "create_search dictionary app.a"
            ],
            "{}",
            said(&out)
        );
        succeeds(approved_apply(&d, db, &path, &allow(&approval(&out))));
        assert_eq!(oid_of(db, "pg_ts_dict", "dictname", "c"), a);
        assert_eq!(oid_of(db, "pg_ts_dict", "dictname", "b"), b);
        assert_ne!(oid_of(db, "pg_ts_dict", "dictname", "a"), a);
        no_changes(&succeeds(d.run(&["plan", "--db", db])));
        succeeds(d.run(&["verify", "--db", db]));
    });
}

fn succeed_baseline(d: &Demo, db: &str) {
    succeeds(d.run(&["baseline", "--db", db, "--reason", "empty database"]));
}

/// The engine facts DEC-2728.5 rests on, run without the tool on both servers: the rename keeps
/// the oid of either kind and takes no lock on a dependent table; only an object of the same
/// kind in the schema occupies a name; what names the object as a parsed constant follows it
/// (a mapping, a view, a materialized view, a `BEGIN ATOMIC` body, a column default) and what
/// names it as text does not (a function body, a thesaurus option, a database setting); a
/// swap needs a third name and a drop must run before a rename into its name.
#[test]
#[ignore = "needs live PostgreSQL 16 and 18"]
fn the_engine_keeps_the_oid_follows_what_it_parsed_and_refuses_an_occupied_name() {
    servers(|server| {
        let own = OwnDatabase::new(server, "search2728r4");
        let db = own.connection();
        on_server(
            db,
            "CREATE SCHEMA app; \
             CREATE TEXT SEARCH DICTIONARY app.words (TEMPLATE = pg_catalog.simple); \
             CREATE TEXT SEARCH CONFIGURATION app.docs (COPY = pg_catalog.english); \
             ALTER TEXT SEARCH CONFIGURATION app.docs ALTER MAPPING FOR asciiword WITH app.words; \
             CREATE TEXT SEARCH DICTIONARY app.thes \
               (TEMPLATE = thesaurus, DICTFILE = thesaurus_sample, DICTIONARY = app.words); \
             CREATE TABLE app.t (body text, cfg regconfig DEFAULT 'app.docs'::regconfig); \
             CREATE VIEW app.v AS SELECT to_tsvector('app.docs'::regconfig, body) AS t FROM app.t; \
             CREATE MATERIALIZED VIEW app.mv AS \
               SELECT to_tsvector('app.docs'::regconfig, body) AS t FROM app.t; \
             CREATE FUNCTION app.f_atomic(x text) RETURNS tsvector LANGUAGE sql \
               BEGIN ATOMIC SELECT to_tsvector('app.docs'::regconfig, x); END; \
             CREATE FUNCTION app.f_text(x text) RETURNS tsvector LANGUAGE sql \
               AS $$ SELECT to_tsvector('app.docs', x) $$",
        );
        on_server(
            db,
            "DO $$ BEGIN EXECUTE format('ALTER DATABASE %I SET default_text_search_config = %L', \
               current_database(), 'app.docs'); END $$",
        );
        let (config, dictionary) = (
            oid_of(db, "pg_ts_config", "cfgname", "docs"),
            oid_of(db, "pg_ts_dict", "dictname", "words"),
        );
        // The rename of each kind, with the locks it holds read from the same transaction.
        on_server(
            db,
            "ALTER TEXT SEARCH CONFIGURATION app.docs RENAME TO notes; \
             ALTER TEXT SEARCH DICTIONARY app.words RENAME TO terms; \
             DO $$ BEGIN \
               IF (SELECT count(*) FROM pg_locks WHERE pid = pg_backend_pid() \
                    AND locktype = 'relation' AND relation = 'app.t'::regclass) > 0 THEN \
                 RAISE EXCEPTION 'the rename locked a dependent table'; END IF; \
             END $$",
        );
        assert_eq!(oid_of(db, "pg_ts_config", "cfgname", "notes"), config);
        assert_eq!(oid_of(db, "pg_ts_dict", "dictname", "terms"), dictionary);
        // What it parsed follows, and reads back the new name.
        for sql in [
            "SELECT pg_get_viewdef('app.v'::regclass)",
            "SELECT pg_get_viewdef('app.mv'::regclass)",
            "SELECT pg_get_functiondef('app.f_atomic'::regproc)",
            "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef WHERE adrelid = 'app.t'::regclass",
        ] {
            let shown = text_of(db, sql);
            assert!(
                shown.contains("app.notes") && !shown.contains("app.docs"),
                "{sql}: {shown}"
            );
        }
        assert_eq!(
            scalar(
                db,
                "SELECT count(*) FROM pg_ts_config_map m JOIN pg_ts_dict d ON d.oid = m.mapdict \
                  WHERE m.mapcfg = (SELECT oid FROM pg_ts_config WHERE cfgname = 'notes') \
                    AND d.dictname = 'terms'"
            ),
            1
        );
        // What it kept as text does not: a function body, a thesaurus's option and a setting.
        let body = try_on_server(db, "SELECT app.f_text('a')").unwrap_err();
        assert!(body.contains("app.docs"), "{body}");
        assert!(
            text_of(
                db,
                "SELECT dictinitoption FROM pg_ts_dict WHERE dictname = 'thes'"
            )
            .contains("app.words")
        );
        assert!(try_on_server(db, "SELECT ts_lexize('app.thes', 'a')").is_err());
        assert_eq!(
            scalar(
                db,
                "SELECT count(*) FROM pg_db_role_setting \
                  WHERE setdatabase = (SELECT oid FROM pg_database WHERE datname = current_database()) \
                    AND setconfig::text LIKE '%app.docs%'"
            ),
            1
        );
        // Only an object of the same kind in the schema holds a name.
        on_server(
            db,
            "CREATE TEXT SEARCH CONFIGURATION app.other (COPY = pg_catalog.simple); \
             CREATE TEXT SEARCH DICTIONARY app.same (TEMPLATE = pg_catalog.simple); \
             CREATE TYPE app.mood AS ENUM ('ok')",
        );
        let taken = try_on_server(
            db,
            "ALTER TEXT SEARCH CONFIGURATION app.other RENAME TO notes",
        )
        .unwrap_err();
        assert!(taken.contains("already exists"), "{taken}");
        for target in ["terms", "mood"] {
            on_server(
                db,
                &format!(
                    "ALTER TEXT SEARCH CONFIGURATION app.other RENAME TO {target}; \
                     ALTER TEXT SEARCH CONFIGURATION app.{target} RENAME TO other"
                ),
            );
        }
        // A direct swap is refused; the third name is what an exchange needs.
        assert!(
            try_on_server(
                db,
                "ALTER TEXT SEARCH CONFIGURATION app.other RENAME TO notes; \
                 ALTER TEXT SEARCH CONFIGURATION app.notes RENAME TO other"
            )
            .is_err()
        );
        // A rename into a dropped object's name needs the drop first.
        on_server(
            db,
            "CREATE TEXT SEARCH CONFIGURATION app.gone (COPY = pg_catalog.simple)",
        );
        assert!(
            try_on_server(
                db,
                "ALTER TEXT SEARCH CONFIGURATION app.other RENAME TO gone; \
                 DROP TEXT SEARCH CONFIGURATION app.gone"
            )
            .is_err()
        );
        on_server(
            db,
            "DROP TEXT SEARCH CONFIGURATION app.gone; \
             ALTER TEXT SEARCH CONFIGURATION app.other RENAME TO gone",
        );
    });
}
