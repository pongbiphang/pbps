//! Stored-value semantics shared by column retypes and type rebuilds (#2461).

use crate::{Change, ColumnType, EnumRebuildStep};

/// What stored values undergo, independently of the statements that move them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ValueChange {
    Cast {
        /// Leaf types, after walking every domain and array link.
        from: ColumnType,
        to: ColumnType,
        /// Array levels through the domains and the column's own `[]`.
        arrays: usize,
        using: Option<String>,
    },
    /// A replaced enum keeps labels by name and refuses removed ones.
    Relabel { removed: Vec<String> },
}

impl ValueChange {
    /// A direct column's cast. Scalar/array crossings have no common shape;
    /// their existing column-specific refusal or recorded conversion remains
    /// responsible for them rather than losing that distinction in leaf types.
    pub fn cast(from: &ColumnType, to: &ColumnType, using: Option<String>) -> Option<Self> {
        (from.is_array() == to.is_array()).then(|| Self::Cast {
            from: from.element(),
            to: to.element(),
            arrays: usize::from(to.is_array()),
            using,
        })
    }
}

impl Change {
    /// The shared judgement input; shape-changing column conversions retain
    /// their full types at their existing callers (see [`ValueChange::cast`]).
    #[allow(clippy::wildcard_enum_match_arm)] // Only column retypes expose this optional judgement.
    pub fn value_change(&self) -> Option<ValueChange> {
        match self {
            Self::AlterColumnType {
                from, to, using, ..
            } => ValueChange::cast(from, to, using.clone()),
            Self::EnumRebuild {
                step: EnumRebuildStep::Retype { values, .. },
                ..
            } => Some((**values).clone()),
            _ => None,
        }
    }
}
