//! Table and column comments through the tool (#2729 PR A, DEC-2729.1): a pulled project
//! starts in sync and rebuilds an equal database; one statement per edit; a comment nothing
//! declares is left alone and counted; `description: ""` removes one; the bytes of a comment
//! survive pull, file and apply; a rename keeps a comment and a rename beside an edit plans one
//! statement under the final name; a partition's comment is its own; the closing guard refuses
//! another session's edit of a comment the plan wrote. Each case runs on both pinned engines.
use super::{
    Demo, OwnDatabase, approved_apply, code, connected_artifact, on_server, scalar, server, stderr,
    stdout, succeeds,
};

fn servers(mut check: impl FnMut(&str)) {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        check(&std::env::var(variable).expect(variable));
    }
}

fn said(output: &std::process::Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

/// A table and three columns with comments, a table with none, one that sits beside an
/// undeclared column, and a partitioned parent whose partitions differ.
const SETUP: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.docs (id integer PRIMARY KEY, body text, note text, tag text); \
    COMMENT ON TABLE app.docs IS 'docs table'; \
    COMMENT ON COLUMN app.docs.body IS 'the body'; \
    COMMENT ON COLUMN app.docs.note IS 'the note'; \
    CREATE TABLE app.plain (id integer PRIMARY KEY, label text); \
    CREATE TABLE app.ev (id integer, at integer, PRIMARY KEY (id, at)) PARTITION BY RANGE (at); \
    COMMENT ON TABLE app.ev IS 'parent events'; \
    CREATE TABLE app.ev_1 PARTITION OF app.ev FOR VALUES FROM (0) TO (10); \
    COMMENT ON TABLE app.ev_1 IS 'first partition'; \
    CREATE TABLE app.ev_2 PARTITION OF app.ev FOR VALUES FROM (10) TO (20)";

fn schema_file(d: &Demo, name: &str) -> std::path::PathBuf {
    d.dir.join("schema").join(name)
}

fn read(d: &Demo, name: &str) -> String {
    std::fs::read_to_string(schema_file(d, name)).unwrap()
}

/// How many comments the catalog holds on `object` (a regclass), at any column.
fn comments_on(connection: &str, object: &str) -> i64 {
    scalar(
        connection,
        &format!("SELECT count(*) FROM pg_description WHERE objoid = '{object}'::regclass"),
    )
}

/// Whether `object` (at `subid`, 0 the table) carries exactly `text`.
fn has_comment(connection: &str, object: &str, subid: i32, text: &str) -> bool {
    scalar(
        connection,
        &format!(
            "SELECT count(*) FROM pg_description WHERE objoid = '{object}'::regclass \
             AND objsubid = {subid} AND description = '{text}'"
        ),
    ) == 1
}

/// Pulled, the project is committed, adopted and verifies.
fn adopted(connection: &str, name: &str) -> Demo {
    let d = Demo::new(name);
    succeeds(d.run(&["pull", "--db", connection]));
    d.commit();
    succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
    succeeds(d.run(&["validate"]));
    d
}

/// The plan file of a committed project against `connection`; the offline plan and the commit
/// `connected_artifact` makes would find nothing to commit after [`adopted`].
fn artifact(d: &Demo, connection: &str) -> std::path::PathBuf {
    let path = d.dir.join("rebuild-plan.json");
    succeed_plan(d, connection, &path);
    path
}

fn succeed_plan(d: &Demo, connection: &str, path: &std::path::Path) {
    succeeds(d.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()]));
}

/// `plan --db` to a plan file and its SQL; the SQL text.
fn plan_sql(d: &Demo, connection: &str) -> (std::path::PathBuf, String) {
    let plan = d.dir.join("plan.json");
    let sql = d.dir.join("plan.sql");
    succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--out",
        plan.to_str().unwrap(),
        "--sql",
        sql.to_str().unwrap(),
    ]));
    (plan, std::fs::read_to_string(&sql).unwrap())
}

fn no_changes(d: &Demo, connection: &str) -> String {
    let out = succeeds(d.run(&["plan", "--db", connection]));
    assert!(stdout(&out).contains("No changes"), "{}", said(&out));
    stdout(&out)
}

/// Pulled, a database with comments is a project that plans nothing, and the partitions keep
/// their own; the project rebuilds an empty database to the same comments, and pulled again
/// from there it is the same files (the round trip). Negatives: a table with no comments has no
/// `description`, and a partition without one does not take its parent's.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_pulled_project_is_in_sync_and_rebuilds_the_same_comments() {
    let _ = server();
    servers(|url| {
        let own = OwnDatabase::new(url, "comments-round-trip");
        let connection = own.connection();
        on_server(connection, SETUP);
        let d = adopted(connection, "comments-round-trip");
        let docs = read(&d, "app.docs.yml");
        for held in [
            "description: docs table",
            "description: the body",
            "description: the note",
        ] {
            assert!(docs.contains(held), "the pull writes `{held}`:\n{docs}");
        }
        assert!(!read(&d, "app.plain.yml").contains("description"));
        let ev = read(&d, "app.ev.yml");
        assert!(
            ev.contains("parent events") && ev.contains("first partition"),
            "{ev}"
        );
        assert_eq!(ev.matches("description:").count(), 2, "ev_2 has none: {ev}");
        let plan = no_changes(&d, connection);
        assert!(!plan.contains("not declared"), "{plan}");
        succeeds(d.run(&["verify", "--db", connection]));

        let target = OwnDatabase::new(url, "comments-rebuilt");
        let db = target.connection();
        on_server(db, "CREATE SCHEMA app");
        succeeds(d.run(&["baseline", "--db", db, "--reason", "empty database"]));
        let plan_file = artifact(&d, db);
        succeeds(approved_apply(&d, db, &plan_file, &[]));
        assert!(has_comment(db, "app.docs", 0, "docs table"));
        assert!(has_comment(db, "app.docs", 2, "the body"));
        assert!(has_comment(db, "app.ev", 0, "parent events"));
        assert!(has_comment(db, "app.ev_1", 0, "first partition"));
        assert_eq!(
            comments_on(db, "app.ev_2"),
            0,
            "a partition's comment is its own"
        );
        assert_eq!(comments_on(db, "app.plain"), 0);
        no_changes(&d, db);

        let again = Demo::new("comments-round-trip-again");
        succeeds(again.run(&["pull", "--db", db]));
        for file in ["app.docs.yml", "app.plain.yml", "app.ev.yml"] {
            assert_eq!(read(&again, file), read(&d, file), "{file}");
        }
    });
}

/// One statement per edit; an undeclared comment is left alone, counted once, and not drift;
/// declared, with other text, it is planned; `""` removes a comment and plans nothing where there
/// is none; a rename keeps the comment and plans no statement, and a rename beside an edit plans
/// one under the new name, after the rename.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn each_edit_is_one_statement_and_an_undeclared_comment_is_left_alone() {
    let _ = server();
    servers(|url| {
        let own = OwnDatabase::new(url, "comments-edits");
        let connection = own.connection();
        on_server(connection, SETUP);
        let d = adopted(connection, "comments-edits");
        let docs = schema_file(&d, "app.docs.yml");

        // One edit, one statement; applied, the next plan is empty.
        std::fs::write(
            &docs,
            read(&d, "app.docs.yml").replace("the body", "edited body"),
        )
        .unwrap();
        d.commit();
        let (plan, sql) = plan_sql(&d, connection);
        assert_eq!(sql.matches("COMMENT ON").count(), 1, "{sql}");
        assert!(
            sql.contains("COMMENT ON COLUMN \"app\".\"docs\".\"body\" IS E'edited body';"),
            "{sql}"
        );
        succeeds(approved_apply(&d, connection, &plan, &[]));
        assert!(has_comment(connection, "app.docs", 2, "edited body"));
        no_changes(&d, connection);

        // A comment nobody declared: no change, one line, no drift. Declared with other text, it
        // is planned.
        on_server(
            connection,
            "COMMENT ON COLUMN app.plain.label IS 'ops note'",
        );
        let said_plan = no_changes(&d, connection);
        assert!(
            said_plan.contains(
                "1 comment in the database is not declared; pbps leaves them as they are."
            ),
            "{said_plan}"
        );
        succeeds(d.run(&["verify", "--db", connection]));
        let plain = schema_file(&d, "app.plain.yml");
        let text = read(&d, "app.plain.yml");
        std::fs::write(
            &plain,
            text.replacen("  label:\n", "  label:\n    description: declared\n", 1),
        )
        .unwrap();
        d.commit();
        let (plan, sql) = plan_sql(&d, connection);
        assert_eq!(sql.matches("COMMENT ON").count(), 1, "{sql}");
        succeeds(approved_apply(&d, connection, &plan, &[]));
        assert!(has_comment(connection, "app.plain", 2, "declared"));

        // A pure rename keeps the comment and plans none; renamed and edited, it plans one,
        // after the rename.
        std::fs::write(
            &docs,
            read(&d, "app.docs.yml").replace("  note:\n", "  remark:\n"),
        )
        .unwrap();
        succeeds(d.run(&["rename", "app.docs.note", "remark"]));
        d.commit();
        let (plan, sql) = plan_sql(&d, connection);
        assert!(
            sql.contains("RENAME COLUMN") && !sql.contains("COMMENT ON"),
            "{sql}"
        );
        succeeds(approved_apply(
            &d,
            connection,
            &plan,
            &["--allow", "rename"],
        ));
        assert!(
            has_comment(connection, "app.docs", 3, "the note"),
            "the comment followed the column"
        );
        std::fs::write(
            &docs,
            read(&d, "app.docs.yml")
                .replace("  body:\n", "  memo:\n")
                .replace("edited body", "memo text"),
        )
        .unwrap();
        succeeds(d.run(&["rename", "app.docs.body", "memo"]));
        d.commit();
        let (plan, sql) = plan_sql(&d, connection);
        assert_eq!(sql.matches("COMMENT ON").count(), 1, "{sql}");
        assert!(
            sql.find("RENAME COLUMN").unwrap() < sql.find("COMMENT ON").unwrap()
                && sql.contains("\"memo\" IS E'memo text'"),
            "{sql}"
        );
        succeeds(approved_apply(
            &d,
            connection,
            &plan,
            &["--allow", "rename"],
        ));
        assert!(has_comment(connection, "app.docs", 2, "memo text"));
        no_changes(&d, connection);

        // `""` removes; where there is no comment it plans nothing.
        std::fs::write(
            &docs,
            read(&d, "app.docs.yml")
                .replace("description: the note", "description: \"\"")
                .replacen("  tag:\n", "  tag:\n    description: \"\"\n", 1),
        )
        .unwrap();
        d.commit();
        let (plan, sql) = plan_sql(&d, connection);
        assert_eq!(sql.matches("COMMENT ON").count(), 1, "{sql}");
        assert!(sql.contains("IS NULL;"), "{sql}");
        succeeds(approved_apply(&d, connection, &plan, &[]));
        assert!(!has_comment(connection, "app.docs", 3, "the note"));
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_description WHERE objoid = 'app.docs'::regclass AND objsubid = 4"
            ),
            0
        );
        no_changes(&d, connection);
    });
}

/// The bytes of a comment survive the pull, the file and the apply: quotes, a backslash, CRLF,
/// tabs, edge spaces, Unicode, YAML-looking lines, a trailing newline and 100 KB. A database
/// rebuilt from the pulled files holds the same bytes (compared by length and hash).
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_comments_bytes_survive_the_pull_the_file_and_the_apply() {
    let _ = server();
    // The expressions that build each text, as the server evaluates them.
    let texts: [(&str, &str); 8] = [
        ("t1", r#"E'it''s a "quote" and a \\ back\\slash'"#),
        ("t2", r"E'line one\r\nline two\r\n'"),
        ("t3", r"E'tab\there\t'"),
        ("t4", "E'  leading and trailing  '"),
        ("t5", r"E'中文 \U0001F600 e\u0301'"),
        (
            "t6",
            r"E'yes\nnull\n- a list item\nkey: value\n# not a comment\n---\n'",
        ),
        ("t7", r"repeat(E'line of text that is long enough\n', 3200)"),
        ("t8", r"E'trailing newline\n'"),
    ];
    let fingerprint = |connection: &str, table: &str, subid: i32| -> (i64, i64) {
        let of = |expression: &str| {
            scalar(
                connection,
                &format!(
                    "SELECT {expression} FROM pg_description WHERE objoid = 'app.{table}'::regclass \
                     AND objsubid = {subid}"
                ),
            )
        };
        (
            of("length(description)::bigint"),
            of("('x' || substr(md5(description), 1, 15))::bit(60)::bigint"),
        )
    };
    servers(|url| {
        let own = OwnDatabase::new(url, "comments-bytes");
        let connection = own.connection();
        on_server(connection, "CREATE SCHEMA app");
        for (table, expression) in texts {
            on_server(
                connection,
                &format!(
                    "CREATE TABLE app.{table} (id integer PRIMARY KEY, v text); \
                     DO $do$ BEGIN \
                       EXECUTE format('COMMENT ON TABLE app.{table} IS %L', {expression}); \
                       EXECUTE format('COMMENT ON COLUMN app.{table}.v IS %L', {expression}); \
                     END $do$"
                ),
            );
        }
        let d = adopted(connection, "comments-bytes");
        let target = OwnDatabase::new(url, "comments-bytes-rebuilt");
        let db = target.connection();
        on_server(db, "CREATE SCHEMA app");
        succeeds(d.run(&["baseline", "--db", db, "--reason", "empty database"]));
        let plan_file = artifact(&d, db);
        succeeds(approved_apply(&d, db, &plan_file, &[]));
        for (table, _) in texts {
            for subid in [0, 2] {
                assert_eq!(
                    fingerprint(db, table, subid),
                    fingerprint(connection, table, subid),
                    "{table} at {subid}"
                );
            }
        }
        assert!(fingerprint(db, "t7", 0).0 >= 100_000);
        no_changes(&d, db);
    });
}

/// A new table and a new column get their declared comments after they are created; the plan
/// is the create plus one statement per comment. Negative: a column the plan adds without a
/// `description` gets none.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_created_table_and_an_added_column_get_their_comments() {
    let _ = server();
    servers(|url| {
        let own = OwnDatabase::new(url, "comments-created");
        let connection = own.connection();
        on_server(connection, SETUP);
        let d = adopted(connection, "comments-created");
        std::fs::write(
            schema_file(&d, "app.fresh.yml"),
            "table: app.fresh\ndescription: fresh table\ncolumns:\n  id:\n    type: integer\n    \
             nullable: false\n    description: fresh id\n  v:\n    type: text\nprimary_key: [id]\n",
        )
        .unwrap();
        let docs = read(&d, "app.docs.yml");
        std::fs::write(
            schema_file(&d, "app.docs.yml"),
            docs.replacen(
                "columns:\n",
                "columns:\n  added:\n    type: text\n    description: added later\n  bare:\n    type: text\n",
                1,
            ),
        )
        .unwrap();
        let path = connected_artifact(&d, connection, false);
        succeeds(approved_apply(&d, connection, &path, &[]));
        assert!(has_comment(connection, "app.fresh", 0, "fresh table"));
        assert!(has_comment(connection, "app.fresh", 1, "fresh id"));
        assert_eq!(comments_on(connection, "app.fresh"), 2);
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_description d JOIN pg_attribute a \
                 ON a.attrelid = d.objoid AND a.attnum = d.objsubid \
                 WHERE d.objoid = 'app.docs'::regclass AND a.attname = 'added' \
                 AND d.description = 'added later'"
            ),
            1
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_description d JOIN pg_attribute a \
                 ON a.attrelid = d.objoid AND a.attnum = d.objsubid \
                 WHERE d.objoid = 'app.docs'::regclass AND a.attname = 'bare'"
            ),
            0
        );
        no_changes(&d, connection);
    });
}

/// A comment the plan sets is held to the apply's close: another writer edits it after the plan
/// wrote it, and the apply says so (the event trigger stands in for the other session, once).
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn another_sessions_edit_of_a_comment_the_plan_wrote_is_refused() {
    let _ = server();
    servers(|url| {
        let own = OwnDatabase::new(url, "comments-guard");
        let connection = own.connection();
        on_server(connection, SETUP);
        let d = adopted(connection, "comments-guard");
        let docs = schema_file(&d, "app.docs.yml");
        std::fs::write(
            &docs,
            read(&d, "app.docs.yml").replace("docs table", "planned text"),
        )
        .unwrap();
        d.commit();
        // The apply's statements run in a transaction the plan owns; a trigger stands in for the
        // other session by editing the comment as the first statement of the apply runs.
        let (plan, _) = plan_sql(&d, connection);
        on_server(
            connection,
            "CREATE FUNCTION public.pbps_concurrent_comment() RETURNS event_trigger LANGUAGE plpgsql AS \
             $$ BEGIN IF coalesce(current_setting('pbps.once', true), '') = '' THEN \
               PERFORM set_config('pbps.once', '1', true); \
               EXECUTE 'COMMENT ON TABLE app.docs IS ''someone else'''; END IF; END $$; \
             CREATE EVENT TRIGGER pbps_concurrent_comment ON ddl_command_end \
             WHEN TAG IN ('COMMENT') EXECUTE FUNCTION public.pbps_concurrent_comment()",
        );
        let refused = approved_apply(&d, connection, &plan, &[]);
        on_server(
            connection,
            "DROP EVENT TRIGGER pbps_concurrent_comment; DROP FUNCTION public.pbps_concurrent_comment()",
        );
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        assert!(said(&refused).contains("comment"), "{}", said(&refused));
    });
}
