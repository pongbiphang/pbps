//! A BRIN index through the whole tool (#2449 PR R1, DEC-2449.1), and the
//! partitioned-parent rule of #2709: the pull writes the method and its two
//! options, the round trip ends in an empty plan, `autosummarize` alters in
//! place, `pages_per_range` replaces the index (a native `ALTER` leaves the
//! metapage at the old granularity), a drop needs `--allow destructive` and
//! nothing else, and a declaration this part does not hold is refused by name.
//! Each case runs on both pinned engines.
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

const SETUP: &str = "CREATE SCHEMA app; \
    CREATE TABLE app.reading (id integer PRIMARY KEY, taken timestamptz NOT NULL, \
      sensor integer NOT NULL, ip inet, note text, flag boolean); \
    CREATE INDEX ix_taken ON app.reading USING brin (taken) \
      WITH (pages_per_range = 16, autosummarize = on); \
    CREATE INDEX ix_plain ON app.reading USING brin (sensor); \
    CREATE INDEX ix_ip ON app.reading USING brin (ip); \
    CREATE INDEX ix_expr ON app.reading USING brin ((flag::integer), sensor) \
      WHERE note IS NOT NULL";

/// The metapage granularity an index was built with: what the index uses, not
/// what the catalog records.
fn granularity(connection: &str, index: &str) -> i64 {
    // The extension is installed for the reading only, so the pull and `verify`
    // never see a package the project did not declare.
    on_server(connection, "CREATE EXTENSION pageinspect SCHEMA public");
    let read = scalar(
        connection,
        &format!(
            "SELECT (public.brin_metapage_info(public.get_raw_page('app.{index}', 0))).pagesperrange::bigint"
        ),
    );
    on_server(connection, "DROP EXTENSION pageinspect");
    read
}

fn relfile(connection: &str, index: &str) -> i64 {
    scalar(
        connection,
        &format!("SELECT relfilenode::bigint FROM pg_class WHERE oid = 'app.{index}'::regclass"),
    )
}

#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_brin_index_round_trips_alters_autosummarize_in_place_and_rebuilds_for_its_granularity() {
    let _ = server();
    servers(|url| {
        let own = OwnDatabase::new(url, "brin-round-trip");
        let connection = own.connection();
        on_server(connection, SETUP);
        let d = Demo::new("brin-round-trip");
        succeeds(d.run(&["pull", "--db", connection]));
        let path = d.dir.join("schema/app.reading.yml");
        let table = std::fs::read_to_string(&path).unwrap();
        for held in [
            "method: brin",
            "pages_per_range: 16",
            "autosummarize: true",
            "flag",
        ] {
            assert!(table.contains(held), "the pull writes `{held}`:\n{table}");
        }
        d.commit();
        succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
        succeeds(d.run(&["validate"]));
        let next = succeeds(d.run(&["plan", "--db", connection]));
        assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
        assert_eq!(granularity(connection, "ix_taken"), 16);

        // `autosummarize` alone is the `ALTER INDEX`: the file is the same, and
        // nothing is dropped or created.
        let plan = d.dir.join("plan.json");
        let sql = d.dir.join("plan.sql");
        let before = relfile(connection, "ix_taken");
        std::fs::write(
            &path,
            table.replace("autosummarize: true", "autosummarize: false"),
        )
        .unwrap();
        d.commit();
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
            script.contains("ALTER INDEX \"app\".\"ix_taken\" SET")
                && !script.contains("DROP INDEX")
                && !script.contains("CREATE INDEX"),
            "{script}"
        );
        succeeds(approved_apply(&d, connection, &plan, &[]));
        assert_eq!(relfile(connection, "ix_taken"), before);
        let next = succeeds(d.run(&["plan", "--db", connection]));
        assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));

        // `pages_per_range` replaces the index: a native `ALTER` would record 4
        // and leave the index at 16, so the plan drops and builds it again, and
        // the index uses the declared granularity afterwards. A drop is an
        // ordinary index drop: `--allow destructive` and no reason.
        let table = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            table.replace("pages_per_range: 16", "pages_per_range: 4"),
        )
        .unwrap();
        d.commit();
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
            script.contains("DROP INDEX") && script.contains("CREATE INDEX"),
            "{script}"
        );
        assert!(!script.contains("ALTER INDEX"), "{script}");
        let refused = approved_apply(&d, connection, &plan, &[]);
        assert_ne!(code(&refused), 0, "{}", said(&refused));
        assert!(said(&refused).contains("destructive"), "{}", said(&refused));
        assert_eq!(granularity(connection, "ix_taken"), 16, "nothing applied");
        succeeds(approved_apply(
            &d,
            connection,
            &plan,
            &["--allow", "destructive"],
        ));
        assert_eq!(granularity(connection, "ix_taken"), 4);
        let next = succeeds(d.run(&["plan", "--db", connection]));
        assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));

        // Removing the option is the same rebuild, back to the engine's default.
        let table = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, {
            let removed = table.replace(", pages_per_range: 4", "");
            assert_ne!(removed, table, "{table}");
            removed
        })
        .unwrap();
        d.commit();
        succeeds(d.run(&["plan", "--db", connection, "--out", plan.to_str().unwrap()]));
        succeeds(approved_apply(
            &d,
            connection,
            &plan,
            &["--allow", "destructive"],
        ));
        assert_eq!(granularity(connection, "ix_taken"), 128);
        let next = succeeds(d.run(&["plan", "--db", connection]));
        assert!(stdout(&next).contains("No changes"), "{}", stdout(&next));
        succeeds(d.run(&["verify", "--db", connection]));
    });
}

/// A declaration the engine would refuse, or this part does not hold, is refused
/// at `validate` by name and never reaches the engine.
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn a_brin_declaration_the_engine_or_this_part_does_not_hold_is_refused_by_name() {
    let _ = server();
    let d = Demo::new("brin-refusals");
    let write = |indexes: &str| {
        std::fs::create_dir_all(d.dir.join("schema")).unwrap();
        std::fs::write(
            d.dir.join("schema/app.t.yml"),
            format!(
                "table: app.t\ncolumns:\n  id: {{type: integer}}\n  n: {{type: integer}}\n  \
                 doc: {{type: tsvector}}\n  flag: {{type: boolean}}\n  label: {{type: text}}\n\
                 indexes:\n{indexes}"
            ),
        )
        .unwrap();
    };
    for (indexes, expected) in [
        (
            "  ix:\n    method: brin\n    columns: [doc]\n",
            "index an expression",
        ),
        (
            "  ix:\n    method: brin\n    columns: [flag]\n",
            "index an expression",
        ),
        (
            "  ix:\n    method: brin\n    unique: true\n    columns: [n]\n",
            "cannot be unique",
        ),
        (
            "  ix:\n    method: brin\n    include: [label]\n    columns: [n]\n",
            "cannot include",
        ),
        (
            "  ix:\n    method: brin\n    keys:\n      - {column: n, order: desc}\n",
            "has no order",
        ),
        (
            "  ix:\n    method: brin\n    keys:\n      - {column: n, opclass: int4_minmax_ops}\n",
            "leave the class out",
        ),
        (
            "  ix:\n    method: brin\n    keys:\n      - {column: n, opclass: int4_bloom_ops}\n",
            "next part of #2449",
        ),
        (
            "  ix:\n    method: brin\n    keys:\n      - {column: n, opclass: int4_minmax_multi_ops}\n",
            "next part of #2449",
        ),
        (
            "  ix:\n    method: brin\n    keys:\n      - column: n\n        opclass_parameters: {values_per_range: 8}\n",
            "opclass_parameters",
        ),
        (
            "  ix:\n    method: brin\n    keys:\n      - {column: label, collation: C}\n",
            "no collation",
        ),
        (
            "  ix:\n    method: brin\n    columns: [n]\n    storage_parameters: {pages_per_range: 0}\n",
            "pages_per_range",
        ),
        (
            "  ix:\n    method: brin\n    columns: [n]\n    storage_parameters: {pages_per_range: 131073}\n",
            "pages_per_range",
        ),
        (
            "  ix:\n    method: brin\n    columns: [n]\n    storage_parameters: {fillfactor: 70}\n",
            "fillfactor",
        ),
        (
            "  ix:\n    method: brin\n    columns: [n]\n    storage_parameters: {values_per_range: 8}\n",
            "values_per_range",
        ),
    ] {
        write(indexes);
        let out = d.run(&["validate"]);
        assert_eq!(code(&out), 2, "{indexes}: {}", said(&out));
        assert!(said(&out).contains(expected), "{indexes}: {}", said(&out));
    }
    // Control: held shapes validate: a column of a type with a default class, an
    // expression over a type with none, several keys, a predicate, both options.
    write(
        "  ix:\n    method: brin\n    columns: [n, label]\n    where: n > 0\n    \
         storage_parameters: {pages_per_range: 32, autosummarize: true}\n  \
         iy:\n    method: brin\n    keys:\n      - {expression: \"(flag::integer)\"}\n",
    );
    succeeds(d.run(&["validate"]));
}

/// #2709: a partitioned parent's index takes no `ALTER INDEX SET` of any method,
/// so an option change on it is the index dropped and built again, and the
/// leaves carry the declared value; the plan then applies and the next one is
/// empty. An ordinary table's index still alters in place (the leaf's own
/// would too, and is not declared here).
#[test]
#[ignore = "needs PostgreSQL 18 and 16; set PBPS_TEST_PG_DB and PBPS_TEST_PG_OLD_DB"]
fn an_option_change_on_a_partitioned_parents_index_rebuilds_it_and_its_leaves_carry_the_value() {
    let _ = server();
    servers(|url| {
        for (method, option, before, after, reloption) in [
            ("btree", "fillfactor", "70", "80", "fillfactor=80"),
            (
                "brin",
                "autosummarize",
                "true",
                "false",
                "autosummarize=false",
            ),
        ] {
            let own = OwnDatabase::new(url, &format!("parent-option-{method}"));
            let connection = own.connection();
            let (using, setting) = if method == "brin" {
                ("USING brin", "autosummarize = on")
            } else {
                ("", "fillfactor = 70")
            };
            on_server(
                connection,
                &format!(
                    "CREATE SCHEMA app; \
                     CREATE TABLE app.m (id integer NOT NULL, v integer NOT NULL) \
                       PARTITION BY RANGE (id); \
                     CREATE TABLE app.m1 PARTITION OF app.m FOR VALUES FROM (0) TO (100); \
                     CREATE TABLE app.m2 PARTITION OF app.m FOR VALUES FROM (100) TO (200); \
                     CREATE INDEX ix_v ON app.m {using} (v) WITH ({setting})"
                ),
            );
            let d = Demo::new(&format!("parent-option-{method}"));
            succeeds(d.run(&["pull", "--db", connection]));
            let path = d.dir.join("schema/app.m.yml");
            let table = std::fs::read_to_string(&path).unwrap();
            assert!(table.contains(&format!("{option}: {before}")), "{table}");
            d.commit();
            succeeds(d.run(&["baseline", "--db", connection, "--reason", "adopt"]));
            let next = succeeds(d.run(&["plan", "--db", connection]));
            assert!(
                stdout(&next).contains("No changes"),
                "{method}: {}",
                stdout(&next)
            );

            std::fs::write(
                &path,
                table.replace(
                    &format!("{option}: {before}"),
                    &format!("{option}: {after}"),
                ),
            )
            .unwrap();
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
                script.contains("DROP INDEX")
                    && script.contains("CREATE INDEX")
                    && !script.contains("ALTER INDEX"),
                "{method}: {script}"
            );
            succeeds(approved_apply(
                &d,
                connection,
                &plan,
                &["--allow", "destructive"],
            ));
            for leaf in ["m1", "m2"] {
                assert_eq!(
                    scalar(
                        connection,
                        &format!(
                            "SELECT count(*) FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
                              WHERE i.indrelid = 'app.{leaf}'::regclass \
                                AND c.reloptions @> ARRAY['{reloption}']"
                        )
                    ),
                    1,
                    "{method}: leaf {leaf} carries {reloption}"
                );
            }
            let next = succeeds(d.run(&["plan", "--db", connection]));
            assert!(
                stdout(&next).contains("No changes"),
                "{method}: {}",
                stdout(&next)
            );
            succeeds(d.run(&["verify", "--db", connection]));
        }
    });
}
