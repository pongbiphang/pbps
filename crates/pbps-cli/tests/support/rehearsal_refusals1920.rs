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

/// The statement that a written file did not pass the rehearsal.
const UNVERIFIED: &str = "did NOT pass the dev rehearsal";

/// Runs `plan --dev` with the given extra arguments in both formats.
fn rehearse_with(
    d: &Demo,
    connection: &str,
    extra: &[&str],
) -> (Output, serde_json::Value, Output) {
    let mut human_args = vec!["plan", "--dev", connection];
    human_args.extend_from_slice(extra);
    let mut json_args = human_args.clone();
    json_args.extend_from_slice(&["--format", "json"]);
    let human = d.run(&human_args);
    let json = d.run(&json_args);
    let envelope: serde_json::Value = serde_json::from_str(&stdout(&json))
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {}", stdout(&json)));
    (human, envelope, json)
}

/// The `rehearsal.output-unverified` findings of an envelope, as (file, severity).
fn unverified(envelope: &serde_json::Value) -> Vec<(String, String)> {
    envelope["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["id"] == "rehearsal.output-unverified")
        .map(|f| {
            (
                f["location"]["file"].as_str().unwrap().to_owned(),
                f["severity"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
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

#[test]
fn a_refused_rehearsal_names_each_written_file_once_in_both_formats() {
    let d = Demo::new("r1920-unverified");
    repaired_history(&d);
    let out = d.dir.join("plan.json");
    let sql = d.dir.join("plan.sql");
    let (out_arg, sql_arg) = (out.to_str().unwrap(), sql.to_str().unwrap());
    let (human, envelope, json) = rehearse_with(&d, REFUSED, &["--out", out_arg, "--sql", sql_arg]);

    // The files are still written before the rehearsal (SPEC 9.3) and exit
    // codes are what they were.
    assert!(out.exists() && sql.exists());
    assert_eq!(code(&human), 1, "{}", stderr(&human));
    assert_eq!(code(&json), 1, "{}", stderr(&json));
    assert_eq!(envelope["result"], "unanswerable", "{envelope}");
    let findings = envelope["findings"].as_array().unwrap();
    assert_eq!(findings[0]["id"], "rehearsal.unavailable", "{envelope}");
    let mut named = unverified(&envelope);
    named.sort();
    assert_eq!(
        named,
        [
            (out_arg.to_owned(), "warning".to_owned()),
            (sql_arg.to_owned(), "warning".to_owned())
        ],
        "{envelope}"
    );
    let err = stderr(&human);
    assert_eq!(err.matches(UNVERIFIED).count(), 2, "{err}");
    for file in [out_arg, sql_arg] {
        assert_eq!(err.matches(file).count(), 1, "{file}: {err}");
    }
    // After the refusal text, on the stream that text uses.
    assert!(err.find("the baseline cannot be expressed").unwrap() < err.find(UNVERIFIED).unwrap());
    assert!(!stdout(&human).contains(UNVERIFIED), "{}", stdout(&human));
}

#[test]
fn a_refused_rehearsal_names_only_the_file_that_was_written() {
    let d = Demo::new("r1920-unverified-one");
    repaired_history(&d);
    let sql = d.dir.join("plan.sql");
    let sql_arg = sql.to_str().unwrap();
    let (human, envelope, _) = rehearse_with(&d, REFUSED, &["--sql", sql_arg]);
    assert_eq!(unverified(&envelope).len(), 1, "{envelope}");
    assert_eq!(stderr(&human).matches(UNVERIFIED).count(), 1);
    assert!(!stderr(&human).contains("plan.json"));
}

#[test]
fn a_refused_rehearsal_without_output_flags_names_no_file() {
    let d = Demo::new("r1920-unverified-none");
    repaired_history(&d);
    let (human, envelope, _) = rehearse(&d, REFUSED);
    assert!(unverified(&envelope).is_empty(), "{envelope}");
    assert!(!stderr(&human).contains(UNVERIFIED), "{}", stderr(&human));
    assert!(!stderr(&human).ends_with("\n\n"), "{:?}", stderr(&human));
}

#[test]
fn a_run_that_skips_the_rehearsal_says_nothing_about_it() {
    let d = Demo::new("r1920-unverified-skipped");
    repaired_history(&d);
    let sql = d.dir.join("plan.sql");
    let sql_arg = sql.to_str().unwrap();
    for flag in ["--no-dev", "--check"] {
        let human = d.run(&["plan", flag, "--sql", sql_arg]);
        let json = d.run(&["plan", flag, "--sql", sql_arg, "--format", "json"]);
        for o in [&human, &json] {
            assert!(!stdout(o).contains(UNVERIFIED), "{flag}: {}", stdout(o));
            assert!(!stdout(o).contains("rehearsal.output-unverified"), "{flag}");
            assert!(!stderr(o).contains(UNVERIFIED), "{flag}: {}", stderr(o));
        }
    }
}

/// A computed column declared as `x + y`, which the engine stores as `[x]+[y]`:
/// the rehearsal runs, and what it reads back differs from what was declared.
fn computed_in_another_spelling(d: &Demo) {
    std::fs::write(
        d.dir.join("schema/dbo.t.yml"),
        "table: dbo.t\ncolumns:\n  x: {type: int}\n  y: {type: int}\n\
         computed:\n  z: {expression: \"x + y\"}\n",
    )
    .unwrap();
}

#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn a_rehearsal_that_does_not_converge_names_the_written_file_in_both_formats() {
    let server = std::env::var("PBPS_TEST_DB").expect("PBPS_TEST_DB is not set");
    let d = Demo::new("r1920-unverified-diverges");
    computed_in_another_spelling(&d);
    let sql = d.dir.join("plan.sql");
    let sql_arg = sql.to_str().unwrap();
    let (human, envelope, json) = rehearse_with(&d, &server, &["--sql", sql_arg]);

    assert!(sql.exists());
    assert_eq!(code(&human), 2, "{}{}", stdout(&human), stderr(&human));
    assert_eq!(code(&json), 2, "{}{}", stdout(&json), stderr(&json));
    let findings = envelope["findings"].as_array().unwrap();
    let diverges = findings
        .iter()
        .position(|f| f["id"] == "rehearsal.does-not-converge")
        .unwrap_or_else(|| panic!("no non-convergence finding: {envelope}"));
    let named: Vec<usize> = findings
        .iter()
        .enumerate()
        .filter(|(_, f)| f["id"] == "rehearsal.output-unverified")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(named.len(), 1, "{envelope}");
    assert!(named[0] > diverges, "after the non-convergence: {envelope}");
    assert_eq!(
        unverified(&envelope),
        [(sql_arg.to_owned(), "warning".into())]
    );
    let err = stderr(&human);
    assert_eq!(err.matches(UNVERIFIED).count(), 1, "{err}");
    assert_eq!(err.matches(sql_arg).count(), 1, "{err}");
    assert!(err.find("does not converge").unwrap() < err.find(UNVERIFIED).unwrap());
}

#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn a_rehearsal_that_does_not_converge_without_output_flags_names_no_file() {
    let server = std::env::var("PBPS_TEST_DB").expect("PBPS_TEST_DB is not set");
    let d = Demo::new("r1920-unverified-diverges-none");
    computed_in_another_spelling(&d);
    let (human, envelope, _) = rehearse(&d, &server);
    assert_eq!(code(&human), 2, "{}{}", stdout(&human), stderr(&human));
    assert!(unverified(&envelope).is_empty(), "{envelope}");
    assert!(!stderr(&human).contains(UNVERIFIED), "{}", stderr(&human));
    assert!(stderr(&human).contains("does not converge"));
}

#[test]
#[ignore = "needs a live SQL Server; set PBPS_TEST_DB (see scripts/live-tests.sh)"]
fn a_rehearsal_that_converges_names_no_written_file() {
    let server = std::env::var("PBPS_TEST_DB").expect("PBPS_TEST_DB is not set");
    let d = Demo::new("r1920-unverified-converges");
    table(&d, "a", true, false);
    let sql = d.dir.join("plan.sql");
    let out = d.dir.join("plan.json");
    let (human, envelope, json) = rehearse_with(
        &d,
        &server,
        &[
            "--sql",
            sql.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ],
    );
    assert_eq!(code(&human), 0, "{}{}", stdout(&human), stderr(&human));
    assert_eq!(code(&json), 0, "{}", stderr(&json));
    assert!(sql.exists() && out.exists());
    assert!(unverified(&envelope).is_empty(), "{envelope}");
    for text in [stdout(&human), stderr(&human)] {
        assert!(!text.contains(UNVERIFIED), "{text}");
    }
}
