//! Declared domains rebuilt under their uid (DEC-1635.9, slice 4b).
//!
//! PostgreSQL cannot change a domain's base type or collation in place, and a
//! domain cannot be repointed at another type, so the keep-uid rebuild of
//! DEC-1635.7 is used: the replacement is created from the final declaration,
//! every column of the domain moves to it, the old type is dropped and the
//! replacement takes its name. This module decides *which* domains are
//! rebuilt and builds each one's steps with [`crate::enum_labels::rebuild_type`];
//! nothing about the mechanism is new.
//!
//! A domain is rebuilt when
//!
//! - its base type or collation changed (a conversion the engine can make from
//!   its own assignment cast; anything else is refused by name, #2331),
//! - the enum or domain it is built on is rebuilt, which can only be done by
//!   building a replacement over the base's replacement, transitively, or
//! - a check or NOT NULL is added while an array of it is a column, which the
//!   engine refuses in place (the connected planner names such domains).

use std::collections::{BTreeMap, BTreeSet};

use pbps_dialect::{Dialect, TypeChangeRisk};
use pbps_model::{
    Change, ColumnType, EnumRebuildStep, Hints, ManagedType, RetypeConversion, TypeRef, Uid,
};

use crate::{
    DiffError, Side,
    enum_labels::{Replacement, rebuild_type},
    identity::type_refs::{TypeIdentity, TypeResolver, TypeSide},
};

/// Why a domain is rebuilt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// Its base type changed.
    Base,
    /// Its collation changed.
    Collation,
    /// The enum or domain it is built on is rebuilt.
    Derived(TypeRef),
    /// A check or NOT NULL is added while an array of it is a column.
    Array,
}

impl Why {
    /// The sentence the plan gives for a rebuild no declaration of the domain
    /// itself asks for.
    pub fn shown(&self) -> String {
        match self {
            Why::Base => "its base type changed".into(),
            Why::Collation => "its collation changed".into(),
            Why::Derived(on) => format!("it is built on {}, which is rebuilt", on.shown()),
            Why::Array => "a check or NOT NULL is added while an array of it is a column".into(),
        }
    }
}

#[derive(Default)]
pub(crate) struct Rebuilt {
    pub changes: Vec<Change>,
    /// The domains rebuilt, with why: they get no in-place alteration.
    pub uids: BTreeMap<Uid, Why>,
    pub errors: Vec<DiffError>,
}

fn refusal(name: &TypeRef, reason: impl Into<String>) -> DiffError {
    DiffError::TypeLifecycleUnsupported {
        name: name.clone(),
        reason: reason.into(),
    }
}

/// The domains this plan rebuilds and the steps for each, given the steps the
/// enums' label changes already made.
pub(crate) fn plan(
    base: Side<'_>,
    desired: Side<'_>,
    hints: &Hints,
    enum_steps: &[Change],
    dialect: &(impl Dialect + ?Sized),
) -> Rebuilt {
    let mut out = Rebuilt::default();
    let resolver = TypeResolver::new(base.ids, desired.ids);
    // The enums rebuilt: replacement, labels removed, and where it is created.
    let mut rebuilt: BTreeMap<Uid, Replaced> = BTreeMap::new();
    for change in enum_steps {
        if let Change::EnumRebuild {
            uid,
            tmp,
            step: EnumRebuildStep::Create {
                removed, ordinal, ..
            },
            ..
        } = change
        {
            rebuilt.insert(
                uid.clone(),
                Replaced {
                    tmp: tmp.clone(),
                    removed: removed.clone(),
                    ordinal: *ordinal,
                    depth: 0,
                },
            );
        }
    }

    // Retained domains: their names on both sides, definitions, and seeds.
    struct Domain<'a> {
        uid: &'a Uid,
        name: &'a TypeRef,
        old: ManagedType,
        new: &'a ManagedType,
    }
    let mut domains: BTreeMap<&Uid, Domain<'_>> = BTreeMap::new();
    for (uid, name) in &desired.ids.types {
        let Some(new @ ManagedType::Domain { .. }) = desired.schema.types.get(name) else {
            continue;
        };
        let Some(was) = resolver.name(TypeSide::Base, uid) else {
            continue;
        };
        let Some(old @ ManagedType::Domain { .. }) = base.schema.types.get(was) else {
            continue;
        };
        // The old base spelled by the names the plan leaves.
        let mut comparable = old.clone();
        if let ManagedType::Domain { base: ty, .. } = &mut comparable {
            resolver.remap(ty, TypeSide::Base, TypeSide::Desired);
        }
        domains.insert(
            uid,
            Domain {
                uid,
                name,
                old: comparable,
                new,
            },
        );
    }

    let normalized = |ty: &ColumnType| dialect.normalize_type(ty).unwrap_or_else(|_| ty.clone());
    let mut why: BTreeMap<Uid, Why> = BTreeMap::new();
    for d in domains.values() {
        let (
            ManagedType::Domain {
                base: old_base,
                collation: old_collation,
                ..
            },
            ManagedType::Domain {
                base: new_base,
                collation: new_collation,
                ..
            },
        ) = (&d.old, d.new)
        else {
            continue;
        };
        let (old_base, new_base) = (normalized(old_base), normalized(new_base));
        if old_base != new_base {
            // The engine's own assignment cast converts compatible bases; the
            // rest needs a conversion the author states, which is #2331.
            let compatible = old_base.type_ref().is_none()
                && new_base.type_ref().is_none()
                && dialect.type_change_risk(&old_base, &new_base) != TypeChangeRisk::Incompatible;
            if compatible {
                why.insert(d.uid.clone(), Why::Base);
            } else {
                out.errors.push(refusal(
                    d.name,
                    format!(
                        "its base type changes from {} to {}, which the engine does not convert by itself; a conversion the author states waits for #2331",
                        old_base.shown(),
                        new_base.shown()
                    ),
                ));
            }
        } else if old_collation != new_collation {
            why.insert(d.uid.clone(), Why::Collation);
        }
        if !why.contains_key(d.uid)
            && hints.domain_array_holders.contains(d.uid)
            && pbps_model::DomainDelta::of(&d.old, d.new).is_some_and(|delta| delta.tightens())
        {
            why.insert(d.uid.clone(), Why::Array);
        }
    }

    // Derived: built on a rebuilt enum or domain, transitively.
    loop {
        let mut grew = false;
        for d in domains.values() {
            // The type it is built on being rebuilt is the reason that holds
            // for every attribute, so it is the one given, over an array's.
            if matches!(
                why.get(d.uid),
                Some(Why::Base | Why::Collation | Why::Derived(_))
            ) {
                continue;
            }
            let ManagedType::Domain { base: ty, .. } = d.new else {
                continue;
            };
            let Some(reference) = ty.type_ref() else {
                continue;
            };
            let TypeIdentity::Declared(on) = resolver.resolve(TypeSide::Desired, reference) else {
                continue;
            };
            if rebuilt.contains_key(on) || why.contains_key(on) {
                why.insert(d.uid.clone(), Why::Derived(reference.clone()));
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }

    // A domain created in this plan over a type it rebuilds would hold the old
    // type, which the swap drops.
    for (uid, name) in &desired.ids.types {
        if domains.contains_key(uid) {
            continue;
        }
        let Some(ManagedType::Domain { base: ty, .. }) = desired.schema.types.get(name) else {
            continue;
        };
        if let Some(reference) = ty.type_ref()
            && let TypeIdentity::Declared(on) = resolver.resolve(TypeSide::Desired, reference)
            && (rebuilt.contains_key(on) || why.contains_key(on))
        {
            out.errors.push(refusal(
                name,
                format!(
                    "it is created in a plan that rebuilds {}; create it in a plan of its own",
                    reference.shown()
                ),
            ));
        }
    }

    // Rebuild each in base-depth order.
    // Each pass plans the domains whose base is planned: a chain of any depth
    // takes one pass per level, so the bound is the number of domains.
    let mut pending: Vec<&Uid> = why.keys().collect();
    let mut failed: BTreeSet<&Uid> = BTreeSet::new();
    for _ in 0..=why.len() {
        if pending.is_empty() {
            break;
        }
        let mut next = Vec::new();
        for uid in pending {
            let d = &domains[uid];
            let ManagedType::Domain { base: ty, .. } = d.new else {
                continue;
            };
            let on = ty.type_ref().and_then(|reference| {
                match resolver.resolve(TypeSide::Desired, reference) {
                    TypeIdentity::Declared(on) => Some(on),
                    TypeIdentity::ExternalOrUnrecorded(_) => None,
                }
            });
            let under = match on {
                Some(on) if rebuilt.contains_key(on) => Some(&rebuilt[on]),
                // Built on a domain this plan refuses: its own refusal says
                // all there is to say.
                Some(on) if failed.contains(on) => {
                    failed.insert(uid);
                    continue;
                }
                Some(on) if why.contains_key(on) => {
                    // Built on a domain not planned yet: wait for it.
                    next.push(uid);
                    continue;
                }
                _ => None,
            };
            let (over, removed, ordinal, depth) = match under {
                Some(r) => (
                    Some(r.tmp.clone()),
                    r.removed.clone(),
                    r.ordinal + 1,
                    r.depth + 1,
                ),
                None => (None, Vec::new(), 0, 1),
            };
            let reason = &why[uid];
            let (conversion, base_change) = match reason {
                Why::Derived(_) => (RetypeConversion::Text, None),
                Why::Base => {
                    let (
                        ManagedType::Domain { base: from, .. },
                        ManagedType::Domain { base: to, .. },
                    ) = (&d.old, d.new)
                    else {
                        continue;
                    };
                    (
                        RetypeConversion::Assign,
                        Some((normalized(from), normalized(to))),
                    )
                }
                Why::Collation | Why::Array => (RetypeConversion::Assign, None),
            };
            match rebuild_type(
                base,
                desired,
                hints,
                (d.uid, d.name),
                Replacement {
                    definition: d.new.clone(),
                    removed: removed.clone(),
                    over,
                    why: Some(reason.shown()),
                    conversion,
                    base_change,
                    ordinal,
                    depth,
                },
            ) {
                Ok(steps) => {
                    if let Some(Change::EnumRebuild { tmp, .. }) = steps.first() {
                        rebuilt.insert(
                            d.uid.clone(),
                            Replaced {
                                tmp: tmp.clone(),
                                removed,
                                ordinal,
                                depth,
                            },
                        );
                    }
                    out.changes.extend(steps);
                }
                // Includes a domain renamed in the plan that rebuilds it: the
                // steps would run under the old name beside the rename.
                Err(reason) => {
                    failed.insert(uid);
                    out.errors.push(refusal(d.name, reason));
                }
            }
        }
        pending = next;
    }
    for uid in pending {
        out.errors.push(refusal(
            domains[uid].name,
            "its base could not be ordered before it",
        ));
    }
    out.uids = why;
    out
}

struct Replaced {
    tmp: TypeRef,
    removed: Vec<String>,
    ordinal: usize,
    depth: usize,
}

#[cfg(test)]
#[allow(clippy::wildcard_enum_match_arm)] // Fixtures name only the rebuild's changes.
mod tests {
    use super::*;
    use pbps_dialect::MinimalDialect;
    use pbps_model::{Column, ColumnRef, IdsFile, Schema, Table, TableName, UidKind};

    /// A dialect that converts every pair of built-in bases but one with an
    /// integer, as the engine's assignment casts do for text and character
    /// types.
    struct Casts;
    impl Dialect for Casts {
        fn name(&self) -> &'static str {
            "casts"
        }
        fn quote_ident(&self, i: &str) -> Result<String, pbps_dialect::DialectError> {
            MinimalDialect.quote_ident(i)
        }
        fn intermediate_tables(
            &self,
            c: &Change,
            s: pbps_model::Strategy,
        ) -> Result<Vec<TableName>, pbps_dialect::DialectError> {
            MinimalDialect.intermediate_tables(c, s)
        }
        fn normalize_type(&self, t: &ColumnType) -> Result<ColumnType, pbps_dialect::DialectError> {
            MinimalDialect.normalize_type(t)
        }
        fn type_change_risk(&self, from: &ColumnType, to: &ColumnType) -> TypeChangeRisk {
            if from.base == "integer" || to.base == "integer" {
                TypeChangeRisk::Incompatible
            } else {
                TypeChangeRisk::Safe
            }
        }
        fn fold_ident<'a>(&self, i: &'a str) -> std::borrow::Cow<'a, str> {
            MinimalDialect.fold_ident(i)
        }
        fn lexicon(&self) -> pbps_dialect::Lexicon {
            MinimalDialect.lexicon()
        }
        fn validate_table(&self, n: &TableName, t: &Table) -> Vec<pbps_dialect::DialectError> {
            MinimalDialect.validate_table(n, t)
        }
        fn transaction_framing(
            &self,
            z: &pbps_model::SessionZone,
        ) -> pbps_dialect::TransactionFraming {
            MinimalDialect.transaction_framing(z)
        }
        fn probe_framing(&self) -> Option<pbps_dialect::TransactionFraming> {
            MinimalDialect.probe_framing()
        }
    }

    fn domain(base: &str, collation: Option<&str>, checks: &[(&str, &str)]) -> ManagedType {
        ManagedType::Domain {
            base: base.parse().unwrap(),
            not_null: false,
            default: None,
            checks: checks
                .iter()
                .map(|(n, e)| ((*n).into(), (*e).into()))
                .collect(),
            collation: collation.map(|c| c.parse().unwrap()),
        }
    }

    fn uid(n: u32) -> Uid {
        format!("y_{n:06}").parse().unwrap()
    }

    /// `app.t(id, v app.d, w app.d2)` beside the declared types, by uid:
    /// `app.e` (1), `app.d` (2) and `app.d2` (3).
    fn world(types: &[(&str, ManagedType)]) -> (Schema, IdsFile) {
        let t: TableName = "app.t".parse().unwrap();
        let mut table = Table::default();
        for (name, ty) in [("id", "integer"), ("v", "app.d"), ("w", "app.d2")] {
            table
                .columns
                .insert(name.into(), Column::new(ty.parse().unwrap()));
        }
        let mut ids = IdsFile {
            tables: [(Uid::derived(UidKind::Table, "t", 0), t.clone())].into(),
            ..Default::default()
        };
        for (n, (name, _)) in types.iter().enumerate() {
            ids.types
                .insert(uid(n as u32 + 1), (*name).parse().unwrap());
        }
        for column in ["id", "v", "w"] {
            ids.columns.insert(
                Uid::derived(UidKind::Column, column, 0),
                ColumnRef {
                    table: t.clone(),
                    name: column.into(),
                },
            );
        }
        (
            Schema {
                types: types
                    .iter()
                    .map(|(n, d)| ((*n).parse().unwrap(), d.clone()))
                    .collect(),
                tables: [(t, table)].into(),
                ..Default::default()
            },
            ids,
        )
    }

    fn enumeration(values: &[&str]) -> ManagedType {
        ManagedType::Enum {
            values: values.iter().map(|v| (*v).into()).collect(),
        }
    }

    fn run(old: &[(&str, ManagedType)], new: &[(&str, ManagedType)], hints: &Hints) -> Rebuilt {
        let (b, d) = (world(old), world(new));
        let (base, desired) = (
            Side {
                schema: &b.0,
                ids: &b.1,
            },
            Side {
                schema: &d.0,
                ids: &d.1,
            },
        );
        let (enum_steps, errors) = crate::enum_labels::changes(base, desired, hints);
        assert!(errors.is_empty(), "{errors:?}");
        let mut out = plan(base, desired, hints, &enum_steps, &Casts);
        let mut all = enum_steps;
        all.append(&mut out.changes);
        out.changes = all;
        out
    }

    fn steps(rebuilt: &Rebuilt, of: &Uid) -> Vec<EnumRebuildStep> {
        rebuilt
            .changes
            .iter()
            .filter_map(|c| match c {
                Change::EnumRebuild { uid, step, .. } if uid == of => Some(step.clone()),
                _ => None,
            })
            .collect()
    }

    fn tables() -> Vec<(&'static str, ManagedType)> {
        vec![("app.e", enumeration(&["a", "b"]))]
    }

    #[test]
    fn a_base_change_is_a_rebuild_whose_retypes_ride_the_assignment_cast() {
        let mut old = tables();
        old.push(("app.d", domain("varchar(20)", None, &[])));
        let mut new = tables();
        new.push(("app.d", domain("text", None, &[])));
        let out = run(&old, &new, &Hints::default());
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.uids.get(&uid(2)), Some(&Why::Base));
        let [create, retype, swap] = &steps(&out, &uid(2))[..] else {
            panic!("{:?}", out.changes);
        };
        assert!(matches!(
            create,
            EnumRebuildStep::Create { why: Some(w), over: None, .. } if w == "its base type changed"
        ));
        let EnumRebuildStep::Retype {
            column,
            array,
            conversion,
            base_change,
            ..
        } = retype
        else {
            panic!("{retype:?}");
        };
        assert_eq!((column.name.raw(), *array), ("v", false));
        assert_eq!(*conversion, RetypeConversion::Assign);
        let (from, to) = &**base_change.as_ref().expect("the narrowing probe's pair");
        assert_eq!(
            (from.shown().to_string(), to.shown().to_string()),
            ("varchar(20)".into(), "text".into())
        );
        assert!(matches!(
            swap,
            EnumRebuildStep::Swap {
                depth: 1,
                over: None,
                ..
            }
        ));
        // Negatives: the same base, and a base the engine cannot convert, are
        // not this rebuild.
        old[1].1 = domain("text", None, &[]);
        assert!(run(&old, &new, &Hints::default()).uids.is_empty());
        new[1].1 = domain("integer", None, &[]);
        let out = run(&old, &new, &Hints::default());
        assert!(out.changes.is_empty() && out.uids.is_empty());
        let said = out.errors[0].to_string();
        assert!(said.contains("app.d") && said.contains("#2331"), "{said}");
    }

    #[test]
    fn a_collation_change_alone_rebuilds_without_a_narrowing_pair() {
        let mut old = tables();
        old.push(("app.d", domain("text", None, &[])));
        let mut new = tables();
        new.push(("app.d", domain("text", Some("pg_catalog.\"C\""), &[])));
        let out = run(&old, &new, &Hints::default());
        assert_eq!(out.uids.get(&uid(2)), Some(&Why::Collation));
        assert!(steps(&out, &uid(2)).iter().any(|s| matches!(
            s,
            EnumRebuildStep::Retype {
                base_change: None,
                conversion: RetypeConversion::Assign,
                ..
            }
        )));
    }

    #[test]
    fn a_rebuilt_enum_takes_the_domains_over_it_in_depth_order_and_unrelated_ones_stay() {
        let enum_old = ("app.e", enumeration(&["a", "b"]));
        let enum_new = ("app.e", enumeration(&["b"]));
        let on = |base: &str| domain(base, None, &[]);
        let old = vec![
            enum_old,
            ("app.d", on("app.e")),
            ("app.d2", on("app.d")),
            ("app.u", on("text")),
        ];
        let mut new = old.clone();
        new[0] = enum_new;
        let out = run(&old, &new, &Hints::default());
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.uids.len(), 2, "{:?}", out.uids);
        assert!(
            matches!(out.uids.get(&uid(2)), Some(Why::Derived(r)) if r.shown().to_string() == "app.e")
        );
        assert!(
            matches!(out.uids.get(&uid(3)), Some(Why::Derived(r)) if r.shown().to_string() == "app.d")
        );
        // The unrelated domain is not rebuilt.
        assert!(!out.uids.contains_key(&uid(4)));
        let first = steps(&out, &uid(2));
        let second = steps(&out, &uid(3));
        assert!(matches!(
            &first[0],
            EnumRebuildStep::Create { over: Some(_), removed, .. } if removed == &["a".to_owned()]
        ));
        assert!(matches!(
            &second[0],
            EnumRebuildStep::Create {
                over: Some(_),
                ordinal: 2,
                ..
            }
        ));
        // Labels are mapped through text, and the deeper swap comes first.
        assert!(matches!(
            &first[1],
            EnumRebuildStep::Retype {
                conversion: RetypeConversion::Text,
                base_change: None,
                ..
            }
        ));
        assert!(matches!(&first[2], EnumRebuildStep::Swap { depth: 1, .. }));
        assert!(matches!(&second[2], EnumRebuildStep::Swap { depth: 2, .. }));
    }

    #[test]
    fn a_tightening_beside_an_array_of_the_domain_is_the_rebuild_and_a_loosening_is_not() {
        let old = vec![
            ("app.e", enumeration(&["a"])),
            ("app.d", domain("text", None, &[("c", "VALUE <> ''")])),
        ];
        let tighter = vec![
            ("app.e", enumeration(&["a"])),
            (
                "app.d",
                domain("text", None, &[("c", "VALUE <> ''"), ("d", "VALUE <> 'x'")]),
            ),
        ];
        let mut hints = Hints::default();
        // No array of it stands: the in-place alteration is the plan.
        assert!(run(&old, &tighter, &hints).uids.is_empty());
        hints.domain_array_holders.insert(uid(2));
        let out = run(&old, &tighter, &hints);
        assert_eq!(out.uids.get(&uid(2)), Some(&Why::Array));
        // A loosening is not the engine's concern, array or not.
        let looser = vec![
            ("app.e", enumeration(&["a"])),
            ("app.d", domain("text", None, &[])),
        ];
        assert!(run(&old, &looser, &hints).uids.is_empty());
    }

    /// `n` domains, each over the one before and the first over `app.e`. The
    /// deepest is named last and listed (so given its uid) first: a swap order
    /// that fell back to names would drop each base under the domain over it,
    /// and a planner that took one pass per 64 levels, in uid order, would
    /// need a pass for each level.
    fn chain(n: usize, labels: &[&str]) -> Vec<(String, ManagedType)> {
        let mut types = vec![("app.e".to_owned(), enumeration(labels))];
        for depth in (1..=n).rev() {
            let on = if depth == 1 {
                "app.e".to_owned()
            } else {
                format!("app.d{:03}", depth - 1)
            };
            types.push((format!("app.d{depth:03}"), domain(&on, None, &[])));
        }
        types
    }

    fn world_owned(types: &[(String, ManagedType)]) -> (Schema, IdsFile) {
        let (mut schema, mut ids) = world(&[]);
        for (n, (name, definition)) in types.iter().enumerate() {
            ids.types.insert(uid(n as u32 + 1), name.parse().unwrap());
            schema
                .types
                .insert(name.parse().unwrap(), definition.clone());
        }
        (schema, ids)
    }

    /// The swaps of a whole plan, by the depth each carries, in plan order.
    fn swap_depths(
        types_old: &[(String, ManagedType)],
        types_new: &[(String, ManagedType)],
    ) -> Vec<usize> {
        let (b, d) = (world_owned(types_old), world_owned(types_new));
        let plan = crate::schema_diff::diff(
            Side {
                schema: &b.0,
                ids: &b.1,
            },
            Side {
                schema: &d.0,
                ids: &d.1,
            },
            &Casts,
            &Hints::default(),
        )
        .expect("a valid rebuild plans");
        plan.changes()
            .changes
            .iter()
            .filter_map(|p| match &p.change {
                Change::EnumRebuild {
                    step: EnumRebuildStep::Swap { depth, .. },
                    ..
                } => Some(*depth),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn swaps_run_deepest_first_at_any_depth_and_a_shallow_chain_keeps_its_order() {
        // Past ten levels, where a one-digit rank gave two depths one value
        // and the names decided: the old base was dropped under its domain.
        for n in [3, 11, 12, 70] {
            let depths = swap_depths(&chain(n, &["a", "b"]), &chain(n, &["b"]));
            let want: Vec<usize> = (0..=n).rev().filter(|d| *d > 0).chain([0]).collect();
            assert_eq!(depths.len(), n + 1, "{n}: {depths:?}");
            // Every domain's swap before the enum's (depth 0), each deeper
            // one before the one it is built on.
            assert_eq!(depths, want, "{n}");
        }
    }

    #[test]
    fn a_domain_renamed_in_the_plan_that_rebuilds_it_is_refused_by_name_with_no_steps() {
        let old = vec![
            ("app.e", enumeration(&["a"])),
            ("app.d", domain("varchar(20)", None, &[])),
        ];
        // Rename and base change: refused.
        let renamed = vec![
            ("app.e", enumeration(&["a"])),
            ("app.d_new", domain("text", None, &[])),
        ];
        let out = run(&old, &renamed, &Hints::default());
        assert!(out.changes.is_empty(), "{:?}", out.changes);
        let said = out
            .errors
            .iter()
            .map(ToString::to_string)
            .collect::<String>();
        assert!(
            said.contains("app.d_new") && said.contains("renamed or moved in this plan"),
            "{said}"
        );
        // Rename alone and rebuild alone are not.
        let only_renamed = vec![
            ("app.e", enumeration(&["a"])),
            ("app.d_new", domain("varchar(20)", None, &[])),
        ];
        let out = run(&old, &only_renamed, &Hints::default());
        assert!(out.errors.is_empty() && out.uids.is_empty() && out.changes.is_empty());
        let only_rebuilt = vec![
            ("app.e", enumeration(&["a"])),
            ("app.d", domain("text", None, &[])),
        ];
        let out = run(&old, &only_rebuilt, &Hints::default());
        assert!(out.errors.is_empty() && out.uids.len() == 1);
        // A domain over a rebuilt enum is rebuilt for that alone, and renamed
        // beside it is refused like any other; the one beside it is not.
        let over = |name: &'static str| {
            vec![
                ("app.e", enumeration(&["a", "b"])),
                (name, domain("app.e", None, &[])),
            ]
        };
        let mut shrunk = over("app.d_new");
        shrunk[0].1 = enumeration(&["b"]);
        let out = run(&over("app.d"), &shrunk, &Hints::default());
        let said = out
            .errors
            .iter()
            .map(ToString::to_string)
            .collect::<String>();
        assert!(said.contains("renamed or moved in this plan"), "{said}");
        assert!(out.changes.iter().all(|c| match c {
            Change::EnumRebuild { uid: u, .. } => *u != uid(2),
            _ => true,
        }));
        let mut kept = over("app.d");
        kept[0].1 = enumeration(&["b"]);
        assert!(
            run(&over("app.d"), &kept, &Hints::default())
                .errors
                .is_empty()
        );
    }
}
