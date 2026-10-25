//! The lineage of a blocking table rewrite (DEC-2298.1, DEC-2298.12).

use std::collections::BTreeMap;

use crate::{ColumnRef, IdsFile, Reason, TableName, Uid};

/// Both ends of one approved rewrite. The source identities are tombstones in
/// the file that holds the record and the replacement ones are live (or later
/// retired like any other), so neither end is a state of its own.
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

/// One lineage record, `source -> replacement`, written when a human approves a
/// rewrite. A declaration fingerprint pins the approval without copying
/// declarations into the identity file; the audit fields follow `Move`'s git
/// record. The record stays after the rewrite: it is how an environment that
/// still holds the source learns what to rebuild.
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

impl Rebuild {
    /// The replacement parent. A rewrite keeps the parent's name, so the
    /// replacement entry with the source parent's name is the one. `None` for a
    /// record whose source parent is not in its own source (a corrupt file).
    pub fn replacement_root(&self, source_root: &Uid) -> Option<&Uid> {
        let name = self.source.tables.get(source_root)?;
        self.replacement
            .tables
            .iter()
            .find_map(|(uid, n)| (n == name).then_some(uid))
    }
}
