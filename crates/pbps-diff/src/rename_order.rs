//! Order table renames against the particular drops that release their names.
//!
//! The ordinary classes still order the rest of the plan. This small graph
//! runs after module drops and before other table work; it cannot move a column,
//! row, grant or module across its existing boundary (DECISIONS 496).
//!
//! Which drops release a name a table can take is the dialect's: an index or
//! the index behind a key where indexes share the table namespace
//! (PostgreSQL), and every named constraint where constraints do (SQL Server,
//! DEC-496.1). Measured on 17.0.4075.5: `sp_rename 'dbo.old', 'c'` is refused
//! (Msg 15335) while another table still has a check `c`, and succeeds once the
//! check is dropped.
//!
//! A dropped table releases its own name on every dialect, and with it the
//! names it carries. It runs after the foreign-key drops that name it or that
//! it owns (DEC-536.1): measured, `sp_rename` into a doomed table's name is Msg
//! 15335 and `ALTER TABLE … RENAME` is `42P07`.

use std::collections::{BTreeMap, BTreeSet};

use pbps_dialect::Dialect;
use pbps_model::{Change, PlannedChange, Renames, TableName};

use crate::Side;

pub(crate) fn order(
    planned: &mut Vec<PlannedChange>,
    base: Side<'_>,
    renames: &Renames,
    dialect: &dyn Dialect,
) {
    let indexes = dialect.indexes_share_namespace_with_tables();
    let constraints = dialect.constraints_share_namespace_with_tables();
    // No early return when neither answer is yes: a dropped table releases
    // its own name regardless of what else shares the namespace.
    let mut owners = BTreeMap::new();
    let mut claims: BTreeMap<TableName, BTreeSet<usize>> = BTreeMap::new();
    // A child the plan drops is gone before its table moves: the move neither
    // carries it into the destination nor releases it from the source; its
    // drop does that (review of #1346). The drop may name the table by either
    // name, so both are asked.
    let dropped_children: BTreeSet<(&TableName, &str)> = planned
        .iter()
        .filter_map(|p| dropped_relation(&p.change, indexes, constraints))
        .collect();
    let dropped_from = |from: &TableName, to: &TableName, name: &str| {
        dropped_children.contains(&(from, name)) || dropped_children.contains(&(to, name))
    };
    // Names the engine generates, of which a table holds one of several
    // alternatives: they order only as weakly as a case-folded name, so an
    // alternative the table does not hold never displaces a real edge
    // (review of #1346).
    let mut weak_claims: BTreeMap<TableName, BTreeSet<usize>> = BTreeMap::new();
    for (i, p) in planned.iter().enumerate() {
        if let Change::RenameTable { from, to, .. } = &p.change {
            owners.insert(to.clone(), (i, from.clone()));
            claims.entry(to.clone()).or_default().insert(i);
            // SET SCHEMA precedes RENAME TO in the PostgreSQL emitter.
            if from.schema != to.schema {
                claims
                    .entry(TableName::new(to.schema.clone(), from.name.clone()))
                    .or_default()
                    .insert(i);
                // And the move carries the table's own named indexes or
                // constraints into the destination, where a same-named one on
                // another table has to be dropped first (review of #969).
                if let Some(moving) = base.schema.tables.get(from) {
                    for carried in carried_names(moving, indexes, constraints)
                        .into_iter()
                        .filter(|carried| !dropped_from(from, to, carried))
                    {
                        claims
                            .entry(TableName::new(to.schema.clone(), carried))
                            .or_default()
                            .insert(i);
                    }
                    for generated in generated_names(dialect, from, moving, constraints) {
                        weak_claims
                            .entry(TableName::new(to.schema.clone(), generated))
                            .or_default()
                            .insert(i);
                    }
                }
            }
            // Any rename then gives the generated defaults the names its new
            // table name generates, as SQL Server's emitter does.
            if let Some(moving) = base.schema.tables.get(from) {
                for generated in generated_names(dialect, to, moving, constraints) {
                    weak_claims
                        .entry(TableName::new(to.schema.clone(), generated))
                        .or_default()
                        .insert(i);
                }
            }
        }
    }
    if owners.is_empty() {
        return;
    }
    let original_name = |table: &TableName| {
        owners
            .get(table)
            .map_or_else(|| table.clone(), |(_, from)| from.clone())
    };
    let before = |table: &TableName| {
        let original = original_name(table);
        base.schema
            .tables
            .get(&original)
            .map(|t| renames.apply(t, &original))
    };
    let mut edges = vec![BTreeSet::new(); planned.len()];
    let mut drops = BTreeSet::new();
    let mut keys = Vec::new();
    // A dropped table's own foreign-key drops carry its name, which a rename
    // into that name also carries once it has run. They are addressed by the
    // doomed table and never rekeyed. Where both tables have a key of one
    // name (PostgreSQL), the two changes are identical, and the first is
    // taken as the doomed table's.
    let mut own = BTreeSet::new();
    for p in planned.iter() {
        if let Change::DropTable { name, .. } = &p.change
            && let Some(t) = base.schema.tables.get(name)
        {
            let mut remaining: BTreeSet<&String> = t.foreign_keys.keys().collect();
            for (j, q) in planned.iter().enumerate() {
                if let Change::DropForeignKey { table, name: fk } = &q.change
                    && table == name
                    && remaining.remove(fk)
                {
                    own.insert(j);
                }
            }
        }
    }
    let mut doomed = Vec::new();
    let fold = |claims: &BTreeMap<TableName, BTreeSet<usize>>| {
        let mut folded: Folded = BTreeMap::new();
        for (claimed, waiting) in claims {
            folded
                .entry((case_folded(&claimed.schema), case_folded(&claimed.name)))
                .or_default()
                .extend(waiting);
        }
        folded
    };
    let claims_folded = fold(&claims);
    let weak_folded = fold(&weak_claims);
    // Two tiers below the exact edges, each placed only where it closes no
    // cycle: a case-folded match of a declared name, then anything through a
    // generated name, one alternative of several (DEC-981.3).
    let mut folded_edges: Vec<(usize, usize)> = Vec::new();
    let mut generated_edges: Vec<(usize, usize)> = Vec::new();
    for (i, p) in planned.iter().enumerate() {
        if let Change::DropTable { name, .. } = &p.change {
            let mut released = vec![(name.name.clone(), true)];
            if let Some(t) = base.schema.tables.get(name) {
                released.extend(
                    carried_names(t, indexes, constraints)
                        .into_iter()
                        .map(|n| (n, true)),
                );
                released.extend(
                    generated_names(dialect, name, t, constraints)
                        .into_iter()
                        .map(|n| (n, false)),
                );
            }
            for (one, strong) in released {
                let exact = claims
                    .get(&TableName::new(name.schema.clone(), one.clone()))
                    .filter(|_| strong);
                if let Some(waiting) = exact {
                    drops.insert(i);
                    edges[i].extend(waiting);
                }
                // A claim spelled only differently in case: one name to a
                // case-insensitive collation, so the drop goes first there
                // too. Added last, and only where it closes no cycle
                // (DEC-981.3).
                let folded = (case_folded(&name.schema), case_folded(&one));
                let (declared, generated) =
                    weak_claimants(&folded, strong, exact, &claims_folded, &weak_folded);
                for claimant in declared {
                    drops.insert(i);
                    folded_edges.push((i, claimant));
                }
                for claimant in generated {
                    drops.insert(i);
                    generated_edges.push((i, claimant));
                }
            }
            if drops.contains(&i) {
                doomed.push((i, name.clone()));
            }
            continue;
        }
        if own.contains(&i) {
            continue;
        }
        let Some((table, name)) = dropped_relation(&p.change, indexes, constraints) else {
            continue;
        };
        // A carried index may occupy the source schema now and the destination
        // schema after its owner's rename. Either can block a claimant.
        let source = original_name(table);
        for schema in [&table.schema, &source.schema] {
            let exact = claims.get(&TableName::new(schema.clone(), name));
            if let Some(waiting) = exact {
                drops.insert(i);
                edges[i].extend(waiting);
            }
            // A claim differing only in case, as for a dropped table (DEC-981.3).
            let (declared, generated) = weak_claimants(
                &(case_folded(schema), case_folded(name)),
                true,
                exact,
                &claims_folded,
                &weak_folded,
            );
            for claimant in declared {
                drops.insert(i);
                folded_edges.push((i, claimant));
            }
            for claimant in generated {
                drops.insert(i);
                generated_edges.push((i, claimant));
            }
        }
        // A table moved to another schema carries its indexes (PostgreSQL's
        // SET SCHEMA) or constraints (SQL Server's TRANSFER) into it, where a
        // name another object holds refuses the whole move. Measured on
        // 17.0.4075.5: `ALTER SCHEMA s2 TRANSFER s1.old` is refused (Msg 15530)
        // while `old` has a check `c` and `s2.c` is a table, and succeeds once
        // the check is dropped. One this plan drops anyway has nothing to
        // carry, so it goes first, addressed by the source name (review of
        // #969).
        if let Some((rename, _)) = owners.get(table)
            && source.schema != table.schema
        {
            drops.insert(i);
            edges[i].insert(*rename);
        }
        if drops.contains(&i)
            && let Some(t) = before(table)
        {
            let columns = if let Change::DropUnique { name, .. } = &p.change {
                t.unique.get(name).map(|u| u.columns.clone())
            } else if let Change::SetPrimaryKey { from: Some(pk), .. } = &p.change {
                Some(pk.columns.clone())
            } else if let Change::DropIndex { name, .. } = &p.change {
                t.indexes
                    .get(name)
                    .filter(|index| index.unique && index.filter.is_none())
                    .and_then(|index| index.column_keys())
            } else {
                None
            };
            if let Some(columns) = columns {
                keys.push((
                    i,
                    table.clone(),
                    columns.into_iter().collect::<BTreeSet<_>>(),
                ));
            }
        }
    }
    // A rename releases its source name as it runs, so a rename into that
    // name runs after it: skipped revisions that rename `z` to `a` and then
    // `y` to `z` leave a chain the alphabet would otherwise order (DEC-536.1).
    // A cycle, two tables trading names, has no order an engine takes without
    // a third name; its edge is left out and the engine refuses it as before.
    // A move to another schema also passes through an intermediate name, its
    // source name in the destination schema, which it takes and then gives
    // up; a rename into that name waits for it too. And it takes the indexes
    // or constraints it carries out of the source schema, so a rename into
    // one of their names there waits for it as well (review of #1346).
    let mut chained = false;
    for (to, (rename, from)) in &owners {
        let mut released = vec![(from.clone(), true)];
        if from.schema != to.schema {
            released.push((TableName::new(to.schema.clone(), from.name.clone()), true));
            if let Some(moving) = base.schema.tables.get(from) {
                released.extend(
                    carried_names(moving, indexes, constraints)
                        .into_iter()
                        .filter(|carried| !dropped_from(from, to, carried))
                        .map(|carried| (TableName::new(from.schema.clone(), carried), true)),
                );
            }
        }
        // A generated default leaves its name in the source schema on any
        // rename: it is carried away, renamed for the new table name, or both.
        if let Some(moving) = base.schema.tables.get(from) {
            released.extend(
                generated_names(dialect, from, moving, constraints)
                    .into_iter()
                    .map(|generated| (TableName::new(from.schema.clone(), generated), false)),
            );
        }
        for (name, strong) in &released {
            let exact = claims.get(name).filter(|_| *strong);
            for &claimant in exact.into_iter().flatten() {
                if claimant != *rename && !reaches(&edges, claimant, *rename) {
                    edges[*rename].insert(claimant);
                    chained = true;
                }
            }
            // And a rename into the name spelled differently in case, which a
            // case-insensitive collation reads as the one this rename vacates
            // (DEC-981.3): `b -> c`, then `a -> B`.
            let (declared, generated) = weak_claimants(
                &(case_folded(&name.schema), case_folded(&name.name)),
                *strong,
                exact,
                &claims_folded,
                &weak_folded,
            );
            for claimant in declared.into_iter().filter(|c| c != rename) {
                folded_edges.push((*rename, claimant));
                chained = true;
            }
            for claimant in generated.into_iter().filter(|c| c != rename) {
                generated_edges.push((*rename, claimant));
                chained = true;
            }
        }
    }
    if drops.is_empty() && !chained {
        return;
    }
    // Before the doomed table goes: its own keys, and every key on another
    // table that references it. The referenced name is read from the
    // baseline, where a table renamed into the doomed name is still its source.
    let mut fixed = BTreeSet::new();
    for (drop, doomed) in &doomed {
        for (j, p) in planned.iter().enumerate() {
            let Change::DropForeignKey { table, name } = &p.change else {
                continue;
            };
            let references = if own.contains(&j) {
                // A dropped table's own key, under its baseline name: the
                // doomed table's, or another dropped table's that references
                // it. Either keeps its address.
                let names_it = base
                    .schema
                    .tables
                    .get(table)
                    .and_then(|t| t.foreign_keys.get(name))
                    .is_some_and(|fk| &fk.references_table == doomed);
                if table != doomed && !names_it {
                    continue;
                }
                fixed.insert(j);
                true
            } else {
                base.schema
                    .tables
                    .get(&original_name(table))
                    .and_then(|t| t.foreign_keys.get(name))
                    .is_some_and(|fk| &fk.references_table == doomed)
            };
            if references {
                drops.insert(j);
                edges[j].insert(*drop);
            }
        }
    }
    for (i, p) in planned.iter().enumerate() {
        let Change::DropForeignKey { table, name } = &p.change else {
            continue;
        };
        // A dropped table's own key is read under its baseline name, which a
        // rename into that name would otherwise redirect to the rename's
        // source; it still has to go before a key it references is dropped.
        let own_key = own.contains(&i);
        let t = if own_key {
            base.schema
                .tables
                .get(table)
                .map(|t| renames.apply(t, table))
        } else {
            before(table)
        };
        if let Some(t) = t
            && let Some(fk) = t.foreign_keys.get(name)
        {
            // Catalog binding is unavailable to this pure differ. A matching
            // column set may back the FK even if another equivalent key stays.
            let columns = fk
                .references_columns
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            for (key, parent, key_columns) in &keys {
                if &fk.references_table == parent && &columns == key_columns {
                    drops.insert(i);
                    edges[i].insert(*key);
                    if own_key {
                        fixed.insert(i);
                    }
                }
            }
        }
    }
    let mut selected = drops.clone();
    selected.extend(owners.values().map(|(i, _)| *i));
    for &drop in &drops {
        if fixed.contains(&drop) || matches!(planned[drop].change, Change::DropTable { .. }) {
            continue;
        }
        let table = planned[drop].change.table().expect("a relation or FK drop");
        if let Some((rename, _)) = owners.get(table) {
            // Prefer the declared spelling, allowing a drop between two
            // specific renames. If its own rename needs this drop first, use
            // the source spelling instead; adding the reverse edge would
            // create a cycle. The prerequisite graph starts FK -> key ->
            // rename, and each added edge is checked, so it remains acyclic.
            if !reaches(&edges, drop, *rename) {
                edges[*rename].insert(drop);
            }
        }
    }
    for (drop, claimant) in folded_edges.into_iter().chain(generated_edges) {
        if !reaches(&edges, claimant, drop) {
            edges[drop].insert(claimant);
        }
    }
    let mut remaining = selected.clone();
    let mut ordered = Vec::new();
    while !remaining.is_empty() {
        // Original stable sort position is the tie-breaker among ready nodes.
        let next = *remaining
            .iter()
            .find(|next| !remaining.iter().any(|i| edges[*i].contains(next)))
            .expect("only acyclic owner edges were added");
        remaining.remove(&next);
        ordered.push(next);
    }
    let positions: BTreeMap<_, _> = ordered.iter().enumerate().map(|(i, n)| (*n, i)).collect();
    for &drop in &drops {
        if fixed.contains(&drop) {
            continue;
        }
        let table = planned[drop].change.table().expect("a relation or FK drop");
        if let Some((rename, source)) = owners.get(table)
            && positions[&drop] < positions[rename]
        {
            // The address is part of the typed, checksummed change, not a
            // hidden emitter rewrite. Declared::advance also consumes this
            // execution order and removes an old filter before rekeying it.
            if let Change::DropIndex { table, .. }
            | Change::DropUnique { table, .. }
            | Change::DropForeignKey { table, .. }
            | Change::DropCheck { table, .. }
            | Change::SetPrimaryKey {
                table, to: None, ..
            } = &mut planned[drop].change
            {
                *table = source.clone();
            }
        }
    }
    let first = *selected.first().expect("there is a freeing drop");
    let mut pending: Vec<_> = std::mem::take(planned).into_iter().map(Some).collect();
    for i in 0..pending.len() {
        if i == first {
            for &n in &ordered {
                planned.push(pending[n].take().expect("selected once"));
            }
        }
        if !selected.contains(&i) {
            planned.push(pending[i].take().expect("not selected"));
        }
    }
}

/// The names a table takes into its schema's table namespace besides its own:
/// the same kinds [`dropped_relation`] releases, by the dialect's two answers.
/// Claims keyed by their case-folded schema and name.
type Folded = BTreeMap<(String, String), BTreeSet<usize>>;

/// The claimants of a released name beyond its exact claims, split by how
/// weakly they order (DEC-981.3): a case-folded match of a declared name, and
/// anything through a generated name, which is one alternative of several.
/// A generated release (`strong` false) puts every claimant in the second.
fn weak_claimants(
    key: &(String, String),
    strong: bool,
    exact: Option<&BTreeSet<usize>>,
    declared: &Folded,
    generated: &Folded,
) -> (Vec<usize>, Vec<usize>) {
    let not_exact = |c: &usize| !exact.is_some_and(|w| w.contains(c));
    let by_declared: Vec<usize> = declared
        .get(key)
        .into_iter()
        .flatten()
        .copied()
        .filter(not_exact)
        .collect();
    let mut by_generated: Vec<usize> = generated
        .get(key)
        .into_iter()
        .flatten()
        .copied()
        .filter(|c| not_exact(c) && !by_declared.contains(c))
        .collect();
    if strong {
        (by_declared, by_generated)
    } else {
        by_generated.extend(by_declared);
        (Vec::new(), by_generated)
    }
}

fn carried_names(
    table: &pbps_model::schema::Table,
    indexes: bool,
    constraints: bool,
) -> Vec<String> {
    let mut names: Vec<String> = table.unique.keys().cloned().collect();
    names.extend(table.primary_key.as_ref().and_then(|pk| pk.name.clone()));
    if indexes {
        names.extend(table.indexes.keys().cloned());
    }
    if constraints {
        names.extend(table.checks.keys().cloned());
        names.extend(table.foreign_keys.keys().cloned());
    }
    names
}

/// The constraint names the engine generates for `table`, where constraints
/// share the namespace. Each is one of the alternatives the table may hold,
/// so an edge through one is weak (review of #1346).
fn generated_names(
    dialect: &dyn Dialect,
    name: &TableName,
    table: &pbps_model::schema::Table,
    constraints: bool,
) -> Vec<String> {
    if constraints {
        dialect.generated_constraint_names(name, table)
    } else {
        Vec::new()
    }
}

/// The table and name a change releases in its schema's table namespace, if
/// any. `indexes` and `constraints` are the dialect's two answers: a key's name
/// is released under either, an index's only where indexes share the namespace,
/// and a check's or a foreign key's only where constraints do.
fn dropped_relation(
    change: &Change,
    indexes: bool,
    constraints: bool,
) -> Option<(&TableName, &str)> {
    if let Change::DropUnique { table, name } = change {
        return Some((table, name));
    }
    if let Change::DropIndex { table, name } = change
        && indexes
    {
        return Some((table, name));
    }
    if let Change::DropCheck { table, name } | Change::DropForeignKey { table, name } = change
        && constraints
    {
        return Some((table, name));
    }
    if let Change::SetPrimaryKey {
        table,
        from: Some(pk),
        to: None,
        ..
    } = change
    {
        return pk.name.as_deref().map(|name| (table, name));
    }
    None
}

/// A name as a case-insensitive collation may read it: each character
/// lower-cased where that is one character, as `pbps_model::module` folds for
/// its scans. Used only to *add* an ordering edge that cannot close a cycle,
/// never to refuse or to merge names (DEC-981.3): which spellings are one name
/// is the target database's to say, and a plan is computed offline (SPEC 7.3),
/// so a fold that is wrong for a case-sensitive database costs an order the
/// plan did not need and nothing else.
pub(crate) fn case_folded(s: &str) -> String {
    s.chars()
        .map(|c| {
            let mut lower = c.to_lowercase();
            match (lower.next(), lower.next()) {
                (Some(one), None) => one,
                _ => c,
            }
        })
        .collect()
}

fn reaches(edges: &[BTreeSet<usize>], from: usize, target: usize) -> bool {
    let mut pending = vec![from];
    let mut seen = BTreeSet::new();
    while let Some(i) = pending.pop() {
        if i == target {
            return true;
        }
        if seen.insert(i) {
            pending.extend(&edges[i]);
        }
    }
    false
}
