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
    pub entry_id: i64,
    pub applied_at: String,
    pub checksum: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Explanation {
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
    pub statement_count: usize,
    pub target: Option<Target>,
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
