//! Normalize portable statement outputs; no membership cut is chosen here.
use super::{columns, facts};
use crate::Postgres;
use pbps_model::change::{ColumnProperty, ColumnTransition, ColumnValue};
use pbps_model::operation::{
    ColumnKey, Effects, ObjectKey, Operation, OwnerError, Owners, Recorded, Snapshot, symbolic::*,
};
use pbps_model::{Change, ValueSource};

/// The guarded column/address producer. Movement, restoration and canonical
/// scheduling integration are separate slices of DEC-2296.4.
#[allow(clippy::wildcard_enum_match_arm)]
pub fn of(
    pg: &Postgres,
    op: &Operation,
    owners: &dyn Owners,
    before: Snapshot<'_>,
    after: Snapshot<'_>,
    facts: &Facts,
) -> Result<Effects, OwnerError> {
    with_lineage(
        pg,
        op,
        owners,
        before,
        after,
        facts,
        &pbps_model::operation::Lineage::of(before, after),
    )
}

#[allow(clippy::too_many_arguments, clippy::wildcard_enum_match_arm)]
pub(crate) fn with_lineage(
    pg: &Postgres,
    op: &Operation,
    owners: &dyn Owners,
    before: Snapshot<'_>,
    after: Snapshot<'_>,
    facts: &Facts,
    lineage: &pbps_model::operation::Lineage,
) -> Result<Effects, OwnerError> {
    let mut fx = Effects::default();
    if facts.parents.is_empty() {
        return Ok(fx);
    }
    if let Some(clauses) = super::movement::of(pg, op, owners, before, after, facts, lineage)? {
        fx.symbolic = clauses;
        return Ok(fx);
    }
    let transitions = op.change().column_transitions();
    if facts.parents.is_empty() || transitions.is_empty() {
        return Ok(fx);
    }
    let (parent, column) = match op.key(owners)? {
        ObjectKey::Column(ColumnKey::Recorded(c)) | ObjectKey::Default(ColumnKey::Recorded(c)) => {
            let parent = [before, after]
                .into_iter()
                .find_map(|s| s.table_identity(&s.ids.columns.get(c.uid())?.table));
            let Some(parent) = parent else {
                fx.symbolic.push(columns::unsupported(
                    "the column's recorded owner is unavailable",
                    None,
                ));
                return Ok(fx);
            };
            (parent, Some(ColumnKey::Recorded(c)))
        }
        ObjectKey::Table(parent) if matches!(op.change(), Change::RenameTable { .. }) => {
            (parent, None)
        }
        _ => return Ok(fx),
    };
    let source = !facts.parents.contains(&parent)
        && matches!(op.change(), Change::AlterColumnDefault { .. })
        && lineage.families().any(|(p, f)| {
            facts.parents.contains(p)
                && f.standing(&parent, pbps_model::operation::End::Goal)
                    == pbps_model::operation::Standing::Member
        });
    if !facts.parents.contains(&parent) && !source {
        return Ok(fx);
    }
    if let Some(refusal) = columns::refusal(pg, op.change()) {
        fx.symbolic.push(refusal);
        return Ok(fx);
    }
    for transition in transitions {
        let property = match &transition.property {
            ColumnProperty::Column(a) => Property::Column(
                column
                    .clone()
                    .expect("a column transition has a column key"),
                a.clone(),
            ),
            ColumnProperty::Address => Property::Address,
        };
        let Some(input) = facts.base.cells.get(&(parent.clone(), property.clone())) else {
            fx.symbolic.push(columns::unsupported(
                format!("the input {property:?} version is unavailable"),
                None,
            ));
            continue;
        };
        // DROP's payload cannot say whether an owned sequence existed. Known
        // absence retires no sequence; an unread value must remain an obligation.
        if transition.property == ColumnProperty::Column(Attribute::Numbering)
            && matches!(op.change(), Change::DropColumn { .. })
        {
            match &input.value {
                Fact::Known(v) if v.datum == Datum::Absent => continue,
                Fact::Known(_) => {}
                Fact::Unread | Fact::Unsupported { .. } => {
                    fx.symbolic.push(Clause::Require(Fact::Unread));
                    continue;
                }
            }
        }
        let Some((mut scope, policy)) = columns::policy(pg, &transition) else {
            fx.symbolic.push(Clause::Require(Fact::Unread));
            continue;
        };
        if source {
            scope = Scope::ParentOnly;
        }
        let to = output(
            pg,
            op.change(),
            &transition,
            &parent,
            &property,
            before,
            after,
            facts,
        );
        fx.symbolic.push(Clause::Recurse(RecursiveWrite {
            key: Key {
                parent: parent.clone(),
                property,
            },
            input: input.version,
            from: input.value.clone(),
            to,
            scope,
            policy,
            lifetime: transition.lifetime,
        }));
    }
    Ok(fx)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn output(
    pg: &Postgres,
    change: &Change,
    transition: &ColumnTransition<'_>,
    parent: &Recorded,
    property: &Property,
    before: Snapshot<'_>,
    after: Snapshot<'_>,
    facts: &Facts,
) -> Fact<Value> {
    let columns = change.columns();
    let at = columns.first();
    match transition.value {
        ColumnValue::Absent => facts::absent(),
        ColumnValue::Address(name) => facts::definition(name.raw().to_string()),
        ColumnValue::Name(name) => facts::definition(name.raw()),
        ColumnValue::Collation(collation) => {
            facts::definition(serde_json::to_string(collation).expect("collation serializes"))
        }
        ColumnValue::Nullable(nullable) => facts::value(Datum::NotNull(!nullable), Ownership::Own),
        ColumnValue::Definition(column) => {
            let Change::AddColumn { table, name, .. } = change else {
                unreachable!()
            };
            let ColumnProperty::Column(attribute) = &transition.property else {
                unreachable!()
            };
            // A new expression has no stored binding evidence. Share the
            // endpoint reader's rule before treating its serialized form as known.
            if *attribute == Attribute::Type
                && let ValueSource::Generated(g) = &column.value_source
                && facts::may_bind_expression(&g.expression)
            {
                return Fact::Unread;
            }
            facts::column(pg, table, name, column)
                .remove(attribute)
                .unwrap_or(Fact::Unread)
        }
        ColumnValue::Type(ty) => {
            let source = change
                .parent_column_uid()
                .and_then(|uid| before.ids.columns.get(uid))
                .and_then(|at| before.schema.tables.get(&at.table)?.columns.get(&at.name));
            let mut value =
                source.map_or(Fact::Unread, |c| facts::type_value(pg, ty, &c.value_source));
            if let (Fact::Known(to), Some(input)) = (
                &mut value,
                facts.base.cells.get(&(parent.clone(), property.clone())),
            ) {
                match &input.value {
                    Fact::Known(from) => to.bindings = from.bindings.clone(),
                    Fact::Unread | Fact::Unsupported { .. } => return input.value.clone(),
                }
            }
            value
        }
        ColumnValue::Default(expression) => {
            let declared = change
                .parent_column_uid()
                .and_then(|uid| after.ids.columns.get(uid))
                .and_then(|at| after.schema.tables.get(&at.table)?.columns.get(&at.name));
            if expression.is_some() && declared.is_some_and(|c| matches!(&c.value_source, ValueSource::Default(e) if Some(e.as_str()) == expression)) {
                facts.goal.cells.get(&(parent.clone(), property.clone())).map(|c| c.value.clone()).unwrap_or(Fact::Unread)
            } else {
                at.map_or(Fact::Unread, |at| facts::default_value(pg, &expression.map_or(ValueSource::None, |e| ValueSource::Default(e.into())), &at.table, at.name.raw()))
            }
        }
        ColumnValue::Numbering(source) => {
            let Some(at) = at else {
                return Fact::Unread;
            };
            let ColumnProperty::Column(attribute) = &transition.property else {
                unreachable!()
            };
            let mut column = pbps_model::Column::new("integer".parse().unwrap());
            column.value_source = source.clone();
            facts::column(pg, &at.table, &at.name, &column)
                .remove(attribute)
                .unwrap_or(Fact::Unread)
        }
    }
}
