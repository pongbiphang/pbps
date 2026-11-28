//! Exclusion constraints: their diff, the hazards a change carries, and when one adopts another.

use super::constraints::may_share_key;
use super::*;

/// Whether `key` is held by `exclusion` as a plain column key excluded with
/// `=`: what PostgreSQL 18 requires of each partition-key column of a
/// partitioned table's exclusion (DEC-2448.6). An `INCLUDE` column, a cast and
/// an expression over the column do not hold it (measured on 18.6).
pub fn holds_key_with_equality(
    exclusion: &pbps_model::ExclusionConstraint,
    key: &ColumnName,
) -> bool {
    exclusion
        .keys
        .iter()
        .any(|k| k.key.key.column() == Some(key) && k.operator.as_symbol() == "=")
}

/// Whether a parent's new exclusion can take a partition's own as its clone:
/// the predicate validation and the differ's refusal of an own the plan adds
/// share (DEC-2448.6). What a plan can drop instead is asked of
/// [`may_adopt_exclusion`] (DEC-1671.1). Measured on 18.6, on `ATTACH` and on a parent's `ADD` over a
/// standing partition: the method, the keys in order with their classes,
/// collations and operators, the `INCLUDE` columns and the predicate must
/// match; the timing, the storage parameters and the name need not. As for
/// an index, an expression key or a predicate matches whatever its text, since
/// the engine compares what each binds to, but an expression key's class is
/// compared, as [`adopts`] compares it (DEC-2961.1). Its collation is not:
/// the model holds none for an expression key (`IndexColumn::collation`).
pub fn adopts_exclusion(
    parent: &pbps_model::ExclusionConstraint,
    own: &pbps_model::ExclusionConstraint,
) -> bool {
    parent.method == own.method
        && parent.filter.is_some() == own.filter.is_some()
        && parent.include == own.include
        && parent.keys.len() == own.keys.len()
        && parent.keys.iter().zip(&own.keys).all(|(p, o)| {
            p.operator == o.operator
                && match (p.key.key.column(), o.key.key.column()) {
                    (Some(p_column), Some(o_column)) => {
                        p_column == o_column
                            && p.key.opclass == o.key.opclass
                            && p.key.collation == o.key.collation
                    }
                    (None, None) => p.key.opclass == o.key.opclass,
                    _ => true,
                }
        })
}

/// Whether a parent's exclusion might take a partition's own as its clone, by
/// a comparison looser than [`adopts_exclusion`]'s, as [`may_adopt`] is for an
/// index (#1671, DEC-1671.1): the number of keys, and at each position the same
/// column ([`may_share_key`]). Operators, method, predicate and `INCLUDE` are
/// left out.
pub fn may_adopt_exclusion(
    parent: &pbps_model::ExclusionConstraint,
    own: &pbps_model::ExclusionConstraint,
    dialect: &(impl Dialect + ?Sized),
) -> bool {
    parent.keys.len() == own.keys.len()
        && parent
            .keys
            .iter()
            .zip(&own.keys)
            .all(|(p, o)| may_share_key(&p.key.key, &o.key.key, dialect))
}

/// A table's exclusion constraints, by name (DEC-2448.3, DEC-2448.4).
///
/// - **Added**: a change of its own.
/// - **Options only** (storage parameters): `ALTER INDEX .. SET/RESET` on the
///   backing index, the constraint standing (measured on 16.15 and 18.6), as an
///   index's are, under the method's per-parameter verdicts.
/// - **Any other difference at one name**: a replacement, as two changes (the
///   old one's drop and the new one's add) so each takes the ordering class its
///   direction needs. It needs no reason: the name keeps its logical member,
///   and the plan states the drop and the validation.
/// - **Gone from the declaration** (which a rename is, with an add beside it):
///   a drop, and it needs the retirement a human recorded for *this*
///   definition, else it is refused with the command that records it. The
///   table's own drop never gets here: its intent covers its parts.
pub(super) fn diff_exclusions(
    name: &TableName,
    base: &Table,
    declared: &Table,
    retired: &dyn Fn(&pbps_model::ObjectLocalName) -> bool,
    changes: &mut Vec<Change>,
    errs: &mut Vec<DiffError>,
) {
    let drop_half = |key: &pbps_model::ObjectLocalName, was: &pbps_model::ExclusionConstraint| {
        Change::Constraint(ConstraintChange::SetExclusion {
            table: name.clone(),
            name: key.clone(),
            from: Some(Box::new(was.clone())),
            to: None,
            rehearsal: None,
        })
    };
    let add_half = |key: &pbps_model::ObjectLocalName, now: &pbps_model::ExclusionConstraint| {
        Change::Constraint(ConstraintChange::SetExclusion {
            table: name.clone(),
            name: key.clone(),
            from: None,
            to: Some(Box::new(now.clone())),
            rehearsal: None,
        })
    };
    for (key, was) in &base.exclude {
        match declared.exclude.get(key) {
            Some(now) if now == was => {}
            Some(now) => {
                let bare = |x: &pbps_model::ExclusionConstraint| {
                    let mut x = x.clone();
                    x.storage_parameters.clear();
                    x
                };
                let edit = (bare(was) == bare(now)).then(|| {
                    parameter_edit(
                        was.method.policy(),
                        &was.storage_parameters,
                        &now.storage_parameters,
                    )
                });
                match edit {
                    // A partitioned parent's index takes no `ALTER INDEX SET`/`RESET`
                    // (42809, measured on 18.6 for its exclusion's index): the edit is a
                    // replacement, whose add carries the final options (DEC-2449.1).
                    Some(ParameterEdit::Alter { .. }) if declared.partition_by.is_some() => {
                        changes.push(drop_half(key, was));
                        changes.push(add_half(key, now));
                    }
                    Some(ParameterEdit::Alter { set, reset }) => {
                        changes.push(Change::Index(IndexChange::SetIndexStorageParameters {
                            table: name.clone(),
                            target: IndexPart::Exclusion(key.clone()),
                            method: was.method,
                            set,
                            reset,
                        }));
                    }
                    Some(ParameterEdit::Refuse { parameter }) => {
                        errs.push(DiffError::IndexParameterRefused {
                            table: name.clone(),
                            target: format!("exclusion constraint {}", key.shown()),
                            method: was.method.as_str(),
                            parameter,
                        });
                    }
                    Some(ParameterEdit::Unchanged) | Some(ParameterEdit::Rebuild) | None => {
                        changes.push(drop_half(key, was));
                        changes.push(add_half(key, now));
                    }
                }
            }
            None if retired(key) => changes.push(drop_half(key, was)),
            None => errs.push(DiffError::ExclusionRefused {
                table: name.clone(),
                name: key.clone(),
                reason: format!(
                    "dropping it (a rename is a drop and an add) needs a recorded reason: run \
                     `pbps drop-exclusion {} {} --reason \"..\"` and commit the identity file \
                     (`--since REV` names the revision that still declares it, if the \
                     declarations that dropped it are committed already)",
                    name.shown(),
                    key.shown()
                ),
            }),
        }
    }
    for (key, now) in &declared.exclude {
        if !base.exclude.contains_key(key) {
            changes.push(add_half(key, now));
        }
    }
}

/// A column change under a standing exclusion constraint (DEC-2448.3, DEC-2448.4,
/// DEC-2448.5). The engine drops an exclusion silently when a plain key column is
/// dropped, and silently rebuilds it, under an exclusive lock, when one is
/// retyped or a generated one has its expression set (measured on 16.15 and
/// 18.6): a guarantee given up, or a rebuild, that no plan states.
///
/// - A **drop** is allowed where the same plan drops the constraint, which is
///   the statement the engine would otherwise have made unseen (a pure drop has
///   passed the retirement gate, a replacement states the drop and the
///   validation). Alone it is refused by name.
/// - A **retype** or an **expression** change where the plan does not already
///   drop or replace the constraint is stated: the constraint's drop and its add
///   of the declared definition are two changes beside it, as a replacement's
///   are, in the classes that put the drop before the column change and the add
///   after it. A conflicting rebuild then fails as one transaction with the old
///   constraint standing, and the plan shows the `destructive` and `constraint`
///   risks the engine would have taken unseen. The definition is declared,
///   because a differing one is a replacement already.
///
/// A key that is an expression, or a predicate, names its columns in text this
/// model does not parse, so a column its text mentions counts as one it holds.
#[allow(clippy::wildcard_enum_match_arm)]
pub(super) fn exclusion_hazards<'a>(
    name: &TableName,
    base: &Table,
    declared: Option<&Table>,
    changes: impl IntoIterator<Item = &'a Change>,
    dialect: &(impl Dialect + ?Sized),
    errs: &mut Vec<DiffError>,
) -> Vec<Change> {
    let mut stated = Vec::new();
    if base.exclude.is_empty() {
        return stated;
    }
    let changes: Vec<&Change> = changes.into_iter().collect();
    let released = |key: &pbps_model::ObjectLocalName| {
        changes.iter().any(|c| {
            matches!(c, Change::Constraint(ConstraintChange::SetExclusion { table, name: n, to: None, .. })
                if table == name && n == key)
        })
    };
    let handled = |key: &pbps_model::ObjectLocalName| {
        changes.iter().any(|c| {
            matches!(c, Change::Constraint(ConstraintChange::SetExclusion { table, name: n, .. })
                if table == name && n == key)
        })
    };
    for change in changes.iter().copied() {
        let (column, what) = match change {
            Change::Column(ColumnChange::DropColumn { column, .. }) => (column, "dropped"),
            Change::Column(ColumnChange::AlterColumnType { column, .. })
            | Change::Column(ColumnChange::AlterColumnExpression { column, .. }) => {
                (column, "retyped")
            }
            Change::TypeValue(TypeValueChange::EnumRebuild {
                step: pbps_model::EnumRebuildStep::Retype { column, .. },
                ..
            }) => (column, "retyped"),
            _ => continue,
        };
        for (key, x) in &base.exclude {
            if !exclusion_names(x, &column.name, dialect) {
                continue;
            }
            if what == "dropped" {
                if !released(key) {
                    errs.push(DiffError::ExclusionRefused {
                        table: name.clone(),
                        name: key.clone(),
                        reason: format!(
                            "column `{}` is dropped while the constraint holds it; drop the \
                             constraint in the same plan (`pbps drop-exclusion`), or the \
                             engine would drop it without a plan saying so",
                            column.name.shown()
                        ),
                    });
                }
            } else if !handled(key) && !stated.iter().any(|s| exclusion_change_of(s, name, key)) {
                // Gone from the declarations is the retirement gate's refusal, not
                // this one's; a definition that differs is a replacement, and
                // `handled` has seen it.
                if let Some(now) = declared.and_then(|t| t.exclude.get(key)) {
                    stated.push(Change::Constraint(ConstraintChange::SetExclusion {
                        table: name.clone(),
                        name: key.clone(),
                        from: Some(Box::new(x.clone())),
                        to: None,
                        rehearsal: None,
                    }));
                    stated.push(Change::Constraint(ConstraintChange::SetExclusion {
                        table: name.clone(),
                        name: key.clone(),
                        from: None,
                        to: Some(Box::new(now.clone())),
                        rehearsal: None,
                    }));
                }
            }
        }
    }
    stated
}

fn exclusion_change_of(
    change: &Change,
    table: &TableName,
    key: &pbps_model::ObjectLocalName,
) -> bool {
    matches!(change, Change::Constraint(ConstraintChange::SetExclusion { table: t, name: n, .. })
        if t == table && n == key)
}

/// Whether an exclusion constraint's keys, payload or predicate hold `column`.
///
/// A key's expression and the predicate are asked of the dialect's own column
/// scan, the one an index key's input columns are asked of
/// ([`Dialect::generation_may_read_column`]): it reads quoted identifiers by
/// the engine's rules (a hyphen, a space, a doubled quote, a mixed case), and
/// leaves string literals and comments out, so the name inside a literal is not
/// a reference. It over-approximates (a function or a field of the same name
/// counts), which is the safe side of a guard.
fn exclusion_names(
    x: &pbps_model::ExclusionConstraint,
    column: &ColumnName,
    dialect: &(impl Dialect + ?Sized),
) -> bool {
    let reads = |text: &str| dialect.generation_may_read_column(text, column);
    x.keys.iter().any(|k| match &k.key.key {
        pbps_model::IndexKey::Column(c) => c == column,
        pbps_model::IndexKey::Expression(text) => reads(text),
    }) || x.include.contains(column)
        || x.filter.as_deref().is_some_and(reads)
}
