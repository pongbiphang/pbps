//! Effects are exhaustive over Change, independent of spelling and emitted SQL.
use super::{
    Side,
    binding::{Entry, Target},
};
use pbps_model::{Change, ChangeSet, PlannedChange, RiskClass, Uid, ValueSource};
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

fn supplied_new(uid: &Uid, cs: &ChangeSet, sides: &[Side<'_>]) -> bool {
    !sides
        .first()
        .is_some_and(|side| side.ids.columns.contains_key(uid))
        && cs
            .changes
            .iter()
            .any(|p| matches!(&p.change, Change::AddColumn { uid: added, .. } if added == uid))
        && sides.last().is_some_and(|side| {
            side.ids
                .columns
                .get(uid)
                .and_then(|c| side.schema.tables.get(&c.table)?.columns.get(&c.name))
                .is_some_and(|c| !matches!(c.value_source, ValueSource::None))
        })
}

fn uses_type(entry: &Entry, name: &pbps_model::TypeRef, sides: &[Side<'_>]) -> bool {
    if entry.table().is_none() {
        return false;
    }
    sides.iter().any(|side| {
        side.schema.tables.iter().any(|(t, table)| {
            side.ids.table_uid(t) == entry.table()
                && table
                    .columns
                    .iter()
                    .chain(
                        table
                            .partition_of
                            .as_ref()
                            .and_then(|of| side.schema.tables.get(&of.parent))
                            .into_iter()
                            .flat_map(|parent| parent.columns.iter()),
                    )
                    .any(|(c, spec)| {
                        spec.ty.type_ref() == Some(name)
                            && (matches!(entry.target, Target::Table(_))
                                || entry.named_column(c)
                                || side
                                    .ids
                                    .column_uid(&t.column(c.clone()))
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
                // Inherited columns have their parent's column identity and their
                // leaf's table identity; parent alterations reach them too.
                entry
                    .table()
                    .and_then(|t| side.ids.tables.get(t))
                    .and_then(|t| side.schema.tables.get(t))
                    .and_then(|t| t.partition_of.as_ref())
                    .is_some_and(|of| of.parent == c.table && entry.named_column(&c.name))
            })
    };
    let named = |name: &pbps_model::ColumnName| hit && entry.named_column(name);
    let nullability = |column_hit: bool, nullable: bool| {
        if entry.writes {
            write_hit && !nullable
        } else {
            column_hit && nullable
        }
    };
    let write = |yes| (entry.writes && yes).then_some(U);
    let unchanged_module = |id| {
        // Descriptions are declaration metadata, absent from introspection.
        sides
            .first()
            .and_then(|s| s.schema.modules.get(id))
            .map(|m| (m.kind, &m.options, &m.definition))
            == sides
                .last()
                .and_then(|s| s.schema.modules.get(id))
                .map(|m| (m.kind, &m.options, &m.definition))
    };
    let before = sides.first().and_then(|s| s.table(table));
    let after = sides.last().and_then(|s| s.table(table));
    match &p.change {
        Change::RebuildPartitionedTable(_) => hit.then_some(U),
        Change::BuildPartitionedIndex(index) => {
            let unique = match &index.definition {
                pbps_model::PartitionIndexDefinition::Index(i) => i.unique,
                pbps_model::PartitionIndexDefinition::PrimaryKey(_)
                | pbps_model::PartitionIndexDefinition::Unique(_) => true,
            };
            write(write_hit && unique)
        }
        Change::RenameEnumValue { name, .. } | Change::DropType { name, .. } => {
            uses_type(entry, name, sides).then_some(U)
        }
        Change::RenameType { from, to, .. } => {
            (uses_type(entry, from, sides) || uses_type(entry, to, sides)).then_some(U)
        }
        Change::EnumRebuild { step, .. } => match step {
            pbps_model::EnumRebuildStep::Retype {
                column_uid,
                removed,
                ..
            } => (column(column_uid) && !removed.is_empty()).then_some(U),
            pbps_model::EnumRebuildStep::Create { .. }
            | pbps_model::EnumRebuildStep::Swap { .. } => None,
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
        } => write(write_hit && !spec.nullable && !supplied_new(uid, cs, sides)),
        Change::AlterColumnType {
            uid,
            from_nullable,
            to_nullable,
            from_collation,
            to_collation,
            ..
        } => (((column(uid)
            && (p.risks.contains(&RiskClass::Narrowing) || from_collation != to_collation))
            || (from_nullable != to_nullable && nullability(column(uid), *to_nullable)))
            && !supplied_new(uid, cs, sides))
        .then_some(U),
        Change::AlterColumnNullability {
            uid, to_nullable, ..
        } => (nullability(column(uid), *to_nullable) && !supplied_new(uid, cs, sides)).then_some(U),
        Change::AlterColumnDefault { uid, .. } => {
            write(column(uid) && !supplied_new(uid, cs, sides))
        }
        Change::AlterColumnExpression { uid, .. } => {
            (column(uid) || (entry.writes && hit)).then_some(U)
        }
        Change::AlterColumnNumbering { uid, .. } => write(
            column(uid)
                && (p.risks.contains(&RiskClass::Numbering)
                    || p.risks.contains(&RiskClass::Rename)),
        ),
        Change::DropComputedColumn { table, name, .. } => {
            if !named(name) || entry.writes {
                return None;
            }
            match sides
                .last()
                .and_then(|s| s.ids.tables.get(entry.table()?))
                .and_then(|t| sides.last()?.schema.tables.get(t))
                .and_then(|t| t.computed.get(name))
            {
                Some(final_column)
                    if before.and_then(|t| t.computed.get(name)) == Some(final_column) =>
                {
                    None
                }
                Some(_) => Some(U),
                None => {
                    let restored = cs.changes.iter().any(|p| matches!(&p.change, Change::AddColumn { table: t, name: n, .. } if t == table && n == name));
                    Some(if restored { U } else { B })
                }
            }
        }
        Change::SetPartitionDefault { .. } => write(hit),
        Change::SetPartitionNotNull {
            column, not_null, ..
        } => nullability(named(column), !not_null).then_some(U),
        // A dependent restoration can carry the engine's spelling. Compare
        // the logical before/after declarations, never that physical payload.
        Change::SetPrimaryKey { to, .. } => write(
            write_hit
                && to.is_some()
                && before.and_then(|t| t.primary_key.as_ref())
                    != after.and_then(|t| t.primary_key.as_ref()),
        ),
        Change::AddUnique { name, .. } => write(
            write_hit
                && before.and_then(|t| t.unique.get(name))
                    != after.and_then(|t| t.unique.get(name)),
        ),
        Change::AddCheck { name, .. } => write(
            write_hit
                && before.and_then(|t| t.checks.get(name))
                    != after.and_then(|t| t.checks.get(name)),
        ),
        Change::AddForeignKey {
            name, constraint, ..
        } => {
            let unchanged = before.and_then(|t| t.foreign_keys.get(name))
                == after.and_then(|t| t.foreign_keys.get(name));
            write(
                !unchanged
                    && (write_hit
                        || sides.iter().any(|s| {
                            s.ids
                                .table_uid(&constraint.references_table)
                                .is_some_and(|uid| written.is_some_and(|set| set.contains(uid)))
                        })),
            )
        }
        Change::AddIndex { name, index, .. } => write(
            write_hit
                && index.unique
                && before.and_then(|t| t.indexes.get(name))
                    != after.and_then(|t| t.indexes.get(name)),
        ),
        Change::CreateModule { id, .. } | Change::AlterModule { id, .. } => (entry.target
            == Target::Module(id.clone())
            && !unchanged_module(id)
            && sides
                .first()
                .is_some_and(|s| s.schema.modules.contains_key(id)))
        .then_some(U),
        Change::DropModule { id, .. } => {
            if entry.target != Target::Module(id.clone()) || unchanged_module(id) {
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
