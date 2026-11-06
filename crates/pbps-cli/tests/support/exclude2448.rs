//! An exclusion constraint through the whole tool (#2448 PR C1a, DEC-2448.3): the
//! pull writes one, the round trip ends in an empty plan, and what the next parts
//! deliver (an add, a drop, a replacement, a column change under one) is refused
//! by name. Each case runs on both pinned engines.
use super::{Demo, OwnDatabase, code, on_server, scalar, server, stderr, stdout, succeeds};

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
fn an_exclusion_constraint_round_trips_and_adding_one_is_refused_by_name() {
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

        // Adding one is the next PR's: refused by name, to a standing table and
        // to a new one, and nothing reaches the database.
        let table = table.replacen(
            "exclude:\n",
            "exclude:\n  by_ip:\n    method: gist\n    keys:\n      - column: ip\n        \
             opclass: inet_ops\n        operator: \"&&\"\n",
            1,
        );
        std::fs::write(&path, &table).unwrap();
        d.commit();
        let refused = d.run(&["plan", "--db", connection]);
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        assert!(
            said(&refused).contains("adding one arrives in the next PR"),
            "{}",
            said(&refused)
        );
        assert_eq!(constraints(connection, "by_ip"), 0);
        std::fs::write(
            &path,
            table.replacen("  by_ip:\n    method: gist\n    keys:\n      - column: ip\n        opclass: inet_ops\n        operator: \"&&\"\n", "", 1),
        )
        .unwrap();
        d.commit();
        // The new table's identities are minted offline and committed first, as
        // the table alone: a plan that is refused mints nothing.
        let slot = d.dir.join("schema/app.slot.yml");
        let bare = "table: app.slot\ncolumns:\n  id: {type: integer, nullable: false}\n  a: {type: \
                    integer, nullable: false}\n  b: {type: integer, nullable: false}\nprimary_key: [id]\n";
        std::fs::write(&slot, bare).unwrap();
        succeeds(d.run(&["plan"]));
        d.commit();
        std::fs::write(
            &slot,
            format!(
                "{bare}exclude:\n  slot_overlap:\n    method: gist\n    keys:\n      - \
                 {{expression: \"int4range(a, b)\", operator: \"&&\"}}\n"
            ),
        )
        .unwrap();
        d.commit();
        let refused = d.run(&["plan", "--db", connection]);
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        assert!(
            said(&refused).contains("adding one arrives in the next PR"),
            "{}",
            said(&refused)
        );
        assert_eq!(constraints(connection, "slot_overlap"), 0);
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
            "pg_catalog",
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

/// What the next PR delivers is refused by name while an exclusion stands: its
/// drop and replacement (which need the retirement record), and a column change
/// under it, which the engine would otherwise answer by dropping or rebuilding
/// the constraint without any plan saying so (measured on 16.15 and 18.6).
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_standing_exclusion_is_not_dropped_replaced_or_stripped_of_a_column_by_a_plan() {
    let _ = server();
    servers(|url| {
        let own = OwnDatabase::new(url, "exclude-standing");
        let connection = own.connection();
        on_server(connection, SETUP);
        let d = Demo::new("exclude-standing");
        succeeds(d.run(&["pull", "--db", connection]));
        d.commit();
        succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
        let path = d.dir.join("schema/app.booking.yml");
        let held = std::fs::read_to_string(&path).unwrap();
        let refused = |text: String, expected: &str| {
            std::fs::write(&path, text).unwrap();
            d.commit();
            let out = d.run(&["plan", "--db", connection]);
            assert_ne!(code(&out), 0, "{expected}: {}", said(&out));
            assert!(said(&out).contains(expected), "{expected}: {}", said(&out));
            assert_eq!(constraints(connection, "no_overlap"), 1);
        };
        // Replaced: another operator on the same name.
        refused(
            held.replacen("operator: \"&&\"", "operator: \"-|-\"", 1),
            "replacing",
        );
        // Dropped, or renamed (a drop and an add): the name leaves the file.
        refused(
            held.replace("no_overlap:", "no_touching:"),
            "retirement record",
        );
        // A column the constraint holds is retyped: the engine would rebuild
        // the constraint under an exclusive lock and say nothing. (A drop of
        // one is refused the same way; the unit test of the differ holds it.)
        refused(
            held.replacen(
                "  room:\n    type: integer\n",
                "  room:\n    type: bigint\n",
                1,
            ),
            "while the constraint holds it",
        );
        // Control: a change that holds none of them is planned. The refused
        // edits are put back and committed first, so the new column is the only
        // difference, and its identity is minted offline and committed.
        std::fs::write(&path, &held).unwrap();
        d.commit();
        std::fs::write(
            &path,
            held.replacen(
                "  ip:\n    type: inet\n",
                "  ip:\n    type: inet\n  extra:\n    type: integer\n",
                1,
            ),
        )
        .unwrap();
        succeeds(d.run(&["plan"]));
        d.commit();
        let out = succeeds(d.run(&["plan", "--db", connection]));
        assert!(!said(&out).contains("exclusion"), "{}", said(&out));
    });
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
