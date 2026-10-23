//! Typed, non-executable blocking-rewrite inputs (DEC-2298.1).

use std::collections::BTreeMap;

use crate::{Collation, ColumnRef, ColumnType, Rebuild, Table, TableName, Uid};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RebuildPartitionedTable {
    pub table: TableName,
    pub intent: Rebuild,
    pub before: BTreeMap<TableName, Table>,
    pub after: BTreeMap<TableName, Table>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RebuildRelation {
    pub uid: Uid,
    /// The address at this operation, including a temporary or backup name.
    pub name: TableName,
}

/// A bound source value, including its type for the dialect's conversion risk.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopySource {
    pub column: ColumnRef,
    pub ty: ColumnType,
    pub collation: Option<Collation>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyColumn {
    /// An added destination column has no source; it receives a typed NULL.
    pub source: Option<CopySource>,
    pub target: ColumnRef,
    pub cast: ColumnType,
    pub collation: Option<Collation>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyTableRows {
    pub source: RebuildRelation,
    pub target: RebuildRelation,
    /// Destination declaration order, never positional SELECT * inference.
    pub columns: Vec<CopyColumn>,
    pub source_partitions: Vec<RebuildRelation>,
    pub target_partitions: Vec<RebuildRelation>,
    pub max_rows: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeTableTree {
    pub parent: RebuildRelation,
    pub partitions: Vec<RebuildRelation>,
}

/// An explicit address binding, not a name allocator or an occupancy proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntermediateAddress {
    pub address: TableName,
    pub uid: Uid,
}
