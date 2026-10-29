//! Immutable input bindings into the same name-property version chains.
use super::symbolic::{Attribute, Key, Property, Version};
use super::*;
use std::collections::BTreeSet;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Name {
    Table(TableName),
    Column(crate::ColumnName),
    Absent,
}
type Read = BTreeMap<Key, Version>;

/// One shared address history; readers consume versions, never spelling flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddressChain {
    cells: BTreeMap<(Key, Version), Name>,
    base: Arc<Read>,
    goal: Arc<Read>,
    readers: BTreeMap<OperationId, Arc<Read>>,
    origins: BTreeMap<OperationId, Origin>,
    writes: BTreeMap<OperationId, Vec<Key>>,
    chains: BTreeMap<Key, Vec<Version>>,
    logical: Vec<(usize, Key, Version)>,
}
impl AddressChain {
    /// Capture the already selected name boundary. This neither chooses an
    /// order nor replays membership. Pending families keep their role inputs.
    pub fn bind(
        names: &PlanNames<'_>,
        operations: &Operations,
        before: &IdsFile,
        after: &IdsFile,
        excluded: &BTreeSet<Uid>,
        readers: &BTreeSet<OperationId>,
    ) -> Self {
        if readers.is_empty() {
            return Self::default();
        }
        assert_eq!(names.changes().len(), operations.len());
        let tables: BTreeSet<_> = before
            .tables
            .keys()
            .chain(after.tables.keys())
            .filter(|u| !excluded.contains(*u))
            .cloned()
            .collect();
        let state = |at| {
            let mut values = BTreeMap::new();
            for uid in &tables {
                values.insert(
                    Key {
                        parent: Recorded(uid.clone()),
                        property: Property::Address,
                    },
                    names
                        .table_at(uid, at)
                        .cloned()
                        .map_or(Name::Absent, Name::Table),
                );
            }
            for (uid, column) in names.columns_at(at) {
                if let Holder::Tracked(owner) = names.holder(&column.table, at)
                    && tables.contains(owner)
                {
                    values.insert(
                        Key {
                            parent: Recorded(owner.clone()),
                            property: Property::Column(
                                ColumnKey::Recorded(Recorded(uid.clone())),
                                Attribute::Name,
                            ),
                        },
                        Name::Column(column.name),
                    );
                }
            }
            values
        };
        let mut result = Self::default();
        let mut values = state(At::Start);
        let mut versions = Read::new();
        for (key, value) in &values {
            versions.insert(key.clone(), Version::Base);
            result
                .cells
                .insert((key.clone(), Version::Base), value.clone());
            result.chains.insert(key.clone(), vec![Version::Base]);
        }
        let mut current = Arc::new(versions.clone());
        result.base = current.clone();
        for (at, op) in operations.iter().enumerate() {
            if readers.contains(&op.id()) {
                result.readers.insert(op.id(), current.clone());
                result.origins.insert(op.id(), op.logical_origin().clone());
            }
            let next = state(At::After(at));
            let keys: BTreeSet<_> = values.keys().chain(next.keys()).cloned().collect();
            let mut changed = false;
            for key in keys {
                let old = values.get(&key).unwrap_or(&Name::Absent);
                let new = next.get(&key).unwrap_or(&Name::Absent);
                if old == new {
                    continue;
                }
                if !versions.contains_key(&key) {
                    // Missing from earlier sparse reads means this declared
                    // property is still at its known-absent base version.
                    result
                        .cells
                        .insert((key.clone(), Version::Base), Name::Absent);
                    result.chains.insert(key.clone(), vec![Version::Base]);
                }
                changed = true;
                let version = Version::After(op.id());
                versions.insert(key.clone(), version);
                result.cells.insert((key.clone(), version), new.clone());
                result.chains.get_mut(&key).unwrap().push(version);
                result.writes.entry(op.id()).or_default().push(key.clone());
                if let Source::Change(logical) = op.source() {
                    result.logical.push((logical, key, version));
                }
            }
            if changed {
                current = Arc::new(versions.clone());
            }
            values = next;
        }
        result.goal = current;
        result
    }

    fn spelling(&self, id: OperationId, intrinsic: Spelling) -> Spelling {
        let Some(read) = self.readers.get(&id) else {
            return intrinsic;
        };
        let matches = match intrinsic {
            Spelling::Base => read == &self.base,
            Spelling::Final => read == &self.goal,
            Spelling::At(at) => {
                let boundary = match at {
                    At::Start => 0,
                    At::End => usize::MAX,
                    At::Before(i) => i,
                    At::After(i) => i + 1,
                };
                let mut expected = self.base.as_ref().clone();
                let mut writes = self
                    .logical
                    .iter()
                    .filter(|(at, _, _)| *at < boundary)
                    .collect::<Vec<_>>();
                writes.sort_by_key(|(at, _, _)| *at);
                for (_, key, version) in writes {
                    expected.insert(key.clone(), *version);
                }
                read.as_ref() == &expected
            }
            Spelling::Input(_) => return intrinsic,
        };
        if matches {
            intrinsic
        } else if read == &self.base {
            Spelling::Base
        } else if read == &self.goal {
            Spelling::Final
        } else {
            Spelling::Input(id)
        }
    }
    pub fn before(&self, reader: OperationId, writer: OperationId) -> Option<bool> {
        let read = self.readers.get(&reader)?;
        let key = self.writes.get(&writer)?.first()?;
        let version = read.get(key)?;
        let chain = self.chains.get(key)?;
        let reader_at = chain.iter().position(|v| v == version)?;
        let writer_at = chain.iter().position(|v| *v == Version::After(writer))?;
        Some(reader_at < writer_at)
    }

    fn table(&self, id: OperationId, name: &TableName) -> Result<Uid, OwnerError> {
        let read = self.readers.get(&id).ok_or(OwnerError::Outside {
            spelling: Spelling::Input(id),
        })?;
        let mut found = read.iter().filter_map(|(key, version)| {
            (key.property == Property::Address
                && self.cells.get(&(key.clone(), *version)) == Some(&Name::Table(name.clone())))
            .then_some(key.parent.uid())
        });
        let uid = found.next().ok_or_else(|| {
            let spelling = Spelling::Input(id);
            if self
                .cells
                .values()
                .any(|value| value == &Name::Table(name.clone()))
            {
                OwnerError::Vacant {
                    name: name.shown().to_string(),
                    spelling,
                }
            } else {
                OwnerError::Unmanaged {
                    name: name.shown().to_string(),
                    spelling,
                }
            }
        })?;
        if found.next().is_some() {
            return Err(OwnerError::Rebuild {
                name: name.shown().to_string(),
                why: "input address has multiple recorded owners",
            });
        }
        Ok(uid.clone())
    }
    fn column(&self, id: OperationId, name: &ColumnRef) -> Result<Uid, OwnerError> {
        let owner = self.table(id, &name.table)?;
        let read = self.readers.get(&id).unwrap();
        let mut found = read.iter().filter_map(|(key, version)| {
            if key.parent.uid() != &owner
                || self.cells.get(&(key.clone(), *version))
                    != Some(&Name::Column(name.name.clone()))
            {
                return None;
            }
            if let Property::Column(ColumnKey::Recorded(uid), Attribute::Name) = &key.property {
                Some(uid.uid())
            } else {
                None
            }
        });
        let uid = found.next().ok_or_else(|| {
            let spelling = Spelling::Input(id);
            if self.cells.iter().any(|((key, _), value)| {
                key.parent.uid() == &owner && value == &Name::Column(name.name.clone())
            }) {
                OwnerError::Vacant {
                    name: name.shown().to_string(),
                    spelling,
                }
            } else {
                OwnerError::Unmanaged {
                    name: name.shown().to_string(),
                    spelling,
                }
            }
        })?;
        if found.next().is_some() {
            return Err(OwnerError::Rebuild {
                name: name.shown().to_string(),
                why: "input column has multiple recorded owners",
            });
        }
        Ok(uid.clone())
    }
}

/// Logical provenance is unchanged; input spelling is read from its chain.
pub struct OriginView<'a>(pub(super) &'a Operation);
impl std::ops::Deref for OriginView<'_> {
    type Target = Origin;
    fn deref(&self) -> &Origin {
        &self.0.origin
    }
}
impl OriginView<'_> {
    pub fn spelling(&self) -> Spelling {
        self.0
            .addresses
            .spelling(self.0.id, self.0.origin.spelling())
    }
}
impl fmt::Debug for OriginView<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.origin.fmt(f)
    }
}
impl PartialEq for OriginView<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.0.origin == other.0.origin
    }
}
impl PartialEq<&Origin> for OriginView<'_> {
    fn eq(&self, other: &&Origin) -> bool {
        &self.0.origin == *other
    }
}
impl PartialEq<OriginView<'_>> for &Origin {
    fn eq(&self, other: &OriginView<'_>) -> bool {
        *self == &other.0.origin
    }
}

pub struct InputOwners<'a> {
    original: &'a dyn Owners,
    chain: &'a AddressChain,
}
impl Owners for InputOwners<'_> {
    fn address_qualified(&self) -> bool {
        self.original.address_qualified()
    }
    fn table(&self, name: &TableName, spelling: Spelling) -> Result<Uid, OwnerError> {
        if let Spelling::Input(id) = spelling {
            self.chain.table(id, name)
        } else {
            self.original.table(name, spelling)
        }
    }
    fn column(&self, name: &ColumnRef, spelling: Spelling) -> Result<Uid, OwnerError> {
        if let Spelling::Input(id) = spelling {
            self.chain.column(id, name)
        } else {
            self.original.column(name, spelling)
        }
    }
    fn role(&self, name: &crate::RoleName, spelling: Spelling) -> Result<Uid, OwnerError> {
        let spelling = if let Spelling::Input(id) = spelling {
            self.chain
                .origins
                .get(&id)
                .ok_or(OwnerError::Outside { spelling })?
                .spelling()
        } else {
            spelling
        };
        self.original.role(name, spelling)
    }
    fn managed_type(&self, name: &crate::TypeRef, spelling: Spelling) -> Result<Uid, OwnerError> {
        let spelling = if let Spelling::Input(id) = spelling {
            self.chain
                .origins
                .get(&id)
                .ok_or(OwnerError::Outside { spelling })?
                .spelling()
        } else {
            spelling
        };
        self.original.managed_type(name, spelling)
    }
}
impl Operation {
    pub fn logical_origin(&self) -> &Origin {
        &self.origin
    }
    pub fn input_owners<'a>(&'a self, owners: &'a dyn Owners) -> InputOwners<'a> {
        InputOwners {
            original: owners,
            chain: &self.addresses,
        }
    }
    pub fn address_before(&self, writer: &Operation) -> Option<bool> {
        self.addresses.before(self.id, writer.id)
    }
}
impl Operations {
    pub fn with_address_chain(&self, chain: AddressChain) -> Self {
        let chain = Arc::new(chain);
        Self::from_operations(self.iter().cloned().map(|mut op| {
            op.addresses = chain.clone();
            op
        }))
    }
}

pub fn effects(
    op: &Operation,
    owners: &dyn Owners,
    before: Snapshot<'_>,
    after: Snapshot<'_>,
) -> Result<Effects, OwnerError> {
    super::effects::effects(op, &op.input_owners(owners), before, after)
}
pub fn base_table<'a>(
    op: &Operation,
    owners: &dyn Owners,
    before: Snapshot<'a>,
    name: &TableName,
) -> Result<Option<BaseTable<'a>>, OwnerError> {
    super::effects::base_table(op, &op.input_owners(owners), before, name)
}
