//! Independent consumer types for the published envelope (ADR-0015 decision 6).
//! Changes are the schema's deliberately opaque field: the page displays every
//! byte of their JSON as data, without interpreting the model's change variants.

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope<T> {
    pub schema_version: u32,
    pub tool_version: String,
    pub command: String,
    pub result: Outcome,
    pub findings: Vec<Finding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Ok,
    Findings,
    Unanswerable,
}

impl Outcome {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Ok => 0,
            Self::Findings => 2,
            Self::Unanswerable => 1,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub id: String,
    pub severity: Severity,
    pub message: String,
    pub location: Option<Location>,
    pub remedy: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Note,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Location {
    pub file: String,
    pub line: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Environment {
    pub environment: String,
    pub description: Option<String>,
    pub state: String,
    pub detail: Option<String>,
    #[serde(serialize_with = "decimal::optional")]
    pub last_entry: Option<i64>,
    pub last_kind: Option<String>,
    pub applied_at: Option<String>,
    pub git_sha: Option<String>,
    pub operator: Option<String>,
    pub locked_by: Option<String>,
    pub lock_unknown: Option<String>,
    pub checked_at: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Drift {
    pub version: u32,
    pub environment: String,
    pub checked_at: String,
    pub baseline: Baseline,
    pub live_checksum: String,
    pub changes: serde_json::Value,
    #[serde(default)]
    pub unmanaged: Vec<String>,
    #[serde(default)]
    pub unexpressible: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    #[serde(serialize_with = "decimal::required")]
    pub entry_id: i64,
    pub applied_at: String,
    pub checksum: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Explanation {
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub dependencies: std::collections::BTreeMap<String, Dependency>,
    pub applyable: bool,
    pub dialect: String,
    pub created_at: String,
    pub git_sha: Option<String>,
    pub baseline: String,
    pub mode: String,
    pub checksum: String,
    pub change_count: usize,
    pub table_count: usize,
    pub module_count: usize,
    pub role_count: usize,
    pub risks: Vec<Risk>,
    pub approve_with: String,
    pub plan_path: Option<String>,
    pub probes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drop_requirements: Vec<String>,
    pub unchecked: Vec<Unchecked>,
    pub statement_count: usize,
    pub target: Option<Target>,
    /// A resolved plan's sealed evidence (#1517); absent for any other plan.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolver: Option<Resolver>,
    /// A plan's sealed rehearsal record (#2379); absent when never asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rehearsal: Option<Rehearsal>,
    // These are observations made while planning; absence claims no checks ran.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub target_assertions: Vec<TargetAssertion>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rehearsal {
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scratch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spelling: Vec<String>,
    /// `rehearsed`, or `not-rehearsed` where the plan's role, grant and
    /// row-security policy changes did not run, with the reason beside it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_reason: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    pub declaration: DependencyDeclaration,
    pub comparison: DependencyComparison,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyDeclaration {
    pub kind: DependencyKind,
    pub columns: std::collections::BTreeMap<String, DependencyColumn>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyKind {
    Table,
    View,
}

#[derive(Debug, Deserialize, Serialize)]
pub enum DependencyComparison {
    #[serde(rename = "compared")]
    Compared,
    #[serde(rename = "declared, not compared")]
    NotCompared,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyColumn {
    #[serde(rename = "type")]
    pub ty: String,
    pub nullable: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Resolver {
    pub entry: String,
    pub scratch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    pub layout: String,
    // Keep the viewer independent of the model crate while retaining the
    // CLI's sealed authorization status and notice (#2570).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_notice: Option<String>,
    pub outside_writes: String,
    // SQL Server reports this policy; PostgreSQL omits it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub same_instance: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<ResolverDependency>,
    pub rule: String,
    pub key_id: String,
    pub sealed: Vec<String>,
    pub limitations: Vec<ResolverLimitation>,
    pub surfaces: Vec<ResolverSurface>,
    pub order: Vec<ResolverOrder>,
    pub prerequisites: Vec<ResolverPrerequisite>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolverDependency {
    pub name: String,
    pub placement: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shape: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetAssertion {
    pub path: String,
    pub sha256: String,
    pub outcome: AssertionOutcome,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssertionOutcome {
    Pass,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolverLimitation {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolverSurface {
    pub surface: String,
    pub verdict: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolverOrder {
    pub before: usize,
    pub after: usize,
    pub reason: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolverPrerequisite {
    pub object: ResolverObject,
    pub fingerprinted: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolverObject {
    pub class: String,
    pub name: Vec<String>,
    pub signature: Vec<ResolverObject>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Unchecked {
    pub check: String,
    pub reason: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Risk {
    pub class: String,
    pub why: String,
    pub changes: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub environment: String,
    pub state: String,
    pub detail: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Timeline {
    pub environment: String,
    pub initialized: bool,
    pub limit: std::num::NonZeroU32,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    #[serde(serialize_with = "decimal::required")]
    pub id: i64,
    pub applied_at: String,
    pub kind: String,
    pub state_version: Option<u32>,
    pub unreadable: Option<Unreadable>,
    pub operator: String,
    pub git_sha: Option<String>,
    pub plan_checksum: Option<String>,
    pub reason: Option<String>,
    pub staged: Option<Staged>,
    pub tables: Option<usize>,
    pub modules: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Unreadable {
    UnsupportedVersion { detail: String },
    Malformed { detail: String },
    Denied { detail: String },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Staged {
    pub completed: usize,
    pub total: usize,
}

/// Validate the whole envelope, including its route and exit-code contract.
/// Ledger identifiers leave this process as decimal strings (#492).
///
/// The CLI's envelope carries them as JSON integers, the full `i64` range, and
/// that contract stays: they are read here as numbers, strictly. What changes is
/// the one hop only this viewer makes, to a page that reads JSON with
/// `response.json()`. A JavaScript number holds integers exactly only up to
/// 2^53 - 1, so `9007199254740993` arrived as `9007199254740992`, the id of a
/// different entry, and an operator copying it from the page would name the
/// wrong one. A string cannot be rounded. Only identifiers are spelled this
/// way: a count is displayed, never copied back, and staying a number keeps it
/// one.
mod decimal {
    use serde::Serializer;

    pub fn required<S: Serializer>(id: &i64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(id)
    }

    pub fn optional<S: Serializer>(id: &Option<i64>, serializer: S) -> Result<S::Ok, S::Error> {
        match id {
            Some(id) => serializer.collect_str(id),
            // Absent stays absent: `null`, never `"null"` or `"0"`.
            None => serializer.serialize_none(),
        }
    }
}

pub fn parse(command: &str, bytes: &[u8], exit_code: i32) -> Result<Vec<u8>, String> {
    fn typed<T: serde::de::DeserializeOwned + Serialize>(
        command: &str,
        bytes: &[u8],
        exit_code: i32,
    ) -> Result<Vec<u8>, String> {
        let envelope: Envelope<T> = serde_json::from_slice(bytes).map_err(|_| {
            "The CLI response does not match this viewer's envelope contract".to_owned()
        })?;
        if envelope.schema_version != 1
            || envelope.command != command
            || envelope.result.exit_code() != exit_code
        {
            return Err("The CLI response has an incompatible version, command or outcome".into());
        }
        serde_json::to_vec(&envelope).map_err(|e| e.to_string())
    }
    match command {
        "status" => typed::<Vec<Environment>>(command, bytes, exit_code),
        "verify" => typed::<Drift>(command, bytes, exit_code),
        "explain" => typed::<Explanation>(command, bytes, exit_code),
        "state list" => typed::<Timeline>(command, bytes, exit_code),
        _ => Err("This viewer does not run that command".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Identifiers are served as decimal strings, from numbers the CLI wrote;
    /// an absent one stays `null`, and a count stays a number (#492).
    #[test]
    fn ledger_identifiers_are_served_as_decimal_strings_and_nothing_else_is() {
        let served = |command: &str, data: serde_json::Value| {
            let cli = json!({"schema_version":1,"tool_version":"0.0.0","command":command,
                "result":"ok","findings":[],"data":data});
            let bytes = parse(command, &serde_json::to_vec(&cli).unwrap(), 0).unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["data"].clone()
        };
        let status = served(
            "status",
            json!([{"environment":"a","state":"ok","checked_at":"now","last_entry":i64::MAX},
                   {"environment":"b","state":"ok","checked_at":"now"}]),
        );
        assert_eq!(status[0]["last_entry"], json!("9223372036854775807"));
        assert_eq!(status[1]["last_entry"], json!(null));
        let timeline = served(
            "state list",
            json!({"environment":"a","initialized":true,"limit":20,"entries":[
                {"id":9_007_199_254_740_993_i64,"applied_at":"now","kind":"apply","operator":"ci",
                 "tables":3}]}),
        );
        assert_eq!(timeline["entries"][0]["id"], json!("9007199254740993"));
        assert_eq!(timeline["limit"], json!(20));
        assert_eq!(timeline["entries"][0]["tables"], json!(3));
        let drift = served(
            "verify",
            json!({"version":1,"environment":"a","checked_at":"now","baseline":{"entry_id":-3,
                "applied_at":"now","checksum":"c"},"live_checksum":"c","changes":[]}),
        );
        assert_eq!(drift["baseline"]["entry_id"], json!("-3"));
        assert_eq!(drift["version"], json!(1));
        // The CLI side stays strict: an id sent as a string is not its contract.
        let cli = json!({"schema_version":1,"tool_version":"0.0.0","command":"verify",
            "result":"ok","findings":[],"data":{"version":1,"environment":"a","checked_at":"now",
            "baseline":{"entry_id":"7","applied_at":"now","checksum":"c"},
            "live_checksum":"c","changes":[]}});
        assert!(parse("verify", &serde_json::to_vec(&cli).unwrap(), 0).is_err());
    }

    #[test]
    fn unknown_fields_and_incompatible_envelopes_are_refused() {
        let base = json!({"schema_version":1,"tool_version":"0.0.0","command":"status",
            "result":"ok","findings":[],"data":[{"environment":"dev","state":"unconfigured","checked_at":"now"}]});
        assert!(parse("status", &serde_json::to_vec(&base).unwrap(), 0).is_ok());
        for pointer in ["", "/data/0"] {
            let mut changed = base.clone();
            changed.pointer_mut(pointer).unwrap()["new_field"] = json!(true);
            assert!(parse("status", &serde_json::to_vec(&changed).unwrap(), 0).is_err());
        }
        for (field, value) in [
            ("schema_version", json!(2)),
            ("command", json!("verify")),
            ("result", json!("unanswerable")),
        ] {
            let mut changed = base.clone();
            changed[field] = value;
            assert!(parse("status", &serde_json::to_vec(&changed).unwrap(), 0).is_err());
        }
        let mut unknown_finding = base.clone();
        unknown_finding["findings"] =
            json!([{"id":"a","severity":"note","message":"m","unknown":true}]);
        assert!(parse("status", &serde_json::to_vec(&unknown_finding).unwrap(), 0).is_err());
        assert!(parse("status", b"not JSON", 1).is_err());
        assert!(parse("apply", b"{}", 0).is_err());
    }

    fn resolver_detail() -> serde_json::Value {
        json!({"entry":"scratch", "scratch":"server", "layout":"run-owned",
            "outside_writes":"refused", "rule":"rule", "key_id":"key", "sealed":[],
            "limitations":[], "surfaces":[], "order":[], "prerequisites":[]})
    }

    #[test]
    fn same_instance_policy_is_optional_without_relaxing_the_resolver_contract() {
        let legacy = resolver_detail();
        for policy in [None, Some("refused"), Some("allowed")] {
            let mut value = legacy.clone();
            if let Some(policy) = policy {
                value["same_instance"] = json!(policy);
            }
            let parsed: Resolver = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), value);
        }
        let mut wrong_type = legacy.clone();
        wrong_type["same_instance"] = json!(true);
        assert!(serde_json::from_value::<Resolver>(wrong_type).is_err());
        let mut unknown = legacy;
        unknown["unknown_policy"] = json!("allowed");
        assert!(serde_json::from_value::<Resolver>(unknown).is_err());
    }

    #[test]
    fn resolver_dependency_placements_retain_shapes_and_strict_fields() {
        for (placement, shape) in [
            ("bound", None),
            ("companion", Some("compared")),
            ("companion", Some("declared, not compared")),
        ] {
            let mut dependency = json!({"name":"other.dbo.t", "placement":placement});
            if let Some(shape) = shape {
                dependency["shape"] = json!(shape);
            }
            let mut value = resolver_detail();
            value["dependencies"] = json!([dependency]);
            let parsed: Resolver = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), value);
            for (pointer, bad) in [
                ("/dependencies", json!({})),
                ("/dependencies/0/name", json!([])),
                ("/dependencies/0/shape", json!(true)),
            ] {
                let mut wrong = value.clone();
                if pointer.ends_with("/shape") {
                    wrong["dependencies"][0]["shape"] = json!("compared");
                }
                *wrong
                    .pointer_mut(pointer)
                    .unwrap_or_else(|| panic!("missing control location {pointer}")) = bad;
                assert!(serde_json::from_value::<Resolver>(wrong).is_err());
            }
            let mut unknown = value;
            unknown["dependencies"][0]["new_field"] = json!(true);
            assert!(serde_json::from_value::<Resolver>(unknown).is_err());
        }
    }

    #[test]
    fn mssql_explanations_keep_reviewed_drop_and_assertion_details() {
        let mut resolver = resolver_detail();
        resolver["same_instance"] = json!("refused");
        resolver["dependencies"] = json!([{"name":"other.dbo.t", "placement":"bound"}]);
        let base = json!({"schema_version":1,"tool_version":"0.0.0","command":"explain",
            "result":"ok","findings":[],"data":{"applyable":true,"dialect":"mssql",
            "created_at":"now","baseline":"prod","mode":"transactional","checksum":"c",
            "change_count":1,"table_count":1,"module_count":0,"role_count":0,"risks":[],
            "approve_with":"pbps apply","probes":[],"unchecked":[],"statement_count":1,
            "resolver":resolver,"drop_requirements":["dbo.t must be empty"],
            "target_assertions":[{"path":"checks/empty.sql","sha256":"a".repeat(64),"outcome":"pass"}]}});
        let served = parse("explain", &serde_json::to_vec(&base).unwrap(), 0).unwrap();
        let served: serde_json::Value = serde_json::from_slice(&served).unwrap();
        for field in ["drop_requirements", "target_assertions", "resolver"] {
            assert_eq!(served["data"][field], base["data"][field]);
        }
        let mut legacy = base.clone();
        for field in ["drop_requirements", "target_assertions"] {
            legacy["data"].as_object_mut().unwrap().remove(field);
        }
        for field in ["same_instance", "dependencies"] {
            legacy["data"]["resolver"]
                .as_object_mut()
                .unwrap()
                .remove(field);
        }
        let served = parse("explain", &serde_json::to_vec(&legacy).unwrap(), 0).unwrap();
        let served: serde_json::Value = serde_json::from_slice(&served).unwrap();
        assert!(served["data"].get("drop_requirements").is_none());
        assert!(served["data"].get("target_assertions").is_none());
        assert!(served["data"]["resolver"].get("same_instance").is_none());
        assert!(served["data"]["resolver"].get("dependencies").is_none());
        for (pointer, bad) in [
            ("/data/drop_requirements", json!([true])),
            ("/data/target_assertions", json!({})),
            ("/data/target_assertions/0/sha256", json!(7)),
            ("/data/target_assertions/0/outcome", json!("violation")),
        ] {
            let mut wrong = base.clone();
            *wrong.pointer_mut(pointer).unwrap() = bad;
            assert!(parse("explain", &serde_json::to_vec(&wrong).unwrap(), 0).is_err());
        }
        let mut unknown = base;
        unknown["data"]["target_assertions"][0]["rows"] = json!(0);
        assert!(parse("explain", &serde_json::to_vec(&unknown).unwrap(), 0).is_err());
    }

    #[test]
    fn dependencies_retain_the_declared_shape_and_refuse_unknown_outcomes_and_kinds() {
        for kind in ["table", "view"] {
            for comparison in ["compared", "declared, not compared"] {
                let value = json!({"declaration": {"kind": kind, "columns": {"id": {"type": "int", "nullable": false}}}, "comparison": comparison});
                let parsed: Dependency = serde_json::from_value(value.clone()).unwrap();
                assert_eq!(serde_json::to_value(parsed).unwrap(), value);
                let mut bad = value.clone();
                bad["comparison"] = "verified".into();
                assert!(serde_json::from_value::<Dependency>(bad).is_err());
                let mut bad = value;
                bad["declaration"]["kind"] = "procedure".into();
                assert!(serde_json::from_value::<Dependency>(bad).is_err());
            }
        }
    }

    #[test]
    fn an_unanswerable_read_keeps_its_findings_and_has_no_invented_data() {
        for command in ["status", "verify", "explain", "state list"] {
            let value = json!({"schema_version":1,"tool_version":"0.0.0","command":command,
                "result":"unanswerable","findings":[{"id":"unreachable","severity":"error","message":"Cannot read","remedy":"pbps doctor"}]});
            let parsed = parse(command, &serde_json::to_vec(&value).unwrap(), 1).unwrap();
            let parsed: serde_json::Value = serde_json::from_slice(&parsed).unwrap();
            assert!(parsed.get("data").is_none());
            assert_eq!(parsed["findings"][0]["remedy"], "pbps doctor");
            assert!(parse(command, &serde_json::to_vec(&value).unwrap(), 0).is_err());
        }
    }
}
