//! Membership and restoration keys come only from the endpoint lineage.
use super::{columns, facts};
use crate::Postgres;
use pbps_model::Change;
use pbps_model::operation::{
    ColumnKey, End, Family, Lineage, ObjectKey, Operation, OwnerError, Owners, Recorded, Role,
    Snapshot, Standing, symbolic::*,
};

fn require(reason: impl Into<String>) -> Vec<Clause> {
    vec![columns::unsupported(reason, None)]
}

// The goal describes intent, not what a restoration statement writes. Only
// normalized definition/binding agreement can lend its ownership (DEC-2296.7).
fn checked_goal(
    output: Fact<Value>,
    goal: Option<&Cell>,
    ownership: Option<Ownership>,
) -> Fact<Value> {
    let Some(goal) = goal else {
        return Fact::Unread;
    };
    match (&output, &goal.value) {
        (_, Fact::Unread | Fact::Unsupported { .. }) => goal.value.clone(),
        (Fact::Known(actual), Fact::Known(expected)) if actual.definition_eq(expected) => {
            let mut value = expected.clone();
            if let Some(ownership) = ownership {
                value.ownership = ownership;
            }
            Fact::Known(value)
        }
        _ => output,
    }
}

struct Context<'a> {
    pg: &'a Postgres,
    before: Snapshot<'a>,
    after: Snapshot<'a>,
    facts: &'a Facts,
    lineage: &'a Lineage,
}
impl Context<'_> {
    fn family(&self, child: &Recorded, end: End) -> Option<(&Recorded, &Family)> {
        self.lineage.families().find(|(p, f)| {
            self.facts.parents.contains(p) && f.standing(child, end) == Standing::Member
        })
    }

    fn movement(
        &self,
        op: &Operation,
        child: &Recorded,
        parent: &Recorded,
        family: &Family,
    ) -> Vec<Clause> {
        let kind = if matches!(op.change(), Change::AttachPartition { .. }) {
            MoveKind::Attach
        } else if matches!(op.change(), Change::DetachPartition { .. }) {
            MoveKind::Detach
        } else {
            MoveKind::Drop
        };
        for side in [self.before, self.after] {
            for uid in [parent, child] {
                if let Some(name) = side.ids.tables.get(uid.uid())
                    && let Some(table) = side.schema.tables.get(name)
                    && table.partition_of.is_none()
                    && table
                        .columns
                        .keys()
                        .any(|c| side.column_identity(&name.column(c)).is_none())
                {
                    return require(
                        "a membership endpoint has an ordinary column without a recorded UID",
                    );
                }
            }
        }
        let mut transfers = Vec::new();
        for (uid, key) in family.columns() {
            let Some(from) = family
                .physical(uid, child, End::Base)
                .or_else(|| family.slot(uid, child, End::Base))
            else {
                return require("the move's baseline physical property is unavailable");
            };
            let to = if kind == MoveKind::Attach {
                family.slot(uid, child, End::Goal)
            } else {
                family
                    .physical(uid, child, End::Goal)
                    .or_else(|| family.slot(uid, child, End::Goal))
            };
            let to = to.map_or_else(|| from.key.clone(), |p| p.key);
            for attribute in facts::ATTRIBUTES {
                let property = Property::Column(key.clone(), attribute.clone());
                let from = Property::Column(from.key.clone(), attribute.clone());
                let to = Property::Column(to.clone(), attribute.clone());
                let goal = self
                    .facts
                    .goal
                    .cells
                    .get(&(child.clone(), to.clone()))
                    .map(|c| c.value.clone())
                    .unwrap_or_else(|| {
                        if kind == MoveKind::Drop {
                            facts::absent()
                        } else {
                            Fact::Unread
                        }
                    });
                let (check, policy) = if kind == MoveKind::Attach {
                    let parents = [&self.facts.base, &self.facts.goal]
                        .into_iter()
                        .filter_map(|s| s.cells.get(&(parent.clone(), property.clone())))
                        .filter_map(|c| c.value.known().ok().cloned())
                        .collect::<Vec<_>>();
                    let candidates = self
                        .facts
                        .base
                        .cells
                        .get(&(child.clone(), from.clone()))
                        .and_then(|c| c.value.known().ok())
                        .cloned()
                        .into_iter()
                        .collect::<Vec<_>>();
                    match super::attach_policy(
                        &attribute,
                        self.pg.server_version_num,
                        &parents,
                        &candidates,
                    ) {
                        Ok(p) => p,
                        Err(why) => return require(why),
                    }
                } else {
                    (ShapeCheck::None, TransferPolicy::Preserve)
                };
                transfers.push(Transfer {
                    parent: property,
                    from,
                    to,
                    check,
                    policy,
                    goal,
                });
            }
        }
        vec![Clause::Move(Move {
            parent: parent.clone(),
            child: child.clone(),
            kind,
            consumes: (kind != MoveKind::Attach).then_some(Version::Base),
            boundary: Boundary::All(transfers),
        })]
    }

    fn restoration(
        &self,
        op: &Operation,
        child: &Recorded,
        key: &ColumnKey,
        attribute: Attribute,
    ) -> Vec<Clause> {
        let Some((parent, family)) = self
            .family(child, End::Goal)
            .or_else(|| self.family(child, End::Base))
        else {
            return vec![];
        };
        let Some(uid) = [End::Goal, End::Base]
            .into_iter()
            .find_map(|e| family.parent_column(key, child, e))
        else {
            return require("the restoration's parent column UID is unavailable");
        };
        let Some((_, parent_key)) = family.columns().find(|(u, _)| *u == uid) else {
            unreachable!()
        };
        let parent_property = Property::Column(parent_key, attribute.clone());
        if matches!(key, ColumnKey::Recorded(_)) {
            let transition = op.change().column_transitions().remove(0);
            let property = Property::Column(key.clone(), attribute);
            let value = super::operations::output(
                self.pg,
                op.change(),
                &transition,
                child,
                &property,
                self.before,
                self.after,
                self.facts,
            );
            return vec![Clause::Restore(Restoration {
                key: Key {
                    parent: parent.clone(),
                    property: parent_property,
                },
                child: child.clone(),
                property,
                value,
                member_of: None,
            })];
        }
        let Some(slot) = family.slot(uid, child, End::Goal) else {
            return require("the restoration's goal column is unavailable");
        };
        let goal_property = Property::Column(slot.key, attribute.clone());
        let target = self
            .facts
            .base
            .members
            .get(child)
            .and_then(|m| m.aliases.get(&parent_property))
            .cloned()
            .unwrap_or_else(|| goal_property.clone());
        let own = self
            .after
            .ids
            .tables
            .get(child.uid())
            .and_then(|n| self.after.schema.tables.get(n))
            .and_then(|t| t.partition_of.as_ref())
            .and_then(|of| of.columns.get(&slot.name));
        let value = match (op.change(), &attribute) {
            (Change::SetPartitionNotNull { not_null, .. }, Attribute::NotNull) => {
                if *not_null != own.is_some_and(|o| o.not_null) {
                    return require(
                        "the NOT NULL restoration payload disagrees with its declared own goal",
                    );
                }
                let nullable = self
                    .after
                    .ids
                    .tables
                    .get(parent.uid())
                    .and_then(|n| self.after.schema.tables.get(n)?.columns.get(&slot.name))
                    .is_some_and(|c| c.nullable);
                if !not_null && !nullable {
                    return require(
                        "a local NOT NULL cannot be dropped while its declared parent requires NOT NULL",
                    );
                }
                let Some(goal) = self.facts.goal.cells.get(&(child.clone(), goal_property)) else {
                    return require("the NOT NULL restoration's goal is unavailable");
                };
                checked_goal(
                    facts::value(Datum::NotNull(*not_null), Ownership::Own),
                    Some(goal),
                    (!not_null).then_some(Ownership::Inherited),
                )
            }
            (Change::SetPartitionDefault { to, fallback, .. }, Attribute::Default) => {
                let Some(name) = self
                    .after
                    .ids
                    .tables
                    .get(child.uid())
                    .or_else(|| self.before.ids.tables.get(child.uid()))
                else {
                    return require("the restoration's recorded table is unavailable");
                };
                // emit::SetPartitionDefault writes to, then fallback, or drops
                // the default. A teardown has the same payload precedence.
                let output = facts::bound_default(
                    self.pg,
                    to.as_deref().or(fallback.as_deref()),
                    name,
                    slot.name.raw(),
                    self.facts
                        .goal
                        .cells
                        .get(&(child.clone(), goal_property.clone())),
                );
                if op.role() == Role::Teardown {
                    output
                } else {
                    checked_goal(
                        output,
                        self.facts.goal.cells.get(&(child.clone(), goal_property)),
                        Some(
                            if to.is_none() && own.and_then(|o| o.default.as_ref()).is_none() {
                                Ownership::Inherited
                            } else {
                                Ownership::Own
                            },
                        ),
                    )
                }
            }
            _ => unreachable!(),
        };
        vec![Clause::Restore(Restoration {
            key: Key {
                parent: parent.clone(),
                property: parent_property,
            },
            child: child.clone(),
            property: target,
            value,
            member_of: Some(parent.clone()),
        })]
    }
}

#[allow(clippy::too_many_arguments, clippy::wildcard_enum_match_arm)]
pub(super) fn of(
    pg: &Postgres,
    op: &Operation,
    owners: &dyn Owners,
    before: Snapshot<'_>,
    after: Snapshot<'_>,
    facts: &Facts,
    lineage: &Lineage,
) -> Result<Option<Vec<Clause>>, OwnerError> {
    let context = Context {
        pg,
        before,
        after,
        facts,
        lineage,
    };
    let key = op.key(owners)?;
    let clauses = match (&key, op.change()) {
        (
            ObjectKey::Table(child),
            Change::AttachPartition { .. }
            | Change::DetachPartition { .. }
            | Change::DropTable {
                detach_from: Some(_),
                ..
            },
        ) => {
            let Some((parent, family)) = context.family(
                child,
                if matches!(op.change(), Change::AttachPartition { .. }) {
                    End::Goal
                } else {
                    End::Base
                },
            ) else {
                return Ok(None);
            };
            context.movement(op, child, parent, family)
        }
        (
            ObjectKey::Column(key @ ColumnKey::Partition { table: child, .. }),
            Change::SetPartitionNotNull { .. },
        ) => context.restoration(op, child, key, Attribute::NotNull),
        (
            ObjectKey::Default(key @ ColumnKey::Partition { table: child, .. }),
            Change::SetPartitionDefault { .. },
        ) => context.restoration(op, child, key, Attribute::Default),
        (
            ObjectKey::Default(key @ ColumnKey::Recorded(_)),
            Change::AlterColumnDefault { uid, .. },
        ) => {
            let child = [after, before].into_iter().find_map(|s| {
                s.ids
                    .columns
                    .get(uid)
                    .and_then(|at| s.table_identity(&at.table))
            });
            let Some(child) = child.filter(|c| {
                !facts.parents.contains(c)
                    && context
                        .family(c, End::Goal)
                        .or_else(|| context.family(c, End::Base))
                        .is_some()
            }) else {
                return Ok(None);
            };
            if context.family(&child, End::Goal).is_some() {
                return Ok(None); // The ordinary writer loop binds a standalone source.
            }
            context.restoration(op, &child, key, Attribute::Default)
        }
        _ => return Ok(None),
    };
    Ok(Some(clauses))
}
