//! Recorded conversions (`pbps convert`, #2331), through the binary.
use super::{
    BIN, Demo, OwnDatabase, approved_apply, bootstrapped_demo, code, connected_artifact,
    old_server, on_server, plan_checksum, scalar, server, stderr, stdout, succeeds, try_on_server,
};
use pbps_model::IdsFile;
use std::io::Write as _;
use std::process::{Command, Output, Stdio};

const TEXT: &str = "table: app.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  amount: {type: text}\nprimary_key: {name: pk_t, columns: [id]}\n";
const INTEGER: &str = "table: app.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  amount: {type: integer}\nprimary_key: {name: pk_t, columns: [id]}\n";

fn with_stdin(d: &Demo, args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(BIN)
        .arg("--project")
        .arg(&d.dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn ids_bytes(d: &Demo) -> Vec<u8> {
    std::fs::read(d.dir.join("schema.ids.json")).unwrap()
}

fn ids(d: &Demo) -> IdsFile {
    serde_json::from_slice(&ids_bytes(d)).unwrap()
}

/// The project at the moment a text column becomes an integer: committed
/// as text, declared as integer, nothing recorded yet.
fn text_becoming_integer(name: &str) -> Demo {
    let d = Demo::new(name);
    d.table(TEXT);
    succeeds(d.run(&["plan", "--no-dev"]));
    d.commit();
    d.table(INTEGER);
    d
}

const CONVERT: [&str; 4] = ["convert", "app.t.amount", "--using", "-"];
const FROM_TEXT: [&str; 6] = ["convert", "app.t.amount", "--using", "-", "--from", "text"];

#[test]
fn a_change_that_needs_a_conversion_is_a_finding_with_the_command_and_writes_nothing() {
    let d = text_becoming_integer("convert2331-finding");
    let before = ids_bytes(&d);
    for args in [
        &["plan", "--no-dev"][..],
        &["plan", "--check"],
        &["--no-input", "plan"],
    ] {
        let out = d.run(args);
        assert_eq!(code(&out), 2, "{args:?}: {}{}", stdout(&out), stderr(&out));
        let text = stderr(&out);
        assert!(
            text.contains("pbps convert app.t.amount --using -"),
            "the copy-pastable command: {text}"
        );
        assert!(text.contains("`text` to `integer`"), "{text}");
        assert_eq!(ids_bytes(&d), before, "{args:?} must record nothing");
    }
    let json = d.run(&["plan", "--check", "--format", "json"]);
    assert_eq!(code(&json), 2);
    let value: serde_json::Value = serde_json::from_str(&stdout(&json)).unwrap();
    let finding = value["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "plan.conversion-required")
        .expect("a typed finding");
    assert_eq!(finding["remedy"], "pbps convert app.t.amount --using -");
    assert_eq!(finding["severity"], "error");
}

#[test]
fn a_recorded_conversion_makes_the_plan_and_shows_the_whole_expression() {
    let d = text_becoming_integer("convert2331-plan");
    let out = with_stdin(&d, &CONVERT, b"nullif(trim(amount), '')::integer\n");
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    let recorded = ids(&d);
    let uid = recorded
        .column_uid(&"app.t.amount".parse().unwrap())
        .unwrap();
    let record = &recorded.conversions[uid];
    assert_eq!(
        (&*record.from, &*record.to, &*record.using),
        ("text", "integer", "nullif(trim(amount), '')::integer"),
        "one final line ending is dropped and nothing else"
    );

    let artifact = d.dir.join("plan.json");
    let sql = d.dir.join("plan.sql");
    let planned = d.run(&[
        "plan",
        "--no-dev",
        "--out",
        artifact.to_str().unwrap(),
        "--sql",
        sql.to_str().unwrap(),
    ]);
    assert_eq!(
        code(&planned),
        0,
        "{}{}",
        stdout(&planned),
        stderr(&planned)
    );
    assert!(
        stdout(&planned).contains("type text -> integer USING nullif(trim(amount), '')::integer"),
        "{}",
        stdout(&planned)
    );
    assert!(
        std::fs::read_to_string(&sql).unwrap().contains(
            "ALTER TABLE \"app\".\"t\" ALTER COLUMN \"amount\" TYPE integer USING nullif(trim(amount), '')::integer;"
        ),
        "{}",
        std::fs::read_to_string(&sql).unwrap()
    );
    let explained = succeeds(d.run(&["explain", "--plan", artifact.to_str().unwrap()]));
    assert!(
        stdout(&explained).contains("USING nullif(trim(amount), '')::integer"),
        "{}",
        stdout(&explained)
    );
    assert!(
        stdout(&explained).contains("destructive"),
        "the change asks for the class that asks for a human: {}",
        stdout(&explained)
    );

    // The expression is in the checksum: another expression, another plan.
    let first = plan_checksum(&artifact);
    succeeds(with_stdin(&d, &CONVERT, b"amount::integer"));
    let second_artifact = d.dir.join("plan2.json");
    succeeds(d.run(&[
        "plan",
        "--no-dev",
        "--out",
        second_artifact.to_str().unwrap(),
    ]));
    assert_ne!(first, plan_checksum(&second_artifact));
    let saved = std::fs::read_to_string(&second_artifact).unwrap();
    assert!(
        saved.contains(r#""using": "amount::integer""#)
            || saved.contains(r#""using":"amount::integer""#),
        "{saved}"
    );
}

#[test]
fn recording_is_offline_convergent_and_reviewed_in_git() {
    let d = text_becoming_integer("convert2331-record");
    succeeds(with_stdin(&d, &CONVERT, b"amount::integer\n"));
    // `--using` as an argument records the same text.
    let by_argument = d.run(&["convert", "app.t.amount", "--using", "amount::integer"]);
    assert_eq!(code(&by_argument), 0);
    assert!(
        stdout(&by_argument).contains("already recorded"),
        "{}",
        stdout(&by_argument)
    );
    // And a multi-line expression keeps its inner line breaks.
    succeeds(with_stdin(
        &d,
        &CONVERT,
        b"CASE\n  WHEN amount = '' THEN NULL\n  ELSE amount::integer\nEND\r\n",
    ));
    let record = ids(&d).conversions.into_values().next().unwrap();
    assert_eq!(
        record.using,
        "CASE\n  WHEN amount = '' THEN NULL\n  ELSE amount::integer\nEND"
    );
    // Run again once the change is committed, the base already holds the new
    // type and the same command still changes nothing.
    succeeds(with_stdin(&d, &CONVERT, b"amount::integer"));
    d.commit();
    let recorded = ids_bytes(&d);
    let again = with_stdin(&d, &CONVERT, b"amount::integer");
    assert_eq!(code(&again), 0, "{}", stderr(&again));
    assert!(
        stdout(&again).contains("already recorded"),
        "{}",
        stdout(&again)
    );
    assert_eq!(ids_bytes(&d), recorded, "the same record changes nothing");
}

#[test]
fn a_blank_or_unreadable_expression_is_refused_before_anything_is_read_or_written() {
    let d = text_becoming_integer("convert2331-blank");
    let before = ids_bytes(&d);
    for (args, input) in [
        (&CONVERT[..], &b""[..]),
        (&CONVERT[..], b"\n"),
        (&CONVERT[..], b" \t\r\n"),
        (&CONVERT[..], b"\xff\xfe"),
        (&["convert", "app.t.amount", "--using", ""][..], b""),
        (&["convert", "app.t.amount", "--using", "  "][..], b""),
    ] {
        let out = with_stdin(&d, args, input);
        assert_eq!(code(&out), 2, "{args:?} {input:?}: {}", stderr(&out));
        assert_eq!(ids_bytes(&d), before);
    }
    // Before discovery: a directory that is no project reports the usage error
    // and not a missing project.
    let nowhere =
        std::env::temp_dir().join(format!("pbps-convert2331-nowhere-{}", std::process::id()));
    std::fs::create_dir_all(&nowhere).unwrap();
    let out = Command::new(BIN)
        .arg("--project")
        .arg(&nowhere)
        .args(CONVERT)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&nowhere);
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("blank"), "{}", stderr(&out));
}

#[test]
fn no_flag_on_plan_or_apply_can_supply_the_expression() {
    let d = text_becoming_integer("convert2331-noflag");
    let before = ids_bytes(&d);
    for args in [
        &["plan", "--using", "amount::integer"][..],
        &["plan", "--convert", "app.t.amount=amount::integer"],
        &["plan", "--db", "x", "--using", "amount::integer"],
        &["apply", "--using", "amount::integer"],
    ] {
        let out = d.run(args);
        assert_eq!(code(&out), 2, "{args:?}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("unexpected argument"),
            "{}",
            stderr(&out)
        );
    }
    assert_eq!(ids_bytes(&d), before);
}

#[test]
fn a_conversion_is_pruned_when_the_declared_type_moves_and_never_revived() {
    let d = text_becoming_integer("convert2331-prune");
    succeeds(with_stdin(&d, &CONVERT, b"amount::integer"));
    succeeds(d.run(&["plan", "--no-dev"]));
    assert_eq!(
        ids(&d).conversions.len(),
        1,
        "still the declared type: kept"
    );
    // The declared type moves on: the record is history, and writing the
    // identities drops it.
    d.table(TEXT);
    succeeds(d.run(&["plan", "--no-dev"]));
    assert!(ids(&d).conversions.is_empty(), "{:?}", ids(&d).conversions);
    // Declared as integer again, the old expression is not revived.
    d.table(INTEGER);
    let again = d.run(&["plan", "--no-dev"]);
    assert_eq!(code(&again), 2, "{}", stderr(&again));
    assert!(
        stderr(&again).contains("pbps convert app.t.amount --using -"),
        "{}",
        stderr(&again)
    );
    assert!(ids(&d).conversions.is_empty());
    // `--check` is told the file is out of date when pruning is all it needs.
    let d2 = text_becoming_integer("convert2331-prune-check");
    succeeds(with_stdin(&d2, &CONVERT, b"amount::integer"));
    succeeds(d2.run(&["plan", "--no-dev"]));
    d2.table(TEXT);
    let check = d2.run(&["plan", "--check"]);
    assert_eq!(code(&check), 2, "{}{}", stdout(&check), stderr(&check));
    assert!(
        stderr(&check).contains("identity file is out of date"),
        "{}",
        stderr(&check)
    );
}

#[test]
fn a_record_written_for_another_starting_type_is_refused_by_name() {
    let d = text_becoming_integer("convert2331-from");
    succeeds(with_stdin(
        &d,
        &[
            "convert",
            "app.t.amount",
            "--using",
            "-",
            "--from",
            "varchar(20)",
        ],
        b"amount::integer",
    ));
    assert_eq!(
        ids(&d).conversions.values().next().unwrap().from,
        "character varying(20)"
    );
    let out = d.run(&["plan", "--no-dev"]);
    assert_ne!(code(&out), 0, "{}", stdout(&out));
    let text = stderr(&out);
    assert!(
        text.contains("a conversion is recorded from `character varying(20)` to `integer`, but this column is `text`"),
        "{text}"
    );
    assert!(
        text.contains("pbps convert app.t.amount --using - --from 'text'"),
        "{text}"
    );
    // `--from` the base's own type is what the default does.
    succeeds(with_stdin(
        &d,
        &["convert", "app.t.amount", "--using", "-", "--from", "TEXT"],
        b"amount::integer",
    ));
    succeeds(d.run(&["plan", "--no-dev"]));
}

#[test]
fn a_column_renamed_in_the_same_plan_is_refused_with_a_request_to_split_it() {
    let d = text_becoming_integer("convert2331-rename");
    d.table(&INTEGER.replace("amount", "total"));
    succeeds(d.run(&["rename", "app.t.amount", "total"]));
    let out = with_stdin(
        &d,
        &["convert", "app.t.total", "--using", "-", "--from", "text"],
        b"total::integer",
    );
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    let planned = d.run(&["plan", "--no-dev"]);
    assert_ne!(code(&planned), 0);
    assert!(
        stderr(&planned).contains("this plan also renames it")
            && stderr(&planned).contains("plan of its own"),
        "{}",
        stderr(&planned)
    );
}

#[test]
fn a_dropped_column_takes_its_record_and_an_unknown_column_is_refused() {
    let d = text_becoming_integer("convert2331-drop");
    succeeds(with_stdin(&d, &CONVERT, b"amount::integer"));
    succeeds(d.run(&["plan", "--no-dev"]));
    d.commit();
    d.table("table: app.t\ncolumns:\n  id: {type: bigint, nullable: false}\nprimary_key: {name: pk_t, columns: [id]}\n");
    succeeds(d.run(&["drop", "app.t.amount", "--reason", "retired"]));
    assert!(ids(&d).conversions.is_empty());
    ids_valid(&d);
    let missing = with_stdin(&d, &["convert", "app.t.nope", "--using", "-"], b"x");
    assert_ne!(code(&missing), 0);
    assert!(
        stderr(&missing).contains("no identity records this column"),
        "{}",
        stderr(&missing)
    );
    let same = d.run(&["convert", "app.t.id", "--using", "id"]);
    assert_ne!(code(&same), 0);
    assert!(
        stderr(&same).contains("nothing to convert"),
        "{}",
        stderr(&same)
    );
}

fn ids_valid(d: &Demo) {
    ids(d).validate().unwrap();
}

#[test]
fn convert_is_postgresql_only_and_a_sql_server_project_refuses_a_record() {
    let d = text_becoming_integer("convert2331-mssql");
    succeeds(with_stdin(&d, &CONVERT, b"amount::integer"));
    std::fs::write(d.dir.join("pbps.yml"), "dialect: mssql\n").unwrap();
    let out = with_stdin(&d, &CONVERT, b"amount::integer");
    assert_ne!(code(&out), 0);
    assert!(
        stderr(&out).contains("only supported for PostgreSQL"),
        "{}",
        stderr(&out)
    );
    let planned = d.run(&["plan", "--no-dev"]);
    assert_ne!(code(&planned), 0);
    assert!(
        stderr(&planned).contains("this engine has no `USING` clause"),
        "{}{}",
        stdout(&planned),
        stderr(&planned)
    );
}

#[test]
fn a_zone_dependent_change_needs_a_record_and_takes_one_that_names_the_zone() {
    let d = Demo::new("convert2331-zone");
    d.table("table: app.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  ts: {type: timestamp}\nprimary_key: {name: pk_t, columns: [id]}\n");
    succeeds(d.run(&["plan", "--no-dev"]));
    d.commit();
    d.table("table: app.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  ts: {type: timestamptz}\nprimary_key: {name: pk_t, columns: [id]}\n");
    let refused = d.run(&["plan", "--no-dev"]);
    assert_eq!(code(&refused), 2, "{}", stderr(&refused));
    assert!(stderr(&refused).contains("pbps convert app.t.ts --using -"));
    succeeds(with_stdin(
        &d,
        &["convert", "app.t.ts", "--using", "-"],
        b"ts AT TIME ZONE 'Asia/Taipei'",
    ));
    let sql = d.dir.join("zone.sql");
    succeeds(d.run(&["plan", "--no-dev", "--sql", sql.to_str().unwrap()]));
    assert!(
        std::fs::read_to_string(&sql)
            .unwrap()
            .contains("TYPE timestamp with time zone USING ts AT TIME ZONE 'Asia/Taipei'"),
        "{}",
        std::fs::read_to_string(&sql).unwrap()
    );
}

/// The live half. Every case runs on PostgreSQL 16 and 18.
fn servers() -> [(i32, String); 2] {
    [(16, old_server()), (18, server())]
}

const GATE: &[&str] = &["--allow", "destructive,narrowing"];

/// Asks the engine, at the end, whether the column is still text.
fn still_text(connection: &str) -> bool {
    scalar(
        connection,
        "SELECT count(*) FROM information_schema.columns WHERE table_schema = 'app' \
         AND table_name = 't' AND column_name = 'amount' AND data_type = 'text'",
    ) == 1
}

/// An event trigger that counts the `ALTER TABLE`s on `app.t`: zero proves no
/// statement of the plan ran, because a sequence's calls survive a rollback.
const WITNESS: &str = "CREATE SCHEMA witness; CREATE SEQUENCE witness.executed; \
    CREATE FUNCTION witness.count_ddl() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN \
    IF EXISTS (SELECT FROM pg_event_trigger_ddl_commands() WHERE objid = 'app.t'::regclass) \
    THEN PERFORM nextval('witness.executed'); END IF; END $$; \
    CREATE EVENT TRIGGER count_conversion_ddl ON ddl_command_end WHEN TAG IN ('ALTER TABLE') \
    EXECUTE FUNCTION witness.count_ddl();";

fn executed(connection: &str) -> i64 {
    scalar(
        connection,
        "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM witness.executed",
    )
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_recorded_text_to_integer_conversion_applies_and_the_rows_read_back() {
    for (version, url) in servers() {
        let slug = format!("convert2331-apply{version}");
        let own = OwnDatabase::new(&url, &slug);
        let connection = own.connection();
        let d = bootstrapped_demo(connection, &slug, TEXT);
        on_server(
            connection,
            "INSERT INTO app.t VALUES (1,' 41 '),(2,''),(3,NULL),(4,'7')",
        );
        d.table(INTEGER);
        // Without a record the change keeps its refusal, and names the way out.
        let bare = d.run(&["plan", "--db", connection]);
        assert_ne!(code(&bare), 0, "PG{version}");
        assert!(
            stderr(&bare).contains("cannot be cast automatically")
                && stderr(&bare).contains("pbps convert app.t.amount --using -"),
            "PG{version}: {}",
            stderr(&bare)
        );
        succeeds(with_stdin(
            &d,
            &CONVERT,
            b"nullif(trim(amount), '')::integer",
        ));
        let artifact = connected_artifact(&d, connection, false);
        let denied = approved_apply(&d, connection, &artifact, &[]);
        assert_ne!(
            code(&denied),
            0,
            "PG{version}: the class that asks for a human gates it"
        );
        assert!(stderr(&denied).contains("--allow"), "{}", stderr(&denied));
        assert!(still_text(connection));
        succeeds(approved_apply(&d, connection, &artifact, GATE));
        assert_eq!(
            scalar(connection, "SELECT sum(amount) FROM app.t"),
            48,
            "PG{version}"
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM app.t WHERE amount IS NULL"
            ),
            2
        );
        succeeds(d.run(&["verify", "--db", connection]));
        // Converged: the record is still the declared pair and plans nothing.
        let after = d.dir.join("after.json");
        succeeds(d.run(&["plan", "--db", connection, "--out", after.to_str().unwrap()]));
        let saved: pbps_model::SavedPlan =
            serde_json::from_str(&std::fs::read_to_string(&after).unwrap()).unwrap();
        assert!(
            saved.changes.changes.is_empty(),
            "PG{version}: {:?}",
            saved.changes.changes
        );
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_row_the_expression_cannot_convert_refuses_in_preflight_and_nothing_runs() {
    for (version, url) in servers() {
        let slug = format!("convert2331-bad{version}");
        let own = OwnDatabase::new(&url, &slug);
        let connection = own.connection();
        let d = bootstrapped_demo(connection, &slug, TEXT);
        on_server(connection, "INSERT INTO app.t VALUES (1,'12'),(2,'v1')");
        d.table(INTEGER);
        succeeds(with_stdin(&d, &CONVERT, b"amount::integer"));
        let artifact = connected_artifact(&d, connection, false);
        on_server(connection, WITNESS);
        let refused = approved_apply(&d, connection, &artifact, GATE);
        assert_eq!(
            code(&refused),
            1,
            "PG{version}: {}{}",
            stdout(&refused),
            stderr(&refused)
        );
        let text = stderr(&refused);
        assert!(
            text.contains("the conversion `USING amount::integer` of app.t.amount fails on a stored row (SQLSTATE 22P02)")
                && text.contains("nothing has been changed"),
            "PG{version}: {text}"
        );
        assert!(
            !text.contains("v1"),
            "the server's text quotes the value: {text}"
        );
        assert!(still_text(connection));
        assert_eq!(
            executed(connection),
            0,
            "PG{version}: no statement of the plan ran"
        );
        on_server(connection, "DELETE FROM app.t WHERE id = 2");
        succeeds(approved_apply(&d, connection, &artifact, GATE));
        assert!(!still_text(connection));
        assert!(executed(connection) > 0, "the witness was live");
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_live_column_on_another_start_type_is_refused_by_name_and_converts_nothing() {
    for (version, url) in servers() {
        let slug = format!("convert2331-from{version}");
        let own = OwnDatabase::new(&url, &slug);
        let connection = own.connection();
        let d = bootstrapped_demo(connection, &slug, &TEXT.replace("text", "varchar(20)"));
        d.table(INTEGER);
        succeeds(with_stdin(
            &d,
            &["convert", "app.t.amount", "--using", "-", "--from", "text"],
            b"amount::integer",
        ));
        let refused = d.run(&["plan", "--db", connection]);
        assert_ne!(code(&refused), 0, "PG{version}");
        let text = stderr(&refused);
        assert!(
            text.contains("a conversion is recorded from `text` to `integer`, but this column is `character varying(20)`")
                && text.contains("pbps convert app.t.amount --using - --from 'character varying(20)'"),
            "PG{version}: {text}"
        );
        // The remedy it names records one for this start, and the plan follows.
        succeeds(with_stdin(
            &d,
            &[
                "convert",
                "app.t.amount",
                "--using",
                "-",
                "--from",
                "character varying(20)",
            ],
            b"amount::integer",
        ));
        succeeds(d.run(&["plan", "--db", connection]));
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_timestamp_becomes_timestamptz_only_with_the_zone_written_into_the_record() {
    let before = "table: app.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  ts: {type: timestamp}\nprimary_key: {name: pk_t, columns: [id]}\n";
    for (version, url) in servers() {
        let slug = format!("convert2331-zone{version}");
        let own = OwnDatabase::new(&url, &slug);
        let connection = own.connection();
        let d = bootstrapped_demo(connection, &slug, before);
        on_server(
            connection,
            "INSERT INTO app.t VALUES (1,'2026-03-08 02:30'),(2,'2026-11-01 01:30'),(3,'2026-06-01 12:00')",
        );
        d.table(&before.replace("timestamp}", "timestamptz}"));
        let bare = d.run(&["plan", "--db", connection]);
        assert_ne!(code(&bare), 0);
        assert!(
            stderr(&bare).contains("gains or loses the time zone"),
            "{}",
            stderr(&bare)
        );
        succeeds(with_stdin(
            &d,
            &["convert", "app.t.ts", "--using", "-"],
            b"ts AT TIME ZONE 'America/New_York'",
        ));
        let artifact = connected_artifact(&d, connection, false);
        succeeds(approved_apply(&d, connection, &artifact, GATE));
        // The gap moves forward and the repeated hour takes standard time:
        // the zone written in the record decides, not the session.
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM app.t WHERE (id = 1 AND ts = '2026-03-08 07:30+00') \
                 OR (id = 2 AND ts = '2026-11-01 06:30+00') OR (id = 3 AND ts = '2026-06-01 16:00+00')"
            ),
            3,
            "PG{version}"
        );
        succeeds(d.run(&["verify", "--db", connection]));
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_default_a_unique_key_and_a_changed_check_survive_the_conversion() {
    let before = "table: app.t\ncolumns:\n  id: {type: bigint, nullable: false}\n  amount: {type: text, default: \"'0'\"}\nprimary_key: {name: pk_t, columns: [id]}\nunique:\n  uq_amount: [amount]\nchecks:\n  ck_amount: \"amount <> ''\"\n";
    let after = before
        .replace("type: text", "type: integer")
        .replace("amount <> ''", "amount >= 0");
    for (version, url) in servers() {
        let slug = format!("convert2331-dependents{version}");
        let own = OwnDatabase::new(&url, &slug);
        let connection = own.connection();
        let d = bootstrapped_demo(connection, &slug, before);
        on_server(connection, "INSERT INTO app.t VALUES (1,'5'),(2,'6')");
        d.table(&after);
        succeeds(with_stdin(&d, &CONVERT, b"amount::integer"));
        let artifact = connected_artifact(&d, connection, false);
        succeeds(approved_apply(
            &d,
            connection,
            &artifact,
            &["--allow", "destructive,narrowing,constraint"],
        ));
        succeeds(d.run(&["verify", "--db", connection]));
        // The default, unchanged in text, came back in the new type.
        on_server(connection, "INSERT INTO app.t(id) VALUES (9)");
        assert_eq!(
            scalar(connection, "SELECT amount::bigint FROM app.t WHERE id = 9"),
            0,
            "PG{version}"
        );
        // The unique key stands and still refuses a duplicate.
        assert!(
            try_on_server(connection, "INSERT INTO app.t VALUES (3,5)").is_err(),
            "PG{version}: uq_amount survived"
        );
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn an_expression_is_bound_to_the_column_and_vetted_before_any_row_evaluates_it() {
    // Each expression, and what must be said about it. None may reach the
    // pre-flight's evaluation: a function that raises would then be reported
    // as a failing row (SQLSTATE P0001), which is the order this pins.
    let cases: [(&str, &str); 5] = [
        ("app.boom(amount)", "is not built into PostgreSQL"),
        (
            "(amount || query_to_xml('select 1', true, true, ''))::integer",
            "runs SQL or code named by its arguments",
        ),
        (
            "(amount OPERATOR(app.@@) 1)::integer",
            "is not built into PostgreSQL",
        ),
        (
            "(amount || id)::integer",
            "does not bind against the column alone (SQLSTATE 42703)",
        ),
        (
            "(SELECT 1)",
            "does not bind against the column alone (SQLSTATE 0A000)",
        ),
    ];
    for (version, url) in servers() {
        let slug = format!("convert2331-vet{version}");
        let own = OwnDatabase::new(&url, &slug);
        let connection = own.connection();
        let d = bootstrapped_demo(connection, &slug, TEXT);
        on_server(connection, "INSERT INTO app.t VALUES (1,'5')");
        on_server(
            connection,
            "CREATE FUNCTION app.boom(text) RETURNS integer LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'ran'; END $$; \
             CREATE FUNCTION app.at_at(text, integer) RETURNS text LANGUAGE sql AS $$ SELECT $1 $$; \
             CREATE OPERATOR app.@@ (LEFTARG = text, RIGHTARG = integer, FUNCTION = app.at_at)",
        );
        d.table(INTEGER);
        for (expression, expected) in cases {
            // Each is recorded for the start the database has: the first plan
            // committed the declared type, so the base no longer says it.
            succeeds(with_stdin(&d, &FROM_TEXT, expression.as_bytes()));
            let artifact = connected_artifact(&d, connection, false);
            let refused = approved_apply(&d, connection, &artifact, GATE);
            let text = stderr(&refused);
            assert_eq!(code(&refused), 1, "PG{version} {expression}: {text}");
            assert!(text.contains(expected), "PG{version} {expression}: {text}");
            assert!(
                !text.contains("P0001"),
                "PG{version} {expression}: it ran: {text}"
            );
            assert!(still_text(connection), "PG{version} {expression}");
            assert_eq!(
                scalar(
                    connection,
                    "SELECT count(*) FROM pg_class WHERE relname = 'pbps_conversion'"
                ),
                0,
                "PG{version} {expression}: nothing is left behind"
            );
        }
        // And the same declaration with a built-in expression applies.
        succeeds(with_stdin(&d, &FROM_TEXT, b"amount::integer"));
        let artifact = connected_artifact(&d, connection, false);
        succeeds(approved_apply(&d, connection, &artifact, GATE));
    }
}
