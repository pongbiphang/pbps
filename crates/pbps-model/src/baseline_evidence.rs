//! Catalog evidence for the DEC-1852.1 baseline refusal only (ADR-0018).
//! No oid accessor exists: callers can record an opaque value or ask this one
//! check, never use catalog numbers to match identities in a diff or plan.

use schemars::JsonSchema;
use std::collections::BTreeMap;

use crate::{Schema, TableName};

/// Persistable evidence with no catalog-number accessor.
///
/// ```compile_fail
/// use pbps_model::baseline_evidence::BaselineEvidence;
/// fn rename_identity(evidence: &BaselineEvidence) {
///     let _ = &evidence.tables;
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BaselineEvidence {
    system_identifier: String,
    database_oid: i64,
    tables: BTreeMap<TableName, i64>,
}

impl BaselineEvidence {
    /// Catalog ingestion for recording only; no numbers can be read back.
    pub fn from_postgres(
        system_identifier: String,
        database_oid: i64,
        tables: impl IntoIterator<Item = (TableName, i64)>,
    ) -> Self {
        Self {
            system_identifier,
            database_oid,
            tables: tables.into_iter().collect(),
        }
    }

    /// Preserve opacity while cutting a catalog recording to the managed set.
    pub fn for_managed(&self, schema: &Schema) -> Self {
        Self {
            system_identifier: self.system_identifier.clone(),
            database_oid: self.database_oid,
            tables: self
                .tables
                .iter()
                .filter(|(name, _)| schema.tables.contains_key(*name))
                .map(|(name, oid)| (name.clone(), *oid))
                .collect(),
        }
    }
}

/// A complete pg_class read, including relations the schema cannot represent.
/// It is neither serializable nor convertible to a schema or identity map.
#[derive(Debug)]
pub struct BaselineCatalog {
    system_identifier: String,
    database_oid: i64,
    relations: BTreeMap<i64, (TableName, Option<i64>)>,
}

/// The only observation callers can obtain from the opaque catalog evidence.
#[derive(Debug, PartialEq, Eq)]
pub enum HeldPartition {
    ParentRefusal,
    Present(TableName),
    Dropped,
}

impl BaselineCatalog {
    /// Engine ingestion only; every pg_class row must be supplied. Omitting an
    /// unsupported relation would mistake unreadable for absent.
    pub fn from_postgres(
        system_identifier: String,
        database_oid: i64,
        relations: impl IntoIterator<Item = (i64, TableName, Option<i64>)>,
    ) -> Self {
        Self {
            system_identifier,
            database_oid,
            relations: relations
                .into_iter()
                .map(|(oid, name, parent)| (oid, (name, parent)))
                .collect(),
        }
    }

    /// Narrow only the pending-drop baseline refusal. Missing evidence and a
    /// different cluster/database retain DEC-1852.1's conservative answer.
    pub fn held_partition(
        &self,
        recorded: Option<&BaselineEvidence>,
        name: &TableName,
        recorded_parent: &TableName,
        working_parent: &TableName,
    ) -> HeldPartition {
        let Some(recorded) = recorded else {
            return HeldPartition::ParentRefusal;
        };
        if recorded.system_identifier != self.system_identifier
            || recorded.database_oid != self.database_oid
        {
            return HeldPartition::ParentRefusal;
        }
        let (Some(oid), Some(parent_oid)) = (
            recorded.tables.get(name),
            recorded.tables.get(recorded_parent),
        ) else {
            return HeldPartition::ParentRefusal;
        };
        match self.relations.get(oid) {
            None => HeldPartition::Dropped,
            Some((live, Some(parent)))
                if parent == parent_oid
                    && self
                        .relations
                        .get(parent)
                        .is_some_and(|(name, _)| name == working_parent) =>
            {
                HeldPartition::Present(live.clone())
            }
            Some(_) => HeldPartition::ParentRefusal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_matching_database_evidence_narrows_the_parent_refusal() {
        let parent = TableName::new("app", "ev");
        let held = TableName::new("app", "held");
        let renamed = TableName::new("app", "renamed");
        let catalog = |cluster: &str, database, child| {
            BaselineCatalog::from_postgres(
                cluster.into(),
                database,
                [(1, parent.clone(), None)].into_iter().chain(child),
            )
        };
        let original = catalog("cluster", 10, Some((2, held.clone(), Some(1))));
        let mut schema = Schema::default();
        schema.tables.insert(parent.clone(), Default::default());
        schema.tables.insert(held.clone(), Default::default());
        let recorded = BaselineEvidence::from_postgres(
            "cluster".into(),
            10,
            [(parent.clone(), 1), (held.clone(), 2)],
        )
        .for_managed(&schema);
        let check = |target: &BaselineCatalog, evidence| {
            target.held_partition(evidence, &held, &parent, &parent)
        };
        assert_eq!(check(&original, None), HeldPartition::ParentRefusal);
        assert_eq!(
            check(&catalog("other", 10, None), Some(&recorded)),
            HeldPartition::ParentRefusal
        );
        assert_eq!(
            check(&catalog("cluster", 11, None), Some(&recorded)),
            HeldPartition::ParentRefusal
        );
        assert_eq!(
            check(
                &catalog("cluster", 10, Some((2, renamed.clone(), Some(1)))),
                Some(&recorded)
            ),
            HeldPartition::Present(renamed.clone())
        );
        assert_eq!(
            check(
                &catalog("cluster", 10, Some((2, renamed.clone(), Some(3)))),
                Some(&recorded)
            ),
            HeldPartition::ParentRefusal
        );
        assert_eq!(
            check(
                &catalog("cluster", 10, Some((2, renamed, None))),
                Some(&recorded)
            ),
            HeldPartition::ParentRefusal
        );
        assert_eq!(
            check(&catalog("cluster", 10, None), Some(&recorded)),
            HeldPartition::Dropped
        );
        // Replacing a parent under its old name does not retain the parent.
        let replacement = BaselineCatalog::from_postgres(
            "cluster".into(),
            10,
            [(3, parent.clone(), None), (2, held.clone(), Some(3))],
        );
        assert_eq!(
            check(&replacement, Some(&recorded)),
            HeldPartition::ParentRefusal
        );
        let incomplete = recorded.for_managed(&Schema::default());
        assert_eq!(
            check(&original, Some(&incomplete)),
            HeldPartition::ParentRefusal
        );
        let back = serde_json::from_str(&serde_json::to_string(&recorded).unwrap()).unwrap();
        assert_eq!(recorded, back);
    }
}
