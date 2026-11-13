//! What a plan pins about the extensions it creates, updates, moves or drops.

use super::*;

/// The packages a plan installs, by name.
pub(crate) fn created_extensions(
    cs: &pbps_model::ChangeSet,
) -> impl Iterator<Item = &pbps_model::ObjectLocalName> {
    cs.changes.iter().filter_map(|p| {
        if let pbps_model::Change::CreateExtension { name, .. } = &p.change {
            Some(name)
        } else {
            None
        }
    })
}

/// The packages a plan installs or updates, by name and what is done: each
/// runs a script pbps cannot read, so each needs a rehearsal to pin (DEC-2447.6,
/// DEC-2447.10).
pub(super) fn changed_extensions(
    cs: &pbps_model::ChangeSet,
) -> impl Iterator<Item = (&pbps_model::ObjectLocalName, &'static str)> {
    cs.changes.iter().filter_map(|p| {
        if let pbps_model::Change::CreateExtension { name, .. } = &p.change {
            Some((name, "created"))
        } else if let pbps_model::Change::UpdateExtension { name, .. } = &p.change {
            Some((name, "updated"))
        } else if let pbps_model::Change::SetExtensionSchema { name, .. } = &p.change {
            Some((name, "moved"))
        } else {
            None
        }
    })
}

/// What the target says about the packages the declaration holds and it lacks,
/// judged before any plan is written (DEC-2447.6): every refusal names the
/// package and the fact. The answer is each package's own prerequisites, which
/// the differ turns into the order among the plan's creates.
pub(crate) async fn extension_creates(
    conn: &mut pbps_db::Conn,
    base: &Schema,
    declared: &Schema,
    ids: &IdsFile,
) -> anyhow::Result<BTreeMap<pbps_model::ObjectLocalName, BTreeSet<pbps_model::ObjectLocalName>>> {
    let wanted: BTreeMap<_, _> = declared
        .extensions
        .iter()
        .filter(|(name, _)| {
            !base.extensions.contains_key(*name) && ids.extension_uid(name).is_some()
        })
        .collect();
    if wanted.is_empty() {
        return Ok(BTreeMap::new());
    }
    let target = pbps_pg::extension_provider::target(conn).await?;
    let pinned: BTreeMap<String, pbps_model::Extension> = wanted
        .iter()
        .map(|(name, definition)| (name.raw().to_owned(), (*definition).clone()))
        .collect();
    let problems: Vec<String> = wanted
        .iter()
        .flat_map(|(name, definition)| {
            pbps_pg::extension_provider::create_problems(&target, name.raw(), definition, &pinned)
        })
        .collect();
    anyhow::ensure!(problems.is_empty(), "{}", problems.join("\n"));
    Ok(wanted
        .iter()
        .filter_map(|(name, definition)| {
            let control = target
                .available
                .get(&(name.raw().to_owned(), definition.version.clone()))?;
            Some((
                (*name).clone(),
                control.requires.iter().map(|r| r.as_str().into()).collect(),
            ))
        })
        .collect())
}

/// The packages the declarations hold at another version than the target, in
/// the same schema, under one recorded identity: each becomes an update, judged
/// before any plan is written (DEC-2447.10). Every refusal names the package;
/// the facts are what the differ turns into the update's order.
pub(crate) async fn extension_update_facts(
    conn: &mut pbps_db::Conn,
    base: &Schema,
    declared: &Schema,
    recorded: &IdsFile,
    ids: &IdsFile,
) -> anyhow::Result<BTreeMap<pbps_model::ObjectLocalName, pbps_model::ExtensionUpdateFacts>> {
    let updates: BTreeMap<_, _> = declared
        .extensions
        .iter()
        .filter_map(|(name, to)| {
            let from = base.extensions.get(name)?;
            let same = recorded.extension_uid(name).is_some()
                && recorded.extension_uid(name) == ids.extension_uid(name);
            // An update runs in the schema the package holds; a move that
            // follows it is [`extension_move_facts`]'s (DEC-2447.11).
            (same && from.version != to.version).then(|| {
                let stays = pbps_model::Extension {
                    schema: from.schema.clone(),
                    version: to.version.clone(),
                };
                (name.clone(), (from.clone(), stays))
            })
        })
        .collect();
    if updates.is_empty() {
        return Ok(BTreeMap::new());
    }
    let creating: BTreeMap<String, pbps_model::Extension> = declared
        .extensions
        .iter()
        .filter(|(name, _)| {
            !base.extensions.contains_key(*name) && ids.extension_uid(name).is_some()
        })
        .map(|(name, definition)| (name.raw().to_owned(), definition.clone()))
        .collect();
    let named: BTreeMap<String, _> = updates
        .iter()
        .map(|(name, pair)| (name.raw().to_owned(), pair.clone()))
        .collect();
    let problems = pbps_pg::extension_provider::update_problems(conn, &named, &creating).await?;
    anyhow::ensure!(problems.is_empty(), "{}", problems.join("\n"));
    let available = pbps_pg::extension_provider::available(conn).await?;
    let all = pbps_pg::extension_provider::providers(conn).await?;
    let mut reach = pbps_pg::extension_provider::drop_facts(conn, &all).await?;
    let mut facts = BTreeMap::new();
    for (name, (from, to)) in updates {
        // The prerequisites of every version on the path, which the same
        // function that judged them above names (DEC-2447.10).
        let path =
            pbps_pg::extension_provider::update_path(conn, name.raw(), &from.version, &to.version)
                .await?
                .unwrap_or_default();
        let requires = pbps_pg::extension_provider::path_requires(&available, name.raw(), &path)
            .iter()
            .map(|r| r.as_str().into())
            .collect();
        let reach = reach.remove(name.raw()).unwrap_or_default();
        facts.insert(name, pbps_model::ExtensionUpdateFacts { requires, reach });
    }
    Ok(facts)
}

/// The packages the declarations hold in another schema than the target, under
/// one recorded identity: each becomes a move, judged before any plan is written
/// (DEC-2447.11). Every refusal names the package; the answer is each package's
/// members as the destination would name them, which the differ turns into the
/// modules the move waits for.
pub(crate) async fn extension_move_facts(
    conn: &mut pbps_db::Conn,
    base: &Schema,
    declared: &Schema,
    recorded: &IdsFile,
    ids: &IdsFile,
) -> anyhow::Result<BTreeMap<pbps_model::ObjectLocalName, BTreeSet<pbps_model::ProviderMember>>> {
    let (mut moves, mut updating) = (BTreeMap::new(), BTreeSet::new());
    for (name, to) in &declared.extensions {
        let Some(from) = base.extensions.get(name) else {
            continue;
        };
        let same = recorded.extension_uid(name).is_some()
            && recorded.extension_uid(name) == ids.extension_uid(name);
        if same && from.schema != to.schema {
            // The move starts from the version this plan's update leaves.
            let starts = pbps_model::Extension {
                schema: from.schema.clone(),
                version: to.version.clone(),
            };
            moves.insert(name.raw().to_owned(), (starts, to.clone()));
            if from.version != to.version {
                updating.insert(name.raw().to_owned());
            }
        }
    }
    if moves.is_empty() {
        return Ok(BTreeMap::new());
    }
    let problems = pbps_pg::extension_provider::move_problems(conn, &moves, &updating).await?;
    anyhow::ensure!(problems.is_empty(), "{}", problems.join("\n"));
    let all = pbps_pg::extension_provider::providers(conn).await?;
    Ok(pbps_pg::extension_provider::relocated_members(&all, &moves)
        .into_iter()
        .map(|(name, members)| (name.as_str().into(), members))
        .collect())
}

/// An update that would leave a member the declarations also manage as their
/// own, or leave the package at another version than declared, is refused
/// before it is sealed: one object cannot have two owners (DEC-2447.7).
pub(super) fn refuse_two_owners(
    name: &pbps_model::ObjectLocalName,
    after: &pbps_model::ExtensionProvider,
    to: &pbps_model::Extension,
    declared: &Schema,
) -> anyhow::Result<()> {
    let collisions = after.collisions(declared);
    anyhow::ensure!(
        collisions.is_empty() && after.definition == *to,
        "extension `{}` would leave {} that the declarations also manage as their own, or not at the declared version; one object cannot have two owners",
        name.shown(),
        collisions
            .iter()
            .map(|m| format!("`{m}`"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}

/// The sealed evidence of each package change: the manifest the target holds
/// now, the one the rehearsal read after running the plan's statements, and the
/// versions the engine reports an update would walk. One pin covers the package's
/// whole chain, an update and then a move. A package the rehearsal read no
/// manifest for, that the declarations also manage a member of, or that a move
/// would put on a name an unmanaged object holds in the destination, is refused,
/// never sealed.
pub(crate) async fn extension_change_pins(
    conn: &mut pbps_db::Conn,
    cs: &pbps_model::ChangeSet,
    manifests: &crate::rehearsal::connected::Manifests,
    declared: &Schema,
) -> anyhow::Result<Vec<pbps_model::ExtensionChangePin>> {
    use pbps_model::{Change, Extension, Uid};
    type Step<'a> = (&'a Extension, &'a Extension);
    type Chain<'a> = (
        &'a pbps_model::ObjectLocalName,
        Option<Step<'a>>,
        Option<(Step<'a>, &'a BTreeSet<pbps_model::ModuleId>)>,
    );
    let mut chains: BTreeMap<&Uid, Chain<'_>> = BTreeMap::new();
    for planned in &cs.changes {
        if let Change::UpdateExtension {
            uid,
            name,
            from,
            to,
            ..
        } = &planned.change
        {
            chains.entry(uid).or_insert((name, None, None)).1 = Some((from, to));
        } else if let Change::SetExtensionSchema {
            uid,
            name,
            from,
            to,
            releases,
        } = &planned.change
        {
            chains.entry(uid).or_insert((name, None, None)).2 = Some(((from, to), releases));
        }
    }
    let mut pins = Vec::new();
    let mut held = None;
    for (uid, (name, update, movement)) in chains {
        let (from, to) = (
            update
                .or(movement.map(|(step, _)| step))
                .map(|(from, _)| from),
            movement.map(|(step, _)| step).or(update).map(|(_, to)| to),
        );
        let (Some(from), Some(to)) = (from, to) else {
            continue;
        };
        let after = manifests.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "the rehearsal read no members for extension `{}`, so the plan cannot pin its change",
                name.shown()
            )
        })?;
        refuse_two_owners(name, after, to, declared)?;
        if held.is_none() {
            held = Some(pbps_pg::extension_provider::providers(conn).await?);
        }
        let before = held
            .as_ref()
            .and_then(|all| all.get(name.raw()))
            .filter(|m| m.definition == *from)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "extension `{}` is not installed as the plan assumed, so the plan cannot pin its change",
                    name.shown()
                )
            })?;
        if let Some((_, releases)) = movement {
            // What the destination holds now that a moved member would take: the
            // modules this plan drops are the handover, anything else is refused
            // here, where the rehearsal's scratch cannot see an unmanaged object.
            let held = pbps_pg::extension_provider::schema_occupants(conn, &to.schema).await?;
            let taken: Vec<String> = after
                .members
                .iter()
                .filter(|m| held.contains(*m) && m.module(releases).is_none())
                .map(|m| format!("`{m}`"))
                .collect();
            anyhow::ensure!(
                taken.is_empty(),
                "extension `{}` cannot move to schema `{}`: it already holds {}, which this plan does not drop, so the engine would refuse",
                name.shown(),
                pbps_model::escape::Escaped(&to.schema),
                taken.join(", ")
            );
        }
        let path = match update {
            Some((from, to)) => pbps_pg::extension_provider::update_path(
                conn,
                name.raw(),
                &from.version,
                &to.version,
            )
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("extension `{}` has no update path to pin", name.shown())
            })?,
            None => Vec::new(),
        };
        pins.push(pbps_model::ExtensionChangePin {
            uid: uid.clone(),
            name: name.clone(),
            from: from.clone(),
            to: to.clone(),
            path,
            before: before.clone(),
            after: after.clone(),
        });
    }
    Ok(pins)
}

/// The packages a recorded intent retires, by name: held by the target, no
/// longer declared, and tombstoned in the identity this plan will carry.
pub(crate) fn retired_extensions(
    base: &Schema,
    declared: &Schema,
    recorded: &IdsFile,
    ids: &IdsFile,
) -> BTreeSet<pbps_model::ObjectLocalName> {
    base.extensions
        .keys()
        .filter(|name| !declared.extensions.contains_key(*name))
        .filter(|name| {
            recorded
                .extension_uid(name)
                .is_some_and(|uid| ids.tombstones.contains_key(uid))
        })
        .cloned()
        .collect()
}

/// What the target says dropping each retired package would reach, judged
/// before any plan is written (DEC-2447.8): the connected role must be allowed
/// to drop it, and the facts become the differ's input, which carries what the
/// plan also removes and refuses the rest by name.
pub(crate) async fn extension_drop_facts(
    conn: &mut pbps_db::Conn,
    retired: &BTreeSet<pbps_model::ObjectLocalName>,
) -> anyhow::Result<BTreeMap<pbps_model::ObjectLocalName, pbps_model::ExtensionDropFacts>> {
    if retired.is_empty() {
        return Ok(BTreeMap::new());
    }
    let names: BTreeSet<String> = retired.iter().map(|n| n.raw().to_owned()).collect();
    let problems = pbps_pg::extension_provider::drop_problems(conn, &names).await?;
    anyhow::ensure!(problems.is_empty(), "{}", problems.join("\n"));
    let all = pbps_pg::extension_provider::providers(conn).await?;
    let mut facts = pbps_pg::extension_provider::drop_facts(conn, &all).await?;
    Ok(retired
        .iter()
        .filter_map(|name| Some((name.clone(), facts.remove(name.raw())?)))
        .collect())
}

/// The sealed evidence of each drop: the manifest the target holds now, and
/// what the drop reaches beyond it that the plan accepted. A package the
/// target no longer holds as the plan assumed is refused, never sealed.
pub(crate) async fn extension_drop_pins(
    conn: &mut pbps_db::Conn,
    cs: &pbps_model::ChangeSet,
) -> anyhow::Result<Vec<pbps_model::ExtensionDropPin>> {
    let drops: Vec<_> = cs
        .changes
        .iter()
        .filter_map(|p| {
            if let pbps_model::Change::DropExtension {
                uid,
                name,
                definition,
                ..
            } = &p.change
            {
                Some((uid, name, definition))
            } else {
                None
            }
        })
        .collect();
    if drops.is_empty() {
        return Ok(Vec::new());
    }
    let all = pbps_pg::extension_provider::providers(conn).await?;
    let mut facts = pbps_pg::extension_provider::drop_facts(conn, &all).await?;
    drops
        .into_iter()
        .map(|(uid, name, definition)| {
            let manifest = all.get(name.raw()).filter(|m| m.definition == *definition);
            let manifest = manifest.ok_or_else(|| {
                anyhow::anyhow!(
                    "extension `{}` is not installed as the plan assumed, so the plan cannot pin its drop",
                    name.shown()
                )
            })?;
            Ok(pbps_model::ExtensionDropPin {
                uid: uid.clone(),
                name: name.clone(),
                definition: definition.clone(),
                manifest: manifest.clone(),
                outside: facts.remove(name.raw()).unwrap_or_default().outside,
            })
        })
        .collect()
}

/// The sealed evidence of each create: the manifest the rehearsal read, after
/// the check that no member is also an object the declaration manages. A
/// create the rehearsal did not capture is refused, never sealed empty.
pub(crate) fn extension_pins(
    cs: &pbps_model::ChangeSet,
    mut manifests: crate::rehearsal::connected::Manifests,
    declared: &Schema,
    ids: &IdsFile,
) -> anyhow::Result<Vec<pbps_model::ExtensionPin>> {
    let mut pins = Vec::new();
    for planned in &cs.changes {
        let pbps_model::Change::CreateExtension {
            uid,
            name,
            definition,
            ..
        } = &planned.change
        else {
            continue;
        };
        let manifest = manifests.remove(name).ok_or_else(|| {
            anyhow::anyhow!(
                "the rehearsal read no members for extension `{}`, so the plan cannot pin it",
                name.shown()
            )
        })?;
        let collisions = manifest.collisions(declared);
        anyhow::ensure!(
            collisions.is_empty() && ids.extension_uid(name) == Some(uid),
            "extension `{}` brings {} that the declarations also manage as their own; one object cannot have two owners",
            name.shown(),
            collisions
                .iter()
                .map(|m| format!("`{m}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        pins.push(pbps_model::ExtensionPin {
            uid: uid.clone(),
            name: name.clone(),
            definition: definition.clone(),
            manifest,
        });
    }
    Ok(pins)
}
