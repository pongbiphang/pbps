//! Declared shared types (DEC-1635.1). Names belong to `Schema::types`.

use std::collections::BTreeMap;

use crate::{CollationName, ColumnType, ObjectLocalName};

/// The definition, without catalog identity, ownership or one-shot intent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedType {
    Enum {
        values: Vec<String>,
    },
    Domain {
        /// Uses the same spelling and normalization as a column type.
        base: ColumnType,
        not_null: bool,
        default: Option<String>,
        checks: BTreeMap<ObjectLocalName, String>,
        /// PostgreSQL collation identities are case-sensitive qualified names.
        collation: Option<CollationName>,
    },
}

impl ManagedType {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Enum { .. } => "enum",
            Self::Domain { .. } => "domain",
        }
    }

    /// Attribute names for the slice-one refusal; never executable changes.
    pub fn differences(&self, wanted: &Self) -> Vec<&'static str> {
        match (self, wanted) {
            (Self::Enum { values: a }, Self::Enum { values: b }) => {
                if a == b {
                    vec![]
                } else {
                    vec!["enum values (addition, removal, order or rename)"]
                }
            }
            (
                Self::Domain {
                    base: a,
                    not_null: an,
                    default: ad,
                    checks: ac,
                    collation: al,
                },
                Self::Domain {
                    base: b,
                    not_null: bn,
                    default: bd,
                    checks: bc,
                    collation: bl,
                },
            ) => [
                (a != b, "domain base type"),
                (an != bn, "domain NOT NULL"),
                (ad != bd, "domain default"),
                (ac != bc, "domain checks"),
                (al != bl, "domain collation"),
            ]
            .into_iter()
            .filter_map(|(changed, field)| changed.then_some(field))
            .collect(),
            _ => vec!["type kind"],
        }
    }
}
