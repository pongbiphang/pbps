//! Qualification of explicit blocking rewrites. No executable operation is
//! produced until the order expansion, engine guards and admission are ready.

use std::collections::{BTreeMap, BTreeSet};

use pbps_model::{
    IdsFile, PartitionStrategy, Reason, Rebuild, RebuildIdentities, Schema, Table, TableName, Uid,
    UidKind,
};

use crate::{Context, Side};

/// The logical root's typed source and destination, ready for a future order
/// expansion. It deliberately cannot be inserted into an executable ChangeSet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildPartitionedTable {
    pub table: TableName,
    pub intent: Rebuild,
    pub before: BTreeMap<TableName, Table>,
    pub after: BTreeMap<TableName, Table>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot rebuild {table}: {why}", table = .table.shown())]
pub struct RebuildError {
    pub table: TableName,
    pub why: &'static str,
}

fn refused(table: &TableName, why: &'static str) -> RebuildError {
    RebuildError {
        table: table.clone(),
        why,
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

fn fingerprint(schema: &Schema) -> String {
    pbps_model::state_checksum(schema, &IdsFile::default())
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
) -> Result<IdsFile, RebuildError> {
    ids.validate()
        .map_err(|_| refused(table, "the identity file is inconsistent"))?;
    if max_rows == 0 {
        return Err(refused(table, "max_rows must be positive"));
    }
    let uid = ids
        .table_uid(table)
        .ok_or_else(|| refused(table, "the source parent must already have an identity"))?;
    let checksum = fingerprint(desired);
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
    shape(base.schema, desired.schema, table)?;
    if intent.target_checksum != fingerprint(desired.schema)
        || base.ids.tables != desired.ids.tables
        || base.ids.columns != desired.ids.columns
        || base.ids.roles != desired.ids.roles
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

#[cfg(test)]
mod tests;
