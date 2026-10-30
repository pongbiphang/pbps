//! Saved resolver artifacts: reviewable from the file alone, and approved
//! through the ordinary command (#616).

use pbps_model::data::{DataMode, DataScope};
use pbps_model::resolver::*;
use pbps_model::{
    Change, ChangeSet, Column, Finding, IdsFile, PlanBaseline, PlanOrigin, PlannedChange,
    SavedPlan, Severity, TableName, Uid,
};
use std::collections::BTreeSet;
use std::process::{Command, Output};

#[path = "support/envelope_archives.rs"]
mod envelope_archives;

fn resolved_plan() -> SavedPlan {
    resolved_plan_with(Default::default())
}

fn resolved_plan_with(
    limitations: std::collections::BTreeMap<pbps_model::Text, pbps_model::Text>,
) -> SavedPlan {
    let plan = sealed_under(pbps_pg::resolver::compatibility::REPORTED_RULE, limitations);
    let PlanAnalysis::Resolved(evidence) = &plan.analysis else {
        unreachable!("sealed_under resolves the plan");
    };
    pbps_pg::resolver::validate_evidence(evidence).unwrap();
    plan
}

/// A plan whose evidence names `rule`, valid or not: what an earlier build
/// sealed, with the checksum that build computed.
fn sealed_under(
    rule: &str,
    limitations: std::collections::BTreeMap<pbps_model::Text, pbps_model::Text>,
) -> SavedPlan {
    sealed(
        rule,
        limitations,
        ChangeSet::default(),
        ResolverRuntime::Scratch {
            entry: "scratch".into(),
            scratch: ScratchSource::Server {},
            layout: ScratchLayout::Supplied,
            outside_writes: OutsideWrites::Refused,
            same_instance: pbps_model::resolver::SameInstance::NotApplicable,
            dependencies: Default::default(),
        },
        IdsFile::default(),
    )
}

/// [`sealed_under`] over a chosen change set, identity mapping and runtime.
fn sealed(
    rule: &str,
    limitations: std::collections::BTreeMap<pbps_model::Text, pbps_model::Text>,
    changes: ChangeSet,
    runtime: ResolverRuntime,
    ids: IdsFile,
) -> SavedPlan {
    // Synthetic, source-free reader evidence, not a claim of live qualification.
    // Use the public sealing constructors so an invalid fixture cannot turn an
    // explanation regression into an unrelated deserialization refusal.
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
            runtime,
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
        ids,
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
        plan.wire_fingerprint()
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
            assert_eq!(value["data"]["checksum"], plan.wire_fingerprint());
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
                assert!(text.contains(&plan.wire_fingerprint()), "{text}");
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
            "build:server_version".into(),
            "the scratch reports another build string of the same version".into(),
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
    assert_ne!(previous.wire_fingerprint(), current.wire_fingerprint());
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
            &previous.wire_fingerprint(),
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
    apply_refused_allowing(artifact, checksum, &[])
}

/// [`apply_refused`] with the risk classes the plan carries approved, so that
/// the refusal under test is not the gate's.
fn apply_refused_allowing(artifact: &Artifact, checksum: &str, allow: &[&str]) -> Output {
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
        .args(allow.iter().flat_map(|class| ["--allow", class]))
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
            .map(|p| p.wire_fingerprint())
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
        &apply_refused(&artifact, &plan.wire_fingerprint()),
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
            .map(|p| p.wire_fingerprint())
            .unwrap_or_else(|_| plan.wire_fingerprint());
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
            &plan.wire_fingerprint(),
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

// ---- the successful text report and the refusals that name an object ----
// (DEC-2032.1, #2035)

/// One address-bearing change set: a column added to a table, a column dropped
/// from it (destructive, so the report lists it under the approval) and a
/// column made NOT NULL (so the report lists a check that runs first), with
/// the identities that hold them, a finding on the first, all spelled by
/// `text`.
fn changes_naming(text: &str) -> (ChangeSet, IdsFile) {
    use pbps_dialect::Dialect as _;
    let table = TableName::new("app", text);
    let uid = |s: &str| -> Uid { s.parse().unwrap() };
    let (kept, dropped) = (format!("{text}1"), format!("{text}2"));
    let planned = |change: Change| {
        let mut planned = PlannedChange::new(change);
        planned.risks = pbps_pg::Postgres::new().change_risks(&planned.change);
        planned
    };
    let mut added = planned(Change::AddColumn {
        uid: uid("c_aaaaaa"),
        table: table.clone(),
        name: kept.clone().into(),
        column: Box::new(Column::new("int".parse().unwrap())),
    });
    added.findings = vec![Finding::new(text, Severity::Warning, text, None)];
    let changes = ChangeSet {
        changes: vec![
            added,
            planned(Change::DropColumn {
                uid: uid("c_bbbbbb"),
                column: table.column(dropped),
            }),
            planned(Change::AlterColumnNullability {
                uid: uid("c_aaaaaa"),
                column: table.column(&kept),
                ty: "int".parse().unwrap(),
                to_nullable: false,
                collation: None,
                through_external_partitions: false,
            }),
        ],
    };
    let ids = IdsFile {
        tables: [(uid("t_aaaaaa"), table.clone())].into(),
        columns: [(uid("c_aaaaaa"), table.column(kept))].into(),
        ..IdsFile::default()
    };
    (changes, ids)
}

/// A database plan in which every string a reviewer is shown holds `text`: the
/// table, the columns, the time, the revision, the baseline, a finding and a
/// reference-data table.
fn plan_holding(text: &str) -> SavedPlan {
    let (changes, ids) = changes_naming(text);
    let mut plan = SavedPlan::new(
        PlanOrigin::Database,
        "postgres",
        text,
        PlanBaseline {
            time_zone: None,
            description: text.into(),
            checksum: "00".repeat(32),
            database_collation: None,
        },
        changes,
        ids,
    );
    plan.git_sha = Some(text.into());
    plan.data.insert(
        TableName::new("app", text),
        DataScope {
            mode: DataMode::Ensure,
            keys: Default::default(),
        },
    );
    plan
}

/// A resolved plan whose resolver entry, image and limitation hold `text`.
/// Evidence is sealed over the changes it is given, so this one has none.
fn resolved_plan_holding(text: &str) -> SavedPlan {
    sealed(
        pbps_pg::resolver::compatibility::REPORTED_RULE,
        [(text.into(), text.into())].into(),
        ChangeSet::default(),
        ResolverRuntime::Scratch {
            entry: text.into(),
            scratch: ScratchSource::Docker { image: text.into() },
            layout: ScratchLayout::RunOwned,
            outside_writes: OutsideWrites::Refused,
            same_instance: pbps_model::resolver::SameInstance::NotApplicable,
            dependencies: Default::default(),
        },
        IdsFile::default(),
    )
}

/// Every line of `text` is one the report wrote: a value that held a line
/// break did not start one, and nothing acts on the terminal.
fn assert_only_the_reports_own_lines(text: &str, plain: &str, what: &str) {
    assert!(
        !text.chars().any(|c| c.is_control() && c != '\n'),
        "{what}: a raw control character in {text:?}"
    );
    assert_eq!(
        text.lines().count(),
        plain.lines().count(),
        "{what}: a value started a line of the report:\n{text}"
    );
}

/// A reviewer approves from the text report, and a hand-made plan holds valid
/// names and free text with control characters in them: the report shows each
/// of them escaped, in the lines it already had. Structured JSON values stay
/// raw while human-facing prose is shown once in every format.
#[test]
fn the_report_keeps_structured_plan_values_raw_and_shows_prose_once() {
    let shown = HOSTILE_SHOWN;
    let explained = |plan: SavedPlan| {
        let artifact = Artifact::new(&serde_json::to_value(plan).unwrap());
        let text = artifact.explain("text");
        assert!(text.status.success() && text.stderr.is_empty(), "{text:?}");
        let json = artifact.explain("json");
        assert!(json.status.success(), "{json:?}");
        (String::from_utf8(text.stdout).unwrap(), json)
    };

    // A plan with changes, a finding, a data scope, a time and a revision.
    let (text, json) = explained(plan_holding(HOSTILE));
    let (plain, _) = explained(plan_holding("x"));
    assert_only_the_reports_own_lines(&text, &plain, "explain text");
    for (what, fragment) in [
        ("created_at", format!("  created     {shown}\n")),
        ("git_sha", format!("  git sha     {shown}\n")),
        ("baseline", format!("  baseline    {shown}\n")),
        ("a data scope's table", format!("app.{shown} (ensure)")),
        ("a change's table", format!("\n  app.{shown}\n")),
        ("a column added", format!("+ add column {shown}1 ")),
        ("a finding", format!("      warning: {shown} — {shown}\n")),
        (
            "a column dropped, under the approval",
            format!("- drop column {shown}2"),
        ),
    ] {
        assert!(
            text.contains(&fragment),
            "{what}: {fragment:?} not in\n{text}"
        );
    }
    // The check that runs first names the column it counts.
    let checks = &text[text.find("Checks that run before").expect(&text)..];
    assert!(checks.contains(shown), "a probe: {checks}");

    // Structured values retain their bytes; generated prose is shown once.
    let raw = String::from_utf8(json.stdout.clone()).unwrap();
    let value = report(&json);
    for (what, got) in [
        ("created_at", &value["data"]["created_at"]),
        ("git_sha", &value["data"]["git_sha"]),
        ("baseline", &value["data"]["baseline"]),
    ] {
        assert_eq!(got, HOSTILE, "{what}");
    }
    assert!(
        raw.contains(&serde_json::to_string(HOSTILE).unwrap()),
        "{raw}"
    );
    let approval = value["data"]["risks"][0]["changes"][0].as_str().unwrap();
    assert!(
        approval.contains(shown) && !approval.contains(HOSTILE),
        "{approval:?}"
    );

    let finding = value["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["id"] == HOSTILE)
        .unwrap();
    assert_eq!(finding["message"], shown);

    // A resolved plan, with the resolver's entry, image and limitation.
    let (text, json) = explained(resolved_plan_holding(HOSTILE));
    let (plain, _) = explained(resolved_plan_holding("x"));
    assert_only_the_reports_own_lines(&text, &plain, "explain text of a resolved plan");
    for (what, fragment) in [
        (
            "the resolver's entry and image",
            format!("  resolver    {shown} (docker scratch, run-owned); image {shown},"),
        ),
        ("a limitation", format!("    {shown}: {shown}\n")),
    ] {
        assert!(
            text.contains(&fragment),
            "{what}: {fragment:?} not in\n{text}"
        );
    }
    let value = report(&json);
    assert_eq!(value["data"]["resolver"]["entry"], HOSTILE);
    assert_eq!(value["data"]["resolver"]["image"], HOSTILE);
    assert_eq!(value["data"]["resolver"]["limitations"][0]["name"], HOSTILE);
    assert_eq!(
        value["data"]["resolver"]["limitations"][0]["description"],
        HOSTILE
    );
    let raw = String::from_utf8(json.stdout).unwrap();
    assert!(!raw.contains(shown), "{raw}");
}

/// Negative: a valid Unicode name, with the punctuation a name may hold, is
/// shown as the file spells it, in the report and in a resolved plan's
/// evidence.
#[test]
fn a_unicode_name_is_shown_as_the_file_spells_it() {
    let name = "名前_ünï 'q' \"d\" {} %s";
    for (plan, fragments) in [
        (
            plan_holding(name),
            vec![
                format!("  created     {name}\n"),
                format!("\n  app.{name}\n"),
                format!("+ add column {name}1 "),
                format!("app.{name} (ensure)"),
            ],
        ),
        (
            resolved_plan_holding(name),
            vec![
                format!("  resolver    {name} (docker scratch, run-owned); image {name},"),
                format!("    {name}: {name}\n"),
            ],
        ),
    ] {
        let artifact = Artifact::new(&serde_json::to_value(plan).unwrap());
        let out = artifact.explain("text");
        let text = String::from_utf8(out.stdout).unwrap();
        for fragment in fragments {
            assert!(text.contains(&fragment), "{fragment:?} not in\n{text}");
        }
    }
}

/// What the free text of a plan can hold that a reviewer reads (#2084): a line
/// break, a carriage return, a terminal sequence and the single-character
/// introducer some terminals read as one.
const FREE_TEXT: &str = "why\nerror: forged\r\u{1b}[31mred\u{9b}2J";
/// What the report shows of [`FREE_TEXT`].
const FREE_TEXT_SHOWN: &str = "why\\nerror: forged\\r\\u{1b}[31mred\\u{9b}2J";

/// A plan whose column is made deprecated for `reason`, whose added column
/// carries `reason` as its description and deprecation, and whose baseline
/// description is `reason` too.
fn plan_with_free_text(reason: &str) -> SavedPlan {
    let table = TableName::new("app", "t");
    let uid = |s: &str| -> Uid { s.parse().unwrap() };
    let mut column = Column::new("int".parse().unwrap());
    column.description = Some(reason.into());
    column.deprecated = Some(reason.into());
    let changes = ChangeSet {
        changes: vec![
            PlannedChange::new(Change::AddColumn {
                uid: uid("c_aaaaaa"),
                table: table.clone(),
                name: "kept".into(),
                column: Box::new(column),
            }),
            PlannedChange::new(Change::SetColumnDeprecated {
                uid: uid("c_bbbbbb"),
                column: table.column("old"),
                reason: Some(reason.into()),
            }),
        ],
    };
    let ids = IdsFile {
        tables: [(uid("t_aaaaaa"), table.clone())].into(),
        columns: [
            (uid("c_aaaaaa"), table.column("kept")),
            (uid("c_bbbbbb"), table.column("old")),
        ]
        .into(),
        ..IdsFile::default()
    };
    SavedPlan::new(
        PlanOrigin::Database,
        "postgres",
        "2026-10-16T00:00:00Z",
        PlanBaseline {
            time_zone: None,
            description: reason.into(),
            checksum: "00".repeat(32),
            database_collation: None,
        },
        changes,
        ids,
    )
}

/// A description, a deprecation reason and a baseline in a hand-made plan may
/// hold a line break or a terminal sequence. The text report shows them escaped
/// in the lines it already had, and `--format json` carries them as it did.
#[test]
fn a_deprecation_reason_and_a_description_in_a_plan_are_shown_escaped_and_json_is_unchanged() {
    let explained = |plan: &SavedPlan| {
        let artifact = Artifact::new(&serde_json::to_value(plan).unwrap());
        let text = artifact.explain("text");
        assert!(text.status.success() && text.stderr.is_empty(), "{text:?}");
        let json = artifact.explain("json");
        assert!(json.status.success(), "{json:?}");
        (String::from_utf8(text.stdout).unwrap(), json)
    };
    let (text, json) = explained(&plan_with_free_text(FREE_TEXT));
    let (plain, _) = explained(&plan_with_free_text("x"));
    assert_only_the_reports_own_lines(&text, &plain, "explain text");
    for (what, fragment) in [
        (
            "a deprecation reason",
            format!("~ old marked deprecated: {FREE_TEXT_SHOWN}\n"),
        ),
        ("the baseline", format!("  baseline    {FREE_TEXT_SHOWN}\n")),
    ] {
        assert!(
            text.contains(&fragment),
            "{what}: {fragment:?} not in\n{text}"
        );
    }

    // JSON is what it was: the value as JSON spells it, not escaped twice.
    let raw = String::from_utf8(json.stdout.clone()).unwrap();
    assert_eq!(report(&json)["data"]["baseline"], FREE_TEXT);
    assert!(
        raw.contains(&serde_json::to_string(FREE_TEXT).unwrap()),
        "{raw}"
    );
    assert!(!raw.contains(FREE_TEXT_SHOWN), "{raw}");

    // Negative: a plain Unicode reason and description are shown unchanged.
    let plain_text = "不再使用 ünï 'q' {} 9";
    let (text, _) = explained(&plan_with_free_text(plain_text));
    assert!(
        text.contains(&format!("~ old marked deprecated: {plain_text}\n")),
        "{text}"
    );
    assert!(
        text.contains(&format!("  baseline    {plain_text}\n")),
        "{text}"
    );
}

/// A plan with the changes of [`changes_naming`] under `text`, as a database
/// plan the gate would read, and the checksum to apply it with.
fn named_plan(text: &str, edit: impl FnOnce(&mut SavedPlan)) -> (Artifact, String) {
    let (changes, ids) = changes_naming(text);
    let mut plan = SavedPlan::new(
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
        ids,
    );
    edit(&mut plan);
    let checksum = plan.wire_fingerprint();
    (
        Artifact::new(&serde_json::to_value(&plan).unwrap()),
        checksum,
    )
}

/// A refusal that names an object shows the name escaped, whichever error type
/// carries it, and stays the one line it was (DEC-2032.1, #2035). Each case
/// reaches a different error type on the way to `explain` and `apply`.
#[test]
fn a_refusal_naming_an_object_shows_the_name_escaped_on_explain_and_apply() {
    let shown = HOSTILE_SHOWN;
    /// One way to make a plan refuse, and what the refusal says of the name.
    struct Case {
        what: &'static str,
        edit: fn(&mut SavedPlan),
        fragment: String,
    }
    let cases = [
        Case {
            // The identity mapping holds one name twice.
            what: "IdsFile::validate (DuplicateName)",
            edit: |plan| {
                let table = plan.ids.tables.values().next().unwrap().clone();
                plan.ids.tables.insert("t_bbbbbb".parse().unwrap(), table);
            },
            fragment: format!("both point at the name `app.{shown}`"),
        },
        Case {
            // A column whose table entry is gone.
            what: "IdsFile::validate (OrphanColumn)",
            edit: |plan| plan.ids.tables.clear(),
            fragment: format!("points at `app.{shown}.{shown}1`, but no live table entry owns it"),
        },
        Case {
            // A column whose table no identity holds, in an identity mapping
            // that is consistent: `PlanNames::of`.
            what: "PlanNames::of (Owner)",
            edit: |plan| {
                plan.ids.tables.clear();
                plan.ids.columns.clear();
            },
            fragment: format!("column app.{shown}.{shown}1 has no managed owning table"),
        },
        Case {
            // A name the SQL cannot carry: the dialect's own refusal. The plan
            // is otherwise consistent, so explain and apply reach the emitter.
            what: "the emitter (UnquotableIdent)",
            edit: |plan| {
                let Change::DropColumn { column, .. } = &mut plan.changes.changes[1].change else {
                    unreachable!()
                };
                column.name = ["\0", column.name.raw()].concat().into();
            },
            fragment: "the identifier `\\u{0}".to_owned(),
        },
    ];
    for Case {
        what,
        edit,
        fragment,
    } in cases
    {
        let (artifact, checksum) = named_plan(HOSTILE, edit);
        for format in ["text", "json"] {
            let out = artifact.explain(format);
            assert_inert(&out, &[&fragment], &format!("explain {format}: {what}"));
        }
        let out = apply_refused_allowing(&artifact, &checksum, &["destructive", "not-null"]);
        assert_inert(&out, &[&fragment], &format!("apply: {what}"));
    }
}

// Exercise the shared artifact carrier and presentation through the existing
// reader fixture. SQL Server qualification/reader admission remains #2143.
#[test]
fn same_instance_is_explained_and_tampering_fails_the_approved_checksum() {
    for policy in [SameInstance::Refused, SameInstance::Allowed] {
        let plan = sealed(
            pbps_pg::resolver::compatibility::REPORTED_RULE,
            Default::default(),
            ChangeSet::default(),
            ResolverRuntime::Scratch {
                entry: "scratch".into(),
                scratch: ScratchSource::Server {},
                layout: ScratchLayout::Supplied,
                outside_writes: OutsideWrites::Refused,
                same_instance: policy,
                dependencies: Default::default(),
            },
            IdsFile::default(),
        );
        let mut value = serde_json::to_value(&plan).unwrap();
        let artifact = Artifact::new(&value);
        let expected = match policy {
            SameInstance::Refused => "refused",
            SameInstance::Allowed => "allowed",
            SameInstance::NotApplicable => unreachable!(),
        };
        let out = artifact.explain("text");
        assert!(out.status.success(), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stdout)
                .contains(&format!("same-instance externals file {expected}")),
            "{out:?}"
        );
        let out = artifact.explain("json");
        assert!(out.status.success(), "{out:?}");
        let output: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(output["data"]["resolver"]["same_instance"], expected);
        value["analysis"]["evidence"]["qualification"]["runtime"]["same_instance"] =
            if policy == SameInstance::Allowed {
                "refused".into()
            } else {
                "allowed".into()
            };
        let tampered = Artifact::new(&value);
        let out = apply_refused(&tampered, &plan.wire_fingerprint());
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("checksum"),
            "{out:?}"
        );
    }
}

#[test]
fn saved_finding_prose_preserves_tool_backslashes_and_the_sealed_plan() {
    let rendered = format!(r"tool bytea `\x` names {HOSTILE_SHOWN}");
    for (message, expected) in [
        (
            format!(r"tool bytea `\x` names {HOSTILE}"),
            rendered.clone(),
        ),
        (rendered.clone(), rendered),
        (
            r"tool bytea `\x` names literal\\n".into(),
            r"tool bytea `\x` names literal\\n".into(),
        ),
        (
            "tool bytea `\\x` names 名前_ünï❤️".into(),
            "tool bytea `\\x` names 名前_ünï❤️".into(),
        ),
    ] {
        let mut plan = plan_holding("ordinary");
        plan.changes.changes[0].findings[0].message = message;
        let checksum = plan.wire_fingerprint();
        let artifact = Artifact::new(&serde_json::to_value(&plan).unwrap());
        let text = artifact.explain("text");
        assert!(text.status.success(), "{text:?}");
        let text = String::from_utf8(text.stdout).unwrap();
        assert!(
            text.contains(&format!("      warning: ordinary — {expected}\n")),
            "{text:?}"
        );
        assert!(
            !text.chars().any(|c| c.is_control() && c != '\n'),
            "{text:?}"
        );
        let json = artifact.explain("json");
        assert!(json.status.success(), "{json:?}");
        let value = report(&json);
        let finding = value["findings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|finding| finding["id"] == "ordinary")
            .unwrap();
        assert_eq!(finding["message"], expected);
        assert_eq!(value["data"]["checksum"], checksum);
        assert_eq!(plan.wire_fingerprint(), checksum);
    }
}

fn plan_holding_bytea_row(value: &str, key_column: &str, identity_key: bool) -> SavedPlan {
    let table = TableName::new("app", "bytes");
    let ids = IdsFile {
        tables: [("t_aaaaaa".parse().unwrap(), table.clone())].into(),
        columns: [
            ("c_aaaaaa".parse().unwrap(), table.column(key_column)),
            ("c_bbbbbb".parse().unwrap(), table.column("payload")),
        ]
        .into(),
        ..Default::default()
    };
    let change = PlannedChange::new(Change::InsertRow {
        table,
        key_column: key_column.into(),
        identity_key,
        key: "1".into(),
        row: [("payload".into(), pbps_model::Value::Text(value.into()))]
            .into_iter()
            .collect(),
        defaults: Default::default(),
        types: [("payload".into(), "bytea".parse().unwrap())].into(),
    });
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
        ChangeSet {
            changes: vec![change],
        },
        ids,
    )
}

#[test]
fn malformed_bytea_saved_plans_show_each_error_operand_once() {
    for (value, expected) in [
        (
            "\\x00\n\u{1b}[31m\u{202e}名前",
            r#"postgres: the value "\\x00\n\u{1b}[31m\u{202e}名前" is declared for a `bytea` column and `00\n\u{1b}[31m\u{202e}名前` is not hex digits"#,
        ),
        (
            r"\x00\n\u{1b}[31m\u{202e}名前",
            r#"postgres: the value "\\x00\\n\\u{1b}[31m\\u{202e}名前" is declared for a `bytea` column and `00\\n\\u{1b}[31m\\u{202e}名前` is not hex digits"#,
        ),
        (
            "\\x名前_ünï",
            r#"postgres: the value "\\x名前_ünï" is declared for a `bytea` column and `名前_ünï` is not hex digits"#,
        ),
    ] {
        let expected = format!("cannot render a change as SQL: {expected}");
        let plan = plan_holding_bytea_row(value, "id", false);
        let artifact = Artifact::new(&serde_json::to_value(&plan).unwrap());
        let before = std::fs::read(artifact.0.join("plan.json")).unwrap();
        let text = artifact.explain("text");
        assert_eq!(text.status.code(), Some(1), "{text:?}");
        assert_eq!(
            String::from_utf8(text.stderr).unwrap(),
            format!("error: {expected}\n")
        );
        let json = artifact.explain("json");
        assert_eq!(json.status.code(), Some(1), "{json:?}");
        assert_eq!(report(&json)["findings"][0]["message"], expected);
        assert_eq!(std::fs::read(artifact.0.join("plan.json")).unwrap(), before);
    }
    let plan = plan_holding_bytea_row(r"\x00", "id", false);
    let artifact = Artifact::new(&serde_json::to_value(&plan).unwrap());
    assert!(artifact.explain("text").status.success());
}

#[test]
fn pinned_identity_refusals_show_saved_column_names_once() {
    for (name, shown) in [
        (
            "key\n\u{1b}[31m\u{202e}名前",
            r"key\n\u{1b}[31m\u{202e}名前",
        ),
        (
            r"key\n\u{1b}[31m\u{202e}名前",
            r"key\\n\\u{1b}[31m\\u{202e}名前",
        ),
        ("名前_ünï", "名前_ünï"),
    ] {
        let plan = plan_holding_bytea_row(r"\x00", name, true);
        let expected = format!(
            "cannot render a change as SQL: postgres: `app.bytes` keys its declared rows by `{shown}`, an identity column. This engine writes a pinned key with `OVERRIDING SYSTEM VALUE` and leaves the sequence behind: the apply succeeds, the plan verifies clean, and the next insert from anywhere else fails on the primary key (ADR-0013 section 2). Declare the rows by a natural key, or place them outside pbps and adopt them with `pbps baseline`."
        );
        let artifact = Artifact::new(&serde_json::to_value(&plan).unwrap());
        let before = std::fs::read(artifact.0.join("plan.json")).unwrap();
        let text = artifact.explain("text");
        assert_eq!(text.status.code(), Some(1), "{text:?}");
        assert_eq!(
            String::from_utf8(text.stderr).unwrap(),
            format!("error: {expected}\n")
        );
        let json = artifact.explain("json");
        assert_eq!(report(&json)["findings"][0]["message"], expected);
        assert_eq!(std::fs::read(artifact.0.join("plan.json")).unwrap(), before);
        let ordinary = plan_holding_bytea_row(r"\x00", name, false);
        assert!(
            Artifact::new(&serde_json::to_value(ordinary).unwrap())
                .explain("text")
                .status
                .success()
        );
    }
}

/// A hand edit of a plan file, applied to its JSON.
type Edit = fn(&mut serde_json::Value);

fn rehearsed_plan(record: pbps_model::Rehearsal) -> SavedPlan {
    let mut plan = plan_holding("t");
    plan.rehearsal = Some(record);
    plan
}

fn explained(plan: &SavedPlan) -> (String, serde_json::Value) {
    let artifact = Artifact::new(&serde_json::to_value(plan).unwrap());
    let text = artifact.explain("text");
    assert!(text.status.success(), "{text:?}");
    let value = report(&artifact.explain("json"));
    (String::from_utf8_lossy(&text.stdout).into_owned(), value)
}

/// The record is sealed in the plan and shown by `explain` from the file
/// alone, in human and JSON output (#2379).
#[test]
fn a_converged_rehearsal_is_explained_with_and_without_spelling_differences() {
    let docker = |spelling: Vec<pbps_model::Text>| pbps_model::Rehearsal::Converged {
        entry: "ci".into(),
        scratch: ScratchSource::Docker {
            image: "postgres:18".into(),
        },
        layout: ScratchLayout::RunOwned,
        spelling,
        authorization: pbps_model::AuthorizationRehearsal::Rehearsed,
    };
    let (text, value) = explained(&rehearsed_plan(docker(vec![])));
    assert!(
        text.contains(
            "rehearsal   converged on ci (docker scratch, run-owned); \
             image postgres:18, as configured, not verified\n"
        ),
        "{text}"
    );
    assert!(!text.contains("spelling difference"), "{text}");
    assert!(!text.contains("\nRehearsal\n"), "{text}");
    assert_eq!(
        value["data"]["rehearsal"],
        serde_json::json!({
            "outcome": "converged", "entry": "ci", "scratch": "docker",
            "image": "postgres:18", "layout": "run-owned", "authorization": "rehearsed",
        })
    );

    let spelt = vec![
        "check c on app.t: declared `a > 0`, the engine stores `(a > 0)`".into(),
        "default on app.t.d: declared `1`, the engine stores `'1'::text`\u{1b}[31m".into(),
    ];
    let (text, value) = explained(&rehearsed_plan(docker(spelt)));
    assert!(text.contains("; 2 spelling differences\n"), "{text}");
    assert!(text.contains("\nRehearsal\n"), "{text}");
    assert!(
        text.contains("    check c on app.t: declared `a > 0`, the engine stores `(a > 0)`\n"),
        "{text}"
    );
    // Engine text is escaped for the terminal, not echoed raw.
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert_eq!(
        value["data"]["rehearsal"]["spelling"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let server = pbps_model::Rehearsal::Converged {
        entry: "shared".into(),
        scratch: ScratchSource::Server {},
        layout: ScratchLayout::Supplied,
        spelling: vec!["one".into()],
        authorization: pbps_model::AuthorizationRehearsal::Rehearsed,
    };
    let (text, value) = explained(&rehearsed_plan(server));
    assert!(
        text.contains(
            "rehearsal   converged on shared (server scratch, supplied); 1 spelling difference\n"
        ),
        "{text}"
    );
    assert!(value["data"]["rehearsal"].get("image").is_none(), "{value}");
}

/// `Off` is the operator's choice and is never read as a rehearsal; a plan
/// with no record says nothing at all.
#[test]
fn an_off_rehearsal_is_never_read_as_rehearsed_and_absence_says_nothing() {
    let (text, value) = explained(&rehearsed_plan(pbps_model::Rehearsal::Off));
    assert!(
        text.contains("rehearsal   off (this plan was not rehearsed)\n"),
        "{text}"
    );
    assert!(!text.contains("converged"), "{text}");
    assert_eq!(
        value["data"]["rehearsal"],
        serde_json::json!({"outcome": "off"})
    );

    let (text, value) = explained(&plan_holding("t"));
    assert!(!text.contains("rehearsal"), "{text}");
    assert!(value["data"].get("rehearsal").is_none(), "{value}");
}

/// A rehearsal that left authorization out is never shown as a full one: the
/// plan's line, its own section and the JSON all say so, with the reason
/// (#2379).
#[test]
fn a_rehearsal_that_skipped_authorization_says_so_wherever_it_is_shown() {
    let skipped = pbps_model::Rehearsal::Converged {
        entry: "shared".into(),
        scratch: ScratchSource::Server {},
        layout: ScratchLayout::Supplied,
        spelling: vec![],
        authorization: pbps_model::AuthorizationRehearsal::NotRehearsed {
            reason: "the scratch account may not create roles".into(),
        },
    };
    let (text, value) = explained(&rehearsed_plan(skipped));
    assert!(
        text.contains(
            "rehearsal   converged on shared (server scratch, supplied); \
             authorization NOT rehearsed\n"
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "\nRehearsal\n  Authorization was NOT rehearsed: \
             the scratch account may not create roles\n"
        ),
        "{text}"
    );
    assert_eq!(value["data"]["rehearsal"]["authorization"], "not-rehearsed");
    assert_eq!(
        value["data"]["rehearsal"]["authorization_reason"],
        "the scratch account may not create roles"
    );
}

/// Negative: a file whose record the reader cannot read, or whose record
/// names nothing, is refused rather than explained.
#[test]
fn explain_refuses_a_rehearsal_record_it_cannot_read() {
    let sealed = serde_json::to_value(rehearsed_plan(pbps_model::Rehearsal::Converged {
        entry: "ci".into(),
        scratch: ScratchSource::Server {},
        layout: ScratchLayout::Supplied,
        spelling: vec![],
        authorization: pbps_model::AuthorizationRehearsal::Rehearsed,
    }))
    .unwrap();
    let edits: [(&str, Edit); 6] = [
        ("an unknown outcome", |v| v["rehearsal"] = "failed".into()),
        ("no entry", |v| {
            v["rehearsal"]["converged"]
                .as_object_mut()
                .unwrap()
                .remove("entry");
        }),
        ("an unknown key", |v| {
            v["rehearsal"]["converged"]["verified"] = true.into()
        }),
        ("an empty entry", |v| {
            v["rehearsal"]["converged"]["entry"] = "".into()
        }),
        ("no word about authorization", |v| {
            v["rehearsal"]["converged"]
                .as_object_mut()
                .unwrap()
                .remove("authorization");
        }),
        ("authorization skipped for no reason", |v| {
            v["rehearsal"]["converged"]["authorization"] =
                serde_json::json!({"not_rehearsed": {"reason": ""}})
        }),
    ];
    for (what, edit) in edits {
        let mut plan = sealed.clone();
        edit(&mut plan);
        let out = Artifact::new(&plan).explain("text");
        assert_eq!(out.status.code(), Some(1), "{what}: {out:?}");
        assert!(out.stdout.is_empty(), "{what}: {out:?}");
    }
}
