//! Saved resolver artifacts: reviewable from the file alone, and approved
//! through the ordinary command (#616).

use pbps_model::resolver::*;
use pbps_model::{ChangeSet, IdsFile, PlanBaseline, PlanOrigin, SavedPlan};
use std::collections::BTreeSet;
use std::process::{Command, Output};

#[path = "support/envelope_archives.rs"]
mod envelope_archives;

fn resolved_plan() -> SavedPlan {
    resolved_plan_with(Default::default())
}

fn resolved_plan_with(limitations: std::collections::BTreeMap<String, String>) -> SavedPlan {
    let plan = sealed_under(pbps_pg::resolver::compatibility::REPORTED_RULE, limitations);
    let PlanAnalysis::Resolved(evidence) = &plan.analysis else {
        unreachable!("sealed_under resolves the plan");
    };
    pbps_pg::resolver::validate_evidence(evidence).unwrap();
    plan
}

/// A plan whose evidence names `rule`, valid or not: what an earlier build
/// sealed, with the checksum that build computed.
fn sealed_under(rule: &str, limitations: std::collections::BTreeMap<String, String>) -> SavedPlan {
    // Synthetic, source-free reader evidence, not a claim of live qualification.
    // Use the public sealing constructors so an invalid fixture cannot turn an
    // explanation regression into an unrelated deserialization refusal.
    let changes = ChangeSet::default();
    let manifest = InputManifest::new(
        "postgres-catalog-inputs-v2".into(),
        18,
        "01".repeat(8),
        ReadScope {
            retained: BTreeSet::new(),
            candidates: BTreeSet::new(),
        },
        "02".repeat(32),
        "03".repeat(32),
        vec![],
        vec![],
        BTreeSet::new(),
        vec![],
    )
    .unwrap();
    let evidence = ResolverEvidence::new(
        &changes,
        Qualification {
            rule: rule.into(),
            target_environment: "04".repeat(32),
            target_environment_after: "04".repeat(32),
            resolver_environment: "05".repeat(32),
            target_build: "06".repeat(32),
            resolver_build: "07".repeat(32),
            channels: "08".repeat(32),
            target_cluster: "09".repeat(32),
            limitations,
            target_scope: pbps_model::resolver::TargetScope {
                schemas: vec!["app".into()],
                write_path_extras: vec![],
            },
            runtime: ResolverRuntime::Scratch {
                entry: "scratch".into(),
                scratch: ScratchSource::Server {},
                layout: ScratchLayout::Supplied,
                outside_writes: OutsideWrites::Refused,
            },
        },
        AuthorizationCondition {
            rule: pbps_pg::resolver::authorization::RULE.into(),
            before: "10".repeat(32),
            after: "10".repeat(32),
            changes: BTreeSet::new(),
        },
        manifest.clone(),
        &manifest,
        vec![],
        vec![],
        OrderingProof::new(&changes, BTreeSet::new()).unwrap(),
    )
    .unwrap();
    SavedPlan::new(
        PlanOrigin::Database,
        "postgres",
        "fixture",
        PlanBaseline {
            time_zone: None,
            description: "target database".into(),
            checksum: "00".repeat(32),
            database_collation: None,
        },
        changes,
        IdsFile::default(),
    )
    .with_resolution(evidence)
    .unwrap()
}

struct Artifact(std::path::PathBuf);
impl Artifact {
    fn new(plan: &serde_json::Value) -> Self {
        let dir = std::env::temp_dir().join(format!("pbps-explain-1217-{}", rand::random::<u64>()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("plan.json"), serde_json::to_vec(plan).unwrap()).unwrap();
        Self(dir)
    }

    fn explain(&self, format: &str) -> Output {
        // No project, key, target, resolver selection or credentials: the file
        // is enough, including for a Linux artifact reviewed on Windows.
        Command::new(env!("CARGO_BIN_EXE_pbps"))
            .current_dir(&self.0)
            .arg("--project")
            .arg(&self.0)
            .args(["explain", "--plan", "plan.json", "--format", format])
            .output()
            .unwrap()
    }
}
impl Drop for Artifact {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn report(out: &Output) -> serde_json::Value {
    let value = serde_json::from_slice(&out.stdout).unwrap();
    envelope_archives::assert_accepted_by_archives(&value, "resolver explanation");
    pbps_ui::contract::parse("explain", &out.stdout, out.status.code().unwrap())
        .expect("the viewer must accept the CLI's actual envelope");
    value
}

/// A resolved plan is approved as any database plan is, with the ordinary
/// command and checksum (#616, #618): nothing about resolver evidence changes
/// what a reviewer types or what the gate compares.
#[test]
fn a_resolved_plan_is_offered_the_ordinary_approval_command() {
    let plan = resolved_plan();
    plan.validate_analysis().unwrap();
    assert_eq!(plan.origin, PlanOrigin::ResolvedDatabase);
    let artifact = Artifact::new(&serde_json::to_value(&plan).unwrap());
    let approve = format!(
        "pbps apply --env \"<environment>\" --plan plan.json --checksum {}",
        plan.checksum()
    );
    for format in ["text", "json"] {
        let out = artifact.explain(format);
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{text} {:?}", out.stderr);
        assert!(out.stderr.is_empty(), "{:?}", out.stderr);
        for forbidden in ["pbps plan", "offline", "plan.preview", "cannot be applied"] {
            assert!(!text.contains(forbidden), "{forbidden}: {text}");
        }
        if format == "json" {
            let value = report(&out);
            assert_eq!(value["data"]["applyable"], true, "{value}");
            assert_eq!(value["data"]["approve_with"], approve.as_str(), "{value}");
            assert_eq!(value["data"]["checksum"], plan.checksum());
            assert_eq!(value["findings"], serde_json::json!([]), "{value}");
            assert!(value["data"].get("target").is_none(), "{value}");
        } else {
            assert!(text.contains("To approve and run it"), "{text}");
            assert!(text.contains(&approve), "{text}");
            assert!(
                text.contains(
                    "computed against the target with sealed resolver evidence — applyable"
                ),
                "{text}"
            );
        }
    }
    // Negative: the same evidence in a staged plan is refused by name, since
    // it describes one transaction.
    let staged = Artifact::new(&serde_json::to_value(resolved_plan().staged()).unwrap());
    let out = staged.explain("text");
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("this plan is staged"),
        "{out:?}"
    );
}

#[test]
fn ordinary_database_approval_and_preview_remedy_keep_their_meaning() {
    for origin in [PlanOrigin::Database, PlanOrigin::Preview] {
        let mut plan = resolved_plan();
        plan.origin = origin;
        plan.analysis = PlanAnalysis::Ordinary;
        let artifact = Artifact::new(&serde_json::to_value(&plan).unwrap());
        for format in ["text", "json"] {
            let out = artifact.explain(format);
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success(), "{text} {:?}", out.stderr);
            if format == "json" {
                let value = report(&out);
                assert_eq!(value["data"]["applyable"], origin == PlanOrigin::Database);
                // An ordinary plan carries no resolver provenance (#1517).
                assert!(value["data"].get("resolver").is_none(), "{value}");
            } else {
                assert!(!text.contains("resolver"), "{text}");
            }
            if origin == PlanOrigin::Database {
                assert!(text.contains("pbps apply --env"), "{text}");
                assert!(text.contains(&plan.checksum()), "{text}");
                assert!(!text.contains("pbps plan"), "{text}");
            } else {
                assert!(text.contains("pbps plan --env"), "{text}");
                assert!(text.contains("offline"), "{text}");
                assert!(!text.contains("pbps apply"), "{text}");
            }
        }
    }
}

/// The plan alone tells a reviewer where its evidence came from and what
/// it seals, in human and JSON output alike, with no project, credentials
/// or resolver (#1517). The JSON is the shared field the human line and
/// the viewer read.
#[test]
fn a_resolved_plan_explains_its_provenance_and_sealed_qualification() {
    let limited = resolved_plan_with(
        [(
            "build:server_version".to_owned(),
            "the scratch reports another build string of the same version".to_owned(),
        )]
        .into(),
    );
    let artifact = Artifact::new(&serde_json::to_value(&limited).unwrap());
    let value = report(&artifact.explain("json"));
    assert_eq!(
        value["data"]["resolver"],
        serde_json::json!({
            "entry": "scratch",
            "scratch": "server",
            "layout": "supplied",
            "outside_writes": "refused",
            "rule": pbps_pg::resolver::compatibility::REPORTED_RULE,
            "key_id": "01".repeat(8),
            "sealed": [
                "target environment",
                "target environment after the plan's grants",
                "resolver environment",
                "target build",
                "resolver build",
                "channels",
                "target cluster identity",
            ],
            "limitations": [{
                "name": "build:server_version",
                "description": "the scratch reports another build string of the same version",
            }],
            "surfaces": [],
            "order": [],
            "prerequisites": [],
        })
    );
    let text = String::from_utf8(artifact.explain("text").stdout).unwrap();
    assert!(
        text.contains("  resolver    scratch (server scratch, supplied)\n"),
        "{text}"
    );
    let section = &text[text.find("\nResolver evidence\n").expect(&text)..];
    let section = &section[..section.find("\nThe environment\n").unwrap()];
    assert_eq!(
        section,
        format!(
            "\nResolver evidence\n  rule        {}\n  key id      {}\n  sealed      \
             target environment, target environment after the plan's grants, resolver \
             environment, target build, resolver build, channels, target cluster identity \
             (keyed fingerprints; values not shown)\n  limitations accepted, not refused:\n    \
             build:server_version: the scratch reports another build string of the same \
             version\n  surfaces\n",
            pbps_pg::resolver::compatibility::REPORTED_RULE,
            "01".repeat(8)
        )
    );
    // No fingerprint value is shown, in either form.
    let json = serde_json::to_string(&value).unwrap();
    for digest in ["04", "05", "06", "07", "08", "09"] {
        let value = digest.repeat(32);
        assert!(!json.contains(&value) && !text.contains(&value), "{digest}");
    }

    // Negative: a plan that seals no limitation shows none.
    let plain = Artifact::new(&serde_json::to_value(resolved_plan()).unwrap());
    let value = report(&plain.explain("json"));
    assert_eq!(
        value["data"]["resolver"]["limitations"],
        serde_json::json!([])
    );
    let text = String::from_utf8(plain.explain("text").stdout).unwrap();
    assert!(text.contains("  limitations none\n"), "{text}");
    assert!(!text.contains("build:server_version"), "{text}");
}

#[test]
fn malformed_or_unsupported_resolver_evidence_is_refused_before_presentation() {
    let valid = serde_json::to_value(resolved_plan()).unwrap();
    for (path, replacement) in [
        ("/analysis", serde_json::json!({"kind": "ordinary"})),
        ("/analysis/evidence/version", serde_json::json!(99)),
        (
            "/analysis/evidence/before/adapter",
            serde_json::json!("postgres-catalog-inputs-v99"),
        ),
        // A runtime this build removed: the measured profiles (#1636).
        (
            "/analysis/evidence/qualification/runtime",
            serde_json::json!({
                "kind": "supplied",
                "profile": "linux-dedicated-v1",
                "identity": "09".repeat(32),
            }),
        ),
    ] {
        let mut value = valid.clone();
        *value.pointer_mut(path).unwrap() = replacement;
        let artifact = Artifact::new(&value);
        for format in ["text", "json"] {
            let out = artifact.explain(format);
            assert_eq!(out.status.code(), Some(1), "{path}: {out:?}");
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(!text.contains("pbps apply"), "{text}");
            if format == "json" {
                let value = report(&out);
                assert_eq!(value["result"], "unanswerable", "{value}");
                assert!(
                    value.get("data").is_none_or(serde_json::Value::is_null),
                    "{value}"
                );
            } else {
                assert!(text.is_empty(), "{text}");
            }
        }
    }
}

/// The qualification rule's identifier moves with the facts it compares
/// (DEC-2012.1), so a resolved plan an earlier build sealed is refused by
/// `explain` and by `apply` before the target is contacted, naming the rule
/// and the remedy, and is never shown as this build's coverage.
#[test]
fn a_plan_sealed_under_the_previous_qualification_rule_is_refused_naming_it() {
    let previous = sealed_under("pg-reported-scope-v1", Default::default());
    let current = resolved_plan();
    assert_ne!(previous.checksum(), current.checksum());
    let artifact = Artifact::new(&serde_json::to_value(&previous).unwrap());
    for format in ["text", "json"] {
        let out = artifact.explain(format);
        assert_eq!(out.status.code(), Some(1), "{format}: {out:?}");
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(
            error.contains("`pg-reported-scope-v1`"),
            "{format}: {error}"
        );
        assert!(error.contains("plan again"), "{format}: {error}");
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("pbps apply"),
            "{format}: {out:?}"
        );
    }
    std::fs::write(artifact.0.join("pbps.yml"), "dialect: postgres\n").unwrap();
    let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let connection = format!(
        "postgres://unused@{}/unused?sslmode=disable",
        target.local_addr().unwrap()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_pbps"))
        .current_dir(&artifact.0)
        .args([
            "apply",
            "--plan",
            "plan.json",
            "--db",
            &connection,
            "--checksum",
            &previous.checksum(),
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(error.contains("`pg-reported-scope-v1`"), "{error}");
    assert_eq!(
        target.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );

    // Negative: the same fixture under the current rule is read, so the
    // refusal above is the rule's and not the fixture's.
    let readable = Artifact::new(&serde_json::to_value(&current).unwrap());
    let out = readable.explain("text");
    assert!(out.status.success(), "{out:?}");
}

/// Text a hand-made plan can put in a value that a refusal names: a line break
/// that starts a line the tool never wrote, a terminal sequence, a carriage
/// return that overwrites one, and a right-to-left override.
const HOSTILE: &str = "x\n\u{1b}[31mred\r\u{202e}";
/// What a refusal shows of [`HOSTILE`] (DEC-2032.1).
const HOSTILE_SHOWN: &str = "x\\n\\u{1b}[31mred\\r\\u{202e}";

/// A refusal is the one line `error: ...` and its own final newline: nothing
/// the file held acts on the terminal, and the file's value is still in it.
fn assert_inert(out: &Output, shown: &[&str], what: &str) {
    assert_eq!(out.status.code(), Some(1), "{what}: {out:?}");
    let error = String::from_utf8_lossy(&out.stderr);
    let body = error.strip_suffix('\n').unwrap_or(&error);
    assert!(
        !body.chars().any(char::is_control),
        "{what}: a raw control character in {error:?}"
    );
    assert!(body.starts_with("error: "), "{what}: {error:?}");
    for fragment in shown {
        assert!(
            body.contains(fragment),
            "{what}: {fragment:?} not in {error:?}"
        );
    }
}

/// `apply` on `plan.json` against a target that must never be contacted.
fn apply_refused(artifact: &Artifact, checksum: &str) -> Output {
    std::fs::write(artifact.0.join("pbps.yml"), "dialect: postgres\n").unwrap();
    let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let connection = format!(
        "postgres://unused@{}/unused?sslmode=disable",
        target.local_addr().unwrap()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_pbps"))
        .current_dir(&artifact.0)
        .args([
            "apply",
            "--plan",
            "plan.json",
            "--db",
            &connection,
            "--checksum",
            checksum,
        ])
        .output()
        .unwrap();
    assert_eq!(
        target.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    out
}

/// A refusal that names a value out of the plan file shows it escaped, so a
/// hand-made plan cannot forge or hide terminal output in the flow that reads
/// a handed-off artifact (DEC-2032.1, #2032). Each case is a different place
/// a reader takes a string from the file into its own sentence.
#[test]
fn a_refusal_shows_the_files_values_escaped_and_never_raw() {
    let resolved = serde_json::to_value(resolved_plan()).unwrap();
    let mut ordinary_plan = resolved_plan();
    ordinary_plan.origin = PlanOrigin::Database;
    ordinary_plan.analysis = PlanAnalysis::Ordinary;
    let ordinary = serde_json::to_value(&ordinary_plan).unwrap();
    let with = |base: &serde_json::Value, pointer: &str| {
        let mut value = base.clone();
        *value.pointer_mut(pointer).unwrap() = serde_json::Value::String(HOSTILE.into());
        value
    };
    let rule = format!("rule `{HOSTILE_SHOWN}`");
    let engine = format!("evidence for {HOSTILE_SHOWN}");
    let dialect = format!("computed for `{HOSTILE_SHOWN}`, which this build cannot explain");
    let mismatch = format!("computed for {HOSTILE_SHOWN} and this project is postgres");
    let variant = format!("unknown variant `{HOSTILE_SHOWN}`");
    let cases: Vec<(&str, serde_json::Value, String, String)> = vec![
        (
            "the qualification rule",
            with(&resolved, "/analysis/evidence/qualification/rule"),
            rule.clone(),
            rule,
        ),
        (
            "the dialect of a resolved plan",
            with(&resolved, "/dialect"),
            engine,
            mismatch.clone(),
        ),
        (
            "the dialect of an ordinary plan",
            with(&ordinary, "/dialect"),
            dialect,
            mismatch,
        ),
        (
            "a variant the decoder does not know",
            with(&ordinary, "/origin"),
            variant.clone(),
            variant,
        ),
    ];
    for (what, value, explained, applied) in cases {
        // The plan's own checksum where it can be read, so the checksum
        // refusal cannot be what the apply below reports.
        let checksum = serde_json::from_value::<SavedPlan>(value.clone())
            .map(|p| p.checksum())
            .unwrap_or_else(|_| "00".repeat(32));
        let artifact = Artifact::new(&value);
        for format in ["text", "json"] {
            let out = artifact.explain(format);
            assert_inert(&out, &[&explained], &format!("explain {format}: {what}"));
        }
        let out = apply_refused(&artifact, &checksum);
        assert_inert(&out, &[&applied], &format!("apply: {what}"));
    }
}

/// The decoder repeats some of the text it stopped at raw (an unknown variant)
/// and some behind Rust's `Debug` escapes (a string where a number belongs), and
/// the refusal shows either with its backslashes doubled, so the two characters
/// `\n` the file held and a line break are two different sentences (#2038). The
/// column serde reports differs between the two files, so what pins the
/// property is the quoted text and not the whole line.
#[test]
fn a_decode_refusal_tells_a_line_break_from_the_two_characters_backslash_n() {
    let ordinary = {
        let mut plan = resolved_plan();
        plan.origin = PlanOrigin::Database;
        plan.analysis = PlanAnalysis::Ordinary;
        serde_json::to_value(&plan).unwrap()
    };
    let with = |pointer: &str, text: &str| {
        let mut value = ordinary.clone();
        *value.pointer_mut(pointer).unwrap() = serde_json::Value::String(text.into());
        value
    };
    // The file holds a line break / the two characters backslash and `n`.
    let (line_break, two_characters) = ("a\nb", "a\\nb");
    for (what, pointer, raw_line_break, raw_two) in [
        (
            "an unknown variant, quoted raw",
            "/origin",
            "unknown variant `a\\nb`",
            "unknown variant `a\\\\nb`",
        ),
        (
            "a string where a number belongs, quoted with Debug escapes",
            "/version",
            "invalid type: string \"a\\\\nb\"",
            "invalid type: string \"a\\\\\\\\nb\"",
        ),
    ] {
        let first = Artifact::new(&with(pointer, line_break));
        let second = Artifact::new(&with(pointer, two_characters));
        for format in ["text", "json"] {
            let first = first.explain(format);
            let second = second.explain(format);
            assert_inert(&first, &[raw_line_break], &format!("{what}: {format}"));
            assert_inert(&second, &[raw_two], &format!("{what}: {format}"));
            let (first, second) = (
                String::from_utf8_lossy(&first.stderr).into_owned(),
                String::from_utf8_lossy(&second.stderr).into_owned(),
            );
            assert_ne!(first, second, "{what}");
            // Negative: neither refusal holds the other's quoted text, which
            // is what a backslash left alone would make both of them show.
            assert!(!first.contains(raw_two), "{what}: {first:?}");
            assert!(!second.contains(raw_line_break), "{what}: {second:?}");
        }
        let (first_applied, second_applied) = (
            apply_refused(&first, &"00".repeat(32)),
            apply_refused(&second, &"00".repeat(32)),
        );
        assert_inert(&first_applied, &[raw_line_break], &format!("apply: {what}"));
        assert_inert(&second_applied, &[raw_two], &format!("apply: {what}"));
    }
}

/// Negative: only control and invisible characters are rewritten. A name that
/// is merely not ASCII is shown as the file spells it, so the escape does not
/// make a refusal harder to read for a valid Unicode identifier.
#[test]
fn a_refusal_names_a_unicode_value_as_the_file_spells_it() {
    let rule = "règle-名前 'q' \u{301}";
    let plan = sealed_under(rule, Default::default());
    let artifact = Artifact::new(&serde_json::to_value(&plan).unwrap());
    assert_inert(
        &artifact.explain("text"),
        &[&format!("rule `{rule}`")],
        "explain",
    );
    assert_inert(
        &apply_refused(&artifact, &plan.checksum()),
        &[&format!("rule `{rule}`")],
        "apply",
    );
}

#[test]
fn current_resolver_artifacts_cannot_bypass_required_evidence_on_apply() {
    let plan = resolved_plan();
    let valid = serde_json::to_value(&plan).unwrap();
    let mut missing = valid.clone();
    missing.as_object_mut().unwrap().remove("analysis");
    let mut cases = vec![missing];
    for (path, replacement) in [
        ("/analysis", serde_json::json!({"kind": "ordinary"})),
        ("/analysis/evidence/version", serde_json::json!(99)),
        (
            "/analysis/evidence/before/adapter",
            serde_json::json!("postgres-catalog-inputs-v99"),
        ),
        // A runtime this build removed: the measured profiles (#1636).
        (
            "/analysis/evidence/qualification/runtime",
            serde_json::json!({
                "kind": "supplied",
                "profile": "linux-dedicated-v1",
                "identity": "09".repeat(32),
            }),
        ),
    ] {
        let mut value = valid.clone();
        *value.pointer_mut(path).unwrap() = replacement;
        cases.push(value);
    }
    for value in cases {
        // If the wire shape is readable, supply its own checksum so a checksum
        // refusal cannot conceal a missing evidence/provenance validation.
        let checksum = serde_json::from_value::<SavedPlan>(value.clone())
            .map(|p| p.checksum())
            .unwrap_or_else(|_| plan.checksum());
        let artifact = Artifact::new(&value);
        std::fs::write(artifact.0.join("pbps.yml"), "dialect: postgres\n").unwrap();
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let connection = format!(
            "postgres://unused@{}/unused?sslmode=disable",
            target.local_addr().unwrap()
        );
        let out = Command::new(env!("CARGO_BIN_EXE_pbps"))
            .current_dir(&artifact.0)
            .args([
                "apply",
                "--plan",
                "plan.json",
                "--db",
                &connection,
                "--checksum",
                &checksum,
            ])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(
            !error.contains("no longer matches the artifact approved"),
            "{error}"
        );
        assert!(!error.is_empty(), "{out:?}");
        assert_eq!(
            target.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

/// A resolved plan's key is checked before apply connects (#616): a target
/// that names no environment has no key, and the refusal names the key the
/// plan was sealed under and the remedy.
#[test]
fn a_resolved_plan_without_its_key_is_refused_before_contacting_the_target() {
    let plan = resolved_plan();
    let artifact = Artifact::new(&serde_json::to_value(&plan).unwrap());
    std::fs::write(artifact.0.join("pbps.yml"), "dialect: postgres\n").unwrap();
    let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let connection = format!(
        "postgres://unused@{}/unused?sslmode=disable",
        target.local_addr().unwrap()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_pbps"))
        .current_dir(&artifact.0)
        .args([
            "apply",
            "--plan",
            "plan.json",
            "--db",
            &connection,
            "--checksum",
            &plan.checksum(),
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(
        error.contains(&format!(
            "sealed under fingerprint key {}, and no key is configured",
            "01".repeat(8)
        )),
        "{error}"
    );
    assert!(error.contains("Plan again"), "{error}");
    assert_eq!(
        target.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn the_viewer_shows_a_resolved_plan_as_a_deployment_with_its_evidence() {
    use std::io::Write;
    use std::process::Stdio;
    let mut cases = vec![];
    for (origin, title) in [
        (PlanOrigin::ResolvedDatabase, "Deployment plan"),
        (PlanOrigin::Database, "Deployment plan"),
        (PlanOrigin::Preview, "Preview plan"),
    ] {
        let mut plan = resolved_plan();
        if origin != PlanOrigin::ResolvedDatabase {
            plan.origin = origin;
            plan.analysis = PlanAnalysis::Ordinary;
        }
        let artifact = Artifact::new(&serde_json::to_value(plan).unwrap());
        let out = artifact.explain("json");
        assert!(out.status.success(), "{out:?}");
        let served = pbps_ui::contract::parse("explain", &out.stdout, 0).unwrap();
        cases.push(serde_json::json!({
            "title": title,
            "resolved": origin == PlanOrigin::ResolvedDatabase,
            "response": String::from_utf8(served).unwrap()
        }));
    }
    let node = std::env::var_os("PBPS_TEST_NODE").unwrap_or_else(|| "node".into());
    let mut child = Command::new(node)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "const CASES = {}; const RESPONSES = {{status: JSON.stringify({{data: [], findings: []}})}};", serde_json::to_string(&cases).unwrap()).unwrap();
    stdin
        .write_all(include_bytes!("../../pbps-ui/tests/app-browser.cjs"))
        .unwrap();
    stdin
        .write_all(include_bytes!("../../pbps-ui/assets/app.js"))
        .unwrap();
    stdin.write_all(br#"
const settle = () => new Promise(resolve => setImmediate(resolve));
(async () => {
  await settle();
  nav.find(button => button.dataset.view === "plan").fire("click");
  elements["selection-value"].value = "plan.json";
  for (const item of CASES) {
    // A record standing for the captured scope: counted, never listed, in
    // the evidence panel and the plan details alike (#1945 review).
    const response = JSON.parse(item.response);
    if (response.data.resolver) response.data.resolver.prerequisites = [{object: {class: "pg_proc", name: ["sentinel", "p"], signature: []}, fingerprinted: true}];
    RESPONSES.plan = JSON.stringify(response);
    elements.selection.fire("submit");
    await settle();
    const titles = [...elements.content.walk()].filter(e => e.tagName === "h2").map(e => e.textContent);
    assert(titles.includes(item.title), JSON.stringify(titles));
    for (const other of CASES.filter(other => other.title !== item.title)) assert(!titles.includes(other.title));
    // A resolved plan's provenance, read from its JSON (#1517); none for
    // any other plan.
    const resolved = item.resolved;
    assert(titles.includes("Resolver evidence") === resolved, JSON.stringify(titles));
    const lines = [...elements.content.walk()].filter(e => e.tagName === "p").map(e => e.textContent);
    assert(lines.includes("scratch (server scratch, supplied)") === resolved, JSON.stringify(lines));
    const shown = [...elements.content.walk()].map(e => e.textContent).join("\n");
    assert(!shown.includes("sentinel"), shown);
  }
})().catch(error => { console.error(error); process.exit(1); });
"#).unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Evidence naming a runtime this build does not run, such as a measured
/// profile #1636 retired, is refused as an unknown runtime with the remedy,
/// before the target is contacted (#616).
#[test]
fn evidence_naming_an_unknown_runtime_is_refused_with_the_remedy() {
    let mut value = serde_json::to_value(resolved_plan()).unwrap();
    value["analysis"]["evidence"]["qualification"]["runtime"] =
        serde_json::json!({"kind": "measured", "image": "retired"});
    let artifact = Artifact::new(&value);
    std::fs::write(artifact.0.join("pbps.yml"), "dialect: postgres\n").unwrap();
    let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let connection = format!(
        "postgres://unused@{}/unused?sslmode=disable",
        target.local_addr().unwrap()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_pbps"))
        .current_dir(&artifact.0)
        .args([
            "apply",
            "--plan",
            "plan.json",
            "--db",
            &connection,
            "--checksum",
            &"0".repeat(64),
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(
        error.contains("names the runtime `measured`, which this build does not run"),
        "{error}"
    );
    assert!(error.contains("Plan again with this build"), "{error}");
    assert_eq!(
        target.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    // Negative: the resolver's own runtime reads.
    let readable = Artifact::new(&serde_json::to_value(resolved_plan()).unwrap());
    assert!(readable.explain("text").status.success());
}
