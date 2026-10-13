//! Large declared data must use the ordinary baseline/plan/apply/verify path.
use super::{Demo, OwnDatabase, apply_plan, code, on_server, scalar, server, stderr, succeeds};
use pbps_model::{Change, SavedPlan};
use serde_json::{Map, Value, json};

fn declare(d: &Demo, mode: &str, count: usize) {
    let rows: Map<String, Value> = (1..=count)
        .map(|n| (format!("SKU_{n}"), json!({})))
        .collect();
    let declaration = json!({
        "table": "app.product",
        "columns": {"code": {"type": "varchar(32)", "nullable": false}},
        "primary_key": {"name": "pk_product", "columns": ["code"]},
        "data": {"mode": mode, "rows": rows}
    });
    std::fs::write(
        d.dir.join("schema/app.product.yml"),
        serde_json::to_string(&declaration).unwrap(),
    )
    .unwrap();
}

fn empty_plan(d: &Demo, connection: &str) {
    let path = d.dir.join("read-plan.json");
    succeeds(d.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()]));
    let saved: SavedPlan = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert!(saved.changes.is_empty(), "{:?}", saved.changes);
    succeeds(d.run(&["verify", "--db", connection]));
}

#[test]
#[ignore = "needs live PostgreSQL; see scripts/live-tests-pg.sh"]
fn large_exact_and_ensure_catalogues_can_be_adopted_planned_and_verified() {
    for mode in ["exact", "ensure"] {
        let own = OwnDatabase::new(&server(), &format!("alias1805_{mode}"));
        let connection = own.connection();
        on_server(
            connection,
            "CREATE SCHEMA app; CREATE TABLE app.product(code varchar(32) NOT NULL, CONSTRAINT pk_product PRIMARY KEY(code)); INSERT INTO app.product SELECT 'SKU_' || n FROM generate_series(1,10000) n;",
        );
        let d = Demo::new(&format!("alias1805-{mode}"));
        declare(&d, mode, 10000);
        succeeds(d.run(&["plan", "--no-dev"]));
        d.commit();
        succeeds(d.run(&[
            "baseline",
            "--db",
            connection,
            "--reason",
            "Adopt the product catalogue",
        ]));
        empty_plan(&d, connection);
        // Batching must not expand ensure's scope or hide exact's extra row.
        on_server(connection, "INSERT INTO app.product VALUES ('EXTERNAL')");
        let extra = d.run(&["verify", "--db", connection]);
        assert_eq!(
            code(&extra),
            if mode == "exact" { 2 } else { 0 },
            "{}",
            stderr(&extra)
        );
        if mode == "ensure" {
            empty_plan(&d, connection);
        }
        on_server(
            connection,
            "DELETE FROM app.product WHERE code IN ('EXTERNAL','SKU_1')",
        );
        let missing = d.run(&["verify", "--db", connection]);
        assert_eq!(code(&missing), 2, "{}", stderr(&missing));
        on_server(connection, "INSERT INTO app.product VALUES ('SKU_1')");
        empty_plan(&d, connection);
    }
}

#[test]
#[ignore = "needs live PostgreSQL; see scripts/live-tests-pg.sh"]
fn large_catalogues_read_back_inside_bootstrap_and_apply_transactions() {
    let own = OwnDatabase::new(&server(), "alias1805_write");
    let connection = own.connection();
    on_server(connection, "CREATE SCHEMA app");
    let d = Demo::new("alias1805-write");
    declare(&d, "exact", 10000);
    succeeds(d.run(&["plan", "--no-dev"]));
    d.commit();
    succeeds(d.run(&["bootstrap", "--db", connection]));
    assert_eq!(
        scalar(connection, "SELECT count(*) FROM app.product"),
        10000
    );
    empty_plan(&d, connection);
    declare(&d, "exact", 10001);
    d.commit();
    let path = d.dir.join("insert-plan.json");
    succeeds(d.run(&["plan", "--db", connection, "--out", path.to_str().unwrap()]));
    let saved: SavedPlan = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved.changes.changes.len(), 1);
    assert!(matches!(
        saved.changes.changes[0].change,
        Change::InsertRow { .. }
    ));
    succeeds(apply_plan(&d, connection, &path, false));
    assert_eq!(
        scalar(connection, "SELECT count(*) FROM app.product"),
        10001
    );
    empty_plan(&d, connection);
}
