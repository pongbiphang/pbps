use std::collections::{BTreeMap, BTreeSet};

use pbps_model::{
    Change, Presence, Renames, Schema, TableName,
    plan_names::{At, Holder, PlanNames},
};

use super::ColumnChanges;

/// Planned identity and observed execution answer different questions. The
/// index translates a surviving UID; the observed rename graph proves which
/// moves a read can undo without collapsing an impostor (DEC-536.1).
pub(crate) struct Tables<'a> {
    names: &'a PlanNames<'a>,
    renamed: BTreeMap<&'a TableName, &'a TableName>,
    retired: BTreeSet<&'a TableName>,
    kept: BTreeSet<&'a TableName>,
    final_presence: BTreeMap<&'a TableName, Presence>,
    pub(crate) undo_before: Renames,
    pub(crate) undo_after: Renames,
}

impl<'a> Tables<'a> {
    pub(crate) fn of(
        names: &'a PlanNames<'a>,
        before: &Schema,
        after: &Schema,
        columns: &ColumnChanges<'_>,
    ) -> Self {
        let mut renamed = BTreeMap::new();
        let mut dropped_tables = BTreeSet::new();
        let mut final_presence = BTreeMap::new();
        let mut touched = BTreeSet::new();
        for planned in names.changes() {
            let change = &planned.change;
            final_presence.extend(change.tables_after());
            touched.extend(change.table());
            if let Change::RenameTable { from, to, .. } = change {
                renamed.insert(from, to);
            }
            if let Change::DropTable { name, .. } = change {
                dropped_tables.insert(name);
            }
        }
        // Any change that works on an existing table keeps its presence,
        // unless an explicit table effect owns that presence (DEC-1687.2).
        let kept = touched
            .into_iter()
            .filter(|name| before.tables.contains_key(*name) && !final_presence.contains_key(*name))
            .collect();
        // A retired occupant is compared nowhere under its replacement's
        // name. The replacement keeps its own baseline (DEC-536.1).
        let retired = dropped_tables
            .iter()
            .copied()
            .filter(|name| {
                renamed
                    .iter()
                    .any(|(from, to)| to == name && before.tables.contains_key(*from))
            })
            .collect();
        // This plan's renames, backwards: the name each renamed thing had, keyed
        // by the name it ends with. The later of the two reads is brought back to
        // the earlier spelling with it, and then compared as it always was.
        //
        // Backwards rather than forwards, and part by part rather than whole,
        // because a staged run is checked after every statement: at a checkpoint
        // the single admitted rename may still be between schema transfer and
        // its final name. A child's references to other parents stay unchanged.
        // Undoing what *has* happened needs no knowledge of what has not (the
        // same reason `settled` exists, DECISIONS 161).
        //
        // A rename is undone only where the two names cannot be confused. If a
        // read holds *both* spellings, the vacated name has been taken by
        // something else — another session creating a table under it, or adding a
        // column back — and rewinding would collapse two identities into one:
        // a foreign key repointed at the impostor would compare equal to one
        // still on the original, and SPEC 7.6's promise that an untouched object's
        // change is caught at the first checkpoint after it lands would be broken.
        // Left un-rewound, the two spellings simply differ and the read is
        // refused, which is the safe direction for an ambiguity that a
        // name-keyed read cannot resolve. The cost is a plan that renames a table
        // and creates another under the vacated name in one revision: its
        // untouched children report as moved. Splitting that revision is the
        // answer, and refusing beats recording another session's write as this
        // plan's own.
        let holds = |s: &Schema, a: &TableName, b: &TableName| {
            s.tables.contains_key(a) && s.tables.contains_key(b)
        };
        // Except where this plan explains the second spelling (DEC-536.1): on the
        // earlier read, an occupant it drops or renames away, whose statement has
        // not run yet; on the later read, a table it renames into the source
        // name, the next link of a chain.
        //
        // Each read undoes only the renames that had run when it was taken: in a
        // chain, `z` on the earlier read is still `z`, and on the later one it is
        // the renamed `y`, so one map for both sides rewrote a reference that had
        // not moved. A rename has run when its source name is gone, or when the
        // table there is the next link's, and that link's rename has run.
        let refills: BTreeMap<&TableName, &TableName> =
            renamed.iter().map(|(from, to)| (*to, *from)).collect();
        let has_run = |s: &Schema, from: &TableName| {
            let mut name = from;
            for _ in 0..=renamed.len() {
                if !s.tables.contains_key(name) {
                    return true;
                }
                match refills.get(name) {
                    Some(next) => name = next,
                    None => return false,
                }
            }
            false
        };
        let mut undo_before = pbps_model::Renames::default();
        let mut undo_after = pbps_model::Renames::default();
        for (from, to) in &renamed {
            let vacated = dropped_tables.contains(to) || renamed.contains_key(to);
            let refilled = refills.contains_key(from);
            if (holds(before, from, to) && !vacated) || (holds(after, from, to) && !refilled) {
                continue;
            }
            if has_run(before, from) {
                undo_before.rename_table((*to).clone(), (*from).clone());
            }
            if has_run(after, from) {
                undo_after.rename_table((*to).clone(), (*from).clone());
            }
        }
        // The identity index supplies final table spellings. A staged table
        // rename is one logical change and carries no separate column rename;
        // ordinary column renames are still undone only on unambiguous reads.
        let holds_column = |s: &Schema,
                            table: &TableName,
                            a: &pbps_model::ColumnName,
                            b: &pbps_model::ColumnName| {
            s.tables
                .get(table)
                .is_some_and(|t| t.columns.contains_key(a) && t.columns.contains_key(b))
        };
        let changing_hands = &columns.handoffs;
        for (table, renames) in &columns.renamed {
            for (to, from) in renames {
                if changing_hands.contains_key(table) {
                    continue;
                }
                // Each read is undone only where it holds the rename's result alone.
                // One that holds both names has not run the rename — or holds a
                // dropped column under the name the rename takes — and undoing it
                // would move that column's constraints onto the renamed one. Both
                // reads were left alone when either held both, so a column renamed
                // into a dropped column's name kept its index under two spellings,
                // and a valid plan was refused at apply (#1699).
                let column = pbps_model::ColumnRef::new(table.clone(), to.clone());
                if !holds_column(before, table, from, to) {
                    undo_before.rename_column(column.clone(), from.clone());
                }
                if !holds_column(after, table, from, to) {
                    undo_after.rename_column(column, from.clone());
                }
            }
        }
        // The earlier read has run nothing, so only the later one is undone.
        for (table, net) in changing_hands {
            for (now, was) in net {
                if let Some(was) = was.as_ref().filter(|was| *was != now) {
                    undo_after
                        .rename_column(pbps_model::ColumnRef::new((*table).clone(), now), was);
                }
            }
        }

        Self {
            names,
            renamed,
            retired,
            kept,
            final_presence,
            undo_before,
            undo_after,
        }
    }

    pub(crate) fn final_name<'b>(&'b self, name: &'b TableName) -> &'b TableName {
        if self.renamed.contains_key(name)
            && let Holder::Tracked(uid) = self.names.holder(name, At::Start)
            && let Some(final_name) = self.names.table_at(uid, At::End)
        {
            return final_name;
        }
        name
    }

    pub(crate) fn retired(&self, name: &TableName) -> bool {
        self.retired.contains(name)
    }

    pub(crate) fn kept(&self, after: &Schema, moved: &mut Vec<String>) {
        for name in &self.kept {
            if !after.tables.contains_key(*name) {
                moved.push(format!(
                    "{name} is gone, and no change of this plan drops it",
                    name = name.shown()
                ));
            }
        }
    }

    pub(crate) fn after(&self, after: &Schema, moved: &mut Vec<String>) {
        for (name, expected) in &self.final_presence {
            match (expected, after.tables.contains_key(*name)) {
                (Presence::Present, false) => moved.push(format!(
                    "{name} is not there, and this plan creates it",
                    name = name.shown()
                )),
                (Presence::Absent, true) => moved.push(format!(
                    "{name} is still there, and this plan removes it",
                    name = name.shown()
                )),
                (Presence::Present, true) | (Presence::Absent, false) => {}
            }
        }
    }
}
