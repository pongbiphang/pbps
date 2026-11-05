//! A GiST index and a key collation through the whole tool (#2448 PR B,
//! DEC-2448.2): the pull writes them, the round trip ends in an empty plan,
//! an option-only change is an `ALTER INDEX`, a collation change replaces the
//! index behind `--allow destructive` alone (an index holds no data, so no
//! other approval), and a declaration the engine would refuse never reaches it.
//! Each case runs on both pinned engines.
use super::{
    Demo, OwnDatabase, approved_apply, code, on_server, scalar, server, stderr, stdout, succeeds,
};

fn servers(mut check: impl FnMut(&str)) {
    for variable in ["PBPS_TEST_PG_OLD_DB", "PBPS_TEST_PG_DB"] {
        check(&std::env::var(variable).expect(variable));
    }
}

const SETUP: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.booking (id integer PRIMARY KEY, starts integer NOT NULL, \
      ends integer NOT NULL, note text, active boolean, label text, doc tsvector); \
    CREATE INDEX ix_span ON app.booking USING gist (int4range(starts, ends, '[)')) \
      INCLUDE (note) WITH (fillfactor = 70) WHERE active; \
    CREATE INDEX ix_doc ON app.booking USING gist (doc); \
    CREATE INDEX ix_label ON app.booking (label COLLATE \"C\")";

#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_gist_index_and_a_key_collation_round_trip_change_and_replace_through_the_tool() {
    let _ = server();
    servers(|url| {
        let own = OwnDatabase::new(url, "gist-round-trip");
        let connection = own.connection();
        on_server(connection, SETUP);
        let d = Demo::new("gist-round-trip");
        succeeds(d.run(&["pull", "--db", connection]));
        let path = d.dir.join("schema/app.booking.yml");
        let table = std::fs::read_to_string(&path).unwrap();
        for held in ["method: gist", "collation: C", "fillfactor", "int4range("] {
            assert!(table.contains(held), "the pull writes `{held}`:\n{table}");
        }
        d.commit();
        succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
        succeeds(d.run(&["validate"]));
        let next = succeeds(d.run(&["plan", "--db", connection]));
        assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));

        // An option-only change is the `ALTER INDEX`, with no rebuild and no
        // destructive risk.
        let plan = d.dir.join("plan.json");
        std::fs::write(&path, table.replace("fillfactor: 70", "fillfactor: 80")).unwrap();
        d.commit();
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
            script.contains("ALTER INDEX \"app\".\"ix_span\" SET")
                && !script.contains("DROP INDEX")
                && !script.contains("CREATE INDEX"),
            "{script}"
        );
        succeeds(approved_apply(&d, connection, &plan, &[]));
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_class WHERE relname = 'ix_span' \
                 AND reloptions @> ARRAY['fillfactor=80']"
            ),
            1
        );
        let next = succeeds(d.run(&["plan", "--db", connection]));
        assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));

        // A collation change is a replacement: refused without
        // `--allow destructive`, applied with it and nothing more.
        let table = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, table.replace("collation: C", "collation: POSIX")).unwrap();
        d.commit();
        succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
        let refused = approved_apply(&d, connection, &plan, &[]);
        assert_ne!(
            code(&refused),
            0,
            "{}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(
            format!("{}{}", stdout(&refused), stderr(&refused)).contains("destructive"),
            "{}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_index i JOIN pg_collation c \
                   ON c.oid = i.indcollation[0] \
                  WHERE i.indexrelid = 'app.ix_label'::regclass AND c.collname = 'C'"
            ),
            1,
            "a refused apply changes nothing"
        );
        succeeds(approved_apply(
            &d,
            connection,
            &plan,
            &["--allow", "destructive"],
        ));
        assert_eq!(
            scalar(
                connection,
                "SELECT count(*) FROM pg_index i JOIN pg_collation c \
                   ON c.oid = i.indcollation[0] \
                  WHERE i.indexrelid = 'app.ix_label'::regclass AND c.collname = 'POSIX'"
            ),
            1
        );
        let next = succeeds(d.run(&["plan", "--db", connection]));
        assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
        succeeds(d.run(&["verify", "--db", connection]));
    });
}

/// A declaration the engine would refuse is refused at `validate`, by name, and
/// never reaches it: a column with no GiST class, `inet` without its class, a
/// collation on an expression, `default`, and a collation of another schema.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_declaration_the_engine_would_refuse_is_refused_by_name_before_it_reaches_it() {
    let _ = server();
    let d = Demo::new("gist-refusals");
    let write = |indexes: &str| {
        std::fs::create_dir_all(d.dir.join("schema")).unwrap();
        std::fs::write(
            d.dir.join("schema/app.t.yml"),
            format!(
                "table: app.t\ncolumns:\n  id: {{type: integer}}\n  n: {{type: integer}}\n  \
                 ip: {{type: inet}}\n  label: {{type: text}}\nindexes:\n{indexes}"
            ),
        )
        .unwrap();
    };
    for (indexes, expected) in [
        (
            "  ix:\n    method: gist\n    columns: [n]\n",
            "index an expression",
        ),
        (
            "  ix:\n    method: gist\n    columns: [ip]\n",
            "needs the class `inet_ops`",
        ),
        (
            "  ix:\n    keys:\n      - {expression: \"lower(label)\", collation: C}\n",
            "column key only",
        ),
        (
            "  ix:\n    keys:\n      - {column: label, collation: default}\n",
            "leave it out",
        ),
        (
            "  ix:\n    keys:\n      - {column: label, collation: other.mine}\n",
            "another schema",
        ),
        (
            "  ix:\n    method: gist\n    unique: true\n    keys:\n      - {expression: \"int4range(n, n)\"}\n",
            "cannot be unique",
        ),
    ] {
        write(indexes);
        let out = d.run(&["validate"]);
        assert_eq!(code(&out), 2, "{indexes}: {}{}", stdout(&out), stderr(&out));
        let said = format!("{}{}", stdout(&out), stderr(&out));
        assert!(said.contains(expected), "{indexes}: {said}");
    }
    // Control: the same table with a held index validates.
    write("  ix:\n    method: gist\n    keys:\n      - {expression: \"int4range(n, n)\"}\n");
    succeeds(d.run(&["validate"]));
}
