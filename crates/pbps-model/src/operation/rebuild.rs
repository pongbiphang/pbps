//! Intermediate spellings retain approval's identities; they confer no execution authority.
use super::{OwnerError, PlanOwners, Spelling};
use crate::{
    ColumnRef, IdsFile, IntermediateAddress, RebuildIdentities, RebuildPartitionedTable, TableName,
    Uid,
};
use std::collections::BTreeMap;

pub(super) struct Bindings {
    source: Side,
    replacement: Side,
    addresses: BTreeMap<TableName, (Uid, TableName, bool)>,
}

// Index once, as ordinary PlanOwners does; an expansion looks up every member.
struct Side {
    tables: BTreeMap<TableName, Uid>,
    columns: BTreeMap<ColumnRef, Uid>,
}
impl From<&RebuildIdentities> for Side {
    fn from(ids: &RebuildIdentities) -> Self {
        Self {
            tables: ids
                .tables
                .iter()
                .map(|(u, n)| (n.clone(), u.clone()))
                .collect(),
            columns: ids
                .columns
                .iter()
                .map(|(u, n)| (n.clone(), u.clone()))
                .collect(),
        }
    }
}

impl PlanOwners<'_> {
    /// Install explicit temporary/backup addresses from a validated approval.
    /// Slot equality and catalog occupancy remain order's separate obligations.
    pub fn with_rebuild_addresses(
        mut self,
        root: &RebuildPartitionedTable,
        recorded: &IdsFile,
        addresses: &[IntermediateAddress],
    ) -> Result<Self, OwnerError> {
        let bad = |why| OwnerError::Rebuild {
            name: root.table.shown().to_string(),
            why,
        };
        recorded
            .validate()
            .map_err(|_| bad("invalid recorded identities"))?;
        let uid = recorded
            .index()
            .table_uid(&root.table)
            .cloned()
            .ok_or_else(|| bad("source parent is not recorded"))?;
        if recorded.rebuilds.get(&uid) != Some(&root.intent) || self.rebuild.is_some() {
            return Err(bad("missing, stale or duplicate approval"));
        }
        for (uid, name) in &root.intent.source.tables {
            if self.base.table_uid(name) != Some(uid) {
                return Err(bad("source table lineage differs"));
            }
        }
        for (uid, name) in &root.intent.source.columns {
            if self.base.column_uid(name) != Some(uid) {
                return Err(bad("source column lineage differs"));
            }
        }
        let replacement = Side::from(&root.intent.replacement);
        let mut bound = BTreeMap::new();
        for address in addresses {
            let (name, source) = if let Some(name) = root.intent.source.tables.get(&address.uid) {
                (name, true)
            } else if let Some(name) = root.intent.replacement.tables.get(&address.uid) {
                (name, false)
            } else {
                return Err(bad("intermediate address has no reserved table UID"));
            };
            if self.base.table_uid(&address.address).is_some()
                || self.desired.table_uid(&address.address).is_some()
                || replacement.tables.contains_key(&address.address)
                || bound
                    .insert(
                        address.address.clone(),
                        (address.uid.clone(), name.clone(), source),
                    )
                    .is_some()
            {
                return Err(bad("intermediate address is duplicated or already managed"));
            }
        }
        self.rebuild = Some(Bindings {
            source: Side::from(&root.intent.source),
            replacement,
            addresses: bound,
        });
        Ok(self)
    }
}

impl Bindings {
    fn covered(&self, name: &TableName) -> bool {
        self.source.tables.contains_key(name) || self.replacement.tables.contains_key(name)
    }
    pub(super) fn table(
        &self,
        name: &TableName,
        spelling: Spelling,
    ) -> Option<Result<Uid, OwnerError>> {
        if let Some((uid, _, source)) = self.addresses.get(name) {
            if matches!(
                (spelling, source),
                (Spelling::Base, false) | (Spelling::Final, true)
            ) {
                return Some(Err(OwnerError::Vacant {
                    name: name.shown().to_string(),
                    spelling,
                }));
            }
            return Some(Ok(uid.clone()));
        }
        let side = match spelling {
            Spelling::Base => &self.source,
            Spelling::Final => &self.replacement,
            Spelling::At(_) => {
                if self.covered(name) {
                    return Some(Err(OwnerError::Rebuild {
                        name: name.shown().to_string(),
                        why: "intermediate operation needs an explicit address",
                    }));
                }
                return None;
            }
        };
        let found = side.tables.get(name).cloned();
        let covered = self.covered(name);
        covered.then(|| {
            found.ok_or_else(|| OwnerError::Vacant {
                name: name.shown().to_string(),
                spelling,
            })
        })
    }

    pub(super) fn column(
        &self,
        column: &ColumnRef,
        spelling: Spelling,
    ) -> Option<Result<Uid, OwnerError>> {
        let (reference, side) = if let Some((_, name, source)) = self.addresses.get(&column.table) {
            if matches!(
                (spelling, source),
                (Spelling::Base, false) | (Spelling::Final, true)
            ) {
                return Some(Err(OwnerError::Vacant {
                    name: column.shown().to_string(),
                    spelling,
                }));
            }
            (
                name.column(&column.name),
                if *source {
                    &self.source
                } else {
                    &self.replacement
                },
            )
        } else {
            let side = match spelling {
                Spelling::Base => &self.source,
                Spelling::Final => &self.replacement,
                Spelling::At(_) => {
                    if self.covered(&column.table) {
                        return Some(Err(OwnerError::Rebuild {
                            name: column.shown().to_string(),
                            why: "intermediate operation needs an explicit address",
                        }));
                    }
                    return None;
                }
            };
            if !self.covered(&column.table) {
                return None;
            }
            (column.clone(), side)
        };
        Some(
            side.columns
                .get(&reference)
                .cloned()
                .ok_or_else(|| OwnerError::Unmanaged {
                    name: column.shown().to_string(),
                    spelling,
                }),
        )
    }
}
