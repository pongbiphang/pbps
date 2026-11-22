//! The facts the sort reads, each computed once from the plan, and the sort key and class built on them.

use super::super::*;

/// The parents this plan attaches partitions to, and the rank of the tables that
/// receive rows or attaches: `late_attach_ranks` reads both, `dependency_rank`
/// the rank.
fn attach_parents_and_data_rank(
    planned: &[PlannedChange],
    declared: Side<'_>,
) -> (BTreeMap<TableName, TableName>, BTreeMap<TableName, usize>) {
    // Rows follow the foreign keys among the tables that declare them. The
    // declared side is the right one to read: a row being inserted is going
    // into the schema as it will be, not as it was.
    //
    // An attach brings rows into its parent and validates the parent's
    // foreign keys over them, so the parents this plan attaches to are
    // ordered with the tables that receive rows: a parent after every table
    // it references, whether rows or another attach fill that one (#1642
    // review).
    let attach_parents: BTreeMap<TableName, TableName> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Partition(PartitionChange::AttachPartition { table, parent, .. }) =
                &p.change
            {
                Some((table.clone(), parent.clone()))
            } else {
                None
            }
        })
        .collect();
    let data_rank = rank_of_tables(&if attach_parents.is_empty() {
        pbps_model::data::insertion_order(declared.schema)
    } else {
        pbps_model::data::supply_order(
            declared.schema,
            declared
                .schema
                .tables
                .iter()
                .filter(|(_, t)| t.data.is_some())
                .map(|(n, _)| n.clone())
                .chain(attach_parents.values().cloned())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        )
    });
    (attach_parents, data_rank)
}

/// The tables a `RenameColumn` of this plan claims a column name on:
/// `frees_a_renamed_column` reads it to find the drop that frees a name.
fn column_name_claims(planned: &[PlannedChange]) -> BTreeSet<TableName> {
    // The tables a `RenameColumn` in this plan claims a column name on. Both
    // this and the drops below carry `declared_table_name` (`diff_columns`),
    // so the two spellings meet.
    //
    // The *table*, and not the name, because the name is a question this
    // cannot answer. On SQL Server what makes two spellings one column name is
    // the database's collation, which a plan computed offline does not have
    // (SPEC 7.3) — and `Mssql::fold_ident` returns the identity for exactly
    // that reason. **Measured** on the pinned image, each in a database of the
    // named collation:
    //
    //     SQL_Latin1_General_CP1_CI_AS   note  vs Note  ->  one name, Msg 15335
    //     SQL_Latin1_General_CP1_CI_AI   café  vs cafe  ->  one name, Msg 15335
    //     Latin1_General_CS_AS           note  vs Note  ->  two names, both kept
    //
    // Case, then accents; width and kana sensitivity are two more flags on the
    // same collation name, and a binary collation is another answer again.
    // Folding the name here would be guessing at which of those the target
    // database chose, and each guess that comes up short refuses a valid plan
    // at the engine — the defect this is here to fix, one collation further
    // out. So the question asked is the one that is true under every
    // collation: a rename can only collide with a column of its own table.
    //
    // The cost is that a drop of a column nothing claims moves too, and
    // nothing between its old class and its new one can notice: a `Revoke`
    // names no column, and a column this plan drops is never a rename's
    // source, since the two come from different uids and no two baseline
    // columns share a name.
    let tables_claiming_a_column_name: BTreeSet<TableName> = planned
        .iter()
        .filter_map(|p| match &p.change {
            Change::Partition(PartitionChange::RebuildPartitionedTable(_))
            | Change::Index(IndexChange::BuildPartitionedIndex(_))
            | Change::Partition(PartitionChange::CopyTableRows(_))
            | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => None,
            Change::Column(ColumnChange::RenameColumn { table, .. }) => Some(table.clone()),
            Change::Table(_)
            | Change::Partition(PartitionChange::DetachPartition { .. })
            | Change::Partition(PartitionChange::AttachPartition { .. })
            | Change::Column(ColumnChange::AddColumn { .. })
            | Change::Column(ColumnChange::DropColumn { .. })
            | Change::Column(ColumnChange::AlterColumnType { .. })
            | Change::Column(ColumnChange::AlterColumnNullability { .. })
            | Change::Column(ColumnChange::AlterColumnDefault { .. })
            | Change::Column(ColumnChange::AlterColumnExpression { .. })
            | Change::Column(ColumnChange::AlterColumnNumbering { .. })
            | Change::Column(ColumnChange::SetColumnDeprecated { .. })
            | Change::TableProperty(_)
            | Change::Constraint(_)
            | Change::Index(IndexChange::SetIndexStorageParameters { .. })
            | Change::Partition(PartitionChange::SetPartitionDefault { .. })
            | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
            | Change::Policy(_)
            | Change::Index(IndexChange::AddIndex { .. })
            | Change::Column(ColumnChange::AddComputedColumn { .. })
            | Change::Column(ColumnChange::DropComputedColumn { .. })
            | Change::Index(IndexChange::DropIndex { .. })
            | Change::Row(_)
            | Change::Module(_)
            | Change::TypeValue(_)
            | Change::Role(_)
            | Change::Type(_)
            | Change::Search(_)
            | Change::Extension(_)
            | Change::Permission(_) => None,
        })
        .collect();
    tables_claiming_a_column_name
}

/// How many renames each `RenameColumn` waits on, which ranks it within its class.
/// It pushes the refusals for cycles of renames into `errs` in place: the
/// order of `errors` is part of the result.
fn rename_chain_depths(
    planned: &[PlannedChange],
    errs: &mut Vec<DiffError>,
) -> BTreeMap<Rename, usize> {
    // A column rename into a name another rename of the same table vacates
    // runs after it: skipped revisions that rename `b` to `c` and then `a` to
    // `b` leave a chain the uids would otherwise order, and `a -> b` while `b`
    // still stands is refused by both engines (DEC-541.1). Its depth is how many
    // links it waits on; the chain's far end, whose target is free, goes
    // first. A cycle, two columns trading names, has no order an engine takes
    // without a temporary name, so the plan says so instead of emitting one.
    let vacates: BTreeMap<(TableName, ColumnName), ColumnName> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Column(ColumnChange::RenameColumn {
                table, from, to, ..
            }) = &p.change
            {
                Some(((table.clone(), from.clone()), to.clone()))
            } else {
                None
            }
        })
        .collect();
    let mut cycles: BTreeSet<(TableName, BTreeSet<ColumnName>)> = BTreeSet::new();
    for ((table, from), to) in &vacates {
        let mut path = vec![from.clone()];
        let mut next = to.clone();
        while let Some(after) = vacates.get(&(table.clone(), next.clone())) {
            // Only the links that close the loop: a chain can lead into one.
            if let Some(start) = path.iter().position(|seen| *seen == next) {
                cycles.insert((table.clone(), path[start..].iter().cloned().collect()));
                break;
            }
            path.push(next.clone());
            next = after.clone();
        }
    }
    // What each rename waits on: the rename vacating its target. Under a
    // case-insensitive collation `a -> B` also waits on `b -> c`, since the
    // engine reads `B` as held by `b` (DEC-981.3). Such a link is added after
    // every exact one and only where it closes no cycle: on a case-sensitive
    // database `a -> B` beside `b -> A` is a valid pair, not a swap, and a
    // folded link must not hide an exact one it would loop through.
    let mut waits: BTreeMap<&Rename, BTreeSet<&Rename>> = vacates
        .iter()
        .map(|(rename, to)| {
            let exact = vacates.get_key_value(&(rename.0.clone(), to.clone()));
            (rename, exact.map(|(k, _)| k).into_iter().collect())
        })
        .collect();
    for (rename, to) in &vacates {
        let folded = crate::order::rename_order::case_folded(to.raw());
        for vacating in vacates.keys() {
            if vacating != rename
                && vacating.0 == rename.0
                && vacating.1 != *to
                && crate::order::rename_order::case_folded(vacating.1.raw()) == folded
                && !reaches(&waits, vacating, rename)
            {
                waits
                    .get_mut(rename)
                    .expect("every rename has an entry")
                    .insert(vacating);
            }
        }
    }
    // Its depth is the longest run of renames it waits on; the chain's far
    // end, whose target is free, goes first. A rename on an exact cycle is
    // refused below, so its depth is only cut short, never relied on.
    let mut memo = BTreeMap::new();
    let chain_depth: BTreeMap<Rename, usize> = vacates
        .keys()
        .map(|rename| {
            (
                rename.clone(),
                depth(&waits, rename, &mut BTreeSet::new(), &mut memo),
            )
        })
        .collect();
    for (table, columns) in cycles {
        errs.push(DiffError::ColumnRenameCycle { table, columns });
    }
    chain_depth
}

type Rename = (TableName, ColumnName);

fn reaches<'a>(
    waits: &BTreeMap<&'a Rename, BTreeSet<&'a Rename>>,
    from: &'a Rename,
    to: &Rename,
) -> bool {
    let mut seen = BTreeSet::new();
    let mut stack = vec![from];
    while let Some(at) = stack.pop() {
        if at == to {
            return true;
        }
        if seen.insert(at) {
            stack.extend(waits.get(at).into_iter().flatten().copied());
        }
    }
    false
}

fn depth<'a>(
    waits: &BTreeMap<&'a Rename, BTreeSet<&'a Rename>>,
    at: &'a Rename,
    open: &mut BTreeSet<&'a Rename>,
    memo: &mut BTreeMap<&'a Rename, usize>,
) -> usize {
    if let Some(&d) = memo.get(at) {
        return d;
    }
    if !open.insert(at) {
        return 0;
    }
    let d = waits
        .get(at)
        .into_iter()
        .flatten()
        .map(|&next| 1 + depth(waits, next, open, memo))
        .max()
        .unwrap_or(0);
    open.remove(at);
    memo.insert(at, d);
    d
}

/// The uids of the base's generated columns, whose drops `sort_class` puts first
/// in their class.
fn generated_columns_in_base(base: Side<'_>, index: &Indexes<'_>) -> BTreeSet<pbps_model::Uid> {
    // The base's generated columns, whose drops go first in their class: an
    // input column cannot be dropped while a generated column still reads it
    // (DEC-1168.1). By uid, not by name: a `DropColumn` names the column
    // under the declared table name, which a rename in the same plan makes
    // differ from the base's.
    let generated_in_base: BTreeSet<pbps_model::Uid> = base
        .schema
        .tables
        .iter()
        .flat_map(|(name, table)| {
            table
                .columns
                .iter()
                .filter(|(_, c)| match &c.value_source {
                    ValueSource::Generated(_) => true,
                    ValueSource::None
                    | ValueSource::Default(_)
                    | ValueSource::Identity(_)
                    | ValueSource::Sequence(_) => false,
                })
                .filter_map(|(column, _)| index.base.column_uid(&name.column(column)).cloned())
        })
        .collect();
    generated_in_base
}

/// The tables this plan writes rows into or deletes rows from: `late_attach` and
/// `sort_class` read it.
fn row_change_tables(planned: &[PlannedChange]) -> BTreeSet<TableName> {
    let row_tables: BTreeSet<TableName> = planned
        .iter()
        .filter_map(|p| row_table(&p.change).cloned())
        .collect();
    row_tables
}

/// The columns this plan tightens or retypes: `sequence_release_ranks` reads them.
fn columns_held_back(planned: &[PlannedChange]) -> BTreeSet<ColumnRef> {
    // A column this plan tightens or retypes: a conversion to an identity
    // waits for both, since the identity is made `NOT NULL`-only and in the
    // column's type at the time (#1930 review).
    let held_back: BTreeSet<ColumnRef> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Column(ColumnChange::AlterColumnNullability {
                column,
                to_nullable: false,
                ..
            })
            | Change::Column(ColumnChange::AlterColumnType { column, .. }) = &p.change
            {
                Some(column.clone())
            } else {
                None
            }
        })
        .collect();
    held_back
}

/// The identity columns whose sequence bounds are retuned before their retype:
/// `sort_class` puts their numbering change first in its class.
fn columns_whose_bounds_go_first(
    planned: &[PlannedChange],
    dialect: &(impl Dialect + ?Sized),
) -> BTreeSet<ColumnRef> {
    // A kept identity on a column this plan narrows: its sequence is
    // retuned before the retype (#2013 review, DEC-1921.1). The engine moves
    // an identity's sequence with its column, and measured on 16 and 18 it
    // refuses the retype while a bound set for the wider type stands. An
    // identity's column is an integer, so a retype that is not safe narrows
    // its range, and bounds declared for the narrower type fit the wider one
    // too; a widening keeps the retype first, where the old bounds fit.
    let bounds_first: BTreeSet<ColumnRef> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Column(ColumnChange::AlterColumnType {
                column, from, to, ..
            }) = &p.change
                && p.change.value_change().map_or_else(
                    || dialect.type_change_risk(from, to),
                    |values| dialect.value_change(&values, "").risk,
                ) != pbps_dialect::TypeChangeRisk::Safe
            {
                Some(column)
            } else {
                None
            }
        })
        .filter(|column| {
            planned.iter().any(|p| {
                matches!(
                    &p.change,
                    Change::Column(ColumnChange::AlterColumnNumbering {
                        column: c,
                        from: ValueSource::Identity(_),
                        to: ValueSource::Identity(_),
                        ..
                    }) if c == *column
                )
            })
        })
        .cloned()
        .collect();
    bounds_first
}

/// How deep each owned-sequence release sits in its chain of renames, which
/// `sort_class` ranks it by. It pushes the refusals for cycles into `errs` in
/// place: the order of `errors` is part of the result.
#[allow(clippy::needless_borrow)] // The block moved as it was; its borrows were of an owned local.
fn sequence_release_ranks(
    planned: &[PlannedChange],
    held_back: &BTreeSet<ColumnRef>,
    errs: &mut Vec<DiffError>,
) -> BTreeMap<ColumnRef, usize> {
    let (early_release, sequence_cycles) =
        sequence_release_depths(planned.iter().map(|p| &p.change), &held_back);
    for renames in sequence_cycles {
        errs.push(DiffError::SequenceRenameCycle { renames });
    }
    early_release
}

/// The attached partitions that wait among the row changes, with the rank of
/// their parent: `sort_class` and the sort key read it.
fn late_attach_ranks(
    declared: Side<'_>,
    attach_parents: &BTreeMap<TableName, TableName>,
    row_tables: &BTreeSet<TableName>,
    data_rank: &BTreeMap<TableName, usize>,
) -> BTreeMap<TableName, isize> {
    // An attach validates its parent's foreign keys over the rows it brings,
    // as `ADD FOREIGN KEY` does, so one whose parent references a table this
    // plan writes rows into, or attaches a table to, waits for them: among
    // the row changes, at its parent's rank, after every table the parent
    // references and before the rows of a table that references the parent.
    // Its own changes follow it there. Any other attach stays in class 7
    // (#1642 review).
    let late_attach: BTreeMap<TableName, isize> = attach_parents
        .iter()
        .filter(|(_, parent)| {
            declared.schema.tables.get(*parent).is_some_and(|t| {
                t.foreign_keys.values().any(|fk| {
                    fk.references_table != **parent
                        && (row_tables.contains(&fk.references_table)
                            || attach_parents.values().any(|p| *p == fk.references_table))
                })
            })
        })
        .map(|(table, parent)| {
            (
                table.clone(),
                data_rank.get(parent).map_or(0, |r| *r as isize),
            )
        })
        .collect();
    late_attach
}

/// The foreign-key graph a persistence switch is ordered by in `sort_class`, none
/// when the plan has no switch, and the depth of its deepest table.
fn persistence_fk_graph<'a>(
    planned: &[PlannedChange],
    declared: Side<'a>,
) -> (Option<ForeignKeyGraph<'a>>, usize) {
    // How deep each declared table sits in the foreign-key graph: 0 for one
    // that references no other, else one more than the deepest it does.
    // Switched to logged, the referenced table goes first, so by depth;
    // switched to unlogged, the referencing one, so by depth reversed: the
    // engine refuses either the other way round (measured on 16 and 18,
    // #1443). Self-references do not count, nor the keys inside a cycle,
    // which are dropped around the switches (#1488 review).
    // Only a persistence switch reads it, so a plan without one does not
    // build it (#1792).
    let fk_graph = planned
        .iter()
        .any(|p| {
            matches!(
                p.change,
                Change::TableProperty(TablePropertyChange::SetTablePersistence { .. })
            )
        })
        .then(|| ForeignKeyGraph::of(declared.schema));
    let deepest = fk_graph.as_ref().map_or(0, ForeignKeyGraph::deepest);
    (fk_graph, deepest)
}

/// The columns of a parent whose default, numbering source or NULL-ness this plan
/// changes: `follows_its_parent` reads it.
fn parent_columns_moved(planned: &[PlannedChange]) -> BTreeSet<(TableName, ColumnName)> {
    // A parent's column default, numbering source or NULL-ness this plan
    // changes, which the engine recurses into every partition: its own on that
    // column is set after it, or the parent's would overwrite it (#1687).
    let parent_moved: BTreeSet<(TableName, ColumnName)> = planned
        .iter()
        .filter_map(|p| {
            // A retype carries its NOT NULL change with it (#1692 review).
            if let Change::Column(ColumnChange::AlterColumnDefault { column, .. })
            | Change::Column(ColumnChange::AlterColumnNumbering { column, .. })
            | Change::Column(ColumnChange::AlterColumnNullability { column, .. }) = &p.change
            {
                Some((column.table.clone(), column.name.clone()))
            } else if let Change::Column(ColumnChange::AlterColumnType {
                column,
                from_nullable,
                to_nullable,
                ..
            }) = &p.change
                && from_nullable != to_nullable
            {
                Some((column.table.clone(), column.name.clone()))
            } else {
                None
            }
        })
        .collect();
    parent_moved
}

/// The partitioned parents whose columns this plan changes: `sort_class` creates
/// their partitions after those changes.
fn parents_with_column_changes(
    planned: &[PlannedChange],
    declared: Side<'_>,
) -> BTreeSet<TableName> {
    // The parents whose columns this plan changes (#1687).
    let parent_columns_change: BTreeSet<TableName> = planned
        .iter()
        .filter(|p| parent_column(&p.change).is_some())
        .filter_map(|p| p.change.table())
        .filter(|t| {
            declared
                .schema
                .tables
                .get(*t)
                .is_some_and(|t| t.partition_by.is_some())
        })
        .cloned()
        .collect();
    parent_columns_change
}

/// The DEFAULT partitions a create of this plan moves rows out of: `sort_class`
/// orders their NOT NULL after those creates.
fn defaults_moved_out_of(planned: &[PlannedChange]) -> BTreeSet<TableName> {
    // The DEFAULT partitions a create in this plan moves rows out of (#1547).
    let moved_out_of: BTreeSet<TableName> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Table(TableChange::CreateTable {
                beside_default: Some(default),
                ..
            }) = &p.change
            {
                Some(default.clone())
            } else {
                None
            }
        })
        .collect();
    moved_out_of
}

/// How deep each type sits in its chain of base domains, in the declarations and
/// in the base: `dependency_rank` reads them. It pushes the refusal for a cycle
/// into `errs` in place: the order of `errors` is part of the result.
fn domain_type_ranks(
    base: Side<'_>,
    declared: Side<'_>,
    errs: &mut Vec<DiffError>,
) -> (
    BTreeMap<pbps_model::TypeRef, isize>,
    BTreeMap<pbps_model::TypeRef, isize>,
) {
    let mut type_rank = BTreeMap::new();
    let mut drop_type_rank = BTreeMap::new();
    for (schema, ranks) in [
        (declared.schema, &mut type_rank),
        (base.schema, &mut drop_type_rank),
    ] {
        for name in schema.types.keys() {
            let mut at = name;
            let mut seen = BTreeSet::new();
            while let Some(pbps_model::ManagedType::Domain { base, .. }) = schema.types.get(at) {
                if !seen.insert(at.clone()) {
                    errs.push(DiffError::TypeLifecycleUnsupported {
                        name: name.clone(),
                        reason: "domain base dependency cycle".into(),
                    });
                    break;
                }
                let Some(next) = base.type_ref() else {
                    break;
                };
                at = next;
            }
            ranks.insert(name.clone(), seen.len() as isize);
        }
    }
    (type_rank, drop_type_rank)
}

/// The rank behind the last package change of the plan: the search ranks start
/// from it, and the sort key adds it to the rank of a created domain.
fn package_floor_of(
    planned: &[PlannedChange],
    extension_rank: &BTreeMap<pbps_model::Uid, isize>,
) -> isize {
    // A domain this plan creates may be built on a type a package of this plan
    // provides, created or moved here (the effects say so: its base needs the
    // package's change). The draft is what orders two changes of one class, and
    // the effects only check it, so the creation ranks behind the last package
    // change instead of tying with it, where the subject decided (#2696 ready
    // round, DEC-2447.12).
    let package_floor: isize = planned
        .iter()
        .filter_map(|p| {
            if let Change::Extension(ExtensionChange::CreateExtension { uid, .. })
            | Change::Extension(ExtensionChange::UpdateExtension { uid, .. }) = &p.change
            {
                Some(extension_rank.get(uid).copied().unwrap_or(0) + 1)
            } else if let Change::Extension(ExtensionChange::SetExtensionSchema { uid, .. }) =
                &p.change
            {
                Some(extension_rank.get(uid).copied().unwrap_or(0) + 2)
            } else {
                None
            }
        })
        .max()
        .unwrap_or(0);
    package_floor
}

fn built_domain(c: &Change) -> Option<&pbps_model::ManagedType> {
    let definition = if let Change::Type(TypeChange::CreateType { definition, .. })
    | Change::TypeValue(TypeValueChange::EnumRebuild {
        step: pbps_model::EnumRebuildStep::Create { definition, .. },
        ..
    }) = c
    {
        definition.as_ref()
    } else {
        return None;
    };
    matches!(definition, pbps_model::ManagedType::Domain { .. }).then_some(definition)
}

/// The search objects this plan creates, and those it alters in place: the search
/// ranks and the domain floor read them.
fn searches_in_the_plan(
    planned: &[PlannedChange],
) -> (
    BTreeSet<&pbps_model::SearchRef>,
    BTreeSet<&pbps_model::SearchRef>,
) {
    // A dictionary is created before the configuration that maps it, and the configuration
    // dropped before the dictionary (DEC-2728.2): the depth of the references among the
    // objects the plan holds on each side. An object created here that maps a dictionary a
    // package may provide starts behind the packages of this plan (the effects:
    // `package_input`), and what maps it sits one rank behind that, so the floor is composed
    // into the depth, not added to the sort key afterwards.
    let created_search: BTreeSet<&pbps_model::SearchRef> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Search(SearchChange::CreateSearch { name, .. }) = &p.change {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    // One altered in place maps what it now maps, so it waits for the packages as a create does.
    let altered_search: BTreeSet<&pbps_model::SearchRef> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Search(
                SearchChange::AlterSearchMapping { name, .. }
                | SearchChange::AlterSearchDictionary { name, .. },
            ) = &p.change
            {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    (created_search, altered_search)
}

/// The depth of each search object among the references on each side, for the
/// declarations and for the base: the sort key ranks the search changes by them.
fn search_ranks(
    base: Side<'_>,
    declared: Side<'_>,
    package_floor: isize,
    created_search: &BTreeSet<&pbps_model::SearchRef>,
    altered_search: &BTreeSet<&pbps_model::SearchRef>,
) -> (
    BTreeMap<pbps_model::SearchRef, isize>,
    BTreeMap<pbps_model::SearchRef, isize>,
) {
    let search_depths = |schema: &Schema, floor: isize| -> BTreeMap<pbps_model::SearchRef, isize> {
        let start = |name: &pbps_model::SearchRef, object: &pbps_model::SearchObject| {
            if floor > 0
                && (created_search.contains(name) || altered_search.contains(name))
                && object.dictionaries().iter().any(|d| {
                    d.name.schema() != "pg_catalog" && declared.ids.search_uid(d).is_none()
                })
            {
                floor
            } else {
                0
            }
        };
        schema
            .search
            .keys()
            .map(|name| {
                (
                    name.clone(),
                    search_depth(schema, name, &start, &mut BTreeSet::new()),
                )
            })
            .collect()
    };
    let (search_rank, drop_search_rank) = (
        search_depths(declared.schema, package_floor),
        search_depths(base.schema, 0),
    );
    (search_rank, drop_search_rank)
}

/// How far each renamed search object's new name sits down a chain of renames of this plan
/// (`a` to `b`, `b` to `c`): the rename out of a name runs before the one into it, since the
/// engine refuses `RENAME TO` onto a name that is taken (DEC-2728.5). A rename's target is
/// never a created or altered name of the plan, so the sort key reads this where it reads the
/// depth of those, and a cycle (refused where the renames are planned) ends at the count.
fn search_rename_depths(planned: &[PlannedChange]) -> BTreeMap<pbps_model::SearchRef, isize> {
    let moves: BTreeMap<&pbps_model::SearchRef, &pbps_model::SearchRef> = planned
        .iter()
        .filter_map(|p| {
            if let Change::Search(SearchChange::RenameSearch { from, to, .. }) = &p.change {
                Some((from, to))
            } else {
                None
            }
        })
        .collect();
    moves
        .values()
        .map(|to| {
            let (mut depth, mut at) = (0, *to);
            while let Some(next) = moves.get(at).filter(|_| depth < moves.len() as isize) {
                depth += 1;
                at = next;
            }
            ((*to).clone(), depth)
        })
        .collect()
}

/// The rank the created domains that name a created search object sort behind, 0
/// for a plan whose domains name none: the sort key adds it to their rank.
#[allow(clippy::let_and_return)] // The block moved as it was; it ends in the binding it returns.
fn domain_floor_behind_searches(
    planned: &[PlannedChange],
    dialect: &(impl Dialect + ?Sized),
    created_search: &BTreeSet<&pbps_model::SearchRef>,
    search_rank: &BTreeMap<pbps_model::SearchRef, isize>,
) -> isize {
    // A domain's check or default may name a configuration this plan creates by a literal
    // (`'app.docs'::regconfig`), which the engine resolves when the domain is created: the
    // same literal scan the scratch order uses (`may_call`). The two share a class, so the
    // domains rank behind every created search object, all of them together, which keeps the
    // order among them (a chain of domains is ranked by its bases). A plan whose domains name
    // none leaves them where they were.
    let names_a_search = planned
        .iter()
        .filter_map(|p| built_domain(&p.change))
        .any(|domain| {
            let pbps_model::ManagedType::Domain {
                default, checks, ..
            } = domain
            else {
                return false;
            };
            created_search.iter().any(|created| {
                default
                    .iter()
                    .chain(checks.values())
                    .any(|text| dialect.generation_may_call(text, created.name.name()))
            })
        });
    let domain_floor = if names_a_search {
        1 + created_search
            .iter()
            .filter_map(|created| search_rank.get(*created))
            .max()
            .copied()
            .unwrap_or(0)
    } else {
        0
    };
    domain_floor
}

/// What the sort of `diff_partial_rebuilding` reads, built once by `sort_facts` before
/// the sort takes the plan mutably. Every field is owned: a fact that borrows
/// `planned` is only an input to another fact and cannot be kept here.
pub(in super::super) struct SortFacts<'a> {
    /// The tables a `RenameColumn` of this plan claims a column name on: `frees_a_renamed_column` reads it.
    tables_claiming_a_column_name: BTreeSet<TableName>,
    /// The uids of the base's generated columns: `class` puts their drops first in their class.
    generated_in_base: BTreeSet<pbps_model::Uid>,
    /// The tables this plan writes rows into or deletes rows from: `class` orders the tightening and the numbering of their columns after the rows.
    row_tables: BTreeSet<TableName>,
    /// The columns this plan puts an identity on: `class` leaves the tightening of one ahead of the inserts of a table that deletes no rows.
    identified: BTreeSet<ColumnRef>,
    /// The tables this plan inserts rows into: `class` reads it with `late`.
    inserted: BTreeSet<TableName>,
    /// The tables a plan puts a numbering source on and both deletes and inserts rows of: `class` orders their inserts after the source.
    late: BTreeSet<TableName>,
    /// The columns this plan puts a numbering source on: `class` orders them after the deletes.
    installed: BTreeSet<ColumnRef>,
    /// The identity columns whose sequence bounds are retuned before their retype: `class` puts their numbering change first.
    bounds_first: BTreeSet<ColumnRef>,
    /// How deep each owned-sequence release sits in its chain of renames: `class` ranks it by that.
    early_release: BTreeMap<ColumnRef, usize>,
    /// The attached partitions that wait among the row changes, with the rank of their parent: `class` and `key` read it.
    late_attach: BTreeMap<TableName, isize>,
    /// The foreign-key graph a persistence switch is ordered by, none without a switch: `class` reads it.
    fk_graph: Option<ForeignKeyGraph<'a>>,
    /// The depth of the deepest table of `fk_graph`: `class` reads it for a switch to unlogged.
    deepest: usize,
    /// The columns of a parent whose default, numbering source or NULL-ness this plan changes: `follows_its_parent` reads it.
    parent_moved: BTreeSet<(TableName, ColumnName)>,
    /// The partitioned parents whose columns this plan changes: `class` creates their partitions after those changes.
    parent_columns_change: BTreeSet<TableName>,
    /// The DEFAULT partitions a create of this plan moves rows out of: `class` orders their NOT NULL after those creates.
    moved_out_of: BTreeSet<TableName>,
    /// The replica identities whose target stands before the plan: `class` sets them first.
    early_identity: BTreeSet<(Uid, Option<ReplicaIdentity>)>,
    /// How many renames each `RenameColumn` waits on: `class` ranks it by that.
    chain_depth: BTreeMap<Rename, usize>,
    /// The declarations: `follows_its_parent`, `class` and `key` read the partitioning and the column order of their tables.
    declared: Side<'a>,
    /// The creation rank of each module among the modules: `key` ranks the module creates by it.
    create_rank: BTreeMap<ModuleId, usize>,
    /// The drop rank of each module among the modules: `key` ranks the module drops by it.
    drop_rank: BTreeMap<ModuleId, usize>,
    /// The rank of the tables that receive rows or attaches: `key` ranks their changes by it.
    data_rank: BTreeMap<TableName, usize>,
    /// How deep each role sits among the roles dropped together: `key` drops a parent before its members.
    role_rank: BTreeMap<pbps_model::RoleName, usize>,
    /// How deep each type sits in its chain of base domains, in the declarations: `key` ranks the type changes by it.
    type_rank: BTreeMap<pbps_model::TypeRef, isize>,
    /// How deep each type sits in its chain of base domains, in the base: `key` ranks the type drops by it.
    drop_type_rank: BTreeMap<pbps_model::TypeRef, isize>,
    /// The depth of each package change among the packages it requires: `key` ranks the extension changes by it.
    extension_rank: BTreeMap<pbps_model::Uid, isize>,
    /// The depth of each search object among the references in the declarations: `key` ranks the search changes by it.
    search_rank: BTreeMap<pbps_model::SearchRef, isize>,
    /// The depth of each search object among the references in the base: `key` ranks the search drops by it.
    drop_search_rank: BTreeMap<pbps_model::SearchRef, isize>,
    /// The rank behind the last package change of the plan: `key` adds it to the rank of a created domain.
    package_floor: isize,
    /// The rank the created domains that name a created search object sort behind: `key` adds it to their rank.
    domain_floor: isize,
}

impl SortFacts<'_> {
    /// Whether `c` is the drop that frees a column name a `RenameColumn` of this plan claims:
    /// `class` asks it of every change.
    // Whether this drop can be what frees a name a `RenameColumn` above is
    // waiting for. A column drop and nothing else: no other change gives up a
    // column name.
    fn frees_a_renamed_column(&self, c: &Change) -> bool {
        match c {
            Change::Partition(PartitionChange::RebuildPartitionedTable(_))
            | Change::Index(IndexChange::BuildPartitionedIndex(_))
            | Change::Partition(PartitionChange::CopyTableRows(_))
            | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => false,
            Change::Column(ColumnChange::DropColumn { column, .. }) => {
                self.tables_claiming_a_column_name.contains(&column.table)
            }
            Change::Table(_)
            | Change::Partition(PartitionChange::DetachPartition { .. })
            | Change::Partition(PartitionChange::AttachPartition { .. })
            | Change::Column(ColumnChange::AddColumn { .. })
            | Change::Column(ColumnChange::RenameColumn { .. })
            | Change::Column(ColumnChange::AlterColumnType { .. })
            | Change::Column(ColumnChange::AlterColumnNullability { .. })
            | Change::Column(ColumnChange::AlterColumnDefault { .. })
            | Change::Column(ColumnChange::AlterColumnExpression { .. })
            | Change::Column(ColumnChange::AlterColumnNumbering { .. })
            | Change::Column(ColumnChange::SetColumnDeprecated { .. })
            | Change::TableProperty(_)
            | Change::Constraint(_)
            | Change::Index(IndexChange::SetIndexStorageParameters { .. })
            | Change::Partition(PartitionChange::SetPartitionDefault { .. })
            | Change::Partition(PartitionChange::SetPartitionNotNull { .. })
            | Change::Policy(_)
            | Change::Index(IndexChange::AddIndex { .. })
            | Change::Column(ColumnChange::AddComputedColumn { .. })
            | Change::Column(ColumnChange::DropComputedColumn { .. })
            | Change::Index(IndexChange::DropIndex { .. })
            | Change::Row(_)
            | Change::Module(_)
            | Change::TypeValue(_)
            | Change::Role(_)
            | Change::Type(_)
            | Change::Search(_)
            | Change::Extension(_)
            | Change::Permission(_) => false,
        }
    }

    /// Whether `c` is a setting on a partition that waits among the row changes: `class` asks it.
    fn follows_late_attach(&self, c: &Change) -> bool {
        if let Change::Partition(PartitionChange::SetPartitionDefault { table, .. })
        | Change::Partition(PartitionChange::SetPartitionNotNull { table, .. })
        | Change::TableProperty(TablePropertyChange::SetTablePersistence { table, .. })
        | Change::TableProperty(TablePropertyChange::SetStorageParameters { table, .. })
        | Change::Index(IndexChange::SetIndexStorageParameters { table, .. }) = c
        {
            self.late_attach.contains_key(table)
        } else {
            false
        }
    }

    /// Whether `c` sets a partition's default or NOT NULL on a column its parent's change moves:
    /// `class` asks it.
    fn follows_its_parent(&self, c: &Change) -> bool {
        let Self {
            declared,
            parent_moved,
            ..
        } = self;
        let (Change::Partition(PartitionChange::SetPartitionDefault { table, column, .. })
        | Change::Partition(PartitionChange::SetPartitionNotNull { table, column, .. })) = c
        else {
            return false;
        };
        declared
            .schema
            .tables
            .get(table)
            .and_then(|t| t.partition_of.as_ref())
            .is_some_and(|of| parent_moved.contains(&(of.parent.clone(), column.clone())))
    }

    /// The ordering class of `c` and a rank inside it: the first two parts of the sort key.
    // The class, and a rank inside it, so a change can sit between two
    // classes without a new ordinal shifting every one below it — the cost
    // `order_key`'s own doc names.
    fn class(&self, c: &Change) -> (u8, usize) {
        let Self {
            generated_in_base,
            row_tables,
            identified,
            inserted,
            late,
            installed,
            bounds_first,
            early_release,
            late_attach,
            fk_graph,
            deepest,
            parent_columns_change,
            moved_out_of,
            early_identity,
            chain_depth,
            declared,
            ..
        } = self;
        if matches!(
            c,
            Change::Type(TypeChange::CreateType { .. })
                | Change::Search(SearchChange::CreateSearch { .. })
                | Change::Search(SearchChange::AlterSearchMapping { .. })
                | Change::Search(SearchChange::AlterSearchDictionary { .. })
                | Change::TypeValue(TypeValueChange::AddEnumValue { .. })
                | Change::TypeValue(TypeValueChange::RenameEnumValue { .. })
                | Change::TypeValue(TypeValueChange::EnumRebuild {
                    step: pbps_model::EnumRebuildStep::Create { .. },
                    ..
                })
        ) {
            return (7, 0);
        }
        // The columns move and the type is swapped among the type-changing
        // alterations of class 9, ranked with a retype (DEC-1635.7).
        if matches!(
            c,
            Change::TypeValue(TypeValueChange::EnumRebuild {
                step: pbps_model::EnumRebuildStep::Retype { .. }
                    | pbps_model::EnumRebuildStep::Swap { .. },
                ..
            })
        ) {
            return (COLUMN_ALTERATIONS, 1);
        }

        // After every module and its PUBLIC decision, then policies before
        // either flag. Keep the established class ordinals (DEC-1172.1).
        if matches!(c, Change::Policy(PolicyChange::CreatePolicy { .. })) {
            return (14, 2);
        }
        if matches!(c, Change::Policy(PolicyChange::SetRowSecurity { .. })) {
            return (14, 3);
        }
        if let Change::Column(ColumnChange::AlterColumnNumbering { column, .. }) = c
            && bounds_first.contains(column)
        {
            return (COLUMN_ALTERATIONS, 0);
        }
        if let Change::Partition(PartitionChange::AttachPartition { table, .. }) = c
            && late_attach.contains_key(table)
        {
            return (ROW_DELETIONS - 1, 1);
        }
        if self.follows_late_attach(c) {
            return (ROW_DELETIONS - 1, 2);
        }
        // A DEFAULT partition's own NOT NULL, tightened in a plan that moves
        // rows out of it, comes after every create that moves them, whatever
        // class the create lands in (7, or (9, 5) under a parent whose columns
        // change): the rows the move takes may be the ones holding NULL there,
        // and the engine scans what is left. Its own checks and uniques are of
        // class 13, after every create already (#1763 review).
        if let Change::Partition(PartitionChange::SetPartitionNotNull {
            table,
            not_null: true,
            ..
        }) = c
            && moved_out_of.contains(table)
        {
            return (COLUMN_ALTERATIONS, 6);
        }
        if self.follows_its_parent(c) {
            return (COLUMN_ALTERATIONS, 4);
        }
        if let Change::TableProperty(TablePropertyChange::SetTablePersistence {
            table,
            unlogged,
            ..
        }) = c
        {
            let depth = fk_graph.as_ref().map_or(0, |g| g.depth(table));
            return (
                COLUMN_ALTERATIONS,
                10 + if *unlogged { deepest - depth } else { depth },
            );
        }
        // A replica identity whose target stands before the plan is set
        // first, under the table's old name: before the drops of class 2 can
        // take its old index, which would leave the table identifying no row
        // (DEC-1444.1).
        if let Change::TableProperty(TablePropertyChange::SetReplicaIdentity { uid, to, .. }) = c
            && early_identity.contains(&(uid.clone(), to.clone()))
        {
            return (0, 1);
        }
        // A tightening of a table whose rows this plan writes or deletes runs
        // after them, at the end of the deletes' class: the rows may be what
        // fills or removes its NULLs, and no row change needs the column NOT
        // NULL first. Still before the additions of class 13, a primary key
        // among them, which SQL Server refuses over a nullable column (#1367).
        //
        // An identity this plan puts on follows its tightening there, since
        // the engine refuses one on a nullable column: after the deletes,
        // which may remove the rows holding NULL. Any source put on follows
        // them too, since it continues past the values left, and those a
        // delete removes may be the greatest. The table's own inserts follow
        // the source, since a declared row leaves the numbered column out to
        // take its number. Where the plan inserts rows into the table and
        // deletes none, the source, and an identity's tightening, stay in
        // class 9 ahead of the inserts, where a referencing table's rows can
        // still follow them (#1930 review).
        let early =
            |column: &ColumnRef| inserted.contains(&column.table) && !late.contains(&column.table);
        if let Change::Column(ColumnChange::AlterColumnNullability {
            column,
            to_nullable: false,
            ..
        }) = c
            && row_tables.contains(&column.table)
            && !(identified.contains(column) && early(column))
        {
            return (ROW_DELETIONS, 2);
        }
        if let Change::Column(ColumnChange::AlterColumnNumbering { column, .. }) = c
            && installed.contains(column)
            && row_tables.contains(&column.table)
            && !early(column)
        {
            return (ROW_DELETIONS, 3);
        }
        if let Change::Row(RowChange::InsertRow { table, .. }) = c
            && late.contains(table)
        {
            return (ROW_DELETIONS, 4);
        }
        // An owned sequence given up, or renamed, frees its name before the
        // tables and columns this plan creates, one of which may take it:
        // after the drops of class 6, which free names too, and before
        // `CreateTable` (#1930 review). None needs anything class 9 does
        // first: giving one up drops a default and a sequence, a rename keeps
        // the sequence, and a conversion to an identity needs the column
        // `NOT NULL`, which a column this plan does not tighten already is.
        // An identity put on a column retyped after it moves with it.
        //
        // Among them, a rename to a name another of them frees follows it,
        // down the chain (#1930 review).
        if let Change::Column(ColumnChange::AlterColumnNumbering { column, .. }) = c
            && let Some(depth) = early_release.get(column)
        {
            return (6, 1 + depth);
        }
        // A generated column is dropped before the ordinary columns of its
        // class, and added after every column change of the class beyond:
        // the additions, and the in-place alterations of class 9. Measured,
        // the engine refuses to drop a column a generated column reads,
        // refuses a generation expression over a column that is not there yet,
        // and refuses to retype one a generated column reads. A generated
        // column never reads another, and nothing in class 9 needs one that is
        // new, so one layer each way is the whole order (DEC-1168.1).
        let drops_generated = matches!(c, Change::Column(ColumnChange::DropColumn { uid, .. }) if generated_in_base.contains(uid));
        if self.frees_a_renamed_column(c) {
            // The layer holds here too: every drop of a table that renames a
            // column comes to this class, the generated one and its input
            // alike, so the generated one keeps its place ahead.
            if drops_generated {
                return (2, 2);
            }
            // Between the constraint and index drops of class 2 — a column a
            // check or an index names cannot be dropped while they stand —
            // and the `RenameColumn` of class 3 that is waiting for its name.
            //
            // Measured on the pinned images, this is the difference between a
            // reviewed plan and one the engine refuses: SQL Server answers
            // `sp_rename` with `Msg 15335, The new name 'note' is already in
            // use as a COLUMN name and would cause a duplicate that is not
            // permitted`, PostgreSQL with `column "note" of relation "s"
            // already exists` (DECISIONS 474, issue #398).
            //
            // Only the drop that frees a claimed name moves. A rename into a
            // name nothing here gives up is refused by `resolve` as an
            // occupied target and never reaches this sort, which is the
            // property `a_single_revision_cannot_rename_into_an_occupied_baseline_name`
            // holds.
            return (2, 3);
        }
        if drops_generated {
            return (order_key(c), 0);
        }
        if let Change::Column(ColumnChange::AddColumn { column, .. }) = c
            && (match &column.value_source {
                ValueSource::Generated(_) => true,
                ValueSource::None
                | ValueSource::Default(_)
                | ValueSource::Identity(_)
                | ValueSource::Sequence(_) => false,
            })
        {
            return (COLUMN_ALTERATIONS, 2);
        }
        // A computed column comes down after every index and constraint drop
        // of class 2, the ones that free a renamed column's name (2, 3)
        // included, and goes up after every alteration of class 9: it reads
        // the columns, in their final type, and nothing in either class reads
        // it (#1174).
        if matches!(c, Change::Column(ColumnChange::DropComputedColumn { .. })) {
            return (2, 4);
        }
        // A package's own objects are what a column, a domain or a module of
        // the plan may use (DEC-2447.12), and its create, update and move share
        // the type class with them, so they go first inside it: nothing else
        // there is ordered against them, and a tie would be decided by a random
        // uid.
        if matches!(
            c,
            Change::Extension(ExtensionChange::CreateExtension { .. })
                | Change::Extension(ExtensionChange::UpdateExtension { .. })
                | Change::Extension(ExtensionChange::SetExtensionSchema { .. })
        ) {
            return (order_key(c), 0);
        }
        if matches!(c, Change::Column(ColumnChange::AddComputedColumn { .. })) {
            return (COLUMN_ALTERATIONS, 3);
        }
        // A detach comes after every table drop of its class: it claims the
        // declared names of what it keeps, and a dropped table's index or
        // key may be what holds one now. A drop claims no name, so nothing
        // waits the other way (#1544).
        if matches!(
            c,
            Change::Partition(PartitionChange::DetachPartition { .. })
        ) {
            return (order_key(c), 2);
        }
        // A partition is created after every parent, whose columns, keys and
        // indexes the engine gives it as it is made (#1170). Under a parent
        // whose columns this plan changes, after those changes too: its own
        // default may name a column the parent adds, and a parent's default
        // set or dropped afterwards would overwrite its own (#1692 review).
        if let Change::Table(TableChange::CreateTable { table, .. }) = c
            && let Some(of) = &table.partition_of
        {
            if parent_columns_change.contains(&of.parent) {
                return (COLUMN_ALTERATIONS, 5);
            }
            return (order_key(c), 2);
        }
        // A partitioned parent's index first in its class, before any
        // partition's own: built after one, it would take it as its clone
        // (#1737 review, DEC-1688.1). First rather than the partition's own
        // last, which would put it after a replica identity naming it. A
        // parent's index reads only its columns, all of earlier classes.
        if let Change::Index(IndexChange::AddIndex { table, .. }) = c
            && declared
                .schema
                .tables
                .get(table)
                .is_some_and(|t| t.partition_by.is_some())
        {
            return (order_key(c), 0);
        }
        if let Change::Column(ColumnChange::RenameColumn { table, from, .. }) = c {
            let depth = chain_depth
                .get(&(table.clone(), from.clone()))
                .copied()
                .unwrap_or(0);
            (order_key(c), 1 + depth)
        } else {
            (order_key(c), 1)
        }
    }

    /// The sort key of `p`: its class, a rank among the dependencies of its kind, then the
    /// tiebreakers. `sort_draft` calls it once per change.
    // The tiebreaker within an ordering class is the table name, then the
    // change's rendering. Debug output alone would sort by uid, which is random
    // at mint time — the plan would be correct but differently ordered per
    // project, and a reviewer diffing two plan.sql files would see noise.
    //
    // `subject()` does not speak for one side consistently: `RenameTable` and
    // `RenameRole` answer with the name the object is losing, everything else
    // with the name it will have. Two changes to one object are therefore
    // sorted against two different spellings of it, and the alphabet — not the
    // dependency — decides which wins. Anything with a real order between them
    // belongs in ordering classes or explicit dependency edges; this
    // tiebreaker cannot express it.
    pub(in super::super) fn key(
        &self,
        p: &PlannedChange,
    ) -> (u8, usize, isize, String, usize, String) {
        let (class, within) = self.class(&p.change);
        (
            class,
            within,
            if let Change::Partition(PartitionChange::AttachPartition { table, .. }) = &p.change
                && let Some(rank) = self.late_attach.get(table)
            {
                *rank
            } else if let Change::Extension(ExtensionChange::CreateExtension { uid, .. })
            | Change::Extension(ExtensionChange::DropExtension { uid, .. })
            | Change::Extension(ExtensionChange::UpdateExtension { uid, .. }) = &p.change
            {
                // Behind the packages of this plan it requires, or that require
                // it for a drop (DEC-2447.6, DEC-2447.8).
                self.extension_rank.get(uid).copied().unwrap_or(0)
            } else if let Change::Search(
                SearchChange::CreateSearch { name, .. }
                | SearchChange::AlterSearchMapping { name, .. }
                | SearchChange::AlterSearchDictionary { name, .. }
                | SearchChange::RenameSearch { to: name, .. },
            ) = &p.change
            {
                // Behind the packages and the dictionaries of this plan it maps, and a
                // rename behind the rename out of the name it takes.
                self.search_rank.get(name).copied().unwrap_or(0)
            } else if let Change::Search(SearchChange::DropSearch { name, .. }) = &p.change {
                -self.drop_search_rank.get(name).copied().unwrap_or(0)
            } else if let Change::Extension(ExtensionChange::SetExtensionSchema { uid, .. }) =
                &p.change
            {
                // One rank behind the package's own update, whose result the
                // move starts from: the two rewrite one package, and the draft
                // is what orders two writes (DEC-2447.11).
                self.extension_rank.get(uid).copied().unwrap_or(0) + 1
            } else {
                dependency_rank(
                    &p.change,
                    &self.create_rank,
                    &self.drop_rank,
                    &self.data_rank,
                    &self.role_rank,
                    &self.type_rank,
                    &self.drop_type_rank,
                ) + if built_domain(&p.change).is_some() {
                    self.package_floor.max(self.domain_floor)
                } else {
                    0
                }
            },
            rebuild_subject(&p.change),
            // Columns added to one table in the order they are declared:
            // the engine appends each, so this is the table's column order,
            // which a pull writes and a positional `INSERT` binds to. Left
            // to the rendering below, the column's random uid decided it
            // (#2080).
            if let Change::Column(ColumnChange::AddColumn { table, name, .. }) = &p.change {
                self.declared
                    .schema
                    .tables
                    .get(table)
                    .and_then(|t| t.columns.get_index_of(name))
                    .unwrap_or(0)
            } else {
                0
            },
            tie_break_text(&p.change),
        )
    }
}

/// The facts the sort reads, computed in the order the passes ran before the split:
/// the three that push into `errs` are `rename_chain_depths`, `sequence_release_ranks`
/// and `domain_type_ranks`, in that order. The plan and the list go in by value and
/// come back, because the sort and the finish go on with them.
#[allow(clippy::too_many_arguments)] // Each fact reads one of these; the parent function is allowed the same.
pub(in super::super) fn sort_facts<'a>(
    planned: Vec<PlannedChange>,
    base: Side<'a>,
    declared: Side<'a>,
    dialect: &(impl Dialect + ?Sized),
    module_deps: pbps_model::module::ModuleDeps,
    index: Indexes<'a>,
    early_identity: BTreeSet<(Uid, Option<ReplicaIdentity>)>,
    mut errs: Vec<DiffError>,
) -> (SortFacts<'a>, Vec<PlannedChange>, Vec<DiffError>) {
    let (create_rank, drop_rank) = module_ranks(base, declared, dialect, &module_deps);
    let (attach_parents, data_rank) = attach_parents_and_data_rank(&planned, declared);
    // Roles that are dropped together are ordered parent before member.
    let role_rank = member_depth(&planned);
    let tables_claiming_a_column_name = column_name_claims(&planned);
    let chain_depth = rename_chain_depths(&planned, &mut errs);
    let generated_in_base = generated_columns_in_base(base, &index);
    let row_tables = row_change_tables(&planned);
    let identified = identities_put_on(planned.iter().map(|p| &p.change));
    let inserted = tables_with_inserts(planned.iter().map(|p| &p.change));
    let late = numbering_after_deletes(planned.iter().map(|p| &p.change));
    let installed = numbering_put_on(planned.iter().map(|p| &p.change));
    let held_back = columns_held_back(&planned);
    let bounds_first = columns_whose_bounds_go_first(&planned, dialect);
    let early_release = sequence_release_ranks(&planned, &held_back, &mut errs);
    let late_attach = late_attach_ranks(declared, &attach_parents, &row_tables, &data_rank);
    // A function a computed column calls is dropped after the column, by a
    // connected plan's pass over the catalog's own edges
    // (`order_computed_by_edges`, DEC-1431.1). An offline plan is never
    // applied, and a text scan here took one function for another (#1174
    // review), so the differ moves nothing for it.
    let (fk_graph, deepest) = persistence_fk_graph(&planned, declared);
    let parent_moved = parent_columns_moved(&planned);
    let parent_columns_change = parents_with_column_changes(&planned, declared);
    let moved_out_of = defaults_moved_out_of(&planned);
    let (type_rank, drop_type_rank) = domain_type_ranks(base, declared, &mut errs);
    let extension_rank = crate::extensions::depths(planned.iter().map(|p| &p.change));
    let package_floor = package_floor_of(&planned, &extension_rank);
    let (created_search, altered_search) = searches_in_the_plan(&planned);
    let (mut search_rank, drop_search_rank) = search_ranks(
        base,
        declared,
        package_floor,
        &created_search,
        &altered_search,
    );
    search_rank.extend(search_rename_depths(&planned));
    let domain_floor =
        domain_floor_behind_searches(&planned, dialect, &created_search, &search_rank);
    (
        SortFacts {
            tables_claiming_a_column_name,
            generated_in_base,
            row_tables,
            identified,
            inserted,
            late,
            installed,
            bounds_first,
            early_release,
            late_attach,
            fk_graph,
            deepest,
            parent_moved,
            parent_columns_change,
            moved_out_of,
            early_identity,
            chain_depth,
            declared,
            create_rank,
            drop_rank,
            data_rank,
            role_rank,
            type_rank,
            drop_type_rank,
            extension_rank,
            search_rank,
            drop_search_rank,
            package_floor,
            domain_floor,
        },
        planned,
        errs,
    )
}
