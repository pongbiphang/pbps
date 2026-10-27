//! Effects are exhaustive over Change, independent of spelling and emitted SQL.
use super::{
    Side,
    binding::{Entry, Target},
};
use pbps_model::{
    Change, ChangeSet, ColumnRef, DomainDelta, DomainHalf, PlannedChange, RiskClass, Uid,
    ValueSource,
};
use std::collections::BTreeSet;

#[derive(Clone, Copy)]
pub(super) enum Class {
    Breaking,
    Unproven,
}
impl Class {
    pub fn id(self) -> &'static str {
        match self {
            Self::Breaking => crate::rules::CONSUMER_BREAKING,
            Self::Unproven => crate::rules::CONSUMER_UNPROVEN,
        }
    }
}

fn supplied_new(at: &ColumnRef, cs: &ChangeSet, sides: &[Side<'_>]) -> bool {
    let Some(side) = sides.last() else {
        return false;
    };
    let Some((source, column)) = side.column(at) else {
        return false;
    };
    let Some(uid) = side.ids.column_uid(&source) else {
        return false;
    };
    !sides
        .first()
        .is_some_and(|s| s.ids.columns.contains_key(uid))
        && cs
            .changes
            .iter()
            .any(|p| matches!(&p.change, Change::AddColumn { uid: added, .. } if added == uid))
        && !matches!(column.value_source, ValueSource::None)
}

// A derived domain carries the base domain's restrictions, unless its own
// attributes (or the column's) already supply the stronger/local definition.
fn domain_column(
    mut column: pbps_model::Column,
    name: &pbps_model::TypeRef,
    schema: &pbps_model::Schema,
) -> Option<pbps_model::Column> {
    let mut ty = column.ty.clone();
    for _ in 0..=schema.types.len() {
        let reference = ty.type_ref()?;
        if reference == name {
            return Some(column);
        }
        let pbps_model::ManagedType::Domain {
            base,
            not_null,
            default,
            collation,
            ..
        } = schema.types.get(reference)?
        else {
            return None;
        };
        column.nullable &= !not_null;
        if matches!(column.value_source, ValueSource::None) {
            column.value_source = default
                .clone()
                .map_or(ValueSource::None, ValueSource::Default);
        }
        column.collation = column.collation.or_else(|| {
            collation
                .as_ref()
                .map(|c| pbps_model::Collation::new(c.raw().to_string()))
        });
        ty = base.clone();
    }
    None
}

fn uses_type(entry: &Entry, name: &pbps_model::TypeRef, sides: &[Side<'_>]) -> bool {
    if entry.table().is_none() {
        return false;
    }
    sides.iter().any(|side| {
        side.schema.tables.keys().any(|owner| {
            side.ids.table_uid(owner) == entry.table()
                && side.columns(owner).any(|(at, _, spec)| {
                    domain_column(spec, name, side.schema).is_some()
                        && (matches!(entry.target, Target::Table(_))
                            || entry.named_column(&at.name, sides)
                            || side
                                .ids
                                .column_uid(&at)
                                .is_some_and(|uid| entry.column(uid)))
                })
        })
    })
}

pub(super) fn classify(
    p: &PlannedChange,
    entry: &Entry,
    table: Option<&Uid>,
    written: Option<&BTreeSet<Uid>>,
    cs: &ChangeSet,
    sides: &[Side<'_>],
    unchanged: &dyn Fn(&Change, Option<&Uid>) -> bool,
) -> Option<Class> {
    use Class::{Breaking as B, Unproven as U};
    let hit = table.is_some() && table == entry.table();
    let write_hit = table.is_some_and(|uid| written.is_some_and(|set| set.contains(uid)));
    let column = |uid: &Uid| {
        (hit && entry.column(uid))
            || sides.iter().any(|side| {
                let Some(c) = side.ids.columns.get(uid) else {
                    return false;
                };
                if side.ids.table_uid(&c.table) == entry.table() {
                    return entry.column(uid);
                }
                entry
                    .table()
                    .and_then(|t| side.ids.tables.get(t))
                    .and_then(|t| side.column(&t.column(c.name.clone())))
                    .is_some_and(|(source, _)| {
                        side.ids.column_uid(&source) == Some(uid)
                            && entry.named_column(&c.name, sides)
                    })
            })
    };
    let supplied = |uid: &Uid| {
        sides
            .last()
            .and_then(|s| s.ids.columns.get(uid))
            .is_some_and(|c| supplied_new(c, cs, sides))
    };
    let named = |name: &pbps_model::ColumnName| hit && entry.named_column(name, sides);
    let nullability = |column_hit: bool, nullable: bool| {
        if entry.writes {
            write_hit && !nullable
        } else {
            column_hit && nullable
        }
    };
    let retype = |uid: &Uid, nullable: bool, collation: bool| {
        (((column(uid) && (p.risks.contains(&RiskClass::Narrowing) || collation)) || nullable)
            && !supplied(uid))
        .then_some(U)
    };
    let write = |yes| (entry.writes && yes).then_some(U);
    match &p.change {
        Change::RebuildPartitionedTable(_) => hit.then_some(U),
        Change::BuildPartitionedIndex(index) => {
            let unique = match &index.definition {
                pbps_model::PartitionIndexDefinition::Index(i) => i.unique,
                pbps_model::PartitionIndexDefinition::PrimaryKey(_)
                | pbps_model::PartitionIndexDefinition::Unique(_) => true,
            };
            write(write_hit && unique && !unchanged(&p.change, table))
        }
        Change::RenameEnumValue { name, .. } | Change::DropType { name, .. } => {
            uses_type(entry, name, sides).then_some(U)
        }
        Change::RenameType { from, to, .. } => {
            (uses_type(entry, from, sides) || uses_type(entry, to, sides)).then_some(U)
        }
        Change::AlterDomain {
            name,
            from,
            to,
            half,
            ..
        } => {
            let delta = DomainDelta::of(from, to)?;
            sides.iter().find_map(|side| {
                side.schema.tables.keys().find_map(|owner| {
                    let table = side.ids.table_uid(owner);
                    side.columns(owner).find_map(|(at, source, spec)| {
                        let spec = domain_column(spec, name, side.schema)?;
                        let uid = side.ids.column_uid(&source)?;
                        let (
                            pbps_model::ManagedType::Domain { collation: was, .. },
                            pbps_model::ManagedType::Domain { collation: now, .. },
                        ) = (from.as_ref(), to.as_ref())
                        else {
                            return None;
                        };
                        if *half == DomainHalf::Tighten
                            && let Some(class) =
                                retype(uid, false, spec.collation.is_none() && was != now)
                        {
                            return Some(class);
                        }
                        let mut effects = Vec::new();
                        if *half == DomainHalf::Tighten {
                            for (name, text) in &delta.add_checks {
                                effects.push(Change::AddCheck {
                                    table: owner.clone(),
                                    name: (*name).clone(),
                                    constraint: pbps_model::CheckConstraint {
                                        expression: (*text).into(),
                                    },
                                });
                            }
                        }
                        if spec.nullable
                            && match half {
                                DomainHalf::Loosen => delta.drop_not_null,
                                DomainHalf::Tighten => delta.set_not_null,
                            }
                        {
                            effects.push(Change::AlterColumnNullability {
                                uid: uid.clone(),
                                column: at.clone(),
                                ty: spec.ty.clone(),
                                to_nullable: *half == DomainHalf::Loosen,
                                collation: spec.collation.clone(),
                                through_external_partitions: false,
                            });
                        }
                        if *half == DomainHalf::Loosen
                            && delta.default.is_some()
                            && matches!(spec.value_source, ValueSource::None)
                        {
                            let pbps_model::ManagedType::Domain { default, .. } = from.as_ref()
                            else {
                                return None;
                            };
                            effects.push(Change::AlterColumnDefault {
                                uid: uid.clone(),
                                column: at,
                                from: default.clone(),
                                to: delta.default.flatten().map(str::to_owned),
                            });
                        }
                        // Reuse the ordinary paths, but ask the differ about
                        // the original domain, never a same-named table part.
                        // The outer finding likewise keeps the domain name.
                        effects.into_iter().find_map(|effect| {
                            classify(
                                &PlannedChange::new(effect),
                                entry,
                                table,
                                written,
                                cs,
                                sides,
                                &|_, table| unchanged(&p.change, table),
                            )
                        })
                    })
                })
            })
        }
        Change::EnumRebuild {
            uid, name, step, ..
        } => match step {
            pbps_model::EnumRebuildStep::Retype {
                column_uid, values, ..
            } => match values.as_ref() {
                pbps_model::ValueChange::Relabel { removed } => {
                    (column(column_uid) && !removed.is_empty()).then_some(U)
                }
                pbps_model::ValueChange::Cast { .. } => retype(column_uid, false, false),
            },
            pbps_model::EnumRebuildStep::Create { definition, .. } => {
                let before = sides.first()?;
                let from = before.schema.types.get(before.ids.types.get(uid)?)?;
                [DomainHalf::Loosen, DomainHalf::Tighten]
                    .into_iter()
                    .find_map(|half| {
                        classify(
                            &PlannedChange::new(Change::AlterDomain {
                                uid: uid.clone(),
                                name: name.clone(),
                                from: Box::new(from.clone()),
                                to: definition.clone(),
                                half,
                            }),
                            entry,
                            table,
                            written,
                            cs,
                            sides,
                            unchanged,
                        )
                    })
            }
            pbps_model::EnumRebuildStep::Swap { .. } => None,
        },
        Change::CreateTable { table: shape, .. } => {
            write(shape.partition_of.as_ref().is_some_and(|of| {
                sides
                    .iter()
                    .any(|s| s.ids.table_uid(&of.parent) == entry.table())
            }))
        }
        Change::DropTable { .. } | Change::RenameTable { .. } => hit.then_some(B),
        Change::DetachPartition { parent, .. } | Change::AttachPartition { parent, .. } => write(
            hit || sides
                .iter()
                .any(|s| s.ids.table_uid(parent) == entry.table()),
        ),
        Change::DropColumn { uid, .. } | Change::RenameColumn { uid, .. } => {
            column(uid).then_some(B)
        }
        Change::AddColumn {
            uid, column: spec, ..
        } => write(write_hit && !spec.nullable && !supplied(uid)),
        Change::AlterColumnType {
            uid,
            from_nullable,
            to_nullable,
            from_collation,
            to_collation,
            ..
        } => retype(
            uid,
            from_nullable != to_nullable && nullability(column(uid), *to_nullable),
            from_collation != to_collation,
        ),
        Change::AlterColumnNullability {
            uid, to_nullable, ..
        } => (nullability(column(uid), *to_nullable) && !supplied(uid)).then_some(U),
        Change::AlterColumnDefault { uid, .. } => {
            write(column(uid) && !supplied(uid) && !unchanged(&p.change, table))
        }
        Change::AlterColumnExpression { uid, .. } => {
            (column(uid) || (entry.writes && hit)).then_some(U)
        }
        Change::AlterColumnNumbering { uid, .. } => write(
            column(uid)
                && (p.risks.contains(&RiskClass::Numbering)
                    || p.risks.contains(&RiskClass::Rename)),
        ),
        Change::DropComputedColumn {
            table: owner, name, ..
        } => {
            if !named(name) || entry.writes {
                return None;
            }
            if unchanged(&p.change, table) {
                None
            } else {
                let restored = sides.last().and_then(|s| s.table(table))
                    .is_some_and(|t| t.computed.contains_key(name))
                    || cs.changes.iter().any(|p| matches!(&p.change, Change::AddColumn { table: t, name: n, .. } if t == owner && n == name));
                Some(if restored { U } else { B })
            }
        }
        Change::SetPartitionDefault {
            table: owner,
            column,
            ..
        } => write(
            hit && !supplied_new(&owner.column(column.clone()), cs, sides)
                && !unchanged(&p.change, table),
        ),
        Change::SetPartitionNotNull {
            table,
            column,
            not_null,
            ..
        } => (nullability(named(column), !not_null)
            && !supplied_new(&table.column(column.clone()), cs, sides))
        .then_some(U),
        // Restoration equivalence belongs to the differ, including generated
        // names, inferred attributes, rename remapping and parameter-only edits.
        Change::SetPrimaryKey { to, .. } => {
            write(write_hit && to.is_some() && !unchanged(&p.change, table))
        }
        Change::AddUnique { .. } | Change::AddCheck { .. } => {
            write(write_hit && !unchanged(&p.change, table))
        }
        Change::AddForeignKey { constraint, .. } => write(
            !unchanged(&p.change, table)
                && (write_hit
                    || sides.iter().any(|s| {
                        s.ids
                            .table_uid(&constraint.references_table)
                            .is_some_and(|uid| written.is_some_and(|set| set.contains(uid)))
                    })),
        ),
        Change::AddIndex { index, .. } => {
            write(write_hit && index.unique && !unchanged(&p.change, table))
        }
        Change::CreateModule { id, .. } | Change::AlterModule { id, .. } => (entry.target
            == Target::Module(id.clone())
            && !unchanged(&p.change, table)
            && sides
                .first()
                .is_some_and(|s| s.schema.modules.contains_key(id)))
        .then_some(U),
        Change::DropModule { id, .. } => {
            if entry.target != Target::Module(id.clone()) || unchanged(&p.change, table) {
                None
            } else {
                Some(
                    if sides
                        .last()
                        .is_some_and(|s| s.schema.modules.contains_key(id))
                    {
                        U
                    } else {
                        B
                    },
                )
            }
        }
        Change::CopyTableRows(_)
        | Change::AnalyzeTableTree(_)
        | Change::AddEnumValue { .. }
        | Change::CreateType { .. }
        | Change::AddComputedColumn { .. }
        | Change::SetColumnDeprecated { .. }
        | Change::SetReplicaIdentity { .. }
        | Change::SetStorageParameters { .. }
        | Change::SetTablePersistence { .. }
        | Change::SetIndexStorageParameters { .. }
        | Change::CreatePolicy { .. }
        | Change::DropPolicy { .. }
        | Change::SetRowSecurity { .. }
        | Change::DropUnique { .. }
        | Change::DropForeignKey { .. }
        | Change::DropCheck { .. }
        | Change::DropIndex { .. }
        | Change::InsertRow { .. }
        | Change::UpdateRow { .. }
        | Change::DeleteRow { .. }
        | Change::SetDataMode { .. }
        | Change::SetEnsureKeys { .. }
        | Change::CreateRole { .. }
        | Change::DropRole { .. }
        | Change::RenameRole { .. }
        | Change::Grant { .. }
        | Change::Revoke { .. }
        | Change::PublicExecution { .. } => None,
    }
}
