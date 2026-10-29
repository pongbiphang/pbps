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

/// Whether one rewrite can take the tree from `base` to `desired`. `link` also
/// asks for what an approval requires of its own rewrite, a changed key or key
/// column type; a composed chain does not, since `A -> B -> A` nets to none.
fn shape(
    base: &Schema,
    desired: &Schema,
    table: &TableName,
    link: bool,
) -> Result<(), RebuildError> {
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
    if link && !changed {
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

/// The parent and the columns a rewrite carries over, paired by name at the
/// approval: the one moment the two snapshots are evidence of what continues
/// what. The pairing is kept as UIDs, so a rename after it breaks nothing
/// (DEC-2298.12).
fn continuing(
    source: &RebuildIdentities,
    replacement: &RebuildIdentities,
    table: &TableName,
) -> Option<(Uid, BTreeMap<Uid, Uid>)> {
    let by_name = |ids: &RebuildIdentities| -> Option<Uid> {
        ids.tables
            .iter()
            .find_map(|(uid, name)| (name == table).then(|| uid.clone()))
    };
    let (from, to) = (by_name(source)?, by_name(replacement)?);
    let mut pairs = BTreeMap::from([(to.clone(), from)]);
    for (uid, column) in &replacement.columns {
        if let Some((was, _)) = source.columns.iter().find(|(_, c)| *c == column) {
            pairs.insert(uid.clone(), was.clone());
        }
    }
    Some((to, pairs))
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
    let conversions: BTreeMap<_, _> = replacement
        .columns
        .keys()
        .filter(|uid| old.continues.contains_key(*uid))
        .filter_map(|uid| ids.conversions.get(uid).map(|c| (uid.clone(), c.clone())))
        .collect();
    let approval = out.rebuilds.get_mut(key).unwrap();
    if approval.target_checksum == checksum
        && approval.replacement == replacement
        && approval.types == shapes(schema, &replacement)
        && approval.conversions == conversions
        && approval.max_rows == max_rows
        && &approval.reason == reason
    {
        return Ok(out);
    }
    approval.target_checksum = checksum.to_owned();
    approval.types = shapes(schema, &replacement);
    approval.conversions = conversions;
    // What a member continues does not change with its name; only a member
    // the file no longer holds leaves the pairing.
    approval.continues.retain(|to, _| {
        replacement.tables.contains_key(to) || replacement.columns.contains_key(to)
    });
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
        let next_link = base.ids.rebuilds.get(key) == Some(old)
            && shape(base.schema, desired, table, true).is_ok();
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
    shape(base.schema, desired, table, true)?;
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
    let (replacement_root, continues) =
        continuing(&source, &replacement, table).ok_or_else(|| {
            refused(
                table,
                "the source and the replacement must both hold the parent",
            )
        })?;
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
                require: None,
                was,
                dropped_at: ctx.today.clone(),
                reason: reason.as_ref().into(),
                operator: ctx.operator.clone(),
            },
        );
    }
    let conversions = continues
        .iter()
        .filter_map(|(to, from)| {
            let conversion = out.conversions.remove(from)?;
            out.conversions.insert(to.clone(), conversion.clone());
            Some((to.clone(), conversion))
        })
        .collect();
    out.tables.extend(replacement.tables.clone());
    out.columns.extend(replacement.columns.clone());
    out.rebuilds.insert(
        uid.clone(),
        Rebuild {
            source,
            types: shapes(desired, &replacement),
            conversions,
            replacement,
            replacement_root,
            continues,
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
    // A conversion recorded for a replacement column went with the column, as
    // a move went with its table; left behind it would orphan the file.
    for uid in record.replacement.columns.keys() {
        out.columns.remove(uid);
        out.conversions.remove(uid);
    }
    for (uid, name) in &record.source.tables {
        out.tombstones.remove(uid);
        out.tables.insert(uid.clone(), name.clone());
    }
    for (to, conversion) in &record.conversions {
        if let Some(from) = record.continues.get(to) {
            out.conversions.insert(from.clone(), conversion.clone());
        }
    }
    for (uid, column) in &record.source.columns {
        out.tombstones.remove(uid);
        out.columns.insert(uid.clone(), column.clone());
    }
    out
}

fn restored_chain<'a>(ids: &IdsFile, links: impl Iterator<Item = &'a Rebuild>) -> IdsFile {
    let links: Vec<_> = links.collect();
    let source = &links[0].source;
    let mut out = ids.clone();
    for record in links.iter().rev() {
        out = restored(&out, record.source_root().unwrap(), record);
    }
    // A member born between approvals never existed at the oldest source.
    // Restoring each link separately would leave that intermediate UID live.
    for record in links {
        for uid in record
            .source
            .tables
            .keys()
            .chain(record.source.columns.keys())
            .filter(|uid| !source.tables.contains_key(*uid) && !source.columns.contains_key(*uid))
        {
            out.tables.remove(uid);
            out.columns.remove(uid);
            out.moves.remove(uid);
            out.conversions.remove(uid);
        }
    }
    out
}

/// The identities the unexpanded rewrite is projected over: the tree still
/// under its source UIDs. The logical root carries no identity event of its
/// own, so naming its plan over the switched file would start the tree under
/// UIDs the environment does not hold (DEC-2298.12).
pub fn logical_view(desired: &IdsFile, root: &RebuildPartitionedTable) -> IdsFile {
    restored_chain(desired, root.links())
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
    let stale = || {
        refused(
            table,
            format!(
                "the declarations changed after the approval; re-approve it with \
                 `pbps rebuild-table {} --max-rows {} --reason <why>`",
                table.shown(),
                intent.max_rows
            ),
        )
    };
    // The two parents are paired by identity, so a rename since the approval
    // is seen and not mistaken for a missing table. A stale approval is
    // refreshed first; one that is current still cannot rebuild into another
    // name, which one table name in the payload does not carry yet.
    if let Some(was) = first.source.tables.get(used[0].0)
        && was != table
    {
        return Err(if intent.target_checksum == after.fingerprint() {
            refused(
                table,
                format!(
                    "the replacement parent was renamed after the approval (the source is {}); \
                     a rebuild across a rename is not available yet, so apply the rename on its \
                     own after the rebuild or withdraw the approval",
                    was.shown()
                ),
            )
        } else {
            stale()
        });
    }
    shape(base.schema, desired.schema, table, used.len() == 1)?;
    if intent.conversions
        != intent
            .replacement
            .columns
            .keys()
            .filter(|uid| intent.continues.contains_key(*uid))
            .filter_map(|uid| {
                desired
                    .ids
                    .conversions
                    .get(uid)
                    .map(|c| (uid.clone(), c.clone()))
            })
            .collect()
    {
        return Err(stale());
    }
    if intent.target_checksum != after.fingerprint() {
        return Err(stale());
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
    let root = &record.replacement_root;
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
            | pbps_model::ids::IdsError::OrphanConversion { .. }
            | pbps_model::ids::IdsError::InvalidDropRequirement { .. }
            | pbps_model::ids::IdsError::InvalidConversion { .. }
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
        let Some(table) = declared.ids.tables.get(&record.replacement_root).cloned() else {
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
            Stand::Source(_) => {
                if let Err(refused) = qualify(base, declared, &table, dialect) {
                    errors.push(error(table, refused.why));
                }
            }
        }
    }
    // The files no longer record a rewrite this environment already applied
    // (withdrawn after the fact, or an older revision): the replacement it
    // holds would read as a table to drop and a source to create.
    for record in base.ids.rebuilds.values() {
        let root = &record.replacement_root;
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

/// Install the identities the last recorded environment actually holds. Names
/// cannot identify a lineage link: a chain can return to its original shape.
pub fn installation(
    ids: &IdsFile,
    previous: Option<&pbps_model::StateSnapshot>,
) -> Result<IdsFile, RebuildError> {
    let mut installed = ids.clone();
    for (key, record) in &ids.rebuilds {
        let Some(table) = ids.tables.get(&record.replacement_root) else {
            continue;
        };
        let previous = previous.ok_or_else(|| refused(table, "the rebuild lineage has no recorded environment identity; bootstrap a new environment, or restore its ledger before baselining"))?;
        let links = chain(ids, key, record);
        match stand(&previous.ids, &links) {
            Stand::Done => {}
            Stand::Source(start) => {
                installed =
                    restored_chain(&installed, links[start..].iter().map(|(_, record)| *record));
            }
            Stand::Fresh | Stand::Partial => {
                return Err(refused(
                    table,
                    "the recorded environment does not identify one complete rebuild lineage link",
                ));
            }
        }
    }
    installed
        .validate()
        .map_err(|e| refused(&TableName::new("?", "?"), e.to_string()))?;
    Ok(installed)
}

/// Pin the complete copy projection, including each intermediate transformation.
/// A USING expression is vetted against its one input column by the engine.
pub fn copy_columns(
    root: &RebuildPartitionedTable,
    dialect: &(impl pbps_dialect::Dialect + ?Sized),
) -> Result<Vec<pbps_model::CopyColumn>, RebuildError> {
    let lineages = root.composed_columns();
    root.after[&root.table]
        .columns
        .iter()
        .map(|(name, declared)| {
            let target = root.table.column(name);
            let lineage = lineages
                .get(&target)
                .ok_or_else(|| refused(&root.table, "copy column has no lineage"))?;
            let source = if lineage.from_environment {
                let column = &lineage.stages[0].column;
                let spec = root.before[&column.table]
                    .columns
                    .get(&column.name)
                    .ok_or_else(|| refused(&root.table, "copy source column is absent"))?;
                Some(pbps_model::CopySource {
                    column: column.clone(),
                    ty: spec.ty.clone(),
                    collation: spec.collation.clone(),
                })
            } else {
                None
            };
            let mut previous = source
                .as_ref()
                .map(|s| s.ty.clone());
            let mut stages = Vec::new();
            for stage in lineage
                .stages
                .iter()
                .skip(usize::from(lineage.from_environment))
            {
                let shape = stage.shape.as_ref().ok_or_else(|| {
                    refused(
                        &root.table,
                        "an intermediate copy type is missing; re-approve the lineage",
                    )
                })?;
                let link = root.links().find(|link| link.replacement.columns.contains_key(&stage.uid)).unwrap();
                let input = link.continues.get(&stage.uid).and_then(|uid| link.source.columns.get(uid)).map_or_else(|| stage.column.name.clone(), |c| c.name.clone());
                let conversion = link.conversions.get(&stage.uid).filter(|c| {
                    c.to == shape.ty.raw().to_string() && previous.as_ref().is_some_and(|p| c.from == p.raw().to_string() || *p != shape.ty)
                });
                if let (Some(c), Some(ty)) = (conversion, &previous)
                    && c.from != ty.raw().to_string() {
                    let from = c.from.parse::<pbps_model::ColumnType>().map_err(|e| refused(&root.table, e.to_string()))?;
                    if dialect.type_change_needs_using(ty, &from) { return Err(refused(&root.table, "an ordinary edit between rebuild approvals needs an unrecorded conversion")); }
                    stages.push(pbps_model::CopyStage { input: input.clone(), cast: from, collation: None, using: None });
                }
                let using = conversion.map(|c| c.using.clone());
                if let Some(from) = &previous
                    && using.is_none()
                    && dialect.type_change_needs_using(from, &shape.ty)
                {
                    return Err(refused(
                        &root.table,
                        format!(
                            "{} needs a recorded conversion from `{}` to `{}`",
                            stage.column.shown(),
                            from.shown(),
                            shape.ty.shown()
                        ),
                    ));
                }
                stages.push(pbps_model::CopyStage {
                    input,
                    cast: shape.ty.clone(),
                    collation: shape.collation.clone(),
                    using,
                });
                previous = Some(shape.ty.clone());
            }
            if stages.len() == 1 && stages[0].using.is_none() {
                stages.clear();
            }
            Ok(pbps_model::CopyColumn {
                source,
                target,
                cast: declared.ty.clone(),
                collation: declared.collation.clone(),
                stages,
            })
        })
        .collect()
}

/// Recheck exactly the locked tree with the approval's canonical type rules.
pub fn same_tree(
    before: &Schema,
    observed: &Schema,
    root: &TableName,
    dialect: &(impl pbps_dialect::Dialect + ?Sized),
) -> bool {
    tree(&canonical_schema(before, dialect).0, root)
        == tree(&canonical_schema(observed, dialect).0, root)
}
