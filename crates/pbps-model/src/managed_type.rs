//! Declared shared types (DEC-1635.1). Names belong to `Schema::types`.

use schemars::JsonSchema;
use std::collections::BTreeMap;

use crate::{CollationName, ColumnType, ObjectLocalName};

/// The definition, without catalog identity, ownership or one-shot intent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
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

/// Which half of an in-place domain alteration a change is (DEC-1635.8). A
/// check is dropped before the plan's rows and added after them, as a table's
/// is, so the two directions are two changes.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DomainHalf {
    /// Drop the checks the declaration removed or changed and NOT NULL it no
    /// longer holds, and set or drop the default.
    Loosen,
    /// Add the checks it added or changed, validated, and set NOT NULL.
    Tighten,
}

/// What changes between two definitions of one domain, split as the two
/// halves of an in-place alteration take it (DEC-1635.8). The differ, the
/// emitter and the risk gate read it from here, so none of them has a second
/// idea of which check is "changed". A check whose text differs is dropped and
/// added again under its name, exactly as a table's is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainDelta<'a> {
    /// Checks the declaration removed or changed, dropped by `Loosen`.
    pub drop_checks: Vec<&'a ObjectLocalName>,
    /// Checks it added or changed, added and validated by `Tighten`.
    pub add_checks: Vec<(&'a ObjectLocalName, &'a str)>,
    /// `Loosen` drops NOT NULL.
    pub drop_not_null: bool,
    /// `Tighten` sets NOT NULL.
    pub set_not_null: bool,
    /// The default, when it changed: `Some(None)` drops it.
    pub default: Option<Option<&'a str>>,
}

impl<'a> DomainDelta<'a> {
    /// `None` unless both are domains.
    pub fn of(from: &'a ManagedType, to: &'a ManagedType) -> Option<Self> {
        let (
            ManagedType::Domain {
                not_null: was_not_null,
                default: was_default,
                checks: was_checks,
                ..
            },
            ManagedType::Domain {
                not_null,
                default,
                checks,
                ..
            },
        ) = (from, to)
        else {
            return None;
        };
        Some(Self {
            drop_checks: was_checks
                .iter()
                .filter(|(name, text)| checks.get(*name) != Some(*text))
                .map(|(name, _)| name)
                .collect(),
            add_checks: checks
                .iter()
                .filter(|(name, text)| was_checks.get(*name) != Some(*text))
                .map(|(name, text)| (name, text.as_str()))
                .collect(),
            drop_not_null: *was_not_null && !*not_null,
            set_not_null: !*was_not_null && *not_null,
            default: (was_default != default).then_some(default.as_deref()),
        })
    }

    /// The domain once `Loosen` has run: what `from` keeps of what `to` still
    /// holds, with `to`'s default. The re-read after that half is held to it.
    pub fn loosened(from: &ManagedType, to: &ManagedType) -> Option<ManagedType> {
        let (
            ManagedType::Domain {
                base,
                not_null: was_not_null,
                checks: was_checks,
                collation,
                ..
            },
            ManagedType::Domain {
                not_null,
                default,
                checks,
                ..
            },
        ) = (from, to)
        else {
            return None;
        };
        Some(ManagedType::Domain {
            base: base.clone(),
            not_null: *was_not_null && *not_null,
            default: default.clone(),
            checks: was_checks
                .iter()
                .filter(|(name, text)| checks.get(*name) == Some(*text))
                .map(|(name, text)| (name.clone(), text.clone()))
                .collect(),
            collation: collation.clone(),
        })
    }

    /// Whether the `Loosen` half has a statement.
    pub fn loosens(&self) -> bool {
        !self.drop_checks.is_empty() || self.drop_not_null || self.default.is_some()
    }

    /// Whether the `Tighten` half has a statement.
    pub fn tightens(&self) -> bool {
        !self.add_checks.is_empty() || self.set_not_null
    }
}

/// The neighbour already exists at this operation's position (DEC-1635.6).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
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

/// One step of rebuilding an enum or a domain while keeping its uid
/// (DEC-1635.7, DEC-1635.9).
///
/// The replacement exists under a pbps-chosen name until `Swap`; the columns
/// move to it one at a time, so every intermediate state is a state the
/// per-statement checks can name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(tag = "step", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnumRebuildStep {
    /// `CREATE TYPE <tmp> AS ENUM (<final labels>)`, or `CREATE DOMAIN <tmp>
    /// AS ..` from the final declaration of a domain. `removed` lists, as the
    /// database spells them today, the labels the final type lacks; a row
    /// holding one is refused at plan time.
    Create {
        definition: Box<ManagedType>,
        removed: Vec<String>,
        /// After the label renames of the same uid, which map by name. A
        /// domain over a rebuilt type is created after it.
        ordinal: usize,
        /// A domain whose base is itself rebuilt is created over that base's
        /// replacement, since the old base is dropped with the swap; this
        /// names it. The declaration's own base is the final one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        over: Option<crate::TypeRef>,
        /// Why a domain is rebuilt, in a sentence for the plan's reader: its
        /// base or collation changed, the type it is built on is rebuilt, or
        /// an array of it blocks an in-place check. Never read back.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        why: Option<String>,
    },
    /// `ALTER COLUMN .. TYPE <tmp>`, with `USING .. ::text::<tmp>` unless the
    /// conversion is the engine's own assignment cast: a full rewrite of the
    /// table under ACCESS EXCLUSIVE, even for a pure reorder. A partitioned
    /// table is retyped at its root only.
    Retype {
        column_uid: crate::Uid,
        column: crate::ColumnRef,
        /// The type the column has once the swap has named the replacement
        /// back: the enum or domain, or an array of it, as the declaration
        /// spells it. The closing read holds the column to it (SPEC §7.6),
        /// since a concurrent retype between this step and the last would
        /// otherwise leave the column as that session made it.
        to: crate::ColumnType,
        array: bool,
        /// The single judgement input shared with a direct column retype.
        values: Box<crate::ValueChange>,
    },
    /// `DROP TYPE <name>; ALTER TYPE <tmp> RENAME TO <name>` (`DOMAIN` for a
    /// domain), then the carried `COMMENT ON`. The uid never changes.
    Swap {
        definition: Box<ManagedType>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        comment: Option<String>,
        /// How many rebuilt types this one is built on: a domain over a
        /// rebuilt enum is swapped before it, so the old base can be dropped.
        #[serde(default, skip_serializing_if = "is_zero")]
        depth: usize,
        /// The replacement a domain's replacement is built over, as in
        /// `Create`: until the base is swapped too, the domain stands over it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        over: Option<crate::TypeRef>,
    },
}

fn is_zero(n: &usize) -> bool {
    *n == 0
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
            retained: Vec::new(),
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
mod domain_tests {
    use super::*;
    use crate::{Change, RiskClass};

    fn domain(not_null: bool, default: Option<&str>, checks: &[(&str, &str)]) -> ManagedType {
        ManagedType::Domain {
            base: "text".parse().unwrap(),
            not_null,
            default: default.map(str::to_owned),
            checks: checks
                .iter()
                .map(|(name, text)| ((*name).into(), (*text).to_owned()))
                .collect(),
            collation: None,
        }
    }

    #[test]
    fn a_delta_splits_the_difference_between_two_domains_into_the_two_halves() {
        let from = domain(
            true,
            Some("'a'"),
            &[("keep", "1"), ("gone", "2"), ("edit", "3")],
        );
        let to = domain(false, None, &[("keep", "1"), ("edit", "4"), ("new", "5")]);
        let delta = DomainDelta::of(&from, &to).unwrap();
        let names = |checks: &[&ObjectLocalName]| -> Vec<String> {
            checks.iter().map(|c| c.raw().to_owned()).collect()
        };
        assert_eq!(names(&delta.drop_checks), ["edit", "gone"]);
        assert_eq!(
            delta
                .add_checks
                .iter()
                .map(|(n, t)| (n.raw().to_owned(), *t))
                .collect::<Vec<_>>(),
            [("edit".to_owned(), "4"), ("new".to_owned(), "5")]
        );
        assert!(delta.drop_not_null && !delta.set_not_null);
        assert_eq!(delta.default, Some(None));
        assert!(delta.loosens() && delta.tightens());
        // What the loosening half leaves.
        assert_eq!(
            DomainDelta::loosened(&from, &to),
            Some(domain(false, None, &[("keep", "1")]))
        );
        // Negatives: equal domains change nothing; an enum has no delta.
        let same = DomainDelta::of(&from, &from).unwrap();
        assert!(!same.loosens() && !same.tightens() && same.default.is_none());
        assert!(DomainDelta::of(&from, &ManagedType::Enum { values: vec![] }).is_none());
        assert!(DomainDelta::loosened(&ManagedType::Enum { values: vec![] }, &to).is_none());
    }

    /// DEC-1635.8: the loosening carries no class, as `DropCheck` does
    /// (DECISIONS 486); the tightening is `constraint` for a check added or
    /// changed and `not-null` for NOT NULL, each only when it has one.
    #[test]
    fn the_halves_carry_the_risk_classes_of_the_statements_they_hold() {
        let alter = |from: ManagedType, to: ManagedType, half| Change::AlterDomain {
            uid: "y_000001".parse().unwrap(),
            name: "app.code".parse().unwrap(),
            from: Box::new(from),
            to: Box::new(to),
            half,
        };
        let plain = domain(false, None, &[]);
        let checked = domain(false, None, &[("c", "VALUE <> ''")]);
        let required = domain(true, None, &[]);
        let both = domain(true, None, &[("c", "VALUE <> ''")]);
        assert!(
            alter(both.clone(), plain.clone(), DomainHalf::Loosen)
                .intrinsic_risks()
                .is_empty()
        );
        assert!(
            alter(plain.clone(), both.clone(), DomainHalf::Loosen)
                .intrinsic_risks()
                .is_empty()
        );
        for (to, expected) in [
            (checked.clone(), vec![RiskClass::Constraint]),
            (required.clone(), vec![RiskClass::NotNull]),
            (
                both.clone(),
                vec![RiskClass::Constraint, RiskClass::NotNull],
            ),
        ] {
            let change = alter(plain.clone(), to, DomainHalf::Tighten);
            assert_eq!(
                change.intrinsic_risks(),
                expected
                    .into_iter()
                    .collect::<std::collections::BTreeSet<_>>()
            );
        }
        // A tightening half with neither (it cannot be planned) is empty.
        assert!(
            alter(plain.clone(), plain, DomainHalf::Tighten)
                .intrinsic_risks()
                .is_empty()
        );
    }

    #[test]
    fn a_domain_alteration_round_trips_and_names_its_type_and_no_table() {
        for half in [DomainHalf::Loosen, DomainHalf::Tighten] {
            let change = Change::AlterDomain {
                uid: "y_000001".parse().unwrap(),
                name: "app.code".parse().unwrap(),
                from: Box::new(domain(false, None, &[])),
                to: Box::new(domain(true, Some("'a'"), &[("c", "VALUE <> ''")])),
                half,
            };
            let json = serde_json::to_value(&change).unwrap();
            assert_eq!(json["op"], "alter_domain");
            assert_eq!(
                json["half"],
                if half == DomainHalf::Loosen {
                    "loosen"
                } else {
                    "tighten"
                }
            );
            assert_eq!(serde_json::from_value::<Change>(json).unwrap(), change);
            assert!(change.table().is_none() && change.columns().is_empty());
            assert_eq!(change.subject(), "app.code");
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
                over: None,
                why: None,
            }),
            step(EnumRebuildStep::Retype {
                column_uid: crate::Uid::derived(crate::UidKind::Column, "m", 0),
                column: ColumnRef {
                    table: "app.t".parse().unwrap(),
                    name: "m".into(),
                },
                to: crate::ColumnType::reference("app.e".parse().unwrap()),
                array: true,
                values: Box::new(crate::ValueChange::Relabel {
                    removed: vec!["gone".into()],
                }),
            }),
            step(EnumRebuildStep::Swap {
                definition,
                comment: Some("moods".into()),
                depth: 0,
                over: None,
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

    /// Plan format 56 carries one required value change, with no legacy fields.
    #[test]
    fn rebuild_value_changes_round_trip_and_reject_legacy_fields() {
        let [_, retype, swap] = steps();
        let text = serde_json::to_string(&retype).unwrap();
        assert!(text.contains("\"values\":{\"relabel\""), "{text}");
        for field in ["conversion", "base_change", "value_arrays"] {
            assert!(!text.contains(field), "{text}");
        }
        let mut domain = retype.clone();
        if let Change::EnumRebuild {
            step: EnumRebuildStep::Retype { values, .. },
            ..
        } = &mut domain
        {
            **values = crate::ValueChange::Cast {
                from: "varchar(20)".parse().unwrap(),
                to: "varchar(10)".parse().unwrap(),
                arrays: 2,
                using: None,
            };
        }
        let text = serde_json::to_string(&domain).unwrap();
        assert!(
            text.contains("\"cast\"") && text.contains("\"arrays\":2"),
            "{text}"
        );
        assert_eq!(serde_json::from_str::<Change>(&text).unwrap(), domain);
        // This is the unreleased v56 shape: missing judgement and legacy
        // independent fields must not silently default to different semantics.
        let mut missing = serde_json::to_value(&domain).unwrap();
        missing["step"].as_object_mut().unwrap().remove("values");
        assert!(serde_json::from_value::<Change>(missing).is_err());
        for field in ["conversion", "base_change", "value_arrays", "removed"] {
            let mut legacy = serde_json::to_value(&domain).unwrap();
            legacy["step"]
                .as_object_mut()
                .unwrap()
                .insert(field.into(), serde_json::Value::Null);
            assert!(serde_json::from_value::<Change>(legacy).is_err(), "{field}");
        }

        let mut over = swap.clone();
        if let Change::EnumRebuild {
            step: EnumRebuildStep::Swap { over, depth, .. },
            ..
        } = &mut over
        {
            *over = Some("app.pbps_rebuild_y_000009".parse().unwrap());
            *depth = 2;
        }
        let text = serde_json::to_string(&over).unwrap();
        assert!(
            text.contains("\"over\"") && text.contains("\"depth\":2"),
            "{text}"
        );
        assert_eq!(serde_json::from_str::<Change>(&text).unwrap(), over);
        assert!(!serde_json::to_string(&swap).unwrap().contains("over"));
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
