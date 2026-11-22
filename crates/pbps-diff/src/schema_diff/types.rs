//! The types phase, and the differences between two declarations of one type.

use super::*;

/// Lifecycle refusals for the attributes no executable change covers, shared
/// with connected prerequisite admission so a missing declared type is named
/// before its column's reference gate. `alterable` leaves out what a retained
/// domain changes in place: its NOT NULL, default and checks (DEC-1635.8).
pub fn type_differences(
    base: &Schema,
    declared: &Schema,
    dialect: &(impl Dialect + ?Sized),
    alterable: bool,
) -> Vec<DiffError> {
    let mut errs = Vec::new();
    // Whole-object changes are handled by uid; retained definitions still
    // refuse the attributes no change covers.
    let names: BTreeSet<_> = base.types.keys().chain(declared.types.keys()).collect();
    for name in names {
        let reason = match (base.types.get(name), declared.types.get(name)) {
            (None, Some(_)) | (Some(_), None) => None,
            (Some(old), Some(new)) => {
                let normalize = |definition: &pbps_model::ManagedType| {
                    let mut definition = definition.clone();
                    if let pbps_model::ManagedType::Domain { base, .. } = &mut definition {
                        *base = dialect
                            .normalize_type(base)
                            .unwrap_or_else(|_| base.clone());
                    }
                    definition
                };
                let mut fields = normalize(old).differences(&normalize(new));
                if alterable {
                    fields.retain(|field| {
                        !matches!(
                            *field,
                            "domain NOT NULL" | "domain default" | "domain checks"
                        )
                    });
                }
                (!fields.is_empty()).then(|| {
                    let why = match new {
                        pbps_model::ManagedType::Enum { .. } => {
                            "an enum's labels change only through its recorded uid (#1635 slice 3, DEC-1635.6, DEC-1635.7)"
                        }
                        pbps_model::ManagedType::Domain { .. } => {
                            "a domain's base type and collation change by rebuilding it (#1635 slice 4b), and its NOT NULL, default and checks only through its recorded uid (DEC-1635.8)"
                        }
                    };
                    format!("{} changed; {why}", fields.join(", "))
                })
            }
            (None, None) => None,
        };
        if let Some(reason) = reason {
            errs.push(DiffError::TypeLifecycleUnsupported {
                name: name.clone(),
                reason,
            });
        }
    }

    errs
}

/// The types phase of `diff_partial_rebuilding`: refusals for types that no
/// identity file records, then drop, alter and rename of the recorded ones, the
/// creates, and the refusals for rename cycles and for retained identities
/// whose declaration is gone.
///
/// It pushes into the caller's lists instead of returning its own, because the
/// order of `errors` is part of the result and the passes that follow read
/// `changes` as the earlier phases left it. It returns the resolver the tables
/// phase reads.
pub(super) fn diff_types<'a>(
    base: Side<'a>,
    declared: Side<'a>,
    dialect: &(impl Dialect + ?Sized),
    rebuilt_domain_uids: &BTreeMap<Uid, crate::domain_rebuild::Why>,
    changes: &mut Vec<Change>,
    errs: &mut Vec<DiffError>,
) -> TypeResolver<'a> {
    // Definitions match by recorded identity. A rename is never inferred
    // from equal definitions or a pair of disappearing/appearing names.
    let types = TypeResolver::new(base.ids, declared.ids);
    let unrecorded_base = Schema {
        types: base
            .schema
            .types
            .iter()
            .filter(|(name, _)| {
                matches!(
                    types.resolve(TypeSide::Base, name),
                    TypeIdentity::ExternalOrUnrecorded(_)
                ) && matches!(
                    types.resolve(TypeSide::Desired, name),
                    TypeIdentity::ExternalOrUnrecorded(_)
                )
            })
            .map(|(n, d)| (n.clone(), d.clone()))
            .collect(),
        ..Default::default()
    };
    errs.extend(type_differences(
        &unrecorded_base,
        declared.schema,
        dialect,
        false,
    ));
    let mut type_renames = BTreeMap::new();
    for (uid, old_name) in &base.ids.types {
        let Some(old) = base.schema.types.get(old_name) else {
            continue;
        };
        match declared
            .ids
            .types
            .get(uid)
            .and_then(|name| declared.schema.types.get(name).map(|ty| (name, ty)))
        {
            None => {
                let used = types.has_retained_dependent(base.schema, declared.schema, uid);
                if used {
                    errs.push(DiffError::TypeLifecycleUnsupported { name: old_name.clone(), reason: "a retained declaration still uses this type; drop or change its dependents first".into() });
                }
                changes.push(Change::Type(TypeChange::DropType {
                    uid: uid.clone(),
                    name: old_name.clone(),
                    definition: Box::new(old.clone()),
                }));
            }
            Some((name, new)) => {
                let mut comparable = old.clone();
                if let pbps_model::ManagedType::Domain { base: ty, .. } = &mut comparable {
                    types.remap(ty, TypeSide::Base, TypeSide::Desired);
                }
                let old_schema = Schema {
                    types: [(name.clone(), comparable)].into(),
                    ..Default::default()
                };
                let new_schema = Schema {
                    types: [(name.clone(), new.clone())].into(),
                    ..Default::default()
                };
                let mut refused = Vec::new();
                // An enum's labels and a domain's every attribute have their
                // own changes; only a change of kind is refused here
                // (DEC-1635.6, DEC-1635.8, DEC-1635.9).
                if !matches!(
                    (&old_schema.types[name], new),
                    (
                        pbps_model::ManagedType::Enum { .. },
                        pbps_model::ManagedType::Enum { .. }
                    ) | (
                        pbps_model::ManagedType::Domain { .. },
                        pbps_model::ManagedType::Domain { .. }
                    )
                ) {
                    refused = type_differences(&old_schema, &new_schema, dialect, true);
                }
                // A retained domain changes its checks, default and NOT NULL
                // in place: what it drops before the plan's rows, and what it
                // adds after them. A base or collation change was refused
                // above and plans nothing (DEC-1635.8).
                let comparable = &old_schema.types[name];
                let alters = refused.is_empty()
                    && !rebuilt_domain_uids.contains_key(uid)
                    && matches!(
                        (comparable, new),
                        (
                            pbps_model::ManagedType::Domain { .. },
                            pbps_model::ManagedType::Domain { .. }
                        )
                    );
                if alters && let Some(delta) = pbps_model::DomainDelta::of(comparable, new) {
                    for (half, wanted) in [
                        (pbps_model::DomainHalf::Loosen, delta.loosens()),
                        (pbps_model::DomainHalf::Tighten, delta.tightens()),
                    ] {
                        if wanted {
                            changes.push(Change::TypeValue(TypeValueChange::AlterDomain {
                                uid: uid.clone(),
                                name: name.clone(),
                                from: Box::new(comparable.clone()),
                                to: Box::new(new.clone()),
                                half,
                            }));
                        }
                    }
                }
                errs.extend(refused);
                if old_name != name {
                    type_renames.insert(old_name.clone(), name.clone());
                    changes.push(Change::Type(TypeChange::RenameType {
                        uid: uid.clone(),
                        from: old_name.clone(),
                        to: name.clone(),
                        // What stands when the rename runs: an enum's labels
                        // and a domain the plan alters are the old ones, the
                        // domain under the spellings of the types it is over
                        // that the plan leaves.
                        definition: Box::new(match (old, alters) {
                            (pbps_model::ManagedType::Enum { .. }, _) => old.clone(),
                            (pbps_model::ManagedType::Domain { .. }, true) => comparable.clone(),
                            (pbps_model::ManagedType::Domain { .. }, false) => new.clone(),
                        }),
                    }));
                }
            }
        }
    }
    for (uid, name) in &declared.ids.types {
        if base
            .ids
            .types
            .get(uid)
            .and_then(|old| base.schema.types.get(old))
            .is_none()
            && let Some(definition) = declared.schema.types.get(name)
        {
            changes.push(Change::Type(TypeChange::CreateType {
                uid: uid.clone(),
                name: name.clone(),
                definition: Box::new(definition.clone()),
            }));
        }
    }
    for start in type_renames.keys() {
        let mut seen = BTreeSet::new();
        let mut name = start;
        while let Some(next) = type_renames.get(name) {
            if !seen.insert(name) {
                errs.push(DiffError::TypeLifecycleUnsupported {
                    name: start.clone(),
                    reason: "type rename cycle (swap) is refused; use separate plans".into(),
                });
                break;
            }
            name = next;
        }
    }

    // A retained uid is still a managed claim after a manual drop and a
    // baseline. It cannot become an unheld identity in an empty apply.
    for (uid, name) in &declared.ids.types {
        if !declared.schema.types.contains_key(name)
            && types
                .name(TypeSide::Base, uid)
                .and_then(|old| base.schema.types.get(old))
                .is_none()
        {
            errs.push(DiffError::TypeLifecycleUnsupported {
                name: name.clone(),
                reason: "its identity remains recorded but its declaration is absent".into(),
            });
        }
    }
    types
}
