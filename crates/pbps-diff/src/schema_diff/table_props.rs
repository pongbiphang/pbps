//! Table-level properties: storage parameters, replica identity, filegroups, and the foreign-key graph a persistence switch reads.

use super::*;

/// The declared foreign-key graph's strongly connected components, and how
/// deep each table sits over them: 0 for a table that references no other,
/// else one more than the deepest table it references outside its own
/// component. A table outside the schema counts as depth 0, so a key to one
/// makes its table 1. Self-references do not count.
///
/// Found in one iterative Tarjan pass, linear in tables and keys (#1792). The
/// earlier version kept every table's full reachable set to answer the same
/// question, which was quadratic in memory on a chain of keys, and found each
/// depth by recursion, one stack frame per link. Tarjan completes a component
/// only after every component it reaches, so each member's depth is final
/// when its component is popped.
pub(super) struct ForeignKeyGraph<'a> {
    index: BTreeMap<&'a TableName, usize>,
    component: Vec<usize>,
    depth: Vec<usize>,
}

impl<'a> ForeignKeyGraph<'a> {
    pub(super) fn of(schema: &'a Schema) -> Self {
        let names: Vec<&TableName> = schema.tables.keys().collect();
        let index: BTreeMap<&TableName, usize> =
            names.iter().enumerate().map(|(i, t)| (*t, i)).collect();
        let n = names.len();
        let mut edges: Vec<Vec<usize>> = vec![Vec::new(); n];
        // A key to a table the schema does not declare: no edge, but depth 1.
        let mut external = vec![false; n];
        for (i, (name, table)) in schema.tables.iter().enumerate() {
            for fk in table.foreign_keys.values() {
                let to = &fk.references_table;
                if to == name {
                    continue;
                }
                match index.get(to) {
                    Some(&j) => edges[i].push(j),
                    None => external[i] = true,
                }
            }
        }

        const UNSEEN: usize = usize::MAX;
        let mut order = vec![UNSEEN; n];
        let mut low = vec![0; n];
        let mut on_stack = vec![false; n];
        let mut stack = Vec::new();
        let mut component = vec![UNSEEN; n];
        let mut depth = vec![0; n];
        let mut visited = 0;
        let mut components = 0;
        // The recursion, made explicit: each frame is a table and the index
        // of the next of its edges to follow.
        let mut frames: Vec<(usize, usize)> = Vec::new();
        for root in 0..n {
            if order[root] != UNSEEN {
                continue;
            }
            order[root] = visited;
            low[root] = visited;
            visited += 1;
            stack.push(root);
            on_stack[root] = true;
            frames.push((root, 0));
            while let Some(&(v, e)) = frames.last() {
                if let Some(&w) = edges[v].get(e) {
                    if let Some(frame) = frames.last_mut() {
                        frame.1 += 1;
                    }
                    if order[w] == UNSEEN {
                        order[w] = visited;
                        low[w] = visited;
                        visited += 1;
                        stack.push(w);
                        on_stack[w] = true;
                        frames.push((w, 0));
                    } else if on_stack[w] {
                        low[v] = low[v].min(order[w]);
                    }
                    continue;
                }
                frames.pop();
                if let Some(&(u, _)) = frames.last() {
                    low[u] = low[u].min(low[v]);
                }
                if low[v] != order[v] {
                    continue;
                }
                // `v` roots a component: it and everything above it on the
                // stack. Every component its members reach is complete.
                let at = stack.iter().rposition(|&x| x == v).unwrap_or(0);
                let members: Vec<usize> = stack.drain(at..).collect();
                for &m in &members {
                    on_stack[m] = false;
                    component[m] = components;
                }
                for &m in &members {
                    depth[m] = edges[m]
                        .iter()
                        .filter(|&&w| component[w] != components)
                        .map(|&w| 1 + depth[w])
                        .max()
                        .unwrap_or(0)
                        .max(usize::from(external[m]));
                }
                components += 1;
            }
        }
        Self {
            index,
            component,
            depth,
        }
    }

    /// Whether both are declared and reach each other through foreign keys.
    pub(super) fn same_component(&self, a: &TableName, b: &TableName) -> bool {
        match (self.index.get(a), self.index.get(b)) {
            (Some(&i), Some(&j)) => self.component[i] == self.component[j],
            _ => false,
        }
    }

    /// 0 for a table the schema does not declare.
    pub(super) fn depth(&self, table: &TableName) -> usize {
        self.index.get(table).map_or(0, |&i| self.depth[i])
    }

    pub(super) fn deepest(&self) -> usize {
        self.depth.iter().copied().max().unwrap_or(0)
    }
}

/// Foreign keys inside a cycle of tables the plan switches between permanent
/// and unlogged, dropped before the switches and added back after them
/// (#1488 review). Inside a cycle no order works: whichever table switches
/// first breaks a key of the other, permanent referencing unlogged. A valid
/// declaration gives a cycle's tables one persistence (a permanent table
/// referencing an unlogged one is refused), so they switch together. A key
/// the plan already adds, new or rebuilt, stands neither side of the switch
/// and is left alone.
pub(super) fn unlink_cycles_around_persistence_switches(
    declared: &Schema,
    changes: &mut Vec<Change>,
) {
    let switching: BTreeSet<TableName> = changes
        .iter()
        .filter_map(|c| {
            if let Change::TableProperty(TablePropertyChange::SetTablePersistence {
                table, ..
            }) = c
            {
                Some(table.clone())
            } else {
                None
            }
        })
        .collect();
    if switching.len() < 2 {
        return;
    }
    let graph = ForeignKeyGraph::of(declared);
    let mut unlinked = Vec::new();
    for (name, table) in &declared.tables {
        if !switching.contains(name) {
            continue;
        }
        for (key, fk) in &table.foreign_keys {
            let to = &fk.references_table;
            let in_cycle = to != name && switching.contains(to) && graph.same_component(name, to);
            let added = changes.iter().any(|c| {
                matches!(c, Change::Constraint(ConstraintChange::AddForeignKey { table: t, name: n, .. }) if t == name && n == key)
            });
            if in_cycle && !added {
                unlinked.push(Change::Constraint(ConstraintChange::DropForeignKey {
                    table: name.clone(),
                    name: key.clone(),
                }));
                unlinked.push(Change::Constraint(ConstraintChange::AddForeignKey {
                    table: name.clone(),
                    name: key.clone(),
                    constraint: Box::new(fk.clone()),
                }));
            }
        }
    }
    changes.extend(unlinked);
}

/// A PostgreSQL table's heap storage parameters (DEC-1441.1): one change
/// setting each declared value that differs from the base's, and resetting
/// each the base has and the declaration does not. Both sides are canonical,
/// so a respelling is no change.
pub(super) fn diff_storage_parameters(
    uid: &Uid,
    name: &TableName,
    base: &Table,
    declared: &Table,
    changes: &mut Vec<Change>,
) {
    let set: BTreeMap<String, String> = declared
        .storage_parameters
        .iter()
        .filter(|(k, v)| base.storage_parameters.get(*k) != Some(*v))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let reset: BTreeSet<String> = base
        .storage_parameters
        .keys()
        .filter(|k| !declared.storage_parameters.contains_key(*k))
        .cloned()
        .collect();
    if !set.is_empty() || !reset.is_empty() {
        changes.push(Change::TableProperty(
            TablePropertyChange::SetStorageParameters {
                uid: uid.clone(),
                table: name.clone(),
                set,
                reset,
            },
        ));
    }
}

/// A PostgreSQL table's replica identity, for each table on both sides
/// (DEC-1444.1). Planned where it changes, and where the plan adds the object
/// whose index it names, new or rebuilt: PostgreSQL lets that index be
/// dropped and leaves the identity naming nothing, and an index created again
/// under the same name is not the identity until it is set again (measured on
/// 16 and 18, #1444).
///
/// Returns the changes, by table uid and target, that run before the plan's
/// drops: those whose target the table can take as it stands, which is every
/// target the plan does not add, when the columns it is over are NOT NULL
/// already. The rest wait for the additions of class 13.
///
/// One that waits while the plan drops the old identity's index is preceded
/// by `FULL`, which every table can take: between the drop and the final
/// setting the table would otherwise identify no row, which the reader leaves
/// out as unreadable, so a staged checkpoint there could not be resumed
/// (#1467 review).
pub(super) fn diff_replica_identity(
    base: &Schema,
    declared: &Schema,
    declared_tables: &BTreeMap<Uid, TableName>,
    base_tables: &BTreeMap<Uid, TableName>,
    changes: &mut Vec<Change>,
) -> BTreeSet<(Uid, Option<ReplicaIdentity>)> {
    let mut early = BTreeSet::new();
    for (uid, declared_name) in declared_tables {
        let Some(base_name) = base_tables.get(uid) else {
            continue;
        };
        let (Some(was), Some(table)) = (
            base.tables.get(base_name),
            declared.tables.get(declared_name),
        ) else {
            continue;
        };
        let readded = changes.iter().any(|c| match (c, &table.replica_identity) {
            (
                Change::Constraint(ConstraintChange::SetPrimaryKey {
                    table: t,
                    to: Some(_),
                    ..
                }),
                Some(ReplicaIdentity::PrimaryKey),
            ) => t == declared_name,
            (
                Change::Constraint(ConstraintChange::AddUnique { table: t, name, .. }),
                Some(ReplicaIdentity::Unique(n)),
            )
            | (
                Change::Index(IndexChange::AddIndex { table: t, name, .. }),
                Some(ReplicaIdentity::Index(n)),
            ) => t == declared_name && name == n,
            _ => false,
        });
        if was.replica_identity == table.replica_identity && !readded {
            continue;
        }
        let mut probe = was.clone();
        probe.replica_identity.clone_from(&table.replica_identity);
        let now = !readded && probe.replica_identity_problems().is_empty();
        let on = |t: &TableName| t == base_name || t == declared_name;
        let old_index_dropped = changes.iter().any(|c| match (c, &was.replica_identity) {
            (
                Change::Constraint(ConstraintChange::SetPrimaryKey {
                    table: t,
                    from: Some(_),
                    ..
                }),
                Some(ReplicaIdentity::PrimaryKey),
            ) => on(t),
            (
                Change::Constraint(ConstraintChange::DropUnique { table: t, name }),
                Some(ReplicaIdentity::Unique(n)),
            )
            | (
                Change::Index(IndexChange::DropIndex { table: t, name }),
                Some(ReplicaIdentity::Index(n)),
            ) => on(t) && name == n,
            _ => false,
        });
        if now {
            early.insert((uid.clone(), table.replica_identity.clone()));
        } else if old_index_dropped {
            early.insert((uid.clone(), Some(ReplicaIdentity::Full)));
            changes.push(Change::TableProperty(
                TablePropertyChange::SetReplicaIdentity {
                    uid: uid.clone(),
                    table: base_name.clone(),
                    to: Some(ReplicaIdentity::Full),
                },
            ));
        }
        changes.push(Change::TableProperty(
            TablePropertyChange::SetReplicaIdentity {
                uid: uid.clone(),
                table: if now { base_name } else { declared_name }.clone(),
                to: table.replica_identity.clone(),
            },
        ));
    }
    early
}

/// A SQL Server table's, key's and index's filegroup, for each that stays
/// (#2813, DEC-2813.1). **Every change is refused here, by name.** Moving the
/// data is a rebuild of the object, and the plan holds no design for moving
/// it, so a plan that would silently leave an object where it is, or drop and
/// create it elsewhere under a user's rows, is refused instead. An object the
/// plan adds is built where its declaration says; one it removes has nothing
/// to compare.
pub(super) fn diff_filegroups(
    name: &TableName,
    base: &Table,
    declared: &Table,
    errs: &mut Vec<DiffError>,
) {
    let mut check = |target: String,
                     was: Option<&pbps_model::Filegroup>,
                     now: Option<&pbps_model::Filegroup>| {
        if was != now {
            let show = |f: Option<&pbps_model::Filegroup>| match f {
                Some(f) => format!("`{}`", pbps_model::escape::Escaped(f.as_str())),
                None => "none declared".to_owned(),
            };
            errs.push(DiffError::FilegroupChangeRefused {
                table: name.clone(),
                target,
                was: show(was),
                now: show(now),
            });
        }
    };
    check(
        "the table".to_owned(),
        base.filegroup.as_ref(),
        declared.filegroup.as_ref(),
    );
    if let (Some(was), Some(now)) = (&base.primary_key, &declared.primary_key) {
        check(
            "the primary key".to_owned(),
            was.filegroup.as_ref(),
            now.filegroup.as_ref(),
        );
    }
    for (n, was) in &base.unique {
        if let Some(now) = declared.unique.get(n) {
            check(
                format!("unique constraint {}", n.shown()),
                was.filegroup.as_ref(),
                now.filegroup.as_ref(),
            );
        }
    }
    for (n, was) in &base.indexes {
        if let Some(now) = declared.indexes.get(n) {
            check(
                format!("index {}", n.shown()),
                was.filegroup.as_ref(),
                now.filegroup.as_ref(),
            );
        }
    }
}
