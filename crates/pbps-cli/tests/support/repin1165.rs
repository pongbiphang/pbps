//! `pbps test fixtures repin`: a pin moves only when the edited fixture leaves
//! the same rows and the same next allocator values (ADR-0019 §5).
use super::*;

const IDENTITY: &str = "identity: [1, 1]";
const TABLE: &str = "table: app.t\ncolumns:\n  id: {type: integer, nullable: false, IDENTITY}\nNOTE  v: {type: text}\nprimary_key: {name: t_pk, columns: [id]}\n";
const PLAIN: &str = "table: app.plain\ncolumns:\n  id: {type: integer, nullable: false}\n  v: {type: text}\nprimary_key: {name: plain_pk, columns: [id]}\n";
const SERIAL: &str = "table: app.s\ncolumns:\n  code: {type: text, nullable: false}\n  n: {type: serial}\nprimary_key: {name: s_pk, columns: [code]}\n";

/// The pinned image of the live runner; the refusal test never starts it, so a
/// host without the variables still checks it.
fn scratch_image(old: bool) -> String {
    std::env::var(if old {
        "PBPS_TEST_PG_OLD_SCRATCH_IMAGE"
    } else {
        "PBPS_TEST_PG_SCRATCH_IMAGE"
    })
    .unwrap_or_else(|_| {
        "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280".into()
    })
}

/// The schema at `old` has `app.t(id, v)`; the base declares `note` between them,
/// so a replayed table (note last) and a bootstrapped one differ in column order.
struct World {
    p: Project,
    old: String,
    base: String,
}

fn world(identity: &str) -> World {
    let d = Demo::new(&format!("repin1165-{:x}", rand::random::<u64>()));
    d.commit();
    let p = Project(d);
    let table = |note: &str| TABLE.replace("IDENTITY", identity).replace("NOTE", note);
    p.write("schema/app.t.yml", table(""));
    p.write("schema/app.plain.yml", PLAIN);
    p.write("schema/app.s.yml", SERIAL);
    p.ids();
    let old = p.commit();
    p.write("schema/app.t.yml", table("  note: {type: text}\n"));
    p.ids();
    let base = p.commit();
    World { p, old, base }
}

impl World {
    /// Fixtures are `(name, at, committed SQL)`; the manifest and the SQL are
    /// committed, as a re-pin requires.
    fn commit_fixtures(&self, old: bool, fixtures: &[(&str, &str, &str)]) {
        for (name, _, sql) in fixtures {
            self.p.write(&format!("tests/{name}.sql"), sql);
        }
        let pins: Vec<(&str, &str)> = fixtures.iter().map(|(n, at, _)| (*n, *at)).collect();
        self.p.manifest(
            &scratch_image(old),
            &self.base,
            &pins,
            "SELECT 1 WHERE false",
        );
        self.p.commit();
    }
    fn manifest_text(&self) -> String {
        std::fs::read_to_string(self.p.0.dir.join("tests/pbps-tests.yml")).unwrap()
    }
    fn repin_command(&self, name: &str, at: &str, format: &str) -> Command {
        let mut c = Command::new(BIN);
        c.arg("--project")
            .arg(&self.p.0.dir)
            .args([
                "test", "fixtures", "repin", name, "--at", at, "--format", format,
            ])
            .env("XDG_STATE_HOME", self.p.state());
        c
    }
    /// Runs the command, checks the envelope against the current and the
    /// archived schema, and returns it.
    fn repin(&self, name: &str, at: &str, expected: i32) -> Value {
        let out = self.repin_command(name, at, "json").output().unwrap();
        assert_eq!(code(&out), expected, "{}{}", stdout(&out), stderr(&out));
        self.envelope(&out)
    }
    fn envelope(&self, out: &std::process::Output) -> Value {
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["command"], "test");
        assert_eq!(v["data"]["operation"], "repin");
        for raw in [
            include_str!("../../../../schemas/envelope.schema.json"),
            include_str!("../fixtures/published-schemas/88/envelope.schema.json"),
        ] {
            let schema = serde_json::from_str(raw).unwrap();
            jsonschema::validator_for(&schema)
                .unwrap()
                .validate(&v)
                .unwrap();
        }
        assert!(v["data"].get("plan").is_none());
        identities(&v);
        v
    }
}

fn finding_ids(v: &Value) -> Vec<String> {
    v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["severity"] == "error")
        .map(|f| f["id"].as_str().unwrap().to_owned())
        .collect()
}
fn messages(v: &Value) -> String {
    v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["severity"] == "error")
        .map(|f| f["message"].as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}
fn outcome(v: &Value, phase: &str, side: &str) -> String {
    v["data"]["phases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["phase"] == phase && p["side"] == side)
        .unwrap_or_else(|| panic!("no {phase} phase on {side}: {v}"))["outcome"]
        .as_str()
        .unwrap()
        .to_owned()
}

const KEPT_AND_DISCARDED: &str = concat!(
    "INSERT INTO app.t(v) VALUES ('kept'); INSERT INTO app.t(v) VALUES ('discarded'); ",
    "DELETE FROM app.t WHERE v = 'discarded';"
);

fn verified_match(old: bool) {
    let w = world(IDENTITY);
    w.commit_fixtures(old, &[("orders", &w.old, KEPT_AND_DISCARDED)]);
    let before = w.manifest_text();
    // The same data and the same next id, written for the newer schema.
    let edited = concat!(
        "INSERT INTO app.t(v, note) VALUES ('kept', NULL), ('discarded', NULL); ",
        "DELETE FROM app.t WHERE v = 'discarded';"
    );
    w.p.write("tests/orders.sql", edited);
    let v = w.repin("orders", &w.base, 0);
    engine(&v, old);
    assert_eq!(v["result"], "ok");
    assert_eq!(v["data"]["cleanup_confirmed"], true);
    assert_eq!(v["data"]["repin"]["manifest"]["status"], "not_attempted");
    let sides = v["data"]["repin"]["sides"].as_array().unwrap();
    assert_eq!(sides.len(), 2);
    assert_ne!(
        sides[0]["run_id"], sides[1]["run_id"],
        "two fresh scratches"
    );
    assert!(
        v["data"]["repin"]["tables"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["equal"] == true)
    );
    let allocators = v["data"]["repin"]["allocators"].as_array().unwrap();
    let ids = allocators
        .iter()
        .find(|a| a["sequence"] == "app.t_id_seq")
        .unwrap();
    assert_eq!(
        (&ids["original_next"], &ids["proposed_next"]),
        (&json!("3"), &json!("3"))
    );
    assert_eq!(ids["used_by"], json!("app.t.id"));
    // The ledger's own allocator is not compared; the serial table's is.
    assert!(
        allocators
            .iter()
            .all(|a| a["sequence"].as_str().unwrap().starts_with("app."))
    );
    for name in ["compare_rows", "compare_allocators"] {
        assert_eq!(phase(&v, name)["outcome"], "passed");
    }
    // Verification only: neither the manifest nor the SQL was touched.
    assert_eq!(w.manifest_text(), before);
    assert_eq!(
        std::fs::read_to_string(w.p.0.dir.join("tests/orders.sql")).unwrap(),
        edited
    );
    w.p.settled();
}

fn allocator_state_differs(old: bool) {
    let w = world(IDENTITY);
    w.commit_fixtures(old, &[("orders", &w.old, KEPT_AND_DISCARDED)]);
    let before = w.p.bytes();
    // The remaining rows are equal; the next generated ids are 3 and 2.
    w.p.write(
        "tests/orders.sql",
        "INSERT INTO app.t(v, note) VALUES ('kept', NULL);",
    );
    let edited = w.p.bytes();
    let v = w.repin("orders", &w.base, 2);
    assert_eq!(v["result"], "findings");
    assert_eq!(finding_ids(&v), ["test.repin.allocator"], "{v}");
    assert!(
        v["data"]["repin"]["tables"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["equal"] == true)
    );
    let allocators = v["data"]["repin"]["allocators"].as_array().unwrap();
    let a = allocators
        .iter()
        .find(|a| a["sequence"] == "app.t_id_seq")
        .unwrap();
    assert_eq!(
        (&a["original_next"], &a["proposed_next"]),
        (&json!("3"), &json!("2"))
    );
    assert_eq!(v["data"]["repin"]["manifest"]["status"], "not_attempted");
    assert_eq!(v["data"]["cleanup_confirmed"], true);
    assert_eq!(w.p.bytes(), edited, "a mismatch must not write anything");
    assert_ne!(before, edited);
    w.p.settled();
}

fn later_fixture_exposes_the_difference(old: bool) {
    let w = world(IDENTITY);
    w.commit_fixtures(
        old,
        &[
            ("first", &w.old, KEPT_AND_DISCARDED),
            ("later", &w.old, "INSERT INTO app.t(v) VALUES ('later');"),
        ],
    );
    w.p.write(
        "tests/first.sql",
        "INSERT INTO app.t(v, note) VALUES ('kept', NULL);",
    );
    let manifest = w.manifest_text();
    let v = w.repin("first", &w.base, 2);
    let ids = finding_ids(&v);
    assert!(ids.contains(&"test.repin.rows".to_owned()), "{v}");
    assert!(ids.contains(&"test.repin.allocator".to_owned()), "{v}");
    let t = v["data"]["repin"]["tables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["table"] == "app.t")
        .unwrap();
    assert_eq!(t["equal"], false);
    assert!(!t["differences"].as_array().unwrap().is_empty());
    assert_eq!(w.manifest_text(), manifest);
}

fn rows_differ(old: bool) {
    let w = world(IDENTITY);
    w.commit_fixtures(
        old,
        &[(
            "plain",
            &w.old,
            "INSERT INTO app.plain VALUES (1, 'a'), (2, NULL);",
        )],
    );
    let manifest = w.manifest_text();
    for edited in [
        "INSERT INTO app.plain VALUES (1, 'A'), (2, NULL);",
        "INSERT INTO app.plain VALUES (1, 'a'), (2, '');",
        "INSERT INTO app.plain VALUES (1, 'a ');",
    ] {
        w.p.write("tests/plain.sql", edited);
        let v = w.repin("plain", &w.base, 2);
        assert_eq!(finding_ids(&v), ["test.repin.rows"], "{edited}: {v}");
        let t = v["data"]["repin"]["tables"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["table"] == "app.plain")
            .unwrap();
        assert_eq!(t["equal"], false, "{edited}");
        assert_eq!(t["original_columns"], json!(["id", "v"]));
        assert!(
            v["data"]["repin"]["allocators"]
                .as_array()
                .unwrap()
                .iter()
                .all(|a| a["equal"] == true)
        );
        assert_eq!(w.manifest_text(), manifest, "{edited}");
    }
    w.p.settled();
}

fn serial_and_identity_allocators(old: bool) {
    let w = world(IDENTITY);
    w.commit_fixtures(
        old,
        &[(
            "s",
            &w.old,
            "INSERT INTO app.s(code) VALUES ('x'), ('y'); DELETE FROM app.s WHERE code = 'y';",
        )],
    );
    w.p.write("tests/s.sql", "INSERT INTO app.s(code) VALUES ('x');");
    let v = w.repin("s", &w.base, 2);
    assert_eq!(finding_ids(&v), ["test.repin.allocator"], "{v}");
    let allocators = v["data"]["repin"]["allocators"].as_array().unwrap();
    let serial = allocators
        .iter()
        .find(|a| a["sequence"] == "app.s_n_seq")
        .unwrap();
    assert_eq!(serial["equal"], false);
    assert_eq!(serial["used_by"], json!("app.s.n"));
    let identity = allocators
        .iter()
        .find(|a| a["sequence"] == "app.t_id_seq")
        .unwrap();
    assert_eq!(
        identity["equal"], true,
        "an allocator nobody touched compares equal"
    );
}

fn another_groups_operations_do_not_commute(old: bool) {
    let w = world(IDENTITY);
    // `late` runs at the base, `early` at the old schema. In the original `early` loads
    // first (id 1); moved to the base after `b` it would take id 2.
    w.commit_fixtures(
        old,
        &[
            (
                "late",
                &w.base,
                "INSERT INTO app.t(v, note) VALUES ('b', NULL);",
            ),
            ("early", &w.old, "INSERT INTO app.t(v) VALUES ('a');"),
        ],
    );
    w.p.write(
        "tests/early.sql",
        "INSERT INTO app.t(v, note) VALUES ('a', NULL);",
    );
    let manifest = w.manifest_text();
    let v = w.repin("early", &w.base, 2);
    assert!(
        finding_ids(&v).contains(&"test.repin.rows".to_owned()),
        "{v}"
    );
    assert_eq!(w.manifest_text(), manifest);
    // Listed first, `early` keeps its place before `late`, and the comparison passes.
    w.commit_fixtures(
        old,
        &[
            ("early", &w.old, "INSERT INTO app.t(v) VALUES ('a');"),
            (
                "late",
                &w.base,
                "INSERT INTO app.t(v, note) VALUES ('b', NULL);",
            ),
        ],
    );
    w.p.write(
        "tests/early.sql",
        "INSERT INTO app.t(v, note) VALUES ('a', NULL);",
    );
    let manifest = w.manifest_text();
    let v = w.repin("early", &w.base, 0);
    assert_eq!(v["data"]["repin"]["manifest"]["status"], "not_attempted");
    assert_eq!(w.manifest_text(), manifest);
}

fn fixture_errors(old: bool) {
    let w = world(IDENTITY);
    w.commit_fixtures(old, &[("orders", &w.old, KEPT_AND_DISCARDED)]);
    let manifest = w.manifest_text();
    for edited in [
        "INSERT INTO app.t(nope) VALUES ('x');",
        "CREATE TABLE app.forbidden(n integer);",
    ] {
        w.p.write("tests/orders.sql", edited);
        let v = w.repin("orders", &w.base, 2);
        assert_eq!(finding_ids(&v), ["test.fixture"], "{edited}: {v}");
        assert_eq!(outcome(&v, "fixture", "original"), "passed");
        assert_eq!(outcome(&v, "fixture", "proposed"), "failed");
        assert_eq!(phase(&v, "compare_rows")["outcome"], "skipped");
        assert_eq!(w.manifest_text(), manifest);
    }
    w.p.settled();
}

fn exhausted_allocator_is_unanswerable(old: bool) {
    let w = world("identity: {seed: 1, increment: 1, max: 2}");
    w.commit_fixtures(
        old,
        &[(
            "orders",
            &w.old,
            "INSERT INTO app.t(v) VALUES ('a'), ('b');",
        )],
    );
    w.p.write(
        "tests/orders.sql",
        "INSERT INTO app.t(v, note) VALUES ('a', NULL), ('b', NULL);",
    );
    let manifest = w.manifest_text();
    let v = w.repin("orders", &w.base, 1);
    assert_eq!(v["result"], "unanswerable");
    assert!(
        finding_ids(&v).contains(&"test.repin.unestablished".to_owned()),
        "{v}"
    );
    assert!(messages(&v).contains("app.t_id_seq"), "{v}");
    assert_eq!(v["data"]["repin"]["manifest"]["status"], "not_attempted");
    assert_eq!(w.manifest_text(), manifest);
    w.p.settled();
}

fn a_changed_input_cannot_move_the_pin(old: bool) {
    for target in ["tests/pbps-tests.yml", "tests/orders.sql"] {
        let w = world(IDENTITY);
        w.commit_fixtures(old, &[("orders", &w.old, KEPT_AND_DISCARDED)]);
        // Equal data; the sleep holds the proposed side open while the file moves.
        w.p.write(
            "tests/orders.sql",
            concat!(
                "INSERT INTO app.t(v, note) VALUES ('kept', NULL), ('discarded', NULL); ",
                "DELETE FROM app.t WHERE v = 'discarded'; SELECT pg_sleep(12);"
            ),
        );
        let manifest = w.manifest_text();
        let (running, _) = waiting_on(&w.p, w.repin_command("orders", &w.base, "json"));
        let path = w.p.0.dir.join(target);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("\n-- edited during the run\n");
        std::fs::write(&path, &text).unwrap();
        let out = running.finish();
        assert_eq!(code(&out), 1, "{target}: {}{}", stdout(&out), stderr(&out));
        let v = w.envelope(&out);
        assert!(messages(&v).contains("changed"), "{target}: {v}");
        assert_eq!(v["data"]["repin"]["manifest"]["status"], "not_attempted");
        let now = w.manifest_text();
        if target.ends_with(".yml") {
            assert_eq!(now, format!("{manifest}\n-- edited during the run\n"));
        } else {
            assert_eq!(now, manifest, "the pin must not move");
        }
        w.p.settled();
    }
}

#[test]
fn repin_refusals_happen_before_any_scratch_and_change_nothing() {
    let w = world(IDENTITY);
    let first = {
        let out = Command::new("git")
            .arg("-C")
            .arg(&w.p.0.dir)
            .args(["rev-list", "--max-parents=0", "HEAD"])
            .output()
            .unwrap();
        stdout(&out).trim().to_owned()
    };
    w.commit_fixtures(true, &[("orders", &w.old, KEPT_AND_DISCARDED)]);
    let tip = w.p.head();
    // A commit on a history the base does not contain.
    w.p.0.git(&["checkout", "-qb", "side", &w.old]);
    w.p.write("side.txt", "side");
    let side = w.p.commit();
    w.p.0.git(&["checkout", "-q", "-"]);
    // Refusals must not even look for a container runtime.
    std::fs::write(w.p.0.dir.with_extension("blocked"), "not a directory").unwrap();
    let blocked = w.p.0.dir.with_extension("blocked");
    let run = |name: &str, at: &str| {
        let mut c = w.repin_command(name, at, "json");
        c.env("XDG_STATE_HOME", &blocked);
        let out = c.output().unwrap();
        let v = w.envelope(&out);
        assert!(v["data"]["run_id"].is_null(), "{v}");
        assert!(finding_ids(&v).iter().all(|id| id == "test.input"), "{v}");
        (code(&out), messages(&v))
    };
    let before = w.p.bytes();
    let cases: Vec<(String, String, &str)> = vec![
        ("nope".into(), w.base.clone(), "no fixture named `nope`"),
        ("orders".into(), w.base[..12].into(), "not a full commit id"),
        ("orders".into(), w.old.clone(), "already pinned"),
        ("orders".into(), first, "does not descend"),
        ("orders".into(), tip, "descendant of the base"),
        ("orders".into(), side, "diverging histories"),
        (
            "orders".into(),
            "f".repeat(40),
            "not a commit this repository has",
        ),
    ];
    for (name, at, expected) in &cases {
        let (exit, message) = run(name, at);
        assert_eq!(exit, 2, "{name} {at}: {message}");
        assert!(message.contains(expected), "{name} {at}: {message}");
        assert_eq!(w.p.bytes(), before);
    }
    // The baseline must be committed, and only the selected fixture may differ.
    let manifest = w.p.0.dir.join("tests/pbps-tests.yml");
    let original = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(&manifest, format!("{original}# local note\n")).unwrap();
    let (exit, message) = run("orders", &w.base);
    assert_eq!(exit, 2);
    assert!(message.contains("uncommitted changes"), "{message}");
    // Every reason to stop is named in one run, not one per attempt.
    let (_, both) = run("orders", &w.old);
    assert!(
        both.contains("uncommitted changes") && both.contains("already pinned"),
        "{both}"
    );
    std::fs::write(&manifest, &original).unwrap();
    w.p.write("tests/other.sql", "SELECT 1;");
    std::fs::write(
        &manifest,
        original.replace(
            "assertions:",
            &format!(
                "  - {{name: other, file: other.sql, at: {}}}\nassertions:",
                w.old
            ),
        ),
    )
    .unwrap();
    // `other.sql` is on disk but its manifest entry is not committed.
    let (exit, message) = run("orders", &w.base);
    assert_eq!(exit, 2);
    assert!(message.contains("uncommitted changes"), "{message}");
    w.p.0.git(&["add", "-A"]);
    w.p.0.git(&["commit", "-qm", "manifest names other"]);
    w.p.write("tests/other.sql", "SELECT 2;");
    let (exit, message) = run("orders", &w.base);
    assert_eq!(exit, 2);
    assert!(
        message.contains("`other`") && message.contains("uncommitted changes"),
        "{message}"
    );
    // A fixture file git does not have is not a baseline.
    w.p.write("tests/other.sql", "SELECT 1;");
    w.p.0.git(&["rm", "-q", "--cached", "tests/other.sql"]);
    w.p.0.git(&["commit", "-qm", "untrack other"]);
    let (exit, message) = run("orders", &w.base);
    assert_eq!(exit, 2);
    assert!(message.contains("not committed at HEAD"), "{message}");
    assert!(!w.p.state().exists());
}

macro_rules! on_both {
    ($($name:ident => $body:ident),* $(,)?) => {$(
        mod $name {
            use super::*;
            #[test]
            #[ignore = "needs docker and the pinned PostgreSQL 16 scratch image"]
            fn pg16() { $body(true) }
            #[test]
            #[ignore = "needs docker and the pinned PostgreSQL 18 scratch image"]
            fn pg18() { $body(false) }
        }
    )*};
}

on_both! {
    repin_equal_rows_and_allocators_verify_without_writing => verified_match,
    repin_equal_rows_different_allocator_state => allocator_state_differs,
    repin_later_fixture_exposes_allocator_difference => later_fixture_exposes_the_difference,
    repin_row_differences_in_null_case_and_space_are_refused => rows_differ,
    repin_serial_and_identity_allocators_are_named => serial_and_identity_allocators,
    repin_other_groups_noncommuting_order_is_refused => another_groups_operations_do_not_commute,
    repin_fixture_errors_name_the_proposed_side => fixture_errors,
    repin_unestablishable_allocator_is_unanswerable => exhausted_allocator_is_unanswerable,
    repin_changed_input_cannot_move_the_pin => a_changed_input_cannot_move_the_pin,
}
