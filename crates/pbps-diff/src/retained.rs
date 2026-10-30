//! Object-addressed engine answers enter comparison before plan construction.
use crate::Side;
use pbps_model::{Change, ExpressionTarget, Hints, RenameAnchor, RetainedExpression, Schema};
use std::{borrow::Cow, collections::BTreeSet};

pub fn candidates(base: Side<'_>, desired: Side<'_>, hints: &Hints) -> Vec<RetainedExpression> {
    if base.schema.modules != desired.schema.modules {
        return Vec::new();
    }
    let (steps, errors) = crate::enum_labels::changes(base, desired, hints);
    if !errors.is_empty() {
        return Vec::new();
    }
    let anchors: Vec<_> = steps
        .iter()
        .filter_map(|c| {
            if let Change::RenameEnumValue { uid, ordinal, .. } = c {
                Some(RenameAnchor {
                    uid: uid.clone(),
                    ordinal: *ordinal,
                })
            } else {
                None
            }
        })
        .collect();
    if anchors.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (uid, table) in &desired.ids.tables {
        if base.ids.tables.get(uid) != Some(table) {
            continue;
        }
        let (Some(old), Some(new)) = (
            base.schema.tables.get(table),
            desired.schema.tables.get(table),
        ) else {
            continue;
        };
        let types = crate::identity::type_refs::TypeResolver::new(base.ids, desired.ids);
        if old.partition_by.is_some()
            || old.partition_of.is_some()
            || new.partition_by.is_some()
            || new.partition_of.is_some()
            || old.computed != new.computed
            || base
                .ids
                .columns
                .iter()
                .filter(|(_, c)| &c.table == table)
                .ne(desired
                    .ids
                    .columns
                    .iter()
                    .filter(|(_, c)| &c.table == table))
            || old.columns.len() != new.columns.len()
            || old.columns.iter().any(|(name, x)| {
                new.columns.get(name).is_none_or(|y| {
                    x.ty.is_array() != y.ty.is_array()
                        || x.collation != y.collation
                        || matches!(x.value_source, pbps_model::ValueSource::Generated(_))
                        || matches!(y.value_source, pbps_model::ValueSource::Generated(_))
                        || !(x.ty == y.ty
                            || x.ty
                                .type_ref()
                                .zip(y.ty.type_ref())
                                .is_some_and(|(x, y)| types.same_declared(x, y)))
                })
            })
        {
            continue;
        }
        for (name, after) in &new.checks {
            if let Some(before) = old.checks.get(name)
                && before != after
            {
                out.push(RetainedExpression {
                    target: ExpressionTarget::Check {
                        uid: uid.clone(),
                        table: table.clone(),
                        name: name.clone(),
                    },
                    before: before.expression.clone(),
                    after: after.expression.clone(),
                    anchors: anchors.clone(),
                });
            }
        }
    }
    out
}

pub(crate) fn comparison<'a>(
    base: Side<'a>,
    desired: Side<'_>,
    hints: &Hints,
    steps: &mut [Change],
) -> Cow<'a, Schema> {
    let mut schema = Cow::Borrowed(base.schema);
    if hints.retained_expressions.is_empty() {
        return schema;
    }
    let allowed = candidates(
        base,
        desired,
        &Hints {
            retained_expressions: Vec::new(),
            ..hints.clone()
        },
    );
    let mut seen = BTreeSet::new();
    for answer in &hints.retained_expressions {
        if !allowed.contains(answer) || !seen.insert(&answer.target) {
            continue;
        }
        let Some(anchor) = answer.anchors.last() else {
            continue;
        };
        let Some(Change::RenameEnumValue { retained, .. }) = steps.iter_mut().find(|c|
            matches!(c, Change::RenameEnumValue { uid, ordinal, .. } if uid == &anchor.uid && ordinal == &anchor.ordinal))
        else { continue; };
        retained.push(answer.clone());
        let ExpressionTarget::Check { table, name, .. } = &answer.target;
        schema
            .to_mut()
            .tables
            .get_mut(table)
            .unwrap()
            .checks
            .get_mut(name)
            .unwrap()
            .expression = answer.after.clone();
    }
    schema
}
