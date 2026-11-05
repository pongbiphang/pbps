//! `pbps test fixtures repin` on an owned SQL Server (#1165 F3): a pin moves only
//! when the edited fixture leaves the same rows and the same next allocator
//! values (ADR-0019 §5). The engine reads themselves are qualified natively by
//! `plan_test::session::mssql_tests`; these tests pin what only the command
//! composes: two fresh scratches, the comparison, and that nothing is written on
//! any path that is not a verified match. Each needs Docker and
//! `PBPS_TEST_DEV_IMAGE`, and fails loudly without.
use super::*;
use serde_json::json;

const KEPT_AND_DISCARDED: &str = concat!(
    "INSERT INTO app.t(v) VALUES (N'kept'); INSERT INTO app.t(v) VALUES (N'discarded'); ",
    "DELETE FROM app.t WHERE v = N'discarded';"
);
/// The same data and the same next id, written for the newer schema. Two
/// INSERT statements, so a statement-level trigger fires as often as before.
const EDITED: &str = concat!(
    "INSERT INTO app.t(v, note) VALUES (N'kept', NULL); ",
    "INSERT INTO app.t(v, note) VALUES (N'discarded', NULL); ",
    "DELETE FROM app.t WHERE v = N'discarded';"
);
const AUDIT: &str = "table: app.audit\ncolumns:\n  n: {type: int, nullable: false, identity: [1, 1]}\n  note: {type: nvarchar(20)}\nprimary_key: {name: audit_pk, columns: [n]}\n";
const WATCH: &str = "trigger: app.watch\non: app.t\ndefinition: |-\n  AFTER INSERT AS INSERT INTO app.audit(note) VALUES (N'ins')\n";

fn table(id: &str, note: bool, extra: &str) -> String {
    format!(
        "table: app.t\ncolumns:\n  id: {{type: {id}, nullable: false}}\n{note}  v: {{type: nvarchar(20)}}\n{extra}primary_key: {{name: t_pk, columns: [id]}}\n",
        note = if note {
            "  note: {type: nvarchar(20)}\n"
        } else {
            ""
        },
    )
}

/// The schema at `old` has `app.t(id, v)`; the base declares `note` between
/// them, so a replayed table (note last) and a bootstrapped one differ in
/// column order.
struct World {
    p: Project,
    old: String,
    base: String,
}

fn world(id: &str, extra: &str, trigger: bool) -> World {
    let d = Demo::new(&format!("repin1165m-{:x}", rand::random::<u64>()));
    d.commit();
    let p = Project(d);
    let write = |note: bool| {
        p.write("schema/app.t.yml", table(id, note, extra));
        if trigger {
            p.write("schema/app.audit.yml", AUDIT);
            p.write("schema/app.watch.yml", WATCH);
        }
        p.ids();
    };
    write(false);
    let old = p.commit();
    write(true);
    let base = p.commit();
    World { p, old, base }
}

impl World {
    /// Fixtures are `(name, at, committed SQL)`; the manifest and the SQL are
    /// committed, as a re-pin requires.
    fn commit_fixtures(&self, fixtures: &[(&str, &str, &str)]) {
        for (name, _, sql) in fixtures {
            self.p.write(&format!("tests/{name}.sql"), sql);
        }
        let pins: Vec<(&str, &str)> = fixtures.iter().map(|(n, at, _)| (*n, *at)).collect();
        self.p
            .manifest(&self.base, &pins, "SELECT 1 AS x WHERE 1 = 0", 10_000);
        self.p.commit();
    }
    fn manifest_text(&self) -> String {
        std::fs::read_to_string(self.p.0.dir.join("tests/pbps-tests.yml")).unwrap()
    }
    /// Runs the re-pin, checks the envelope against the current and the
    /// archived schema, and returns it.
    fn repin(&self, name: &str, at: &str, expected: i32) -> Value {
        let out = Command::new(BIN)
            .arg("--project")
            .arg(&self.p.0.dir)
            .args([
                "test", "fixtures", "repin", name, "--at", at, "--format", "json",
            ])
            .env("XDG_STATE_HOME", self.p.state())
            .output()
            .unwrap();
        assert_eq!(code(&out), expected, "{}{}", stdout(&out), stderr(&out));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["command"], "test");
        assert_eq!(v["data"]["operation"], "repin");
        for raw in [
            include_str!("../../../../schemas/envelope.schema.json"),
            include_str!("../fixtures/published-schemas/89/envelope.schema.json"),
        ] {
            let schema = serde_json::from_str(raw).unwrap();
            jsonschema::validator_for(&schema)
                .unwrap()
                .validate(&v)
                .unwrap();
        }
        assert!(v["data"].get("plan").is_none());
        v
    }
}

fn error_ids(v: &Value) -> Vec<String> {
    v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["severity"] == "error")
        .map(|f| f["id"].as_str().unwrap().to_owned())
        .collect()
}
fn error_text(v: &Value) -> String {
    v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["severity"] == "error")
        .map(|f| f["message"].as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}
fn allocator<'a>(v: &'a Value, name: &str) -> &'a Value {
    v["data"]["repin"]["allocators"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["sequence"] == name)
        .unwrap_or_else(|| panic!("no allocator {name}: {v}"))
}

#[test]
#[ignore = "needs docker and the pinned SQL Server image"]
fn repin1165_mssql_moves_the_pin_when_rows_and_allocators_match_even_through_a_trigger() {
    let w = world("int, identity: [1, 1]", "", true);
    w.commit_fixtures(&[("orders", &w.old, KEPT_AND_DISCARDED)]);
    let before = w.manifest_text();
    w.p.write("tests/orders.sql", EDITED);
    let v = w.repin("orders", &w.base, 0);
    assert_eq!(v["result"], "ok");
    assert_eq!(v["data"]["cleanup_confirmed"], true);
    assert_eq!(v["data"]["repin"]["manifest"]["status"], "written");
    let build = v["data"]["engine_build"].as_str().unwrap();
    assert!(build.contains("SQL Server"), "{build}");
    let sides = v["data"]["repin"]["sides"].as_array().unwrap();
    assert_eq!(sides.len(), 2);
    assert_ne!(
        sides[0]["run_id"], sides[1]["run_id"],
        "two fresh scratches"
    );
    // The trigger's rows are compared too, and its own allocator, which the
    // probe of `app.t` advanced the same way on both sides.
    let tables = v["data"]["repin"]["tables"].as_array().unwrap();
    assert!(tables.iter().all(|t| t["equal"] == true), "{v}");
    assert!(tables.iter().any(|t| t["table"] == "app.audit"), "{v}");
    let ids = allocator(&v, "app.t.id (identity)");
    assert_eq!(
        (&ids["original_next"], &ids["proposed_next"]),
        (&json!("3"), &json!("3"))
    );
    assert_eq!(ids["used_by"], json!("app.t.id"));
    let audit = allocator(&v, "app.audit.n (identity)");
    assert_eq!(audit["original_next"], audit["proposed_next"]);
    // The ledger's own allocator is not compared.
    assert!(
        v["data"]["repin"]["allocators"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["sequence"].as_str().unwrap().starts_with("app."))
    );
    for name in ["compare_rows", "compare_allocators", "manifest_write"] {
        assert_eq!(phase(&v, name)["outcome"], "passed");
    }
    // Exactly the selected pin moved; the SQL is untouched.
    assert_eq!(
        w.manifest_text(),
        before.replacen(&format!("at: {}", w.old), &format!("at: {}", w.base), 1)
    );
    assert_eq!(
        std::fs::read_to_string(w.p.0.dir.join("tests/orders.sql")).unwrap(),
        EDITED
    );
    w.p.settled();
    // Pinned where it now is, the pin cannot advance again.
    let again = w.repin("orders", &w.base, 2);
    assert!(error_text(&again).contains("already pinned"), "{again}");
}

#[test]
#[ignore = "needs docker and the pinned SQL Server image"]
fn repin1165_mssql_refuses_when_only_the_next_generated_id_differs_and_writes_nothing() {
    let w = world("int, identity: [1, 1]", "", false);
    w.commit_fixtures(&[("orders", &w.old, KEPT_AND_DISCARDED)]);
    // The remaining rows are equal; the next generated ids are 3 and 2.
    w.p.write(
        "tests/orders.sql",
        "INSERT INTO app.t(v, note) VALUES (N'kept', NULL);",
    );
    let edited = w.p.bytes();
    let v = w.repin("orders", &w.base, 2);
    assert_eq!(v["result"], "findings");
    assert_eq!(error_ids(&v), ["test.repin.allocator"], "{v}");
    assert!(
        v["data"]["repin"]["tables"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["equal"] == true)
    );
    let ids = allocator(&v, "app.t.id (identity)");
    assert_eq!(
        (&ids["original_next"], &ids["proposed_next"]),
        (&json!("3"), &json!("2"))
    );
    assert_eq!(v["data"]["repin"]["manifest"]["status"], "not_attempted");
    assert_eq!(v["data"]["cleanup_confirmed"], true);
    assert_eq!(w.p.bytes(), edited, "a mismatch must not write anything");
    w.p.settled();
}

#[test]
#[ignore = "needs docker and the pinned SQL Server image"]
fn repin1165_mssql_refuses_rows_that_differ_only_by_case_or_a_trailing_space() {
    let w = world("int, identity: [1, 1]", "", false);
    w.commit_fixtures(&[("orders", &w.old, "INSERT INTO app.t(v) VALUES (N'kept');")]);
    // The default collation folds both; the comparison must not.
    for edited in ["Kept", "kept "] {
        w.p.write(
            "tests/orders.sql",
            format!("INSERT INTO app.t(v, note) VALUES (N'{edited}', NULL);"),
        );
        let before = w.p.bytes();
        let v = w.repin("orders", &w.base, 2);
        assert_eq!(error_ids(&v), ["test.repin.rows"], "{edited:?}: {v}");
        let t = v["data"]["repin"]["tables"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["table"] == "app.t")
            .unwrap();
        assert_eq!(t["equal"], false);
        assert_eq!(t["differences"].as_array().unwrap().len(), 2, "{v}");
        assert_eq!(v["data"]["repin"]["manifest"]["status"], "not_attempted");
        assert_eq!(w.p.bytes(), before, "a mismatch must not write anything");
    }
    w.p.settled();
}

#[test]
#[ignore = "needs docker and the pinned SQL Server image"]
fn repin1165_mssql_cannot_compare_a_rowversion_and_names_it_without_writing() {
    let w = world(
        "int, identity: [1, 1]",
        "  ver: {type: rowversion, nullable: false}\n",
        false,
    );
    w.commit_fixtures(&[("orders", &w.old, KEPT_AND_DISCARDED)]);
    w.p.write("tests/orders.sql", EDITED);
    let manifest = w.manifest_text();
    let v = w.repin("orders", &w.base, 1);
    assert_eq!(v["result"], "unanswerable");
    assert!(
        error_ids(&v).contains(&"test.repin.unestablished".to_owned()),
        "{v}"
    );
    assert!(error_text(&v).contains("app.t.ver"), "{v}");
    assert_eq!(v["data"]["repin"]["manifest"]["status"], "not_attempted");
    assert_eq!(w.manifest_text(), manifest);
    w.p.settled();
}

#[test]
#[ignore = "needs docker and the pinned SQL Server image"]
fn repin1165_mssql_cannot_establish_an_exhausted_identity_column_and_names_it() {
    // tinyint from 250 by 3: two inserts take 250 and 253, the next would be 256.
    let w = world("tinyint, identity: [250, 3]", "", false);
    w.commit_fixtures(&[(
        "orders",
        &w.old,
        "INSERT INTO app.t(v) VALUES (N'a'), (N'b');",
    )]);
    w.p.write(
        "tests/orders.sql",
        "INSERT INTO app.t(v, note) VALUES (N'a', NULL), (N'b', NULL);",
    );
    let manifest = w.manifest_text();
    let v = w.repin("orders", &w.base, 1);
    assert_eq!(v["result"], "unanswerable");
    assert!(
        error_ids(&v).contains(&"test.repin.unestablished".to_owned()),
        "{v}"
    );
    assert!(error_text(&v).contains("app.t"), "{v}");
    assert_eq!(v["data"]["repin"]["manifest"]["status"], "not_attempted");
    assert_eq!(w.manifest_text(), manifest);
    w.p.settled();
}
