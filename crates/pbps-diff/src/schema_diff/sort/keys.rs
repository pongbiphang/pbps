//! The coarse order of a change: its dependency rank, its class order and the ranks of the objects it waits on.

use super::super::*;

/// Position in the dependency order, by table name.
pub(in super::super) fn rank_of_tables(order: &[TableName]) -> BTreeMap<TableName, usize> {
    order
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), i))
        .collect()
}

/// Position in the dependency order, by identity.
pub(in super::super) fn rank_of(order: &[ModuleId]) -> BTreeMap<ModuleId, usize> {
    order
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), i))
        .collect()
}

/// Where a change sorts *within* its ordering class.
///
/// Modules, reference rows and foreign keys each have a dependency order among
/// their class; everything else is zero and keeps the tiebreakers that were
/// already there. In every family the removing direction is the reverse of the
/// creating one, so a dependent goes before the thing it depends on.
///
/// Modules and rows carry a computed rank because their dependencies form a
/// graph. A foreign key's is a two-level layering — it depends on candidate
/// keys, and nothing depends on it — so a constant says it exactly.
pub(in super::super) fn dependency_rank(
    change: &Change,
    create_rank: &BTreeMap<ModuleId, usize>,
    drop_rank: &BTreeMap<ModuleId, usize>,
    data_rank: &BTreeMap<TableName, usize>,
    role_rank: &BTreeMap<pbps_model::RoleName, usize>,
    type_rank: &BTreeMap<pbps_model::TypeRef, isize>,
    drop_type_rank: &BTreeMap<pbps_model::TypeRef, isize>,
) -> isize {
    match change {
        Change::Partition(PartitionChange::RebuildPartitionedTable(_))
        | Change::Index(IndexChange::BuildPartitionedIndex(_))
        | Change::Partition(PartitionChange::CopyTableRows(_))
        | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => {
            unreachable!("rebuild operations are expanded by order, never ranked by the differ")
        }
        Change::Module(ModuleChange::CreateModule { id, .. }) | Change::Module(ModuleChange::AlterModule { id, .. }) => {
            create_rank.get(id).map_or(0, |r| *r as isize)
        }
        Change::Module(ModuleChange::DropModule { id, .. }) => -(drop_rank.get(id).map_or(0, |r| *r as isize)),
        // Module ranks can be arbitrarily deep. A fixed -1 would leave a
        // policy behind a dropped routine it calls (DEC-1173.2).
        Change::Policy(PolicyChange::DropPolicy { .. }) => -(drop_rank.len() as isize) - 1,
        Change::TypeValue(TypeValueChange::AddEnumValue { ordinal, .. }) | Change::TypeValue(TypeValueChange::RenameEnumValue { ordinal, .. }) => {
            *ordinal as isize
        }
        // After every label change of class 7, which it may name.
        Change::TypeValue(TypeValueChange::AlterDomain {
            half: pbps_model::DomainHalf::Loosen,
            from,
            to,
            ..
        }) if domain_sets_default(from, to) => isize::MAX,
        Change::TypeValue(TypeValueChange::AlterDomain { .. }) => 0,
        Change::TypeValue(TypeValueChange::EnumRebuild {
            step: pbps_model::EnumRebuildStep::Create { ordinal, .. },
            ..
        }) => *ordinal as isize,
        // With a retype: after the defaults taken off for it (-2) and before
        // the ones written for the type it ends with (0). The swap follows
        // every column of its type because they share a subject and rank and
        // the rendering that breaks the tie names `Retype` before `Swap`
        // (pinned by `the_whole_plan_drops_and_restores_what_names_a_moved_column_and_orders_the_steps`).
        Change::TypeValue(TypeValueChange::EnumRebuild { .. }) => -1,
        Change::Type(TypeChange::CreateType { name, .. }) | Change::Type(TypeChange::RenameType { to: name, .. }) => {
            *type_rank.get(name).unwrap_or(&0)
        }
        Change::Type(TypeChange::DropType { name, .. }) => -drop_type_rank.get(name).copied().unwrap_or(0),
        Change::Table(TableChange::CreateTable { .. })
        // Ranked by their depth where the sort key is built (`search_rank`).
        | Change::Search(SearchChange::CreateSearch { .. })
        | Change::Search(SearchChange::AlterSearchMapping { .. })
        | Change::Search(SearchChange::AlterSearchDictionary { .. })
        | Change::Search(SearchChange::DropSearch { .. })
        | Change::Extension(ExtensionChange::CreateExtension { .. })
        | Change::Extension(ExtensionChange::DropExtension { .. })
        | Change::Extension(ExtensionChange::UpdateExtension { .. })
        | Change::Extension(ExtensionChange::SetExtensionSchema { .. })
        | Change::Table(TableChange::DropTable { .. })
        | Change::Partition(PartitionChange::DetachPartition { .. })
        | Change::Partition(PartitionChange::AttachPartition { .. })
        | Change::Table(TableChange::RenameTable { .. })
        | Change::Column(ColumnChange::AddColumn { .. })
        | Change::Column(ColumnChange::DropColumn { .. })
        | Change::Column(ColumnChange::RenameColumn { .. })
        | Change::Column(ColumnChange::SetColumnDeprecated { .. })
        | Change::TableProperty(TablePropertyChange::SetComment { .. })
        | Change::Index(IndexChange::SetIndexStorageParameters { .. })
        | Change::TableProperty(TablePropertyChange::SetTablePersistence { .. })
        | Change::Policy(PolicyChange::CreatePolicy { .. })
        | Change::Policy(PolicyChange::SetRowSecurity { .. })
        | Change::TableProperty(TablePropertyChange::SetStorageParameters { .. })
        | Change::Constraint(ConstraintChange::DropUnique { .. })
        | Change::Constraint(ConstraintChange::SetExclusion { .. })
        | Change::Constraint(ConstraintChange::AddCheck { .. })
        | Change::Constraint(ConstraintChange::DropCheck { .. })
        | Change::Index(IndexChange::DropIndex { .. })
        | Change::Column(ColumnChange::AddComputedColumn { .. })
        | Change::Column(ColumnChange::DropComputedColumn { .. })
        | Change::Row(RowChange::SetDataMode { .. })
        | Change::Row(RowChange::SetEnsureKeys { .. }) => 0,
        // The clustered index goes in ahead of the rest of the addition
        // class. Building it rebuilds every nonclustered index already on the
        // table, whose row locators become its key, so an index added before
        // it is built twice (#1178). Correctness needs nothing here: the old
        // clustered index is gone by now, since every drop is in class 2.
        //
        // A key with no layout to speak of — every PostgreSQL key, whose
        // `nonclustered` is always false — moves ahead of its siblings too,
        // which orders nothing that depends on it.
        Change::Constraint(ConstraintChange::SetPrimaryKey {
            to: Some(_),
            nonclustered: false,
            ..
        })
        | Change::Constraint(ConstraintChange::AddUnique {
            clustered: true, ..
        })
        | Change::Index(IndexChange::AddIndex {
            clustered: true, ..
        }) => -1,
        Change::Constraint(ConstraintChange::SetPrimaryKey { .. }) | Change::Constraint(ConstraintChange::AddUnique { .. }) | Change::Index(IndexChange::AddIndex { .. }) => 0,
        // After every addition of its class, the index it names among them,
        // and the foreign keys too: nothing in the class reads it.
        Change::TableProperty(TablePropertyChange::SetReplicaIdentity { .. }) => 2,
        // A foreign key needs the key it references to exist, so it goes last
        // in the addition class and first in the drop class. Measured on the
        // pinned image, both halves are real: added before its key, the engine
        // refuses with 1776, "There are no primary or candidate keys in the
        // referenced table ... that match the referencing column list"; and
        // the key cannot be dropped while the foreign key stands (3727 for a
        // `UNIQUE` constraint, 3723 for a unique index).
        //
        // A constant, not a rank computed from the referenced columns, because
        // the dependency here is a total layering rather than a graph: every
        // supplier of a candidate key — `SetPrimaryKey`, `AddUnique` and
        // `AddIndex`, since a foreign key may reference a plain unique index —
        // is above every foreign key, and no foreign key supplies a key to
        // another. Matching suppliers by name would add a way to fail (a
        // supplier not recognised, and the ordering silently back) and buy
        // nothing. Everything else in both classes keeps rank 0 and the
        // tiebreaker it had.
        Change::Constraint(ConstraintChange::AddForeignKey { .. }) => 1,
        Change::Constraint(ConstraintChange::DropForeignKey { .. }) => -1,
        // A column's new type goes in before a default written for it. They
        // share class 9 and the tiebreaker below is the change's rendering,
        // which sorts `AlterColumnDefault` ahead of `AlterColumnType` by the
        // alphabet — so a default that only fits the new type was set against
        // the old one. Measured, PostgreSQL refuses `SET DEFAULT 'abc'` on an
        // `integer` column with "invalid input syntax for type integer", and
        // the plan that widened the column to `text` in the next statement
        // rolled back.
        //
        // A constant, like the foreign key's above and for the same reason:
        // the dependency is a layering, not a graph. A type change never needs
        // a default that is already there, and the nullability travels inside
        // the type change rather than beside it.
        Change::Column(ColumnChange::AlterColumnType { .. }) => -1,
        // And the old default goes before the type, which makes the three
        // phases a column can need one rank each: drop the default the old
        // type gave meaning to, change the type, install the default written
        // for the new one. Measured, SQL Server refuses the middle statement
        // while the first one's constraint stands (5074 with 4922 behind it),
        // and `diff_columns` splits a *replaced* default into these two halves
        // when the type moves, so both ranks have something to order.
        Change::Column(ColumnChange::AlterColumnDefault { to: None, .. }) => -2,
        Change::Column(ColumnChange::AlterColumnDefault { to: Some(_), .. }) => 0,
        // After any type change, as a new default is: the expression is
        // recomputed under the column's final type (DEC-1168.1).
        Change::Column(ColumnChange::AlterColumnExpression { .. }) => 0,
        // A numbering source (#1889, DEC-1889.1). One given up, and an
        // identity turned into a sequence, go first: ahead of the relaxation
        // its column may also take, since measured on 16 and 18 `DROP NOT
        // NULL` on an identity column is refused ("is an identity column"),
        // and ahead of the plain default it may give way to, which is set at
        // 0 and which giving up a sequence's default would drop (#1930
        // review). Anything else goes last: an identity
        // is added only to a column already `NOT NULL` ("must be declared
        // NOT NULL before identity can be added"), so after the tightening,
        // and a sequence is moved past the column's values in the type the
        // column ends with, so after the retype.
        Change::Column(ColumnChange::AlterColumnNumbering {
            from: ValueSource::Identity(_) | ValueSource::Sequence(_),
            to: ValueSource::None,
            ..
        })
        | Change::Column(ColumnChange::AlterColumnNumbering {
            from: ValueSource::Identity(_),
            to: ValueSource::Sequence(_),
            ..
        }) => -3,
        Change::Column(ColumnChange::AlterColumnNumbering { .. }) => 2,
        // A recomputation is checked against the column's nullability as it
        // stands, so a relaxation goes before one and a tightening after it.
        // Measured on 17.11, `SET EXPRESSION` that yields a NULL is refused
        // under `NOT NULL` ("contains null values") and accepted once
        // `DROP NOT NULL` has run. Nothing else in the class reads nullability.
        Change::Column(ColumnChange::AlterColumnNullability {
            to_nullable: true, ..
        }) => -1,
        Change::Column(ColumnChange::AlterColumnNullability {
            to_nullable: false, ..
        }) => 1,
        // A partition's own, alike: its column's type is its parent's, which
        // no change here moves (#1581).
        Change::Partition(PartitionChange::SetPartitionDefault { .. }) => 0,
        Change::Partition(PartitionChange::SetPartitionNotNull {
            not_null: false, ..
        }) => -1,
        Change::Partition(PartitionChange::SetPartitionNotNull { not_null: true, .. }) => 1,
        // Rows follow the foreign keys between their tables: a referenced
        // table's rows go in first, and out last.
        Change::Row(RowChange::InsertRow { table, .. }) | Change::Row(RowChange::UpdateRow { table, .. }) => {
            data_rank.get(table).map_or(0, |r| *r as isize)
        }
        Change::Row(RowChange::DeleteRow { table, .. }) => -(data_rank.get(table).map_or(0, |r| *r as isize)),
        // Two roles dropped together are ordered by membership: see
        // [`member_depth`].
        Change::Role(RoleChange::DropRole { name, .. }) => role_rank.get(name).map_or(0, |r| *r as isize),
        // The rest depend on nothing among themselves; a grant's target is
        // ordered by the class of the change, not by rank.
        Change::Role(RoleChange::CreateRole { .. })
        | Change::Role(RoleChange::RenameRole { .. })
        | Change::Permission(GrantChange::Grant { .. })
        | Change::Permission(GrantChange::Revoke { .. }) => 0,
        // The rank of the routine it settles, so it sorts beside that
        // routine's `CREATE` in class 14 (see `order_key`, #687).
        Change::Permission(GrantChange::PublicExecution { routine, .. }) => create_rank
            .get(&ModuleId::Routine(routine.clone()))
            .map_or(0, |r| *r as isize),
    }
}

/// The order of application.
///
/// Ordinarily the table rename comes first, so later steps use current names.
/// The specific freeing-drop graph may order some drops around it and supplies
/// their execution addresses (DECISIONS 496). Remaining constraint and index drops come next: they must precede both the
/// column renames they would otherwise block and the column drops they
/// reference. Adding them back must follow adding columns.
/// Inserting a class shifts every class below it, and these ordinals are
/// quoted in prose that uses them to justify behaviour: DECISIONS 140, 146,
/// 151, 174 and 237, `docs/PITFALLS.md`, `preflight.rs` and `deploy.rs`. A new
/// class means renumbering those in the same commit — a stale ordinal there
/// reads as a statement about the code and is not checked against it.
/// The class of the changes that alter an existing column in place: its
/// type, nullability, default or generation expression. Named because a
/// generated column's addition is placed after it (DEC-1168.1).
pub(in super::super) const COLUMN_ALTERATIONS: u8 = 9;

/// The class of row deletions, the last row changes. Named because a
/// tightening of a table with row changes is placed at its end (#1367).
pub(in super::super) const ROW_DELETIONS: u8 = 12;

/// Whether going from `from` to `to` sets a domain's default, as against
/// dropping or keeping it.
fn domain_sets_default(from: &pbps_model::ManagedType, to: &pbps_model::ManagedType) -> bool {
    pbps_model::DomainDelta::of(from, to).is_some_and(|d| matches!(d.default, Some(Some(_))))
}

/// The subject a change is sorted by within its class and rank. A rebuilt
/// type's steps lead with their phase, so that every retype of every rebuilt
/// type comes before the first swap, and a swap before the swap of the type
/// it is built on: the old base cannot be dropped while a domain over it
/// stands (DEC-1635.9). Every other change sorts by its own subject.
pub(in super::super) fn rebuild_subject(change: &Change) -> String {
    use pbps_model::EnumRebuildStep as Step;
    let Change::TypeValue(TypeValueChange::EnumRebuild { step, .. }) = change else {
        return change.subject();
    };
    match step {
        Step::Retype { .. } => format!("0 {}", change.subject()),
        // Fixed width, reversed: a deeper swap sorts first at any depth.
        Step::Swap { depth, .. } => format!("1 {:020} {}", usize::MAX - *depth, change.subject()),
        Step::Create { .. } => change.subject(),
    }
}

pub(in super::super) fn order_key(c: &Change) -> u8 {
    match c {
        Change::Partition(PartitionChange::RebuildPartitionedTable(_))
        | Change::Index(IndexChange::BuildPartitionedIndex(_))
        | Change::Partition(PartitionChange::CopyTableRows(_))
        | Change::Partition(PartitionChange::AnalyzeTableTree(_)) => {
            unreachable!("rebuild operations are expanded by order, never ranked by the differ")
        }
        Change::TypeValue(TypeValueChange::AddEnumValue { .. })
        | Change::TypeValue(TypeValueChange::RenameEnumValue { .. })
        | Change::TypeValue(TypeValueChange::EnumRebuild {
            step: pbps_model::EnumRebuildStep::Create { .. },
            ..
        })
        // A package's own objects are what a later type, module or table of
        // the plan may use, so it goes with the type creates; the order among
        // packages is the `requires` edge, not this number (DEC-2447.6).
        | Change::Type(TypeChange::CreateType { .. })
        // Created before the tables, defaults, indexes and checks that name them by a
        // string literal, and a dictionary before the configuration that maps it
        // (DEC-2728.2). One altered in place goes first of its dependents too (DEC-2728.3).
        | Change::Search(SearchChange::CreateSearch { .. })
        | Change::Search(SearchChange::AlterSearchMapping { .. })
        | Change::Search(SearchChange::AlterSearchDictionary { .. })
        | Change::Extension(ExtensionChange::CreateExtension { .. })
        | Change::Extension(ExtensionChange::UpdateExtension { .. })
        | Change::Extension(ExtensionChange::SetExtensionSchema { .. }) => 7,
        // A domain's checks and NOT NULL are dropped with a table's, before
        // the plan's rows, and added with them after the rows (DEC-1635.8).
        // A default the half sets can name a label the plan renames or adds,
        // so that half waits for the type changes (class 7); it still
        // precedes every column and row.
        Change::TypeValue(TypeValueChange::AlterDomain {
            half: pbps_model::DomainHalf::Loosen,
            from,
            to,
            ..
        }) => {
            if domain_sets_default(from, to) {
                7
            } else {
                2
            }
        }
        Change::TypeValue(TypeValueChange::AlterDomain {
            half: pbps_model::DomainHalf::Tighten,
            ..
        }) => 13,
        Change::TypeValue(TypeValueChange::EnumRebuild { .. }) => COLUMN_ALTERATIONS,
        // After every module this plan drops and the packages it requires
        // are gone: late, like the type drops (DEC-2447.8).
        Change::Type(TypeChange::DropType { .. }) | Change::Search(SearchChange::DropSearch { .. }) | Change::Extension(ExtensionChange::DropExtension { .. }) => 18,
        Change::Type(TypeChange::RenameType { .. }) => 1,
        // A policy releases its bindings before module and column teardown.
        Change::Policy(PolicyChange::DropPolicy { .. }) => 0,
        // Modules go first and last, and both ends are load-bearing. A
        // SCHEMABINDING view blocks a rename of the column it binds, so every
        // module that is going has to go before the table changes; and a view
        // can only be created once the columns it selects exist.
        Change::Module(ModuleChange::DropModule { .. }) => 0,
        // A role drop needs nothing else gone first, and a plan that also
        // recreates the name wants the old one out of the way early.
        Change::Role(RoleChange::DropRole { .. }) => 0,
        // A table rename, and a role rename that depends on nothing here.
        Change::Table(TableChange::RenameTable { .. }) | Change::Role(RoleChange::RenameRole { .. }) => 1,
        // Before the column renames, and after the table rename that gives
        // these drops the name they use.
        //
        // The engine refuses `sp_rename` on a column a check constraint or a
        // filtered index's predicate names — 15336 for the check, 5074 with
        // 4922 behind it for the index, measured — so for those two the drop
        // is what makes the rename possible, and running it after was a valid,
        // reviewed plan the engine would not perform. The re-add stays in the
        // constraint class far below, where the new spelling can be written.
        //
        // The whole group moves, not the two kinds that need it. Measured, a
        // rename is never blocked by a constraint that does not name the
        // column, so moving the rest costs nothing — and the alternative is
        // asking which columns a check's expression names, which this tool
        // deliberately never parses (DECISIONS 174). `DropForeignKey`'s rank
        // travels with them and still puts it ahead of the key it references.
        //
        // A primary key that is *only* dropped is one of these drops and
        // travels with them. It was left in the addition class below because
        // one variant carries both directions, and a key that is still there
        // blocks the column changes every class from here down can make:
        // measured, `DROP NOT NULL` on a key column is `42P16` on PostgreSQL
        // and 5074 with 4922 behind it on SQL Server, and dropping the column
        // outright is the same 5074. So a declaration that gives up a key and
        // relaxes its column produced a plan neither engine would perform.
        //
        // Only `to: None`, and that is not a partial answer: a key being
        // *replaced* is emitted as two changes, its drop and its add (see
        // `diff_constraints`), so the drop arrives here and the add stays
        // below, where the columns its new shape may name have been added.
        //
        // `dependency_rank` already keeps a foreign-key drop ahead of it
        // inside this class, which is the order the engine requires and the
        // reason that rank exists.
        Change::Index(IndexChange::DropIndex { .. })
        | Change::Constraint(ConstraintChange::DropUnique { .. })
        | Change::Constraint(ConstraintChange::DropForeignKey { .. })
        | Change::Constraint(ConstraintChange::DropCheck { .. })
        | Change::Constraint(ConstraintChange::SetExclusion { to: None, .. })
        | Change::Constraint(ConstraintChange::SetPrimaryKey { to: None, .. }) => 2,
        // After the drops above (`sort_class` places it at the class's end),
        // since an index or check over it blocks it (4922), and before every
        // rename, drop or retype of a column its expression reads, which it
        // blocks in turn (15336, 4922; measured on 17.0, #1174).
        Change::Column(ColumnChange::DropComputedColumn { .. }) => 2,
        // A class of its own, after the table renames: `sp_rename` on a column
        // names the table, and `resolve_columns` iterates the *declared*
        // schema, so a `RenameColumn` always carries the post-rename table.
        //
        // Not sharing class 1 with `RenameTable`, which is where it was. The
        // two are not peers — one depends on the other — and sharing a class
        // left `subject()` to separate them. `RenameTable` answers with its
        // `from`, the old name, while the column rename carries the new one,
        // so the two were sorted against two different names for one table and
        // the alphabet decided: `dbo.customers` -> `dbo.clients` put the
        // column rename first, against a table that did not exist yet.
        Change::Column(ColumnChange::RenameColumn { .. }) => 3,
        // After the renames, so a revoke names the role and the object as
        // they now are. It shared a class with the drops above until they
        // moved ahead of the renames; it has the opposite need, so it stayed
        // and took a class of its own. A revoke on an object this plan drops
        // is never emitted (see `diff_roles`).
        Change::Permission(GrantChange::Revoke { .. }) => 4,
        Change::Column(ColumnChange::DropColumn { .. }) => 5,
        // A detach frees its range, as a drop does, before a partition is
        // created over it in class 7 (#1544).
        Change::Table(TableChange::DropTable { .. }) | Change::Partition(PartitionChange::DetachPartition { .. }) => 6,
        // An attach takes its range as a created partition does, and once
        // a detach or a drop has freed it (#1545).
        Change::Table(TableChange::CreateTable { .. }) | Change::Partition(PartitionChange::AttachPartition { .. }) => 7,
        Change::Column(ColumnChange::AddColumn { .. }) => 8,
        Change::Column(ColumnChange::AlterColumnType { .. })
        | Change::Column(ColumnChange::AlterColumnNullability { .. })
        | Change::Column(ColumnChange::AlterColumnDefault { .. })
        | Change::Column(ColumnChange::AlterColumnNumbering { .. })
        | Change::Column(ColumnChange::AlterColumnExpression { .. })
        | Change::Partition(PartitionChange::SetPartitionDefault { .. })
        | Change::Partition(PartitionChange::SetPartitionNotNull { .. }) => COLUMN_ALTERATIONS,
        // Once every column its expression reads is there in its final type
        // (`sort_class` places it after the class's alterations), and before
        // the indexes and checks of class 13 that may be over it (#1174).
        Change::Column(ColumnChange::AddComputedColumn { .. }) => COLUMN_ALTERATIONS,
        Change::Column(ColumnChange::SetColumnDeprecated { .. }) => 10,
        // A comment is metadata too, on the object as the plan leaves it: after its
        // create (7, 8), alterations (9) and every rename (1), under its final name
        // (DEC-2729.1, measured: a drop and a create lose a comment, a rename keeps it).
        Change::TableProperty(TablePropertyChange::SetComment { .. }) => 10,
        // Metadata too: no statement reads a storage parameter, and it runs
        // under the table's final name (DEC-1441.1).
        Change::TableProperty(TablePropertyChange::SetStorageParameters { .. }) => 10,
        // After the foreign-key drops of class 2 and before the adds of 13:
        // the engine checks the keys that stand when it switches (#1443).
        Change::TableProperty(TablePropertyChange::SetTablePersistence { .. }) => COLUMN_ALTERATIONS,
        // In place too, and nothing reads one (DEC-1442.1).
        Change::Index(IndexChange::SetIndexStorageParameters { .. }) => 10,
        // Rows arrive once every column they name exists and has its final
        // type, and before the constraints below: ADR-0004's "create table ->
        // insert rows -> add the foreign key that references them".
        Change::Row(RowChange::InsertRow { .. }) | Change::Row(RowChange::UpdateRow { .. }) => 11,
        // Rows leave after every insert and update, and after the foreign keys
        // that could block them are gone. No single order satisfies every
        // shape — a delete-then-insert on a table with a UNIQUE elsewhere
        // wants the delete first — but this is the order whose failure is
        // *loud*: the engine refuses inside the transaction and the plan rolls
        // back. Deleting first fails silently: a child row that moves its
        // foreign key to another parent in this same plan is still pointing
        // at the old one when the old one goes, and `ON DELETE CASCADE` takes
        // the child with it, after which the update touches zero rows and
        // nothing says so.
        Change::Row(RowChange::DeleteRow { .. }) => ROW_DELETIONS,
        Change::Constraint(ConstraintChange::SetPrimaryKey { .. })
        | Change::Constraint(ConstraintChange::AddUnique { .. })
        | Change::Constraint(ConstraintChange::AddForeignKey { .. })
        | Change::Constraint(ConstraintChange::AddCheck { .. })
        | Change::Constraint(ConstraintChange::SetExclusion { to: Some(_), .. })
        | Change::Index(IndexChange::AddIndex { .. }) => 13,
        // Late, it follows the index it names (DEC-1444.1); `sort_class`
        // moves the rest to the front.
        Change::TableProperty(TablePropertyChange::SetReplicaIdentity { .. }) => 13,
        Change::Policy(PolicyChange::CreatePolicy { .. })
        | Change::Policy(PolicyChange::SetRowSecurity { .. })
        | Change::Module(ModuleChange::CreateModule { .. })
        | Change::Module(ModuleChange::AlterModule { .. }) => 14,
        // The `PUBLIC` decision for a routine travels with the routine: class
        // 14 at that routine's own create rank (`dependency_rank`), and its
        // subject is the routine's, so the sort puts it immediately after its
        // `CREATE` or `ALTER` with nothing between them. In a transactional
        // apply nothing outside sees the gap either way; in the autocommit
        // script `--sql` renders (DECISIONS 259) a routine created at 14 and
        // revoked at 16 held the engine's default `EXECUTE` for `PUBLIC` for
        // the rest of the modules and every role create (#687). A class of its
        // own after the modules would still leave a later routine's `CREATE`
        // in between, and would renumber every ordinal below (DEC-687.1).
        Change::Permission(GrantChange::PublicExecution { .. }) => 14,
        // A grant names an object, so it comes after every object exists —
        // and after the role does.
        Change::Role(RoleChange::CreateRole { .. }) => 15,
        Change::Permission(GrantChange::Grant { .. }) => 16,
        // Emits nothing; it exists so the recorded state matches the file. Last
        // keeps it out of the way of everything that does emit.
        Change::Row(RowChange::SetDataMode { .. }) | Change::Row(RowChange::SetEnsureKeys { .. }) => 17,
    }
}
