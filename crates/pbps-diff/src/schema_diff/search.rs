//! Text search objects: what a plan creates, alters and drops, and the order they depend in.

use super::*;

/// The text search objects a plan drops: a baseline identity the declarations no longer
/// hold, whose name the baseline schema has. The one definition of "dropped" that the
/// differ's reference check and a package's drop analysis both read (DEC-2728.2).
pub(crate) fn dropped_search<'a>(
    base: Side<'a>,
    declared: Side<'_>,
) -> std::collections::BTreeSet<&'a pbps_model::SearchRef> {
    base.ids
        .search
        .iter()
        .filter(|(uid, name)| {
            base.schema.search.contains_key(*name) && !declared.ids.search.contains_key(*uid)
        })
        .map(|(_, name)| name)
        .collect()
}

/// The text search objects a plan frees from a package whose dictionaries live in
/// `package_schema` (DEC-2728.2, DEC-2728.3): the ones it drops, and a configuration it keeps
/// and alters away from a dictionary the project does not hold. The alteration (class 7) runs
/// before the package's removal. One that may still map such a dictionary stays refused.
pub(crate) fn released_search<'a>(
    base: Side<'a>,
    declared: Side<'_>,
    package_schema: &str,
) -> std::collections::BTreeSet<&'a pbps_model::SearchRef> {
    let held = |used: &pbps_model::SearchRef| {
        declared.schema.search.contains_key(used) || base.schema.search.contains_key(used)
    };
    let package_owned = |used: &pbps_model::SearchRef| {
        !held(used) && used.name.schema() != "pg_catalog" && used.name.schema() == package_schema
    };
    let mut released = dropped_search(base, declared);
    for (uid, name) in &base.ids.search {
        let (Some(was), Some(kept)) = (
            base.schema.search.get(name),
            declared
                .ids
                .search
                .get(uid)
                .and_then(|kept| declared.schema.search.get(kept)),
        ) else {
            continue;
        };
        let (was, now) = (was.dictionaries(), kept.dictionaries());
        if name.kind == pbps_model::SearchKind::Config
            && was.iter().any(&package_owned)
            && !now.iter().any(&package_owned)
        {
            released.insert(name);
        }
    }
    released
}

/// Text search configurations and dictionaries, by kind and name (DEC-2728.1, DEC-2728.2).
/// A declared object the target lacks is created and one the declarations gave up is
/// dropped, both by recorded identity, and a standing one whose mappings or options differ is
/// altered in place (DEC-2728.3). **Refused here, by name**: a changed parser or template
/// (no `ALTER` form exists), an in-place change whose dependents the target did not list or
/// the catalog proves stale, and a rename that also changes the object, crosses a schema or
/// leaves a thesaurus naming the old dictionary. An object the target has and the
/// declarations do not, with no identity recorded, is left alone.
///
/// A rename (DEC-2728.5) is an identity that moved to another name in the same schema. The
/// engine keeps every reference by oid, so a configuration over a renamed dictionary is the
/// same configuration, and the comparison reads the baseline with the dictionary names the
/// plan renames already spelled the new way.
pub(super) fn diff_search(
    base: Side<'_>,
    declared: Side<'_>,
    hints: &Hints,
    errs: &mut Vec<DiffError>,
) -> Vec<Change> {
    let target_read = hints.target_read;
    let mut changes = Vec::new();
    let mut refuse = |name: &pbps_model::SearchRef, reason: &str| {
        errs.push(DiffError::SearchRefused {
            name: name.clone(),
            reason: reason.to_owned(),
        });
    };
    // The objects this plan drops: a reference resolves only to one the plan keeps. The
    // baseline holding it is no answer, since the drop comes after the create (class 18)
    // and the engine's RESTRICT would refuse it while anything still maps it.
    let dropped = dropped_search(base, declared);
    // The renames of this plan, old name to new: one identity under another name in both
    // sides, the object held on both. `renamed_from` is the inverse, by new name.
    let search_renames: BTreeMap<pbps_model::SearchRef, pbps_model::SearchRef> = base
        .ids
        .search
        .iter()
        .filter_map(|(uid, old)| {
            let new = declared.ids.search.get(uid)?;
            (new != old
                && base.schema.search.contains_key(old)
                && declared.schema.search.contains_key(new))
            .then(|| (old.clone(), new.clone()))
        })
        .collect();
    let renamed_from: BTreeMap<_, _> = search_renames.iter().map(|(old, new)| (new, old)).collect();
    // What the baseline holds under `name`: nothing once the plan renames it away (the name
    // is vacated, and a new object may take it), and the dictionary names a rename moved
    // spelled the new way, since the engine's mapping follows the dictionary, not its name.
    let standing = |name: &pbps_model::SearchRef| -> Option<SearchObject> {
        if search_renames.contains_key(name) {
            return None;
        }
        base.schema
            .search
            .get(name)
            .map(|object| object.respelled(&search_renames))
    };
    // A rename loop (a swap) in an ids file the resolver did not write: no order of
    // `RENAME TO` can run it, because each target is the other's source (DEC-2728.5).
    for start in search_renames.keys() {
        let (mut seen, mut at) = (BTreeSet::new(), start);
        while let Some(next) = search_renames.get(at) {
            if !seen.insert(at) {
                refuse(
                    start,
                    "is part of a rename cycle (a swap), which no order of renames runs; rename in separate plans",
                );
                break;
            }
            at = next;
        }
    }
    // A dictionary the project neither declares nor the target holds, in a schema no package
    // of the project provides and that is not the engine's own, is one the engine would refuse
    // at `ADD MAPPING`.
    let unresolved = |object: &SearchObject| -> Vec<pbps_model::SearchRef> {
        object
            .dictionaries()
            .into_iter()
            .filter(|used| {
                !declared.schema.search.contains_key(used)
                    && !base.schema.search.contains_key(used)
                    && used.name.schema() != "pg_catalog"
                    && !declared
                        .schema
                        .extensions
                        .values()
                        .any(|package| package.schema == used.name.schema())
            })
            .collect()
    };
    let undeclared = |missing: &[pbps_model::SearchRef]| -> String {
        let shown: Vec<_> = missing.iter().map(|m| m.shown().to_string()).collect();
        format!(
            "uses {}, which the project does not declare and the target does not hold; declare \
             it, or leave the mapping out",
            shown.join(", ")
        )
    };
    for (name, now) in &declared.schema.search {
        let retired: Vec<String> = now
            .dictionaries()
            .iter()
            .filter(|used| dropped.contains(used))
            .map(|used| used.shown().to_string())
            .collect();
        if !retired.is_empty() {
            refuse(
                name,
                &format!(
                    "uses {}, which this plan drops; keep it declared, or leave the mapping out",
                    retired.join(", ")
                ),
            );
            continue;
        }
        let moved: Vec<String> = now
            .dictionaries()
            .iter()
            .filter(|used| !declared.schema.search.contains_key(used))
            .filter_map(|used| search_renames.get(used))
            .map(|new| new.shown().to_string())
            .collect();
        if !moved.is_empty() {
            refuse(
                name,
                &format!(
                    "uses a dictionary this plan renames; name it {} instead",
                    moved.join(", ")
                ),
            );
            continue;
        }
        // The identity first, as for a package: a name the base holds under one recorded
        // identity and the declarations under another is an object retired and declared
        // again before the drop reached the target. Equal definitions would skip the create
        // while the drop of the old identity still came, leaving a declared object dropped.
        if standing(name).is_some()
            && let (Some(was), Some(now)) =
                (base.ids.search_uid(name), declared.ids.search_uid(name))
            && was != now
        {
            refuse(
                name,
                "was retired and is declared again before the drop reached the target; drop it \
                 in one plan and declare the name again in the next",
            );
            continue;
        }
        match standing(name).as_ref() {
            Some(was) if was == now => {}
            Some(was) => {
                let Some(uid) = declared.ids.search_uid(name) else {
                    refuse(
                        name,
                        "has no recorded identity; run `pbps plan` to record it",
                    );
                    continue;
                };
                let what = match (was, now) {
                    (
                        SearchObject::Config { parser: a, .. },
                        SearchObject::Config { parser: b, .. },
                    ) if a != b => {
                        refuse(
                            name,
                            "its parser differs, and no `ALTER` form changes a configuration's \
                             parser; declare the new one under a new name and move what uses it",
                        );
                        continue;
                    }
                    (
                        SearchObject::Dictionary { template: a, .. },
                        SearchObject::Dictionary { template: b, .. },
                    ) if a != b => {
                        refuse(
                            name,
                            "its template differs, and no `ALTER` form changes a dictionary's \
                             template; declare the new one under a new name and move what \
                             uses it",
                        );
                        continue;
                    }
                    (SearchObject::Config { .. }, SearchObject::Config { .. }) => "mappings",
                    (SearchObject::Dictionary { .. }, SearchObject::Dictionary { .. }) => "options",
                    _ => {
                        refuse(
                            name,
                            "is declared as another kind of object than the target holds",
                        );
                        continue;
                    }
                };
                // What was computed with the old behaviour stays until rebuilt, and the
                // engine does not rebuild it: an index, a stored column or a check over the
                // object keeps answering as it did. The target's catalog lists them; a plan
                // without it cannot say there are none. What the catalog proves is refused
                // by name; what stands behind a node it cannot see into is carried by the
                // change, which the gated `stale-results` class approves (DEC-2728.3).
                let Some(stale) = hints.search_dependents.get(name).filter(|_| target_read) else {
                    refuse(
                        name,
                        &format!(
                            "its {what} differ, and what depends on it is the target's catalog \
                             to list; plan against the database (`--db`)"
                        ),
                    );
                    continue;
                };
                if !stale.stored.is_empty() {
                    refuse(
                        name,
                        &format!(
                            "its {what} differ, but {} would keep the old behaviour until \
                             rebuilt; declare the new behaviour under a new name and move the \
                             dependents, in two plans",
                            stale.stored.join(", ")
                        ),
                    );
                    continue;
                }
                // A reference the standing object already makes resolves: the target holds the
                // object with it, so the engine accepted it, whether or not the model holds what
                // it names (a package's or another tool's dictionary). Only what the alteration
                // adds can be one the engine would refuse.
                let held = was.dictionaries();
                let missing: Vec<_> = unresolved(now)
                    .into_iter()
                    .filter(|used| !held.contains(used))
                    .collect();
                if !missing.is_empty() {
                    refuse(name, &undeclared(&missing));
                    continue;
                }
                let (uid, name, opaque) = (uid.clone(), name.clone(), stale.opaque.clone());
                let (before, after) = (Box::new(was.clone()), Box::new(now.clone()));
                changes.push(Change::Search(if what == "mappings" {
                    SearchChange::AlterSearchMapping {
                        uid,
                        name,
                        before,
                        after,
                        opaque,
                    }
                } else {
                    SearchChange::AlterSearchDictionary {
                        uid,
                        name,
                        before,
                        after,
                        opaque,
                    }
                }));
            }
            None => {
                match declared.ids.search_uid(name) {
                    None => refuse(
                        name,
                        "has no recorded identity; run `pbps plan` to record it",
                    ),
                    Some(uid) if let Some(old) = renamed_from.get(name) => {
                        match rename_search(uid, old, name, now, base, &dropped, &search_renames) {
                            Ok(change) => changes.push(change),
                            Err(reason) => refuse(name, &reason),
                        }
                    }
                    Some(uid) => {
                        // A dictionary the project neither declares nor the target holds, in a
                        // schema no package of the project provides and that is not the
                        // engine's own, is one the engine would refuse at `ADD MAPPING`.
                        // Against the target that is a refusal. Offline the baseline only
                        // stands for the target, and a dictionary made on the database
                        // outside pbps is not in it, so the plan goes on and says on its
                        // line that the database settles it (DEC-2728.2).
                        let missing = unresolved(now);
                        if missing.is_empty() || !target_read {
                            changes.push(Change::Search(SearchChange::CreateSearch {
                                uid: uid.clone(),
                                name: name.clone(),
                                definition: Box::new(now.clone()),
                                unresolved: missing,
                            }));
                        } else {
                            refuse(name, &undeclared(&missing));
                        }
                    }
                }
            }
        }
    }
    for (uid, name) in &base.ids.search {
        if let Some(definition) = base.schema.search.get(name)
            && !declared.ids.search.contains_key(uid)
        {
            changes.push(Change::Search(SearchChange::DropSearch {
                uid: uid.clone(),
                name: name.clone(),
                definition: Box::new(definition.clone()),
            }));
        }
    }
    changes
}

/// How many dictionaries deep `name` is among the text search objects of `schema`, at least
/// what `start` gives it: a dictionary mapping or naming none is 0. A cycle is the engine's
/// to refuse, so a reference back to an object being measured counts nothing.
pub(super) fn search_depth(
    schema: &Schema,
    name: &pbps_model::SearchRef,
    start: &dyn Fn(&pbps_model::SearchRef, &pbps_model::SearchObject) -> isize,
    seen: &mut BTreeSet<pbps_model::SearchRef>,
) -> isize {
    if !seen.insert(name.clone()) {
        return 0;
    }
    let depth = schema.search.get(name).map_or(0, |object| {
        object
            .dictionaries()
            .iter()
            .filter(|used| schema.search.contains_key(*used))
            .map(|used| 1 + search_depth(schema, used, start, seen))
            .max()
            .unwrap_or(0)
            .max(start(name, object))
    });
    seen.remove(name);
    depth
}

/// The change for one object whose identity moved from `old` to `name` (DEC-2728.5): a lone
/// `RenameSearch`, or the reason it cannot be planned. Refused by name: a move to another
/// schema (`RENAME TO` keeps the schema and `SET SCHEMA` is not planned), a definition that
/// also changes (the census, the staged pin chain and the saved plan's admission each key an
/// object on one name), and a dictionary a held thesaurus names in its option, which the
/// engine stores as text and does not follow (DEC-2728.3).
fn rename_search(
    uid: &Uid,
    old: &pbps_model::SearchRef,
    name: &pbps_model::SearchRef,
    now: &SearchObject,
    base: Side<'_>,
    dropped: &BTreeSet<&pbps_model::SearchRef>,
    renames: &BTreeMap<pbps_model::SearchRef, pbps_model::SearchRef>,
) -> Result<Change, String> {
    let was = base
        .schema
        .search
        .get(old)
        .map(|object| object.respelled(renames))
        .ok_or_else(|| "renames an object the target does not hold".to_owned())?;
    if old.kind != name.kind {
        return Err("is declared as another kind of object than the target holds".into());
    }
    if old.name.schema() != name.name.schema() {
        return Err(format!(
            "moves from schema {} to {}; a rename keeps the schema, and moving one is not planned",
            old.name.schema(),
            name.name.schema()
        ));
    }
    if was != *now {
        return Err(format!(
            "is renamed from {} and also changes; rename it in one plan and change it in the next",
            old.shown()
        ));
    }
    let naming: Vec<String> = base
        .schema
        .search
        .iter()
        .filter(|(other, object)| {
            !dropped.contains(other) && object.dictionaries().contains(old) && **other != *old
        })
        .filter(|(_, object)| matches!(object, SearchObject::Dictionary { .. }))
        .map(|(other, _)| other.shown().to_string())
        .collect();
    if !naming.is_empty() {
        return Err(format!(
            "is named by {} in its `dictionary` option, which the engine keeps as text and does \
             not follow; declare the new dictionary under a new name and move that one, in two plans",
            naming.join(", ")
        ));
    }
    Ok(Change::Search(SearchChange::RenameSearch {
        uid: uid.clone(),
        from: old.clone(),
        to: name.clone(),
        definition: Box::new(was),
    }))
}
