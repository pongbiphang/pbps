//! A type change beside a foreign key, through the binary (#2359, DEC-2359.1).
//!
//! The key comes down around a change that leaves the types a key can join,
//! recorded conversion or ordinary retype alike, and goes back after the last
//! of them; a key the plan would leave between two types the engine cannot
//! join is refused by `plan` by name. Every case runs on PostgreSQL 16 and 18.
use super::{
    BIN, Demo, OwnDatabase, approved_apply, code, connected_artifact, old_server, on_server,
    scalar, server, stderr, stdout, succeeds,
};
use std::io::Write as _;
use std::process::{Command, Output, Stdio};

const PARENT: &str = "table: app.p\ncolumns:\n  id: {type: text, nullable: false}\nprimary_key: {name: pk_p, columns: [id]}\n";
const CHILD: &str = "table: app.c\ncolumns:\n  id: {type: bigint, nullable: false}\n  pid: {type: text}\nprimary_key: {name: pk_c, columns: [id]}\nforeign_keys:\n  fk_c_p:\n    columns: [pid]\n    references: app.p(id)\n";

const GATE: &[&str] = &["--allow", "destructive,narrowing,constraint"];

fn servers() -> [(i32, String); 2] {
    [(16, old_server()), (18, server())]
}

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

fn convert(d: &Demo, column: &str, using: &str) {
    succeeds(with_stdin(
        d,
        &["convert", column, "--using", "-"],
        using.as_bytes(),
    ));
}

fn put(d: &Demo, file: &str, body: &str) {
    std::fs::write(d.dir.join("schema").join(file), body).unwrap();
}

/// Two tables, `app.p` and the `app.c` that references it, created and
/// committed with the given declarations and bootstrapped into `connection`.
fn bootstrapped_pair(connection: &str, slug: &str, parent: &str, child: &str) -> Demo {
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new(slug);
    put(&d, "app.p.yml", parent);
    put(&d, "app.c.yml", child);
    succeeds(d.run(&["plan"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    d
}

/// The declarations with a column's type changed. The two files hold one
/// `text` column each (the key and its referencing column), so the first
/// match is the column.
fn declared_as(body: &str, ty: &str) -> String {
    body.replacen("type: text", &format!("type: {ty}"), 1)
}

fn column_type(connection: &str, table: &str, column: &str) -> i64 {
    scalar(
        connection,
        &format!(
            "SELECT count(*) FROM information_schema.columns WHERE table_schema = 'app' \
             AND table_name = '{table}' AND column_name = '{column}' AND data_type = 'text'"
        ),
    )
}

/// Both columns are `text` again, as they were.
fn both_still_text(connection: &str) -> bool {
    column_type(connection, "p", "id") == 1 && column_type(connection, "c", "pid") == 1
}

/// The key stands, validated, as one constraint.
fn key_stands_validated(connection: &str) -> bool {
    scalar(
        connection,
        "SELECT count(*) FROM pg_constraint WHERE conname = 'fk_c_p' AND contype = 'f' \
         AND convalidated",
    ) == 1
}

fn key_oid(connection: &str) -> i64 {
    scalar(
        connection,
        "SELECT oid::bigint FROM pg_constraint WHERE conname = 'fk_c_p'",
    )
}

/// An event trigger that counts every `ALTER TABLE` on the two tables: zero
/// proves no statement of the plan ran, because a sequence's calls survive a
/// rollback.
const WITNESS: &str = "CREATE SCHEMA witness; CREATE SEQUENCE witness.executed; \
    CREATE FUNCTION witness.count_ddl() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN \
    IF EXISTS (SELECT FROM pg_event_trigger_ddl_commands() \
    WHERE objid IN ('app.p'::regclass, 'app.c'::regclass)) \
    THEN PERFORM nextval('witness.executed'); END IF; END $$; \
    CREATE EVENT TRIGGER count_key_ddl ON ddl_command_end WHEN TAG IN ('ALTER TABLE') \
    EXECUTE FUNCTION witness.count_ddl();";

fn executed(connection: &str) -> i64 {
    scalar(
        connection,
        "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM witness.executed",
    )
}

/// The plan, as a person reads it: the key is down before and up after.
fn shows_the_key_rebuilt(plan: &str) -> bool {
    plan.contains("- foreign key fk_c_p") && plan.contains("+ foreign key fk_c_p")
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_recorded_conversion_on_both_ends_of_a_key_applies_with_the_key_validated() {
    for (version, url) in servers() {
        let slug = format!("convert2359-both{version}");
        let own = OwnDatabase::new(&url, &slug);
        let connection = own.connection();
        let d = bootstrapped_pair(connection, &slug, PARENT, CHILD);
        on_server(
            connection,
            "INSERT INTO app.p VALUES ('1'),('02'),('3'); \
             INSERT INTO app.c VALUES (1,'1'),(2,'02'),(3,NULL)",
        );
        put(&d, "app.p.yml", &declared_as(PARENT, "integer"));
        put(&d, "app.c.yml", &declared_as(CHILD, "integer"));
        convert(&d, "app.p.id", "id::integer + 100");
        convert(&d, "app.c.pid", "pid::integer + 100");
        let planned = d.run(&["plan", "--db", connection]);
        assert!(
            shows_the_key_rebuilt(&stdout(&planned)),
            "PG{version}: {}{}",
            stdout(&planned),
            stderr(&planned)
        );
        let artifact = connected_artifact(&d, connection, false);
        succeeds(approved_apply(&d, connection, &artifact, GATE));
        // The rows read back converted, and the key holds them.
        assert_eq!(
            scalar(connection, "SELECT sum(id)::bigint FROM app.p"),
            306,
            "PG{version}"
        );
        assert_eq!(
            scalar(connection, "SELECT sum(pid)::bigint FROM app.c"),
            203,
            "PG{version}"
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM information_schema.columns WHERE table_schema = 'app' \
                 AND column_name IN ('id', 'pid') AND data_type = 'integer' \
                 AND table_name IN ('p', 'c')"
            ),
            2,
            "PG{version}: both columns are integer now"
        );
        assert!(key_stands_validated(connection), "PG{version}");
        succeeds(d.run(&["verify", "--db", connection]));
        // The key refuses an orphan, so it is the key and not a stand-in.
        assert!(
            super::try_on_server(connection, "INSERT INTO app.c VALUES (9, 99)").is_err(),
            "PG{version}"
        );
        // Converged: nothing is left to plan.
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
fn an_ordinary_retype_across_the_types_a_key_joins_takes_the_key_down_the_same_way() {
    // The same path with no record: `integer` to `text` on both ends, which
    // the engine refuses on the first column while the key stands between
    // the new type and the old (measured on 16 and 18).
    let (parent, child) = (
        declared_as(PARENT, "integer"),
        declared_as(CHILD, "integer"),
    );
    for (version, url) in servers() {
        let slug = format!("convert2359-ordinary{version}");
        let own = OwnDatabase::new(&url, &slug);
        let connection = own.connection();
        let d = bootstrapped_pair(connection, &slug, &parent, &child);
        on_server(
            connection,
            "INSERT INTO app.p VALUES (1),(2); INSERT INTO app.c VALUES (1,1),(2,2),(3,NULL)",
        );
        put(&d, "app.p.yml", PARENT);
        put(&d, "app.c.yml", CHILD);
        let planned = d.run(&["plan", "--db", connection]);
        assert!(
            shows_the_key_rebuilt(&stdout(&planned)),
            "PG{version}: {}{}",
            stdout(&planned),
            stderr(&planned)
        );
        let artifact = connected_artifact(&d, connection, false);
        succeeds(approved_apply(&d, connection, &artifact, GATE));
        assert!(both_still_text(connection), "PG{version}: both are text");
        assert_eq!(
            scalar(connection, "SELECT sum(pid::integer) FROM app.c"),
            3,
            "PG{version}"
        );
        assert!(key_stands_validated(connection), "PG{version}");
        succeeds(d.run(&["verify", "--db", connection]));
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_converted_value_that_breaks_either_end_is_refused_before_any_statement() {
    for (version, url) in servers() {
        for (side, parent, child) in [
            ("child", "id::integer", "pid::integer + 100"),
            ("parent", "id::integer + 100", "pid::integer"),
        ] {
            let slug = format!("convert2394-{side}{version}");
            let own = OwnDatabase::new(&url, &slug);
            let connection = own.connection();
            let d = bootstrapped_pair(connection, &slug, PARENT, CHILD);
            on_server(
                connection,
                "INSERT INTO app.p VALUES ('1'),('2'); INSERT INTO app.c VALUES (1,'1'),(2,'2')",
            );
            put(&d, "app.p.yml", &declared_as(PARENT, "integer"));
            put(&d, "app.c.yml", &declared_as(CHILD, "integer"));
            convert(&d, "app.p.id", parent);
            convert(&d, "app.c.pid", child);
            let artifact = connected_artifact(&d, connection, false);
            let key_before = key_oid(connection);
            on_server(connection, WITNESS);
            let refused = approved_apply(&d, connection, &artifact, GATE);
            let text = stderr(&refused);
            assert_eq!(code(&refused), 1, "PG{version} {side}: {text}");
            assert!(
                text.contains("nothing has been changed")
                    && text.contains("fk_c_p")
                    && text.contains("2 rows"),
                "PG{version} {side}: {text}"
            );
            // The nontransactional counter distinguishes early refusal from
            // a constraint error that rolls an executed ALTER back.
            assert_eq!(executed(connection), 0, "PG{version} {side}: {text}");
            assert!(both_still_text(connection), "PG{version} {side}");
            assert!(key_stands_validated(connection), "PG{version} {side}");
            assert_eq!(key_oid(connection), key_before, "PG{version} {side}");
            assert_eq!(
                scalar(connection, "SELECT sum(pid::integer) FROM app.c"),
                3,
                "PG{version} {side}: the rows are as they were"
            );
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_key_left_between_types_that_do_not_meet_is_refused_by_plan_before_any_statement() {
    // (the end retyped, the end left behind, the conversion to record)
    let cases = [
        ("app.c.pid", "app.p.id", "pid::integer"),
        ("app.p.id", "app.c.pid", "id::integer"),
    ];
    for (version, url) in servers() {
        for (k, (column, other, using)) in cases.iter().enumerate() {
            let slug = format!("convert2359-oneside{version}-{k}");
            let own = OwnDatabase::new(&url, &slug);
            let connection = own.connection();
            let d = bootstrapped_pair(connection, &slug, PARENT, CHILD);
            on_server(
                connection,
                "INSERT INTO app.p VALUES ('1'); INSERT INTO app.c VALUES (1,'1')",
            );
            on_server(connection, WITNESS);
            if *column == "app.c.pid" {
                put(&d, "app.c.yml", &declared_as(CHILD, "integer"));
            } else {
                put(&d, "app.p.yml", &declared_as(PARENT, "integer"));
            }
            convert(&d, column, using);
            // Offline and against the database: the same refusal, by name.
            for args in [vec!["plan"], vec!["plan", "--db", connection]] {
                let refused = d.run(&args);
                let text = format!("{}{}", stdout(&refused), stderr(&refused));
                assert_ne!(code(&refused), 0, "PG{version} {args:?}: {text}");
                for needed in [
                    "fk_c_p",
                    "app.c.pid",
                    "app.p.id",
                    "`integer`",
                    "`text`",
                    &format!("Change {other} to a type a key can join"),
                    &format!("pbps convert {other} --using -"),
                    &format!("declare {column} with the type it has today"),
                ] {
                    assert!(
                        text.contains(needed),
                        "PG{version} {args:?}: {needed} in {text}"
                    );
                }
            }
            assert_eq!(executed(connection), 0, "PG{version}: no statement ran");
            assert!(both_still_text(connection), "PG{version}");
        }
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_conversion_beside_a_key_on_another_column_leaves_the_key_standing() {
    // The negative: a conversion on a column with no key over it plans
    // nothing for the key, and the same constraint stands after the apply.
    let child = "table: app.c\ncolumns:\n  id: {type: bigint, nullable: false}\n  pid: {type: bigint}\n  amount: {type: text}\nprimary_key: {name: pk_c, columns: [id]}\nforeign_keys:\n  fk_c_p:\n    columns: [pid]\n    references: app.p(id)\n";
    let parent = "table: app.p\ncolumns:\n  id: {type: bigint, nullable: false}\nprimary_key: {name: pk_p, columns: [id]}\n";
    for (version, url) in servers() {
        let slug = format!("convert2359-other{version}");
        let own = OwnDatabase::new(&url, &slug);
        let connection = own.connection();
        let d = bootstrapped_pair(connection, &slug, parent, child);
        on_server(
            connection,
            "INSERT INTO app.p VALUES (1); INSERT INTO app.c VALUES (1,1,'12')",
        );
        let key_before = key_oid(connection);
        put(
            &d,
            "app.c.yml",
            &child.replace("amount: {type: text}", "amount: {type: integer}"),
        );
        succeeds(with_stdin(
            &d,
            &["convert", "app.c.amount", "--using", "-"],
            b"amount::integer",
        ));
        let planned = d.run(&["plan", "--db", connection]);
        assert!(
            !stdout(&planned).contains("foreign key"),
            "PG{version}: {}",
            stdout(&planned)
        );
        let artifact = connected_artifact(&d, connection, false);
        succeeds(approved_apply(
            &d,
            connection,
            &artifact,
            &["--allow", "destructive,narrowing"],
        ));
        assert_eq!(
            scalar(connection, "SELECT sum(amount) FROM app.c"),
            12,
            "PG{version}"
        );
        assert_eq!(key_oid(connection), key_before, "PG{version}: the same key");
        assert!(key_stands_validated(connection), "PG{version}");
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_change_inside_the_types_a_key_joins_leaves_the_key_to_the_engine() {
    // `integer` to `bigint` on both ends: the engine rebuilds the key and
    // checks it itself, as it always did, and the plan neither names the key
    // nor needs the `constraint` allowance for it.
    let parent = "table: app.p\ncolumns:\n  id: {type: integer, nullable: false}\nprimary_key: {name: pk_p, columns: [id]}\n";
    let child = "table: app.c\ncolumns:\n  id: {type: bigint, nullable: false}\n  pid: {type: integer}\nprimary_key: {name: pk_c, columns: [id]}\nforeign_keys:\n  fk_c_p:\n    columns: [pid]\n    references: app.p(id)\n";
    for (version, url) in servers() {
        let slug = format!("convert2359-group{version}");
        let own = OwnDatabase::new(&url, &slug);
        let connection = own.connection();
        let d = bootstrapped_pair(connection, &slug, parent, child);
        on_server(
            connection,
            "INSERT INTO app.p VALUES (1),(2); INSERT INTO app.c VALUES (1,1),(2,2)",
        );
        put(&d, "app.p.yml", &parent.replace("integer", "bigint"));
        put(
            &d,
            "app.c.yml",
            &child.replace("pid: {type: integer}", "pid: {type: bigint}"),
        );
        let planned = d.run(&["plan", "--db", connection]);
        assert!(
            !stdout(&planned).contains("foreign key"),
            "PG{version}: {}",
            stdout(&planned)
        );
        let artifact = connected_artifact(&d, connection, false);
        succeeds(approved_apply(&d, connection, &artifact, &[]));
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM information_schema.columns WHERE table_schema = 'app' \
                 AND column_name IN ('id', 'pid') AND table_name IN ('p', 'c') \
                 AND data_type = 'bigint'"
            ),
            3,
            "PG{version}: p.id, c.id and c.pid"
        );
        assert!(key_stands_validated(connection), "PG{version}");
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_key_from_a_table_to_itself_is_rebuilt_around_the_conversion() {
    let tree = "table: app.n\ncolumns:\n  id: {type: text, nullable: false}\n  up: {type: text}\nprimary_key: {name: pk_n, columns: [id]}\nforeign_keys:\n  fk_n_n:\n    columns: [up]\n    references: app.n(id)\n";
    for (version, url) in servers() {
        let slug = format!("convert2359-self{version}");
        let own = OwnDatabase::new(&url, &slug);
        let connection = own.connection();
        on_server(connection, "CREATE SCHEMA app");
        let d = Demo::new(&slug);
        put(&d, "app.n.yml", tree);
        succeeds(d.run(&["plan"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", connection]));
        on_server(
            connection,
            "INSERT INTO app.n VALUES ('1', NULL), ('02', '1'), ('3', '02')",
        );
        put(
            &d,
            "app.n.yml",
            &tree.replace("type: text", "type: integer"),
        );
        convert(&d, "app.n.id", "id::integer");
        convert(&d, "app.n.up", "up::integer");
        let artifact = connected_artifact(&d, connection, false);
        succeeds(approved_apply(&d, connection, &artifact, GATE));
        assert_eq!(
            scalar(connection, "SELECT sum(up)::bigint FROM app.n"),
            3,
            "PG{version}"
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_constraint WHERE conname = 'fk_n_n' AND convalidated"
            ),
            1,
            "PG{version}"
        );
        succeeds(d.run(&["verify", "--db", connection]));
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_default_and_a_key_on_one_converted_column_are_both_taken_down_and_put_back() {
    // The conversion's own steps (the old default removed, the new one set)
    // and the key's meet on one column; both orders hold at once.
    let child = CHILD.replace("pid: {type: text}", "pid: {type: text, default: \"'1'\"}");
    for (version, url) in servers() {
        let slug = format!("convert2359-default{version}");
        let own = OwnDatabase::new(&url, &slug);
        let connection = own.connection();
        let d = bootstrapped_pair(connection, &slug, PARENT, &child);
        on_server(
            connection,
            "INSERT INTO app.p VALUES ('1'),('02'); INSERT INTO app.c VALUES (1,'1'),(2,'02')",
        );
        put(&d, "app.p.yml", &declared_as(PARENT, "integer"));
        put(&d, "app.c.yml", &declared_as(&child, "integer"));
        convert(&d, "app.p.id", "id::integer");
        convert(&d, "app.c.pid", "pid::integer");
        let artifact = connected_artifact(&d, connection, false);
        succeeds(approved_apply(&d, connection, &artifact, GATE));
        assert_eq!(
            scalar(connection, "SELECT sum(pid)::bigint FROM app.c"),
            3,
            "PG{version}"
        );
        on_server(connection, "INSERT INTO app.c(id) VALUES (9)");
        assert_eq!(
            scalar(connection, "SELECT pid::bigint FROM app.c WHERE id = 9"),
            1,
            "PG{version}: the default came back in the new type"
        );
        assert!(key_stands_validated(connection), "PG{version}");
        succeeds(d.run(&["verify", "--db", connection]));
    }
}

#[test]
#[ignore = "needs live PostgreSQL 16 and 18; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn nonrepeatable_recorded_self_references_apply_with_an_explicit_unchecked_reason() {
    let before = "table: app.t\ncolumns:\n  id: {type: text, nullable: false}\nprimary_key: {name: pk_t, columns: [id]}\nforeign_keys:\n  fk_self:\n    columns: [id]\n    references: app.t(id)\n";
    for (version, url) in servers() {
        for (case, ty, expression) in [
            ("random", "double precision", "random()"),
            ("now", "timestamptz", "now()"),
            ("sqlclock", "timestamptz", "CURRENT_TIMESTAMP"),
            (
                "setting",
                "double precision",
                "current_setting('server_version_num')::double precision",
            ),
            (
                "assignment",
                "timestamptz",
                "pg_catalog.make_timestamp(2020, 1, 1, 0, 0, 0)",
            ),
            ("input", "timestamptz", "id::timestamptz"),
        ] {
            let slug = format!("convert2394-{case}{version}");
            let own = OwnDatabase::new(&url, &slug);
            let connection = own.connection();
            on_server(connection, "CREATE SCHEMA app");
            let d = Demo::new(&slug);
            put(&d, "app.t.yml", before);
            succeeds(d.run(&["plan"]));
            d.commit();
            succeeds(d.run(&["bootstrap", "--db", connection]));
            on_server(connection, "INSERT INTO app.t VALUES ('2020-01-01')");
            put(&d, "app.t.yml", &declared_as(before, ty));
            convert(&d, "app.t.id", expression);
            let artifact = connected_artifact(&d, connection, false);
            let applied = approved_apply(&d, connection, &artifact, GATE);
            let text = stderr(&applied);
            assert_eq!(code(&applied), 0, "PG{version} {case}: {text}");
            assert!(
                text.contains("fk_self")
                    && text.contains(expression)
                    && text.contains("not certified IMMUTABLE"),
                "PG{version} {case}: {text}"
            );
            assert_eq!(scalar(connection, "SELECT count(*) FROM app.t"), 1);
            assert_eq!(
                scalar(
                    connection,
                    "SELECT count(*) FROM pg_constraint WHERE conname = 'fk_self' AND convalidated"
                ),
                1
            );
            succeeds(d.run(&["verify", "--db", connection]));
        }
    }
}
