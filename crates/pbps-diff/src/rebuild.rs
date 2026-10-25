//! Qualification of explicit blocking rewrites. No executable operation is
//! produced until the order expansion, engine guards and admission are ready.

use std::collections::{BTreeMap, BTreeSet};

use pbps_model::{
    IdsFile, PartitionStrategy, Reason, Rebuild, RebuildIdentities, Schema, Table, TableName, Uid,
    UidKind,
};

use crate::{Context, Side};

pub use pbps_model::RebuildPartitionedTable;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot rebuild {table}: {why}", table = .table.shown())]
pub struct RebuildError {
    pub table: TableName,
    pub why: String,
}

fn refused(table: &TableName, why: impl Into<String>) -> RebuildError {
    RebuildError {
        table: table.clone(),
        why: why.into(),
    }
}

fn tree(schema: &Schema, root: &TableName) -> BTreeMap<TableName, Table> {
    schema
        .tables
        .iter()
        .filter(|(name, t)| {
            *name == root || t.partition_of.as_ref().is_some_and(|of| &of.parent == root)
        })
        .map(|(n, t)| (n.clone(), t.clone()))
        .collect()
}

fn identities(ids: &IdsFile, tables: &BTreeMap<TableName, Table>) -> RebuildIdentities {
    RebuildIdentities {
        tables: ids
            .tables
            .iter()
            .filter(|(_, n)| tables.contains_key(*n))
            .map(|(u, n)| (u.clone(), n.clone()))
            .collect(),
        columns: ids
            .columns
            .iter()
            .filter(|(_, c)| {
                tables
                    .get(&c.table)
                    .is_some_and(|t| t.columns.contains_key(&c.name))
            })
            .map(|(u, c)| (u.clone(), c.clone()))
            .collect(),
    }
}

/// Only this constructor may prepare a rebuild's shape and fingerprint. Raw
/// declarations and catalog reads must converge before either is compared.
struct CanonicalSchema(Schema);

fn canonical_schema(
    schema: &Schema,
    dialect: &(impl pbps_dialect::Dialect + ?Sized),
) -> CanonicalSchema {
    let mut schema = schema.clone();
    for definition in schema.types.values_mut() {
        if let pbps_model::ManagedType::Domain { base, .. } = definition
            && let Ok(ty) = dialect.normalize_type(base)
        {
            *base = ty;
        }
    }
    for table in schema.tables.values_mut() {
        for column in table.columns.values_mut() {
            if let Ok(ty) = dialect.normalize_type(&column.ty) {
                column.ty = ty;
            }
        }
    }
    CanonicalSchema(schema)
}

impl CanonicalSchema {
    fn fingerprint(&self) -> String {
        pbps_model::state_checksum(&self.0, &IdsFile::default())
    }
}

fn source_identities(base: Side<'_>, table: &TableName) -> Result<RebuildIdentities, RebuildError> {
    let members = tree(base.schema, table);
    let source = identities(base.ids, &members);
    let ids = source.as_ids();
    if members.iter().any(|(name, t)| {
        ids.table_uid(name).is_none()
            || (t.partition_of.is_none()
                && t.columns
                    .keys()
                    .any(|c| ids.column_uid(&name.column(c)).is_none()))
    }) {
        return Err(refused(
            table,
            "every source table and column must have an identity",
        ));
    }
    Ok(source)
}

fn refuse_partition_column_overrides(
    members: &BTreeMap<TableName, Table>,
    table: &TableName,
) -> Result<(), RebuildError> {
    // A partition's own defaults and NOT NULLs are not Table::columns:
    // those are empty because its columns belong to the parent.
    for (name, member) in members {
        if let Some(column) = member
            .partition_of
            .as_ref()
            .and_then(|of| of.columns.keys().next())
        {
            return Err(refused(
                table,
                format!(
                    "{} has a partition column override; only plain copied columns are qualified",
                    name.column(column).shown()
                ),
            ));
        }
    }
    Ok(())
}

fn shape(base: &Schema, desired: &Schema, table: &TableName) -> Result<(), RebuildError> {
    for schema in [base, desired] {
        let Some(root) = schema.tables.get(table) else {
            return Err(refused(
                table,
                "the source and desired parent must both be declared",
            ));
        };
        if root.partition_of.is_some()
            || !root.partition_by.as_ref().is_some_and(|by| {
                by.strategy == PartitionStrategy::Range
                    && !by.external
                    && !by.columns.is_empty()
                    && by.columns.iter().all(|c| root.columns.contains_key(c))
            })
        {
            return Err(refused(table, "only a managed RANGE parent can be rebuilt"));
        }
        let members = tree(schema, table);
        refuse_partition_column_overrides(&members, table)?;
        if members.values().any(|t| {
            (t.partition_of.is_some() && t.partition_by.is_some())
                || !t.computed.is_empty()
                || t.columns
                    .values()
                    .any(|c| c.value_source != pbps_model::ValueSource::None)
                || t.system_time.is_some()
                || t.data.is_some()
                || t.replica_identity.is_some()
                || t.row_security.enabled
                || t.row_security.forced
                || !t.row_policies.is_empty()
        }) || schema.tables.values().any(|t| {
            t.partition_of
                .as_ref()
                .is_some_and(|of| &of.parent != table && members.contains_key(&of.parent))
        }) {
            return Err(refused(
                table,
                "only a flat tree with plain copied columns is qualified",
            ));
        }
    }
    let before = &base.tables[table];
    let after = &desired.tables[table];
    let changed = before.partition_by != after.partition_by
        || before
            .partition_by
            .as_ref()
            .unwrap()
            .columns
            .iter()
            .any(|key| {
                before.columns.get(key).map(|c| (&c.ty, &c.collation))
                    != after.columns.get(key).map(|c| (&c.ty, &c.collation))
            });
    if !changed {
        return Err(refused(
            table,
            "neither the partition key nor a key column type changed",
        ));
    }
    let mut outside_before = base.clone();
    let mut outside_after = desired.clone();
    for name in tree(base, table).keys() {
        outside_before.tables.remove(name);
    }
    for name in tree(desired, table).keys() {
        outside_after.tables.remove(name);
    }
    if outside_before != outside_after {
        return Err(refused(
            table,
            "unrelated schema edits must be planned separately",
        ));
    }
    Ok(())
}

/// Records intent like move-rows: convergent for the same fingerprint, reason
/// and cap; another approval updates audit fields but preserves reserved uids.
#[allow(clippy::too_many_arguments)]
pub fn record(
    base: Side<'_>,
    desired: &Schema,
    ids: &IdsFile,
    table: &TableName,
    max_rows: u64,
    reason: &Reason,
    ctx: &Context,
    dialect: &(impl pbps_dialect::Dialect + ?Sized),
) -> Result<IdsFile, RebuildError> {
    ids.validate()
        .map_err(|_| refused(table, "the identity file is inconsistent"))?;
    if max_rows == 0 {
        return Err(refused(table, "max_rows must be positive"));
    }
    let uid = ids
        .table_uid(table)
        .ok_or_else(|| refused(table, "the source parent must already have an identity"))?;
    let before = canonical_schema(base.schema, dialect);
    let after = canonical_schema(desired, dialect);
    let base = Side {
        schema: &before.0,
        ids: base.ids,
    };
    let desired = &after.0;
    // Re-recording after committing the desired key is convergent, but an
    // old approval must still pass the column-override guard on both ends.
    refuse_partition_column_overrides(&tree(base.schema, table), table)?;
    refuse_partition_column_overrides(&tree(desired, table), table)?;
    let checksum = after.fingerprint();
    let mut out = ids.clone();
    if let Some(old) = ids.rebuilds.get(uid) {
        if old.target_checksum != checksum {
            return Err(refused(
                table,
                "the desired schema changed after approval; the recorded rebuild is stale",
            ));
        }
        if old.max_rows == max_rows && &old.reason == reason {
            return Ok(out);
        }
        let approval = out.rebuilds.get_mut(uid).unwrap();
        approval.max_rows = max_rows;
        approval.reason = reason.clone();
        approval.allowed_at = ctx.today.clone();
        approval.operator = ctx.operator.clone();
        return Ok(out);
    }
    shape(base.schema, desired, table)?;
    if base.ids.tables != ids.tables
        || base.ids.columns != ids.columns
        || base.ids.roles != ids.roles
        || base.ids.types != ids.types
    {
        return Err(refused(
            table,
            "commit the source identities before recording a rebuild",
        ));
    }
    if !ids.rebuilds.is_empty() {
        return Err(refused(
            table,
            "only one blocking rewrite may be recorded at a time",
        ));
    }
    let source = source_identities(base, table)?;
    let mut reserved = BTreeSet::new();
    let mut fresh = |kind| loop {
        let uid = Uid::generate(kind);
        if !ids.contains_uid(&uid) && reserved.insert(uid.clone()) {
            break uid;
        }
    };
    let mut replacement = RebuildIdentities {
        tables: BTreeMap::new(),
        columns: BTreeMap::new(),
    };
    for (name, target) in tree(desired, table) {
        replacement
            .tables
            .insert(fresh(UidKind::Table), name.clone());
        for column in target
            .columns
            .keys()
            .filter(|_| target.partition_of.is_none())
        {
            replacement
                .columns
                .insert(fresh(UidKind::Column), name.column(column));
        }
    }
    out.rebuilds.insert(
        uid.clone(),
        Rebuild {
            source,
            replacement,
            target_checksum: checksum,
            max_rows,
            allowed_at: ctx.today.clone(),
            reason: reason.clone(),
            operator: ctx.operator.clone(),
        },
    );
    out.validate().map_err(|_| {
        refused(
            table,
            "the reserved replacement identities are inconsistent",
        )
    })?;
    Ok(out)
}

/// Match the approved lineage and target shape against the actual baseline.
/// Catalog publication/dependent readability and row/size observations belong
/// to connected qualification, never an empty default supplied here.
pub fn qualify(
    base: Side<'_>,
    desired: Side<'_>,
    table: &TableName,
    dialect: &(impl pbps_dialect::Dialect + ?Sized),
) -> Result<RebuildPartitionedTable, RebuildError> {
    desired
        .ids
        .validate()
        .map_err(|_| refused(table, "the identity file is inconsistent"))?;
    let intent = desired
        .ids
        .table_uid(table)
        .and_then(|uid| desired.ids.rebuilds.get(uid))
        .ok_or_else(|| refused(table, "no explicit rebuild intent is recorded"))?;
    let before = canonical_schema(base.schema, dialect);
    let after = canonical_schema(desired.schema, dialect);
    let base = Side {
        schema: &before.0,
        ids: base.ids,
    };
    let desired = Side {
        schema: &after.0,
        ids: desired.ids,
    };
    shape(base.schema, desired.schema, table)?;
    if intent.target_checksum != after.fingerprint()
        || base.ids.tables != desired.ids.tables
        || base.ids.columns != desired.ids.columns
        || base.ids.roles != desired.ids.roles
        || base.ids.types != desired.ids.types
        || intent.source != source_identities(base, table)?
        || intent.replacement.tables.values().collect::<BTreeSet<_>>()
            != tree(desired.schema, table).keys().collect::<BTreeSet<_>>()
        || intent
            .replacement
            .columns
            .values()
            .cloned()
            .collect::<BTreeSet<_>>()
            != tree(desired.schema, table)
                .iter()
                .filter(|(_, t)| t.partition_of.is_none())
                .flat_map(|(n, t)| t.columns.keys().map(|c| n.column(c)))
                .collect::<BTreeSet<_>>()
    {
        return Err(refused(
            table,
            "the approved source lineage or desired fingerprint does not match",
        ));
    }
    Ok(RebuildPartitionedTable {
        table: table.clone(),
        intent: intent.clone(),
        before: tree(base.schema, table),
        after: tree(desired.schema, table),
    })
}

/// Proposed ledger state only: the recorded file is consumed by PR4 after
/// successful execution, never by planning this transition (DEC-2298.2).
pub fn target_identities(
    base: Side<'_>,
    desired: Side<'_>,
    root: &RebuildPartitionedTable,
    dialect: &(impl pbps_dialect::Dialect + ?Sized),
) -> Result<pbps_model::ValidIds, RebuildError> {
    if qualify(base, desired, &root.table, dialect)? != *root {
        return Err(refused(
            &root.table,
            "rebuild payload differs from the approved tree",
        ));
    }
    let mut ids = desired.ids.clone();
    ids.rebuilds
        .remove(desired.ids.table_uid(&root.table).unwrap());
    for (uid, was) in root
        .intent
        .source
        .tables
        .iter()
        .map(|(u, n)| (u, n.raw().to_string()))
        .chain(
            root.intent
                .source
                .columns
                .iter()
                .map(|(u, n)| (u, n.raw().to_string())),
        )
    {
        ids.tables.remove(uid);
        ids.columns.remove(uid);
        ids.moves.remove(uid);
        ids.tombstones.insert(
            uid.clone(),
            pbps_model::Tombstone {
                was,
                dropped_at: root.intent.allowed_at.clone(),
                reason: root.intent.reason.as_ref().into(),
                operator: root.intent.operator.clone(),
            },
        );
    }
    ids.tables.extend(root.intent.replacement.tables.clone());
    ids.columns.extend(root.intent.replacement.columns.clone());
    ids.check_held_by(desired.schema)
        .map_err(|e| refused(&root.table, e.to_string()))?;
    ids.validated()
        .map_err(|e| refused(&root.table, e.to_string()))
}

#[cfg(test)]
mod tests;

// One case table drives pure resolution, offline CLI and both live engines.
// It must not become a second description of the supported member lifecycle.
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub mod member_cases;
