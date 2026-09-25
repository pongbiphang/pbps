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
    if !indexes && !constraints {
        return;
    }
    let mut owners = BTreeMap::new();
    let mut claims: BTreeMap<TableName, BTreeSet<usize>> = BTreeMap::new();
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
                    for carried in carried_names(moving, indexes, constraints) {
                        claims
                            .entry(TableName::new(to.schema.clone(), carried))
                            .or_default()
                            .insert(i);
                    }
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
    for (i, p) in planned.iter().enumerate() {
        let Some((table, name)) = dropped_relation(&p.change, indexes, constraints) else {
            continue;
        };
        // A carried index may occupy the source schema now and the destination
        // schema after its owner's rename. Either can block a claimant.
        let source = original_name(table);
        for schema in [&table.schema, &source.schema] {
            if let Some(waiting) = claims.get(&TableName::new(schema.clone(), name)) {
                drops.insert(i);
                edges[i].extend(waiting);
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
                    .map(|index| index.columns.iter().map(|c| c.name.clone()).collect())
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
    if drops.is_empty() {
        return;
    }
    for (i, p) in planned.iter().enumerate() {
        if let Change::DropForeignKey { table, name } = &p.change
            && let Some(t) = before(table)
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
                }
            }
        }
    }
    let mut selected = drops.clone();
    selected.extend(owners.values().map(|(i, _)| *i));
    for &drop in &drops {
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
    } = change
    {
        return pk.name.as_deref().map(|name| (table, name));
    }
    None
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
