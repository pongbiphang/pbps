//! PostgreSQL's measured membership policy, as portable planning data.
//! No catalog I/O or tentative ordering belongs in these rules (DEC-2296.2).
mod columns;
pub(crate) mod facts;
mod operations;
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
    let mut cases = Vec::new();
    for parent in parents {
        let Datum::NotNull(p) = parent.datum else {
            return Err("the parent NOT NULL boundary fact is unavailable".into());
        };
        for candidate in candidates {
            let Datum::NotNull(c) = candidate.datum else {
                return Err("the candidate NOT NULL boundary fact is unavailable".into());
            };
            let result = if p && !c {
                Fact::Unsupported {
                    sqlstate: Some("42804".into()),
                    reason: "the candidate is nullable while its parent is NOT NULL".into(),
                }
            } else {
                Fact::Known(Value {
                    ownership: if c && !p {
                        if version >= 180000 {
                            Ownership::LocalNotNull
                        } else {
                            Ownership::Own
                        }
                    } else {
                        Ownership::Inherited
                    },
                    ..candidate.clone()
                })
            };
            let row = MergeCase {
                parent: parent.clone(),
                candidate: candidate.clone(),
                result,
            };
            if !cases.contains(&row) {
                cases.push(row);
            }
        }
    }
    Ok(TransferPolicy::Merge(Merge { cases }))
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
                    datum: Datum::Absent,
                    ..parents[0].clone()
                }],
                &candidates
            )
            .is_err()
        );
    }

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
