//! The lineage of a blocking table rewrite (DEC-2298.1, DEC-2298.12).

use std::collections::BTreeMap;

use crate::{Collation, ColumnRef, ColumnType, IdsFile, Reason, TableName, Uid};

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
    /// The replacement parent, by identity. Which replacement entry is the
    /// parent is not left to the names, which a rename after the approval
    /// changes on one side only (DEC-2298.12).
    pub replacement_root: Uid,
    /// The source identity each replacement identity continues: the parent
    /// and every column that kept its meaning through the rewrite, paired by
    /// name at the one moment that is evidence of it, the approval. A
    /// replacement member without an entry was added since, and a source
    /// member without one is not carried over. Renames afterwards leave the
    /// pairing as it is.
    pub continues: BTreeMap<Uid, Uid>,
    pub target_checksum: String,
    pub max_rows: u64,
    pub allowed_at: String,
    pub reason: Reason,
    pub operator: String,
    /// What each replacement column was declared as when this link was
    /// approved. An environment that holds an older link is rebuilt once, to
    /// the latest, and the conversions along the way compose as nested casts;
    /// the declarations of the revisions in between are not available then, so
    /// the approval keeps the types (DEC-2298.12).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub types: BTreeMap<Uid, ColumnShape>,
    /// Conversion history belongs to this link, paired with replacement UIDs.
    /// The global map continues to describe live columns only (DEC-2298.13).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub conversions: BTreeMap<Uid, crate::Conversion>,
}

/// A replacement column's type and collation at approval.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnShape {
    pub ty: ColumnType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collation: Option<Collation>,
}

impl Rebuild {
    /// The source parent. The record is keyed by it, and `continues` ties the
    /// replacement parent to it, so no name is read to find either end.
    pub fn source_root(&self) -> Option<&Uid> {
        self.continues.get(&self.replacement_root)
    }

    /// Whether every pairing joins a replacement member to a source member of
    /// the same kind, and no source member is continued twice.
    pub fn continues_are_whole(&self) -> bool {
        let mut taken = std::collections::BTreeSet::new();
        self.continues.iter().all(|(to, from)| {
            let table =
                self.replacement.tables.contains_key(to) && self.source.tables.contains_key(from);
            let column =
                self.replacement.columns.contains_key(to) && self.source.columns.contains_key(from);
            (table || column) && taken.insert(from)
        })
    }

    /// The source column a replacement column continues, as the source named
    /// it. The replacement is found by the name it had when this record last
    /// described it, which is one snapshot and so unambiguous; the pairing
    /// itself is by UID, so a rename on either side since does not break it.
    pub fn source_column(&self, replacement: &ColumnRef) -> Option<&ColumnRef> {
        let (uid, _) = self
            .replacement
            .columns
            .iter()
            .find(|(_, column)| *column == replacement)?;
        self.source.columns.get(self.continues.get(uid)?)
    }
}
