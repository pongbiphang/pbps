//! PostgreSQL's measured membership policy, as portable planning data.
//! No catalog I/O or tentative ordering belongs in these rules (DEC-2296.2).
mod columns;
pub(crate) mod facts;
mod movement;
pub(crate) mod operations;
pub use operations::of as symbolic;
#[cfg(test)]
mod scenes_test;
use pbps_model::operation::symbolic::{
    Datum, Fact, Merge, MergeCase, Ownership, TransferPolicy, Value,
};

/// Construct the measured transfer over all supplied property-version facts.
/// PG16 has effective column nullability and stronger own NOT NULL; PG18 also
/// records a local constraint, which ATTACH merges when its parent is NOT NULL.
/// The supplied values retain expression/binding identity; no SQL is parsed.
pub fn not_null_merge(
    server_version: Option<i64>,
    parents: &[Value],
    candidates: &[Value],
) -> Result<TransferPolicy, String> {
    let version =
        server_version.ok_or("the PostgreSQL version for the boundary merge was not read")?;
    if version < 160000 {
        return Err("the PostgreSQL NOT NULL boundary policy requires version 16 or later".into());
    }
    for (label, values) in [("parent", parents), ("candidate", candidates)] {
        if values
            .iter()
            .any(|v| !matches!(v.datum, Datum::Absent | Datum::NotNull(_)))
        {
            return Err(format!("the {label} NOT NULL boundary fact is unavailable"));
        }
    }
    Ok(merge(parents, candidates, |parent, candidate| {
        match (&parent.datum, &candidate.datum) {
            (Datum::Absent, Datum::Absent) => Fact::Known(Value {
                ownership: Ownership::Inherited,
                ..candidate.clone()
            }),
            (Datum::NotNull(p), Datum::NotNull(c)) if !p || *c => Fact::Known(Value {
                ownership: if *c && !p {
                    if version >= 180000 {
                        Ownership::LocalNotNull
                    } else {
                        Ownership::Own
                    }
                } else {
                    Ownership::Inherited
                },
                ..candidate.clone()
            }),
            _ => Fact::Unsupported {
                sqlstate: Some("42804".into()),
                reason: "the candidate's column presence or NOT NULL does not match its parent"
                    .into(),
            },
        }
    }))
}

fn merge(
    parents: &[Value],
    candidates: &[Value],
    result: impl Fn(&Value, &Value) -> Fact<Value>,
) -> TransferPolicy {
    let mut cases = Vec::new();
    for parent in parents {
        for candidate in candidates {
            let row = MergeCase {
                parent: parent.clone(),
                candidate: candidate.clone(),
                result: result(parent, candidate),
            };
            if !cases.contains(&row) {
                cases.push(row);
            }
        }
    }
    TransferPolicy::Merge(Merge { cases })
}

/// Attachment preserves the candidate's actual expression/bindings. Equality
/// under the engine's normalized facts determines inherited ownership; source
/// text or a future parent spelling cannot establish that equality.
pub fn default_merge(parents: &[Value], candidates: &[Value]) -> TransferPolicy {
    merge(parents, candidates, |parent, candidate| {
        Fact::Known(Value {
            ownership: if candidate.definition_eq(parent) {
                Ownership::Inherited
            } else {
                Ownership::Own
            },
            ..candidate.clone()
        })
    })
}

/// One column property's ATTACH policy. Numbering here is owned sequence
/// ownership: it stays the candidate's, while its sequence-backed DEFAULT is
/// transferred independently. Identity/readback capabilities are qualified by
/// the operation mapper before this policy can be used.
pub fn attach_policy(
    attribute: &pbps_model::operation::symbolic::Attribute,
    version: Option<i64>,
    parents: &[Value],
    candidates: &[Value],
) -> Result<(pbps_model::operation::symbolic::ShapeCheck, TransferPolicy), String> {
    use pbps_model::operation::symbolic::{Attribute::*, ShapeCheck};
    Ok(match attribute {
        Presence | Name | Type | Collation => (ShapeCheck::SameDefinition, TransferPolicy::Inherit),
        Default => (ShapeCheck::None, default_merge(parents, candidates)),
        NotNull => (
            ShapeCheck::NotNullAtLeast,
            not_null_merge(version, parents, candidates)?,
        ),
        Numbering => (ShapeCheck::None, TransferPolicy::Preserve),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    fn nn(value: bool, ownership: Ownership) -> Value {
        Value {
            datum: Datum::NotNull(value),
            bindings: BTreeSet::new(),
            ownership,
        }
    }
    #[test]
    fn pg18_attach_merges_local_not_null_at_the_read_parent_boundary() {
        let parents = [nn(false, Ownership::Own), nn(true, Ownership::Own)];
        let candidates = [nn(false, Ownership::Own), nn(true, Ownership::LocalNotNull)];
        let TransferPolicy::Merge(merge) =
            not_null_merge(Some(180000), &parents, &candidates).unwrap()
        else {
            panic!()
        };
        for p in &parents {
            for c in &candidates {
                let actual = merge.apply(p, c);
                match (&p.datum, &c.datum) {
                    (Datum::NotNull(true), Datum::NotNull(false)) => {
                        assert!(actual.unwrap_err().contains("42804"))
                    }
                    (Datum::NotNull(false), Datum::NotNull(true)) => {
                        assert_eq!(actual.unwrap(), nn(true, Ownership::LocalNotNull))
                    }
                    (Datum::NotNull(_), Datum::NotNull(value)) => {
                        assert_eq!(actual.unwrap(), nn(*value, Ownership::Inherited))
                    }
                    _ => unreachable!(),
                }
            }
        }
    }
    #[test]
    fn pg16_stronger_not_null_does_not_claim_a_pg18_local_constraint() {
        let parents = [nn(false, Ownership::Own), nn(true, Ownership::Own)];
        let candidates = [nn(true, Ownership::Own)];
        let TransferPolicy::Merge(merge) =
            not_null_merge(Some(160000), &parents, &candidates).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            merge.apply(&parents[0], &candidates[0]).unwrap().ownership,
            Ownership::Own
        );
        assert_eq!(
            merge.apply(&parents[1], &candidates[0]).unwrap().ownership,
            Ownership::Inherited
        );
        assert!(not_null_merge(None, &parents, &candidates).is_err());
        assert!(not_null_merge(Some(150000), &parents, &candidates).is_err());
        assert!(
            not_null_merge(
                Some(180000),
                &[Value {
                    datum: Datum::Definition("unread normalized NOT NULL".into()),
                    ..parents[0].clone()
                }],
                &candidates
            )
            .is_err()
        );
    }
    #[test]
    fn default_ownership_follows_normalized_definition_and_binding_identity() {
        use pbps_model::operation::{BoundInput, Incarnation, ObjectKey};
        let root = ObjectKey::Module(pbps_model::ModuleId::Routine(pbps_model::RoutineId::new(
            "app.f".parse().unwrap(),
            vec![],
        )));
        let before = Value {
            datum: Datum::Definition("f()".into()),
            ownership: Ownership::Own,
            bindings: [BoundInput {
                input: root.clone(),
                incarnation: Incarnation::Base,
            }]
            .into_iter()
            .collect(),
        };
        let after = Value {
            bindings: [BoundInput {
                input: root,
                incarnation: Incarnation::Final,
            }]
            .into_iter()
            .collect(),
            ..before.clone()
        };
        let TransferPolicy::Merge(merge) = default_merge(
            &[before.clone(), after.clone()],
            std::slice::from_ref(&before),
        ) else {
            panic!()
        };
        assert_eq!(
            merge.apply(&before, &before).unwrap().ownership,
            Ownership::Inherited
        );
        assert_eq!(
            merge.apply(&after, &before).unwrap().ownership,
            Ownership::Own
        );
        assert_eq!(
            merge.apply(&after, &before).unwrap().bindings,
            before.bindings
        );
        assert!(merge.apply(&before, &after).is_err());
        assert!(matches!(
            attach_policy(
                &pbps_model::operation::symbolic::Attribute::Numbering,
                Some(180000),
                &[before],
                &[after]
            )
            .unwrap(),
            (
                pbps_model::operation::symbolic::ShapeCheck::None,
                TransferPolicy::Preserve
            )
        ));
    }
}

#[cfg(test)]
mod absence_tests {
    use super::*;
    use pbps_model::operation::symbolic::ShapeCheck;
    #[test]
    fn known_absence_is_a_valid_not_null_boundary_only_when_both_columns_are_absent() {
        let absent = Value {
            datum: Datum::Absent,
            bindings: Default::default(),
            ownership: Ownership::Own,
        };
        let present = Value {
            datum: Datum::NotNull(false),
            ..absent.clone()
        };
        for version in [160000, 180000] {
            let TransferPolicy::Merge(merge) = not_null_merge(
                Some(version),
                &[absent.clone(), present.clone()],
                &[absent.clone(), present.clone()],
            )
            .unwrap() else {
                panic!()
            };
            assert!(ShapeCheck::NotNullAtLeast.accepts(&absent, &absent));
            assert!(!ShapeCheck::NotNullAtLeast.accepts(&absent, &present));
            assert!(!ShapeCheck::NotNullAtLeast.accepts(&present, &absent));
            assert_eq!(
                merge.apply(&absent, &absent).unwrap(),
                Value {
                    ownership: Ownership::Inherited,
                    ..absent.clone()
                }
            );
            assert!(
                merge
                    .apply(&absent, &present)
                    .unwrap_err()
                    .contains("42804")
            );
            assert!(
                merge
                    .apply(&present, &absent)
                    .unwrap_err()
                    .contains("42804")
            );
        }
    }
}

#[cfg(test)]
mod golden_tests {
    use std::collections::BTreeSet;
    /// `meeting_plans.golden` characterizes, per scene (13 kinds of parent
    /// change x 3 motions x 2 cuts), the plan the first implementation
    /// (`c97cd4ad`) settles on PostgreSQL 16 and 18 when the parent also has
    /// dependent defaults, a rebuilt routine and an unrelated renamed table:
    /// `<scene>\t<OK|ERR> <sha256 of the changes and spellings>` for each
    /// version. The three `ERR` plans are the conservative refusals of a
    /// retained non-convex cut. Later slices assert against it; this one keeps
    /// it whole.
    #[test]
    fn the_meeting_plan_golden_names_every_scene_once() {
        let golden = include_str!("engine_effects/meeting_plans.golden");
        let rows: Vec<Vec<&str>> = golden.lines().map(|l| l.split('\t').collect()).collect();
        assert_eq!(rows.len(), 78);
        assert_eq!(rows.iter().map(|r| r[0]).collect::<BTreeSet<_>>().len(), 78);
        for motion in ["Attach", "Detach", "Drop"] {
            assert_eq!(
                rows.iter()
                    .filter(|r| r[0].split('/').nth(1) == Some(motion))
                    .count(),
                26
            );
        }
        let plans: Vec<_> = rows.iter().flat_map(|r| &r[1..]).collect();
        assert_eq!(plans.len(), 156);
        assert!(plans.iter().all(|p| {
            let (status, hash) = p.split_once(' ').unwrap();
            matches!(status, "OK" | "ERR") && hash.len() == 64
        }));
        assert_eq!(plans.iter().filter(|p| p.starts_with("ERR")).count(), 3);
    }
}

#[cfg(test)]
mod production_tests;
