//! An exclusion constraint through the whole tool (#2448 PR C1, DEC-2448.3): the
//! pull writes one, the round trip ends in an empty plan, a declared one is added
//! behind the `constraint` risk and enforces, a new table takes its constraint
//! after its create, and what the next PR delivers (a drop, a replacement, a
//! column change under one) is refused by name. Each case runs on both pinned
//! engines.
use super::{
    Demo, OwnDatabase, approved_apply, code, on_server, scalar, server, stderr, stdout, succeeds,
    try_on_server,
};

fn servers(mut check: impl FnMut(&str)) {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        check(&std::env::var(variable).expect(variable));
    }
}

fn said(output: &std::process::Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

const SETUP: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.booking (id integer PRIMARY KEY, room integer NOT NULL, \
      starts integer NOT NULL, ends integer NOT NULL, note text, \
      active boolean NOT NULL DEFAULT true, ip inet); \
    INSERT INTO app.booking VALUES (1, 1, 1, 5, 'a', true, '10.0.0.0/24'), \
      (2, 1, 5, 9, 'b', true, '10.0.1.0/24'); \
    ALTER TABLE app.booking ADD CONSTRAINT no_overlap EXCLUDE USING gist \
      (int4range(starts, ends, '[)') WITH &&) INCLUDE (note) WITH (fillfactor = 70) \
      WHERE (active) DEFERRABLE INITIALLY DEFERRED; \
    ALTER TABLE app.booking ADD CONSTRAINT one_per_room EXCLUDE USING btree \
      (room WITH =) WHERE (id < 0)";

fn constraints(connection: &str, name: &str) -> i64 {
    scalar(
        connection,
        &format!("SELECT count(*) FROM pg_constraint WHERE conname = '{name}' AND contype = 'x'"),
    )
}

#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn an_exclusion_constraint_round_trips_is_added_behind_its_risk_and_enforces() {
    let _ = server();
    servers(|url| {
        let own = OwnDatabase::new(url, "exclude-round-trip");
        let connection = own.connection();
        on_server(connection, SETUP);
        let d = Demo::new("exclude-round-trip");
        let pulled = succeeds(d.run(&["pull", "--db", connection]));
        let path = d.dir.join("schema/app.booking.yml");
        let table = std::fs::read_to_string(&path).unwrap();
        for held in [
            "exclude:",
            "no_overlap:",
            "method: gist",
            "operator:",
            "deferrable: true",
            "initially: deferred",
            "one_per_room:",
            "method: btree",
        ] {
            assert!(table.contains(held), "the pull writes `{held}`:\n{table}");
        }
        // Each constraint is declared once, with the index it owns: neither is
        // an index of its own.
        assert!(!table.contains("indexes:"), "{table}\n{}", said(&pulled));
        d.commit();
        succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
        succeeds(d.run(&["validate"]));
        let next = succeeds(d.run(&["plan", "--db", connection]));
        assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
        succeeds(d.run(&["verify", "--db", connection]));

        // A declared exclusion is added behind `constraint` and nothing else.
        let table = table.replacen(
            "exclude:\n",
            "exclude:\n  by_ip:\n    method: gist\n    keys:\n      - column: ip\n        \
             opclass: inet_ops\n        operator: \"&&\"\n",
            1,
        );
        std::fs::write(&path, table).unwrap();
        d.commit();
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
        let script = std::fs::read_to_string(&sql).unwrap();
        assert!(
            script.contains("ADD CONSTRAINT \"by_ip\" EXCLUDE USING gist")
                && script.contains("WITH OPERATOR(\"pg_catalog\".&&)")
                && script.contains("\"pg_catalog\".\"inet_ops\"")
                && !script.contains("DROP"),
            "{script}"
        );
        let refused = approved_apply(&d, connection, &plan, &[]);
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        assert!(said(&refused).contains("constraint"), "{}", said(&refused));
        assert_eq!(
            constraints(connection, "by_ip"),
            0,
            "a refused apply changes nothing"
        );
        succeeds(approved_apply(
            &d,
            connection,
            &plan,
            &["--allow", "constraint"],
        ));
        assert_eq!(constraints(connection, "by_ip"), 1);
        let next = succeeds(d.run(&["plan", "--db", connection]));
        assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
        succeeds(d.run(&["verify", "--db", connection]));
        // It enforces: a second row of an address the first covers is refused.
        assert!(
            try_on_server(
                connection,
                "INSERT INTO app.booking VALUES (3, 2, 20, 21, 'c', true, '10.0.0.0/24')"
            )
            .is_err()
        );

        // A new table takes its exclusion after its create, with no flag: the
        // table has no rows to check.
        std::fs::write(
            d.dir.join("schema/app.slot.yml"),
            "table: app.slot\ncolumns:\n  id: {type: integer, nullable: false}\n  a: {type: integer, \
             nullable: false}\n  b: {type: integer, nullable: false}\nprimary_key: [id]\nexclude:\n  \
             slot_overlap:\n    method: gist\n    keys:\n      - {expression: \"int4range(a, b)\", \
             operator: \"&&\"}\n",
        )
        .unwrap();
        // An offline plan mints the new table's identities, and the project
        // commits them: a connected plan computes against what reviewers read.
        succeeds(d.run(&["plan"]));
        d.commit();
        let sql = d.dir.join("slot.sql");
        succeeds(d.run(&[
            "plan",
            "--db",
            connection,
            "--out",
            plan.to_str().unwrap(),
            "--sql",
            sql.to_str().unwrap(),
        ]));
        let script = std::fs::read_to_string(&sql).unwrap();
        let create = script.find("CREATE TABLE").expect(&script);
        let add = script
            .find("ADD CONSTRAINT \"slot_overlap\"")
            .expect(&script);
        assert!(create < add, "{script}");
        succeeds(approved_apply(&d, connection, &plan, &[]));
        assert_eq!(constraints(connection, "slot_overlap"), 1);
        let next = succeeds(d.run(&["plan", "--db", connection]));
        assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
        succeeds(d.run(&["verify", "--db", connection]));
    });
}

/// A declaration the engine would refuse, or this tool does not hold yet, is
/// refused at `validate` by name and never reaches the engine; one the engine
/// accepts but the model cannot read back is left out of the pull, named.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_declaration_this_tool_cannot_hold_is_refused_and_an_unreadable_exclusion_is_left_out() {
    let _ = server();
    let d = Demo::new("exclude-refusals");
    let write = |exclude: &str| {
        std::fs::create_dir_all(d.dir.join("schema")).unwrap();
        std::fs::write(
            d.dir.join("schema/app.t.yml"),
            format!(
                "table: app.t\ncolumns:\n  id: {{type: integer}}\n  n: {{type: integer}}\n  \
                 ip: {{type: inet}}\nexclude:\n{exclude}"
            ),
        )
        .unwrap();
    };
    let key = "    keys:\n      - {column: n, operator: \"=\"}\n";
    for (exclude, expected) in [
        (format!("  x:\n{key}"), "method"),
        (
            "  x:\n    method: gin\n    keys:\n      - {column: n, operator: \"=\"}\n".to_owned(),
            "cannot back an exclusion",
        ),
        (
            "  x:\n    method: gist\n    keys:\n      - {column: n, operator: \"&&\"}\n".to_owned(),
            "index an expression",
        ),
        (
            format!("  x:\n    method: btree\n{key}    initially: deferred\n"),
            "not deferrable",
        ),
        (
            "  x:\n    method: btree\n    keys:\n      - {column: n, operator: \"ext.=\"}\n"
                .to_owned(),
            // A schema's operator is held since #2447 PR G2 (DEC-2447.14), where the
            // project declares a package in that schema. This project declares none in
            // `ext`, so it is refused for that, no longer for being outside `pg_catalog`.
            "declares no extension in schema",
        ),
        (
            format!("  x:\n    method: btree\n{key}    not_valid: true\n"),
            "not_valid",
        ),
        (
            format!("  x:\n    method: btree\n{key}    include: [ghost]\n"),
            "ghost",
        ),
    ] {
        write(&exclude);
        let out = d.run(&["validate"]);
        assert_eq!(code(&out), 2, "{exclude}: {}", said(&out));
        assert!(said(&out).contains(expected), "{exclude}: {}", said(&out));
    }
    // Control: the same table with a held exclusion validates.
    write(&format!("  x:\n    method: btree\n{key}"));
    succeeds(d.run(&["validate"]));
    // A partitioned table's exclusion is the next PR's, by name.
    std::fs::write(
        d.dir.join("schema/app.t.yml"),
        "table: app.t\ncolumns:\n  id: {type: integer}\n  n: {type: integer}\npartition_by: [id]\n\
         partitions:\n  p1: {from: [0], to: [10]}\nexclude:\n  x:\n    method: btree\n    keys:\n      \
         - {column: n, operator: \"=\"}\n",
    )
    .unwrap();
    let out = d.run(&["validate"]);
    assert_eq!(code(&out), 2, "{}", said(&out));
    assert!(
        said(&out).contains("partitioned table or a partition"),
        "{}",
        said(&out)
    );

    // The engine accepts what the model cannot read: each is left out of the
    // pull, named, and is not read back weaker.
    servers(|url| {
        let own = OwnDatabase::new(url, "exclude-unreadable");
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA app; \
             CREATE TABLE app.t (id integer PRIMARY KEY, a integer, label text, ip inet); \
             ALTER TABLE app.t ADD CONSTRAINT h EXCLUDE USING hash (a WITH =); \
             ALTER TABLE app.t ADD CONSTRAINT s EXCLUDE USING spgist (ip WITH &&); \
             ALTER TABLE app.t ADD CONSTRAINT p EXCLUDE USING btree \
               (label text_pattern_ops WITH =); \
             ALTER TABLE app.t ADD CONSTRAINT ok EXCLUDE USING btree (a WITH =) WHERE (id < 0)",
        );
        let pull = Demo::new("exclude-unreadable");
        let out = succeeds(pull.run(&["pull", "--db", connection]));
        let said = said(&out);
        for expected in [
            "uses the `hash` method",
            "uses the `spgist` method",
            "text_pattern_ops",
        ] {
            assert!(said.contains(expected), "{expected}: {said}");
        }
        let table = std::fs::read_to_string(pull.dir.join("schema/app.t.yml")).unwrap();
        assert!(table.contains("ok:"), "{table}");
        for left_out in ["  h:", "  s:", "  p:"] {
            assert!(!table.contains(left_out), "{left_out}: {table}");
        }
    });
}

/// Dropping an exclusion needs its recorded reason, for that definition; with
/// it, the plan drops the constraint and its index in one statement behind the
/// `destructive` gate, and a pull afterwards writes none and plans nothing
/// (DEC-2448.4).
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_dropped_exclusion_needs_its_recorded_reason_and_a_pull_after_it_round_trips() {
    let _ = server();
    servers(|url| {
        let own = OwnDatabase::new(url, "exclude-drop");
        let connection = own.connection();
        on_server(connection, SETUP);
        let d = Demo::new("exclude-drop");
        succeeds(d.run(&["pull", "--db", connection]));
        d.commit();
        succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
        let path = d.dir.join("schema/app.booking.yml");
        let held = std::fs::read_to_string(&path).unwrap();
        let dropped = remove_exclusion(&held, "no_overlap");
        assert_ne!(dropped, held, "{held}");
        std::fs::write(&path, &dropped).unwrap();
        // No record: refused at identity, naming the command, and nothing runs.
        let out = d.run(&["plan", "--db", connection]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("pbps drop-exclusion app.booking no_overlap --reason"),
            "{}",
            said(&out)
        );
        assert_eq!(constraints(connection, "no_overlap"), 1);
        // A name the baseline does not hold is a typo, not a silent success.
        let out = d.run(&["drop-exclusion", "app.booking", "nope", "--reason", "x"]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        // Recorded: the reason is kept, and saying it twice changes nothing.
        let recorded = succeeds(d.run(&[
            "drop-exclusion",
            "app.booking",
            "no_overlap",
            "--reason",
            "the booking service enforces it now",
        ]));
        assert!(said(&recorded).contains("updated"), "{}", said(&recorded));
        let again = succeeds(d.run(&[
            "drop-exclusion",
            "app.booking",
            "no_overlap",
            "--reason",
            "the booking service enforces it now",
        ]));
        assert!(said(&again).contains("unchanged"), "{}", said(&again));
        d.commit();
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
        let script = std::fs::read_to_string(&sql).unwrap();
        assert!(
            script.contains("DROP CONSTRAINT \"no_overlap\"") && !script.contains("DROP INDEX"),
            "{script}"
        );
        let refused = approved_apply(&d, connection, &plan, &[]);
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        assert!(said(&refused).contains("destructive"), "{}", said(&refused));
        assert_eq!(constraints(connection, "no_overlap"), 1);
        succeeds(approved_apply(
            &d,
            connection,
            &plan,
            &["--allow", "destructive"],
        ));
        assert_eq!(constraints(connection, "no_overlap"), 0);
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_class WHERE relname = 'no_overlap'"
            ),
            0,
            "the backing index went with the constraint"
        );
        assert_eq!(constraints(connection, "one_per_room"), 1);
        let next = succeeds(d.run(&["plan", "--db", connection]));
        assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
        succeeds(d.run(&["verify", "--db", connection]));
        // A pull of the database as it is now writes none of it: the declarations
        // are what the database holds, and the record stays in the identity files.
        let fresh = Demo::new("exclude-drop-pull");
        succeeds(fresh.run(&["pull", "--db", connection]));
        let pulled = std::fs::read_to_string(fresh.dir.join("schema/app.booking.yml")).unwrap();
        assert!(
            !pulled.contains("no_overlap") && pulled.contains("one_per_room"),
            "{pulled}"
        );
        // Declared again, the name is added like any new constraint: the record
        // that stays is no obstacle, since it only ever authorises a drop.
        std::fs::write(&path, &held).unwrap();
        d.commit();
        let script = succeed_plan(&d, connection, &plan, &sql);
        assert!(script.contains("ADD CONSTRAINT \"no_overlap\""), "{script}");
    });
}

/// A changed definition at one name replaces the constraint, behind both risks
/// and with no reason; a change of its storage parameters alone leaves the
/// constraint and its index standing (DEC-2448.4, measured on 16.15 and 18.6).
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_replaced_exclusion_is_rebuilt_and_an_option_only_change_keeps_it_standing() {
    let _ = server();
    servers(|url| {
        let own = OwnDatabase::new(url, "exclude-replace");
        let connection = own.connection();
        on_server(connection, SETUP);
        let d = Demo::new("exclude-replace");
        succeeds(d.run(&["pull", "--db", connection]));
        d.commit();
        succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
        let path = d.dir.join("schema/app.booking.yml");
        let held = std::fs::read_to_string(&path).unwrap();
        let oid = |name: &str| {
            scalar(
                connection,
                &format!("SELECT oid::bigint FROM pg_constraint WHERE conname = '{name}'"),
            )
        };
        let plan = d.dir.join("plan.json");
        let sql = d.dir.join("plan.sql");
        let plan_to = |text: &str| -> String {
            std::fs::write(&path, text).unwrap();
            d.commit();
            succeed_plan(&d, connection, &plan, &sql)
        };
        // Options only: an `ALTER INDEX`, with no flag, and the constraint's own
        // identity is the same afterwards.
        let tuned = held.replacen("fillfactor: 70", "fillfactor: 60", 1);
        assert_ne!(tuned, held, "{held}");
        let before = oid("no_overlap");
        let script = plan_to(&tuned);
        assert!(
            script.contains("ALTER INDEX \"app\".\"no_overlap\" SET (fillfactor = '60')")
                && !script.contains("DROP")
                && !script.contains("ADD CONSTRAINT"),
            "{script}"
        );
        succeeds(approved_apply(&d, connection, &plan, &[]));
        assert_eq!(oid("no_overlap"), before, "the constraint stands");
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_class WHERE relname = 'no_overlap' \
                 AND reloptions @> ARRAY['fillfactor=60']"
            ),
            1
        );
        let next = succeeds(d.run(&["plan", "--db", connection]));
        assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
        // A structural change at the name replaces it: the drop, then the add,
        // behind `destructive` and `constraint`, and no reason is asked.
        let replaced = tuned.replacen("    include:\n      - note\n", "", 1);
        let replaced = if replaced == tuned {
            tuned.replacen("include: [note]", "", 1)
        } else {
            replaced
        };
        assert_ne!(replaced, tuned, "{tuned}");
        let script = plan_to(&replaced);
        let drop = script
            .find("DROP CONSTRAINT \"no_overlap\"")
            .expect(&script);
        let add = script.find("ADD CONSTRAINT \"no_overlap\"").expect(&script);
        assert!(drop < add && !script.contains("DROP INDEX"), "{script}");
        let refused = approved_apply(&d, connection, &plan, &["--allow", "destructive"]);
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        assert!(said(&refused).contains("constraint"), "{}", said(&refused));
        assert_eq!(oid("no_overlap"), before, "a refused apply changes nothing");
        succeeds(approved_apply(
            &d,
            connection,
            &plan,
            &["--allow", "destructive", "--allow", "constraint"],
        ));
        assert_ne!(oid("no_overlap"), before, "the constraint was built again");
        let next = succeeds(d.run(&["plan", "--db", connection]));
        assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
        succeeds(d.run(&["verify", "--db", connection]));
        // It still enforces.
        assert!(
            try_on_server(
                connection,
                "INSERT INTO app.booking VALUES (3, 2, 1, 5, 'c', true, NULL)"
            )
            .is_err()
        );
    });
}

/// A key column of an exclusion is dropped only in a plan that drops the
/// constraint with its record; alone, the engine would drop the constraint and
/// say nothing, so the plan is refused by name, and a retype is refused whatever
/// else the plan holds (DEC-2448.4, measured on 16.15 and 18.6).
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_key_column_drop_under_an_exclusion_is_refused_alone_and_planned_with_its_drop() {
    let _ = server();
    servers(|url| {
        let own = OwnDatabase::new(url, "exclude-column");
        let connection = own.connection();
        on_server(connection, SETUP);
        let d = Demo::new("exclude-column");
        succeeds(d.run(&["pull", "--db", connection]));
        d.commit();
        succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
        let path = d.dir.join("schema/app.booking.yml");
        let held = std::fs::read_to_string(&path).unwrap();
        // `room` is `one_per_room`'s key. A declaration cannot keep the constraint
        // without the column (`validate` refuses it), so the column goes with it,
        // and without the constraint's record the plan is refused at identity,
        // naming the command: the engine would otherwise have dropped the
        // constraint with the column and said nothing.
        let no_room = remove_exclusion(&remove_column(&held, "room"), "one_per_room");
        assert_ne!(no_room, held, "{held}");
        std::fs::write(&path, &no_room).unwrap();
        succeeds(d.run(&["drop", "app.booking.room", "--reason", "retired"]));
        d.commit();
        let out = d.run(&["plan", "--db", connection]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        assert!(
            said(&out).contains("pbps drop-exclusion app.booking one_per_room --reason"),
            "{}",
            said(&out)
        );
        assert_eq!(constraints(connection, "one_per_room"), 1);
        // With its record, the constraint leaves first and the column after:
        // planned, and applied.
        // HEAD no longer declares it: the baseline is the revision before.
        let out = d.run(&[
            "drop-exclusion",
            "app.booking",
            "one_per_room",
            "--reason",
            "the column is retired",
        ]);
        assert_ne!(code(&out), 0, "{}", said(&out));
        succeeds(d.run(&[
            "drop-exclusion",
            "app.booking",
            "one_per_room",
            "--reason",
            "the column is retired",
            "--since",
            "HEAD~1",
        ]));
        d.commit();
        let plan = d.dir.join("plan.json");
        let sql = d.dir.join("plan.sql");
        let script = succeed_plan(&d, connection, &plan, &sql);
        let constraint = script
            .find("DROP CONSTRAINT \"one_per_room\"")
            .expect(&script);
        let column = script.find("DROP COLUMN \"room\"").expect(&script);
        assert!(constraint < column, "{script}");
        succeeds(approved_apply(
            &d,
            connection,
            &plan,
            &["--allow", "destructive"],
        ));
        assert_eq!(constraints(connection, "one_per_room"), 0);
        assert_eq!(constraints(connection, "no_overlap"), 1);
        let next = succeeds(d.run(&["plan", "--db", connection]));
        assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
        succeeds(d.run(&["verify", "--db", connection]));
    });
}

/// The plan's SQL for the declarations as they stand, which must plan.
fn succeed_plan(
    d: &Demo,
    connection: &str,
    plan: &std::path::Path,
    sql: &std::path::Path,
) -> String {
    succeeds(d.run(&[
        "plan",
        "--db",
        connection,
        "--out",
        plan.to_str().unwrap(),
        "--sql",
        sql.to_str().unwrap(),
    ]));
    std::fs::read_to_string(sql).unwrap()
}

/// `text` without the `exclude:` entry `name`: the entry's own lines, up to the
/// next one at its indent. A pulled file indents an entry two spaces under
/// `exclude:`.
fn remove_exclusion(text: &str, name: &str) -> String {
    let mut out = Vec::new();
    let mut skipping = false;
    for line in text.lines() {
        if line == format!("  {name}:") {
            skipping = true;
            continue;
        }
        if skipping {
            if line.starts_with("    ") {
                continue;
            }
            skipping = false;
        }
        out.push(line);
    }
    let mut out = out.join("\n") + "\n";
    // The map is left empty when its last entry went: drop the key too.
    if out.contains("exclude:\n") && !out.contains("exclude:\n  ") {
        out = out.replace("exclude:\n", "");
    }
    out
}

/// `text` without the column `name`: its lines under `columns:`, at the
/// indent a pulled file writes them.
fn remove_column(text: &str, name: &str) -> String {
    let mut out = Vec::new();
    let mut skipping = false;
    for line in text.lines() {
        if line == format!("  {name}:") {
            skipping = true;
            continue;
        }
        if skipping {
            if line.starts_with("    ") {
                continue;
            }
            skipping = false;
        }
        out.push(line);
    }
    out.join("\n") + "\n"
}

/// A retype of a column an exclusion's expression holds is refused by name
/// whatever the column is called: the guard reads the expression with the
/// engine's identifier scan, so a quoted name with a hyphen, a space or a mixed
/// case is found (DEC-2448.3).
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_retype_of_a_quoted_key_column_in_an_expression_is_refused_by_name() {
    let _ = server();
    servers(|url| {
        let own = OwnDatabase::new(url, "exclude-quoted");
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA app; \
             CREATE TABLE app.q (id integer PRIMARY KEY, \"Start-Time\" integer NOT NULL, \
               \"end time\" integer NOT NULL, free integer); \
             ALTER TABLE app.q ADD CONSTRAINT x EXCLUDE USING gist \
               (int4range(\"Start-Time\", \"end time\") WITH &&)",
        );
        let d = Demo::new("exclude-quoted");
        succeeds(d.run(&["pull", "--db", connection]));
        d.commit();
        succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
        let path = d.dir.join("schema/app.q.yml");
        let held = std::fs::read_to_string(&path).unwrap();
        let retyped = |column: &str| {
            let text = held.replacen(
                &format!("{column}:\n    type: integer"),
                &format!("{column}:\n    type: bigint"),
                1,
            );
            assert_ne!(text, held, "{column}:\n{held}");
            std::fs::write(&path, text).unwrap();
            d.commit();
            d.run(&["plan", "--db", connection])
        };
        for column in ["Start-Time", "end time"] {
            let out = retyped(column);
            assert_ne!(code(&out), 0, "{column}: {}", said(&out));
            assert!(
                said(&out).contains("while the constraint holds it"),
                "{column}: {}",
                said(&out)
            );
        }
        // Control: a column the expression does not mention retypes freely.
        succeeds(retyped("free"));
        assert_eq!(constraints(connection, "x"), 1);
    });
}

/// A label removed from an enum rebuilds the type and retypes every column of
/// it, in a change the enum pass pushes before the table loop runs: the guard
/// is asked of the whole plan, so it refuses by name where an exclusion holds
/// that column, and a rebuild of a column no exclusion holds still plans
/// (DEC-2448.3).
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn an_enum_rebuild_that_retypes_a_held_column_is_refused_by_name() {
    let _ = server();
    servers(|url| {
        for (held, expect_refused) in [("m", true), ("n", false)] {
            let own = OwnDatabase::new(url, "exclude-enum");
            let connection = own.connection();
            on_server(
                connection,
                &format!(
                    "CREATE SCHEMA app; CREATE TYPE app.mood AS ENUM ('a', 'b', 'c'); \
                     CREATE TABLE app.e (id integer PRIMARY KEY, m app.mood, n integer); \
                     ALTER TABLE app.e ADD CONSTRAINT x EXCLUDE USING btree ({held} WITH =)"
                ),
            );
            let d = Demo::new("exclude-enum");
            succeeds(d.run(&["pull", "--db", connection]));
            d.commit();
            succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
            let path = d.dir.join("schema/app.mood.type.yml");
            let text = std::fs::read_to_string(&path).unwrap();
            let without = text.replace("a, b, c", "a, c");
            assert_ne!(text, without, "{text}");
            std::fs::write(&path, without).unwrap();
            d.commit();
            let out = d.run(&["plan", "--db", connection]);
            let said = said(&out);
            if expect_refused {
                assert_ne!(code(&out), 0, "{held}: {said}");
                assert!(said.contains("while the constraint holds it"), "{said}");
            } else {
                assert!(
                    !said.contains("while the constraint holds it"),
                    "control {held}: {said}"
                );
            }
            assert_eq!(constraints(connection, "x"), 1);
        }
    });
}
