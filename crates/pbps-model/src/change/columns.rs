//! Portable statement outputs, without engine recursion or normalization policy
//! (DEC-2296.4).
use super::Change;
use crate::operation::symbolic::{Attribute, Lifetime};
use crate::{Collation, Column, ColumnName, ColumnType, TableName, Uid, ValueSource};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnProperty {
    Column(Attribute),
    Address,
}

/// Borrow the statement's actual output, rather than the desired endpoint:
/// a TYPE rewrite must not acquire a later DEFAULT restoration's bindings.
#[derive(Debug, Clone, Copy)]
pub enum ColumnValue<'a> {
    Absent,
    Definition(&'a Column),
    Name(&'a ColumnName),
    Type(&'a ColumnType),
    Collation(&'a Option<Collation>),
    Nullable(bool),
    Default(Option<&'a str>),
    Numbering(&'a ValueSource),
    Address(&'a TableName),
}

#[derive(Debug, Clone)]
pub struct ColumnTransition<'a> {
    pub property: ColumnProperty,
    pub lifetime: Lifetime,
    pub value: ColumnValue<'a>,
}

impl Change {
    /// The seven column changes that can meet a parent's membership change.
    /// Partition-local restorations and deprecation metadata are separate.
    #[allow(clippy::wildcard_enum_match_arm)]
    pub fn parent_column_uid(&self) -> Option<&Uid> {
        match self {
            Self::AddColumn { uid, .. }
            | Self::DropColumn { uid, .. }
            | Self::RenameColumn { uid, .. }
            | Self::AlterColumnType { uid, .. }
            | Self::AlterColumnNullability { uid, .. }
            | Self::AlterColumnDefault { uid, .. }
            | Self::AlterColumnNumbering { uid, .. } => Some(uid),
            _ => None,
        }
    }

    /// Property events made by this statement. Numbering has its own lifetime;
    /// retuning an existing sequence emits no column-default event. Engines
    /// choose recursion and ownership policy after normalizing these values.
    #[allow(clippy::wildcard_enum_match_arm)]
    pub fn column_transitions(&self) -> Vec<ColumnTransition<'_>> {
        use Attribute::*;
        use ColumnValue as V;
        use Lifetime::*;
        let mut result = Vec::new();
        let mut put = |attribute, lifetime, value| {
            result.push(ColumnTransition {
                property: ColumnProperty::Column(attribute),
                lifetime,
                value,
            });
        };
        match self {
            Self::AddColumn { .. } | Self::DropColumn { .. } => {
                let (lifetime, value) = if let Self::AddColumn { column, .. } = self {
                    (Create, V::Definition(column))
                } else {
                    (Retire, V::Absent)
                };
                for attribute in [Presence, Name, Type, Collation, Default, NotNull] {
                    put(attribute, lifetime, value);
                }
                if matches!(self, Self::DropColumn { .. })
                    || matches!(self, Self::AddColumn { column, .. } if matches!(column.value_source, ValueSource::Sequence(_) | ValueSource::Identity(_)))
                {
                    put(Numbering, lifetime, value);
                }
            }
            Self::RenameColumn { to, .. } => put(Name, Retain, V::Name(to)),
            Self::AlterColumnType {
                from,
                to,
                from_collation,
                to_collation,
                from_nullable,
                to_nullable,
                ..
            } => {
                if from != to {
                    put(Type, Retain, V::Type(to));
                }
                if from_collation != to_collation {
                    put(Collation, Retain, V::Collation(to_collation));
                }
                if from_nullable != to_nullable {
                    put(NotNull, Retain, V::Nullable(*to_nullable));
                }
            }
            Self::AlterColumnNullability { to_nullable, .. } => {
                put(NotNull, Retain, V::Nullable(*to_nullable))
            }
            Self::AlterColumnDefault { to, .. } => put(Default, Retain, V::Default(to.as_deref())),
            Self::AlterColumnNumbering { from, to, .. } => {
                let owns = |s: &ValueSource| {
                    matches!(s, ValueSource::Sequence(_) | ValueSource::Identity(_))
                };
                if !matches!(
                    (from, to),
                    (ValueSource::Sequence(_), ValueSource::Sequence(_))
                ) {
                    put(Default, Retain, V::Numbering(to));
                }
                put(
                    Numbering,
                    if owns(from) && owns(to) {
                        Retain
                    } else if owns(to) {
                        Create
                    } else {
                        Retire
                    },
                    V::Numbering(to),
                );
            }
            Self::RenameTable { to, .. } => result.push(ColumnTransition {
                property: ColumnProperty::Address,
                lifetime: Retain,
                value: V::Address(to),
            }),
            _ => {}
        }
        result
    }
}

#[cfg(test)]
mod tests;
