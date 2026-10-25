//! Qualification of explicit blocking rewrites. No executable operation is
//! produced until the order expansion, engine guards and admission are ready.

use std::collections::{BTreeMap, BTreeSet};

use pbps_model::{
    ColumnShape, IdsFile, PartitionStrategy, Reason, Rebuild, RebuildIdentities, Schema, Table,
    TableName, Uid, UidKind,
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

/// What `ids` holds beyond one side of a record: the rest of the environment,
/// which a rewrite of this tree must leave exactly as the files have it.
fn outside(
    ids: &IdsFile,
    mine: &RebuildIdentities,
) -> (
    BTreeMap<Uid, TableName>,
    BTreeMap<Uid, pbps_model::ColumnRef>,
) {
    (
        ids.tables
            .iter()
            .filter(|(u, _)| !mine.tables.contains_key(*u))
            .map(|(u, n)| (u.clone(), n.clone()))
            .collect(),
        ids.columns
            .iter()
            .filter(|(u, _)| !mine.columns.contains_key(*u))
            .map(|(u, c)| (u.clone(), c.clone()))
            .collect(),
    )
}

/// What each replacement column is declared as, kept with the approval so a
/// chain of them can be composed later (DEC-2298.12).
fn shapes(schema: &Schema, replacement: &RebuildIdentities) -> BTreeMap<Uid, ColumnShape> {
    replacement
        .columns
        .iter()
        .filter_map(|(uid, column)| {
            let declared = schema
                .tables
                .get(&column.table)?
                .columns
                .get(&column.name)?;
            Some((
                uid.clone(),
                ColumnShape {
                    ty: declared.ty.clone(),
                    collation: declared.collation.clone(),
                },
            ))
        })
        .collect()
}

fn reapprove(
    ids: &IdsFile,
    schema: &Schema,
    key: &Uid,
    table: &TableName,
    approval: (u64, &Reason, &str, &Context),
) -> Result<IdsFile, RebuildError> {
    let (max_rows, reason, checksum, ctx) = approval;
    let old = &ids.rebuilds[key];
    // The tree must already be resolved: this rewrites the record against the
    // declarations, it does not decide identity for what changed since.
    let replacement = source_identities(
        Side {
            schema,
            ids,
        },
        table,
    )
    .map_err(|_| refused(table, "resolve identity for the changed members first: every member and column must have an identity"))?;
    if let Some(stale) = old
        .replacement
        .tables
        .keys()
        .find(|u| ids.tables.contains_key(*u) && !replacement.tables.contains_key(*u))
        .and_then(|u| ids.tables.get(u))
        .or_else(|| {
            old.replacement
                .columns
                .keys()
                .find(|u| ids.columns.contains_key(*u) && !replacement.columns.contains_key(*u))
                .and_then(|u| ids.columns.get(u).map(|c| &c.table))
        })
    {
        return Err(refused(
            table,
            format!(
                "{} is still recorded but no longer declared; record its drop or rename first",
                stale.shown()
            ),
        ));
    }
    let mut out = ids.clone();
    let approval = out.rebuilds.get_mut(key).unwrap();
    if approval.target_checksum == checksum
        && approval.replacement == replacement
        && approval.types == shapes(schema, &replacement)
        && approval.max_rows == max_rows
        && &approval.reason == reason
    {
        return Ok(out);
    }
    approval.target_checksum = checksum.to_owned();
    approval.types = shapes(schema, &replacement);
    approval.replacement = replacement;
    approval.max_rows = max_rows;
    approval.reason = reason.clone();
    approval.allowed_at = ctx.today.clone();
    approval.operator = ctx.operator.clone();
    out.validate()
        .map_err(|_| refused(table, "the refreshed approval is inconsistent"))?;
    Ok(out)
}

/// Approves a rewrite the way `move-rows` records a move, and switches the
/// identities in the same write (DEC-2298.12): the tree's replacement
/// identities become live, the source ones tombstones, and a lineage record
/// keeps `source -> replacement` for the environments that still hold the
/// source. Repeating an approval keeps the record's source and only refreshes
/// it against the current declarations; a table that was rebuilt and whose key
/// then changes against the committed baseline starts the next link instead.
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
    // An old approval must still pass the column-override guard on both ends.
    refuse_partition_column_overrides(&tree(base.schema, table), table)?;
    refuse_partition_column_overrides(&tree(desired, table), table)?;
    let checksum = after.fingerprint();
    if let Some((key, old)) = ids.rebuild_into(uid) {
        let next_link =
            base.ids.rebuilds.get(key) == Some(old) && shape(base.schema, desired, table).is_ok();
        if !next_link {
            return reapprove(
                ids,
                desired,
                &key.clone(),
                table,
                (max_rows, reason, &checksum, ctx),
            );
        }
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
    let source = source_identities(base, table)?;
    if let Some(moved) = source.tables.keys().find(|u| ids.moves.contains_key(*u)) {
        return Err(refused(
            table,
            format!(
                "a recorded move into {} would be lost; apply or remove it first",
                ids.tables[moved].shown()
            ),
        ));
    }
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
    let mut out = ids.clone();
    for (uid, was) in source
        .tables
        .iter()
        .map(|(u, n)| (u, n.raw().to_string()))
        .chain(source.columns.iter().map(|(u, c)| (u, c.raw().to_string())))
    {
        out.tables.remove(uid);
        out.columns.remove(uid);
        out.tombstones.insert(
            uid.clone(),
            pbps_model::Tombstone {
                was,
                dropped_at: ctx.today.clone(),
                reason: reason.as_ref().into(),
                operator: ctx.operator.clone(),
            },
        );
    }
    out.tables.extend(replacement.tables.clone());
    out.columns.extend(replacement.columns.clone());
    out.rebuilds.insert(
        uid.clone(),
        Rebuild {
            source,
            types: shapes(desired, &replacement),
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
            "the replacement identities are inconsistent with the file",
        )
    })?;
    Ok(out)
}

/// `ids` with one record undone: the sources live again, the replacements and
/// the record gone. Only the caller knows whether that is a withdrawal or a view.
fn restored(ids: &IdsFile, key: &Uid, record: &Rebuild) -> IdsFile {
    let mut out = ids.clone();
    out.rebuilds.remove(key);
    for uid in record.replacement.tables.keys() {
        out.tables.remove(uid);
        out.moves.remove(uid);
    }
    for uid in record.replacement.columns.keys() {
        out.columns.remove(uid);
    }
    for (uid, name) in &record.source.tables {
        out.tombstones.remove(uid);
        out.tables.insert(uid.clone(), name.clone());
    }
    for (uid, column) in &record.source.columns {
        out.tombstones.remove(uid);
        out.columns.insert(uid.clone(), column.clone());
    }
    out
}

/// The identities the unexpanded rewrite is projected over: the tree still
/// under its source UIDs. The logical root carries no identity event of its
/// own, so naming its plan over the switched file would start the tree under
/// UIDs the environment does not hold (DEC-2298.12).
pub fn logical_view(desired: &IdsFile, root: &RebuildPartitionedTable) -> IdsFile {
    let key = root
        .intent
        .source
        .tables
        .iter()
        .find_map(|(uid, name)| (name == &root.table).then_some(uid))
        .expect("a qualified root names its source parent");
    restored(desired, key, &root.intent)
}

/// The inverse of an approval, as an offline edit of the identity file
/// (DEC-2298.12): the source identities come back, the replacement ones and
/// the record go. An environment that already applied the rewrite holds the
/// replacement; its plan refuses by name rather than dropping and creating.
pub fn withdraw(ids: &IdsFile, table: &TableName) -> Result<IdsFile, RebuildError> {
    ids.validate()
        .map_err(|_| refused(table, "the identity file is inconsistent"))?;
    let uid = ids
        .table_uid(table)
        .ok_or_else(|| refused(table, "the table has no identity"))?;
    let (key, record) = ids
        .rebuild_into(uid)
        .ok_or_else(|| refused(table, "no rebuild is recorded for it"))?;
    let retired = record
        .replacement
        .tables
        .keys()
        .chain(record.replacement.columns.keys())
        .any(|u| ids.tombstones.contains_key(u));
    if retired {
        return Err(refused(
            table,
            "a replacement member was dropped after the approval; restoring the sources would bring it back. \
             Use `git revert` on the approval commit instead",
        ));
    }
    let out = restored(ids, key, record);
    out.validate().map_err(|error| {
        refused(
            table,
            format!("the sources cannot be restored beside the current identities: {error}"),
        )
    })?;
    Ok(out)
}

/// The approvals that lead to a live parent, oldest first. A link's source
/// parent is the previous link's replacement parent, and a source is a
/// tombstone while a parent is live, so walking back from a live parent ends.
fn chain<'a>(ids: &'a IdsFile, key: &'a Uid, record: &'a Rebuild) -> Vec<(&'a Uid, &'a Rebuild)> {
    let mut links = vec![(key, record)];
    while let Some(previous) = ids.rebuild_into(links[0].0) {
        links.insert(0, previous);
    }
    links
}

/// Where an environment stands along a chain, read from what its baseline holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stand {
    /// It holds the source of this link, and so none of the later ones.
    Source(usize),
    /// It holds the last replacement: nothing to rebuild.
    Done,
    /// It holds nothing of the chain: the last replacement is created directly.
    Fresh,
    /// Parts of two links, or a link without its parent. Never "nothing to do".
    Partial,
}

fn stand(base: &IdsFile, links: &[(&Uid, &Rebuild)]) -> Stand {
    let held: Vec<_> = links
        .iter()
        .map(|(key, record)| holding(base, key, record))
        .collect();
    let at = |wanted| held.iter().position(|h| *h == wanted);
    let count = |wanted| held.iter().filter(|h| **h == wanted).count();
    match (
        count(Holding::Partial),
        count(Holding::Source),
        count(Holding::Replacement),
    ) {
        (0, 0, 0) => Stand::Fresh,
        (0, 1, 0) => Stand::Source(at(Holding::Source).unwrap()),
        // Holding a replacement parent is holding the next link's source parent.
        (0, 1, 1) if at(Holding::Replacement).unwrap() + 1 == at(Holding::Source).unwrap() => {
            Stand::Source(at(Holding::Source).unwrap())
        }
        (0, 0, 1) if at(Holding::Replacement) == Some(links.len() - 1) => Stand::Done,
        _ => Stand::Partial,
    }
}

/// Match the approved lineage and target shape against the environment's
/// baseline. It holds the source of some link; a chain it is behind on is
/// rebuilt once, from what it holds to the latest replacement, so only the
/// latest checksum must match today's declarations (DEC-2298.12). Catalog
/// publication/dependent readability and row/size observations belong to
/// connected qualification, never an empty default supplied here.
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
    let (key, head) = desired
        .ids
        .table_uid(table)
        .and_then(|uid| desired.ids.rebuild_into(uid))
        .ok_or_else(|| refused(table, "no explicit rebuild intent is recorded"))?;
    let links = chain(desired.ids, key, head);
    let start = match stand(base.ids, &links) {
        Stand::Source(start) => start,
        Stand::Partial => {
            return Err(refused(
                table,
                "the baseline holds only part of an approved source or replacement lineage",
            ));
        }
        Stand::Done | Stand::Fresh => {
            return Err(refused(
                table,
                "the baseline does not hold an approved source lineage",
            ));
        }
    };
    let used = &links[start..];
    let (first, intent) = (used[0].1, head);
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
    if intent.target_checksum != after.fingerprint() {
        return Err(refused(
            table,
            format!(
                "the declarations changed after the approval; re-approve it with \
                 `pbps rebuild-table {} --max-rows {} --reason <why>`",
                table.shown(),
                intent.max_rows
            ),
        ));
    }
    if base.ids.roles != desired.ids.roles
        || base.ids.types != desired.ids.types
        || first.source != source_identities(base, table)?
        || intent.replacement != identities(desired.ids, &tree(desired.schema, table))
        || outside(base.ids, &first.source) != outside(desired.ids, &intent.replacement)
    {
        return Err(refused(
            table,
            "the approved source lineage or desired fingerprint does not match",
        ));
    }
    Ok(RebuildPartitionedTable {
        table: table.clone(),
        intent: intent.clone(),
        earlier: used[..used.len() - 1]
            .iter()
            .map(|(_, record)| (*record).clone())
            .collect(),
        before: tree(base.schema, table),
        after: tree(desired.schema, table),
    })
}

/// The ledger state a rebuild leaves: the files' identities, which approval
/// already switched. Proposed only; planning never consumes anything.
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
    desired
        .ids
        .check_held_by(desired.schema)
        .map_err(|e| refused(&root.table, e.to_string()))?;
    desired
        .ids
        .clone()
        .validated()
        .map_err(|e| refused(&root.table, e.to_string()))
}

/// The tables a historical identity mapping holds in the form they had before
/// a rewrite the `current` files approved since (DEC-2298.12). Restoring such a
/// state is a reverse rebuild, which does not exist; a declaration export of it
/// would only be refused later, at the first plan, with less to go on.
pub fn rebuilt_since(historical: &IdsFile, current: &IdsFile) -> Vec<TableName> {
    current
        .rebuilds
        .iter()
        .filter(|(key, _)| historical.tables.contains_key(*key))
        .filter_map(|(key, record)| record.source.tables.get(key).cloned())
        .collect()
}

/// What an environment's baseline holds of one lineage record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holding {
    /// The source parent: the record supplies the rebuild.
    Source,
    /// The replacement parent: nothing to rebuild.
    Replacement,
    /// Neither: the replacement is created directly.
    Neither,
    /// Parts of both, or of one without its parent. Never "nothing to rebuild".
    Partial,
}

pub fn holding(base: &IdsFile, key: &Uid, record: &Rebuild) -> Holding {
    let has = |ids: &RebuildIdentities| {
        ids.tables
            .keys()
            .filter(|u| base.tables.contains_key(*u))
            .count()
            + ids
                .columns
                .keys()
                .filter(|u| base.columns.contains_key(*u))
                .count()
    };
    let size = |ids: &RebuildIdentities| ids.tables.len() + ids.columns.len();
    let Some(root) = record.replacement_root(key) else {
        return Holding::Partial;
    };
    match (
        base.tables.contains_key(key),
        base.tables.contains_key(root),
    ) {
        (true, false)
            if has(&record.source) == size(&record.source) && has(&record.replacement) == 0 =>
        {
            Holding::Source
        }
        (false, true) if has(&record.source) == 0 => Holding::Replacement,
        (false, false) if has(&record.source) + has(&record.replacement) == 0 => Holding::Neither,
        _ => Holding::Partial,
    }
}

/// The refusals the lineage records impose on a plan from `base` to `declared`.
/// Each live parent's chain of approvals is read against what the baseline holds.
pub(crate) fn lineage_errors(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &(impl pbps_dialect::Dialect + ?Sized),
) -> Vec<crate::DiffError> {
    let error = |table: TableName, why: String| crate::DiffError::RebuildTable { table, why };
    let mut errors = Vec::new();
    // A record the identities around it contradict is unreadable lineage, never
    // "nothing to rebuild" (DEC-2298.12).
    if !declared.ids.rebuilds.is_empty()
        && let Err(unreadable) = declared.ids.validate()
    {
        let named = match &unreadable {
            pbps_model::ids::IdsError::InvalidRebuild { uid } => declared
                .ids
                .rebuilds
                .get(uid)
                .and_then(|r| r.source.tables.get(uid)),
            pbps_model::ids::IdsError::UnsupportedVersion { .. }
            | pbps_model::ids::IdsError::LiveAndTombstoned { .. }
            | pbps_model::ids::IdsError::DuplicateName { .. }
            | pbps_model::ids::IdsError::KindMismatch { .. }
            | pbps_model::ids::IdsError::OrphanColumn { .. }
            | pbps_model::ids::IdsError::OrphanMove { .. }
            | pbps_model::ids::IdsError::NotHeld { .. } => None,
        };
        errors.push(error(
            named.cloned().unwrap_or_else(|| TableName::new("?", "?")),
            unreadable.to_string(),
        ));
        return errors;
    }
    for (key, record) in &declared.ids.rebuilds {
        // Only the latest link of a chain is a parent the files hold live; the
        // earlier links are read through it.
        let Some(table) = record
            .replacement_root(key)
            .and_then(|root| declared.ids.tables.get(root))
            .cloned()
        else {
            continue;
        };
        // Only PostgreSQL can hold a record the CLI wrote; one in another
        // engine's files was edited in by hand and is not read as history.
        if dialect.name() != "postgres" {
            errors.push(error(
                table,
                "this engine does not support table rebuilds".to_owned(),
            ));
            continue;
        }
        let links = chain(declared.ids, key, record);
        match stand(base.ids, &links) {
            Stand::Done | Stand::Fresh => {}
            Stand::Partial => errors.push(error(
                table,
                "the environment holds only part of the approved rewrite's source or replacement identities; \
                 it cannot be read as rebuilt or as not yet rebuilt"
                    .to_owned(),
            )),
            Stand::Source(start) => {
                let composed = links.len() - start;
                let why = match qualify(base, declared, &table, dialect) {
                    Ok(_) if composed > 1 => format!(
                        "recorded intent is valid, as one rebuild composed over {composed} approved links; \
                         table rebuild execution is not available yet"
                    ),
                    Ok(_) => "recorded intent is valid; table rebuild execution is not available yet"
                        .to_owned(),
                    Err(error) => error.why,
                };
                errors.push(error(table, why));
            }
        }
    }
    // The files no longer record a rewrite this environment already applied
    // (withdrawn after the fact, or an older revision): the replacement it
    // holds would read as a table to drop and a source to create.
    for (key, record) in &base.ids.rebuilds {
        let Some(root) = record.replacement_root(key) else {
            continue;
        };
        if base.ids.tables.contains_key(root)
            && !declared.ids.tables.contains_key(root)
            && !declared.ids.tombstones.contains_key(root)
        {
            errors.push(error(
                base.ids.tables[root].clone(),
                "this environment already applied an approved rewrite that the identity files no longer record \
                 (withdrawn afterwards, or an older revision or state); neither a drop and create nor a \
                 reverse rebuild is planned, so restore the approval to plan from here"
                    .to_owned(),
            ));
        }
    }
    errors
}

#[cfg(test)]
mod tests;

// One case table drives pure resolution, offline CLI and both live engines.
// It must not become a second description of the supported member lifecycle.
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub mod member_cases;
