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

use pbps_model::{
    ColumnRef, IdsFile, Intent, ModuleId, Schema, TableName, Tombstone, Uid, UidKind,
};

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
    /// Roles both disappeared and appeared (ADR-0005).
    AmbiguousRoles {
        disappeared: Vec<String>,
        appeared: Vec<String>,
    },
    /// A role vanished from the declarations with no reason for dropping it.
    DropRoleNeedsReason { role: String },
    /// An intent was given that matches nothing in either the declarations or the
    /// identity file — almost always a typo.
    ///
    /// Ignoring it silently would leave the user facing an ambiguity error they
    /// cannot explain.
    UnusedIntent { intent: Intent },
    /// Two or more rename intents claim one name: one source renamed to two
    /// targets, or two sources renamed onto one name.
    ///
    /// Not an ambiguity to be resolved by choosing, which is why this carries
    /// no candidates. The matching loops consume from `disappeared` and
    /// `appeared`, so the first intent to reach a contested name takes it and
    /// the rest fall through — the winner is declaration order, which is not a
    /// decision anyone made.
    ConflictingRenameIntents {
        side: RenameSide,
        /// The contested name, spelled as the user wrote it: `dbo.t.old` for a
        /// column, `dbo.t` for a table, the bare name for a role.
        name: String,
        /// Every intent claiming it, in declaration order. Always more than
        /// one, and always of one kind.
        intents: Vec<Intent>,
    },
    /// A rename's target is already occupied by a name that remains in the
    /// declarations. The source is left available so a companion drop intent
    /// can still account for it, instead of making the rename look absorbed.
    RenameTargetExists { target: String },
    /// A schema, table, column, or module (view/routine/trigger) name pbps's
    /// own `.`-joined format cannot carry: it contains the separator that
    /// format reserves, and minting an identity for it — or, for a module,
    /// simply keeping it in the declarations, since a module's identity is
    /// its name — would write something the next load cannot read back
    /// (issue #108, DECISIONS 444, `pbps_model::check_segment`).
    ///
    /// Reached from both directions for a table or column: the loader
    /// already refuses this shape for a declared column at the point it is
    /// read (`pbps-load::convert`), but a *table* or *schema* part is never
    /// split apart from a single string there — `table:` is one YAML
    /// scalar, parsed whole — so it cannot carry an embedded `.` by
    /// construction. A live database has no such restriction: `[a.b]` is a
    /// legal bracket-quoted SQL Server identifier, and a dialect's
    /// introspection builds `TableName` from the catalog's separate schema
    /// and name columns directly, bypassing that parse. This is where every
    /// fresh identity — declared or pulled alike — is actually minted, so it
    /// is where an introspected name meets the same refusal a declared one
    /// already got earlier.
    ///
    /// A module carries no such loader-side defense at all — `resolve_modules`
    /// is the only check it gets, run every time rather than only on a fresh
    /// name, because a module has no persisted identity to compare against
    /// (see [`resolve_modules`]'s own doc comment).
    UnrepresentableName {
        /// `"schema"`, `"table"`, or `"column"` for the table/column case;
        /// `"view"`, `"routine"`, or `"trigger"` for the object's own name in
        /// the module case — which part, or which kind of module, contained
        /// the separator.
        what: &'static str,
        /// The offending part itself, not a joined name: joining it is
        /// exactly the operation that made it ambiguous in the first place.
        part: String,
        /// The table this part belongs to: the table a bad column is on, or
        /// the table a bad trigger is on. `None` for a table's own schema or
        /// name, a view's, or a routine's.
        table: Option<TableName>,
    },
}

/// The half of a rename that two intents fought over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameSide {
    /// They all rename *from* the contested name. The object it names can only
    /// become one of the targets.
    Source,
    /// They all rename *to* it. Only one object can end up with the name.
    Target,
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
    pub created_roles: Vec<(Uid, String)>,
    pub dropped_roles: Vec<(Uid, String)>,
    /// `(uid, old name, new name)`
    pub renamed_roles: Vec<(Uid, String, String)>,
}

pub fn resolve(
    declared: &Schema,
    ids: &IdsFile,
    intents: &[Intent],
    ctx: &Context,
) -> Result<Resolution, Vec<Blocker>> {
    resolve_with_provenance(declared, ids, intents, None, ctx)
}

/// Resolve intents while preserving which leading entries came from declaration
/// annotations. `pbps` appends CLI and prompt decisions after the annotations;
/// only an annotation that is now absorbed by a matching drop may be ignored
/// when its reused source name is removed. Explicit decisions must still report
/// an occupied target. The ordinary [`resolve`] entry point retains the legacy
/// unknown-provenance behavior used by dialect tests and library consumers.
pub fn resolve_with_annotations(
    declared: &Schema,
    ids: &IdsFile,
    intents: &[Intent],
    annotation_count: usize,
    ctx: &Context,
) -> Result<Resolution, Vec<Blocker>> {
    assert!(
        annotation_count <= intents.len(),
        "annotation count cannot exceed the intent count"
    );
    resolve_with_provenance(declared, ids, intents, Some(annotation_count), ctx)
}

fn resolve_with_provenance(
    declared: &Schema,
    ids: &IdsFile,
    intents: &[Intent],
    annotation_count: Option<usize>,
    ctx: &Context,
) -> Result<Resolution, Vec<Blocker>> {
    let mut r = Resolution {
        ids: ids.clone(),
        ..Default::default()
    };
    let mut blockers = Vec::new();
    let mut used: BTreeSet<usize> = BTreeSet::new();

    resolve_tables(
        declared,
        intents,
        annotation_count,
        ctx,
        &mut r,
        &mut blockers,
        &mut used,
    );
    resolve_columns(
        declared,
        intents,
        annotation_count,
        ctx,
        &mut r,
        &mut blockers,
        &mut used,
    );
    // Not "appeared" like tables and columns: a module carries no identity
    // to mint (ADR-0002, DECISIONS 200 — a view/routine/trigger is compared
    // by its own name every run, never assigned a `Uid`), so there is no
    // "already known" set to skip. Every declared module is checked every
    // time, which the sweep this round found the original fix had missed
    // (issue #108, DECISIONS 444): `ModuleId` has the identical `.`-joined
    // round-trip hazard `TableName`/`ColumnRef` do, and both `pull` and
    // `init --from` can hand it a module name a live database allows but
    // this format cannot carry.
    resolve_modules(declared, &mut blockers);

    resolve_roles(
        declared,
        intents,
        annotation_count,
        ctx,
        &mut r,
        &mut blockers,
        &mut used,
    );

    // An intent a conflict has already named is not unused, however many times
    // it was written and whatever the matching loop did with it. Asked by
    // equality rather than by index: the guard's own bookkeeping was the wrong
    // place for this, and three review rounds found three ways for an index to
    // go missing before the shape was moved here, where a contender cannot be
    // both reported as contested and reported as a likely typo (DECISIONS 246).
    let contested: BTreeSet<Intent> = blockers
        .iter()
        .filter_map(|b| {
            if let Blocker::ConflictingRenameIntents { intents, .. } = b {
                Some(intents.clone())
            } else {
                None
            }
        })
        .flatten()
        .collect();

    // Repeated statements share the matched statement's disposition. Absorption
    // itself must use the input: newly minted identities cannot validate a typo,
    // and reusing a retired name cannot invalidate an already applied annotation.
    let accounted: BTreeSet<&Intent> = used.iter().map(|&i| &intents[i]).collect();
    for (i, intent) in intents.iter().enumerate() {
        // A current command is a new identity decision, not a retained
        // annotation whose scope moved with its declaration -- and not one the
        // declarations may excuse either: its author has to hear that it
        // matched nothing.
        let current_decision = annotation_count.is_some_and(|count| i >= count);
        // Column annotations use the declared table name. A table rename moves
        // their scope, but does not make newly added columns prior identities.
        let original_intent = match intent {
            _ if current_decision => None,
            Intent::RenameColumn { table, from, to } => {
                r.renamed_tables
                    .iter()
                    .find_map(|(_, old_table, new_table)| {
                        (table == new_table).then(|| Intent::RenameColumn {
                            table: old_table.clone(),
                            from: from.clone(),
                            to: to.clone(),
                        })
                    })
            }
            Intent::DropColumn { column, reason } => {
                r.renamed_tables
                    .iter()
                    .find_map(|(_, old_table, new_table)| {
                        (&column.table == new_table).then(|| Intent::DropColumn {
                            column: old_table.column(&column.name),
                            reason: reason.clone(),
                        })
                    })
            }
            Intent::RenameTable { .. }
            | Intent::DropTable { .. }
            | Intent::RenameRole { .. }
            | Intent::DropRole { .. } => None,
        };
        // The identities are read in the namespace this annotation was written
        // against, the declarations in the namespace the user wrote: a table
        // rename moves the first and not the second, so the two halves of one
        // question take two spellings of one intent.
        let source = if current_decision {
            RenameSource::Vacated
        } else {
            RenameSource::of(intent, declared)
        };
        if !accounted.contains(intent)
            && !contested.contains(intent)
            && !intent_is_absorbed(original_intent.as_ref().unwrap_or(intent), ids, source)
        {
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

/// One rename intent that could match in the scope being resolved: what it
/// says, and the two names it claims.
struct Claim<'a, T> {
    intent: &'a Intent,
    source: T,
    target: T,
}

/// The rename claims that contend for one name, on either side.
///
/// One implementation for tables, columns and roles. The three matching loops
/// are the same code over three types, and a rule with three homes is three
/// chances to be fixed once (`docs/PITFALLS.md`, "One rule, spelled in three
/// places"). DECISIONS 246.
///
/// **Only claims that could match here.** The caller passes intents whose
/// source is in `disappeared` and whose target is in `appeared`, and that
/// filter is the whole difference between a conflict and a leftover. A
/// `renamed_from` annotation lives on until `pbps fmt` strips it, so an
/// annotation recording a rename that already happened is *expected* to be
/// sitting in the file — and if its vacated source name has since been reused
/// by a new object that this revision renames, grouping the two by their
/// shared source would refuse a plan that is not ambiguous at all. The stale
/// one cannot match: its target is on both sides of the declarations, so it is
/// not in `appeared`.
///
/// **Identical claims are one claim.** The same rename reaches `resolve` twice
/// whenever an annotation is also answered at the interactive prompt, and
/// refusing that would refuse a valid plan for saying one true thing twice, so
/// the repeats are collapsed before the claimants are counted.
///
/// Nothing here marks an intent used. A contender does not need it: the sweep
/// at the end of [`resolve`] skips every intent a conflict names, by equality,
/// so being reported as contested and being reported as a likely typo are
/// mutually exclusive by construction rather than by bookkeeping.
///
/// The caller raises what this returns and then carries on. It must not skip
/// its matching loop over a conflict, though the loop can no longer produce a
/// correct answer: `resolve` discards its whole `Resolution` when it returns
/// `Err`, so the order-dependent decision goes nowhere — while skipping the
/// loop leaves every *other* intent of that kind unmatched, and the sweep then
/// reports each of those as a likely typo too.
fn contested_rename_claims<T: Ord + Clone + std::fmt::Display>(
    claims: &[Claim<'_, T>],
) -> (Vec<Blocker>, BTreeSet<String>) {
    let mut out = Vec::new();
    let mut conflicting_sources = BTreeSet::new();
    let pass = |side: RenameSide,
                pick: for<'a> fn(&'a Claim<'_, T>) -> &'a T,
                out: &mut Vec<Blocker>,
                conflicting_sources: &mut BTreeSet<String>| {
        let mut grouped: BTreeMap<&T, Vec<&Claim<'_, T>>> = BTreeMap::new();
        for c in claims {
            grouped.entry(pick(c)).or_default().push(c);
        }
        for (name, group) in grouped {
            // One statement written twice is still one statement, so the
            // repeats are collapsed before the claimants are counted.
            let mut distinct: Vec<&Intent> = Vec::new();
            for c in &group {
                if !distinct.contains(&c.intent) {
                    distinct.push(c.intent);
                }
            }
            if distinct.len() < 2 {
                continue;
            }
            conflicting_sources.extend(group.iter().map(|c| c.source.to_string()));
            out.push(Blocker::ConflictingRenameIntents {
                side,
                name: name.to_string(),
                intents: distinct.into_iter().cloned().collect(),
            });
        }
    };
    pass(
        RenameSide::Source,
        |c| &c.source,
        &mut out,
        &mut conflicting_sources,
    );
    pass(
        RenameSide::Target,
        |c| &c.target,
        &mut out,
        &mut conflicting_sources,
    );
    (out, conflicting_sources)
}

/// Whether this intent has already taken effect — its fact is in the ids file.
///
/// Intents have to be idempotent. A `renamed_from` annotation stays in the
/// declaration file after the identity file has been updated (only `pbps fmt`
/// clears it), and reporting it as an unmatched intent would hand the user a
/// baffling failure right after a successful rename.
///
/// The test is whether the world is already in the shape the intent asks for: for
/// a rename, the target name exists and the source name is no longer one this
/// rename has to vacate ([`RenameSource`]); for a drop, the object is already
/// gone from the identity file.
///
/// Public because `pbps fmt` shares this exact judgement: an annotation whose
/// intent is absorbed is redundant and gets stripped, while a pending one must
/// survive the rewrite (SPEC §6.2). Two definitions of "absorbed" would drift.
pub fn intent_is_absorbed(intent: &Intent, ids: &IdsFile, source: RenameSource) -> bool {
    let has_column = |c: &ColumnRef| ids.column_uid(c).is_some();
    let has_table = |t: &TableName| ids.table_uid(t).is_some();
    let has_role = |r: &str| ids.role_uid(r).is_some();
    // A live source name stops being evidence of a pending rename once the
    // declarations ask for that name again: what holds it then is the new
    // object, not the one this rename is still waiting to move.
    let vacated = |live: bool| !live || source == RenameSource::Redeclared;

    match intent {
        Intent::RenameTable { from, to } => has_table(to) && vacated(has_table(from)),
        Intent::RenameColumn { table, from, to } => {
            has_column(&table.column(to)) && vacated(has_column(&table.column(from)))
        }
        // A drop asks this question about one name, and no declaration file can
        // carry one: `renamed_from` is the only annotation that becomes an
        // intent, so every drop here is a current command. Reading the
        // declarations for it would silence `pbps drop` against a column that
        // is still declared, which is a contradiction the author must hear.
        Intent::DropTable { table, .. } => !has_table(table),
        Intent::DropColumn { column, .. } => !has_column(column),
        Intent::RenameRole { from, to } => has_role(to) && vacated(has_role(from)),
        Intent::DropRole { role, .. } => !has_role(role),
    }
}

/// What the declarations say about the name a rename intent claims to have left
/// behind.
///
/// The identity file records no history, so "the rename happened and a new
/// object has since taken the vacated name" and "this rename never happened"
/// reach [`intent_is_absorbed`] as the same two live names. The declarations
/// settle it: a pending rename is precisely a name the declarations have given
/// up, so a source name they ask for again cannot be one a rename is still
/// waiting to leave.
///
/// Without this, reusing a vacated name converged on nothing: the run that
/// reused it succeeded, and every run after it refused the unchanged
/// declarations as an unused intent while `fmt` kept the annotation that caused
/// it, leaving the user to delete the line by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameSource {
    /// The declarations do not ask for the source name -- or the caller cannot
    /// see the declarations, or is judging an explicit command, which must be
    /// reported when it matched nothing whatever they say. The identity file
    /// answers alone, which keeps an annotation rather than stripping one that
    /// cannot be proved redundant.
    Vacated,
    /// The declarations ask for the source name again.
    Redeclared,
}

impl RenameSource {
    /// Reads the state of `intent`'s source name out of `declared`.
    ///
    /// `intent` must be spelled as the user wrote it, because `declared` is: a
    /// table rename moves a column annotation's scope in the identity file and
    /// not in the file that declares it, so the two halves of absorption are
    /// asked about two spellings of one intent.
    pub fn of(intent: &Intent, declared: &Schema) -> Self {
        let redeclared = match intent {
            Intent::RenameTable { from, .. } => declared.tables.contains_key(from),
            Intent::RenameColumn { table, from, .. } => declared
                .tables
                .get(table)
                .is_some_and(|t| t.columns.contains_key(from)),
            Intent::RenameRole { from, .. } => declared.roles.contains_key(from),
            // A drop vacates no name, and its absorption does not ask.
            Intent::DropTable { .. } | Intent::DropColumn { .. } | Intent::DropRole { .. } => false,
        };
        if redeclared {
            Self::Redeclared
        } else {
            Self::Vacated
        }
    }
}

/// Roles, by the same rules as tables (ADR-0005): a name on both sides is the
/// same role, a rename needs intent, and a drop needs a reason. Membership is
/// what drop + add would destroy, which is why a role is on this side of the
/// line at all.
fn resolve_roles(
    declared: &Schema,
    intents: &[Intent],
    annotation_count: Option<usize>,
    ctx: &Context,
    r: &mut Resolution,
    blockers: &mut Vec<Blocker>,
    used: &mut BTreeSet<usize>,
) {
    let declared_names: BTreeSet<&String> = declared.roles.keys().collect();
    let known: BTreeMap<String, Uid> = r
        .ids
        .roles
        .iter()
        .map(|(u, n)| (n.clone(), u.clone()))
        .collect();

    let mut appeared: BTreeSet<String> = declared_names
        .iter()
        .filter(|n| !known.contains_key(**n))
        .map(|n| (*n).clone())
        .collect();
    let mut disappeared: BTreeSet<String> = known
        .keys()
        .filter(|n| !declared_names.contains(n))
        .cloned()
        .collect();

    let claims: Vec<Claim<'_, &String>> = intents
        .iter()
        .filter_map(|intent| {
            let Intent::RenameRole { from, to } = intent else {
                return None;
            };
            (disappeared.contains(from) && appeared.contains(to)).then_some(Claim {
                intent,
                source: from,
                target: to,
            })
        })
        .collect();
    let (rename_blockers, conflicting_sources) = contested_rename_claims(&claims);
    blockers.extend(rename_blockers);

    // A target that is already known and still declared is not in `appeared`,
    // so this rename cannot match. Report that specific collision before the
    // matching loop and leave `from` in `disappeared`; a companion drop intent
    // must still be able to account for the source. Only a leading annotation
    // may defer to another matchable claim for that source; an appended current
    // decision must be judged on its own, or the annotation's match can make a
    // different command report success. A leading annotation paired with a
    // drop is stale provenance. Account for these recognized annotations here,
    // since the final absorption check describes the input identities. Deduplicate
    // by target so two statements against one occupied name describe one
    // collision rather than repeating the same diagnosis.
    let mut occupied_targets = BTreeSet::new();
    for (i, intent) in intents.iter().enumerate() {
        let Intent::RenameRole { from, to } = intent else {
            continue;
        };
        let current_decision = annotation_count.is_some_and(|count| i >= count);
        let stale_annotation = annotation_count.is_some_and(|count| i < count)
            && intents.iter().any(
                |candidate| matches!(candidate, Intent::DropRole { role, .. } if role == from),
            );
        if disappeared.contains(from) && declared_names.contains(&to) && known.contains_key(to) {
            if (current_decision || !claims.iter().any(|claim| claim.source == from))
                && !stale_annotation
                && occupied_targets.insert(to.clone())
            {
                blockers.push(Blocker::RenameTargetExists { target: to.clone() });
            }
            used.insert(i);
        }
    }

    // Raised, and then everything below runs as it always did. Returning here
    // was the obvious move and the wrong one: the loop's decision is discarded
    // anyway — `resolve` throws `r` away when it returns `Err` — while
    // returning early strands every *other* intent of this kind unmatched, and
    // the sweep at the end then calls a perfectly good annotation a likely
    // typo. The contenders are already marked used, which is what the sweep
    // has to be told; the bystanders match their way to the same place.
    for (i, intent) in intents.iter().enumerate() {
        if let Intent::RenameRole { from, to } = intent
            && disappeared.contains(from)
            && appeared.contains(to)
        {
            disappeared.remove(from);
            appeared.remove(to);
            let uid = known[from].clone();
            r.ids.roles.insert(uid.clone(), to.clone());
            r.renamed_roles.push((uid, from.clone(), to.clone()));
            used.insert(i);
        }
    }

    for (i, intent) in intents.iter().enumerate() {
        if let Intent::DropRole { role, reason } = intent
            && disappeared.remove(role)
        {
            let uid = known[role].clone();
            r.ids.roles.remove(&uid);
            r.ids.tombstones.insert(
                uid.clone(),
                Tombstone {
                    was: role.clone(),
                    dropped_at: ctx.today.clone(),
                    reason: reason.clone(),
                    operator: ctx.operator.clone(),
                },
            );
            r.dropped_roles.push((uid, role.clone()));
            used.insert(i);
        }
    }

    if !appeared.is_empty() && !disappeared.is_empty() {
        blockers.push(Blocker::AmbiguousRoles {
            disappeared: disappeared.into_iter().collect(),
            appeared: appeared.into_iter().collect(),
        });
        return;
    }

    for role in disappeared {
        if !conflicting_sources.contains(&role) {
            blockers.push(Blocker::DropRoleNeedsReason { role });
        }
    }

    for name in appeared {
        let uid = fresh_uid(&r.ids, UidKind::Role);
        r.ids.roles.insert(uid.clone(), name.clone());
        r.created_roles.push((uid, name));
    }
}

fn resolve_tables(
    declared: &Schema,
    intents: &[Intent],
    annotation_count: Option<usize>,
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

    let claims: Vec<Claim<'_, &TableName>> = intents
        .iter()
        .filter_map(|intent| {
            let Intent::RenameTable { from, to } = intent else {
                return None;
            };
            (disappeared.contains(from) && appeared.contains(to)).then_some(Claim {
                intent,
                source: from,
                target: to,
            })
        })
        .collect();
    let (rename_blockers, conflicting_sources) = contested_rename_claims(&claims);
    blockers.extend(rename_blockers);

    // Check both sets before the matching loop mutates either one. If the
    // target is a known name that remains declared, it is an occupied target,
    // not an unused rename; keep the source in `disappeared` for a companion
    // drop intent and account for the rename in the final sweep. Only a leading
    // annotation may defer to another matchable claim for that source; an
    // appended current decision must be judged on its own, or the annotation's
    // match can make a different command report success. A leading annotation
    // paired with a drop is stale provenance. Account for these recognized
    // annotations here, since the final absorption check describes the input
    // identities. Deduplicate by target so two statements against one occupied
    // name describe one collision rather than repeating the diagnosis.
    let mut occupied_targets = BTreeSet::new();
    for (i, intent) in intents.iter().enumerate() {
        let Intent::RenameTable { from, to } = intent else {
            continue;
        };
        let current_decision = annotation_count.is_some_and(|count| i >= count);
        let stale_annotation = annotation_count.is_some_and(|count| i < count)
            && intents.iter().any(
                |candidate| matches!(candidate, Intent::DropTable { table, .. } if table == from),
            );
        if disappeared.contains(from) && declared_names.contains(&to) && known.contains_key(to) {
            if (current_decision || !claims.iter().any(|claim| claim.source == from))
                && !stale_annotation
                && occupied_targets.insert(to.clone())
            {
                blockers.push(Blocker::RenameTargetExists {
                    target: to.clone().to_string(),
                });
            }
            used.insert(i);
        }
    }

    // Rename wins over drop: if both intents are given for one table, rename is
    // the more specific statement.
    for (i, intent) in intents.iter().enumerate() {
        if let Intent::RenameTable { from, to } = intent
            && disappeared.contains(from)
            && appeared.contains(to)
        {
            disappeared.remove(from);
            appeared.remove(to);
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
        if !conflicting_sources.contains(&table.to_string()) {
            blockers.push(Blocker::DropTableNeedsReason { table });
        }
    }

    for name in appeared {
        // The loader already refuses this shape for a table written as one
        // `schema.table` scalar (`pbps-load::convert`), so a declared table
        // never reaches here with an embedded `.`. A pulled one can: a
        // dialect's introspection builds `TableName` straight from the
        // catalog's separate schema and name columns, and `[a.b]` is a legal
        // bracket-quoted identifier on the live database this name came from
        // (issue #108). Caught here rather than minted: an identity for it
        // would write a name the next load could not read back.
        if pbps_model::check_segment(&name.schema).is_err() {
            blockers.push(Blocker::UnrepresentableName {
                what: "schema",
                part: name.schema.clone(),
                table: None,
            });
            continue;
        }
        if pbps_model::check_segment(&name.name).is_err() {
            blockers.push(Blocker::UnrepresentableName {
                what: "table",
                part: name.name.clone(),
                table: None,
            });
            continue;
        }
        let uid = fresh_uid(&r.ids, UidKind::Table);
        r.ids.tables.insert(uid.clone(), name.clone());
        r.created_tables.push((uid, name));
    }
}

fn resolve_columns(
    declared: &Schema,
    intents: &[Intent],
    annotation_count: Option<usize>,
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
        // Table resolution has already moved column owners to their new names;
        // this stage deliberately reads that output, before changing columns.
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

        let claims: Vec<Claim<'_, ColumnRef>> = intents
            .iter()
            .filter_map(|intent| {
                let Intent::RenameColumn { table, from, to } = intent else {
                    return None;
                };
                (table == table_name && disappeared.contains(from) && appeared.contains(to)).then(
                    || Claim {
                        intent,
                        // Qualified, so the blocker names the column the way the
                        // user would go looking for it.
                        source: table_name.column(from),
                        target: table_name.column(to),
                    },
                )
            })
            .collect();
        let (rename_blockers, conflicting_sources) = contested_rename_claims(&claims);
        blockers.extend(rename_blockers);

        // The target must be checked while both sets still describe the
        // baseline. In particular, do not consume `from` when `to` is already
        // occupied by a declared column: a companion drop intent still needs
        // to see that source in `disappeared`. Only a leading annotation may
        // defer to another matchable claim for that source; an appended current
        // decision must be judged on its own, or the annotation's match can
        // make a different command report success. A leading annotation paired
        // with a drop is stale provenance. Account for these recognized
        // annotations here, since absorption describes the input identities.
        // Deduplicate by target to keep one diagnosis per name.
        let mut occupied_targets = BTreeSet::new();
        for (i, intent) in intents.iter().enumerate() {
            let Intent::RenameColumn { table, from, to } = intent else {
                continue;
            };
            let current_decision = annotation_count.is_some_and(|count| i >= count);
            let stale_annotation = annotation_count.is_some_and(|count| i < count)
                && intents.iter().any(|candidate| {
                    matches!(candidate, Intent::DropColumn { column, .. }
                        if &column.table == table_name && column.name == *from)
                });
            if table == table_name
                && disappeared.contains(from)
                && declared_cols.contains(&to)
                && known.contains_key(to)
            {
                if (current_decision || !claims.iter().any(|claim| claim.source.name == *from))
                    && !stale_annotation
                    && occupied_targets.insert(to.clone())
                {
                    blockers.push(Blocker::RenameTargetExists {
                        target: table_name.column(to).to_string(),
                    });
                }
                used.insert(i);
            }
        }

        for (i, intent) in intents.iter().enumerate() {
            if let Intent::RenameColumn { table, from, to } = intent
                && table == table_name
                && disappeared.contains(from)
                && appeared.contains(to)
            {
                disappeared.remove(from);
                appeared.remove(to);
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
            if !conflicting_sources.contains(&table_name.column(&name).to_string()) {
                blockers.push(Blocker::DropColumnNeedsReason {
                    column: table_name.column(name),
                });
            }
        }

        for name in appeared {
            // Same refusal as the loader's, reached from the other
            // direction: a column pulled from a live database never went
            // through `pbps-load::convert`'s check on the way in — a
            // dialect's introspection reads the catalog's column name
            // straight into `Table::columns`, and a double-quoted or
            // bracket-quoted `a.b` is a legal identifier there too (issue
            // #108). This is the one place a fresh column identity is
            // minted, declared or pulled alike, so it is where that check
            // has to live for pull.
            if pbps_model::check_segment(&name).is_err() {
                blockers.push(Blocker::UnrepresentableName {
                    what: "column",
                    part: name,
                    table: Some(table_name.clone()),
                });
                continue;
            }
            let uid = fresh_uid(&r.ids, UidKind::Column);
            let col = table_name.column(name);
            r.ids.columns.insert(uid.clone(), col.clone());
            r.added_columns.push((uid, col));
        }
    }
}

/// Every declared module's identity, checked for the same `.`-joined
/// round-trip hazard as a table or column name (issue #108, DECISIONS 444) —
/// but validated only, never minted. A module is deliberately not
/// identity-tracked (ADR-0002, DECISIONS 200): its key is recomputed from the
/// declarations on every run rather than assigned a `Uid`, so there is no
/// "already known" set the way `resolve_tables`/`resolve_columns` have, and
/// nothing to skip. Every call re-checks every declared module; that costs
/// nothing next to the `Vec<RoutineArg>` comparisons `Schema` equality
/// already does on the same map.
///
/// A routine's own argument **types** are deliberately left unchecked. A `.`
/// there is not this format's separator meeting itself — it is a legitimate
/// schema-qualified type name (`dl.money$type`), which `RoutineArg` stores as
/// opaque text and never splits apart on `.` to recover a schema and a type
/// separately (see its own doc comment in `pbps-model::module`); the
/// argument list is delimited by commas and balanced parentheses/brackets,
/// not by `.`, so a dot inside one argument's text cannot be confused with
/// the boundary between two arguments or between a schema and a name.
/// Checking it would refuse a routine that already round-trips correctly —
/// exactly the false-refusal direction issue #108 exists to avoid, not
/// produce.
fn resolve_modules(declared: &Schema, blockers: &mut Vec<Blocker>) {
    for id in declared.modules.keys() {
        match id {
            ModuleId::Named(n) => check_object_name(n, "view", None, blockers),
            ModuleId::Routine(r) => {
                check_object_name(&r.name, "routine", None, blockers);
                // `r.args` is not checked — see this function's doc comment.
            }
            ModuleId::Trigger { on, name } => {
                check_object_name(on, "table", None, blockers);
                if pbps_model::check_segment(name).is_err() {
                    blockers.push(Blocker::UnrepresentableName {
                        what: "trigger",
                        part: name.clone(),
                        table: Some(on.clone()),
                    });
                }
            }
        }
    }
}

/// Checks both parts of a `schema.name` identity — a table, a view, or a
/// routine's own name, before its argument list — against the separator this
/// format reserves, pushing one [`Blocker::UnrepresentableName`] per bad part.
///
/// `kind` names what a bad **name** part is (`"table"`, `"view"`,
/// `"routine"`); a bad schema part is always reported as `"schema"`,
/// regardless of `kind`, because the remedy is the same rename either way and
/// the report is about the securable a `GRANT`-shaped remedy would need to
/// name, not about which kind of object sits in it.
fn check_object_name(
    n: &TableName,
    kind: &'static str,
    table: Option<TableName>,
    blockers: &mut Vec<Blocker>,
) {
    if pbps_model::check_segment(&n.schema).is_err() {
        blockers.push(Blocker::UnrepresentableName {
            what: "schema",
            part: n.schema.clone(),
            table: table.clone(),
        });
    }
    if pbps_model::check_segment(&n.name).is_err() {
        blockers.push(Blocker::UnrepresentableName {
            what: kind,
            part: n.name.clone(),
            table,
        });
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
        if !ids.contains_uid(&u) {
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
    r.created_roles.sort();
    r.dropped_roles.sort();
    r.renamed_roles.sort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Column, Table};

    fn schema(columns: &[&str]) -> Schema {
        let mut table = Table::default();
        for name in columns {
            table
                .columns
                .insert((*name).into(), Column::new("int".parse().unwrap()));
        }
        let mut schema = Schema::default();
        schema.tables.insert("dbo.customer".parse().unwrap(), table);
        schema
    }

    fn ctx() -> Context {
        Context {
            operator: "test".into(),
            today: "2026-09-11".into(),
        }
    }

    #[test]
    fn an_absorbed_column_rename_allows_reusing_its_source_name() {
        let before = resolve(&schema(&["code_v1"]), &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let table: TableName = "dbo.customer".parse().unwrap();
        let intent = Intent::RenameColumn {
            table: table.clone(),
            from: "code".into(),
            to: "code_v1".into(),
        };
        let result = resolve(&schema(&["code_v1", "code"]), &before, &[intent], &ctx()).unwrap();
        assert!(result.renamed_columns.is_empty());
        assert_eq!(result.added_columns.len(), 1);
        assert_eq!(result.added_columns[0].1, table.column("code"));
        assert_eq!(
            result.ids.column_uid(&table.column("code_v1")),
            before.column_uid(&table.column("code_v1"))
        );
    }

    #[test]
    fn a_typo_in_a_new_columns_rename_source_is_not_absorbed() {
        let before = resolve(&schema(&["id"]), &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let intent = Intent::RenameColumn {
            table: "dbo.customer".parse().unwrap(),
            from: "custmer_name".into(),
            to: "full_name".into(),
        };
        let errors = resolve(
            &schema(&["id", "full_name"]),
            &before,
            std::slice::from_ref(&intent),
            &ctx(),
        )
        .unwrap_err();
        assert_eq!(errors, vec![Blocker::UnusedIntent { intent }]);
    }

    #[test]
    fn a_table_rename_preserves_absorbed_column_intents_without_absorbing_typos() {
        let before = resolve(&schema(&["code_v1"]), &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let old_table: TableName = "dbo.customer".parse().unwrap();
        let new_table: TableName = "dbo.clients".parse().unwrap();
        let mut declared = schema(&["code_v1", "code"]);
        let table = declared.tables.remove(&old_table).unwrap();
        declared.tables.insert(new_table.clone(), table);
        let rename_table = Intent::RenameTable {
            from: old_table.clone(),
            to: new_table.clone(),
        };
        let annotation = Intent::RenameColumn {
            table: new_table.clone(),
            from: "code".into(),
            to: "code_v1".into(),
        };
        let result = resolve(
            &declared,
            &before,
            &[rename_table.clone(), annotation],
            &ctx(),
        )
        .unwrap();
        assert_eq!(result.renamed_tables.len(), 1);
        assert!(result.renamed_columns.is_empty());
        assert_eq!(result.added_columns.len(), 1);
        assert_eq!(
            result.ids.column_uid(&new_table.column("code_v1")),
            before.column_uid(&old_table.column("code_v1"))
        );

        let typo = Intent::RenameColumn {
            table: new_table,
            from: "custmer_code".into(),
            to: "code".into(),
        };
        let errors =
            resolve(&declared, &before, &[rename_table, typo.clone()], &ctx()).unwrap_err();
        assert_eq!(errors, vec![Blocker::UnusedIntent { intent: typo }]);
    }

    /// Reusing a vacated source name is only worth allowing if the revision
    /// that does it converges: the annotation is still in the file on the next
    /// run, and the run that recorded the new column must not be the reason the
    /// next one is refused.
    #[test]
    fn a_reused_source_name_stays_absorbed_on_the_following_run() {
        let before = resolve(&schema(&["code_v1"]), &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let table: TableName = "dbo.customer".parse().unwrap();
        let declared = schema(&["code_v1", "code"]);
        let annotation = Intent::RenameColumn {
            table,
            from: "code".into(),
            to: "code_v1".into(),
        };
        let after = resolve(
            &declared,
            &before,
            std::slice::from_ref(&annotation),
            &ctx(),
        )
        .unwrap()
        .ids;
        let again = resolve(&declared, &after, std::slice::from_ref(&annotation), &ctx())
            .expect("the annotation the first run absorbed cannot refuse the next plan");
        assert!(again.renamed_columns.is_empty());
        assert!(again.added_columns.is_empty());
        assert_eq!(
            again.ids, after,
            "an unchanged revision records nothing new"
        );
        // And `pbps fmt` can strip the line, or the file never converges either.
        assert!(intent_is_absorbed(
            &annotation,
            &after,
            RenameSource::of(&annotation, &declared)
        ));
    }

    /// A redeclared source name excuses a retained annotation, never a command
    /// the user just ran: `pbps rename` against a column the declarations still
    /// ask for matched nothing, and its author is the one person who has to hear
    /// that. The two provenances get different answers to one statement.
    #[test]
    fn a_current_rename_onto_a_taken_name_is_reported_where_the_annotation_is_not() {
        let declared = schema(&["code_v1", "code"]);
        let ids = resolve(&declared, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let statement = Intent::RenameColumn {
            table: "dbo.customer".parse().unwrap(),
            from: "code".into(),
            to: "code_v1".into(),
        };
        let intents = std::slice::from_ref(&statement);
        let errors = resolve_with_annotations(&declared, &ids, intents, 0, &ctx()).unwrap_err();
        assert_eq!(
            errors,
            vec![Blocker::UnusedIntent {
                intent: statement.clone()
            }]
        );
        resolve_with_annotations(&declared, &ids, intents, 1, &ctx())
            .expect("the same statement as a retained annotation is redundant, not a typo");
    }

    /// The declarations are read where the user wrote them and the identities
    /// where the annotation was written, so a table rename in the same revision
    /// moves one and not the other.
    #[test]
    fn a_reused_source_name_stays_absorbed_while_its_table_is_renamed() {
        let old_table: TableName = "dbo.customer".parse().unwrap();
        let new_table: TableName = "dbo.clients".parse().unwrap();
        let before = resolve(&schema(&["code_v1"]), &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let annotation = Intent::RenameColumn {
            table: old_table.clone(),
            from: "code".into(),
            to: "code_v1".into(),
        };
        let after = resolve(
            &schema(&["code_v1", "code"]),
            &before,
            std::slice::from_ref(&annotation),
            &ctx(),
        )
        .unwrap()
        .ids;

        let mut declared = schema(&["code_v1", "code"]);
        let table = declared.tables.remove(&old_table).unwrap();
        declared.tables.insert(new_table.clone(), table);
        let intents = [
            Intent::RenameTable {
                from: old_table,
                to: new_table.clone(),
            },
            Intent::RenameColumn {
                table: new_table.clone(),
                from: "code".into(),
                to: "code_v1".into(),
            },
        ];
        let r = resolve(&declared, &after, &intents, &ctx())
            .expect("a retained annotation whose source was reused survives its table moving");
        assert_eq!(r.renamed_tables.len(), 1);
        assert!(r.renamed_columns.is_empty());
        assert!(r.added_columns.is_empty());
        assert_eq!(
            r.ids.column_uid(&new_table.column("code")),
            after.column_uid(&"dbo.customer.code".parse().unwrap())
        );
    }

    #[test]
    fn a_current_column_command_is_not_absorbed_through_a_table_rename() {
        let before = resolve(&schema(&["code_v1"]), &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let old_table: TableName = "dbo.customer".parse().unwrap();
        let new_table: TableName = "dbo.clients".parse().unwrap();
        let mut declared = schema(&["code_v1"]);
        let table = declared.tables.remove(&old_table).unwrap();
        declared.tables.insert(new_table.clone(), table);
        let intents = [
            Intent::RenameTable {
                from: old_table,
                to: new_table.clone(),
            },
            Intent::RenameColumn {
                table: new_table,
                from: "missing_code".into(),
                to: "code_v1".into(),
            },
        ];
        let errors = resolve_with_annotations(&declared, &before, &intents, 1, &ctx()).unwrap_err();
        assert_eq!(
            errors,
            vec![Blocker::UnusedIntent {
                intent: intents[1].clone()
            }]
        );
        resolve_with_annotations(&declared, &before, &intents, 2, &ctx())
            .expect("the same retained annotation remains harmless after its table moves");
    }
}
