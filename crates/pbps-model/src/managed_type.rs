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

    /// Changed attributes; the differ decides which differences are executable.
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

/// The neighbour already exists at this operation's position (DEC-1635.6).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "position",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum EnumPlacement {
    Before(String),
    After(String),
}

#[cfg(test)]
mod label_tests {
    use super::*;
    use crate::{Change, RiskClass};

    #[test]
    fn additions_require_no_risk_approval_and_label_renames_require_only_rename() {
        let uid = "y_000001".parse().unwrap();
        let name = "app.e".parse().unwrap();
        let definition = Box::new(ManagedType::Enum {
            values: vec!["blue".into()],
        });
        let addition = Change::AddEnumValue {
            uid,
            name,
            value: "blue".into(),
            placement: None,
            ordinal: 0,
            definition,
        };
        let Change::AddEnumValue {
            uid,
            name,
            definition,
            ..
        } = addition.clone()
        else {
            unreachable!()
        };
        let rename = Change::RenameEnumValue {
            uid,
            name,
            from: "sad".into(),
            to: "blue".into(),
            ordinal: 0,
            definition,
        };
        assert!(addition.intrinsic_risks().is_empty());
        assert_eq!(rename.intrinsic_risks(), [RiskClass::Rename].into());
        for step in [addition, rename] {
            assert!(step.table().is_none());
            assert!(step.columns().is_empty());
            assert_eq!(step.subject(), "app.e");
            assert_eq!(
                serde_json::from_str::<Change>(&serde_json::to_string(&step).unwrap()).unwrap(),
                step
            );
        }
    }
}
