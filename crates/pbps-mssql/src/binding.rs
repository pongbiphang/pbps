//! Schema binding: which standing modules the engine refuses to leave beside a
//! change (DEC-2329.1).
//!
//! `WITH SCHEMABINDING` makes a module a dependent the engine enforces. Measured
//! on 17.0, a bound view stops `ALTER COLUMN` (a retype, a collation or a
//! nullability change, 5074 behind 4922), `DROP COLUMN`, `sp_rename` of a column
//! or of the table (15336) and `DROP TABLE` (3729) on what it reads, and
//! `ALTER` or `DROP` (3729) of a view or function it reads. A column it does
//! not read, a default, a check and a primary key are not stopped.
//!
//! The answer is a rebuild: drop the module before the change and create it
//! again after. The differ's classes already do that bracketing (module drops
//! first, creates after the table changes), so this only says *which* modules
//! need it; no pass or tie-break is added. Both the differ and the engine
//! effects ask the same question here, so the plan the first writes is the
//! plan the second verifies.
//!
//! The answer is lexical, by the dialect's own name scan, and over-approximates
//! (a false yes rebuilds a view that needed no rebuild; a false no leaves the
//! engine to refuse the apply, which rolls back). The connected plan's catalog
//! edges (DEC-1431.1) are the stricter judge for computed columns; modules are
//! screened by text on both paths.

use std::collections::BTreeSet;

use pbps_dialect::Dialect as _;
use pbps_model::{Change, Module, ModuleId, ModuleKind, Schema, TableName};

use crate::Mssql;

/// Whether the module holds `WITH SCHEMABINDING`: a view by its flag, a
/// function by its own header, where the model keeps every option it has.
pub(crate) fn is_schema_bound(module: &Module) -> bool {
    match module.kind {
        ModuleKind::View => module.options.schemabinding,
        ModuleKind::Function => crate::introspect::function_is_schema_bound(&module.definition),
        ModuleKind::Procedure | ModuleKind::Trigger => false,
    }
}

fn bound(schema: &Schema) -> impl Iterator<Item = (&ModuleId, &Module)> {
    schema.modules.iter().filter(|(_, m)| is_schema_bound(m))
}

/// The bound modules `change` alone stops from standing, before any module
/// that is bound to *them*.
pub(crate) fn blocked_by(dialect: &Mssql, base: &Schema, change: &Change) -> BTreeSet<ModuleId> {
    let reading = |table: &TableName, column: Option<&str>| {
        bound(base)
            .filter(|(_, m)| {
                dialect.may_name_qualified(&m.definition, &table.schema, &table.name)
                    && column.is_none_or(|c| dialect.may_name(&m.definition, c))
            })
            .map(|(id, _)| id.clone())
            .collect()
    };
    match change {
        Change::AlterColumnType { column, .. }
        | Change::AlterColumnNullability { column, .. }
        | Change::DropColumn { column, .. } => reading(&column.table, Some(column.name.raw())),
        Change::RenameColumn { table, from, .. } => reading(table, Some(from.raw())),
        // A computed column a view reads is a column of its table.
        Change::DropComputedColumn { table, name, .. } => reading(table, Some(name.raw())),
        Change::DropTable { name, .. } | Change::RenameTable { from: name, .. } => {
            reading(name, None)
        }
        // A bound *view* over a view or function cannot stand while that
        // module is altered or dropped. A bound function's caller keeps the
        // ordering DEC-2192.1 and DEC-2228.1 give it: it is altered in place,
        // after or before its callee by the effects, and only a cycle is
        // refused.
        Change::AlterModule { id, .. } | Change::DropModule { id, .. } => {
            let Some(name) = id.referenced_name() else {
                return BTreeSet::new();
            };
            bound(base)
                .filter(|(m, module)| {
                    *m != id
                        && module.kind == ModuleKind::View
                        && dialect.may_name_qualified(&module.definition, &name.schema, &name.name)
                })
                .map(|(m, _)| m.clone())
                .collect()
        }
        // Listed rather than a wildcard, so a new kind of change has to say
        // whether a bound module can stand beside it.
        Change::RebuildPartitionedTable(_)
        | Change::BuildPartitionedIndex(_)
        | Change::CopyTableRows(_)
        | Change::AnalyzeTableTree(_)
        | Change::CreateTable { .. }
        | Change::CreateModule { .. }
        | Change::DetachPartition { .. }
        | Change::AttachPartition { .. }
        | Change::AddColumn { .. }
        | Change::AlterColumnDefault { .. }
        | Change::AlterColumnExpression { .. }
        | Change::AlterColumnNumbering { .. }
        | Change::SetColumnDeprecated { .. }
        | Change::SetPrimaryKey { .. }
        | Change::SetIndexStorageParameters { .. }
        | Change::SetTablePersistence { .. }
        | Change::SetPartitionDefault { .. }
        | Change::SetPartitionNotNull { .. }
        | Change::DropPolicy { .. }
        | Change::CreatePolicy { .. }
        | Change::SetRowSecurity { .. }
        | Change::SetStorageParameters { .. }
        | Change::SetReplicaIdentity { .. }
        | Change::AddUnique { .. }
        | Change::DropUnique { .. }
        | Change::AddForeignKey { .. }
        | Change::DropForeignKey { .. }
        | Change::AddCheck { .. }
        | Change::DropCheck { .. }
        | Change::AddIndex { .. }
        | Change::AddComputedColumn { .. }
        | Change::DropIndex { .. }
        | Change::InsertRow { .. }
        | Change::UpdateRow { .. }
        | Change::DeleteRow { .. }
        | Change::SetDataMode { .. }
        | Change::SetEnsureKeys { .. }
        | Change::CreateType { .. }
        | Change::DropType { .. }
        | Change::RenameType { .. }
        | Change::CreateRole { .. }
        | Change::DropRole { .. }
        | Change::RenameRole { .. }
        | Change::Grant { .. }
        | Change::Revoke { .. }
        | Change::AddEnumValue { .. }
        | Change::RenameEnumValue { .. }
        | Change::EnumRebuild { .. }
        | Change::PublicExecution { .. } => BTreeSet::new(),
    }
}

/// Every bound module the changes stop from standing, and every bound module
/// bound to one of those: a view cannot be dropped while a bound caller reads
/// it, so the caller goes first and comes back last, whatever it is.
pub(crate) fn torn_down(dialect: &Mssql, base: &Schema, changes: &[Change]) -> BTreeSet<ModuleId> {
    let mut torn: BTreeSet<ModuleId> = changes
        .iter()
        .flat_map(|change| blocked_by(dialect, base, change))
        .collect();
    close(dialect, base, &mut torn);
    torn
}

/// [`torn_down`] for one change, with the modules bound to them.
pub(crate) fn torn_down_by(dialect: &Mssql, base: &Schema, change: &Change) -> BTreeSet<ModuleId> {
    let mut torn = blocked_by(dialect, base, change);
    close(dialect, base, &mut torn);
    torn
}

fn close(dialect: &Mssql, base: &Schema, torn: &mut BTreeSet<ModuleId>) {
    loop {
        let readers: BTreeSet<ModuleId> = bound(base)
            .filter(|(id, module)| {
                !torn.contains(*id)
                    && torn.iter().any(|gone| {
                        gone.referenced_name().is_some_and(|name| {
                            dialect.may_name_qualified(&module.definition, &name.schema, &name.name)
                        })
                    })
            })
            .map(|(id, _)| id.clone())
            .collect();
        if readers.is_empty() {
            return;
        }
        torn.extend(readers);
    }
}
