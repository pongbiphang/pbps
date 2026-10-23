//! Symbolic effects keep recursion scoped to membership at execution time
//! (DEC-2296.1). These facts do not carry SQL or execution authority.
//!
//! ```compile_fail
//! fn wire<T: serde::Serialize>() {}
//! wire::<pbps_model::operation::symbolic::Clause>();
//! ```

use super::{BoundInput, ColumnKey, ObjectKey, OperationId, PartKey, Recorded};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Attribute {
    Presence,
    Name,
    Type,
    Collation,
    Default,
    Numbering,
    NotNull,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Property {
    Column(ColumnKey, Attribute),
    Address,
    Part(PartKey),
    Rows,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    pub parent: Recorded,
    pub property: Property,
}

/// An event identity survives equal-valued releases and restorations. It is
/// separate from the two object incarnations, and never part of wire bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Version {
    Base,
    After(OperationId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fact<T> {
    Known(T),
    Unread,
    Unsupported {
        sqlstate: Option<String>,
        reason: String,
    },
}
impl<T> Fact<T> {
    pub fn known(&self) -> Result<&T, String> {
        match self {
            Self::Known(value) => Ok(value),
            Self::Unread => Err("the required catalog fact was not read".into()),
            Self::Unsupported { sqlstate, reason } => {
                Err(format!("unsupported capability ({sqlstate:?}): {reason}"))
            }
        }
    }
}

/// Tokens are normalized definitions supplied by an engine, not SQL parsed by
/// the model. Binding identity and local ownership remain separate facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Datum {
    Absent,
    Definition(String),
    NotNull(bool),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    Own,
    Inherited,
    LocalNotNull,
    Clone { parent: Recorded, part: PartKey },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Value {
    pub datum: Datum,
    pub bindings: BTreeSet<BoundInput>,
    pub ownership: Ownership,
}
impl Value {
    pub fn definition_eq(&self, other: &Self) -> bool {
        self.datum == other.datum && self.bindings == other.bindings
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    ParentOnly,
    MembersAtOperation,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritePolicy {
    Overwrite,
    PreserveOwn,
    StrengthenNotNull,
    PreserveOwnership,
}
impl WritePolicy {
    pub fn apply(self, previous: &Value, next: &Value) -> Value {
        match self {
            Self::PreserveOwn
                if matches!(previous.ownership, Ownership::Own | Ownership::LocalNotNull) =>
            {
                previous.clone()
            }
            Self::StrengthenNotNull if previous.datum == Datum::NotNull(true) => previous.clone(),
            Self::PreserveOwnership => Value {
                ownership: previous.ownership.clone(),
                ..next.clone()
            },
            Self::Overwrite | Self::PreserveOwn | Self::StrengthenNotNull => Value {
                ownership: Ownership::Inherited,
                ..next.clone()
            },
        }
    }
}
/// Lifetime refers to the physical object, independently of changing an
/// expression/default on a column that continues to stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifetime {
    Retain,
    Create,
    Retire,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecursiveWrite {
    pub lifetime: Lifetime,
    pub key: Key,
    pub input: Version,
    pub from: Fact<Value>,
    pub to: Fact<Value>,
    pub scope: Scope,
    pub policy: WritePolicy,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeCheck {
    None,
    SameDefinition,
    NotNullAtLeast,
}
impl ShapeCheck {
    pub fn accepts(self, parent: &Value, child: &Value) -> bool {
        match self {
            Self::None => true,
            Self::SameDefinition => parent.definition_eq(child),
            Self::NotNullAtLeast => matches!(
                (&parent.datum, &child.datum),
                (Datum::NotNull(false), Datum::NotNull(_))
                    | (Datum::NotNull(true), Datum::NotNull(true))
            ),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferPolicy {
    Preserve,
    Inherit,
    Adopt(PartKey),
}
impl TransferPolicy {
    pub fn attach(&self, parent: &Recorded, inherited: &Value, previous: &Value) -> Value {
        match self {
            Self::Preserve => previous.clone(),
            Self::Inherit => Value {
                ownership: Ownership::Inherited,
                ..inherited.clone()
            },
            Self::Adopt(part) => Value {
                ownership: Ownership::Clone {
                    parent: parent.clone(),
                    part: part.clone(),
                },
                ..inherited.clone()
            },
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    pub parent: Property,
    pub from: Property,
    pub to: Property,
    pub check: ShapeCheck,
    pub policy: TransferPolicy,
    pub goal: Fact<Value>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Boundary {
    All(Vec<Transfer>),
    /// An explicit disjunction cannot silently become a conjunction of edges.
    Alternatives(Vec<Vec<Transfer>>),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveKind {
    Attach,
    Detach,
    Drop,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    pub parent: Recorded,
    pub child: Recorded,
    pub kind: MoveKind,
    pub consumes: Option<Version>,
    pub boundary: Boundary,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restoration {
    pub key: Key,
    pub child: Recorded,
    pub property: Property,
    pub value: Fact<Value>,
    /// Some(parent) for partition-form operations; None for standalone ones.
    pub member_of: Option<Recorded>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clause {
    Recurse(RecursiveWrite),
    Move(Move),
    Restore(Restoration),
    Require(Fact<()>),
}

/// A managed-scope transfer never changes the physical birth of the cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalOrigin {
    pub table: Recorded,
    pub object: ObjectKey,
    pub birth: Version,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub value: Fact<Value>,
    pub version: Version,
    /// None is a known absent/retired physical source, not an unread fact.
    pub origin: Option<PhysicalOrigin>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    pub parent: Recorded,
    pub incarnation: Version,
    /// The parent's property maps to the child's current physical slot.
    pub aliases: BTreeMap<Property, Property>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    pub tables: BTreeSet<Recorded>,
    pub cells: BTreeMap<(Recorded, Property), Cell>,
    pub members: BTreeMap<Recorded, Membership>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn symbolic_obligations_are_merged_without_becoming_empty_or_duplicated() {
        let clause = Clause::Require(Fact::Unread);
        let fx = super::super::Effects {
            symbolic: vec![clause.clone()],
            ..Default::default()
        };
        assert!(!fx.is_empty());
        assert_eq!(
            super::super::Effects::default()
                .merged(fx.clone())
                .symbolic
                .as_slice(),
            std::slice::from_ref(&clause)
        );
        assert_eq!(fx.clone().merged(fx).symbolic, [clause]);
        assert!(super::super::Effects::default().is_empty());
    }
    #[test]
    fn unknown_capability_and_known_empty_are_different_facts() {
        assert_eq!(Fact::Known(Vec::<Value>::new()).known().unwrap().len(), 0);
        assert!(Fact::<Vec<Value>>::Unread.known().is_err());
        assert!(
            Fact::<()>::Unsupported {
                sqlstate: Some("55000".into()),
                reason: "own identity".into()
            }
            .known()
            .unwrap_err()
            .contains("55000")
        );
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    #[test]
    fn strengthening_cannot_erase_a_local_not_null() {
        let old = Value {
            datum: Datum::NotNull(true),
            bindings: BTreeSet::new(),
            ownership: Ownership::LocalNotNull,
        };
        let relaxed = Value {
            datum: Datum::NotNull(false),
            ownership: Ownership::Inherited,
            ..old.clone()
        };
        assert_eq!(WritePolicy::StrengthenNotNull.apply(&old, &relaxed), old);
        let tightened = Value {
            datum: Datum::NotNull(true),
            ..relaxed.clone()
        };
        assert_eq!(
            WritePolicy::StrengthenNotNull.apply(&relaxed, &tightened),
            tightened
        );
    }
}
