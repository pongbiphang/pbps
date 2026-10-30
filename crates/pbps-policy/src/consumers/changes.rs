//! Effects are exhaustive over Change, independent of spelling and emitted SQL.
use super::{
    Side,
    binding::{Entry, Target},
    effective::{ColumnPair, Interfaces},
};
use pbps_model::{
    Change, ColumnName, DomainDelta, DomainHalf, ManagedType, PlannedChange, RiskClass, TypeRef,
    Uid,
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

pub(super) fn classify(
    p: &PlannedChange,
    entry: &Entry,
    table: Option<&Uid>,
    written: Option<&BTreeSet<Uid>>,
    interfaces: &Interfaces,
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
    let pair_hit = |pair: ColumnPair<'_>| {
        Some(&pair.key.0) == entry.table()
            && (entry.column(&pair.key.1)
                // Names bridge inherited targets, never a different ordinary UID.
                || (matches!(&entry.target, Target::NamedColumn { .. })
                    && pair
                        .addresses()
                        .any(|at| entry.named_column(&at.name, sides))))
    };
    let written_pair = |pair: ColumnPair<'_>| written.is_some_and(|set| set.contains(&pair.key.0));
    let supplied = |uid: &Uid| {
        interfaces
            .columns()
            .any(|pair| &pair.key.1 == uid && pair_hit(pair) && pair.supplied())
    };
    let named = |name: &ColumnName| hit && entry.named_column(name, sides);
    let nullability = |pair: ColumnPair<'_>| {
        (if entry.writes {
            written_pair(pair)
        } else {
            pair_hit(pair)
        }) && pair.nullability(entry.writes)
            && !pair.supplied()
    };
    let default = |pair: ColumnPair<'_>, affected| {
        entry.writes
            && affected
            && pair.default_changed()
            && !pair.supplied()
            && !unchanged(&p.change, Some(&pair.key.0))
    };
    let collation =
        |pair: ColumnPair<'_>| pair_hit(pair) && pair.collation_changed() && !pair.supplied();
    let retype = |uid: &Uid, nullable: bool, collation: bool| {
        (((column(uid) && (p.risks.contains(&RiskClass::Narrowing) || collation)) || nullable)
            && !supplied(uid))
        .then_some(U)
    };
    let write = |yes| (entry.writes && yes).then_some(U);
    let uses_type = |ty: &TypeRef| {
        interfaces
            .columns()
            .any(|pair| pair_hit(pair) && pair.uses(ty))
    };
    let domain =
        |name: &TypeRef, from: &ManagedType, to: &ManagedType, half: Option<DomainHalf>| {
            let delta = DomainDelta::of(from, to)?;
            interfaces
                .columns()
                .any(|pair| {
                    pair.uses(name)
                        && ((half.is_none_or(|h| {
                            h == if entry.writes {
                                DomainHalf::Tighten
                            } else {
                                DomainHalf::Loosen
                            }
                        }) && nullability(pair))
                            || (half != Some(DomainHalf::Tighten) && default(pair, pair_hit(pair)))
                            || (half != Some(DomainHalf::Loosen)
                                && (collation(pair)
                                    || (entry.writes
                                        && written_pair(pair)
                                        && pair.adds_checks(name, &delta)
                                        && !unchanged(&p.change, Some(&pair.key.0))))))
                })
                .then_some(U)
        };
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
            uses_type(name).then_some(U)
        }
        Change::RenameType { from, to, .. } => (uses_type(from) || uses_type(to)).then_some(U),
        Change::AlterDomain {
            name,
            from,
            to,
            half,
            ..
        } => domain(name, from, to, Some(*half)),
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
                domain(name, from, definition, None)
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
        Change::AddColumn { uid, .. } => write(interfaces.columns().any(|pair| {
            &pair.key.1 == uid
                && written_pair(pair)
                && pair.after.is_some_and(|c| !c.column.nullable)
                && !pair.supplied()
        })),
        Change::AlterColumnType { uid, .. } => retype(
            uid,
            interfaces
                .columns()
                .filter(|pair| &pair.key.1 == uid)
                .any(nullability),
            interfaces
                .columns()
                .filter(|pair| &pair.key.1 == uid)
                .any(collation),
        ),
        Change::AlterColumnNullability { uid, .. } => interfaces
            .columns()
            .filter(|pair| &pair.key.1 == uid)
            .any(nullability)
            .then_some(U),
        Change::AlterColumnDefault { uid, .. } => interfaces
            .columns()
            .filter(|pair| &pair.key.1 == uid)
            .any(|pair| default(pair, pair_hit(pair)))
            .then_some(U),
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
                let restored = sides
                    .last()
                    .and_then(|s| s.table(table))
                    .is_some_and(|t| t.computed.contains_key(name))
                    || interfaces.adds_column(&owner.column(name.clone()));
                Some(if restored { U } else { B })
            }
        }
        Change::SetPartitionDefault { column, .. } => interfaces
            .columns()
            .filter(|pair| {
                table == Some(&pair.key.0) && pair.addresses().any(|at| &at.name == column)
            })
            .any(|pair| default(pair, hit))
            .then_some(U),
        Change::SetPartitionNotNull { column, .. } => interfaces
            .columns()
            .filter(|pair| {
                table == Some(&pair.key.0) && pair.addresses().any(|at| &at.name == column)
            })
            .any(nullability)
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
