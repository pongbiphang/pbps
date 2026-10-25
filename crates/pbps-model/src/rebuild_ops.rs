//! Typed, non-executable blocking-rewrite inputs (DEC-2298.1).

use std::collections::BTreeMap;

use crate::{Collation, ColumnRef, ColumnType, Rebuild, Table, TableName, Uid};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RebuildPartitionedTable {
    pub table: TableName,
    /// The latest approval: the one the files' identities came from.
    pub intent: Rebuild,
    /// The approvals before it, oldest first, when the environment holds an
    /// older link of a chain. It is rebuilt once, from the oldest source it
    /// holds to the latest replacement (DEC-2298.12). Empty for one link.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub earlier: Vec<Rebuild>,
    pub before: BTreeMap<TableName, Table>,
    pub after: BTreeMap<TableName, Table>,
}

/// One state of a column on its way along a chain of approvals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnStage {
    pub uid: Uid,
    pub column: ColumnRef,
    /// What the approval declared it as; `None` for the environment's own
    /// column, whose type its baseline holds.
    pub shape: Option<crate::ColumnShape>,
}

/// A final column's states, oldest first, ending in its latest replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnLineage {
    /// Whether the first stage is a column the environment holds. A column born
    /// along the chain has none: the copy gives it no source value.
    pub from_environment: bool,
    pub stages: Vec<ColumnStage>,
}

impl RebuildPartitionedTable {
    /// Every link, oldest first.
    pub fn links(&self) -> impl Iterator<Item = &Rebuild> {
        self.earlier.iter().chain(std::iter::once(&self.intent))
    }

    /// The identities the environment holds: the oldest link's source.
    pub fn source(&self) -> &crate::RebuildIdentities {
        &self.links().next().expect("a root has a link").source
    }

    /// Every link's cap applies to the one copy, so the strictest is the cap.
    pub fn cap(&self) -> u64 {
        self.links()
            .map(|l| l.max_rows)
            .min()
            .expect("a root has a link")
    }

    /// Each final column's states along the lineage UIDs: inside a link a
    /// column is followed through what the approval paired, and between links
    /// by its UID, so a rename at either place is followed. The states' types
    /// are the nested casts the one copy composes. Ordinary edits made between
    /// two approvals are not states of their own: they fold into the next cast.
    pub fn composed_columns(&self) -> BTreeMap<ColumnRef, ColumnLineage> {
        let links: Vec<&Rebuild> = self.links().collect();
        let last = links.len() - 1;
        let mut out = BTreeMap::new();
        for (uid, column) in &links[last].replacement.columns {
            let mut stages = vec![ColumnStage {
                uid: uid.clone(),
                column: column.clone(),
                shape: links[last].types.get(uid).cloned(),
            }];
            let (mut current, mut from_environment) = (uid.clone(), false);
            for link in (0..=last).rev() {
                // The approval paired this column with the one it continues.
                let Some(source) = links[link].continues.get(&current) else {
                    break;
                };
                let Some(named) = links[link].source.columns.get(source) else {
                    break;
                };
                if link == 0 {
                    stages.push(ColumnStage {
                        uid: source.clone(),
                        column: named.clone(),
                        shape: None,
                    });
                    from_environment = true;
                    break;
                }
                // Between links the UID is the previous link's replacement.
                let Some(before) = links[link - 1].replacement.columns.get(source) else {
                    break;
                };
                stages.push(ColumnStage {
                    uid: source.clone(),
                    column: before.clone(),
                    shape: links[link - 1].types.get(source).cloned(),
                });
                current = source.clone();
            }
            stages.reverse();
            out.insert(
                column.clone(),
                ColumnLineage {
                    from_environment,
                    stages,
                },
            );
        }
        out
    }
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
