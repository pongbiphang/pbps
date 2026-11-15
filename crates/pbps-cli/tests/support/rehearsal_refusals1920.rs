//! A rehearsal that cannot compare reports every reason the differ gave, once,
//! in both output formats, and nothing else of the run is lost (#1920).
//!
//! The comparison that follows the plan's statements is covered in
//! `engine::mssql_live_tests::rehearsal_refusal1920`: no project this CLI can load makes it refuse, so it is
//! driven from the entry the CLI calls, with a plan that omits a change.
use super::*;

/// The scratch server is refused before anything is started, so a rehearsal
/// that gets as far as connecting has passed every refusal that comes first.
const REFUSED: &str = "Server=127.0.0.1,1;User Id=sa;Password=x;TrustServerCertificate=true";

const NO_KEY: &str = "declares `data:` but has no single-column primary key";

fn table(d: &Demo, name: &str, key: bool, rows: bool) {
    std::fs::write(
        d.dir.join(format!("schema/dbo.{name}.yml")),
        format!(
            "table: dbo.{name}\ncolumns:\n  id: {{type: int, nullable: false}}\n  \
             c: {{type: int}}\n{}{}",
            if key {
                format!("primary_key: {{name: pk_{name}, columns: [id]}}\n")
            } else {
                String::new()
            },
            if rows {
                "data:\n  mode: ensure\n  rows:\n    1: {c: 5}\n"
            } else {
                ""
            }
        ),
    )
    .unwrap();
}

/// A history in which both tables declared rows and neither declared a key, as
/// a project written before that was refused records it, then the repair.
fn repaired_history(d: &Demo) {
    for name in ["a", "b"] {
        table(d, name, false, false);
    }
    let o = d.run(&["plan"]);
    assert_eq!(code(&o), 0, "{}{}", stdout(&o), stderr(&o));
    for name in ["a", "b"] {
        table(d, name, false, true);
    }
    d.commit();
    for name in ["a", "b"] {
        table(d, name, true, true);
    }
}

/// Runs `plan --dev` and returns the human and the JSON outputs of it.
fn rehearse(d: &Demo, connection: &str) -> (Output, serde_json::Value, Output) {
    let human = d.run(&["plan", "--dev", connection]);
    let json = d.run(&["plan", "--dev", connection, "--format", "json"]);
    let envelope: serde_json::Value = serde_json::from_str(&stdout(&json))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&json)));
    (human, envelope, json)
}

#[test]
fn a_baseline_with_several_refusals_names_each_once_in_both_formats() {
    let d = Demo::new("r1920-baseline");
    repaired_history(&d);
    let (human, envelope, json) = rehearse(&d, REFUSED);

    assert_eq!(code(&human), 1, "{}", stderr(&human));
    assert_eq!(code(&json), 1, "{}", stderr(&json));
    assert_eq!(envelope["result"], "unanswerable", "{envelope}");
    let findings = envelope["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{envelope}");
    assert_eq!(findings[0]["id"], "rehearsal.unavailable", "{envelope}");
    let message = findings[0]["message"].as_str().unwrap();
    for (name, text) in [("JSON", message), ("human", &stderr(&human)[..])] {
        assert_eq!(
            text.matches("the baseline cannot be expressed").count(),
            1,
            "{name}: {text}"
        );
        for table in ["dbo.a", "dbo.b"] {
            assert_eq!(
                text.matches(&format!("{table} {NO_KEY}")).count(),
                1,
                "{name} {table}: {text}"
            );
        }
        // No count stands in for them.
        assert!(!text.contains("2 change(s)"), "{name}: {text}");
    }
    // The preview a plan prints with no engine in the room is not lost.
    assert!(
        stdout(&human).contains("2 change(s) across 2 table(s)"),
        "{}",
        stdout(&human)
    );
}

#[test]
fn a_baseline_without_a_refusal_reports_only_what_stopped_the_rehearsal() {
    let d = Demo::new("r1920-clean");
    for name in ["a", "b"] {
        table(&d, name, true, true);
    }
    assert_eq!(code(&d.run(&["plan"])), 0);
    d.commit();
    table(&d, "a", true, false);
    let (human, envelope, _) = rehearse(&d, REFUSED);

    assert_eq!(code(&human), 1, "{}", stderr(&human));
    let message = envelope["findings"][0]["message"].as_str().unwrap();
    for (name, text) in [("JSON", message), ("human", &stderr(&human)[..])] {
        // It stopped at the connection, which the message says, and a refusal
        // that never happened is not reported.
        assert!(text.contains("the scratch server"), "{name}: {text}");
        assert!(!text.contains("cannot be expressed"), "{name}: {text}");
        assert!(!text.contains(NO_KEY), "{name}: {text}");
    }
}

/// A statement the engine rejects is an operational failure of the rehearsal,
/// not a refusal of the differ, and its diagnostic is printed once.
#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn a_statement_the_engine_rejects_is_reported_once_and_is_not_a_convergence_failure() {
    let server = std::env::var("PBPS_TEST_DB").expect("PBPS_TEST_DB is not set");
    let d = Demo::new("r1920-rejected");
    // `z` is declared as a column and as a computed column, which the engine
    // refuses when it creates the table.
    std::fs::write(
        d.dir.join("schema/dbo.t.yml"),
        "table: dbo.t\ncolumns:\n  x: {type: int}\n  z: {type: int}\n\
         computed:\n  z: {expression: \"[x]+1\"}\n",
    )
    .unwrap();
    let (human, envelope, _) = rehearse(&d, &server);

    assert_eq!(code(&human), 1, "{}", stderr(&human));
    let findings = envelope["findings"].as_array().unwrap();
    assert!(
        findings
            .iter()
            .all(|f| f["id"] != "rehearsal.does-not-converge"),
        "{envelope}"
    );
    assert_eq!(findings.len(), 1, "{envelope}");
    assert_eq!(findings[0]["id"], "rehearsal.unavailable", "{envelope}");
    let message = findings[0]["message"].as_str().unwrap();
    for (name, text) in [("JSON", message), ("human", &stderr(&human)[..])] {
        assert_eq!(
            text.matches("rejected statement 1 of the plan").count(),
            1,
            "{name}: {text}"
        );
        assert!(
            text.contains("Column names in each table must be unique"),
            "{name}: {text}"
        );
    }
}
