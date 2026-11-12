//! The names a plan claims and the objects already holding them: created-name
//! inventories, occupants, clashes, carried moves, and names history or roles
//! already took.

use super::*;

/// The relation names this plan creates: its tables, the views it creates
/// that it does not also drop, the indexes it creates by name, and fixed
/// sequence targets (DEC-1899.1). A view dropped and created in one plan is
/// a rebuild, and its name is already its own.
// The complement is every change that creates or frees no relation name.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn created_relation_names(cs: &pbps_model::ChangeSet) -> Vec<TableName> {
    use pbps_model::Change;
    let dropped: BTreeSet<&ModuleId> = cs
        .changes
        .iter()
        .filter_map(|p| match &p.change {
            Change::DropModule { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    cs.changes
        .iter()
        .filter(|p| !matches!(&p.change, Change::CreateModule { id, .. } if dropped.contains(id)))
        .flat_map(|p| {
            relation_names_created(&p.change).into_iter().chain(
                sequence_names_claimed(&p.change)
                    .into_iter()
                    .map(|(name, _)| name),
            )
        })
        .collect()
}

/// Fixed sequence targets, with their columns for diagnostics. Keep these
/// out of `relation_names_created`: its identity-release and detach callers
/// select ordering edges, which this namespace check does not add (DEC-1899.1).
/// The dialect calls the emitter's `owned_sequence`, including truncation.
// The complement is every change that creates or renames no owned sequence.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn sequence_names_claimed(
    change: &pbps_model::Change,
) -> Vec<(TableName, pbps_model::ColumnRef)> {
    use pbps_model::{Change, ColumnRef, ValueSource};
    let named = |column: ColumnRef, source: &ValueSource| {
        let ValueSource::Sequence(sequence) = source else {
            return None;
        };
        let name = pbps_pg::Postgres::new()
            .owned_sequence_name(&column.table, column.name.raw(), sequence)
            .expect("PostgreSQL always names an owned sequence");
        Some((TableName::new(&column.table.schema, name), column))
    };
    match change {
        Change::CreateTable { name, table, .. } => table
            .columns
            .iter()
            .filter_map(|(column, declared)| {
                named(
                    ColumnRef::new(name.clone(), column.clone()),
                    &declared.value_source,
                )
            })
            .collect(),
        Change::AddColumn {
            table,
            name,
            column,
            ..
        } => named(
            ColumnRef::new(table.clone(), name.clone()),
            &column.value_source,
        )
        .into_iter()
        .collect(),
        Change::AlterColumnNumbering {
            column, from, to, ..
        } => {
            if let (ValueSource::Sequence(was), ValueSource::Sequence(now)) = (from, to)
                && !(was.name.is_some() && now.name.is_some() && was.name != now.name)
            {
                // A retune, or an omitted name on either side, keeps the
                // catalog name; the emitter does not rename it.
                return Vec::new();
            }
            named(column.clone(), to).into_iter().collect()
        }
        _ => Vec::new(),
    }
}

/// The relation names one change creates: a table, a view, and the index
/// names of [`index_names_created`].
// The complement is every change that creates no relation.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn relation_names_created(change: &pbps_model::Change) -> Vec<TableName> {
    use pbps_model::Change;
    let mut out = match change {
        Change::CreateTable { name, .. } => vec![name.clone()],
        Change::CreateModule {
            id: ModuleId::Named(name),
            module,
        } if module.kind == pbps_model::ModuleKind::View => vec![name.clone()],
        _ => Vec::new(),
    };
    out.extend(index_names_created(change));
    out
}

/// The index names this plan creates ([`index_names_created`]).
pub(super) fn created_index_names(cs: &pbps_model::ChangeSet) -> Vec<TableName> {
    cs.changes
        .iter()
        .flat_map(|p| index_names_created(&p.change))
        .collect()
}

/// The index names a change creates, each in its table's schema (#1355):
/// an added index, the index behind an added unique constraint or a named
/// primary key, which PostgreSQL names after the constraint, and those a
/// created table declares. An index shares the relation namespace with
/// tables, views and sequences, so `CREATE INDEX` at a held name fails with
/// 42P07. An unnamed key's `<table>_pkey` is not here: PostgreSQL gives it a
/// fallback when that is held (DEC-465.1). Changes on a renamed table carry
/// its new name, which is the schema the index is created in.
// The complement is every change that creates no index.
#[allow(clippy::wildcard_enum_match_arm)]
pub(super) fn index_names_created(change: &pbps_model::Change) -> Vec<TableName> {
    use pbps_model::Change;
    let in_schema = |t: &TableName, name: &str| TableName::new(t.schema.clone(), name);
    match change {
        Change::AddIndex { table, name, .. } | Change::AddUnique { table, name, .. } => {
            vec![in_schema(table, name.raw())]
        }
        Change::SetPrimaryKey {
            table,
            to: Some(key),
            ..
        } => key
            .name
            .iter()
            .map(|name| in_schema(table, name.raw()))
            .collect(),
        Change::DetachPartition { table, names, .. } => names
            .iter()
            .filter(|n| {
                matches!(
                    n.kind,
                    pbps_model::DetachedKind::PrimaryKey
                        | pbps_model::DetachedKind::Unique
                        | pbps_model::DetachedKind::Index
                )
            })
            .filter_map(|n| n.name.as_ref().map(pbps_model::ObjectLocalName::raw))
            .map(|name| in_schema(table, name))
            .collect(),
        Change::CreateTable { name, table, .. } => table
            .primary_key
            .iter()
            .filter_map(|k| k.name.as_ref().map(pbps_model::ObjectLocalName::raw))
            .chain(table.unique.keys().map(pbps_model::ObjectLocalName::raw))
            .chain(table.indexes.keys().map(pbps_model::ObjectLocalName::raw))
            .map(|n| in_schema(name, n))
            .collect(),
        _ => Vec::new(),
    }
}

/// Refuses a name this plan creates that a sequence, an index or a composite
/// type already holds (#951), the relation-namespace entries the catalog
/// inventory behind [`refuse_occupied_names`] does not report. One this plan
/// frees first is not an occupant: an index it drops, and an index or owned
/// sequence of a table it drops, which goes with its table.
///
/// The occupants are replayed through the plan in its order ([`Occupant`]),
/// and each name a change creates is compared with them as they are at that
/// change, after its own drops (#1723). A rename moves what its table owns,
/// and a cross-schema one carries it into the destination schema (#1084).
/// There it also meets the tables and views at its landing name (`relations`,
/// managed or not), which a drop or a rename away frees first and a rename
/// into it holds from then on (#1765).
pub(crate) fn refuse_uninventoried_occupants(
    cs: &pbps_model::ChangeSet,
    occupants: &[pbps_pg::catalog::NameOccupant],
    relations: &[(TableName, &'static str)],
    label: &str,
    catalog: &pbps_dialect::slots::CatalogFacts,
) -> anyhow::Result<()> {
    let taken: Vec<String> = name_clashes(&cs.changes, occupants, relations, catalog)
        .into_iter()
        .map(|(_, clash)| clash)
        .collect();
    if taken.is_empty() {
        return Ok(());
    }
    // A detach's clone-name claim needs a separate-deployment remedy,
    // naming the detach.
    let remedy = if cs
        .changes
        .iter()
        .any(|p| matches!(p.change, pbps_model::Change::DetachPartition { .. }))
    {
        "Each creation, schema move, or detach name change above would be refused at apply. \
         Free that name in a plan of its own first (another partition's clone: detach that \
         partition first), or declare another name (DEC-1565.1)."
    } else {
        "Each `CREATE`, sequence rename, or the `SET SCHEMA` of each cross-schema rename would be \
         refused at apply. \
         Rename the declaration, or drop or rename the object in the database. Where a later \
         rename in this plan moves the holder away, apply that rename in a plan of its own \
         first."
    };
    bail!(
        "`{label}` already uses {} name(s) this plan would create or move an object onto, for \
         objects that share PostgreSQL's relation namespace with tables and views:\n  {}\n{remedy}",
        taken.len(),
        taken.join("\n  "),
        label = pbps_model::escape::Escaped(label),
    );
}

/// Orders a cross-schema rename's carried objects clear of a holder that a
/// rename of this plan moves away (#1767, #1957, DEC-1957.2). Where the
/// replay ([`name_clashes`]) finds a move landing on a held name, the plan is
/// settled over its name handovers: the plan's own names and the catalog's
/// occupants as the first holders of theirs (`order::slots::handovers`), so
/// the move runs after what moves its holder away, and a chain of such moves
/// is ordered by the same evidence. The order settled is kept only where
/// table renames alone pass one another, and the replay finds fewer clashes
/// in it: a release build computes no other edge, and a table rename meets
/// another only through the names both see, which the edges and the replay
/// hold. Anything else between them, a cycle, or names that cannot be read
/// keep the order, and [`refuse_uninventoried_occupants`] refuses it. A
/// creation taking an identity sequence's catalog name also follows its
/// release, with the effects holding its dependents after it (DEC-1986.1).
/// Detached partitions hand over their exact catalog clone names the same
/// way, retaining each detach's own block and guards (DEC-1558.1).
pub(crate) fn order_carried_moves<S: Clone>(
    cs: &mut PlanningPlan<S>,
    occupants: &[pbps_pg::catalog::NameOccupant],
    relations: &[(TableName, &'static str)],
    sides: (pbps_diff::Side<'_>, pbps_diff::Side<'_>),
    dialect: &dyn pbps_dialect::Dialect,
    catalog: &pbps_dialect::slots::CatalogFacts,
) {
    use pbps_model::{Change, ValueSource};

    let clashes = name_clashes(&cs.changes, occupants, relations, catalog);
    if clashes.is_empty() {
        return;
    }
    let mut facts = occupant_slots(occupants);
    facts.detach_releases.clone_from(&catalog.detach_releases);
    let Ok(mut schedule) =
        pbps_diff::order::slots::handovers(&cs.changes, sides.0, sides.1, dialect, &facts)
    else {
        return;
    };
    // A retype and a numbering change are both possible leavers to the
    // generic slots. The PostgreSQL identity release is exact, so state its
    // own handover instead of requiring a unique generic edge (DEC-1986.2).
    let mut moving = vec![false; cs.changes.len()];
    let mut detach_steps = vec![false; cs.changes.len()];
    let mut present = Occupant::with_detaches(occupants, catalog);
    for (before, release) in cs.changes.iter().enumerate() {
        detach_steps[before] = matches!(
            &release.change,
            Change::DetachPartition { .. }
                | Change::DropTable {
                    detach_from: Some(_),
                    ..
                }
        ) && present.iter().any(|o| o.freed_by(&release.change));
        if matches!(&release.change, Change::AlterColumnNumbering {
            from: ValueSource::Identity(_), to, ..
        } if !matches!(to, ValueSource::Identity(_)))
        {
            for (after, claim) in cs.changes.iter().enumerate() {
                if relation_names_created(&claim.change).iter().any(|name| {
                    present.iter().any(|o| {
                        o.found.kind == "sequence" && o.name == *name && o.freed_by(&release.change)
                    })
                }) {
                    schedule.edge(
                        before,
                        after,
                        pbps_diff::order::Source::Catalog,
                        pbps_model::resolver::OrderReason::Identity,
                    );
                    moving[after] = true;
                }
            }
        }
        present.retain(|o| !o.freed_by(&release.change));
        for o in &mut present {
            if let Change::RenameTable { from, to, .. } = &release.change {
                o.moved(from, to);
            }
            if let Change::RenameColumn {
                table, from, to, ..
            } = &release.change
            {
                o.column_renamed(table, from, to);
            }
        }
    }
    // Detach handovers already have exact catalog edges. Renames can claim
    // the clone's name directly or carry an index onto it; neither is a
    // creation, but both need the same effect closure (DEC-1558.2).
    for edge in schedule.constraints() {
        if edge.source == pbps_diff::order::Source::Catalog
            && detach_steps[edge.before]
            && (matches!(cs.changes[edge.after].change, Change::RenameTable { .. })
                || !relation_names_created(&cs.changes[edge.after].change).is_empty())
        {
            moving[edge.after] = true;
        }
    }
    let Ok(mut settled) = schedule.settle(cs.clone()) else {
        return;
    };
    let renames: Vec<bool> = cs
        .changes
        .iter()
        .map(|p| matches!(p.change, pbps_model::Change::RenameTable { .. }))
        .collect();
    if !only_crossed_by(&settled.sequence, &renames) {
        use pbps_diff::order::{Baseline, Checks, Schedule, analyse_with_catalog, graph};
        use pbps_model::operation::{ModuleEdits, Operations, PlanOwners, Snapshot};
        if !moving.iter().any(|&moves| moves) {
            return;
        }
        let Ok(names) = pbps_model::plan_names::PlanNames::of(&cs.changes, sides.1.ids) else {
            return;
        };
        let owners = PlanOwners::new(names, sides.0.ids, sides.1.ids);
        let operations = Operations::of_plan(&cs.changes, ModuleEdits::Rebuilt);
        let before = Snapshot {
            schema: sides.0.schema,
            ids: sides.0.ids,
        };
        let after = Snapshot {
            schema: sides.1.schema,
            ids: sides.1.ids,
        };
        let effects = analyse_with_catalog(&operations, &owners, before, after, dialect, None);
        if !effects.unowned.is_empty() {
            return;
        }
        let graph = graph(
            operations.as_slice(),
            &effects.effects,
            &Baseline::of(before, &owners),
            &pbps_model::ModuleDeps::new(),
            Checks::ALL,
        );
        let effects = Schedule::of_graph(&graph, operations.as_slice());
        // A delayed claimant carries its consumers with it. Other steps
        // retain their relative order: this is not permission to reorder
        // the rest of the plan using an incomplete set of catalog facts.
        loop {
            let mut changed = false;
            for edge in effects.constraints() {
                if moving[edge.before] && !moving[edge.after] {
                    moving[edge.after] = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let mut previous = None;
        for (at, &moves) in moving.iter().enumerate() {
            if !moves {
                if let Some(previous) = previous {
                    schedule.keep(
                        previous,
                        at,
                        "only a catalog name's new holder and its dependents may move".into(),
                    );
                }
                previous = Some(at);
            }
        }
        for edge in effects.constraints() {
            schedule.edge(edge.before, edge.after, edge.source, edge.reason);
        }
        let Ok(ordered) = schedule.settle(cs.clone()) else {
            return;
        };
        settled = ordered;
    }
    if name_clashes(&settled.changes.changes, occupants, relations, catalog).len() < clashes.len() {
        *cs = settled.changes;
    }
}

/// Whether every step `sequence` moves past another, or another moves past,
/// is one `allowed` marks: an inverted pair of positions has both ends
/// marked. A step has passed another exactly where an earlier step of the
/// order came from a later position, or a later one from an earlier.
pub(super) fn only_crossed_by(sequence: &[usize], allowed: &[bool]) -> bool {
    let mut later_min = vec![usize::MAX; sequence.len() + 1];
    for (at, &from) in sequence.iter().enumerate().rev() {
        later_min[at] = later_min[at + 1].min(from);
    }
    let mut earlier_max: Option<usize> = None;
    for (at, &from) in sequence.iter().enumerate() {
        let crossed = earlier_max.is_some_and(|m| m > from) || later_min[at + 1] < from;
        if crossed && !allowed[from] {
            return false;
        }
        earlier_max = Some(earlier_max.map_or(from, |m| m.max(from)));
    }
    true
}

/// The occupants as name slots, for the handover edges and the slot replay
/// (#1964, #1957). The tables and views at the carried names are not
/// occupants: the plan manages them or refuses them already
/// (`refuse_uninventoried_occupants`).
pub(crate) fn occupant_slots(
    occupants: &[pbps_pg::catalog::NameOccupant],
) -> pbps_dialect::slots::CatalogFacts {
    use pbps_dialect::slots::{CatalogFacts, Namespace, Occupant, SlotKey};
    CatalogFacts {
        occupants: occupants
            .iter()
            .flat_map(|o| {
                let mut slots = vec![Occupant {
                    slot: SlotKey {
                        namespace: Namespace::Relations(o.name.schema.clone()),
                        name: o.name.name.clone(),
                    },
                    kind: o.kind.to_owned(),
                    owner: o.owner.clone(),
                    column: o.owner_column.clone(),
                }];
                if o.kind == "composite type" {
                    let mut row_type = slots[0].clone();
                    row_type.slot.namespace = Namespace::Types(o.name.schema.clone());
                    slots.push(row_type);
                }
                slots
            })
            .collect(),
        ..CatalogFacts::default()
    }
}

/// Names a detach certainly vacates or retains, not every index on its
/// table. An unnamed key keeps its engine name, and ambiguity keeps the DO
/// guard's existing refusal instead of inventing a release (DEC-1558.1).
pub(crate) fn detach_clone_slots(
    uid: &pbps_model::Uid,
    names: &[pbps_model::DetachedName],
    clones: &[pbps_pg::catalog::DetachClone],
) -> pbps_dialect::slots::CatalogFacts {
    use pbps_dialect::slots::{CatalogFacts, Namespace, SlotKey};
    let mut facts = CatalogFacts::default();
    for wanted in names {
        let Some(to) = &wanted.name else { continue };
        let matching: Vec<_> = clones
            .iter()
            .filter(|c| {
                c.kind == wanted.kind
                    && (c.kind == pbps_model::DetachedKind::PrimaryKey || c.parent == wanted.parent)
            })
            .collect();
        let [clone] = matching[..] else { continue };
        let slots = if clone.name.name != to.raw() {
            &mut facts.detach_releases
        } else {
            &mut facts.detach_retained
        };
        slots.insert(
            SlotKey {
                namespace: Namespace::Relations(clone.name.schema.clone()),
                name: clone.name.name.clone(),
            },
            uid.clone(),
        );
    }
    facts
}

/// The names a table rename takes as it runs: its target and, on its way
/// across schemas under a new name, its old name in the target schema, which
/// `SET SCHEMA` lands it on before `RENAME TO` (as `rename_order` claims).
pub(super) fn claims_of(from: &TableName, to: &TableName) -> Vec<TableName> {
    let mut out = vec![to.clone()];
    if from.schema != to.schema && from.name != to.name {
        out.push(TableName::new(&to.schema, &from.name));
    }
    out
}

/// The names this plan's table renames take as they run ([`claims_of`]), for
/// the catalog reads behind [`name_clashes`] (#1785 review).
pub(crate) fn rename_claims(cs: &pbps_model::ChangeSet) -> Vec<TableName> {
    let mut out: Vec<TableName> = cs
        .changes
        .iter()
        .filter_map(|p| {
            if let pbps_model::Change::RenameTable { from, to, .. } = &p.change {
                Some(claims_of(from, to))
            } else {
                None
            }
        })
        .flatten()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Each name [`refuse_uninventoried_occupants`] refuses, described, with the
/// position of the cross-schema rename whose carried object meets a holder,
/// or `None` for a name a change creates.
pub(super) fn name_clashes(
    changes: &[pbps_model::PlannedChange],
    occupants: &[pbps_pg::catalog::NameOccupant],
    relations: &[(TableName, &'static str)],
    catalog: &pbps_dialect::slots::CatalogFacts,
) -> Vec<(Option<usize>, String)> {
    let mut present = Occupant::with_detaches(occupants, catalog);
    let mut held: Vec<(TableName, String)> = relations
        .iter()
        .map(|(name, kind)| {
            (
                name.clone(),
                format!("{kind} `{name}` already is", name = name.shown()),
            )
        })
        .collect();
    let mut created: BTreeSet<TableName> = BTreeSet::new();
    let mut sequences = BTreeMap::<TableName, pbps_model::ColumnRef>::new();
    let mut taken: Vec<(Option<usize>, String)> = Vec::new();
    for (at, p) in changes.iter().enumerate() {
        present.retain(|o| !o.freed_by(&p.change));
        held.retain(|(name, _)| !relation_freed_by(&p.change, name));
        if let pbps_model::Change::RenameColumn {
            table, from, to, ..
        } = &p.change
        {
            for o in &mut present {
                o.column_renamed(table, from, to);
            }
        }
        if let pbps_model::Change::RenameTable { from, to, .. } = &p.change {
            let moved: Vec<usize> = (0..present.len())
                .filter(|&k| present[k].moved(from, to))
                .collect();
            for &k in &moved {
                let o = &present[k];
                // `SET SCHEMA` refuses to carry one onto a name the plan
                // already created there, or that another relation holds
                // there, one an earlier rename carried in included
                // (measured on 16 and 18, #1749).
                if created.contains(&o.name) {
                    taken.push((None, o.taken()));
                }
                let holder = present
                    .iter()
                    .enumerate()
                    .find(|&(j, q)| j != k && q.name == o.name);
                let there = match holder {
                    Some((_, q)) => Some(match &q.carried {
                        Some((f, t)) => format!(
                            "{} `{}`, which the rename of `{f}` to `{t}` moves there first",
                            q.found.kind,
                            q.found.name.shown(),
                            f = f.shown(),
                            t = t.shown()
                        ),
                        None => format!("{} `{}` already is", q.found.kind, q.name.shown()),
                    }),
                    None => held
                        .iter()
                        .find(|(name, _)| name == &o.name)
                        .map(|(_, there)| there.clone()),
                };
                if let Some(there) = there {
                    taken.push((
                        Some(at),
                        format!(
                            "`{}`: this plan moves {} `{}` on `{from}` there, with the rename of \
                             `{from}` to `{to}`, where {there}",
                            o.name.shown(),
                            o.found.kind,
                            o.found.name.shown(),
                            from = from.shown(),
                            to = to.shown()
                        ),
                    ));
                }
            }
            // What the table takes on its way, with what it carried there:
            // a name an occupant or another table or view still holds is
            // refused by the engine. The ordering by handovers relies on
            // this: an order that runs a rename before what frees its name
            // has a clash here (#1785 review).
            for name in claims_of(from, to) {
                let there = present
                    .iter()
                    .find(|q| q.name == name)
                    .map(|q| match &q.carried {
                        Some((f, t)) => format!(
                            "{} `{}`, which the rename of `{f}` to `{t}` moves there first",
                            q.found.kind,
                            q.found.name.shown(),
                            f = f.shown(),
                            t = t.shown()
                        ),
                        None => format!("{} `{}` already is", q.found.kind, q.name.shown()),
                    })
                    .or_else(|| {
                        held.iter()
                            .find(|(held, _)| held == &name)
                            .map(|(_, there)| there.clone())
                    });
                if let Some(there) = there {
                    let how = if &name == to {
                        String::new()
                    } else {
                        " on its way, `SET SCHEMA` before `RENAME TO`".to_owned()
                    };
                    taken.push((
                        Some(at),
                        format!(
                            "`{name}`: this plan renames `{from}` to `{to}`, which takes \
                             `{name}`{how}, where {there}",
                            name = name.shown(),
                            from = from.shown(),
                            to = to.shown()
                        ),
                    ));
                }
            }
            // The table itself holds its new name from here on.
            held.push((
                to.clone(),
                format!(
                    "table `{to}`, which the rename of `{from}` to `{to}` moves there first",
                    to = to.shown(),
                    from = from.shown()
                ),
            ));
        }
        for name in relation_names_created(&p.change) {
            if let Some(column) = sequences.get(&name) {
                taken.push((
                    None,
                    format!(
                        "relation `{}` this plan creates meets sequence `{}` for column `{}`",
                        name.shown(),
                        name.shown(),
                        column.shown()
                    ),
                ));
            }
            let detaching =
                if let pbps_model::Change::DetachPartition { uid, table, .. } = &p.change {
                    Some((uid, table))
                } else {
                    None
                };
            // Moving clones were released above. Only an exact retained
            // clone may still hold its batch's name: ownership alone also
            // includes sequences and indexes the detach never renames
            // (DEC-2230.1). The fact uses the original catalog identity even
            // after an earlier change moves its owner.
            taken.extend(
                present
                    .iter()
                    .filter(|o| {
                        use pbps_dialect::slots::{Namespace, SlotKey};
                        o.name == name
                            && !detaching.is_some_and(|(uid, table)| {
                                o.owner.as_ref() == Some(table)
                                    && catalog.detach_retained.get(&SlotKey {
                                        namespace: Namespace::Relations(
                                            o.found.name.schema.clone(),
                                        ),
                                        name: o.found.name.name.clone(),
                                    }) == Some(uid)
                            })
                    })
                    .map(|o| {
                        (
                            None,
                            match detaching {
                                Some((_, table)) => format!(
                                    "detach of `{}` claims `{}`: {}",
                                    table.shown(),
                                    name.shown(),
                                    o.taken()
                                ),
                                None => o.taken(),
                            },
                        )
                    }),
            );
            if let Some((_, table)) = detaching {
                taken.extend(
                    held.iter()
                        .filter(|(at, _)| at == &name)
                        .map(|(_, holder)| {
                            (
                                None,
                                format!(
                                    "detach of `{}` claims `{}` where {holder}",
                                    table.shown(),
                                    name.shown()
                                ),
                            )
                        }),
                );
            }
            created.insert(name);
        }
        for (name, column) in sequence_names_claimed(&p.change) {
            let claim = format!(
                "sequence `{}` for column `{}`",
                name.shown(),
                column.shown()
            );
            for occupant in present.iter().filter(|o| o.name == name) {
                taken.push((None, format!("{claim}: {}", occupant.taken())));
            }
            if let Some((_, holder)) = held.iter().find(|(at, _)| at == &name) {
                taken.push((
                    None,
                    format!("{claim} claims `{}` where {holder}", name.shown()),
                ));
            } else if created.contains(&name) {
                taken.push((
                    None,
                    format!(
                        "{claim} meets relation `{}` this plan creates",
                        name.shown()
                    ),
                ));
            }
            held.push((
                name.clone(),
                format!("{claim}, which this plan claims first"),
            ));
            sequences.insert(name.clone(), column);
            created.insert(name);
        }
    }
    taken
}

/// Whether `change` frees the table or view name `name` (#1765): its table
/// dropped or renamed away, or its view dropped.
// The complement is every change that frees no table or view name.
#[allow(clippy::wildcard_enum_match_arm)]
pub(super) fn relation_freed_by(change: &pbps_model::Change, name: &TableName) -> bool {
    use pbps_model::Change;
    match change {
        Change::DropTable { name: table, .. } | Change::RenameTable { from: table, .. } => {
            table == name
        }
        Change::DropModule {
            id: ModuleId::Named(view),
            ..
        } => view == name,
        _ => false,
    }
}

/// The names this plan's cross-schema renames carry `occupants` onto, in
/// their destination schemas (#1749). What holds one of them now is outside
/// what the plan creates and what the moved tables own, so the caller reads
/// it in a second pass before [`refuse_uninventoried_occupants`].
pub(crate) fn carried_names(
    cs: &pbps_model::ChangeSet,
    occupants: &[pbps_pg::catalog::NameOccupant],
) -> Vec<TableName> {
    let mut present = Occupant::all(occupants);
    let mut out = Vec::new();
    for p in &cs.changes {
        present.retain(|o| !o.freed_by(&p.change));
        if let pbps_model::Change::RenameTable { from, to, .. } = &p.change {
            for o in &mut present {
                if o.moved(from, to) {
                    out.push(o.name.clone());
                }
            }
        }
    }
    out
}

/// A [`pbps_pg::catalog::NameOccupant`] as the plan has left it so far: its
/// name and its owner's as they are at that point. A change matches it by
/// the name the change carries, so one emitted before a rename names the
/// table as it was, and one after it as it is (`rename_order` emits either,
/// #1723).
pub(super) struct Occupant<'a> {
    found: &'a pbps_pg::catalog::NameOccupant,
    name: TableName,
    owner: Option<TableName>,
    /// The column of its owner it belongs to, an owned sequence's, under
    /// the name the changes replayed so far give it (#1930 review).
    owner_column: Option<pbps_model::ColumnName>,
    /// The cross-schema rename that last carried it, by its two names.
    carried: Option<(TableName, TableName)>,
    detached_by: Option<pbps_model::Uid>,
}

impl<'a> Occupant<'a> {
    /// Each occupant as the catalog read it, before any change.
    fn all(occupants: &'a [pbps_pg::catalog::NameOccupant]) -> Vec<Self> {
        occupants
            .iter()
            .map(|found| Occupant {
                found,
                name: found.name.clone(),
                owner: found.owner.clone(),
                owner_column: found.owner_column.clone(),
                carried: None,
                detached_by: None,
            })
            .collect()
    }

    fn with_detaches(
        occupants: &'a [pbps_pg::catalog::NameOccupant],
        facts: &pbps_dialect::slots::CatalogFacts,
    ) -> Vec<Self> {
        use pbps_dialect::slots::{Namespace, SlotKey};
        let mut present = Self::all(occupants);
        for o in &mut present {
            o.detached_by = facts
                .detach_releases
                .get(&SlotKey {
                    namespace: Namespace::Relations(o.name.schema.clone()),
                    name: o.name.name.clone(),
                })
                .cloned();
        }
        present
    }

    /// Whether `change` takes it away: an index it drops, a unique
    /// constraint's or a primary key's it drops, and what a table or a
    /// column it drops owns. Anything else, a dropped column's indexes among
    /// them, is kept, which can refuse a plan the engine would take and
    /// never takes one it would refuse.
    // The complement is every change that frees no occupant.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn freed_by(&self, change: &pbps_model::Change) -> bool {
        use pbps_model::Change;
        let owned_by = |t: &TableName| self.owner.as_ref() == Some(t);
        let at = |t: &TableName, name: &str| self.name.schema == t.schema && self.name.name == name;
        match change {
            Change::DropIndex { table, name } | Change::DropUnique { table, name } => {
                at(table, name.raw())
            }
            // A connected plan reads each key with the name its index has,
            // a numbered `<table>_pkey1` included (measured, #1723). An
            // unnamed one's is named after its table as the catalog read it,
            // which a rename does not change (#1082 review).
            Change::SetPrimaryKey {
                table,
                from: Some(key),
                ..
            } => match &key.name {
                Some(name) => at(table, name.raw()),
                None => {
                    owned_by(table)
                        && self.found.owner.as_ref().is_some_and(|catalog| {
                            self.found.name.name
                                == pbps_pg::implicit_primary_key_name(&catalog.name)
                        })
                }
            },
            Change::DetachPartition { uid, table, .. } => {
                owned_by(table) && self.detached_by.as_ref() == Some(uid)
            }
            Change::DropTable { name, .. } => owned_by(name),
            Change::DropColumn { column, .. } => {
                owned_by(&column.table) && self.owner_column.as_ref() == Some(&column.name)
            }
            // A numbering change that drops its column's sequence, or renames
            // it, frees the name it had (#1930 review).
            Change::AlterColumnNumbering {
                column, from, to, ..
            } => {
                use pbps_model::ValueSource;
                let frees = match (from, to) {
                    (ValueSource::Identity(_), ValueSource::Identity(_)) => false,
                    (ValueSource::Sequence(was), ValueSource::Sequence(now)) => {
                        was.name.is_some() && now.name.is_some() && was.name != now.name
                    }
                    (ValueSource::Identity(_) | ValueSource::Sequence(_), _) => true,
                    (
                        ValueSource::None | ValueSource::Default(_) | ValueSource::Generated(_),
                        _,
                    ) => false,
                };
                frees && owned_by(&column.table) && self.owner_column.as_ref() == Some(&column.name)
            }
            _ => false,
        }
    }

    /// Follows the rename of its owning column.
    fn column_renamed(
        &mut self,
        table: &TableName,
        from: &pbps_model::ColumnName,
        to: &pbps_model::ColumnName,
    ) {
        if self.owner.as_ref() == Some(table) && self.owner_column.as_ref() == Some(from) {
            self.owner_column = Some(to.to_owned());
        }
    }

    /// Follows the rename of its owner: the owner takes the new name, and a
    /// move to another schema takes it along under its own name. Whether
    /// this rename carried it into another schema.
    fn moved(&mut self, from: &TableName, to: &TableName) -> bool {
        if self.owner.as_ref() != Some(from) {
            return false;
        }
        self.owner = Some(to.clone());
        if from.schema == to.schema {
            return false;
        }
        self.name.schema.clone_from(&to.schema);
        self.carried = Some((from.clone(), to.clone()));
        true
    }

    /// The refusal for a name it holds.
    fn taken(&self) -> String {
        let (name, kind, found) = (&self.name, self.found.kind, &self.found.name);
        match (&self.carried, &self.found.owner) {
            (Some((from, to)), _) => format!(
                "`{name}`: this plan moves {kind} `{found}` on `{from}` there first, with the \
                 rename of `{from}` to `{to}`",
                name = name.shown(),
                found = found.shown(),
                from = from.shown(),
                to = to.shown()
            ),
            (None, Some(owner)) => {
                format!(
                    "`{name}`: the database already has {kind} `{name}` on `{owner}`",
                    name = name.shown(),
                    owner = owner.shown()
                )
            }
            (None, None) => format!(
                "`{name}`: the database already has {kind} `{name}`",
                name = name.shown()
            ),
        }
    }
}

/// The partitions this plan creates `UNLOGGED`, each with its parent (#1595).
pub(super) fn unlogged_partitions_created(
    changes: &[pbps_model::PlannedChange],
) -> Vec<(usize, &TableName, &TableName)> {
    changes
        .iter()
        .enumerate()
        .filter_map(|(at, p)| {
            unlogged_partition(&p.change).map(|(partition, parent)| (at, partition, parent))
        })
        .collect()
}

/// The same statement-time parents the refusal compares, spelled for the
/// catalog read that supplies its referencers.
pub(crate) fn stored_unlogged_parents(
    names: &pbps_model::plan_names::PlanNames<'_>,
) -> Vec<TableName> {
    use pbps_model::plan_names::{At, Holder};
    unlogged_partitions_created(names.changes())
        .into_iter()
        .filter_map(
            |(at, _, parent)| match names.holder(parent, At::Before(at)) {
                Holder::Unmanaged => Some(parent.clone()),
                Holder::Tracked(_) | Holder::Vacant => names.stored(parent, At::Before(at)),
            },
        )
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The partition `change` leaves `UNLOGGED`, with its parent: one it creates
/// so, or a table it attaches that the plan leaves so, which is a new
/// partition under the parent's referencing keys all the same (#1545).
pub(crate) fn unlogged_partition(change: &pbps_model::Change) -> Option<(&TableName, &TableName)> {
    if let pbps_model::Change::CreateTable { name, table, .. } = change {
        return table
            .partition_of
            .as_ref()
            .filter(|_| table.unlogged)
            .map(|of| (name, &of.parent));
    }
    if let pbps_model::Change::AttachPartition {
        table,
        parent,
        shape,
        ..
    } = change
    {
        return shape.unlogged.then_some((table, parent));
    }
    None
}

/// Refuses an `UNLOGGED` partition this plan creates under a parent that a
/// permanent table's foreign key references (#1595, DEC-1595.1). The engine
/// takes it, and a crash would empty the partition under rows that still
/// reference it. A declared referencing table is refused before planning
/// (DEC-1580.1); this is the one the declarations do not hold, read from the
/// catalog. A key this plan takes away is not one: a table it drops or makes
/// unlogged, or a key it drops, anywhere in a transactional plan and before
/// the partition's `CREATE` in a staged one.
// The complement is every change that leaves a referencing key in place.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn refuse_permanent_referencers(
    cs: &pbps_model::ChangeSet,
    names: &pbps_model::plan_names::PlanNames<'_>,
    referencers: &[pbps_pg::catalog::PermanentReferencer],
    staged: bool,
) -> anyhow::Result<()> {
    use pbps_model::{
        Change,
        plan_names::{At, Holder},
    };
    // The catalog names each referencing table as the database does now, and
    // the plan's changes on a table it also renames carry the new name, so
    // each is read back to the catalog's before it is compared: by uid where
    // the change has one, since a rename's destination can be a name the
    // plan drops first, and that drop names the table that leaves (#1612
    // review). A key drop carries its execution address, which can precede
    // its owner's rename (DECISIONS 496); resolve it during the walk below.
    // Each removal with its place in the plan, and a whole table's with no
    // key. In a staged plan only one that runs before the partition's
    // `CREATE` counts: a staged apply commits between statements, and a
    // removal after it would leave the unlogged partition under a permanent
    // key until it ran. The persistence switch sorts after table creation, so
    // there making the referencing table unlogged takes a plan of its own
    // first. A transactional plan commits both or neither (#1612 review).
    let mut removals: Vec<(usize, TableName, Option<&str>)> = Vec::new();
    for (at, p) in cs.changes.iter().enumerate() {
        match &p.change {
            Change::DropTable { uid, .. } => {
                if let Some(table) = names.table_at(uid, At::Start) {
                    removals.push((at, table.clone(), None));
                }
            }
            Change::SetTablePersistence {
                uid,
                unlogged: true,
                ..
            } => {
                if let Some(table) = names.table_at(uid, At::Start) {
                    removals.push((at, table.clone(), None));
                }
            }
            Change::DropForeignKey { table, name } => {
                // Only earlier renames decide which stored table owns this
                // removal; a later name handoff must not change it (DEC-1770.1).
                let stored = match names.holder(table, At::Before(at)) {
                    Holder::Unmanaged => Some(table.clone()),
                    Holder::Tracked(_) | Holder::Vacant => names.stored(table, At::Before(at)),
                };
                if let Some(stored) = stored {
                    removals.push((at, stored, Some(name.raw())));
                }
            }
            _ => {}
        }
    }
    let removed_before = |at: usize, table: &TableName, key: &str| {
        removals
            .iter()
            .any(|(j, t, k)| (!staged || *j < at) && t == table && k.is_none_or(|k| k == key))
    };
    let mut lines = Vec::new();
    for (at, p) in cs.changes.iter().enumerate() {
        let Some((partition, parent)) = unlogged_partition(&p.change) else {
            continue;
        };
        // The parent too, which the plan can rename while it adds the
        // partition under the new name (#1613, #1690); the message names it
        // as declared.
        let in_catalog = match names.holder(parent, At::Before(at)) {
            Holder::Unmanaged => Some(parent.clone()),
            Holder::Tracked(_) | Holder::Vacant => names.stored(parent, At::Before(at)),
        };
        let Some(in_catalog) = in_catalog else {
            continue;
        };
        for r in referencers.iter().filter(|r| r.parent == in_catalog) {
            // A leaf's copy goes with the key its partitioned table declares,
            // which is the one a plan names.
            if removed_before(at, &r.table, &r.key)
                || removed_before(at, &r.root_table, &r.root_key)
            {
                continue;
            }
            let through = if r.table == r.root_table {
                String::new()
            } else {
                format!(" (on its partition {} as `{}`)", r.table.shown(), r.key)
            };
            lines.push(format!(
                "{partition} would be {} unlogged under {parent}, which {}'s foreign key \
                 `{}`{through} references; a crash would empty {partition} and leave rows of {} \
                 referencing nothing",
                if matches!(p.change, Change::AttachPartition { .. }) {
                    "attached"
                } else {
                    "created"
                },
                r.root_table.shown(),
                r.root_key,
                r.table.shown(),
                partition = partition.shown(),
                parent = parent.shown()
            ));
        }
    }
    if lines.is_empty() {
        return Ok(());
    }
    bail!(
        "{} unlogged partition(s) under a permanent table's foreign key:\n  {}\n\
         PostgreSQL refuses a permanent table's key to an unlogged table, but not through a \
         partitioned one. Declare the partition permanent, or first drop that key or make its \
         table unlogged (in a plan before this one, when this one is staged); a table the \
         declarations do not hold is the operator's to change (DEC-1595.1).",
        lines.len(),
        lines.join("\n  ")
    );
}

/// The table `change` makes unlogged, with its identity (#1489).
pub(crate) fn unlogged_table(
    change: &pbps_model::Change,
) -> Option<(&pbps_model::Uid, &TableName)> {
    if let pbps_model::Change::SetTablePersistence {
        uid,
        table,
        unlogged: true,
    } = change
    {
        Some((uid, table))
    } else {
        None
    }
}

/// Whether the plan leaves anything `UNLOGGED` that the two catalog checks
/// above read about: a partition it creates or attaches so, or a table it
/// makes so. The same selectors as [`stored_unlogged_parents`] and
/// [`tables_made_unlogged`], without the plan names they then spell the tables
/// by, which need the identities the catalog was read under.
pub(crate) fn leaves_something_unlogged(cs: &pbps_model::ChangeSet) -> bool {
    cs.changes
        .iter()
        .any(|p| unlogged_partition(&p.change).is_some() || unlogged_table(&p.change).is_some())
}

/// Each table this plan makes unlogged, as declared and as the catalog names
/// it when the plan starts, which is when the publications are read (#1489).
pub(crate) fn tables_made_unlogged(
    names: &pbps_model::plan_names::PlanNames<'_>,
) -> Vec<(TableName, TableName)> {
    let mut out = Vec::new();
    for p in names.changes() {
        if let Some((uid, table)) = unlogged_table(&p.change)
            && let Some(stored) = names.table_at(uid, pbps_model::plan_names::At::Start)
        {
            out.push((table.clone(), stored.clone()));
        }
    }
    out
}

/// Refuses a plan that makes a table unlogged while a publication names it
/// (#1489, DEC-1489.1). The engine refuses that `SET UNLOGGED` at apply; asked
/// here, the refusal names the publication before anything runs.
pub(crate) fn refuse_published_unlogged(
    made_unlogged: &[(TableName, TableName)],
    naming: &[pbps_pg::catalog::NamingPublication],
) -> anyhow::Result<()> {
    use pbps_model::escape::Escaped;
    let mut lines = Vec::new();
    for (declared, stored) in made_unlogged {
        for found in naming.iter().filter(|n| n.table == *stored) {
            // The statement is shown only where it runs exactly as shown: a
            // name whose display escaping would change the statement's
            // spelling (a backslash, a control character) gets the words
            // instead, since a quoted identifier takes those literally.
            let remove = found
                .drop_statement()
                .filter(|statement| Escaped(statement).to_string() == *statement)
                .map_or_else(
                    || "remove it from that publication".to_owned(),
                    |statement| format!("remove it there (`{statement}`)"),
                );
            lines.push(format!(
                "{} is listed by the publication `{}`; {remove}, or keep the table logged",
                declared.shown(),
                Escaped(&found.publication),
            ));
        }
    }
    if lines.is_empty() {
        return Ok(());
    }
    bail!(
        "{} table(s) this plan makes unlogged are published:\n  {}\n\
         PostgreSQL refuses `SET UNLOGGED` on a table a publication lists, because an \
         unlogged table cannot be replicated. A publication is outside the declarations, so \
         it is the operator's to change (DEC-1489.1).",
        lines.len(),
        lines.join("\n  ")
    );
}

/// The tables this plan moves to another schema, by the catalog's names for
/// them: `SET SCHEMA` carries their indexes and owned sequences along
/// (#1084), so the occupant read asks what each of them owns.
// The complement is every change that moves no table between schemas.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn transferred_tables(cs: &pbps_model::ChangeSet) -> Vec<TableName> {
    use pbps_model::Change;
    cs.changes
        .iter()
        .filter_map(|p| match &p.change {
            Change::RenameTable { from, to, .. } if from.schema != to.schema => Some(from.clone()),
            _ => None,
        })
        .collect()
}

/// The `sys.objects` names this plan creates on SQL Server: its tables, a
/// system-versioned table's history (#1176), and the modules it creates that
/// it does not also drop. A module dropped and created in one plan is a
/// rebuild, and its name is already its own. A trigger's object is in its
/// table's schema.
// The complement is every change that creates no object name.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn created_object_names(cs: &pbps_model::ChangeSet) -> Vec<TableName> {
    use pbps_model::Change;
    let dropped: BTreeSet<&ModuleId> = cs
        .changes
        .iter()
        .filter_map(|p| match &p.change {
            Change::DropModule { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    cs.changes
        .iter()
        .flat_map(|p| match &p.change {
            Change::CreateTable { name, table, .. } => {
                let history = table
                    .system_time
                    .as_ref()
                    .and_then(|st| st.versioning.as_ref())
                    .map(|v| v.history.clone());
                std::iter::once(name.clone()).chain(history).collect()
            }
            Change::CreateModule { id, .. } if !dropped.contains(id) => vec![module_object(id)],
            _ => Vec::new(),
        })
        .collect()
}

/// The `sys.objects` entry a module is, on SQL Server.
pub(super) fn module_object(id: &ModuleId) -> TableName {
    match id {
        ModuleId::Named(name) => name.clone(),
        ModuleId::Routine(routine) => routine.name.clone(),
        ModuleId::Trigger { on, name } => TableName::new(on.schema.clone(), name.clone()),
    }
}

/// What the SQL Server occupant read has to cover beyond the names the plan
/// creates: the constraints and generated defaults it adds, which the walk
/// checks like a created table (#1201) and which only this read can find
/// already held in the database (#1214); the names its renames may move a
/// generated default to (#975),
/// the generated one and the digested fallback taken when that is held
/// (DEC-981.1), and the tables whose constraints, defaults and triggers its
/// changes move or remove, by the catalog's names for them.
// The complement is every change that moves or removes no object.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn object_reads(
    cs: &pbps_model::ChangeSet,
    index: &pbps_model::plan_names::PlanNames<'_>,
) -> (Vec<TableName>, Vec<TableName>) {
    use pbps_model::Change;
    use pbps_mssql::emit::default_constraint_name as generated;
    use pbps_mssql::emit::fallback_default_constraint_name as fallback_name;
    let in_schema = |t: &TableName, name: pbps_model::ObjectLocalName| {
        TableName::new(t.schema.clone(), name.into_raw())
    };
    let mut names = created_object_names(cs);
    let mut parents = Vec::new();
    for (at, p) in cs.changes.iter().enumerate() {
        let now = |t: &TableName| {
            index
                .stored(t, pbps_model::plan_names::At::Before(at))
                .unwrap_or_else(|| t.clone())
        };
        match &p.change {
            Change::RenameTable {
                from, to, defaults, ..
            } => {
                parents.push(now(from));
                // The target itself, so the walk sees an object the project
                // does not record holding it (DEC-981.2).
                names.push(to.clone());
                // And the name a transfer gives it in the new schema before
                // `sp_rename` runs, which such an object can hold too (#1362).
                if from.schema != to.schema {
                    names.push(in_schema(to, from.name.clone().into()));
                }
                for c in defaults {
                    names.push(in_schema(to, generated(to, c.raw())));
                    names.push(in_schema(to, fallback_name(to, c.raw())));
                }
            }
            Change::RenameColumn { table, to, .. } => {
                parents.push(now(table));
                names.push(in_schema(table, generated(table, to.raw())));
                names.push(in_schema(table, fallback_name(table, to.raw())));
            }
            Change::DropTable { name, .. } => parents.push(now(name)),
            Change::DropColumn { column, .. } => parents.push(now(&column.table)),
            Change::AlterColumnDefault { column, to, .. } => {
                parents.push(now(&column.table));
                if to.is_some() {
                    names.push(in_schema(
                        &column.table,
                        generated(&column.table, column.name.raw()),
                    ));
                }
            }
            Change::DropUnique { table, .. }
            | Change::DropForeignKey { table, .. }
            | Change::DropCheck { table, .. } => parents.push(now(table)),
            Change::SetPrimaryKey { table, to, .. } => {
                parents.push(now(table));
                if let Some(name) = to
                    .as_ref()
                    .and_then(|k| k.name.as_ref().map(pbps_model::ObjectLocalName::raw))
                {
                    names.push(in_schema(table, name.into()));
                }
            }
            Change::AddUnique { table, name, .. }
            | Change::AddForeignKey { table, name, .. }
            | Change::AddCheck { table, name, .. } => {
                names.push(in_schema(table, name.clone()));
            }
            Change::AddColumn {
                table,
                name,
                column,
                ..
            } if (match column.value_source {
                pbps_model::ValueSource::Default(_) => true,
                pbps_model::ValueSource::None
                | pbps_model::ValueSource::Identity(_)
                | pbps_model::ValueSource::Sequence(_)
                | pbps_model::ValueSource::Generated(_) => false,
            }) =>
            {
                names.push(in_schema(table, generated(table, name.raw())))
            }
            Change::CreateTable { name, table, .. } => {
                for (column, spec) in &table.columns {
                    if match spec.value_source {
                        pbps_model::ValueSource::Default(_) => true,
                        pbps_model::ValueSource::None
                        | pbps_model::ValueSource::Identity(_)
                        | pbps_model::ValueSource::Sequence(_)
                        | pbps_model::ValueSource::Generated(_) => false,
                    } {
                        names.push(in_schema(name, generated(name, column.raw())));
                    }
                }
                let named = table
                    .primary_key
                    .iter()
                    .filter_map(|k| k.name.clone())
                    .chain(table.unique.keys().cloned())
                    .chain(table.foreign_keys.keys().cloned())
                    .chain(table.checks.keys().cloned());
                names.extend(named.map(|n| in_schema(name, n)));
            }
            _ => {}
        }
    }
    for list in [&mut names, &mut parents] {
        list.sort();
        list.dedup();
    }
    (names, parents)
}

/// Every name the walk may hold that the collation question has to see
/// (#1215): the names the plan reads (`object_reads`), the names the catalog
/// found, each renamed table's new name, each module it creates (a rebuilt
/// one too), and the destination of each occupant a cross-schema
/// `RenameTable` carries. The walk moves a carried constraint into the new schema under
/// its own name, so `archive.Ck_Name` arrives there without ever being read,
/// and a later `archive.ck_name` must still meet it (review of #1240).
// The complement is every change that moves no table between schemas.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn alike_candidates(
    cs: &pbps_model::ChangeSet,
    names: &[TableName],
    occupants: &[pbps_mssql::catalog::NameOccupant],
) -> Vec<TableName> {
    use pbps_model::Change;
    let mut out: Vec<TableName> = names.to_vec();
    out.extend(occupants.iter().map(|o| o.name.clone()));
    for p in &cs.changes {
        // A renamed table's new name, which the plan uses without reading,
        // and every module the plan creates, a rebuilt one included, which
        // `created_object_names` leaves out of the read (review of #1240).
        match &p.change {
            Change::RenameTable { to, .. } => out.push(to.clone()),
            Change::CreateModule { id, .. } => out.push(module_object(id)),
            _ => {}
        }
    }
    out.extend(carried_destinations(cs, occupants));
    out.sort();
    out.dedup();
    out
}

/// Where each cross-schema `RenameTable` carries the children the catalog
/// read found on its table: the same name, in the new schema. A transfer is
/// refused while another object holds one of them there (Msg 15530), so
/// each is read like a name the plan creates (#1366).
// The complement is every change that moves no table between schemas.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn carried_destinations(
    cs: &pbps_model::ChangeSet,
    occupants: &[pbps_mssql::catalog::NameOccupant],
) -> Vec<TableName> {
    use pbps_model::Change;
    let mut out = Vec::new();
    for p in &cs.changes {
        if let Change::RenameTable { from, to, .. } = &p.change
            && from.schema != to.schema
        {
            out.extend(
                occupants
                    .iter()
                    .filter(|o| o.parent.as_ref() == Some(from))
                    .map(|o| TableName::new(to.schema.clone(), o.name.name.clone())),
            );
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The history names a plan's `CREATE TABLE`s take that something in the
/// database still holds when that `CREATE` runs (#1176): each table, its
/// history, and the occupant. Ordered as the `sys.objects` walk orders the
/// plan, so an object the plan drops or renames away first frees the name, as
/// it does at plan time (#1501 review).
pub(super) fn blocking_history_occupants<'a>(
    cs: &pbps_model::ChangeSet,
    held: &'a [pbps_mssql::catalog::NameOccupant],
) -> Vec<(TableName, TableName, &'a pbps_mssql::catalog::NameOccupant)> {
    let histories: BTreeMap<&TableName, &TableName> = cs
        .changes
        .iter()
        .filter_map(|p| {
            if let pbps_model::Change::CreateTable { name, table, .. } = &p.change {
                let v = table.system_time.as_ref()?.versioning.as_ref()?;
                Some((&v.history, name))
            } else {
                None
            }
        })
        .collect();
    held.iter()
        .filter_map(|o| {
            let table = histories.get(&o.wanted)?;
            let facts = NameFacts::new(std::slice::from_ref(o), &[]);
            let mut walk = Walk::new(&facts);
            walk.run(&cs.changes);
            (!walk.is_clear()).then(|| ((*table).clone(), o.wanted.clone(), o))
        })
        .collect()
}

/// Refuses a plan whose `CREATE TABLE` names, as a system-versioned table's
/// history, a name the database still holds when it runs (#1176). Not left
/// to the engine: given an existing table whose columns match, SQL Server
/// takes it as the history, rows, layout and all, where any other `CREATE`
/// would be refused (measured on 17.0).
pub(super) async fn refuse_taken_history_names(
    conn: &mut Conn,
    cs: &pbps_model::ChangeSet,
    label: &str,
) -> anyhow::Result<()> {
    let names: Vec<TableName> = cs
        .changes
        .iter()
        .filter_map(|p| {
            if let pbps_model::Change::CreateTable { table, .. } = &p.change {
                Some(
                    table
                        .system_time
                        .as_ref()?
                        .versioning
                        .as_ref()?
                        .history
                        .clone(),
                )
            } else {
                None
            }
        })
        .collect();
    if names.is_empty() {
        return Ok(());
    }
    let Some(held) = crate::engine::history_name_occupants(conn, &names).await? else {
        return Ok(());
    };
    let taken: Vec<String> = blocking_history_occupants(cs, &held)
        .into_iter()
        .map(|(table, history, o)| {
            format!(
                "{table} names {history} as its history table, and the database already has {} \
                 `{}`",
                o.kind,
                o.name.shown(),
                table = table.shown(),
                history = history.shown()
            )
        })
        .collect();
    if taken.is_empty() {
        return Ok(());
    }
    bail!(
        "`{label}` already holds {} history table name(s) this plan would create:\n  {}\n\
         SQL Server would take an existing table whose columns match as the history, with its \
         rows, rather than refuse. Drop or rename that object, or name another history table.",
        taken.len(),
        taken.join("\n  "),
        label = pbps_model::escape::Escaped(label),
    );
}

/// Refuses, before anything runs, a role name another database principal
/// holds: users, roles and application roles share one namespace, and a
/// `CREATE ROLE` or `ALTER ROLE ... WITH NAME` onto a taken name fails
/// after everything ordered before it has run (DECISIONS 118).
///
/// `wanted` are the names the plan's remaining statements create or rename
/// to; `vacated` the ones they drop or rename away, which are free for
/// this purpose. The engine decides which names are the same, under the
/// database's collation — `Shadow` is `shadow` to most databases and not
/// to a string comparison here (DECISIONS 119).
pub(super) async fn refuse_taken_role_names(
    conn: &mut Conn,
    wanted: &[pbps_model::RoleName],
    vacated: &[pbps_model::RoleName],
) -> anyhow::Result<()> {
    if wanted.is_empty() {
        return Ok(());
    }
    let wanted: Vec<&str> = wanted.iter().map(pbps_model::RoleName::raw).collect();
    let vacated: Vec<&str> = vacated.iter().map(pbps_model::RoleName::raw).collect();
    // The wanted names against one another first: two declared roles the
    // database reads as one name pass every check against the catalog, and
    // the second `CREATE ROLE` fails after everything before it has run
    // (DECISIONS 123).
    let alike = crate::engine::names_alike(conn, &wanted)
        .await
        .context("cannot compare the declared role names")?;
    if !alike.is_empty() {
        let pairs: Vec<String> = alike
            .iter()
            .map(|(earlier, later)| format!("`{earlier}` and `{later}`"))
            .collect();
        bail!(
            "two declared roles are one name to this database: {}.\n\
             Names are compared the way this database compares them, and the second \
             `CREATE ROLE` or `ALTER ROLE ... WITH NAME` would be refused after everything \
             before it had run. Rename one of them.",
            pairs.join(", ")
        );
    }
    let held = crate::engine::principals_holding(conn, &wanted, &vacated)
        .await
        .context("cannot read the database principals")?;
    if held.is_empty() {
        return Ok(());
    }
    let taken: Vec<String> = held
        .iter()
        .map(|(declared, held, kind)| {
            if declared == held {
                format!("`{declared}` is a {kind}")
            } else {
                format!("`{declared}` is `{held}` to this database, a {kind}")
            }
        })
        .collect();
    bail!(
        "a declared role's name is already held in this database: {}.\n\
         Users, roles and application roles share one namespace, compared the way this \
         database compares names, and `CREATE ROLE` or `ALTER ROLE ... WITH NAME` would be \
         refused after everything before it had run. Rename the role, or rename or drop the \
         principal by hand.",
        taken.join(", ")
    );
}
