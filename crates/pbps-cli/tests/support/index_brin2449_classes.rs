//! BRIN's minmax-multi and Bloom classes, with the options of their own, through
//! the whole tool (#2449 PR R2, DEC-2449.2): the pull writes each key's
//! `opclass_parameters` from `attoptions` in their canonical spelling, the round
//! trip ends in an empty plan, a class option change or removal replaces the
//! index (one replacement, carrying every final option), a bootstrap creates the
//! declared index and reads it back, a value outside the engine's bounds is
//! refused at `validate`, a filter that does not fit fails the apply and leaves
//! the old index and its rows, and a shape this model does not hold is left out
//! of the pull by name. Each case runs on both pinned engines.
use super::{
    Demo, OwnDatabase, approved_apply, code, on_server, scalar, server, stderr, stdout, succeeds,
};

fn servers(mut check: impl FnMut(&str)) {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        check(&std::env::var(variable).expect(variable));
    }
}

fn said(output: &std::process::Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

/// How many of the index's keys at `attnum` carry exactly this `attoptions`
/// entry: the options are the key's, not the index's `reloptions`.
fn has_option(connection: &str, index: &str, attnum: i32, entry: &str) -> bool {
    scalar(
        connection,
        &format!(
            "SELECT count(*) FROM pg_attribute WHERE attrelid = 'app.{index}'::regclass \
             AND attnum = {attnum} AND attoptions @> ARRAY['{entry}']"
        ),
    ) == 1
}

fn no_options(connection: &str, index: &str, attnum: i32) -> bool {
    scalar(
        connection,
        &format!(
            "SELECT count(*) FROM pg_attribute WHERE attrelid = 'app.{index}'::regclass \
             AND attnum = {attnum} AND attoptions IS NULL"
        ),
    ) == 1
}

fn empty_plan(d: &Demo, connection: &str) {
    let next = succeeds(d.run(&["plan", "--db", connection]));
    assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
}

fn plan_sql(d: &Demo, connection: &str) -> String {
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
    std::fs::read_to_string(&sql).unwrap()
}

const SETUP: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.reading (id integer PRIMARY KEY, taken timestamptz NOT NULL, \
      sensor integer NOT NULL, ip inet, note text, label varchar(10)); \
    CREATE INDEX ix_multi ON app.reading USING brin (sensor int4_minmax_multi_ops (values_per_range = '040')); \
    CREATE INDEX ix_pin ON app.reading USING brin (sensor int4_minmax_multi_ops (values_per_range = 32)); \
    CREATE INDEX ix_class ON app.reading USING brin (taken timestamptz_minmax_multi_ops); \
    CREATE INDEX ix_bloom ON app.reading USING brin ((sensor + 1) int4_bloom_ops \
      (n_distinct_per_range = '3.0e1', false_positive_rate = '1e-2')); \
    CREATE INDEX ix_mixed ON app.reading USING brin (taken timestamptz_minmax_multi_ops \
      (values_per_range = 64), ip inet_bloom_ops (false_positive_rate = 0.05), note text_bloom_ops, \
      label) WITH (pages_per_range = 32) WHERE note IS NOT NULL; \
    CREATE INDEX ix_default ON app.reading USING brin (sensor)";

#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn brin_classes_with_options_round_trip_and_a_change_replaces_the_index_once() {
    let _ = server();
    servers(|url| {
        let own = OwnDatabase::new(url, "brin-classes");
        let connection = own.connection();
        on_server(connection, SETUP);
        let d = Demo::new("brin-classes");
        succeeds(d.run(&["pull", "--db", connection]));
        let path = d.dir.join("schema/app.reading.yml");
        let table = std::fs::read_to_string(&path).unwrap();
        // Each option is the key's, in its canonical spelling: octal `040` is 32,
        // `3.0e1` is 30, `1e-2` is 0.01; an explicit default stays a pin.
        for held in [
            "opclass: int4_minmax_multi_ops",
            "opclass_parameters: {values_per_range: 32}",
            "opclass_parameters: {false_positive_rate: 0.01, n_distinct_per_range: 30}",
            "opclass_parameters: {values_per_range: 64}",
            "opclass_parameters: {false_positive_rate: 0.05}",
            "opclass: text_bloom_ops",
            "opclass: timestamptz_minmax_multi_ops",
        ] {
            assert!(table.contains(held), "the pull writes `{held}`:\n{table}");
        }
        assert!(
            !table.contains("040") && !table.contains("3.0e1"),
            "no engine spelling survives:\n{table}"
        );
        d.commit();
        succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
        succeeds(d.run(&["validate"]));
        empty_plan(&d, connection);
        succeeds(d.run(&["verify", "--db", connection]));

        // A class option change replaces the index: it is not an `ALTER`, and the
        // drop is an ordinary index drop (`--allow destructive`, no reason).
        let plan = d.dir.join("plan.json");
        std::fs::write(
            &path,
            table.replacen(
                "opclass_parameters: {values_per_range: 64}",
                "opclass_parameters: {values_per_range: 128}",
                1,
            ),
        )
        .unwrap();
        d.commit();
        let script = plan_sql(&d, connection);
        assert!(
            script.contains("DROP INDEX")
                && script.contains("CREATE INDEX")
                && script.contains("values_per_range = '128'")
                && !script.contains("ALTER INDEX"),
            "{script}"
        );
        let refused = approved_apply(&d, connection, &plan, &[]);
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        assert!(said(&refused).contains("destructive"), "{}", said(&refused));
        assert!(has_option(connection, "ix_mixed", 1, "values_per_range=64"));
        succeeds(approved_apply(
            &d,
            connection,
            &plan,
            &["--allow", "destructive"],
        ));
        assert!(has_option(
            connection,
            "ix_mixed",
            1,
            "values_per_range=128"
        ));
        // The key's other options and the index's own come back with it.
        assert!(has_option(
            connection,
            "ix_mixed",
            2,
            "false_positive_rate=0.05"
        ));
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_class WHERE oid = 'app.ix_mixed'::regclass \
                 AND reloptions @> ARRAY['pages_per_range=32']"
            ),
            1
        );
        empty_plan(&d, connection);

        // Taking a pin out is the engine's default policy and a rebuild; a class
        // option and an in-place option changed together on another index are one
        // replacement that carries both, with no `ALTER` beside it.
        let table = std::fs::read_to_string(&path).unwrap();
        let (head, tail) = table.split_once("  ix_pin:\n").expect("ix_pin is written");
        let unpinned = tail.replacen("opclass_parameters: {values_per_range: 32}", "", 1);
        assert_ne!(unpinned, tail, "{table}");
        let edited = format!("{head}  ix_pin:\n{unpinned}")
            .replacen(
                "opclass_parameters: {values_per_range: 128}",
                "opclass_parameters: {values_per_range: 16}",
                1,
            )
            .replacen(
                "pages_per_range: 32",
                "pages_per_range: 32, autosummarize: true",
                1,
            );
        std::fs::write(&path, edited).unwrap();
        d.commit();
        let script = plan_sql(&d, connection);
        assert_eq!(script.matches("CREATE INDEX").count(), 2, "{script}");
        assert!(!script.contains("ALTER INDEX"), "{script}");
        assert!(script.contains("autosummarize = 'true'"), "{script}");
        succeeds(approved_apply(
            &d,
            connection,
            &plan,
            &["--allow", "destructive"],
        ));
        assert!(no_options(connection, "ix_pin", 1), "the pin is gone");
        assert!(has_option(connection, "ix_mixed", 1, "values_per_range=16"));
        assert!(
            has_option(connection, "ix_mixed", 2, "false_positive_rate=0.05"),
            "the other keys' options come back with the replacement"
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_class WHERE oid = 'app.ix_mixed'::regclass \
                 AND reloptions @> ARRAY['autosummarize=true', 'pages_per_range=32']"
            ),
            1
        );
        empty_plan(&d, connection);
        succeeds(d.run(&["verify", "--db", connection]));
    });
}

#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_declared_brin_class_with_options_is_created_by_bootstrap_and_read_back() {
    let _ = server();
    servers(|url| {
        let own = OwnDatabase::new(url, "brin-classes-bootstrap");
        let connection = own.connection();
        on_server(connection, "CREATE SCHEMA app");
        let d = Demo::new("brin-classes-bootstrap");
        d.table(
            "table: app.reading\ncolumns:\n  id: {type: integer, nullable: false}\n  \
             taken: {type: timestamptz}\n  sensor: {type: integer}\n  note: {type: text}\n\
             primary_key: {columns: [id]}\nindexes:\n  ix:\n    method: brin\n    keys:\n      \
             - column: sensor\n        opclass: int4_minmax_multi_ops\n        \
             opclass_parameters: {values_per_range: '0x20'}\n      - column: taken\n      \
             - column: note\n        opclass: pg_catalog.text_bloom_ops\n        \
             opclass_parameters: {n_distinct_per_range: -0.1, false_positive_rate: 1e-2}\n    \
             storage_parameters: {pages_per_range: 8}\n",
        );
        succeeds(d.run(&["validate"]));
        // The identities of the declarations are minted by a plan, before bootstrap.
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        succeeds(d.run(&["bootstrap", "--db", connection]));
        assert!(
            has_option(connection, "ix", 1, "values_per_range=0x20")
                || has_option(connection, "ix", 1, "values_per_range=32")
        );
        assert!(has_option(connection, "ix", 3, "false_positive_rate=0.01"));
        assert!(has_option(connection, "ix", 3, "n_distinct_per_range=-0.1"));
        assert!(no_options(connection, "ix", 2));
        empty_plan(&d, connection);
        succeeds(d.run(&["verify", "--db", connection]));
        // The pull of what bootstrap made says the same thing as the declaration.
        let pulled = Demo::new("brin-classes-bootstrap-pull");
        succeeds(pulled.run(&["pull", "--db", connection]));
        let text = std::fs::read_to_string(pulled.dir.join("schema/app.reading.yml")).unwrap();
        for held in [
            "values_per_range: 32",
            "false_positive_rate: 0.01",
            "n_distinct_per_range: -0.1",
            "pages_per_range: 8",
        ] {
            assert!(text.contains(held), "`{held}`:\n{text}");
        }
    });
}

/// A value outside the engine's bounds, an option of another family, options
/// with no class, and a class that is not the engine's for the key's type are
/// refused at `validate` by name; the endpoints and an explicit default validate.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_class_option_the_engine_would_refuse_is_refused_by_name_and_the_endpoints_are_held() {
    let _ = server();
    let d = Demo::new("brin-class-bounds");
    let write = |key: &str| {
        d.table(&format!(
            "table: app.t\ncolumns:\n  n: {{type: integer}}\n  ip: {{type: inet}}\nindexes:\n  ix:\n    \
             method: brin\n    keys:\n      - {key}\n"
        ))
    };
    for (key, expected) in [
        (
            "{column: n, opclass: int4_minmax_multi_ops, opclass_parameters: {values_per_range: 7}}",
            "outside 8 to 256",
        ),
        (
            "{column: n, opclass: int4_minmax_multi_ops, opclass_parameters: {values_per_range: 257}}",
            "outside 8 to 256",
        ),
        (
            "{column: n, opclass: int4_bloom_ops, opclass_parameters: {n_distinct_per_range: -1.0001}}",
            "outside -1 to 2147483647",
        ),
        (
            "{column: n, opclass: int4_bloom_ops, opclass_parameters: {n_distinct_per_range: 2147483648}}",
            "outside -1 to 2147483647",
        ),
        (
            "{column: n, opclass: int4_bloom_ops, opclass_parameters: {false_positive_rate: 0.00009}}",
            "outside 0.0001 to 0.25",
        ),
        (
            "{column: n, opclass: int4_bloom_ops, opclass_parameters: {false_positive_rate: 0.2501}}",
            "outside 0.0001 to 0.25",
        ),
        (
            "{column: n, opclass: int4_bloom_ops, opclass_parameters: {false_positive_rate: 'nan'}}",
            "not a value",
        ),
        (
            "{column: n, opclass: int4_bloom_ops, opclass_parameters: {values_per_range: 16}}",
            "takes `n_distinct_per_range`",
        ),
        (
            "{column: n, opclass: int4_minmax_multi_ops, opclass_parameters: {false_positive_rate: 0.01}}",
            "takes `values_per_range`",
        ),
        (
            "{column: ip, opclass: inet_minmax_ops, opclass_parameters: {values_per_range: 16}}",
            "takes no options",
        ),
        (
            "{column: n, opclass_parameters: {values_per_range: 16}}",
            "names its `opclass:`",
        ),
        (
            "{column: n, opclass: int8_minmax_multi_ops}",
            "over `integer` here takes",
        ),
        (
            "{column: ip, opclass: inet_inclusion_ops}",
            "leave the class out",
        ),
    ] {
        write(key);
        let out = d.run(&["validate"]);
        assert_eq!(code(&out), 2, "{key}: {}", said(&out));
        assert!(said(&out).contains(expected), "{key}: {}", said(&out));
    }
    // Controls: the endpoints, an explicit default and a class over an expression.
    for key in [
        "{column: n, opclass: int4_minmax_multi_ops, opclass_parameters: {values_per_range: 8}}",
        "{column: n, opclass: int4_minmax_multi_ops, opclass_parameters: {values_per_range: 256}}",
        "{column: n, opclass: int4_bloom_ops, opclass_parameters: {n_distinct_per_range: -1, false_positive_rate: 0.0001}}",
        "{column: n, opclass: int4_bloom_ops, opclass_parameters: {n_distinct_per_range: 2147483647, false_positive_rate: 0.25}}",
        "{column: n, opclass: int4_minmax_multi_ops, opclass_parameters: {values_per_range: 32}}",
        "{expression: '(n + 1)', opclass: int8_bloom_ops, opclass_parameters: {n_distinct_per_range: 0}}",
        "{column: ip, opclass: inet_minmax_multi_ops}",
    ] {
        write(key);
        succeeds(d.run(&["validate"]));
    }
}

/// Values in bounds that the filter cannot hold fail at the build, over rows: the
/// plan applies in one transaction, so the failed replacement leaves the old
/// index, its rows and the recorded state, and the same plan is still pending
/// (DEC-2449.2).
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_bloom_filter_that_does_not_fit_fails_the_apply_and_keeps_the_old_index() {
    let _ = server();
    servers(|url| {
        let own = OwnDatabase::new(url, "brin-classes-size");
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA app; CREATE TABLE app.t (id integer PRIMARY KEY, n integer NOT NULL); \
             INSERT INTO app.t SELECT g, g FROM generate_series(1, 2000) g; \
             CREATE INDEX ix ON app.t USING brin (n)",
        );
        let d = Demo::new("brin-classes-size");
        succeeds(d.run(&["pull", "--db", connection]));
        d.commit();
        succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
        let path = d.dir.join("schema/app.t.yml");
        let table = std::fs::read_to_string(&path).unwrap();
        let oid = scalar(connection, "SELECT 'app.ix'::regclass::oid::bigint");
        assert!(table.contains("columns: [\"n\"]"), "{table}");
        std::fs::write(
            &path,
            table.replacen(
                "columns: [\"n\"]",
                "keys:\n      - column: n\n        opclass: int4_bloom_ops\n        \
                 opclass_parameters: {n_distinct_per_range: -1}",
                1,
            ),
        )
        .unwrap();
        d.commit();
        succeeds(d.run(&["validate"]));
        let plan = d.dir.join("plan.json");
        succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
        let failed = approved_apply(&d, connection, &plan, &["--allow", "destructive"]);
        assert_ne!(code(&failed), 0, "{}", said(&failed));
        assert!(
            said(&failed).contains("too large"),
            "the engine's own words: {}",
            said(&failed)
        );
        assert_eq!(
            scalar(connection, "SELECT 'app.ix'::regclass::oid::bigint"),
            oid
        );
        assert_eq!(scalar(connection, "SELECT count(*) FROM app.t"), 2000);
        assert!(no_options(connection, "ix", 1));
        let next = succeeds(d.run(&["plan", "--db", connection]));
        assert!(!stdout(&next).contains("No changes"), "{}", stdout(&next));
        // Control: a smaller range makes the same class fit.
        let table = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            table.replacen(
                "method: brin",
                "method: brin\n    storage_parameters: {pages_per_range: 1}",
                1,
            ),
        )
        .unwrap();
        d.commit();
        succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
        succeeds(approved_apply(
            &d,
            connection,
            &plan,
            &["--allow", "destructive"],
        ));
        assert!(has_option(connection, "ix", 1, "n_distinct_per_range=-1"));
        empty_plan(&d, connection);
    });
}

/// What this model does not hold is left out of the pull and named: a class that
/// is not the engine's for the key's type (the engine takes `bpchar_bloom_ops`
/// over `text`), an option spelled as a hexadecimal real, and a column of a type
/// BRIN is not held over; the table and its other indexes stay.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_brin_shape_this_model_does_not_hold_is_left_out_of_the_pull_by_name() {
    let _ = server();
    servers(|url| {
        let own = OwnDatabase::new(url, "brin-classes-pull");
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA app; \
             CREATE TABLE app.t (id integer PRIMARY KEY, note text, n integer); \
             CREATE INDEX ix_ok ON app.t USING brin (n int4_bloom_ops (false_positive_rate = 0.02)); \
             CREATE INDEX ix_wrong_class ON app.t USING brin (note bpchar_bloom_ops); \
             CREATE INDEX ix_hex ON app.t USING brin (n int4_bloom_ops (n_distinct_per_range = '0x10'))",
        );
        let d = Demo::new("brin-classes-pull");
        let out = succeeds(d.run(&["pull", "--db", connection]));
        let text = std::fs::read_to_string(d.dir.join("schema/app.t.yml")).unwrap();
        assert!(
            text.contains("ix_ok") && text.contains("false_positive_rate: 0.02"),
            "{text}"
        );
        assert!(
            !text.contains("ix_wrong_class") && !text.contains("ix_hex"),
            "{text}"
        );
        let said = said(&out);
        for named in ["ix_wrong_class", "ix_hex"] {
            assert!(said.contains(named), "{named} is named: {said}");
        }
    });
}
