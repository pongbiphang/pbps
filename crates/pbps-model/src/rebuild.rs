//! Recorded permission for a blocking table rewrite (DEC-2298.1).

use std::collections::BTreeMap;

use crate::{ColumnRef, IdsFile, Reason, TableName, Uid};

/// Both ends are recorded by the human command. Replacement identities are
/// reserved here, not installed as live identities before ordering expands it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RebuildIdentities {
    pub tables: BTreeMap<Uid, TableName>,
    pub columns: BTreeMap<Uid, ColumnRef>,
}

impl RebuildIdentities {
    pub fn as_ids(&self) -> IdsFile {
        IdsFile {
            tables: self.tables.clone(),
            columns: self.columns.clone(),
            ..IdsFile::default()
        }
    }
}

/// A declaration fingerprint pins intent without copying declarations into the
/// identity file. The command's audit fields follow `Move`'s git record.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rebuild {
    pub source: RebuildIdentities,
    pub replacement: RebuildIdentities,
    pub target_checksum: String,
    pub max_rows: u64,
    pub allowed_at: String,
    pub reason: Reason,
    pub operator: String,
}
