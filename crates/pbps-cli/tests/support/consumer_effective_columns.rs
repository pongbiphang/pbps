//! Connected evidence for the private consumer value interface (DEC-2484.1).
use super::*;

fn engines(mut run: impl FnMut(&str)) {
    for variable in ["PBPS_TEST_PG_DB", "PBPS_TEST_PG_OLD_DB"] {
        run(&std::env::var(variable).unwrap());
    }
}
fn config(d: &Demo, contract: Option<(&str, bool)>) {
    let declaration = contract.map_or(String::new(), |(entry, writes)| {
        format!(
            "consumers:\n  client: {{{}: [{entry}]}}\n",
            if writes { "writes" } else { "uses" }
        )
    });
    std::fs::write(
        d.dir.join("pbps.yml"),
        format!("dialect: postgres\n{declaration}"),
    )
    .unwrap();
}
fn domain(d: &Demo, name: &str, base: &str, attributes: &str) {
    std::fs::write(
        d.dir.join(format!("schema/{name}.yml")),
        format!("domain: app.{name}\ntype: '{base}'\n{attributes}"),
    )
    .unwrap();
}
fn setup(d: &Demo, connection: &str) {
    config(d, None);
    succeeds(d.run(&["plan"]));
    d.commit();
    on_server(connection, "CREATE SCHEMA app");
    succeeds(d.run(&["bootstrap", "--db", connection]));
}
fn check(d: &Demo, connection: &str, expected: i32) -> Option<pbps_model::SavedPlan> {
    let path = d.dir.join("effective.json");
    if path.exists() {
        std::fs::remove_file(&path).unwrap();
    }
    let out = d.run(&[
        "plan",
        "--db",
        connection,
        "--format",
        "json",
        "--out",
        path.to_str().unwrap(),
    ]);
    assert_eq!(code(&out), expected, "{}{}", stdout(&out), stderr(&out));
    if expected == 2 {
        let report = json_output(out);
        assert!(
            report["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["id"] == "consumer.unproven"),
            "{report}"
        );
        assert!(!path.exists(), "a refused plan must not be published");
        None
    } else {
        Some(serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap())
    }
}
fn apply(d: &Demo, connection: &str) {
    let path = d.dir.join("effective.json");
    succeeds(d.run(&[
        "apply",
        "--db",
        connection,
        "--plan",
        path.to_str().unwrap(),
        "--checksum",
        &plan_checksum(&path),
        "--allow",
        "type-rebuild,not-null,constraint,narrowing,destructive",
    ]));
    succeeds(d.run(&["verify", "--db", connection]));
}
fn rejects_null(connection: &str, statement: &str) {
    // Catch the engine's actual 23502, not text containing the input's NULL.
    on_server(
        connection,
        &format!(
            "DO $case$ BEGIN {statement}; RAISE EXCEPTION 'expected not_null_violation'; EXCEPTION WHEN not_null_violation THEN NULL; END $case$"
        ),
    );
}
fn nullable_write(connection: &str, statement: &str, accepts: bool) {
    if accepts {
        on_server(connection, &format!("BEGIN; {statement}; ROLLBACK"));
    } else {
        rejects_null(connection, statement);
    }
}

#[test]
#[ignore = "needs PostgreSQL 16 and 18"]
fn ordinary_edits_use_effective_attributes_and_keep_real_change_controls() {
    engines(|server| {
        // Literal outcomes (reader, writer), including both R7 false refusals.
        for (nn, inherited, before, after, expected, nulls) in [
            (true, "", "", ", nullable: false", [0, 0], [false, false]),
            (false, "", "", ", nullable: false", [0, 2], [true, false]),
            (true, "", ", nullable: false", "", [0, 0], [false, false]),
            (false, "", ", nullable: false", "", [2, 0], [false, true]),
            (
                false,
                "default: '1'\n",
                ", default: '1'",
                "",
                [0, 0],
                [true, true],
            ),
            (
                false,
                "default: '1'\n",
                ", default: '2'",
                "",
                [0, 2],
                [true, true],
            ),
            (
                false,
                "default: '1'\n",
                "",
                ", default: '1'",
                [0, 0],
                [true, true],
            ),
            (
                false,
                "default: '1'\n",
                "",
                ", default: '2'",
                [0, 2],
                [true, true],
            ),
        ] {
            let own = OwnDatabase::new(server, "effective_ordinary2484");
            let connection = own.connection();
            let d = Demo::new("effective-ordinary");
            domain(&d, "d", "integer", &format!("not_null: {nn}\n{inherited}"));
            d.table(&format!(
                "table: app.t\ncolumns:\n  v: {{type: app.d{before}}}\n"
            ));
            setup(&d, connection);
            nullable_write(connection, "INSERT INTO app.t(v) VALUES(NULL)", nulls[0]);
            d.table(&format!(
                "table: app.t\ncolumns:\n  v: {{type: app.d{after}}}\n"
            ));
            for (writes, outcome) in [false, true].into_iter().zip(expected) {
                config(&d, Some(("app.t.v", writes)));
                check(&d, connection, outcome);
            }
            config(&d, None);
            check(&d, connection, 0);
            apply(&d, connection);
            nullable_write(connection, "INSERT INTO app.t(v) VALUES(NULL)", nulls[1]);
        }
        for (override_c, expected) in [(false, [2, 2]), (true, [0, 0])] {
            let own = OwnDatabase::new(server, "effective_collation2484");
            let c = own.connection();
            let d = Demo::new("effective-collation");
            domain(&d, "d", "text", "collation: pg_catalog.\"C\"\n");
            // PostgreSQL declarations express this override on a scalar
            // domain; explicit column collation is the SQL Server unit case.
            domain(
                &d,
                "outer_d",
                "app.d",
                "collation: pg_catalog.\"default\"\n",
            );
            d.table(&format!(
                "table: app.t\ncolumns:\n  v: {{type: app.{}}}\n",
                if override_c { "outer_d" } else { "d" }
            ));
            setup(&d, c);
            domain(&d, "d", "text", "collation: pg_catalog.\"POSIX\"\n");
            for (writes, outcome) in [false, true].into_iter().zip(expected) {
                config(&d, Some(("app.t.v", writes)));
                check(&d, c, outcome);
            }
        }
    });
}

#[test]
#[ignore = "needs PostgreSQL 16 and 18"]
fn required_domain_columns_need_a_whole_value_supply_for_existing_writers() {
    engines(|server| {
        for (nn, inherited, explicit, expected) in [
            (true, "", "", 2),
            (false, "", "", 0),
            (true, "default: '1'\n", "", 0),
            (true, "", ", default: '1'", 0),
        ] {
            let own = OwnDatabase::new(server, "effective_added2484");
            let c = own.connection();
            let d = Demo::new("effective-added");
            domain(&d, "d", "integer", &format!("not_null: {nn}\n{inherited}"));
            d.table("table: app.t\ncolumns:\n  v: {type: integer}\n");
            setup(&d, c);
            on_server(c, "BEGIN; INSERT INTO app.t(v) VALUES(1); ROLLBACK");
            d.table(&format!("table: app.t\ncolumns:\n  v: {{type: integer}}\n  added: {{type: app.d{explicit}}}\n"));
            // A deployment must use the new column identity reviewed in git.
            succeeds(d.run(&["plan"]));
            d.commit();
            config(&d, Some(("app.t", true)));
            check(&d, c, expected);
            config(&d, None);
            check(&d, c, 0);
            apply(&d, c);
            if expected == 2 {
                rejects_null(c, "INSERT INTO app.t(v) VALUES(1)");
            } else {
                on_server(c, "INSERT INTO app.t(v) VALUES(1)");
            }
        }
    });
}

#[test]
#[ignore = "needs PostgreSQL 16 and 18"]
fn element_contexts_are_protected_through_connected_domain_rebuilds() {
    engines(|server| {
        // Base widening forces the loosening through a real rebuild too.
        // A surviving inner NOT NULL is the unchanged-interface control.
        for (ty, inner, was, now, nullable, writes, expected, element_null) in [
            ("app.d[]", false, false, true, true, true, 2, [true, false]),
            (
                "app.d[]",
                false,
                true,
                false,
                false,
                false,
                2,
                [false, true],
            ),
            (
                "app.d[]",
                true,
                true,
                false,
                false,
                false,
                0,
                [false, false],
            ),
            (
                "app.nested",
                false,
                false,
                true,
                true,
                true,
                2,
                [true, false],
            ),
        ] {
            let own = OwnDatabase::new(server, "effective_elements2484");
            let c = own.connection();
            let d = Demo::new("effective-elements");
            domain(&d, "inner_d", "integer", &format!("not_null: {inner}\n"));
            domain(&d, "d", "app.inner_d", &format!("not_null: {was}\n"));
            domain(&d, "array_d", "app.d[]", "not_null: true\n");
            domain(&d, "nested", "app.array_d[]", "");
            d.table(&format!(
                "table: app.t\ncolumns:\n  v: {{type: '{ty}', nullable: {nullable}}}\n"
            ));
            setup(&d, c);
            let null_element = if ty == "app.nested" {
                "INSERT INTO app.t(v) VALUES(ARRAY[ARRAY[NULL::app.d]::app.array_d])"
            } else {
                "INSERT INTO app.t(v) VALUES(ARRAY[NULL::app.d])"
            };
            nullable_write(c, "INSERT INTO app.t(v) VALUES(NULL)", nullable);
            nullable_write(c, null_element, element_null[0]);
            domain(&d, "inner_d", "bigint", &format!("not_null: {inner}\n"));
            domain(&d, "d", "app.inner_d", &format!("not_null: {now}\n"));
            config(&d, Some(("app.t.v", writes)));
            check(&d, c, expected);
            config(&d, None);
            let saved = check(&d, c, 0).unwrap();
            assert!(
                serde_json::to_string(&saved)
                    .unwrap()
                    .contains("enum_rebuild"),
                "must exercise the domain rebuild path"
            );
            apply(&d, c);
            nullable_write(c, "INSERT INTO app.t(v) VALUES(NULL)", nullable);
            nullable_write(c, null_element, element_null[1]);
        }
        let own = OwnDatabase::new(server, "effective_check2484");
        let c = own.connection();
        let d = Demo::new("effective-check");
        domain(&d, "d", "integer", "");
        d.table("table: app.t\ncolumns:\n  v: {type: 'app.d[]'}\n");
        setup(&d, c);
        on_server(
            c,
            "BEGIN; INSERT INTO app.t(v) VALUES(ARRAY[-1::app.d]); ROLLBACK",
        );
        domain(&d, "d", "bigint", "checks:\n  positive: VALUE > 0\n");
        for (writes, expected) in [(false, 0), (true, 2)] {
            config(&d, Some(("app.t.v", writes)));
            check(&d, c, expected);
        }
        config(&d, None);
        let saved = check(&d, c, 0).unwrap();
        assert!(
            serde_json::to_string(&saved)
                .unwrap()
                .contains("enum_rebuild")
        );
        apply(&d, c);
        on_server(
            c,
            "DO $check$ BEGIN INSERT INTO app.t(v) VALUES(ARRAY[-1::app.d]); RAISE EXCEPTION 'expected check_violation'; EXCEPTION WHEN check_violation THEN NULL; END $check$",
        );
        // A CHECK is opaque: do not turn it into element NOT NULL.
        on_server(c, "INSERT INTO app.t(v) VALUES(ARRAY[NULL::app.d])");
    });
}

#[test]
#[ignore = "needs PostgreSQL 16 and 18"]
fn partition_overrides_keep_standing_interfaces_and_refuse_real_tightening() {
    engines(|server| {
        for (parent_required, expected) in [(true, 0), (false, 2)] {
            let own = OwnDatabase::new(server, "effective_partition2484");
            let c = own.connection();
            let d = Demo::new("effective-partition");
            let tree = |parent, local: &str| {
                format!(
                    "table: app.t\ncolumns:\n  id: {{type: integer}}\n  v: {{type: integer, nullable: {parent}}}\npartition_by: [id]\npartitions:\n  leaf:\n    default: true\n{local}"
                )
            };
            d.table(&tree(!parent_required, ""));
            setup(&d, c);
            nullable_write(
                c,
                "INSERT INTO app.leaf(id,v) VALUES(1,NULL)",
                !parent_required,
            );
            d.table(&tree(true, "    columns:\n      v: {nullable: false}\n"));
            config(&d, Some(("app.leaf.v", true)));
            check(&d, c, expected);
            config(&d, None);
            check(&d, c, 0);
            apply(&d, c);
            rejects_null(c, "INSERT INTO app.leaf(id,v) VALUES(1,NULL)");
        }
        for (remove_local, expected) in [(false, 0), (true, 2)] {
            let own = OwnDatabase::new(server, "effective_partdefault2484");
            let c = own.connection();
            let d = Demo::new("effective-partdefault");
            let tree = |parent, own: &str| {
                format!(
                    "table: app.t\ncolumns:\n  id: {{type: integer}}\n  v: {{type: integer, default: '{parent}'}}\npartition_by: [id]\npartitions:\n  leaf:\n    default: true\n{own}"
                )
            };
            let local = "    columns:\n      v: {default: '2'}\n";
            d.table(&tree("1", local));
            setup(&d, c);
            // Redundant local defaults are rejected declarations. Keep a real
            // override while its parent changes for the unchanged control.
            d.table(&tree(
                if remove_local { "1" } else { "3" },
                if remove_local { "" } else { local },
            ));
            config(&d, Some(("app.leaf.v", true)));
            check(&d, c, expected);
        }
    });
}
