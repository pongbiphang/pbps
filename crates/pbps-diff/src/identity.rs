//! Identity resolution: mapping the names in the declarations back to UIDs and
//! finding the changes a human has to adjudicate.
//!
//! # The core problem
//!
//! The identity file records the last known uid-to-name mapping. The declarations
//! record what is wanted now. Comparing the two produces three cases: a name is
//! on both sides (the same thing), only in the declarations (new), or only in the
//! identity file (it disappeared).
//!
//! The trouble is the last two happening **at the same time**: when one table
//! loses a column and gains another, there is no structural way to tell a rename
//! from a drop plus an add. That information exists only in the author's head, so
//! it has to be supplied ([`Intent`]) and the algorithm is not allowed to guess —
//! the price of guessing wrong is lost data.
//!
//! # Why a drop needs a reason too
//!
//! A pure deletion with no additions in the same table really is unambiguous as
//! an operation. But the tombstone has to answer an audit's "who dropped this,
//! when, and why", and no algorithm can produce the why. So a drop needs an
//! intent as well — the difference being that what it asks for is not "was this a
//! rename" but "why".

use std::collections::{BTreeMap, BTreeSet};

use pbps_model::{ColumnRef, IdsFile, Intent, Schema, TableName, Tombstone, Uid, UidKind};

/// A situation that cannot be decided automatically and needs a human.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    /// One table both lost and gained columns.
    AmbiguousColumns {
        table: TableName,
        disappeared: Vec<String>,
        appeared: Vec<String>,
    },
    /// Tables both disappeared and appeared.
    AmbiguousTables {
        disappeared: Vec<TableName>,
        appeared: Vec<TableName>,
    },
    /// A column vanished from the declarations with no reason for dropping it.
    DropColumnNeedsReason { column: ColumnRef },
    /// A table vanished from the declarations with no reason for dropping it.
    DropTableNeedsReason { table: TableName },
    /// An intent was given that matches nothing in either the declarations or the
    /// identity file — almost always a typo.
    ///
    /// Ignoring it silently would leave the user facing an ambiguity error they
    /// cannot explain.
    UnusedIntent { intent: Intent },
}

/// Information a tombstone needs that cannot be derived from the files.
#[derive(Debug, Clone)]
pub struct Context {
    pub operator: String,
    /// `YYYY-MM-DD`. Supplied by the caller so that this layer stays pure and
    /// testable.
    pub today: String,
}

/// The result of identity resolution: identity-level facts only, no attribute
/// changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolution {
    /// The identity file with this round's changes applied.
    pub ids: IdsFile,
    pub created_tables: Vec<(Uid, TableName)>,
    pub dropped_tables: Vec<(Uid, TableName)>,
    /// `(uid, old name, new name)`
    pub renamed_tables: Vec<(Uid, TableName, TableName)>,
    pub added_columns: Vec<(Uid, ColumnRef)>,
    pub dropped_columns: Vec<(Uid, ColumnRef)>,
    pub renamed_columns: Vec<(Uid, ColumnRef, ColumnRef)>,
}

pub fn resolve(
    declared: &Schema,
    ids: &IdsFile,
    intents: &[Intent],
    ctx: &Context,
) -> Result<Resolution, Vec<Blocker>> {
    let mut r = Resolution {
        ids: ids.clone(),
        ..Default::default()
    };
    let mut blockers = Vec::new();
    let mut used: BTreeSet<usize> = BTreeSet::new();

    resolve_tables(declared, intents, ctx, &mut r, &mut blockers, &mut used);
    resolve_columns(declared, intents, ctx, &mut r, &mut blockers, &mut used);

    for (i, intent) in intents.iter().enumerate() {
        if !used.contains(&i) && !intent_is_absorbed(intent, &r.ids) {
            blockers.push(Blocker::UnusedIntent {
                intent: intent.clone(),
            });
        }
    }

    if blockers.is_empty() {
        sort_resolution(&mut r);
        Ok(r)
    } else {
        Err(blockers)
    }
}

/// Whether this intent has already taken effect — its fact is in the ids file.
///
/// Intents have to be idempotent. A `renamed_from` annotation stays in the
/// declaration file after the identity file has been updated (only `pbps fmt`
/// clears it), and reporting it as an unmatched intent would hand the user a
/// baffling failure right after a successful rename.
///
/// The test is whether the world is already in the shape the intent asks for: for
/// a rename, the target name exists and the source name does not; for a drop, the
/// object is already gone from the identity file.
///
/// Public because `pbps fmt` shares this exact judgement: an annotation whose
/// intent is absorbed is redundant and gets stripped, while a pending one must
/// survive the rewrite (SPEC §6.2). Two definitions of "absorbed" would drift.
pub fn intent_is_absorbed(intent: &Intent, ids: &IdsFile) -> bool {
    let has_column = |c: &ColumnRef| ids.column_uid(c).is_some();
    let has_table = |t: &TableName| ids.table_uid(t).is_some();

    match intent {
        Intent::RenameTable { from, to } => has_table(to) && !has_table(from),
        Intent::RenameColumn { table, from, to } => {
            has_column(&table.column(to)) && !has_column(&table.column(from))
        }
        Intent::DropTable { table, .. } => !has_table(table),
        Intent::DropColumn { column, .. } => !has_column(column),
    }
}

fn resolve_tables(
    declared: &Schema,
    intents: &[Intent],
    ctx: &Context,
    r: &mut Resolution,
    blockers: &mut Vec<Blocker>,
    used: &mut BTreeSet<usize>,
) {
    let declared_names: BTreeSet<&TableName> = declared.tables.keys().collect();
    // An owned copy: the loop below reads while mutating `r.ids`.
    let known: BTreeMap<TableName, Uid> = r
        .ids
        .tables
        .iter()
        .map(|(u, n)| (n.clone(), u.clone()))
        .collect();

    let mut appeared: BTreeSet<TableName> = declared_names
        .iter()
        .filter(|n| !known.contains_key(**n))
        .map(|n| (*n).clone())
        .collect();
    let mut disappeared: BTreeSet<TableName> = known
        .keys()
        .filter(|n| !declared_names.contains(n))
        .cloned()
        .collect();

    // Rename wins over drop: if both intents are given for one table, rename is
    // the more specific statement.
    for (i, intent) in intents.iter().enumerate() {
        if let Intent::RenameTable { from, to } = intent
            && disappeared.remove(from)
            && appeared.remove(to)
        {
            let uid = known[from].clone();
            rename_table_in_ids(&mut r.ids, &uid, from, to);
            r.renamed_tables.push((uid, from.clone(), to.clone()));
            used.insert(i);
        }
    }

    for (i, intent) in intents.iter().enumerate() {
        if let Intent::DropTable { table, reason } = intent
            && disappeared.remove(table)
        {
            let uid = known[table].clone();
            drop_table_in_ids(&mut r.ids, &uid, table, reason, ctx);
            r.dropped_tables.push((uid, table.clone()));
            used.insert(i);
        }
    }

    if !appeared.is_empty() && !disappeared.is_empty() {
        blockers.push(Blocker::AmbiguousTables {
            disappeared: disappeared.into_iter().collect(),
            appeared: appeared.into_iter().collect(),
        });
        return;
    }

    for table in disappeared {
        blockers.push(Blocker::DropTableNeedsReason { table });
    }

    for name in appeared {
        let uid = fresh_uid(&r.ids, UidKind::Table);
        r.ids.tables.insert(uid.clone(), name.clone());
        r.created_tables.push((uid, name));
    }
}

fn resolve_columns(
    declared: &Schema,
    intents: &[Intent],
    ctx: &Context,
    r: &mut Resolution,
    blockers: &mut Vec<Blocker>,
    used: &mut BTreeSet<usize>,
) {
    // Only tables still present in the declarations are handled here. Columns of
    // a newly created table are all additions, and for a dropped table the
    // tombstones for its columns are handled in drop_table_in_ids.
    for (table_name, table) in &declared.tables {
        let declared_cols: BTreeSet<&String> = table.columns.keys().collect();
        let known: BTreeMap<String, Uid> = r
            .ids
            .columns
            .iter()
            .filter(|(_, c)| &c.table == table_name)
            .map(|(u, c)| (c.name.clone(), u.clone()))
            .collect();

        let mut appeared: BTreeSet<String> = declared_cols
            .iter()
            .filter(|n| !known.contains_key(**n))
            .map(|n| (*n).clone())
            .collect();
        let mut disappeared: BTreeSet<String> = known
            .keys()
            .filter(|n| !declared_cols.contains(n))
            .cloned()
            .collect();

        for (i, intent) in intents.iter().enumerate() {
            if let Intent::RenameColumn { table, from, to } = intent
                && table == table_name
                && disappeared.remove(from)
                && appeared.remove(to)
            {
                let uid = known[from].clone();
                let old = table_name.column(from);
                let new = table_name.column(to);
                r.ids.columns.insert(uid.clone(), new.clone());
                r.renamed_columns.push((uid, old, new));
                used.insert(i);
            }
        }

        for (i, intent) in intents.iter().enumerate() {
            if let Intent::DropColumn { column, reason } = intent
                && &column.table == table_name
                && disappeared.remove(&column.name)
            {
                let uid = known[&column.name].clone();
                r.ids.columns.remove(&uid);
                r.ids.tombstones.insert(
                    uid.clone(),
                    Tombstone {
                        was: column.to_string(),
                        dropped_at: ctx.today.clone(),
                        reason: reason.clone(),
                        operator: ctx.operator.clone(),
                    },
                );
                r.dropped_columns.push((uid, column.clone()));
                used.insert(i);
            }
        }

        if !appeared.is_empty() && !disappeared.is_empty() {
            blockers.push(Blocker::AmbiguousColumns {
                table: table_name.clone(),
                disappeared: disappeared.into_iter().collect(),
                appeared: appeared.into_iter().collect(),
            });
            continue;
        }

        for name in disappeared {
            blockers.push(Blocker::DropColumnNeedsReason {
                column: table_name.column(name),
            });
        }

        for name in appeared {
            let uid = fresh_uid(&r.ids, UidKind::Column);
            let col = table_name.column(name);
            r.ids.columns.insert(uid.clone(), col.clone());
            r.added_columns.push((uid, col));
        }
    }
}

/// When a table is renamed, every column reference beneath it has to move too:
/// the columns' identities have not changed, but their qualified names contain
/// the table name. Skip this and the next diff sees every column in the table as
/// brand new.
fn rename_table_in_ids(ids: &mut IdsFile, uid: &Uid, from: &TableName, to: &TableName) {
    ids.tables.insert(uid.clone(), to.clone());
    let moved: Vec<(Uid, ColumnRef)> = ids
        .columns
        .iter()
        .filter(|(_, c)| &c.table == from)
        .map(|(u, c)| (u.clone(), to.column(&c.name)))
        .collect();
    for (u, c) in moved {
        ids.columns.insert(u, c);
    }
}

fn drop_table_in_ids(ids: &mut IdsFile, uid: &Uid, table: &TableName, reason: &str, ctx: &Context) {
    let make = |was: String| Tombstone {
        was,
        dropped_at: ctx.today.clone(),
        reason: reason.to_owned(),
        operator: ctx.operator.clone(),
    };

    ids.tables.remove(uid);
    ids.tombstones.insert(uid.clone(), make(table.to_string()));

    let cols: Vec<(Uid, ColumnRef)> = ids
        .columns
        .iter()
        .filter(|(_, c)| &c.table == table)
        .map(|(u, c)| (u.clone(), c.clone()))
        .collect();
    for (u, c) in cols {
        ids.columns.remove(&u);
        ids.tombstones.insert(u, make(c.to_string()));
    }
}

/// Allocates a UID that does not collide with an existing identity.
///
/// Collisions are very rare, but silently reusing an identity would directly
/// produce a wrong rename decision, so one extra check is worth it.
fn fresh_uid(ids: &IdsFile, kind: UidKind) -> Uid {
    loop {
        let u = Uid::generate(kind);
        if !ids.tables.contains_key(&u)
            && !ids.columns.contains_key(&u)
            && !ids.tombstones.contains_key(&u)
        {
            return u;
        }
    }
}

/// Output order must be stable, or identical input would produce plans and
/// diagnostics in differing orders.
fn sort_resolution(r: &mut Resolution) {
    r.created_tables.sort();
    r.dropped_tables.sort();
    r.renamed_tables.sort();
    r.added_columns.sort();
    r.dropped_columns.sort();
    r.renamed_columns.sort();
}
