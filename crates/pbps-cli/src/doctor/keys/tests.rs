use super::*;
use crate::output::Severity;
use std::path::{Path, PathBuf};

fn project() -> Project {
    Project {
        root: PathBuf::from("unused"),
        config: pbps_config::Config::parse(
            "dialect: postgres\nenvironments:\n  a: {url_env: A}\n  b: {url_env: B}\n  c: {url_env: C}\n",
            Path::new("pbps.yml"),
        ).unwrap(),
    }
}

fn id() -> KeyId {
    FingerprintKey::parse(&FingerprintKey::generate(), "private test fixture")
        .unwrap()
        .id()
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn only_selected_environment_keys_are_loaded() {
    let project = project();
    let id = id();
    for (request, expected) in [
        (None, vec!["a", "b", "c"]),
        (
            Some(Requested {
                name: Some("b".into()),
                target: Err(anyhow::anyhow!("key-only test does not resolve a target")),
            }),
            vec!["b"],
        ),
        (
            Some(Requested {
                name: None,
                target: Err(anyhow::anyhow!("key-only test does not resolve a target")),
            }),
            vec![],
        ),
    ] {
        let mut calls = Vec::new();
        let findings = for_request_with(&project, request.as_ref(), |name| {
            calls.push(name.to_owned());
            Loaded::Key(id.clone())
        });
        assert_eq!(calls, expected);
        assert_eq!(findings.len(), expected.len());
        if findings.len() == 1 {
            assert_eq!(findings[0].severity, Severity::Note);
        }
    }
}

#[test]
fn duplicate_groups_replace_all_healthy_notes_without_hiding_other_results() {
    let shared = id();
    let unique = id();
    assert_ne!(shared, unique);
    let requested = names(&["fault", "c", "bare", "b", "unique", "a", "c"]);
    let mut calls = Vec::new();
    let findings = classify(&requested, |name| {
        calls.push(name.to_owned());
        match name {
            "fault" => Loaded::Finding(Finding::error(
                "environment.fingerprint-key",
                "fixture unreadable",
            )),
            "bare" => Loaded::Finding(Finding::note(
                "environment.no-fingerprint-key",
                "fixture no source",
            )),
            "unique" => Loaded::Key(unique.clone()),
            _ => Loaded::Key(shared.clone()),
        }
    });
    assert_eq!(calls, names(&["fault", "c", "bare", "b", "unique", "a"]));
    assert_eq!(findings.len(), 6);
    assert_eq!(findings[0].message, "fixture unreadable");
    assert_eq!(findings[2].id, "environment.no-fingerprint-key");
    assert_eq!(findings[4].severity, Severity::Note);
    assert!(findings[4].message.contains(unique.as_str()));
    for (index, name) in [(1, "c"), (3, "b"), (5, "a")] {
        let finding = &findings[index];
        assert_eq!(finding.id, "environment.shared-fingerprint-key");
        assert_eq!(finding.severity, Severity::Error);
        assert!(
            finding
                .message
                .starts_with(&format!("environment `{name}`"))
        );
        assert!(finding.message.contains("`a`, `b`, `c`"));
        assert!(finding.message.contains(shared.as_str()));
    }
}

#[test]
fn a_repeated_name_is_one_environment_and_failures_are_not_key_groups() {
    let id = id();
    let mut calls = 0;
    let findings = classify(&names(&["a", "a"]), |_| {
        calls += 1;
        Loaded::Key(id.clone())
    });
    assert_eq!(calls, 1);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].severity, Severity::Note);
    assert!(classify(&[], |_| panic!("no selected secrets")).is_empty());
    let findings = classify(&names(&["a", "b"]), |_| {
        Loaded::Finding(Finding::error("environment.fingerprint-key", "same error"))
    });
    assert_eq!(findings.len(), 2);
    assert!(
        findings
            .iter()
            .all(|finding| finding.id == "environment.fingerprint-key")
    );
    let findings = classify(&names(&["a", "b"]), |_| Loaded::Key(self::id()));
    assert!(
        findings
            .iter()
            .all(|finding| finding.severity == Severity::Note)
    );
}

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pbps-key964-{}",
            pbps_model::Uid::generate(pbps_model::UidKind::Table)
        ));
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
#[test]
fn two_distinct_key_files_with_one_id_are_errors_in_the_report() {
    use crate::doctor::{Diagnosis, outcome, unanswerable};
    use crate::output::{self, Outcome, Report};
    use std::os::unix::fs::PermissionsExt;
    let dir = Directory::new();
    let secret = FingerprintKey::generate();
    let expected = FingerprintKey::parse(&secret, "test").unwrap().id();
    for name in ["one.key", "two.key"] {
        let path = dir.0.join(name);
        std::fs::write(&path, &secret).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    std::fs::write(dir.0.join("pbps.yml"), "dialect: postgres\nenvironments:\n  a: {url_env: A, fingerprint_key_file: one.key}\n  b: {url_env: B, fingerprint_key_file: two.key}\n").unwrap();
    let project = Project::load(&dir.0.join("pbps.yml")).unwrap();
    let findings = for_request(&project, None);
    assert_eq!(findings.len(), 2);
    assert!(
        findings
            .iter()
            .all(|finding| finding.severity == Severity::Error
                && finding.id == "environment.shared-fingerprint-key")
    );
    let report = Report::<Diagnosis>::new("doctor", findings, None);
    assert_eq!(report.result, Outcome::Findings);
    assert_eq!(unanswerable(&report), 0);
    assert!(
        outcome(&report)
            .unwrap_err()
            .downcast_ref::<crate::Found>()
            .is_some()
    );
    let human = output::human(&report.findings);
    let json = serde_json::to_string(&report).unwrap();
    for rendered in [&human, &json] {
        assert!(rendered.contains(expected.as_str()));
        assert!(rendered.contains("`a`"));
        assert!(rendered.contains("`b`"));
        assert!(!rendered.contains(&secret));
    }
    let json: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(json["result"], "findings");
    assert_eq!(json["findings"][0]["severity"], "error");

    // The actual selected path must not inspect the other now-unreadable key.
    std::fs::remove_file(dir.0.join("two.key")).unwrap();
    let request = Requested {
        name: Some("a".into()),
        target: Err(anyhow::anyhow!("key-only test does not resolve a target")),
    };
    let selected = for_request(&project, Some(&request));
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].severity, Severity::Note);
    let raw = Requested {
        name: None,
        target: Err(anyhow::anyhow!("key-only test does not resolve a target")),
    };
    assert!(for_request(&project, Some(&raw)).is_empty());

    let mut findings = report.findings;
    findings.push(Finding::error(
        "environment.unreachable",
        "independent failure",
    ));
    let report = Report::<Diagnosis>::new("doctor", findings, None).unanswerable();
    assert_eq!(report.result, Outcome::Unanswerable);
    assert_eq!(unanswerable(&report), 1);
    assert!(
        outcome(&report)
            .unwrap_err()
            .downcast_ref::<crate::Found>()
            .is_none()
    );
}

fn git(root: &Path, args: &[&str]) -> String {
    let result = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().to_owned()
}

#[test]
fn historical_shared_sources_keep_the_baseline_refusal_cause() {
    let dir = Directory::new();
    git(&dir.0, &["init", "--quiet"]);
    let config = dir.0.join("pbps.yml");
    let shared = "dialect: postgres\nenvironments:\n  a: {url_env: A, fingerprint_key_file: old.key}\n  b: {url_env: B, fingerprint_key_file: old.key}\n";
    std::fs::write(&config, shared).unwrap();
    git(&dir.0, &["add", "pbps.yml"]);
    git(
        &dir.0,
        &["commit", "--quiet", "-m", "historical shared source"],
    );
    let old = git(&dir.0, &["rev-parse", "HEAD"]);
    let distinct = shared.replace(
        "b: {url_env: B, fingerprint_key_file: old.key}",
        "b: {url_env: B, fingerprint_key_file: different-old.key}",
    );
    std::fs::write(&config, &distinct).unwrap();
    git(&dir.0, &["add", "pbps.yml"]);
    git(
        &dir.0,
        &["commit", "--quiet", "-m", "distinct historical sources"],
    );
    let valid = git(&dir.0, &["rev-parse", "HEAD"]);
    let project = Project::load(&config).unwrap();
    let error = crate::baseline::config_at(&project, &old)
        .unwrap_err()
        .to_string();
    assert!(error.contains("does not parse"), "{error}");
    assert!(error.contains("share one fingerprint key file"), "{error}");
    assert!(error.contains("`a`") && error.contains("`b`"), "{error}");
    assert!(
        crate::baseline::config_at(&project, &valid)
            .unwrap()
            .is_some()
    );
    assert!(!dir.0.join("old.key").exists());
    assert!(!dir.0.join("different-old.key").exists());
    assert_eq!(std::fs::read_to_string(config).unwrap(), distinct);
    assert_eq!(git(&dir.0, &["status", "--porcelain"]), "");
}
