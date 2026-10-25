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

/// One step of rebuilding an enum while keeping its uid (DEC-1635.7).
///
/// The replacement exists under a pbps-chosen name until `Swap`; the columns
/// move to it one at a time, so every intermediate state is a state the
/// per-statement checks can name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "step", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnumRebuildStep {
    /// `CREATE TYPE <tmp> AS ENUM (<final labels>)`. `removed` lists, as the
    /// database spells them today, the labels the final type lacks; a row
    /// holding one is refused at plan time.
    Create {
        definition: Box<ManagedType>,
        removed: Vec<String>,
        /// After the label renames of the same uid, which map by name.
        ordinal: usize,
    },
    /// `ALTER COLUMN .. TYPE <tmp> USING .. ::text::<tmp>`: a full rewrite of
    /// the table under ACCESS EXCLUSIVE, even for a pure reorder. A
    /// partitioned table is retyped at its root only.
    Retype {
        column_uid: crate::Uid,
        column: crate::ColumnRef,
        /// The type the column has once the swap has named the replacement
        /// back: the enum, or an array of it, as the declaration spells it.
        /// The closing read holds the column to it (SPEC §7.6), since a
        /// concurrent retype between this step and the last would otherwise
        /// leave the column as that session made it.
        to: crate::ColumnType,
        array: bool,
        /// The labels the rebuild removes, as stored now: the cast refuses a
        /// row holding one, and the plan-time probe counts them.
        removed: Vec<String>,
    },
    /// `DROP TYPE <name>; ALTER TYPE <tmp> RENAME TO <name>`, then the
    /// carried `COMMENT ON TYPE`. The uid never changes.
    Swap {
        definition: Box<ManagedType>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        comment: Option<String>,
    },
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

#[cfg(test)]
mod rebuild_tests {
    use super::*;
    use crate::{Change, ColumnRef, RiskClass};

    fn steps() -> [Change; 3] {
        let uid: crate::Uid = "y_000001".parse().unwrap();
        let name: crate::TypeRef = "app.e".parse().unwrap();
        let tmp: crate::TypeRef = "app.pbps_rebuild_y_000001".parse().unwrap();
        let definition = Box::new(ManagedType::Enum {
            values: vec!["ok".into(), "sad".into()],
        });
        let step = |step| Change::EnumRebuild {
            uid: uid.clone(),
            name: name.clone(),
            tmp: tmp.clone(),
            step,
        };
        [
            step(EnumRebuildStep::Create {
                definition: definition.clone(),
                removed: vec!["gone".into()],
                ordinal: 0,
            }),
            step(EnumRebuildStep::Retype {
                column_uid: crate::Uid::derived(crate::UidKind::Column, "m", 0),
                column: ColumnRef {
                    table: "app.t".parse().unwrap(),
                    name: "m".into(),
                },
                to: crate::ColumnType::reference("app.e".parse().unwrap()),
                array: true,
                removed: vec!["gone".into()],
            }),
            step(EnumRebuildStep::Swap {
                definition,
                comment: Some("moods".into()),
            }),
        ]
    }

    #[test]
    fn every_rebuild_step_asks_for_the_type_rebuild_class_and_only_that() {
        for step in steps() {
            assert_eq!(step.intrinsic_risks(), [RiskClass::TypeRebuild].into());
        }
        assert_eq!(
            "type-rebuild".parse::<RiskClass>(),
            Ok(RiskClass::TypeRebuild)
        );
        assert!(RiskClass::TypeRebuild.is_gated());
        assert!(RiskClass::TypeRebuild.why().contains("ACCESS EXCLUSIVE"));
    }

    #[test]
    fn only_a_retype_step_belongs_to_a_table_and_a_column() {
        let [create, retype, swap] = steps();
        for step in [&create, &swap] {
            assert!(step.table().is_none(), "{step:?}");
            assert!(step.columns().is_empty());
            assert!(step.columns_redefined().is_empty());
            assert_eq!(step.subject(), "app.e");
        }
        assert_eq!(retype.table().unwrap().shown().to_string(), "app.t");
        assert_eq!(retype.columns().len(), 1);
        // The column holds the replacement until the swap, so no read between
        // can promise its type.
        assert_eq!(
            retype.columns_redefined(),
            vec![(retype.columns()[0].clone(), crate::ColumnField::Type)]
        );
        assert_eq!(retype.subject(), "app.e");
    }

    #[test]
    fn rebuild_steps_round_trip_and_refuse_unknown_fields() {
        for step in steps() {
            let text = serde_json::to_string(&step).unwrap();
            assert_eq!(serde_json::from_str::<Change>(&text).unwrap(), step);
        }
        let text = serde_json::to_string(&steps()[2]).unwrap();
        let tampered = text.replace("\"comment\"", "\"commentary\"");
        assert!(serde_json::from_str::<Change>(&tampered).is_err());
        // A swap without a comment writes no field at all.
        let mut bare = steps()[2].clone();
        if let Change::EnumRebuild {
            step: EnumRebuildStep::Swap { comment, .. },
            ..
        } = &mut bare
        {
            *comment = None;
        }
        assert!(!serde_json::to_string(&bare).unwrap().contains("comment"));
    }
}
