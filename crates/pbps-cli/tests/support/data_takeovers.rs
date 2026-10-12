//! Reference-data scope changes reach the ordinary saved-plan gate (DEC-1803.1, DEC-1808.1).
use super::{Demo, OwnDatabase, code, on_server, stderr, stdout};
use pbps_model::{Change, DataMode, SavedPlan};
use std::process::Output;

fn ok(out: Output) -> Output {
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    out
}

fn schema(pg: bool) -> &'static str {
    if pg { "app" } else { "dbo" }
}

fn prepare(d: &Demo, connection: &str, pg: bool, key_only: bool, empty: bool) {
    if pg {
        on_server(connection, "CREATE SCHEMA app");
    }
    let label = if key_only {
        ""
    } else {
        ", label varchar(40) NOT NULL"
    };
    on_server(
        connection,
        &format!(
            "CREATE TABLE {}.country (code varchar(2) NOT NULL{label}, CONSTRAINT pk_country PRIMARY KEY (code))",
            schema(pg)
        ),
    );
    if !empty {
        let values = if key_only {
            "('TW')"
        } else {
            "('TW', 'Taiwan')"
        };
        on_server(
            connection,
            &format!("INSERT INTO {}.country VALUES {values}", schema(pg)),
        );
    }
    ok(d.run(&["pull", "--db", connection]));
    d.commit();
    ok(d.run(&[
        "baseline",
        "--db",
        connection,
        "--reason",
        "adopt the existing schema",
    ]));
}

fn declare(d: &Demo, pg: bool, mode: &str, key_only: bool, empty: bool) {
    let path = d.dir.join(format!("schema/{}.country.yml", schema(pg)));
    let mut body = std::fs::read_to_string(&path).unwrap();
    let rows = if empty {
        "{}"
    } else if key_only {
        "\n    TW: {}"
    } else {
        "\n    TW: {label: Taiwan}"
    };
    body.push_str(&format!("\ndata:\n  mode: {mode}\n  rows: {rows}\n"));
    std::fs::write(path, body).unwrap();
}

fn plan(d: &Demo, connection: &str) -> SavedPlan {
    let path = d.dir.join("plan.json");
    ok(d.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()]));
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn apply(d: &Demo, connection: &str, checksum: &str, allow: bool) -> Output {
    let path = d.dir.join("plan.json");
    let mut args = vec![
        "apply",
        "--db",
        connection,
        "--plan",
        path.to_str().unwrap(),
        "--checksum",
        checksum,
    ];
    if allow {
        args.extend(["--allow", "rename"]);
    }
    d.run(&args)
}

fn history(d: &Demo, connection: &str) -> serde_json::Value {
    let out = ok(d.run(&["state", "list", "--db", connection, "--format", "json"]));
    serde_json::from_str::<serde_json::Value>(&stdout(&out)).unwrap()["data"]["entries"].clone()
}

fn assert_takeover(saved: &SavedPlan, table: &str, mode: DataMode) {
    let takeovers: Vec<_> = saved
        .changes
        .changes
        .iter()
        .filter_map(|planned| {
            if let Change::SetDataMode { table, from, to } = &planned.change {
                Some((table.to_string(), *from, *to))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(takeovers, vec![(table.to_owned(), None, Some(mode))]);
}

pub fn matching(server: &str, pg: bool) {
    for (mode, key_only, empty) in [
        ("exact", false, false),
        ("ensure", false, false),
        ("ensure", true, false),
        ("exact", false, true),
    ] {
        let slug = format!("takeover_{mode}_{key_only}_{empty}");
        let own = OwnDatabase::new(server, &slug);
        let connection = own.connection();
        let d = Demo::new(&slug);
        prepare(&d, connection, pg, key_only, empty);
        declare(&d, pg, mode, key_only, empty);
        let table = format!("{}.country", schema(pg));
        let before = history(&d, connection);
        let saved = plan(&d, connection);
        assert_takeover(
            &saved,
            &table,
            if mode == "exact" {
                DataMode::Exact
            } else {
                DataMode::Ensure
            },
        );
        // This is a recording-only plan: no row or schema change hides the
        // failure to carry an otherwise empty takeover into apply.
        assert_eq!(saved.changes.changes.len(), 1);
        ok(apply(&d, connection, &saved.checksum(), false));
        let after = history(&d, connection);
        assert_eq!(
            after.as_array().unwrap().len(),
            before.as_array().unwrap().len() + 1
        );
        assert_eq!(after[0]["kind"], "apply");
        ok(d.run(&["verify", "--db", connection]));
        let noop = plan(&d, connection);
        assert!(noop.changes.is_empty());
        ok(apply(&d, connection, &noop.checksum(), false));
        assert_eq!(history(&d, connection), after);
        if mode == "ensure" {
            let values = if key_only {
                "('JP')"
            } else {
                "('JP', 'Japan')"
            };
            on_server(connection, &format!("INSERT INTO {table} VALUES {values}"));
            ok(d.run(&["verify", "--db", connection]));
        }
        let drift = if empty {
            format!("INSERT INTO {table} VALUES ('TW', 'Taiwan')")
        } else if key_only {
            format!("DELETE FROM {table} WHERE code = 'TW'")
        } else {
            format!("UPDATE {table} SET label = 'Changed' WHERE code = 'TW'")
        };
        on_server(connection, &drift);
        let out = d.run(&["verify", "--db", connection]);
        assert_eq!(code(&out), 2, "{}{}", stdout(&out), stderr(&out));
        assert_eq!(history(&d, connection), after);
    }
}

pub fn refusals(server: &str, pg: bool) {
    let own = OwnDatabase::new(server, "takeover_refusals");
    let connection = own.connection();
    let d = Demo::new("takeover-refusals");
    prepare(&d, connection, pg, false, false);
    declare(&d, pg, "exact", false, false);
    let before = history(&d, connection);
    let saved = plan(&d, connection);
    assert_takeover(&saved, &format!("{}.country", schema(pg)), DataMode::Exact);
    let out = apply(&d, connection, &"0".repeat(64), false);
    assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
    assert!(stderr(&out).contains("checksum"), "{}", stderr(&out));
    assert_eq!(history(&d, connection), before);
    on_server(
        connection,
        &format!(
            "UPDATE {}.country SET label = 'Changed' WHERE code = 'TW'",
            schema(pg)
        ),
    );
    let out = apply(&d, connection, &saved.checksum(), false);
    assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
    assert!(stderr(&out).contains("baseline"), "{}", stderr(&out));
    let failed = history(&d, connection);
    assert_eq!(failed[0]["kind"], "failed");
    assert_eq!(&failed.as_array().unwrap()[1..], before.as_array().unwrap());
    // The ordinary failed-attempt audit entry must not adopt the new scope:
    // changed rows are still outside the last successful recorded state.
    ok(d.run(&["verify", "--db", connection]));
    // Restoring the pinned rows makes the same artifact applicable; neither
    // refusal has secretly recorded the new scope or consumed the plan.
    on_server(
        connection,
        &format!(
            "UPDATE {}.country SET label = 'Taiwan' WHERE code = 'TW'",
            schema(pg)
        ),
    );
    ok(apply(&d, connection, &saved.checksum(), false));
    ok(d.run(&["verify", "--db", connection]));
}

pub fn renamed(server: &str, pg: bool) {
    let own = OwnDatabase::new(server, "takeover_rename");
    let connection = own.connection();
    let d = Demo::new("takeover-rename");
    prepare(&d, connection, pg, false, false);
    declare(&d, pg, "exact", false, false);
    let old = format!("{}.country", schema(pg));
    let new = format!("{}.countries", schema(pg));
    let old_path = d.dir.join(format!("schema/{old}.yml"));
    let body = std::fs::read_to_string(&old_path)
        .unwrap()
        .replace(&old, &new);
    std::fs::remove_file(old_path).unwrap();
    std::fs::write(d.dir.join(format!("schema/{new}.yml")), body).unwrap();
    ok(d.run(&["rename-table", &old, &new]));
    d.commit();
    let before = history(&d, connection);
    let saved = plan(&d, connection);
    assert_takeover(&saved, &new, DataMode::Exact);
    assert_eq!(saved.changes.changes.len(), 2);
    let out = apply(&d, connection, &saved.checksum(), false);
    assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
    assert!(
        stderr(&out).contains("risks that were not approved: rename"),
        "{}",
        stderr(&out)
    );
    assert_eq!(history(&d, connection), before);
    ok(apply(&d, connection, &saved.checksum(), true));
    ok(d.run(&["verify", "--db", connection]));
    assert!(plan(&d, connection).changes.is_empty());
    on_server(
        connection,
        &format!("UPDATE {new} SET label = 'Changed' WHERE code = 'TW'"),
    );
    let out = d.run(&["verify", "--db", connection]);
    assert_eq!(code(&out), 2, "{}{}", stdout(&out), stderr(&out));
}

fn ensure_rows(d: &Demo, table: &str, rows: &str) {
    let path = d.dir.join(format!("schema/{table}.yml"));
    let body = std::fs::read_to_string(&path).unwrap();
    let schema = body.split("\ndata:").next().unwrap();
    std::fs::write(
        path,
        format!("{schema}\ndata:\n  mode: ensure\n  rows: {rows}\n"),
    )
    .unwrap();
}

fn managed_ensure(d: &Demo, connection: &str, pg: bool) -> String {
    prepare(d, connection, pg, false, false);
    declare(d, pg, "ensure", false, false);
    let saved = plan(d, connection);
    ok(apply(d, connection, &saved.checksum(), false));
    format!("{}.country", schema(pg))
}

fn assert_keys(saved: &SavedPlan, table: &str, from: &[&str], to: &[&str]) {
    let changes: Vec<_> = saved
        .changes
        .changes
        .iter()
        .filter_map(|p| {
            if let Change::SetEnsureKeys { table, from, to } = &p.change {
                Some((table.to_string(), from.clone(), to.clone()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        changes,
        vec![(
            table.to_owned(),
            from.iter().copied().map(Into::into).collect(),
            to.iter().copied().map(Into::into).collect()
        )]
    );
}

fn drift(d: &Demo, connection: &str) {
    let out = d.run(&["verify", "--db", connection, "--format", "json"]);
    assert_eq!(code(&out), 2, "{}{}", stdout(&out), stderr(&out));
    assert!(stdout(&out).contains("state.drift"), "{}", stdout(&out));
}

pub fn ensure_scope(server: &str, pg: bool) {
    let own = OwnDatabase::new(server, "ensure_scope");
    let connection = own.connection();
    let d = Demo::new("ensure-scope");
    let table = managed_ensure(&d, connection, pg);
    on_server(
        connection,
        &format!("INSERT INTO {table} VALUES ('JP', 'Japan')"),
    );
    ok(d.run(&["verify", "--db", connection]));
    ensure_rows(
        &d,
        &table,
        "\n    TW: {label: Taiwan}\n    JP: {label: Japan}",
    );
    let saved = plan(&d, connection);
    assert_keys(&saved, &table, &["TW"], &["TW", "JP"]);
    assert_eq!(saved.changes.changes.len(), 1);
    let before = history(&d, connection);
    ok(apply(&d, connection, &saved.checksum(), false));
    let recorded = history(&d, connection);
    assert_eq!(
        recorded.as_array().unwrap().len(),
        before.as_array().unwrap().len() + 1
    );
    assert_eq!(recorded[0]["kind"], "apply");
    on_server(
        connection,
        &format!("UPDATE {table} SET label = 'Changed' WHERE code = 'JP'"),
    );
    drift(&d, connection);
    on_server(
        connection,
        &format!("UPDATE {table} SET label = 'Japan' WHERE code = 'JP'"),
    );
    let noop = plan(&d, connection);
    assert!(noop.changes.is_empty());
    ok(apply(&d, connection, &noop.checksum(), false));
    assert_eq!(history(&d, connection), recorded);

    ensure_rows(&d, &table, "\n    JP: {label: Japan}");
    let saved = plan(&d, connection);
    assert_keys(&saved, &table, &["TW", "JP"], &["JP"]);
    assert_eq!(saved.changes.changes.len(), 1);
    ok(apply(&d, connection, &saved.checksum(), false));
    on_server(
        connection,
        &format!("UPDATE {table} SET label = 'Changed' WHERE code = 'TW'"),
    );
    ok(d.run(&["verify", "--db", connection]));
    on_server(
        connection,
        &format!("UPDATE {table} SET label = 'Changed' WHERE code = 'JP'"),
    );
    drift(&d, connection);
    on_server(
        connection,
        &format!("UPDATE {table} SET label = 'Japan' WHERE code = 'JP'"),
    );

    ensure_rows(&d, &table, "{}");
    let saved = plan(&d, connection);
    assert_keys(&saved, &table, &["JP"], &[]);
    assert_eq!(saved.changes.changes.len(), 1);
    ok(apply(&d, connection, &saved.checksum(), false));
    on_server(
        connection,
        &format!("UPDATE {table} SET label = 'Changed' WHERE code = 'JP'"),
    );
    ok(d.run(&["verify", "--db", connection]));
    assert!(plan(&d, connection).changes.is_empty());
    // Re-adopting both rows must require metadata only: removal did not
    // delete either live row, even when the last key left the scope.
    ensure_rows(
        &d,
        &table,
        "\n    TW: {label: Changed}\n    JP: {label: Changed}",
    );
    let saved = plan(&d, connection);
    assert_keys(&saved, &table, &[], &["TW", "JP"]);
    assert_eq!(saved.changes.changes.len(), 1);
    ok(apply(&d, connection, &saved.checksum(), false));
    ok(d.run(&["verify", "--db", connection]));
}

pub fn ensure_refusals(server: &str, pg: bool) {
    let own = OwnDatabase::new(server, "ensure_refusals");
    let connection = own.connection();
    let d = Demo::new("ensure-refusals");
    let table = managed_ensure(&d, connection, pg);
    on_server(
        connection,
        &format!("INSERT INTO {table} VALUES ('JP', 'Japan')"),
    );
    ensure_rows(
        &d,
        &table,
        "\n    TW: {label: Taiwan}\n    JP: {label: Japan}",
    );
    let saved = plan(&d, connection);
    assert_keys(&saved, &table, &["TW"], &["TW", "JP"]);
    let before = history(&d, connection);
    let refused = apply(&d, connection, &"0".repeat(64), false);
    assert_eq!(code(&refused), 1);
    assert!(stderr(&refused).contains("checksum"));
    assert_eq!(history(&d, connection), before);
    on_server(
        connection,
        &format!("UPDATE {table} SET label = 'Changed' WHERE code = 'JP'"),
    );
    let refused = apply(&d, connection, &saved.checksum(), false);
    assert_eq!(code(&refused), 1);
    assert!(
        stderr(&refused).contains("baseline"),
        "{}",
        stderr(&refused)
    );
    let failed = history(&d, connection);
    assert_eq!(failed[0]["kind"], "failed");
    assert_eq!(&failed.as_array().unwrap()[1..], before.as_array().unwrap());
    ok(d.run(&["verify", "--db", connection]));
    on_server(
        connection,
        &format!("UPDATE {table} SET label = 'Japan' WHERE code = 'JP'"),
    );
    ok(apply(&d, connection, &saved.checksum(), false));
    // A removed key is still part of the union pinned by this plan.
    ensure_rows(&d, &table, "\n    TW: {label: Taiwan}");
    let saved = plan(&d, connection);
    on_server(
        connection,
        &format!("UPDATE {table} SET label = 'Changed' WHERE code = 'JP'"),
    );
    let refused = apply(&d, connection, &saved.checksum(), false);
    assert_eq!(code(&refused), 1);
    assert!(
        stderr(&refused).contains("baseline"),
        "{}",
        stderr(&refused)
    );
    drift(&d, connection);
    on_server(
        connection,
        &format!("UPDATE {table} SET label = 'Japan' WHERE code = 'JP'"),
    );
    ok(apply(&d, connection, &saved.checksum(), false));
}

pub fn ensure_writes_and_rename(server: &str, pg: bool) {
    let own = OwnDatabase::new(server, "ensure_writes");
    let connection = own.connection();
    let d = Demo::new("ensure-writes");
    let table = managed_ensure(&d, connection, pg);
    ensure_rows(
        &d,
        &table,
        "\n    TW: {label: Taiwan}\n    JP: {label: Japan}",
    );
    let saved = plan(&d, connection);
    assert_keys(&saved, &table, &["TW"], &["TW", "JP"]);
    assert_eq!(saved.changes.changes.len(), 2);
    assert!(
        saved
            .changes
            .changes
            .iter()
            .any(|p| matches!(&p.change, Change::InsertRow { .. }))
    );
    ok(apply(&d, connection, &saved.checksum(), false));
    ensure_rows(
        &d,
        &table,
        "\n    TW: {label: Taiwan}\n    JP: {label: Nippon}",
    );
    let saved = plan(&d, connection);
    assert_eq!(saved.changes.changes.len(), 1);
    assert!(matches!(
        &saved.changes.changes[0].change,
        Change::UpdateRow { .. }
    ));
    let path = d.dir.join("plan.json");
    ok(d.run(&[
        "apply",
        "--db",
        connection,
        "--plan",
        path.to_str().unwrap(),
        "--checksum",
        &saved.checksum(),
        "--allow",
        "data-update",
    ]));
    let renamed = format!("{}.countries", schema(pg));
    ensure_rows(&d, &table, "\n    JP: {label: Nippon}");
    let old_path = d.dir.join(format!("schema/{table}.yml"));
    let body = std::fs::read_to_string(&old_path)
        .unwrap()
        .replace(&table, &renamed);
    std::fs::remove_file(old_path).unwrap();
    std::fs::write(d.dir.join(format!("schema/{renamed}.yml")), body).unwrap();
    ok(d.run(&["rename-table", &table, &renamed]));
    d.commit();
    let saved = plan(&d, connection);
    assert_keys(&saved, &renamed, &["TW", "JP"], &["JP"]);
    assert_eq!(saved.changes.changes.len(), 2);
    let before = history(&d, connection);
    assert_eq!(code(&apply(&d, connection, &saved.checksum(), false)), 1);
    assert_eq!(history(&d, connection), before);
    ok(apply(&d, connection, &saved.checksum(), true));
    ok(d.run(&["verify", "--db", connection]));
    assert!(plan(&d, connection).changes.is_empty());
    on_server(
        connection,
        &format!("UPDATE {renamed} SET label = 'Changed' WHERE code = 'JP'"),
    );
    drift(&d, connection);
}
