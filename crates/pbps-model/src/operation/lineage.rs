//! Where the columns of a partition family physically are at the two ends of a
//! plan (DEC-2296.2).
//!
//! A partition holds no UID for the columns it inherits, so "which column of
//! this child carries that parent column" is a question about two snapshots.
//! This is the only place that answers it, and the only constructor of
//! [`ColumnKey::Partition`] slots: facts, movements and projections read the
//! answer instead of rediscovering it from names. The index describes both
//! ends; it picks no end to prefer and orders nothing.

use super::{ColumnKey, Recorded, Snapshot};
use crate::{ColumnName, TableName, Uid};
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
mod tests;

/// One end of a plan: the baseline it departs from, or the state it reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum End {
    Base,
    Goal,
}

impl End {
    pub fn other(self) -> Self {
        match self {
            Self::Base => Self::Goal,
            Self::Goal => Self::Base,
        }
    }

    fn at(self) -> usize {
        self as usize
    }
}

/// How a table stands to a family at one end. A member of some other parent
/// is `Absent` here: it belongs to that parent's family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    Absent,
    Standalone,
    Member,
}

/// The physical column holding a parent column in one child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Physical {
    pub key: ColumnKey,
    pub name: ColumnName,
}

/// One moving parent: its columns by UID, and its children by role.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Family {
    present: [bool; 2],
    names: BTreeMap<Uid, [Option<ColumnName>; 2]>,
    roles: BTreeMap<Recorded, [Standing; 2]>,
    /// The recorded columns of a child that stands alone at an end.
    own: BTreeMap<(Recorded, End), BTreeMap<ColumnName, Uid>>,
}

impl Family {
    /// Whether the parent table exists at `end`.
    pub fn present(&self, end: End) -> bool {
        self.present[end.at()]
    }

    /// Every parent column recorded at either end, with its key.
    pub fn columns(&self) -> impl Iterator<Item = (&Uid, ColumnKey)> {
        self.names
            .keys()
            .map(|uid| (uid, ColumnKey::Recorded(Recorded(uid.clone()))))
    }

    /// The parent column's name at `end`, when the parent holds it there.
    pub fn name(&self, column: &Uid, end: End) -> Option<&ColumnName> {
        self.names.get(column)?[end.at()].as_ref()
    }

    pub fn children(&self) -> impl Iterator<Item = &Recorded> {
        self.roles.keys()
    }

    pub fn standing(&self, child: &Recorded, end: End) -> Standing {
        self.roles
            .get(child)
            .map_or(Standing::Absent, |r| r[end.at()])
    }

    /// The namespace slot the parent column occupies in a child at `end`: the
    /// name the parent gives it there, else the name it gives it at the other
    /// end. A column the parent lacks at `end` still has a slot, which is
    /// known to be empty. A table the family never held has no slot.
    pub fn slot(&self, column: &Uid, child: &Recorded, end: End) -> Option<Physical> {
        if !self.roles.contains_key(child) {
            return None;
        }
        let name = self
            .name(column, end)
            .or_else(|| self.name(column, end.other()))?;
        Some(Physical {
            key: ColumnKey::Partition {
                table: child.clone(),
                column: name.clone(),
            },
            name: name.clone(),
        })
    }

    /// The column that carries the parent's column in `child` at `end`. A
    /// member holds the parent's name for it; a child standing alone holds
    /// its own recorded column, found under the name the parent gives it at
    /// `end`, else under the name it gives it at the other end.
    pub fn physical(&self, column: &Uid, child: &Recorded, end: End) -> Option<Physical> {
        match self.standing(child, end) {
            Standing::Absent => None,
            Standing::Member => self.name(column, end).and(self.slot(column, child, end)),
            Standing::Standalone => {
                let own = self.own.get(&(child.clone(), end))?;
                [self.name(column, end), self.name(column, end.other())]
                    .into_iter()
                    .flatten()
                    .find_map(|name| {
                        Some(Physical {
                            key: ColumnKey::Recorded(Recorded(own.get(name)?.clone())),
                            name: name.clone(),
                        })
                    })
            }
        }
    }

    /// The reverse of [`Family::physical`]: the one parent column a physical
    /// column of `child` carries at `end`. Two parent columns claiming the
    /// same physical column are no answer at all.
    pub fn parent_column(&self, key: &ColumnKey, child: &Recorded, end: End) -> Option<&Uid> {
        let mut found = self
            .names
            .keys()
            .filter(|u| self.physical(u, child, end).is_some_and(|p| &p.key == key));
        let only = found.next()?;
        found.next().is_none().then_some(only)
    }
}

/// The moving families of a plan, by parent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lineage(BTreeMap<Recorded, Family>);

impl Snapshot<'_> {
    /// Identity evidence from this snapshot, with no fallback to the other
    /// end's spelling. These keys carry no execution authority.
    pub fn table_identity(&self, name: &TableName) -> Option<Recorded> {
        self.ids.table_uid(name).cloned().map(Recorded)
    }

    pub fn column_identity(&self, column: &crate::ColumnRef) -> Option<ColumnKey> {
        let uid = self.ids.column_uid(column)?.clone();
        Some(ColumnKey::Recorded(Recorded(uid)))
    }

    /// The parent a table is a member of, by identity.
    fn parent(&self, name: &TableName) -> Option<Recorded> {
        let of = self.schema.tables.get(name)?.partition_of.as_ref()?;
        self.table_identity(&of.parent)
    }
}

impl Lineage {
    /// Every parent that gains or loses a member between the two ends. A
    /// table is followed by its UID, so a rename or `SET SCHEMA` of parent or
    /// child does not hide a move.
    pub fn of(before: Snapshot<'_>, after: Snapshot<'_>) -> Self {
        let ends = [before, after];
        let mut moving = BTreeSet::new();
        for (uid, name) in &before.ids.tables {
            let old = before.parent(name);
            let new = after.ids.tables.get(uid).and_then(|n| after.parent(n));
            if old != new {
                moving.extend(old.into_iter().chain(new));
            }
        }
        let mut families = BTreeMap::new();
        for parent in moving {
            let mut family = Family::default();
            for end in [End::Base, End::Goal] {
                let side = ends[end.at()];
                let Some(name) = side.ids.tables.get(parent.uid()) else {
                    continue;
                };
                family.present[end.at()] = true;
                let columns = side.schema.tables.get(name).map(|t| &t.columns);
                for column in columns.into_iter().flat_map(|c| c.keys()) {
                    if let Some(ColumnKey::Recorded(uid)) =
                        side.column_identity(&name.column(column))
                    {
                        family.names.entry(uid.uid().clone()).or_default()[end.at()] =
                            Some(column.clone());
                    }
                }
            }
            for end in [End::Base, End::Goal] {
                let side = ends[end.at()];
                for (uid, name) in &side.ids.tables {
                    let Some(table) = side.schema.tables.get(name) else {
                        continue;
                    };
                    let child = Recorded(uid.clone());
                    let role = if side.parent(name).as_ref() == Some(&parent) {
                        Standing::Member
                    } else if table.partition_of.is_none() && child != parent {
                        Standing::Standalone
                    } else {
                        continue;
                    };
                    family
                        .roles
                        .entry(child.clone())
                        .or_insert([Standing::Absent; 2])[end.at()] = role;
                    if role == Standing::Standalone {
                        let own = table.columns.keys().filter_map(|column| {
                            let ColumnKey::Recorded(id) =
                                side.column_identity(&name.column(column))?
                            else {
                                return None;
                            };
                            Some((column.clone(), id.uid().clone()))
                        });
                        family.own.insert((child, end), own.collect());
                    }
                }
            }
            // A standalone table is a child only where the family moved it.
            family.roles.retain(|_, r| r.contains(&Standing::Member));
            family.own.retain(|(c, _), _| family.roles.contains_key(c));
            families.insert(parent, family);
        }
        Self(families)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn parents(&self) -> impl Iterator<Item = &Recorded> {
        self.0.keys()
    }

    pub fn family(&self, parent: &Recorded) -> Option<&Family> {
        self.0.get(parent)
    }

    pub fn families(&self) -> impl Iterator<Item = (&Recorded, &Family)> {
        self.0.iter()
    }

    /// The family `child` is a member of at `end`.
    pub fn parent_of(&self, child: &Recorded, end: End) -> Option<&Recorded> {
        self.0
            .iter()
            .find_map(|(p, f)| (f.standing(child, end) == Standing::Member).then_some(p))
    }
}
