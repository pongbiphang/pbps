//! Self-reported external work. These records contain identities, never SQL to run.

use crate::Text;

pub const NOTES_BYTES: usize = 4096;
pub const EXTERNAL_VERSION_BYTES: usize = 1024;

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DataWorkIdentity {
    Script { path: Text, sha256: String },
    ExternalVersion { version: Text },
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DataWorkOutcome {
    Completed,
    Failed,
}

impl std::str::FromStr for DataWorkOutcome {
    type Err = DataWorkError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            _ => Err(DataWorkError("outcome must be completed or failed".into())),
        }
    }
}

impl std::fmt::Display for DataWorkOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
        })
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DataWorkRecord {
    pub identity: DataWorkIdentity,
    pub expand_checksum: String,
    pub outcome: DataWorkOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<Text>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct DataWorkError(pub String);

impl DataWorkRecord {
    pub fn validate(&self) -> Result<(), DataWorkError> {
        let hash = |value: &str| value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit());
        if !hash(&self.expand_checksum) {
            return Err(DataWorkError(
                "expand checksum must contain exactly 64 hexadecimal characters".into(),
            ));
        }
        match &self.identity {
            DataWorkIdentity::Script { path, sha256 } => {
                if path.raw().trim().is_empty() || !hash(sha256) {
                    return Err(DataWorkError(
                        "script identity requires a nonblank path and a SHA-256 content hash"
                            .into(),
                    ));
                }
            }
            DataWorkIdentity::ExternalVersion { version } => {
                if version.raw().trim().is_empty() || version.raw().len() > EXTERNAL_VERSION_BYTES {
                    return Err(DataWorkError(format!(
                        "external version must be nonblank and at most {EXTERNAL_VERSION_BYTES} UTF-8 bytes"
                    )));
                }
            }
        }
        if self
            .notes
            .as_ref()
            .is_some_and(|n| n.raw().len() > NOTES_BYTES)
        {
            return Err(DataWorkError(format!(
                "notes exceed {NOTES_BYTES} UTF-8 bytes"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IdsFile, Reason, Schema, StateKind, StateSnapshot};

    fn work() -> DataWorkRecord {
        DataWorkRecord {
            identity: DataWorkIdentity::ExternalVersion {
                version: "job-v7".into(),
            },
            expand_checksum: "a".repeat(64),
            outcome: DataWorkOutcome::Completed,
            notes: None,
        }
    }
    fn recorded() -> StateSnapshot {
        StateSnapshot::new(
            StateKind::Apply,
            Schema::default(),
            IdsFile::default(),
            "deployer",
        )
        .recording_data_work(
            work(),
            "reporter".into(),
            Reason::new("backfill").unwrap(),
            Some("operator-branch".into()),
        )
        .unwrap()
    }

    #[test]
    fn data_work_is_not_schema_provenance_even_when_completed() {
        assert_eq!(
            StateKind::ALL
                .into_iter()
                .filter(|k| k.establishes_schema())
                .collect::<Vec<_>>(),
            [
                StateKind::Apply,
                StateKind::Baseline,
                StateKind::Bootstrap,
                StateKind::Staged
            ]
        );
        assert!(!StateKind::Failed.establishes_schema());
        assert!(!recorded().kind.establishes_schema());
    }

    #[test]
    fn data_work_upgrades_a_known_old_snapshot_without_rewriting_it() {
        let mut old = StateSnapshot::new(
            StateKind::Apply,
            Schema::default(),
            IdsFile::default(),
            "deployer",
        );
        old.version = 33;
        old.git_sha = Some("deployed-branch".into());
        old.plan_checksum = Some("a".repeat(64));
        let bytes = serde_json::to_string(&old).unwrap();
        let read = StateSnapshot::from_json(&bytes).unwrap();
        assert_eq!(read.version, 33);
        assert!(read.data_work.is_none());
        let new = read
            .recording_data_work(
                work(),
                "reporter".into(),
                Reason::new("backfill").unwrap(),
                Some("operator-branch".into()),
            )
            .unwrap();
        assert_eq!(new.version, 35);
        assert_eq!(new.schema, old.schema);
        assert_eq!(new.ids, old.ids);
        assert_eq!(new.module_deps, old.module_deps);
        assert_eq!(new.declared, old.declared);
        assert_eq!(new.baseline_evidence, old.baseline_evidence);
        assert!(new.plan_checksum.is_none());
        assert_eq!(new.git_sha.unwrap().raw(), "operator-branch");
        assert_eq!(serde_json::to_string(&old).unwrap(), bytes);
    }

    #[test]
    fn data_work_metadata_must_be_complete_and_belong_to_its_kind_and_version() {
        let value = serde_json::to_value(recorded()).unwrap();
        assert!(StateSnapshot::from_json(&value.to_string()).is_ok());
        for (path, replacement) in [
            ("/data_work", serde_json::Value::Null),
            ("/kind", serde_json::json!("apply")),
            ("/version", serde_json::json!(33)),
            ("/version", serde_json::json!(5)),
            ("/version", serde_json::json!(999)),
            ("/reason", serde_json::json!(" \n")),
            ("/plan_checksum", serde_json::json!("a".repeat(64))),
            (
                "/staged",
                serde_json::json!({"completed": 1, "total": 2, "last_statement": "x"}),
            ),
            (
                "/data_work/expand_checksum",
                serde_json::json!("not-a-checksum"),
            ),
            (
                "/data_work/identity",
                serde_json::json!({"kind":"external_version", "version":" "}),
            ),
            (
                "/data_work/identity",
                serde_json::json!({"kind":"script", "path":"x", "sha256":"bad"}),
            ),
            ("/data_work/outcome", serde_json::json!("running")),
        ] {
            let mut bad = value.clone();
            if path[1..].contains('/') {
                *bad.pointer_mut(path).unwrap() = replacement;
            } else {
                bad[&path[1..]] = replacement;
            }
            assert!(
                StateSnapshot::from_json(&bad.to_string()).is_err(),
                "{path}: {bad}"
            );
        }
        let mut staged = recorded();
        staged.staged = Some(crate::StagedProgress {
            completed: 2,
            total: 2,
            last_statement: "x".into(),
        });
        assert!(
            staged
                .recording_data_work(work(), "r".into(), Reason::new("why").unwrap(), None)
                .is_err()
        );
    }

    #[test]
    fn data_work_bounds_count_utf8_bytes_and_do_not_truncate() {
        let mut w = work();
        w.notes = Some("é".repeat(NOTES_BYTES / 2).into());
        w.identity = DataWorkIdentity::ExternalVersion {
            version: "é".repeat(EXTERNAL_VERSION_BYTES / 2).into(),
        };
        assert!(w.validate().is_ok());
        w.notes = Some(("é".repeat(NOTES_BYTES / 2) + "x").into());
        assert!(w.validate().is_err());
        w.notes = None;
        w.identity = DataWorkIdentity::ExternalVersion {
            version: ("é".repeat(EXTERNAL_VERSION_BYTES / 2) + "x").into(),
        };
        assert!(w.validate().is_err());
        w.identity = DataWorkIdentity::Script {
            path: " ".into(),
            sha256: "b".repeat(64),
        };
        assert!(w.validate().is_err());
        assert!("failed".parse::<DataWorkOutcome>().is_ok());
        assert!("running".parse::<DataWorkOutcome>().is_err());
    }
}
