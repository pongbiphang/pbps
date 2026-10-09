//! The order of a SQL Server plan's table renames, from what the target's
//! catalog holds (#1366).
//!
//! The differ orders renames and the drops that free their names from the
//! declarations alone (`pbps_diff::rename_order`, DEC-981.3). It cannot see
//! what only the target says: a default adopted under a hand-chosen name,
//! which of its generated names a default holds, an object the project does
//! not record, or which spellings the target's collation reads as one name.
//! The namespace walk (`deploy::Walk`) simulates exactly those, from the
//! catalog read, but on its own it can only refuse. So where the walk refuses
//! the differ's order, the other orders of the same changes are walked, and
//! the first one it clears is the plan's.
//!
//! What moves is bounded by the classes DECISIONS 496 keeps:
//! - the table renames move among the drops of classes 2 and 6, and among
//!   one another;
//! - the drops keep their own order, which already puts a foreign key's drop
//!   before the key or the table it references (DEC-536.1). A drop claims no
//!   name, so no order a rename needs is one the drops have to give up;
//! - a module drop the computed edges ordered (DEC-1431.1), among the drops
//!   or ahead of them, moves like a rename where a rename can claim its
//!   name, but only between what the edges say it waits for (the drop that
//!   releases it) and what waits for it (a table it is schema-bound to,
//!   #1461): dropped early it frees a name a default then takes (#1680);
//! - every other change keeps its place, after the renames and drops. A name
//!   one of them frees is refused with that change named (DEC-1366.2).
//!
//! A drop of a renamed table's constraint names the table as it is called
//! where the drop runs: by its source name before the rename, and by its new
//! name after it. The address is part of the typed, checksummed change
//! (DECISIONS 496), so it is rewritten with the move.
//!
//! The search is depth first, over the plan's own order first, and a state
//! of the namespace reached twice is not walked twice. The order a default
//! rename takes depends on which names are free when it runs (DEC-981.1), so
//! a rename that can run is not always the one to run first: one that parks
//! a default at its fallback name can strand a later rename into that name.
//! Only a whole order the walk clears is kept.

use std::collections::{BTreeMap, BTreeSet};

use pbps_model::{Change, ChangeSet, PlannedChange, TableName};

use crate::deploy::{NameFacts, Walk};

/// The orders tried before the search stops and the plan is refused as one
/// whose order was not found. Each is one change run on a copy of the
/// namespace; the plans this tool meets need tens.
const TRIALS: usize = 20_000;

/// Orders `cs`'s table renames so that every name a change claims is free
/// when it runs, over what the catalog read found (`occupants`) and which
/// names the database reads as one (`alike`); refuses the plan where no
/// order does. A plan the walk clears in its own order is left as it is.
pub(crate) fn order_occupied_objects_under(
    cs: &mut ChangeSet,
    occupants: &[pbps_mssql::catalog::NameOccupant],
    alike: &[(TableName, TableName)],
    label: &str,
    precedence: &[(Change, Change)],
) -> anyhow::Result<()> {
    let facts = NameFacts::new(occupants, alike);
    let mut walk = Walk::new(&facts);
    walk.run(&cs.changes);
    if walk.is_clear() {
        return Ok(());
    }
    let mut search = Search::new(&cs.changes, precedence, &facts);
    let Some(order) = search.run(&facts) else {
        let refusal = walk.refusal(&cs.changes, label);
        if search.trials > TRIALS {
            anyhow::bail!(
                "{refusal}\npbps stopped looking for another order of the table renames after \
                 {TRIALS} trials, so one may exist: deploy some of the renames in a plan of \
                 their own (DEC-1366.1)."
            );
        }
        return Err(refusal);
    };
    cs.changes = search.reordered(&order);
    // The order found is walked once more, as the plan now stands: the same
    // check every other plan gets.
    let mut again = Walk::new(&facts);
    again.run(&cs.changes);
    if again.is_clear() {
        Ok(())
    } else {
        Err(again.refusal(&cs.changes, label))
    }
}

/// A column by its table and name.
pub(crate) type Column = (TableName, String);

/// The columns whose names the database is asked about before column renames
/// are ordered: each rename's source and target, on every table that renames
/// two or more of its columns in this plan (DEC-1366.3).
// The complement is every change that renames no column.
#[allow(clippy::wildcard_enum_match_arm)]
pub(crate) fn renamed_column_names(cs: &ChangeSet) -> Vec<Column> {
    let mut per_table: BTreeMap<&TableName, usize> = BTreeMap::new();
    for p in &cs.changes {
        if let Change::RenameColumn { table, .. } = &p.change {
            *per_table.entry(table).or_default() += 1;
        }
    }
    let mut out: Vec<(TableName, String)> = cs
        .changes
        .iter()
        .filter_map(|p| match &p.change {
            Change::RenameColumn {
                table, from, to, ..
            } if per_table[table] > 1 => {
                Some([(table.clone(), from.clone()), (table.clone(), to.clone())])
            }
            _ => None,
        })
        .flatten()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Orders each table's column renames by which names the database reads as
/// one (`alike`, as `catalog::column_names_alike` answers): a rename into a
/// name runs after the rename that vacates it, under the collation as under
/// the exact spelling. The renames keep the positions they had and only trade
/// them, so an order the differ already got right stays as it is. Renames
/// that wait on one another, such as `a -> B` beside `b -> A` on a
/// case-insensitive database, are refused (DEC-1366.3).
pub(crate) fn order_column_renames(
    cs: &mut ChangeSet,
    alike: &[(Column, Column)],
    label: &str,
) -> anyhow::Result<()> {
    let mut first: BTreeMap<&Column, &Column> = BTreeMap::new();
    for (earlier, later) in alike {
        let root = *first.get(earlier).unwrap_or(&earlier);
        first.insert(later, root);
    }
    let one = |c: &Column| -> Column { (*first.get(c).unwrap_or(&c)).clone() };
    // Each table's renames, by position, with the names they vacate and
    // take as the database reads them.
    type Ends = (usize, Column, Column);
    let mut tables: BTreeMap<TableName, Vec<Ends>> = BTreeMap::new();
    for (i, p) in cs.changes.iter().enumerate() {
        if let Change::RenameColumn {
            table, from, to, ..
        } = &p.change
        {
            tables.entry(table.clone()).or_default().push((
                i,
                one(&(table.clone(), from.clone())),
                one(&(table.clone(), to.clone())),
            ));
        }
    }
    let mut placed: Vec<(Vec<usize>, Vec<usize>)> = Vec::new();
    for (table, renames) in tables {
        if renames.len() < 2 {
            continue;
        }
        // What each rename waits for: the one vacating the name it takes.
        let waits: Vec<Vec<usize>> = (0..renames.len())
            .map(|r| {
                (0..renames.len())
                    .filter(|&v| v != r && renames[v].1 == renames[r].2)
                    .collect()
            })
            .collect();
        let mut left: Vec<usize> = (0..renames.len()).collect();
        let mut order: Vec<usize> = Vec::new();
        while !left.is_empty() {
            let Some(k) = left
                .iter()
                .position(|&r| waits[r].iter().all(|v| order.contains(v)))
            else {
                let waiting: Vec<String> = left
                    .iter()
                    .map(|&r| crate::report::describe(&cs.changes[renames[r].0].change))
                    .collect();
                anyhow::bail!(
                    "`{label}` reads names these column renames of `{table}` use as one, under \
                     its collation, and they wait on one another, so no order runs them: {}. \
                     Rename one of them through a name nothing here uses, in a plan of its own \
                     first, then this one (DEC-1366.3).",
                    waiting.join(", ")
                );
            };
            order.push(left.remove(k));
        }
        placed.push((
            renames.iter().map(|r| r.0).collect(),
            order.into_iter().map(|r| renames[r].0).collect(),
        ));
    }
    if placed.is_empty() {
        return Ok(());
    }
    let mut taken: Vec<Option<PlannedChange>> = std::mem::take(&mut cs.changes)
        .into_iter()
        .map(Some)
        .collect();
    let mut at: Vec<usize> = (0..taken.len()).collect();
    for (slots, order) in placed {
        for (slot, from) in slots.into_iter().zip(order) {
            at[slot] = from;
        }
    }
    cs.changes = at
        .into_iter()
        .map(|i| taken[i].take().expect("each change once"))
        .collect();
    Ok(())
}

/// Whether a change moves in the search, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    /// Moves among the drops and the other renames.
    Rename,
    /// Keeps its order among the other drops.
    Drop,
    /// A module drop the computed edges ordered: moves among the drops and
    /// the renames, after what it waits for and before what waits for it.
    Module,
}

// The complement is every change that keeps its place.
#[allow(clippy::wildcard_enum_match_arm)]
fn part(change: &Change) -> Option<Part> {
    match change {
        Change::RenameTable { .. } => Some(Part::Rename),
        Change::DropTable { .. }
        | Change::DropIndex { .. }
        | Change::DropUnique { .. }
        | Change::DropForeignKey { .. }
        | Change::DropCheck { .. }
        // A drop on a table as an index's is (DEC-1174.1): it names the
        // table, and is addressed by its name where it runs.
        | Change::DropComputedColumn { .. }
        | Change::SetPrimaryKey { to: None, .. } => Some(Part::Drop),
        _ => None,
    }
}

/// `change`, addressed to `table` where it is a drop on a table a rename
/// moves.
// The complement is every change that names no renamed table.
#[allow(clippy::wildcard_enum_match_arm)]
fn addressed(change: &Change, table: &TableName) -> Change {
    let mut change = change.clone();
    match &mut change {
        Change::DropIndex { table: at, .. }
        | Change::DropUnique { table: at, .. }
        | Change::DropForeignKey { table: at, .. }
        | Change::DropCheck { table: at, .. }
        | Change::DropComputedColumn { table: at, .. }
        | Change::SetPrimaryKey {
            table: at,
            to: None,
            ..
        } => *at = table.clone(),
        _ => {}
    }
    change
}

type Seen = BTreeSet<(
    usize,
    BTreeSet<usize>,
    Vec<(TableName, String, Option<TableName>, Option<String>)>,
)>;

struct Search<'c> {
    changes: &'c [PlannedChange],
    /// The position of the first rename or drop: the changes before it run
    /// first, but a module drop the search moves.
    start: usize,
    /// The positions of the renames and drops, in the plan's order.
    region: Vec<usize>,
    drops: Vec<usize>,
    renames: Vec<usize>,
    /// The module drops that move, by position.
    modules: Vec<usize>,
    /// What each change of the region waits for, by position: the edges'
    /// order of the drops (`computed_order::drop_precedence`).
    waits: BTreeMap<usize, BTreeSet<usize>>,
    /// Each rename, by its position, with its source and target.
    moves: BTreeMap<usize, (TableName, TableName)>,
    /// Each drop on a renamed table, by its position, with that rename's.
    owners: BTreeMap<usize, usize>,
    seen: Seen,
    trials: usize,
}

impl<'c> Search<'c> {
    // The complement is every change that is not a table rename.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn new(
        changes: &'c [PlannedChange],
        precedence: &[(Change, Change)],
        facts: &NameFacts,
    ) -> Self {
        // A module drop after the first rename or drop is one the computed
        // edges placed among the drops (DEC-1431.1): after the removal of a
        // computed column that calls it, and before a table it is
        // schema-bound to. It moves between those, by the pairs the edges
        // gave (#1680); moved past them, `DROP TABLE` would run while the
        // module still binds the table (#1461). One no pair names keeps its
        // place among the drops, as the drops keep theirs; so does one whose
        // place no rename can tell apart (`NameFacts::module_drop_matters`):
        // its name is held by nothing, or claimed by no rename, so every
        // place it may take walks the same. Made movable, a table's drop
        // releasing a dozen such functions multiplied the orders past the
        // search's bound (#1680 review). A module drop a pair ties to a
        // movable one moves too, through every such pair: a function bound to
        // the movable one drops after it, and one that must follow it cannot
        // be held in front of where it has to go. A module drop the edges
        // left alone runs in class 0, before the first of them, and stays
        // there unless a pair names it and it is movable by the same rule:
        // one that must only precede a table it is bound to is not moved by
        // `release`, yet a rename can need to run before it (#1680 review).
        // The changes before the first rename or drop keep running first.
        let first = (0..changes.len()).find(|&i| part(&changes[i].change).is_some());
        let at = |c: &Change| (0..changes.len()).find(|&i| changes[i].change == *c);
        let pairs: Vec<(usize, usize)> = precedence
            .iter()
            .filter_map(|(before, after)| Some((at(before)?, at(after)?)))
            .collect();
        let late_module = |i: usize| {
            first.is_some_and(|f| i > f) && matches!(changes[i].change, Change::DropModule { .. })
        };
        let named = |i: usize| pairs.iter().any(|&(a, b)| a == i || b == i);
        // A module drop the search may move: one among the drops, or one
        // before them that a pair names.
        let edge_module = |i: usize| {
            late_module(i)
                || (first.is_some_and(|f| i < f)
                    && named(i)
                    && matches!(changes[i].change, Change::DropModule { .. }))
        };
        let mut movable: BTreeSet<usize> = (0..changes.len())
            .filter(|&i| {
                edge_module(i)
                    && named(i)
                    && matches!(&changes[i].change,
                        Change::DropModule { id, .. } if facts.module_drop_matters(changes, id))
            })
            .collect();
        loop {
            let before = movable.len();
            for &(a, b) in &pairs {
                if edge_module(a)
                    && edge_module(b)
                    && (movable.contains(&a) || movable.contains(&b))
                {
                    movable.insert(a);
                    movable.insert(b);
                }
            }
            if movable.len() == before {
                break;
            }
        }
        let part_at = |i: usize| -> Option<Part> {
            part(&changes[i].change).or_else(|| {
                if movable.contains(&i) {
                    Some(Part::Module)
                } else {
                    late_module(i).then_some(Part::Drop)
                }
            })
        };
        let region: Vec<usize> = (0..changes.len())
            .filter(|&i| part_at(i).is_some())
            .collect();
        let of = |kind: Part| -> Vec<usize> {
            region
                .iter()
                .copied()
                .filter(|&i| part_at(i) == Some(kind))
                .collect()
        };
        let (drops, renames, modules) = (of(Part::Drop), of(Part::Rename), of(Part::Module));
        // Only waits within the region: what runs before it has run.
        let mut waits: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
        for &(before, after) in &pairs {
            if region.contains(&before) && region.contains(&after) {
                waits.entry(after).or_default().insert(before);
            }
        }
        let moves: BTreeMap<usize, (TableName, TableName)> = region
            .iter()
            .filter_map(|&i| match &changes[i].change {
                Change::RenameTable { from, to, .. } => Some((i, (from.clone(), to.clone()))),
                _ => None,
            })
            .collect();
        // Which table each drop names, read along the plan's own order: the
        // differ addressed it by the name the table has where the drop runs
        // (DECISIONS 496). A dropped table is never renamed, so it names
        // itself.
        let mut names: BTreeMap<usize, &TableName> =
            moves.iter().map(|(r, (from, _))| (*r, from)).collect();
        let mut owners = BTreeMap::new();
        for &i in &region {
            let change = &changes[i].change;
            if let Some((_, to)) = moves.get(&i) {
                names.insert(i, to);
            } else if !matches!(change, Change::DropTable { .. })
                && let Some(table) = change.table()
                && let Some((r, _)) = names.iter().find(|(_, now)| **now == table)
            {
                owners.insert(i, *r);
            }
        }
        Search {
            changes,
            start: first.unwrap_or(changes.len()),
            region,
            drops,
            renames,
            modules,
            waits,
            moves,
            owners,
            seen: BTreeSet::new(),
            trials: 0,
        }
    }

    /// The change at position `i`, addressed for the renames `done`.
    fn change(&self, i: usize, done: &BTreeSet<usize>) -> Change {
        let change = &self.changes[i].change;
        match self
            .owners
            .get(&i)
            .and_then(|r| Some((r, self.moves.get(r)?)))
        {
            Some((r, (from, to))) => addressed(change, if done.contains(r) { to } else { from }),
            None => change.clone(),
        }
    }

    /// The first order of the renames and drops the walk clears, with the
    /// rest of the plan after it, by position.
    fn run(&mut self, facts: &NameFacts) -> Option<Vec<usize>> {
        self.region.first()?;
        let mut walk = Walk::new(facts);
        for i in self.before() {
            walk.step(i, &self.changes[i].change);
        }
        if !walk.is_clear() {
            return None;
        }
        let left: BTreeSet<usize> = self.renames.iter().chain(&self.modules).copied().collect();
        self.next(&walk, 0, &left, &mut Vec::new())
    }

    fn next(
        &mut self,
        walk: &Walk<'_>,
        drop: usize,
        left: &BTreeSet<usize>,
        order: &mut Vec<usize>,
    ) -> Option<Vec<usize>> {
        self.trials += 1;
        if self.trials > TRIALS {
            return None;
        }
        let done: BTreeSet<usize> = self
            .renames
            .iter()
            .copied()
            .filter(|r| !left.contains(r))
            .collect();
        if drop == self.drops.len() && left.is_empty() {
            let mut rest = walk.clone();
            for i in self.after() {
                rest.step(i, &self.changes[i].change);
            }
            return rest.is_clear().then(|| order.clone());
        }
        if !self.seen.insert((drop, left.clone(), walk.state())) {
            return None;
        }
        let mut candidates: Vec<usize> = left.iter().copied().collect();
        candidates.extend(self.drops.get(drop));
        candidates.sort_unstable();
        for i in candidates {
            if self
                .waits
                .get(&i)
                .is_some_and(|before| !before.iter().all(|b| order.contains(b)))
            {
                continue;
            }
            let mut trial = walk.clone();
            trial.step(i, &self.change(i, &done));
            if !trial.is_clear() {
                continue;
            }
            let (drop, mut left) = (drop, left.clone());
            let drop = if left.remove(&i) { drop } else { drop + 1 };
            order.push(i);
            if let Some(found) = self.next(&trial, drop, &left, order) {
                return Some(found);
            }
            order.pop();
            if self.trials > TRIALS {
                return None;
            }
        }
        None
    }

    /// The positions that run before the region: every change ahead of the
    /// first rename or drop, but a module drop the search moves.
    fn before(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.start).filter(|i| !self.region.contains(i))
    }

    /// The positions that run after it, in the plan's order.
    fn after(&self) -> impl Iterator<Item = usize> + '_ {
        (self.start..self.changes.len()).filter(|i| !self.region.contains(i))
    }

    /// The plan with its renames and drops in `order`, each drop addressed
    /// for where it now runs: everything before the first of them, then
    /// them, then the rest in the plan's order.
    fn reordered(&self, order: &[usize]) -> Vec<PlannedChange> {
        let mut out: Vec<PlannedChange> = self.before().map(|i| self.changes[i].clone()).collect();
        let mut done = BTreeSet::new();
        for &i in order {
            let mut p = self.changes[i].clone();
            p.change = self.change(i, &done);
            if self.moves.contains_key(&i) {
                done.insert(i);
            }
            out.push(p);
        }
        out.extend(self.after().map(|i| self.changes[i].clone()));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_mssql::catalog::NameOccupant;
    use pbps_mssql::emit::{default_constraint_name, fallback_default_constraint_name};

    fn name(s: &str) -> TableName {
        s.parse().unwrap()
    }

    fn rename(from: &str, to: &str, defaults: &[&str]) -> PlannedChange {
        PlannedChange::new(Change::RenameTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, from, 0),
            from: name(from),
            to: name(to),
            defaults: defaults.iter().map(|d| (*d).to_owned()).collect(),
        })
    }

    fn drop_check(table: &str, check: &str) -> PlannedChange {
        PlannedChange::new(Change::DropCheck {
            table: name(table),
            name: check.into(),
        })
    }

    /// An object the catalog read found, under the name the plan asked for.
    fn held(at: &str, kind: &str, parent: Option<&str>, column: Option<&str>) -> NameOccupant {
        NameOccupant {
            wanted: name(at),
            name: name(at),
            kind: kind.into(),
            parent: parent.map(name),
            parent_column: column.map(str::to_owned),
        }
    }

    fn renames(cs: &ChangeSet) -> Vec<(String, String)> {
        cs.changes
            .iter()
            .filter_map(|p| {
                if let Change::RenameTable { from, to, .. } = &p.change {
                    Some((from.to_string(), to.to_string()))
                } else {
                    None
                }
            })
            .collect()
    }

    fn order(
        changes: Vec<PlannedChange>,
        occupants: &[NameOccupant],
        alike: &[(TableName, TableName)],
    ) -> anyhow::Result<ChangeSet> {
        let mut cs = ChangeSet { changes };
        order_occupied_objects_under(&mut cs, occupants, alike, "prod", &[])?;
        // Whatever order was settled, the walk clears it as it stands.
        crate::deploy::refuse_occupied_objects_under(&cs, occupants, alike, "prod")?;
        Ok(cs)
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.to_owned(), b.to_owned())
    }

    /// #1361: a default adopted under a hand-chosen name is in the catalog
    /// only. A move of its table carries it out of the source schema, so a
    /// rename into that name runs after the move; with nothing moving it, the
    /// rename is refused with the default named.
    #[test]
    fn an_adopted_default_name_is_free_once_its_table_moves() {
        let adopted = held("s1.c", "default constraint", Some("s1.old"), Some("x"));
        let cs = order(
            vec![
                rename("s1.a", "s1.c", &[]),
                rename("s1.old", "s2.new", &["x"]),
            ],
            std::slice::from_ref(&adopted),
            &[],
        )
        .unwrap();
        assert_eq!(
            renames(&cs),
            [pair("s1.old", "s2.new"), pair("s1.a", "s1.c")]
        );

        let e = order(vec![rename("s1.a", "s1.c", &[])], &[adopted], &[])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("the database already has default constraint `s1.c` on `s1.old`"),
            "{e}"
        );
        assert!(!e.contains("a later change frees"), "{e}");
    }

    /// #1361 (review of #1346): which of its generated names a default holds
    /// is the catalog's to say. `z.old`'s default parked at its fallback
    /// leaves `z.DF_pbps_old_x` free, so `a.other` moves there first and
    /// frees `a.DF_pbps_other_y` for `z.old`; with the default at its
    /// generated name the two wait on each other, and the plan is refused
    /// with that said.
    #[test]
    fn the_generated_name_a_default_holds_decides_the_order() {
        let old = name("z.old");
        let other = name("a.other");
        let parked = held(
            &format!("z.{}", fallback_default_constraint_name(&old, "x")),
            "default constraint",
            Some("z.old"),
            Some("x"),
        );
        let others = held(
            &format!("a.{}", default_constraint_name(&other, "y")),
            "default constraint",
            Some("a.other"),
            Some("y"),
        );
        let plan = || {
            vec![
                rename("z.old", "a.DF_pbps_other_y", &["x"]),
                rename("a.other", "z.DF_pbps_old_x", &["y"]),
            ]
        };
        let cs = order(plan(), &[parked, others.clone()], &[]).unwrap();
        assert_eq!(
            renames(&cs),
            [
                pair("a.other", "z.DF_pbps_old_x"),
                pair("z.old", "a.DF_pbps_other_y")
            ]
        );

        let generated = held(
            &format!("z.{}", default_constraint_name(&old, "x")),
            "default constraint",
            Some("z.old"),
            Some("x"),
        );
        let e = order(plan(), &[generated, others], &[])
            .unwrap_err()
            .to_string();
        assert!(e.contains("which a later change frees"), "{e}");
        assert!(e.contains("DEC-1366.2"), "{e}");
    }

    /// #1366 (review of #1346): which spellings are one name is the
    /// collation's. On `Turkish_100_CI_AS` `A` and `a` are one name and `I`
    /// and `i` are two, so `A -> i` runs before `I -> a`; on a database that
    /// reads all four as distinct, the plan's own order stands.
    #[test]
    fn the_databases_alike_pairs_order_a_case_chain() {
        let plan = || vec![rename("dbo.I", "dbo.a", &[]), rename("dbo.A", "dbo.i", &[])];
        // The read of `dbo.a` finds the table the catalog spells `dbo.A`.
        let turkish = NameOccupant {
            wanted: name("dbo.a"),
            name: name("dbo.A"),
            kind: "user table".into(),
            parent: None,
            parent_column: None,
        };
        let alike = [(name("dbo.A"), name("dbo.a"))];
        let cs = order(plan(), &[turkish], &alike).unwrap();
        assert_eq!(
            renames(&cs),
            [pair("dbo.A", "dbo.i"), pair("dbo.I", "dbo.a")]
        );

        let cs = order(plan(), &[], &[]).unwrap();
        assert_eq!(
            renames(&cs),
            [pair("dbo.I", "dbo.a"), pair("dbo.A", "dbo.i")]
        );
    }

    /// #1362: a transfer puts the table at its old name in the new schema
    /// before `sp_rename` runs. An object the project does not record there
    /// refuses the plan, since nothing in it frees that name; the name is
    /// read for it.
    #[test]
    fn a_transfers_intermediate_name_is_claimed() {
        let moved = vec![rename("s1.old", "s2.new", &[])];
        assert!(
            crate::deploy::object_reads(&ChangeSet {
                changes: moved.clone()
            })
            .0
            .contains(&name("s2.old")),
            "the intermediate name is read"
        );
        let sequence = held("s2.old", "sequence object", None, None);
        let e = order(moved.clone(), &[sequence], &[])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("`s2.old`: the database already has sequence object `s2.old`"),
            "{e}"
        );
        // A move that keeps its name has no other stop than its target.
        order(vec![rename("s1.old", "s2.old", &[])], &[], &[]).unwrap();
        order(moved, &[], &[]).unwrap();
    }

    /// #1366 (review of #1346): a move carries a generated default into the
    /// new schema under its old name, and renames it there only afterwards.
    /// A rename into that old name runs after the move; where both names the
    /// move could give the default are held, the default stays and the plan
    /// is refused.
    #[test]
    fn a_carried_defaults_old_name_is_free_after_the_move() {
        let old = name("s1.old");
        let new = name("s2.new");
        let default = held(
            &format!("s1.{}", default_constraint_name(&old, "x")),
            "default constraint",
            Some("s1.old"),
            Some("x"),
        );
        let plan = || {
            vec![
                rename("s2.a", "s2.DF_pbps_old_x", &[]),
                rename("s1.old", "s2.new", &["x"]),
            ]
        };
        let cs = order(plan(), std::slice::from_ref(&default), &[]).unwrap();
        assert_eq!(
            renames(&cs),
            [pair("s1.old", "s2.new"), pair("s2.a", "s2.DF_pbps_old_x")]
        );

        let taken = |n: String| held(&format!("s2.{n}"), "sequence object", None, None);
        let e = order(
            plan(),
            &[
                default,
                taken(default_constraint_name(&new, "x")),
                taken(fallback_default_constraint_name(&new, "x")),
            ],
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("`s2.DF_pbps_old_x`"), "{e}");
    }

    /// DEC-1366.2: a column rename (class 3) that moves a generated default
    /// away from the name a table rename (class 1) claims runs after every
    /// table rename (DECISIONS 496). The plan is refused with the change
    /// that frees the name and the two-deployment remedy, not left to fail
    /// at `sp_rename`.
    #[test]
    fn a_name_only_a_later_class_frees_is_refused_with_its_remedy() {
        let t = name("dbo.t");
        let generated = default_constraint_name(&t, "old");
        let e = order(
            vec![
                rename("dbo.a", &format!("dbo.{generated}"), &[]),
                PlannedChange::new(Change::RenameColumn {
                    uid: pbps_model::Uid::derived(pbps_model::UidKind::Column, "dbo.t.old", 0),
                    table: t.clone(),
                    from: "old".into(),
                    to: "new".into(),
                    table_was: None,
                }),
            ],
            &[held(
                &format!("dbo.{generated}"),
                "default constraint",
                Some("dbo.t"),
                Some("old"),
            )],
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("which a later change frees: `dbo.t` ~ rename column old -> new"),
            "{e}"
        );
        assert!(e.contains("in a plan of its own"), "{e}");
    }

    /// Two tables trading names wait on each other in every order: refused,
    /// never reordered into something else.
    #[test]
    fn a_swap_is_refused_as_two_changes_waiting_on_each_other() {
        let e = order(
            vec![rename("dbo.a", "dbo.b", &[]), rename("dbo.b", "dbo.a", &[])],
            &[
                held("dbo.a", "user table", None, None),
                held("dbo.b", "user table", None, None),
            ],
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("`dbo.b`: the database already has user table `dbo.b`"),
            "{e}"
        );
        assert!(e.contains("which a later change frees"), "{e}");
    }

    /// The default a rename moves goes to its generated name when that is
    /// free and to its fallback otherwise (DEC-981.1), so the first rename
    /// that can run is not always the one to run: here `A` run first parks
    /// its default at the fallback `B` is renamed into. Only a whole order the
    /// walk clears is kept, and one exists.
    #[test]
    fn a_rename_that_would_strand_a_later_one_waits() {
        let z = name("dbo.z");
        let preferred = default_constraint_name(&z, "v");
        let fallback = fallback_default_constraint_name(&z, "v");
        let a = name("dbo.A");
        let cs = order(
            vec![
                rename("dbo.A", "dbo.z", &["v"]),
                rename("dbo.B", &format!("dbo.{fallback}"), &[]),
                rename(&format!("dbo.{preferred}"), "dbo.other", &[]),
            ],
            &[
                held(
                    &format!("dbo.{}", default_constraint_name(&a, "v")),
                    "default constraint",
                    Some("dbo.A"),
                    Some("v"),
                ),
                held(&format!("dbo.{preferred}"), "user table", None, None),
            ],
            &[],
        )
        .unwrap();
        let ran = renames(&cs);
        let at = |from: &str| ran.iter().position(|(f, _)| f == from).unwrap();
        assert!(at("dbo.B") < at("dbo.A"), "{ran:?}");
    }

    fn rename_column(table: &str, from: &str, to: &str) -> PlannedChange {
        PlannedChange::new(Change::RenameColumn {
            uid: pbps_model::Uid::derived(
                pbps_model::UidKind::Column,
                &format!("{table}.{from}"),
                0,
            ),
            table: name(table),
            from: from.into(),
            to: to.into(),
            table_was: None,
        })
    }

    fn column_renames(cs: &ChangeSet) -> Vec<(String, String, String)> {
        cs.changes
            .iter()
            .filter_map(|p| {
                if let Change::RenameColumn {
                    table, from, to, ..
                } = &p.change
                {
                    Some((table.to_string(), from.clone(), to.clone()))
                } else {
                    None
                }
            })
            .collect()
    }

    fn column(table: &str, column: &str) -> Column {
        (name(table), column.to_owned())
    }

    /// DEC-1366.3: which column names are one is the collation's. On
    /// `Turkish_100_CI_AS` `A` and `a` are one name and `I` and `i` are two,
    /// so `A -> i` runs before `I -> a`; another table's rename keeps its
    /// place. Where the database reads all four as distinct, the plan's own
    /// order stands, and an exact chain is still ordered.
    #[test]
    fn the_databases_alike_column_names_order_a_rename_chain() {
        let plan = || {
            vec![
                rename_column("dbo.t", "I", "a"),
                rename_column("dbo.u", "x", "y"),
                rename_column("dbo.t", "A", "i"),
            ]
        };
        let mut cs = ChangeSet { changes: plan() };
        assert_eq!(
            renamed_column_names(&cs),
            [
                column("dbo.t", "A"),
                column("dbo.t", "I"),
                column("dbo.t", "a"),
                column("dbo.t", "i"),
            ],
            "only a table renaming two columns is asked about"
        );
        let turkish = [(column("dbo.t", "A"), column("dbo.t", "a"))];
        order_column_renames(&mut cs, &turkish, "prod").unwrap();
        let t = |from: &str, to: &str| ("dbo.t".to_owned(), from.to_owned(), to.to_owned());
        assert_eq!(
            column_renames(&cs),
            [
                t("A", "i"),
                ("dbo.u".to_owned(), "x".to_owned(), "y".to_owned()),
                t("I", "a")
            ]
        );

        let mut cs = ChangeSet { changes: plan() };
        order_column_renames(&mut cs, &[], "prod").unwrap();
        assert_eq!(column_renames(&cs)[0], t("I", "a"));

        let mut cs = ChangeSet {
            changes: vec![
                rename_column("dbo.t", "a", "b"),
                rename_column("dbo.t", "b", "c"),
            ],
        };
        order_column_renames(&mut cs, &[], "prod").unwrap();
        assert_eq!(column_renames(&cs), [t("b", "c"), t("a", "b")]);
        // A rename into a case variant of its own name waits for nothing.
        let mut cs = ChangeSet {
            changes: vec![
                rename_column("dbo.t", "a", "A"),
                rename_column("dbo.t", "b", "c"),
            ],
        };
        order_column_renames(
            &mut cs,
            &[(column("dbo.t", "a"), column("dbo.t", "A"))],
            "prod",
        )
        .unwrap();
        assert_eq!(column_renames(&cs), [t("a", "A"), t("b", "c")]);
    }

    /// Two column renames into each other's names under the collation have
    /// no order on that database: refused at `plan --db`, naming both.
    #[test]
    fn column_renames_trading_names_under_the_collation_are_refused() {
        let mut cs = ChangeSet {
            changes: vec![
                rename_column("dbo.t", "a", "B"),
                rename_column("dbo.t", "b", "A"),
            ],
        };
        let alike = [
            (column("dbo.t", "A"), column("dbo.t", "a")),
            (column("dbo.t", "B"), column("dbo.t", "b")),
        ];
        let e = order_column_renames(&mut cs, &alike, "prod")
            .unwrap_err()
            .to_string();
        assert!(e.contains("wait on one another"), "{e}");
        assert!(
            e.contains("rename column a -> B") && e.contains("rename column b -> A"),
            "{e}"
        );
        // On a database that reads them as four names, the pair is valid.
        order_column_renames(&mut cs, &[], "prod").unwrap();
    }

    /// #1461: a module drop the computed edges placed between two table
    /// drops, after the table whose computed column calls it and before the
    /// table it is schema-bound to, keeps that place when a name conflict
    /// makes the search reorder the renames. Moved after every drop, `DROP
    /// TABLE dbo.lookup` would run while the schema-bound function stands.
    #[test]
    fn a_module_drop_among_the_drops_keeps_its_place_in_the_search() {
        let drop_table = |table: &str| {
            PlannedChange::new(Change::DropTable {
                uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, table, 0),
                name: name(table),
                detach_from: None,
            })
        };
        let drop_function = PlannedChange::new(Change::DropModule {
            id: pbps_model::ModuleId::Named(name("dbo.f")),
            kind: pbps_model::ModuleKind::Function,
        });
        // The conflict of the test above: `s1.a` cannot move to `s1.c` until
        // `s1.old` carries the default holding that name away.
        let adopted = held("s1.c", "default constraint", Some("s1.old"), Some("x"));
        let cs = order(
            vec![
                drop_table("dbo.u"),
                drop_function,
                drop_table("dbo.lookup"),
                rename("s1.a", "s1.c", &[]),
                rename("s1.old", "s2.new", &["x"]),
            ],
            &[adopted],
            &[],
        )
        .unwrap();
        assert_eq!(
            renames(&cs),
            [pair("s1.old", "s2.new"), pair("s1.a", "s1.c")],
            "the search ran"
        );
        let drops: Vec<String> = cs
            .changes
            .iter()
            .filter(|p| {
                matches!(
                    p.change,
                    Change::DropTable { .. } | Change::DropModule { .. }
                )
            })
            .map(|p| p.change.subject().to_string())
            .collect();
        assert_eq!(drops, ["dbo.u", "dbo.f", "dbo.lookup"]);
        // The same with the edges' pairs, which let the function move: it
        // still runs after `u` and before `lookup`.
        let mut cs = ChangeSet {
            changes: vec![
                drop_table("dbo.u"),
                PlannedChange::new(Change::DropModule {
                    id: pbps_model::ModuleId::Named(name("dbo.f")),
                    kind: pbps_model::ModuleKind::Function,
                }),
                drop_table("dbo.lookup"),
                rename("s1.a", "s1.c", &[]),
                rename("s1.old", "s2.new", &["x"]),
            ],
        };
        let f = cs.changes[1].change.clone();
        let pairs = [
            (cs.changes[0].change.clone(), f.clone()),
            (f, cs.changes[2].change.clone()),
        ];
        let adopted = held("s1.c", "default constraint", Some("s1.old"), Some("x"));
        order_occupied_objects_under(&mut cs, std::slice::from_ref(&adopted), &[], "prod", &pairs)
            .unwrap();
        let drops: Vec<String> = cs
            .changes
            .iter()
            .filter(|p| {
                matches!(
                    p.change,
                    Change::DropTable { .. } | Change::DropModule { .. }
                )
            })
            .map(|p| p.change.subject().to_string())
            .collect();
        assert_eq!(drops, ["dbo.u", "dbo.f", "dbo.lookup"]);

        // Negative: a module drop before the first rename or drop is class
        // 0's, and stays where it is, ahead of the region.
        let ahead = [
            PlannedChange::new(Change::DropModule {
                id: pbps_model::ModuleId::Named(name("dbo.g")),
                kind: pbps_model::ModuleKind::Function,
            }),
            drop_table("dbo.u"),
            rename("s1.a", "s1.c", &[]),
        ];
        assert_eq!(
            Search::new(&ahead, &[], &NameFacts::new(&[], &[])).region,
            [1, 2]
        );
    }

    /// #1680: a module drop the edges ordered moves between what it waits
    /// for and what waits for it, not only where it stands among every drop.
    /// `dbo.u`'s computed column calls `dbo.DF_pbps_new_x`, bound to nothing
    /// dropped, so the function waits for `u` alone. Dropped right after `u`,
    /// it frees its name before `old` becomes `new`; the default of `x` then
    /// takes it (DEC-981.1), and the check claiming it collides. Run after
    /// the rename, it lets the default fall back while it still holds the
    /// name, and frees it for the check.
    #[test]
    fn a_module_drop_moves_between_what_its_edges_order_it_after_and_before() {
        let drop_table = |table: &str| {
            PlannedChange::new(Change::DropTable {
                uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, table, 0),
                name: name(table),
                detach_from: None,
            })
        };
        let new = name("dbo.new");
        let generated = default_constraint_name(&new, "x");
        let function = PlannedChange::new(Change::DropModule {
            id: pbps_model::ModuleId::Named(name(&format!("dbo.{generated}"))),
            kind: pbps_model::ModuleKind::Function,
        });
        let check = PlannedChange::new(Change::AddCheck {
            table: name("dbo.t"),
            name: generated.clone(),
            constraint: pbps_model::CheckConstraint {
                expression: "a > 0".into(),
            },
        });
        // As `release` leaves it: the function right after `u`.
        let plan = || {
            vec![
                drop_table("dbo.u"),
                function.clone(),
                drop_table("dbo.z"),
                rename("dbo.old", "dbo.new", &["x"]),
                check.clone(),
            ]
        };
        let occupants = [
            held(
                &format!("dbo.{}", default_constraint_name(&name("dbo.old"), "x")),
                "default constraint",
                Some("dbo.old"),
                Some("x"),
            ),
            // An adopted default on `z` under the name `old` moves to.
            held("dbo.new", "default constraint", Some("dbo.z"), Some("y")),
            held(
                &format!("dbo.{generated}"),
                "sql scalar function",
                None,
                None,
            ),
        ];
        let precedence = [(drop_table("dbo.u").change, function.change.clone())];
        // Its name is one the rename's default can take, so it moves.
        let facts = NameFacts::new(&occupants, &[]);
        assert_eq!(Search::new(&plan(), &precedence, &facts).modules, [1]);
        // Negative: with nothing under that name in the catalog read, its
        // drop frees nothing, and it stays.
        let unheld = NameFacts::new(&occupants[..2], &[]);
        assert!(
            Search::new(&plan(), &precedence, &unheld)
                .modules
                .is_empty()
        );
        let mut cs = ChangeSet { changes: plan() };
        order_occupied_objects_under(&mut cs, &occupants, &[], "prod", &precedence).unwrap();
        crate::deploy::refuse_occupied_objects_under(&cs, &occupants, &[], "prod").unwrap();
        let at = |c: &Change| cs.changes.iter().position(|p| p.change == *c).unwrap();
        let (u, f) = (at(&drop_table("dbo.u").change), at(&function.change));
        let renamed = cs
            .changes
            .iter()
            .position(|p| matches!(p.change, Change::RenameTable { .. }))
            .unwrap();
        assert!(u < renamed && renamed < f, "{:?}", cs.changes);

        // Negative: with no pair, the function keeps its place among the
        // drops, and no order clears the plan.
        let mut cs = ChangeSet { changes: plan() };
        order_occupied_objects_under(&mut cs, &occupants, &[], "prod", &[]).unwrap_err();

        // Negative: a pair after it holds too. Schema-bound to `z`, the
        // function drops before it, so the name is free before the rename
        // can run, and the plan is refused rather than reordered past it.
        let bound = [
            (drop_table("dbo.u").change, function.change.clone()),
            (function.change.clone(), drop_table("dbo.z").change),
        ];
        let mut cs = ChangeSet { changes: plan() };
        order_occupied_objects_under(&mut cs, &occupants, &[], "prod", &bound).unwrap_err();

        // The same with the function bound to a computed column dropped
        // before `z` (#1680 review): that drop waits for the function, so
        // the function cannot pass it, and the plan is refused.
        let computed = PlannedChange::new(Change::DropComputedColumn {
            table: name("dbo.b"),
            name: "c".into(),
            computed: pbps_model::ComputedColumn {
                expression: "a * 2".into(),
                persisted: false,
                not_null: false,
            },
        });
        let mut changes = plan();
        changes.insert(2, computed.clone());
        let bound = [
            (drop_table("dbo.u").change, function.change.clone()),
            (function.change.clone(), computed.change.clone()),
        ];
        let mut cs = ChangeSet {
            changes: changes.clone(),
        };
        order_occupied_objects_under(&mut cs, &occupants, &[], "prod", &bound).unwrap_err();
        // Without that pair the function would move past the computed drop.
        let mut cs = ChangeSet { changes };
        order_occupied_objects_under(&mut cs, &occupants, &[], "prod", &precedence).unwrap();
        let at = |c: &Change| cs.changes.iter().position(|p| p.change == *c).unwrap();
        assert!(
            at(&computed.change) < at(&function.change),
            "{:?}",
            cs.changes
        );
    }

    /// #1680 review: a module drop whose name the catalog read found nothing
    /// under frees no name wherever it runs, so it keeps its place. `dbo.z`'s
    /// drop releases eleven such functions and frees a check's name the
    /// rename's default would then take; the rename runs first and parks the
    /// default at its fallback. Movable, the eleven drops multiplied the
    /// orders past the search's bound and the plan was refused.
    #[test]
    fn a_module_drop_that_frees_no_name_keeps_its_place() {
        let new = name("dbo.new");
        let generated = default_constraint_name(&new, "x");
        let functions: Vec<PlannedChange> = (0..11)
            .map(|k| {
                PlannedChange::new(Change::DropModule {
                    id: pbps_model::ModuleId::Named(name(&format!("dbo.f{k}"))),
                    kind: pbps_model::ModuleKind::Function,
                })
            })
            .collect();
        let z = PlannedChange::new(Change::DropTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "dbo.z", 0),
            name: name("dbo.z"),
            detach_from: None,
        });
        let mut changes = vec![z.clone()];
        changes.extend(functions.iter().cloned());
        changes.push(rename("dbo.old", "dbo.new", &["x"]));
        changes.push(PlannedChange::new(Change::AddCheck {
            table: name("dbo.t"),
            name: generated.clone(),
            constraint: pbps_model::CheckConstraint {
                expression: "a > 0".into(),
            },
        }));
        let occupants = [
            held(
                &format!("dbo.{}", default_constraint_name(&name("dbo.old"), "x")),
                "default constraint",
                Some("dbo.old"),
                Some("x"),
            ),
            held(
                &format!("dbo.{generated}"),
                "check constraint",
                Some("dbo.z"),
                None,
            ),
        ];
        let precedence: Vec<(Change, Change)> = functions
            .iter()
            .map(|f| (z.change.clone(), f.change.clone()))
            .collect();
        let mut cs = ChangeSet { changes };
        order_occupied_objects_under(&mut cs, &occupants, &[], "prod", &precedence).unwrap();
        assert!(
            matches!(cs.changes[0].change, Change::RenameTable { .. }),
            "{:?}",
            cs.changes
        );
        let facts = NameFacts::new(&occupants, &[]);
        assert!(
            Search::new(&cs.changes, &precedence, &facts)
                .modules
                .is_empty()
        );
    }

    /// #1680 review: a module drop whose name only a check the plan adds
    /// claims keeps its place too. The check runs after every module drop in
    /// any order, so only a rename's claim can tell two places apart. Here
    /// the eleven functions are in the catalog, and checks reuse their names.
    #[test]
    fn a_module_drop_whose_name_no_rename_claims_keeps_its_place() {
        let new = name("dbo.new");
        let generated = default_constraint_name(&new, "x");
        let check = |n: &str| {
            PlannedChange::new(Change::AddCheck {
                table: name("dbo.t"),
                name: n.into(),
                constraint: pbps_model::CheckConstraint {
                    expression: "a > 0".into(),
                },
            })
        };
        let functions: Vec<PlannedChange> = (0..11)
            .map(|k| {
                PlannedChange::new(Change::DropModule {
                    id: pbps_model::ModuleId::Named(name(&format!("dbo.g{k}"))),
                    kind: pbps_model::ModuleKind::Function,
                })
            })
            .collect();
        let z = PlannedChange::new(Change::DropTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "dbo.z", 0),
            name: name("dbo.z"),
            detach_from: None,
        });
        let mut changes = vec![z.clone()];
        changes.extend(functions.iter().cloned());
        changes.push(rename("dbo.old", "dbo.new", &["x"]));
        changes.push(check(&generated));
        changes.extend((0..11).map(|k| check(&format!("g{k}"))));
        let mut occupants = vec![
            held(
                &format!("dbo.{}", default_constraint_name(&name("dbo.old"), "x")),
                "default constraint",
                Some("dbo.old"),
                Some("x"),
            ),
            held(
                &format!("dbo.{generated}"),
                "check constraint",
                Some("dbo.z"),
                None,
            ),
        ];
        occupants
            .extend((0..11).map(|k| held(&format!("dbo.g{k}"), "sql scalar function", None, None)));
        let precedence: Vec<(Change, Change)> = functions
            .iter()
            .map(|f| (z.change.clone(), f.change.clone()))
            .collect();
        let facts = NameFacts::new(&occupants, &[]);
        assert!(
            Search::new(&changes, &precedence, &facts)
                .modules
                .is_empty()
        );
        let mut cs = ChangeSet { changes };
        order_occupied_objects_under(&mut cs, &occupants, &[], "prod", &precedence).unwrap();
        assert!(
            matches!(cs.changes[0].change, Change::RenameTable { .. }),
            "{:?}",
            cs.changes
        );
    }

    /// #1680 review: across schemas a rename claims the names of the children
    /// it carries, which are the read's children of its own table, and not
    /// those of another table. Counted as claimed, a function sharing an
    /// unrelated constraint's name moved and multiplied the orders.
    #[test]
    fn a_transfer_claims_only_its_own_tables_children() {
        let function = |n: &str| pbps_model::ModuleId::Named(name(n));
        let occupants = [
            held("s1.c1", "check constraint", Some("s1.old"), None),
            held("dbo.c2", "check constraint", Some("dbo.other"), None),
            held("s2.c1", "sql scalar function", None, None),
            held("s2.c2", "sql scalar function", None, None),
        ];
        let facts = NameFacts::new(&occupants, &[]);
        let changes = [rename("s1.old", "s2.new", &[])];
        assert!(facts.module_drop_matters(&changes, &function("s2.c1")));
        assert!(!facts.module_drop_matters(&changes, &function("s2.c2")));
        // Within a schema nothing is carried.
        let changes = [rename("s1.old", "s1.new", &[])];
        assert!(!facts.module_drop_matters(&changes, &function("s1.c1")));
    }

    /// #1680 review: a module drop tied by a pair to a movable one moves with
    /// it. The example's function `F` is schema-bound to `dbo.g`, which this
    /// plan also drops, so `g` drops after `F`. `g`'s name matters to no
    /// rename, but held fixed between `u` and `z` it pinned `F` before the
    /// rename, and the plan was refused; `u → z → rename → F → g` clears it.
    #[test]
    fn a_module_drop_tied_to_a_movable_one_moves_with_it() {
        let drop_table = |table: &str| {
            PlannedChange::new(Change::DropTable {
                uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, table, 0),
                name: name(table),
                detach_from: None,
            })
        };
        let new = name("dbo.new");
        let generated = default_constraint_name(&new, "x");
        let drop_function = |n: &str| {
            PlannedChange::new(Change::DropModule {
                id: pbps_model::ModuleId::Named(name(n)),
                kind: pbps_model::ModuleKind::Function,
            })
        };
        let f = drop_function(&format!("dbo.{generated}"));
        let g = drop_function("dbo.g");
        let changes = vec![
            drop_table("dbo.u"),
            f.clone(),
            g.clone(),
            drop_table("dbo.z"),
            rename("dbo.old", "dbo.new", &["x"]),
            PlannedChange::new(Change::AddCheck {
                table: name("dbo.t"),
                name: generated.clone(),
                constraint: pbps_model::CheckConstraint {
                    expression: "a > 0".into(),
                },
            }),
        ];
        let occupants = [
            held(
                &format!("dbo.{}", default_constraint_name(&name("dbo.old"), "x")),
                "default constraint",
                Some("dbo.old"),
                Some("x"),
            ),
            held("dbo.new", "default constraint", Some("dbo.z"), Some("y")),
            held(
                &format!("dbo.{generated}"),
                "sql scalar function",
                None,
                None,
            ),
            held("dbo.g", "sql scalar function", None, None),
        ];
        let precedence = [
            (drop_table("dbo.u").change, f.change.clone()),
            (f.change.clone(), g.change.clone()),
        ];
        let facts = NameFacts::new(&occupants, &[]);
        assert_eq!(Search::new(&changes, &precedence, &facts).modules, [1, 2]);
        let mut cs = ChangeSet { changes };
        order_occupied_objects_under(&mut cs, &occupants, &[], "prod", &precedence).unwrap();
        let at = |c: &Change| cs.changes.iter().position(|p| p.change == *c).unwrap();
        let renamed = cs
            .changes
            .iter()
            .position(|p| matches!(p.change, Change::RenameTable { .. }))
            .unwrap();
        assert!(
            renamed < at(&f.change) && at(&f.change) < at(&g.change),
            "{:?}",
            cs.changes
        );
    }

    /// #1680 review: a module drop ahead of the renames and drops that a pair
    /// names moves too. `F` must only drop before `dbo.lookup`, which it is
    /// bound to, so `release` leaves it in class 0. Run there, it frees its
    /// name for the rename's default, and the check claiming that name
    /// collides; `rename → F → lookup` clears the plan. A class-0 change no
    /// pair names still runs first.
    #[test]
    fn a_leading_module_drop_a_pair_names_moves_too() {
        let new = name("dbo.new");
        let generated = default_constraint_name(&new, "x");
        let drop_function = |n: &str| {
            PlannedChange::new(Change::DropModule {
                id: pbps_model::ModuleId::Named(name(n)),
                kind: pbps_model::ModuleKind::Function,
            })
        };
        let f = drop_function(&format!("dbo.{generated}"));
        let view = drop_function("dbo.v");
        let lookup = PlannedChange::new(Change::DropTable {
            uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "dbo.lookup", 0),
            name: name("dbo.lookup"),
            detach_from: None,
        });
        let plan = || {
            vec![
                f.clone(),
                view.clone(),
                lookup.clone(),
                rename("dbo.old", "dbo.new", &["x"]),
                PlannedChange::new(Change::AddCheck {
                    table: name("dbo.t"),
                    name: generated.clone(),
                    constraint: pbps_model::CheckConstraint {
                        expression: "a > 0".into(),
                    },
                }),
            ]
        };
        let occupants = [
            held(
                &format!("dbo.{}", default_constraint_name(&name("dbo.old"), "x")),
                "default constraint",
                Some("dbo.old"),
                Some("x"),
            ),
            held(
                &format!("dbo.{generated}"),
                "sql scalar function",
                None,
                None,
            ),
        ];
        let precedence = [(f.change.clone(), lookup.change.clone())];
        let facts = NameFacts::new(&occupants, &[]);
        let changes = plan();
        let search = Search::new(&changes, &precedence, &facts);
        assert_eq!(search.modules, [0]);
        assert_eq!(search.region, [0, 2, 3]);
        let mut cs = ChangeSet { changes: plan() };
        order_occupied_objects_under(&mut cs, &occupants, &[], "prod", &precedence).unwrap();
        assert_eq!(cs.changes[0].change, view.change, "{:?}", cs.changes);
        let at = |c: &Change| cs.changes.iter().position(|p| p.change == *c).unwrap();
        let renamed = cs
            .changes
            .iter()
            .position(|p| matches!(p.change, Change::RenameTable { .. }))
            .unwrap();
        assert!(
            renamed < at(&f.change) && at(&f.change) < at(&lookup.change),
            "{:?}",
            cs.changes
        );
        // Negative: without the pair it stays in class 0, and no order
        // clears the plan.
        let mut cs = ChangeSet { changes: plan() };
        order_occupied_objects_under(&mut cs, &occupants, &[], "prod", &[]).unwrap_err();
    }

    /// A drop on a renamed table names the table as it is called where the
    /// drop runs, whichever side of the rename the order puts it.
    #[test]
    fn a_drop_is_addressed_by_its_tables_name_where_it_runs() {
        let table = |cs: &[PlannedChange]| -> Vec<String> {
            cs.iter()
                .filter_map(|p| {
                    if let Change::DropCheck { table, .. }
                    | Change::DropComputedColumn { table, .. } = &p.change
                    {
                        Some(table.to_string())
                    } else {
                        None
                    }
                })
                .collect()
        };
        // A computed column's drop is addressed the same way (#1174).
        let computed = PlannedChange::new(Change::DropComputedColumn {
            table: name("s2.new"),
            name: "c".into(),
            computed: pbps_model::ComputedColumn {
                expression: "a * 2".into(),
                persisted: false,
                not_null: false,
            },
        });
        let after = [rename("s1.old", "s2.new", &[]), computed];
        let search = Search::new(&after, &[], &NameFacts::new(&[], &[]));
        assert_eq!(table(&search.reordered(&[0, 1])), ["s2.new"]);
        assert_eq!(table(&search.reordered(&[1, 0])), ["s1.old"]);
        let before = [drop_check("s1.old", "c"), rename("s1.old", "s2.new", &[])];
        let search = Search::new(&before, &[], &NameFacts::new(&[], &[]));
        assert_eq!(table(&search.reordered(&[0, 1])), ["s1.old"]);
        assert_eq!(table(&search.reordered(&[1, 0])), ["s2.new"]);
        let after = [rename("s1.old", "s2.new", &[]), drop_check("s2.new", "c")];
        let search = Search::new(&after, &[], &NameFacts::new(&[], &[]));
        assert_eq!(table(&search.reordered(&[0, 1])), ["s2.new"]);
        assert_eq!(table(&search.reordered(&[1, 0])), ["s1.old"]);
        // A table no rename moves keeps its name.
        let other = [drop_check("dbo.t", "c"), rename("s1.old", "s2.new", &[])];
        let search = Search::new(&other, &[], &NameFacts::new(&[], &[]));
        assert_eq!(table(&search.reordered(&[1, 0])), ["dbo.t"]);
    }
}
