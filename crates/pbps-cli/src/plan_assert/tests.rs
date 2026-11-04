use super::*;
use pbps_db::query::QueryColumn;
use sha2::Digest as _;

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        let dir =
            std::env::temp_dir().join(format!("pbps-plan-assert-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(dir.join("project")).unwrap();
        Self(dir)
    }
    fn root(&self) -> PathBuf {
        self.0.join("project")
    }
    fn write(&self, rel: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
        let path = self.root().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }
    fn prepare(
        &self,
        files: &[PathBuf],
        conn: Option<&str>,
    ) -> Result<Option<Selection>, Vec<Finding>> {
        let conn = conn
            .map(str::to_owned)
            .ok_or(std::env::VarError::NotPresent);
        Selection::prepare(&self.root(), files, conn)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ids(findings: &[Finding]) -> Vec<&str> {
    findings.iter().map(|f| f.id.as_str()).collect()
}

fn evidence(rows: Vec<Vec<EvidenceCell>>, truncated: bool) -> QueryEvidence {
    QueryEvidence {
        columns: vec![QueryColumn {
            name: "id".into(),
            type_name: "int4".into(),
        }],
        rows,
        truncated,
    }
}

#[test]
fn no_assertion_means_no_selection_whatever_the_variable_says() {
    let p = Project::new();
    assert!(p.prepare(&[], None).unwrap().is_none());
    assert!(p.prepare(&[], Some("host=x")).unwrap().is_none());
}

#[test]
fn a_selection_records_the_project_path_and_the_hash_of_the_bytes_read() {
    let p = Project::new();
    let file = p.write("checks/orders.sql", "SELECT 1 WHERE false\n");
    let selection = p.prepare(&[file], Some("host=x")).unwrap().unwrap();
    let record = &selection.assertions[0].record;
    assert_eq!(record.path.raw(), "checks/orders.sql");
    assert_eq!(
        record.sha256,
        format!("{:x}", sha2::Sha256::digest(b"SELECT 1 WHERE false\n"))
    );
    assert!(record.is_complete());
}

#[test]
fn an_edited_file_has_another_hash() {
    let p = Project::new();
    let file = p.write("a.sql", "SELECT 1 WHERE false");
    let before = p
        .prepare(std::slice::from_ref(&file), Some("c"))
        .unwrap()
        .unwrap();
    p.write("a.sql", "SELECT 1 WHERE false -- reviewed again");
    let after = p.prepare(&[file], Some("c")).unwrap().unwrap();
    assert_ne!(
        before.assertions[0].record.sha256,
        after.assertions[0].record.sha256
    );
}

#[test]
fn a_missing_or_blank_variable_is_refused_naming_it() {
    let p = Project::new();
    let file = p.write("a.sql", "SELECT 1 WHERE false");
    for conn in [None, Some(""), Some("  \t")] {
        let findings = p
            .prepare(std::slice::from_ref(&file), conn)
            .err()
            .expect("refused");
        assert_eq!(ids(&findings), ["plan.assertion.connection"]);
        assert!(findings[0].message.contains(CONNECTION_VARIABLE));
        assert!(
            findings[0]
                .remedy
                .as_deref()
                .unwrap()
                .contains(CONNECTION_VARIABLE)
        );
    }
}

#[test]
fn an_unreadable_variable_is_not_read_as_a_missing_one() {
    let p = Project::new();
    let file = p.write("a.sql", "SELECT 1 WHERE false");
    let bad = std::env::VarError::NotUnicode(std::ffi::OsString::new());
    let findings = Selection::prepare(&p.root(), &[file], Err(bad))
        .err()
        .unwrap();
    assert!(findings[0].message.contains("not valid UTF-8"));
}

#[test]
fn every_problem_is_reported_together() {
    let p = Project::new();
    let ok = p.write("ok.sql", "SELECT 1 WHERE false");
    let empty = p.write("empty.sql", "  \n");
    let binary = p.write("bad.sql", [0xff, 0xfe, 0x00]);
    let outside = p.0.join("outside.sql");
    std::fs::write(&outside, "SELECT 1").unwrap();
    let missing = p.root().join("missing.sql");
    let findings = p
        .prepare(&[ok.clone(), ok, empty, binary, outside, missing], None)
        .err()
        .unwrap();
    let messages: Vec<_> = findings.iter().map(|f| f.message.as_str()).collect();
    assert_eq!(findings.len(), 6, "{messages:#?}");
    assert_eq!(findings[0].id, "plan.assertion.connection");
    assert!(messages.iter().any(|m| m.contains("same file")));
    assert!(messages.iter().any(|m| m.contains("is empty")));
    assert!(messages.iter().any(|m| m.contains("not valid UTF-8")));
    assert!(messages.iter().any(|m| m.contains("outside the project")));
    assert!(messages.iter().any(|m| m.contains("does not exist")));
}

#[cfg(unix)]
#[test]
fn a_symlink_out_of_the_project_is_refused_and_a_symlink_to_a_twin_is_a_duplicate() {
    let p = Project::new();
    let real = p.write("real.sql", "SELECT 1 WHERE false");
    let outside = p.0.join("elsewhere.sql");
    std::fs::write(&outside, "SELECT 1").unwrap();
    std::os::unix::fs::symlink(&outside, p.root().join("escape.sql")).unwrap();
    std::os::unix::fs::symlink(&real, p.root().join("twin.sql")).unwrap();
    let findings = p
        .prepare(&[p.root().join("escape.sql")], Some("c"))
        .err()
        .unwrap();
    assert!(findings[0].message.contains("outside the project"));
    let findings = p
        .prepare(&[real, p.root().join("twin.sql")], Some("c"))
        .err()
        .unwrap();
    assert!(findings[0].message.contains("same file"));
}

fn refused(stop: Stop) -> Finding {
    match stop {
        Stop::Refused(f) => f,
        Stop::Unanswerable(f) => panic!("expected a refusal, got unanswerable {}", f.id),
    }
}

fn unanswerable(stop: Stop) -> Finding {
    match stop {
        Stop::Unanswerable(f) => f,
        Stop::Refused(f) => panic!("expected unanswerable, got refusal {}", f.id),
    }
}

#[test]
fn only_a_pass_continues_planning() {
    assert!(classify("a.sql", Ok(AssertionResult::Pass(evidence(vec![], false)))).is_ok());
    let not_passes = [
        Ok(AssertionResult::Violation(evidence(
            vec![vec![EvidenceCell::Null]],
            false,
        ))),
        Ok(AssertionResult::InvalidResult {
            message: "no columns".into(),
        }),
        Ok(AssertionResult::SqlError(DbError::BadRow("e".into()))),
        Ok(AssertionResult::BoundaryViolation),
        Ok(AssertionResult::Unanswerable(DbError::BadRow("e".into()))),
        Err(DbError::Refused("no".into())),
        Err(DbError::DeadlineExceeded("late")),
        Err(DbError::BadRow("shape".into())),
    ];
    for result in not_passes {
        let label = format!("{result:?}");
        assert!(classify("a.sql", result).is_err(), "{label} continued");
    }
}

#[test]
fn each_failure_names_its_file_and_its_kind() {
    let rows = classify(
        "checks/a.sql",
        Ok(AssertionResult::Violation(evidence(
            vec![
                vec![EvidenceCell::Text {
                    value: "7".into(),
                    truncated: false,
                }],
                vec![EvidenceCell::Null],
            ],
            true,
        ))),
    )
    .err()
    .map(refused)
    .unwrap();
    assert_eq!(rows.id, "plan.assertion.rows");
    assert!(rows.message.contains("checks/a.sql"));
    assert!(rows.message.contains("2 row(s) or more"));
    assert!(rows.message.contains("id=7"));
    assert!(rows.message.contains("id=NULL"));
    assert_eq!(rows.location.as_ref().unwrap().file, "checks/a.sql");

    let sql = refused(
        classify(
            "a.sql",
            Ok(AssertionResult::SqlError(DbError::BadRow("boom".into()))),
        )
        .err()
        .unwrap(),
    );
    assert_eq!(sql.id, "plan.assertion.sql");
    assert!(sql.message.contains("not a pass"));
    let invalid = refused(
        classify(
            "a.sql",
            Ok(AssertionResult::InvalidResult {
                message: "zero columns".into(),
            }),
        )
        .err()
        .unwrap(),
    );
    assert_eq!(invalid.id, "plan.assertion.sql");
    assert!(invalid.message.contains("invalid result"));
    let boundary = refused(
        classify("a.sql", Ok(AssertionResult::BoundaryViolation))
            .err()
            .unwrap(),
    );
    assert_eq!(boundary.id, "plan.assertion.boundary");
    let admission = refused(
        classify("a.sql", Err(DbError::Refused("login can write".into())))
            .err()
            .unwrap(),
    );
    assert_eq!(admission.id, "plan.assertion.refused");
    assert!(admission.message.contains("login can write"));
}

#[test]
fn a_deadline_or_an_unreadable_answer_is_unanswerable_not_a_refusal() {
    for result in [
        Err(DbError::DeadlineExceeded("assertion deadline exceeded")),
        Err(DbError::BadRow("unreadable".into())),
        Ok(AssertionResult::Unanswerable(DbError::BadRow("cut".into()))),
    ] {
        let f = unanswerable(classify("a.sql", result).err().unwrap());
        assert_eq!(f.id, "plan.assertion.unanswerable");
        assert!(f.message.contains("a.sql"));
    }
}

#[test]
fn rows_shown_with_a_violation_are_bounded_and_escaped() {
    let many: Vec<_> = (0..10)
        .map(|n| {
            vec![EvidenceCell::Text {
                value: format!("v{n}\u{1b}[31m"),
                truncated: false,
            }]
        })
        .collect();
    let text = violation("assertion `a.sql`", &evidence(many, false));
    assert!(text.contains("10 row(s)"));
    assert_eq!(text.lines().count(), 1 + SHOWN_ROWS);
    assert!(!text.contains('\u{1b}'));
}

#[test]
fn an_unanswerable_assertion_keeps_its_own_finding_through_the_envelope() {
    let finding = unanswerable(
        classify("checks/a.sql", Err(DbError::DeadlineExceeded("late")))
            .err()
            .unwrap(),
    );
    // Wrapped in context, as planning wraps its failures.
    let error = anyhow::Error::new(Unanswerable(finding)).context("planning");
    let findings = crate::output::failure_findings(&error, "plan.failed", Vec::new());
    assert_eq!(ids(&findings), ["plan.assertion.unanswerable"]);
    assert_eq!(findings[0].location.as_ref().unwrap().file, "checks/a.sql");
    // Any other failure keeps the generic id.
    let other = anyhow::anyhow!("boom");
    assert_eq!(
        ids(&crate::output::failure_findings(
            &other,
            "plan.failed",
            Vec::new()
        )),
        ["plan.failed"]
    );
}
