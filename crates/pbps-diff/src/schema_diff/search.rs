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
/// the catalog proves stale, and a rename, which arrives in a later part. An object the
/// target has and the declarations do not, with no identity recorded, is left alone.
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
        // The identity first, as for a package: a name the base holds under one recorded
        // identity and the declarations under another is an object retired and declared
        // again before the drop reached the target. Equal definitions would skip the create
        // while the drop of the old identity still came, leaving a declared object dropped.
        if base.schema.search.contains_key(name)
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
        match base.schema.search.get(name) {
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
                    Some(uid)
                        if base.ids.search.get(uid).is_some_and(|was| {
                            was != name && base.schema.search.contains_key(was)
                        }) =>
                    {
                        refuse(name, "renaming one arrives in a later part of #2728");
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
