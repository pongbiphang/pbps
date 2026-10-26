use super::*;

const BASE: &str = "0123456789abcdef0123456789abcdef01234567";
const IMAGE: &str =
    "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280";

fn manifest(extra: &str) -> String {
    format!(
        "version: 1\ndialect: postgres\nbase: {BASE}\nengine:\n  image: {IMAGE}\nfixtures: []\n\
         assertions:\n  - name: a\n    file: a.sql\n{extra}"
    )
}

/// A manifest with one key's line replaced, to vary one thing at a time.
fn with(key: &str, line: &str) -> String {
    manifest("")
        .lines()
        .map(|l| if l.starts_with(key) { line } else { l })
        .collect::<Vec<_>>()
        .join("\n")
}

fn errors(text: &str) -> Vec<ManifestError> {
    TestManifest::parse(text).expect_err("the manifest is refused")
}

fn syntax(text: &str) -> String {
    match errors(text).as_slice() {
        [ManifestError::Syntax(message)] => message.clone(),
        other => panic!("expected one syntax error, got {other:?}"),
    }
}

#[test]
fn the_adr_example_parses_with_every_field() {
    let text = format!(
        "version: 1\ndialect: postgres\nbase: {BASE}\ncandidate: working-tree\nengine:\n  \
         image: {IMAGE}\nfixtures:\n  - name: orders\n    file: fixtures/orders.sql\n    at: {BASE}\n\
         assertions:\n  - name: preserved-orders\n    file: assertions/preserved-orders.sql\n\
         timeouts:\n  startup_ms: 120000\n  step_ms: 120000\n  fixture_ms: 30000\n  \
         assertion_ms: 10000\n  run_ms: 600000\n  cleanup_ms: 30000\n"
    );
    let parsed = TestManifest::parse(&text).unwrap();
    assert_eq!(parsed.dialect, DialectName::Postgres);
    assert_eq!(parsed.base.as_str(), BASE);
    assert_eq!(parsed.candidate, Candidate::WorkingTree);
    assert_eq!(parsed.image.to_string(), IMAGE);
    assert_eq!(parsed.image.name(), "postgres");
    assert_eq!(parsed.fixtures[0].name, "orders");
    assert_eq!(parsed.fixtures[0].at.as_str(), BASE);
    assert_eq!(parsed.assertions[0].file, "assertions/preserved-orders.sql");
    assert_eq!(parsed.timeouts, Timeouts::default());
}

#[test]
fn omitted_candidate_and_timeouts_take_their_defaults() {
    let parsed = TestManifest::parse(&manifest("")).unwrap();
    assert_eq!(parsed.candidate, Candidate::WorkingTree);
    assert_eq!(parsed.timeouts, Timeouts::default());
    let partial = TestManifest::parse(&manifest("timeouts:\n  run_ms: 5\n")).unwrap();
    assert_eq!(partial.timeouts.run_ms.get(), 5);
    assert_eq!(partial.timeouts.step_ms, Timeouts::default().step_ms);
}

#[test]
fn an_unknown_key_is_refused_at_every_level() {
    for text in [
        manifest("surprise: 1\n"),
        manifest("timeouts:\n  forever_ms: 1\n"),
        manifest("").replace("engine:\n", "engine:\n  tag: 16\n"),
        format!(
            "{}  - name: b\n    file: b.sql\n    extra: 1\n",
            manifest("")
        ),
        with(
            "fixtures:",
            "fixtures:\n  - name: f\n    file: f.sql\n    at: x\n    extra: 1",
        ),
    ] {
        assert!(syntax(&text).contains("unknown"), "{text}");
    }
}

#[test]
fn a_duplicate_mapping_key_is_refused() {
    let text = manifest("base: ffffffffffffffffffffffffffffffffffffffff\n");
    assert!(syntax(&text).to_lowercase().contains("duplicate"));
}

#[test]
fn a_second_yaml_document_is_not_silently_ignored() {
    let text = format!("{}---\nversion: 1\n", manifest(""));
    assert!(TestManifest::parse(&text).is_err());
}

#[test]
fn an_unsupported_version_is_reported_before_keys_this_build_does_not_know() {
    let future = "version: 2\ndialect: postgres\nsomething_new: [1]\n";
    assert_eq!(
        errors(future),
        [ManifestError::UnsupportedVersion("2".into())]
    );
    assert!(matches!(
        errors("version: one\nbase: x\n").as_slice(),
        [ManifestError::UnsupportedVersion(_)]
    ));
    assert_eq!(
        errors("version: 0\n"),
        [ManifestError::UnsupportedVersion("0".into())]
    );
}

#[test]
fn a_manifest_without_a_version_is_not_version_one() {
    let text = manifest("").replace("version: 1\n", "");
    assert_eq!(errors(&text), [ManifestError::MissingVersion]);
}

#[test]
fn every_required_key_is_required() {
    for key in ["dialect:", "base:", "fixtures:", "assertions:"] {
        let text: String = manifest("")
            .lines()
            .filter(|l| !l.starts_with(key))
            .map(|l| format!("{l}\n"))
            .collect();
        let text = text.replace("  - name: a\n    file: a.sql\n", "");
        let message = syntax(&text);
        assert!(message.contains("missing"), "{key}: {message}");
    }
    let no_engine = manifest("").replace(&format!("engine:\n  image: {IMAGE}\n"), "");
    assert!(syntax(&no_engine).contains("missing"));
}

#[test]
fn an_empty_fixture_list_is_accepted_and_an_empty_assertion_list_is_not() {
    assert!(
        TestManifest::parse(&manifest(""))
            .unwrap()
            .fixtures
            .is_empty()
    );
    let none = manifest("")
        .replace("  - name: a\n    file: a.sql\n", "")
        .replace("assertions:", "assertions: []");
    assert_eq!(errors(&none), [ManifestError::NoAssertions]);
}

#[test]
fn a_pin_must_be_a_full_commit_id() {
    for value in [
        "main",
        "v1.0",
        "HEAD",
        "0123456",
        &BASE[..39],
        &format!("{BASE}0"),
        &BASE.replace('0', "g"),
    ] {
        let text = with("base:", &format!("base: \"{value}\""));
        assert!(
            matches!(errors(&text).as_slice(), [ManifestError::InvalidCommit { field, .. }] if field == "base"),
            "{value}"
        );
    }
    let sha256 = "ab".repeat(32);
    let ok = TestManifest::parse(&with("base:", &format!("base: {sha256}"))).unwrap();
    assert_eq!(ok.base.as_str(), sha256);
}

#[test]
fn a_pin_is_normalised_to_lowercase() {
    let upper = BASE.to_ascii_uppercase();
    let parsed = TestManifest::parse(&with("base:", &format!("base: {upper}"))).unwrap();
    assert_eq!(parsed.base, CommitId::parse(BASE).unwrap());
}

#[test]
fn a_fixture_pin_is_judged_like_the_base() {
    let text = with(
        "fixtures:",
        "fixtures:\n  - name: f\n    file: f.sql\n    at: main",
    );
    assert!(matches!(
        errors(&text).as_slice(),
        [ManifestError::InvalidCommit { field, .. }] if field == "fixtures[0].at"
    ));
}

#[test]
fn an_image_must_be_named_by_digest_alone() {
    let digest = "a".repeat(64);
    for bad in [
        "postgres:16".to_owned(),
        "postgres".to_owned(),
        format!("postgres:16@sha256:{digest}"),
        format!("postgres@sha512:{digest}"),
        format!("postgres@sha256:{}", &digest[1..]),
        format!("postgres@sha256:{}", "A".repeat(64)),
        format!("@sha256:{digest}"),
        format!("-p@sha256:{digest}"),
        format!("po stgres@sha256:{digest}"),
        format!("a@b@sha256:{digest}"),
    ] {
        let text = with("  image:", &format!("  image: \"{bad}\""));
        assert!(
            matches!(
                errors(&text).as_slice(),
                [ManifestError::InvalidImage { .. }]
            ),
            "{bad}"
        );
    }
    let port = format!("localhost:5000/team/postgres@sha256:{digest}");
    let parsed = TestManifest::parse(&with("  image:", &format!("  image: {port}"))).unwrap();
    assert_eq!(parsed.image.digest(), format!("sha256:{digest}"));
    assert_eq!(parsed.image.name(), "localhost:5000/team/postgres");
}

#[test]
fn a_candidate_is_the_working_tree_or_a_revision_git_can_be_asked_about() {
    let rev = |text: &str| TestManifest::parse(&manifest(&format!("candidate: \"{text}\"\n")));
    assert_eq!(
        rev("HEAD").unwrap().candidate,
        Candidate::Revision("HEAD".into())
    );
    assert_eq!(
        rev("HEAD~2").unwrap().candidate,
        Candidate::Revision("HEAD~2".into())
    );
    assert_eq!(
        rev("working-tree").unwrap().candidate,
        Candidate::WorkingTree
    );
    // Git, not this parser, judges the expression; these have spaces.
    for spaced in ["HEAD^{/read the manifest}", "HEAD@{1 day ago}"] {
        assert_eq!(
            rev(spaced).unwrap().candidate,
            Candidate::Revision(spaced.into())
        );
    }
    for bad in ["", "--upload-pack=x", "-x", "a\\nb", "a\\tb"] {
        assert!(
            matches!(
                rev(bad).unwrap_err().as_slice(),
                [ManifestError::InvalidCandidate { .. }]
            ),
            "{bad:?}"
        );
    }
}

#[test]
fn a_timeout_is_a_positive_u32_and_names_its_field() {
    let max =
        TestManifest::parse(&manifest(&format!("timeouts:\n  step_ms: {}\n", u32::MAX))).unwrap();
    assert_eq!(max.timeouts.step_ms.get(), u32::MAX);
    for (field, value) in [("step_ms", 0u64), ("run_ms", u64::from(u32::MAX) + 1)] {
        let text = manifest(&format!("timeouts:\n  {field}: {value}\n"));
        assert_eq!(
            errors(&text),
            [ManifestError::TimeoutOutOfRange { field, value }]
        );
    }
    for bad in ["-1", "1.5", "soon"] {
        syntax(&manifest(&format!("timeouts:\n  step_ms: {bad}\n")));
    }
}

#[test]
fn names_are_nonempty_and_unique_within_their_list_only() {
    let text = format!("{}  - name: a\n    file: b.sql\n", manifest(""));
    assert_eq!(
        errors(&text),
        [ManifestError::DuplicateName {
            list: Entries::Assertions,
            name: "a".into(),
            first: 0,
            second: 1
        }]
    );
    let empty = with("  - name:", "  - name: \"\"");
    assert_eq!(
        errors(&empty),
        [ManifestError::EmptyField {
            list: Entries::Assertions,
            index: 0,
            field: "name"
        }]
    );
    // The same name in the other list is another entry.
    let both = with(
        "fixtures:",
        "fixtures:\n  - name: a\n    file: f.sql\n    at: 0123456789abcdef0123456789abcdef01234567",
    );
    assert!(TestManifest::parse(&both).is_ok());
}

#[test]
fn a_file_path_is_relative_and_its_containment_is_left_to_the_caller() {
    assert_eq!(
        errors(&with("    file:", "    file: /etc/passwd")),
        [ManifestError::NotRelative {
            list: Entries::Assertions,
            name: "a".into(),
            path: "/etc/passwd".into()
        }]
    );
    assert_eq!(
        errors(&with("    file:", "    file: \"\"")),
        [ManifestError::EmptyField {
            list: Entries::Assertions,
            index: 0,
            field: "file"
        }]
    );
    // Whether `..` leaves the project depends on where the manifest is.
    assert!(TestManifest::parse(&with("    file:", "    file: ../a.sql")).is_ok());
}

#[test]
fn every_problem_is_reported_in_one_pass() {
    let text = with("base:", "base: main")
        .replace(IMAGE, "postgres:16")
        .replace(
            "  - name: a\n",
            "  - name: a\n    file: a.sql\n  - name: a\n",
        );
    let found = errors(&text);
    assert!(
        found
            .iter()
            .any(|e| matches!(e, ManifestError::InvalidCommit { .. }))
    );
    assert!(
        found
            .iter()
            .any(|e| matches!(e, ManifestError::InvalidImage { .. }))
    );
    assert!(
        found
            .iter()
            .any(|e| matches!(e, ManifestError::DuplicateName { .. }))
    );
}

#[test]
fn messages_show_user_text_escaped() {
    let shown = ManifestError::InvalidCommit {
        field: "base".into(),
        value: "a\nb".into(),
    }
    .to_string();
    assert!(!shown.contains('\n'));
}
