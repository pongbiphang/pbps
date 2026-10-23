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

/// Parent-only construction, explicit leaf builds and attachment (DEC-2298.8).
/// A key clone includes a same-named leaf constraint before attachment.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildPartitionedIndex {
    pub parent: RebuildRelation,
    pub name: crate::ObjectLocalName,
    pub definition: PartitionIndexDefinition,
    /// Every clone belongs to this parent part, never an independent leaf index.
    pub clones: Vec<PartitionIndexClone>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartitionIndexDefinition {
    Index(crate::Index),
    PrimaryKey(crate::PrimaryKey),
    Unique(crate::UniqueConstraint),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartitionIndexClone {
    pub leaf: RebuildRelation,
    pub name: crate::ObjectLocalName,
}

impl BuildPartitionedIndex {
    pub fn part(&self) -> crate::IndexPart {
        match self.definition {
            PartitionIndexDefinition::Index(_) => crate::IndexPart::Index(self.name.clone()),
            PartitionIndexDefinition::PrimaryKey(_) => crate::IndexPart::PrimaryKey,
            PartitionIndexDefinition::Unique(_) => crate::IndexPart::Unique(self.name.clone()),
        }
    }

    /// The ordinary parent definition, without recursive clone creation semantics.
    pub fn parent_change(&self) -> crate::Change {
        let table = self.parent.name.clone();
        let name = self.name.clone();
        match &self.definition {
            PartitionIndexDefinition::Index(index) => crate::Change::AddIndex {
                table,
                name,
                index: Box::new(index.clone()),
                clustered: false,
            },
            PartitionIndexDefinition::PrimaryKey(key) => crate::Change::SetPrimaryKey {
                table,
                from: None,
                to: Some(key.clone()),
                nonclustered: false,
            },
            PartitionIndexDefinition::Unique(key) => crate::Change::AddUnique {
                table,
                name,
                constraint: key.clone(),
                clustered: false,
            },
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if let PartitionIndexDefinition::PrimaryKey(key) = &self.definition
            && key.name.as_ref() != Some(&self.name)
        {
            return Err("parent primary key needs the recipe's explicit name");
        }
        let mut ids = std::collections::BTreeSet::from([&self.parent.uid]);
        let mut tables = std::collections::BTreeSet::from([&self.parent.name]);
        let mut names = std::collections::BTreeSet::from([(&self.parent.name.schema, &self.name)]);
        if self.parent.uid.kind() != crate::UidKind::Table
            || self.clones.iter().any(|c| {
                c.leaf.uid.kind() != crate::UidKind::Table
                    || !ids.insert(&c.leaf.uid)
                    || !tables.insert(&c.leaf.name)
                    || !names.insert((&c.leaf.name.schema, &c.name))
            })
        {
            return Err("clone owners or explicit names are foreign or duplicated");
        }
        Ok(())
    }
}
