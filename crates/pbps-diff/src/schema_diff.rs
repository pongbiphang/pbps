//! Change planning: comparing two "state + identity" pairs to produce a change
//! set.
//!
//! # Why both sides carry an identity file
//!
//! Matching up "which column is which" cannot rely on names — a rename makes the
//! names disagree across the two sides. Nor can it rely on this revision's intent
//! annotations, because a deployment may be many versions behind: when prod sits
//! at v1 and the declarations have reached v5, that rename intent left the
//! working tree long ago.
//!
//! So each side brings its own identity file and matching happens by **uid**: the
//! base's ids say `c_x → customer_name`, the declared ids say `c_x → full_name`,
//! and one comparison gives the rename directly, with no need to walk a chain of
//! names version by version. (This is exactly the problem SPEC §4.2 sets out to
//! solve.)
//!
//! # Where the base comes from
//!
//! `base` may come from the previous version in source control (an offline
//! preview) or from querying the database itself (Phase 3's authoritative plan).
//! This layer does not care which; it does the comparison and nothing else.

use std::collections::BTreeMap;

use pbps_dialect::Dialect;
use pbps_model::{
    Change, ChangeSet, ColumnRef, ColumnType, Hints, IdsFile, ObjectName, PlannedChange, Schema,
    Table, TableName, Uid,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiffError {
    /// In most databases IDENTITY cannot be changed with ALTER; the whole table
    /// has to be rebuilt. That is beyond what declarative automation should do on
    /// its own — a human has to choose the migration strategy.
    #[error(
        "the IDENTITY property of {column} changed, but IDENTITY cannot be modified with ALTER"
    )]
    IdentityChangeUnsupported { column: ColumnRef },
}

/// One side's complete input: a state plus its own identity mapping.
#[derive(Debug, Clone, Copy)]
pub struct Side<'a> {
    pub schema: &'a Schema,
    pub ids: &'a IdsFile,
}

/// Compares the base against the declarations and produces a change set.
///
/// `description` is **not compared** yet: it only affects data-catalogue prose,
/// not structure, and writing it to an extended property belongs to Phase 5.
///
/// The hints are the **declared** side's: a strategy describes how to operate on
/// the table as it will be, and a table that no longer exists has nothing left
/// to operate on (ADR-0003).
pub fn diff(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &dyn Dialect,
    hints: &Hints,
) -> Result<ChangeSet, Vec<DiffError>> {
    let mut changes = Vec::new();
    let mut errs = Vec::new();

    let base_tables = &base.ids.tables;
    let declared_tables = &declared.ids.tables;

    // Table present only in the base: dropped. Its foreign keys are dropped
    // first as separate changes — two dropped tables that reference each other
    // would otherwise fail or succeed depending on which DROP TABLE runs first.
    for (uid, name) in base_tables {
        if !declared_tables.contains_key(uid) {
            if let Some(t) = base.schema.tables.get(name) {
                for fk_name in t.foreign_keys.keys() {
                    changes.push(Change::DropForeignKey {
                        table: name.clone(),
                        name: fk_name.clone(),
                    });
                }
            }
            changes.push(Change::DropTable {
                uid: uid.clone(),
                name: name.clone(),
            });
        }
    }

    // Table present only in the declarations: created. Foreign keys are split
    // out of the CREATE into their own changes, because they sort after every
    // CreateTable — a new table's FK may reference another new table, and
    // within one ordering class the order of creation is not meaningful.
    for (uid, name) in declared_tables {
        if !base_tables.contains_key(uid)
            && let Some(t) = declared.schema.tables.get(name)
        {
            let mut table = t.clone();
            for (fk_name, fk) in std::mem::take(&mut table.foreign_keys) {
                changes.push(Change::AddForeignKey {
                    table: name.clone(),
                    name: fk_name,
                    constraint: Box::new(fk),
                });
            }
            changes.push(Change::CreateTable {
                uid: uid.clone(),
                name: name.clone(),
                table: Box::new(table),
            });
        }
    }

    // Table present on both sides: possibly renamed, and its contents must be
    // compared.
    for (uid, declared_name) in declared_tables {
        let Some(base_name) = base_tables.get(uid) else {
            continue;
        };
        if base_name != declared_name {
            changes.push(Change::RenameTable {
                uid: uid.clone(),
                from: base_name.clone(),
                to: declared_name.clone(),
            });
        }
        let (Some(base_table), Some(declared_table)) = (
            base.schema.tables.get(base_name),
            declared.schema.tables.get(declared_name),
        ) else {
            continue;
        };

        diff_columns(
            base,
            declared,
            base_name,
            declared_name,
            base_table,
            declared_table,
            dialect,
            &mut changes,
            &mut errs,
        );
        diff_constraints(declared_name, base_table, declared_table, &mut changes);
    }

    diff_modules(base.schema, declared.schema, dialect, &mut changes);

    if !errs.is_empty() {
        return Err(errs);
    }

    let mut planned: Vec<PlannedChange> = changes.into_iter().map(PlannedChange::new).collect();
    for p in &mut planned {
        if let Change::AlterColumnType { from, to, .. } = &p.change
            && let Some(r) = dialect.type_change_risk(from, to).risk_class()
        {
            p.risks.insert(r);
        }
        // Attached here, not looked up at emit time: the plan file is the
        // artifact the deployment gate reviews, and a hint resolved later
        // against a YAML file the deployment host may not have is a hint
        // nobody read (ADR-0003).
        if let Some(strategy) = hints.strategies.get(p.change.table()) {
            p.strategy = *strategy;
        }
    }

    // Modules are ordered among themselves by dependency: a view over a view
    // has to be created second, and dropped first (ADR-0002).
    let create_rank = rank_of(&pbps_model::module::creation_order(
        &declared.schema.modules,
        &hints.module_deps,
    ));
    let drop_rank = rank_of(&pbps_model::module::creation_order(
        &base.schema.modules,
        &hints.module_deps,
    ));
    // The tiebreaker within an ordering class is the table name, then the
    // change's rendering. Debug output alone would sort by uid, which is random
    // at mint time — the plan would be correct but differently ordered per
    // project, and a reviewer diffing two plan.sql files would see noise.
    planned.sort_by_key(|p| {
        (
            order_key(&p.change),
            module_rank(&p.change, &create_rank, &drop_rank),
            p.change.table().to_string(),
            format!("{:?}", p.change),
        )
    });
    Ok(ChangeSet { changes: planned })
}

/// Columns of a surviving table that exist on both sides: compare attributes.
#[allow(clippy::too_many_arguments)]
fn diff_columns(
    base: Side<'_>,
    declared: Side<'_>,
    base_table_name: &TableName,
    declared_table_name: &TableName,
    base_table: &Table,
    declared_table: &Table,
    dialect: &dyn Dialect,
    changes: &mut Vec<Change>,
    errs: &mut Vec<DiffError>,
) {
    let base_cols = columns_of(base.ids, base_table_name);
    let declared_cols = columns_of(declared.ids, declared_table_name);

    for (uid, declared_ref) in &declared_cols {
        let Some(base_ref) = base_cols.get(uid) else {
            // Present only on the declared side: a new column.
            if let Some(c) = declared_table.columns.get(&declared_ref.name) {
                changes.push(Change::AddColumn {
                    uid: uid.clone(),
                    table: declared_table_name.clone(),
                    name: declared_ref.name.clone(),
                    column: Box::new(c.clone()),
                });
            }
            continue;
        };

        if base_ref.name != declared_ref.name {
            changes.push(Change::RenameColumn {
                uid: uid.clone(),
                table: declared_table_name.clone(),
                from: base_ref.name.clone(),
                to: declared_ref.name.clone(),
            });
        }

        let (Some(base_col), Some(col)) = (
            base_table.columns.get(&base_ref.name),
            declared_table.columns.get(&declared_ref.name),
        ) else {
            continue;
        };

        if base_col.identity != col.identity {
            errs.push(DiffError::IdentityChangeUnsupported {
                column: declared_ref.clone(),
            });
        }

        let norm = |t: &ColumnType| dialect.normalize_type(t).unwrap_or_else(|_| t.clone());
        let (from_ty, to_ty) = (norm(&base_col.ty), norm(&col.ty));
        // A type change subsumes a nullability change rather than sitting beside
        // one: `ALTER COLUMN` restates the whole definition, so two changes would
        // mean two statements where the second undoes half of the first.
        if from_ty != to_ty {
            changes.push(Change::AlterColumnType {
                uid: uid.clone(),
                column: declared_ref.clone(),
                from: from_ty,
                to: to_ty,
                from_nullable: base_col.nullable,
                to_nullable: col.nullable,
            });
        } else if base_col.nullable != col.nullable {
            changes.push(Change::AlterColumnNullability {
                uid: uid.clone(),
                column: declared_ref.clone(),
                ty: to_ty,
                to_nullable: col.nullable,
            });
        }
        if base_col.default != col.default {
            changes.push(Change::AlterColumnDefault {
                uid: uid.clone(),
                column: declared_ref.clone(),
                from: base_col.default.clone(),
                to: col.default.clone(),
            });
        }
        if base_col.deprecated != col.deprecated {
            changes.push(Change::SetColumnDeprecated {
                uid: uid.clone(),
                column: declared_ref.clone(),
                reason: col.deprecated.clone(),
            });
        }
    }

    // Present only on the base side: a dropped column.
    for (uid, base_ref) in &base_cols {
        if !declared_cols.contains_key(uid) {
            changes.push(Change::DropColumn {
                uid: uid.clone(),
                column: base_ref.clone(),
            });
        }
    }
}

fn columns_of(ids: &IdsFile, table: &TableName) -> BTreeMap<Uid, ColumnRef> {
    ids.columns
        .iter()
        .filter(|(_, c)| &c.table == table)
        .map(|(u, c)| (u.clone(), c.clone()))
        .collect()
}

/// Constraints and indexes are always matched by name and never modified in
/// place — the database itself does drop + add, and pretending otherwise would
/// only give the emitter one more path that can fail.
fn diff_constraints(name: &TableName, base: &Table, declared: &Table, changes: &mut Vec<Change>) {
    if base.primary_key != declared.primary_key {
        changes.push(Change::SetPrimaryKey {
            table: name.clone(),
            from: base.primary_key.clone(),
            to: declared.primary_key.clone(),
        });
    }

    macro_rules! by_name {
        ($field:ident, $add:ident, $drop:ident, $wrap:expr) => {
            for (n, c) in &declared.$field {
                if base.$field.get(n) != Some(c) {
                    if base.$field.contains_key(n) {
                        changes.push(Change::$drop {
                            table: name.clone(),
                            name: n.clone(),
                        });
                    }
                    changes.push(Change::$add {
                        table: name.clone(),
                        name: n.clone(),
                        constraint: $wrap(c.clone()),
                    });
                }
            }
            for n in base
                .$field
                .keys()
                .filter(|n| !declared.$field.contains_key(*n))
            {
                changes.push(Change::$drop {
                    table: name.clone(),
                    name: n.clone(),
                });
            }
        };
    }

    by_name!(unique, AddUnique, DropUnique, std::convert::identity);
    by_name!(foreign_keys, AddForeignKey, DropForeignKey, Box::new);
    by_name!(checks, AddCheck, DropCheck, std::convert::identity);

    for (n, ix) in &declared.indexes {
        if base.indexes.get(n) != Some(ix) {
            if base.indexes.contains_key(n) {
                changes.push(Change::DropIndex {
                    table: name.clone(),
                    name: n.clone(),
                });
            }
            changes.push(Change::AddIndex {
                table: name.clone(),
                name: n.clone(),
                index: Box::new(ix.clone()),
            });
        }
    }
    for n in base
        .indexes
        .keys()
        .filter(|n| !declared.indexes.contains_key(*n))
    {
        changes.push(Change::DropIndex {
            table: name.clone(),
            name: n.clone(),
        });
    }
}

/// Position in the dependency order, by name.
fn rank_of(order: &[ObjectName]) -> BTreeMap<ObjectName, usize> {
    order
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), i))
        .collect()
}

/// Where a change sorts *within* its ordering class.
///
/// Only module changes have anything to say here; everything else is zero and
/// keeps the tiebreakers that were already there. Drops run in reverse creation
/// order, so a dependent goes before the thing it depends on.
fn module_rank(
    change: &Change,
    create_rank: &BTreeMap<ObjectName, usize>,
    drop_rank: &BTreeMap<ObjectName, usize>,
) -> isize {
    match change {
        Change::CreateModule { name, .. } | Change::AlterModule { name, .. } => {
            create_rank.get(name).map_or(0, |r| *r as isize)
        }
        Change::DropModule { name, .. } => -(drop_rank.get(name).map_or(0, |r| *r as isize)),
        Change::CreateTable { .. }
        | Change::DropTable { .. }
        | Change::RenameTable { .. }
        | Change::AddColumn { .. }
        | Change::DropColumn { .. }
        | Change::RenameColumn { .. }
        | Change::AlterColumnType { .. }
        | Change::AlterColumnNullability { .. }
        | Change::AlterColumnDefault { .. }
        | Change::SetColumnDeprecated { .. }
        | Change::SetPrimaryKey { .. }
        | Change::AddUnique { .. }
        | Change::DropUnique { .. }
        | Change::AddForeignKey { .. }
        | Change::DropForeignKey { .. }
        | Change::AddCheck { .. }
        | Change::DropCheck { .. }
        | Change::AddIndex { .. }
        | Change::DropIndex { .. } => 0,
    }
}

/// Modules are matched by **name**, never by uid: they carry no data, so they
/// carry no identity (ADR-0002).
///
/// Two of the three comparisons are ordinary. The third is the interesting one:
/// a module whose *kind* or trigger table changed is not an alteration at all —
/// `CREATE OR ALTER` cannot turn a view into a procedure, or move a trigger to
/// another table — so it is emitted as a drop followed by a create.
fn diff_modules(
    base: &Schema,
    declared: &Schema,
    dialect: &dyn Dialect,
    changes: &mut Vec<Change>,
) {
    for (name, module) in &declared.modules {
        match base.modules.get(name) {
            None => changes.push(Change::CreateModule {
                name: name.clone(),
                module: Box::new(module.clone()),
            }),
            Some(before) if before.kind != module.kind || before.on != module.on => {
                changes.push(Change::DropModule {
                    name: name.clone(),
                    kind: before.kind,
                });
                changes.push(Change::CreateModule {
                    name: name.clone(),
                    module: Box::new(module.clone()),
                });
            }
            Some(before) => {
                // The definition is compared after the dialect's lightweight
                // normalization, never by understanding it (SPEC §8.2). What
                // survives that is re-stated in full, which is idempotent and
                // keeps the permissions granted on the object.
                if dialect.normalize_definition(&before.definition)
                    != dialect.normalize_definition(&module.definition)
                {
                    changes.push(Change::AlterModule {
                        name: name.clone(),
                        module: Box::new(module.clone()),
                    });
                }
            }
        }
    }

    for (name, module) in &base.modules {
        if !declared.modules.contains_key(name) {
            changes.push(Change::DropModule {
                name: name.clone(),
                kind: module.kind,
            });
        }
    }
}

/// The order of application.
///
/// Renames come first, so every later step can use current names. Dropping
/// constraints and indexes must precede dropping columns, since they may
/// reference those columns; adding them must follow adding columns.
fn order_key(c: &Change) -> u8 {
    match c {
        // Modules go first and last, and both ends are load-bearing. A
        // SCHEMABINDING view blocks a rename of the column it binds, so every
        // module that is going has to go before the table changes; and a view
        // can only be created once the columns it selects exist.
        Change::DropModule { .. } => 0,
        Change::RenameTable { .. } | Change::RenameColumn { .. } => 1,
        Change::DropIndex { .. }
        | Change::DropUnique { .. }
        | Change::DropForeignKey { .. }
        | Change::DropCheck { .. } => 2,
        Change::DropColumn { .. } => 3,
        Change::DropTable { .. } => 4,
        Change::CreateTable { .. } => 5,
        Change::AddColumn { .. } => 6,
        Change::AlterColumnType { .. }
        | Change::AlterColumnNullability { .. }
        | Change::AlterColumnDefault { .. } => 7,
        Change::SetColumnDeprecated { .. } => 8,
        Change::SetPrimaryKey { .. }
        | Change::AddUnique { .. }
        | Change::AddForeignKey { .. }
        | Change::AddCheck { .. }
        | Change::AddIndex { .. } => 9,
        Change::CreateModule { .. } | Change::AlterModule { .. } => 10,
    }
}
#[cfg(test)]
#[allow(clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;
    use crate::identity::Context;
    use indexmap::IndexMap;
    use pbps_dialect::MinimalDialect;
    use pbps_model::{Column, ColumnType, IdsFile, Index, IndexColumn, Intent, RiskClass, Uid};

    fn ctx() -> Context {
        Context {
            operator: "leon".into(),
            today: "2026-08-30".into(),
        }
    }

    fn ty(s: &str) -> ColumnType {
        s.parse().unwrap()
    }

    fn table(cols: &[(&str, Column)]) -> Table {
        let mut columns = IndexMap::new();
        for (n, c) in cols {
            columns.insert((*n).to_string(), c.clone());
        }
        Table {
            columns,
            ..Default::default()
        }
    }

    fn schema_of(name: &str, t: Table) -> Schema {
        let mut s = Schema::default();
        s.tables.insert(name.parse().unwrap(), t);
        s
    }

    /// Reproduces the real flow: the base side's identity file is the one from
    /// that version, and the declared side's is the resolved result.
    fn run(base: &Schema, declared: &Schema, intents: &[Intent]) -> ChangeSet {
        let base_ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let declared_ids = crate::resolve(declared, &base_ids, intents, &ctx())
            .unwrap()
            .ids;
        diff(
            Side {
                schema: base,
                ids: &base_ids,
            },
            Side {
                schema: declared,
                ids: &declared_ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .unwrap()
    }

    fn kinds(cs: &ChangeSet) -> Vec<String> {
        cs.changes
            .iter()
            .map(|p| {
                format!("{:?}", p.change)
                    .split_whitespace()
                    .next()
                    .unwrap_or("?")
                    .trim_end_matches('{')
                    .trim()
                    .to_string()
            })
            .collect()
    }

    /// Found by the live convergence test: a new table's FK referenced another
    /// new table, and whether the referenced CREATE ran first depended on the
    /// random uid order. The FK must always come out as its own change, after
    /// every CreateTable — from either direction of the reference.
    #[test]
    fn a_foreign_key_between_two_new_tables_sorts_after_both_creates() {
        // Run it both ways round: customer -> region and region2 -> aaa. With
        // the bug, one of the two directions fails depending on name order.
        for (referencing, referenced) in [("dbo.customer", "dbo.region"), ("dbo.aaa", "dbo.zzz")] {
            let mut fk_table = table(&[("other_id", Column::new(ty("int")))]);
            fk_table.foreign_keys.insert(
                "fk_link".into(),
                pbps_model::ForeignKey {
                    columns: vec!["other_id".into()],
                    references_table: referenced.parse().unwrap(),
                    references_columns: vec!["id".into()],
                    on_delete: Default::default(),
                    on_update: Default::default(),
                },
            );
            let mut declared = schema_of(referencing, fk_table);
            declared.tables.insert(
                referenced.parse().unwrap(),
                table(&[("id", Column::new(ty("int")).not_null())]),
            );

            let cs = run(&Schema::default(), &declared, &[]);
            let ks = kinds(&cs);
            assert_eq!(
                ks,
                ["CreateTable", "CreateTable", "AddForeignKey"],
                "{referencing} -> {referenced}: {ks:?}"
            );
            // And the CreateTable no longer smuggles the FK along.
            assert!(
                cs.changes.iter().all(|p| match &p.change {
                    Change::CreateTable { table, .. } => table.foreign_keys.is_empty(),
                    _ => true,
                }),
                "the FK must not also ride inside the CREATE"
            );
        }
    }

    /// The mirror image: dropping two tables that reference each other must
    /// shed the foreign keys before either DROP TABLE runs.
    #[test]
    fn foreign_keys_of_dropped_tables_are_dropped_before_the_tables() {
        let mut fk_table = table(&[("other_id", Column::new(ty("int")))]);
        fk_table.foreign_keys.insert(
            "fk_link".into(),
            pbps_model::ForeignKey {
                columns: vec!["other_id".into()],
                references_table: "dbo.zzz".parse().unwrap(),
                references_columns: vec!["id".into()],
                on_delete: Default::default(),
                on_update: Default::default(),
            },
        );
        let mut base = schema_of("dbo.aaa", fk_table);
        base.tables.insert(
            "dbo.zzz".parse().unwrap(),
            table(&[("id", Column::new(ty("int")).not_null())]),
        );

        let intents = [
            Intent::DropTable {
                table: "dbo.aaa".parse().unwrap(),
                reason: "test".into(),
            },
            Intent::DropTable {
                table: "dbo.zzz".parse().unwrap(),
                reason: "test".into(),
            },
        ];
        let cs = run(&base, &Schema::default(), &intents);
        assert_eq!(
            kinds(&cs),
            ["DropForeignKey", "DropTable", "DropTable"],
            "{:?}",
            kinds(&cs)
        );
    }

    #[test]
    fn identical_schemas_produce_no_changes() {
        let s = schema_of("dbo.t", table(&[("a", Column::new(ty("int")))]));
        assert!(run(&s, &s, &[]).is_empty());
    }

    #[test]
    fn widening_a_type_carries_no_risk() {
        let base = schema_of("dbo.t", table(&[("a", Column::new(ty("nvarchar(50)")))]));
        let want = schema_of("dbo.t", table(&[("a", Column::new(ty("nvarchar(100)")))]));
        let cs = run(&base, &want, &[]);

        assert_eq!(cs.changes.len(), 1);
        assert!(cs.risks().is_empty(), "widening a length needs no approval");
    }

    #[test]
    fn narrowing_a_type_requires_approval() {
        let base = schema_of("dbo.t", table(&[("a", Column::new(ty("nvarchar(100)")))]));
        let want = schema_of("dbo.t", table(&[("a", Column::new(ty("nvarchar(50)")))]));
        let cs = run(&base, &want, &[]);

        assert!(cs.risks().contains(&RiskClass::Narrowing));
        assert_eq!(
            cs.unapproved_risks(&Default::default()),
            [RiskClass::Narrowing].into_iter().collect()
        );
    }

    /// A case difference is not a change, or retyping a type would produce a
    /// phantom diff every time.
    #[test]
    fn type_case_difference_is_not_a_change() {
        let base = schema_of("dbo.t", table(&[("a", Column::new(ty("NVARCHAR(100)")))]));
        let want = schema_of("dbo.t", table(&[("a", Column::new(ty("nvarchar(100)")))]));
        assert!(run(&base, &want, &[]).is_empty());
    }

    #[test]
    fn tightening_nullability_requires_approval() {
        let base = schema_of("dbo.t", table(&[("a", Column::new(ty("int")))]));
        let want = schema_of("dbo.t", table(&[("a", Column::new(ty("int")).not_null())]));
        let cs = run(&base, &want, &[]);

        assert!(cs.risks().contains(&RiskClass::NotNull));
    }

    #[test]
    fn loosening_nullability_is_safe() {
        let base = schema_of("dbo.t", table(&[("a", Column::new(ty("int")).not_null())]));
        let want = schema_of("dbo.t", table(&[("a", Column::new(ty("int")))]));
        assert!(run(&base, &want, &[]).risks().is_empty());
    }

    #[test]
    fn dropping_a_column_is_destructive() {
        let base = schema_of(
            "dbo.t",
            table(&[("a", Column::new(ty("int"))), ("b", Column::new(ty("int")))]),
        );
        let want = schema_of("dbo.t", table(&[("a", Column::new(ty("int")))]));
        let intents = vec![Intent::DropColumn {
            column: "dbo.t.b".parse().unwrap(),
            reason: "no longer in use".into(),
        }];
        let cs = run(&base, &want, &intents);

        assert_eq!(kinds(&cs), ["DropColumn"]);
        assert!(cs.risks().contains(&RiskClass::Destructive));
    }

    /// A rename must produce exactly one RenameColumn, never an add and a drop
    /// alongside it.
    #[test]
    fn renaming_produces_exactly_one_change() {
        let base = schema_of("dbo.t", table(&[("old", Column::new(ty("int")))]));
        let want = schema_of("dbo.t", table(&[("new", Column::new(ty("int")))]));
        let intents = vec![Intent::RenameColumn {
            table: "dbo.t".parse().unwrap(),
            from: "old".into(),
            to: "new".into(),
        }];
        let cs = run(&base, &want, &intents);

        assert_eq!(kinds(&cs), ["RenameColumn"]);
        assert!(cs.risks().contains(&RiskClass::Rename));
    }

    /// A rename plus a type change: with identity mapped correctly, the type
    /// comparison must be against the old column.
    #[test]
    fn rename_and_retype_are_both_detected() {
        let base = schema_of("dbo.t", table(&[("old", Column::new(ty("nvarchar(100)")))]));
        let want = schema_of("dbo.t", table(&[("new", Column::new(ty("nvarchar(50)")))]));
        let intents = vec![Intent::RenameColumn {
            table: "dbo.t".parse().unwrap(),
            from: "old".into(),
            to: "new".into(),
        }];
        let cs = run(&base, &want, &intents);

        assert_eq!(kinds(&cs), ["RenameColumn", "AlterColumnType"]);
        assert!(cs.risks().contains(&RiskClass::Narrowing));
    }

    /// A new table's columns ride along in CreateTable; no per-column AddColumn
    /// should be emitted as well.
    #[test]
    fn new_table_does_not_also_emit_add_column() {
        let base = Schema::default();
        let want = schema_of(
            "dbo.t",
            table(&[("a", Column::new(ty("int"))), ("b", Column::new(ty("int")))]),
        );
        assert_eq!(kinds(&run(&base, &want, &[])), ["CreateTable"]);
    }

    #[test]
    fn changed_index_becomes_drop_then_add() {
        let ix = |c: &str| Index {
            columns: vec![IndexColumn {
                name: c.into(),
                descending: false,
            }],
            include: vec![],
            unique: false,
            filter: None,
        };
        let mut base_t = table(&[("a", Column::new(ty("int"))), ("b", Column::new(ty("int")))]);
        base_t.indexes.insert("ix_t".into(), ix("a"));
        let mut want_t = base_t.clone();
        want_t.indexes.insert("ix_t".into(), ix("b"));

        let cs = run(
            &schema_of("dbo.t", base_t),
            &schema_of("dbo.t", want_t),
            &[],
        );
        assert_eq!(kinds(&cs), ["DropIndex", "AddIndex"]);
    }

    /// The order has to be safely executable: renames first, dropping constraints
    /// before dropping columns, adding constraints last.
    #[test]
    fn changes_are_ordered_for_execution() {
        let mut base_t = table(&[
            ("old", Column::new(ty("int"))),
            ("doomed", Column::new(ty("int"))),
        ]);
        base_t.indexes.insert(
            "ix_doomed".into(),
            Index {
                columns: vec![IndexColumn {
                    name: "doomed".into(),
                    descending: false,
                }],
                include: vec![],
                unique: false,
                filter: None,
            },
        );
        let want_t = table(&[("new", Column::new(ty("int")))]);

        let intents = vec![
            Intent::RenameColumn {
                table: "dbo.t".parse().unwrap(),
                from: "old".into(),
                to: "new".into(),
            },
            Intent::DropColumn {
                column: "dbo.t.doomed".parse().unwrap(),
                reason: "no longer in use".into(),
            },
        ];
        let cs = run(
            &schema_of("dbo.t", base_t),
            &schema_of("dbo.t", want_t),
            &intents,
        );

        assert_eq!(
            kinds(&cs),
            ["RenameColumn", "DropIndex", "DropColumn"],
            "an index must be dropped before the column it references"
        );
    }

    /// description affects documentation, not structure, so Phase 1 deliberately
    /// produces no change for it.
    #[test]
    fn description_change_is_not_a_structural_change() {
        let base = schema_of("dbo.t", table(&[("a", Column::new(ty("int")))]));
        let mut c = Column::new(ty("int"));
        c.description = Some("Customer identifier".into());
        let want = schema_of("dbo.t", table(&[("a", c)]));
        assert!(run(&base, &want, &[]).is_empty());
    }

    #[test]
    fn deprecating_a_column_is_a_change_but_not_a_risk() {
        let base = schema_of("dbo.t", table(&[("a", Column::new(ty("int")))]));
        let mut c = Column::new(ty("int"));
        c.deprecated = Some("superseded by email".into());
        let want = schema_of("dbo.t", table(&[("a", c)]));
        let cs = run(&base, &want, &[]);

        assert_eq!(kinds(&cs), ["SetColumnDeprecated"]);
        assert!(cs.risks().is_empty());
    }

    /// IDENTITY cannot be modified with ALTER, so it must be blocked explicitly
    /// rather than emitting invalid SQL.
    #[test]
    fn identity_change_is_rejected() {
        let base = schema_of("dbo.t", table(&[("a", Column::new(ty("int")))]));
        let mut c = Column::new(ty("int"));
        c.identity = Some(pbps_model::Identity {
            seed: 1,
            increment: 1,
        });
        let want = schema_of("dbo.t", table(&[("a", c)]));

        let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let declared_ids = crate::resolve(&want, &base_ids, &[], &ctx()).unwrap().ids;
        let err = diff(
            Side {
                schema: &base,
                ids: &base_ids,
            },
            Side {
                schema: &want,
                ids: &declared_ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .unwrap_err();
        assert!(matches!(
            err[0],
            DiffError::IdentityChangeUnsupported { .. }
        ));
    }

    /// A jump-version deploy: the entire reason uid matching exists.
    ///
    /// Prod sits at v1 while the declarations have moved on to v3, with a rename
    /// somewhere in between. That intent left the working tree long ago — but both
    /// identity files still record the same uid, so the rename is still detected,
    /// in one step, with no need to walk the v1→v2→v3 chain of names.
    #[test]
    fn a_rename_is_detected_across_many_versions() {
        let v1 = schema_of(
            "dbo.t",
            table(&[("customer_name", Column::new(ty("nvarchar(50)")))]),
        );
        let v1_ids = crate::resolve(&v1, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;

        // v2: the rename. The intent exists only in this version.
        let v2 = schema_of(
            "dbo.t",
            table(&[("full_name", Column::new(ty("nvarchar(50)")))]),
        );
        let v2_ids = crate::resolve(
            &v2,
            &v1_ids,
            &[Intent::RenameColumn {
                table: "dbo.t".parse().unwrap(),
                from: "customer_name".into(),
                to: "full_name".into(),
            }],
            &ctx(),
        )
        .unwrap()
        .ids;

        // v3: just a longer column. No intent at all.
        let v3 = schema_of(
            "dbo.t",
            table(&[("full_name", Column::new(ty("nvarchar(200)")))]),
        );
        let v3_ids = crate::resolve(&v3, &v2_ids, &[], &ctx()).unwrap().ids;

        // Apply v3 to an environment stuck at v1, supplying no intent.
        let cs = diff(
            Side {
                schema: &v1,
                ids: &v1_ids,
            },
            Side {
                schema: &v3,
                ids: &v3_ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .unwrap();

        assert_eq!(
            kinds(&cs),
            ["RenameColumn", "AlterColumnType"],
            "across versions a rename must still read as a rename, not a drop plus an add"
        );
        assert!(
            !cs.risks().contains(&RiskClass::Destructive),
            "this must never turn into a data-losing plan"
        );
    }

    /// Identical input must produce a plan in the same order, or the plan's
    /// checksum would be unstable.
    #[test]
    fn output_is_deterministic() {
        let base = schema_of(
            "dbo.t",
            table(&[("a", Column::new(ty("int"))), ("b", Column::new(ty("int")))]),
        );
        let want = schema_of(
            "dbo.t",
            table(&[
                ("a", Column::new(ty("bigint"))),
                ("b", Column::new(ty("bigint"))),
                ("c", Column::new(ty("int"))),
            ]),
        );
        let first = run(&base, &want, &[]);
        for _ in 0..10 {
            let again = run(&base, &want, &[]);
            assert_eq!(
                kinds(&first),
                kinds(&again),
                "identical input should produce the plan in the same order"
            );
        }
        let _ = Uid::generate(pbps_model::UidKind::Column);
    }

    // ---- modules (ADR-0002) ----

    fn a_module(kind: pbps_model::ModuleKind, definition: &str) -> pbps_model::Module {
        pbps_model::Module {
            kind,
            description: None,
            on: None,
            definition: definition.to_owned(),
        }
    }

    fn with_modules(mut schema: Schema, specs: &[(&str, &str)]) -> Schema {
        for (name, definition) in specs {
            schema.modules.insert(
                name.parse().unwrap(),
                a_module(pbps_model::ModuleKind::View, definition),
            );
        }
        schema
    }

    /// Modules are matched by name and carry no identity, so `run`'s uid
    /// machinery is bypassed: this calls the differ directly with the same ids
    /// on both sides.
    fn module_diff(base: &Schema, declared: &Schema) -> ChangeSet {
        let ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        diff(
            Side {
                schema: base,
                ids: &ids,
            },
            Side {
                schema: declared,
                ids: &ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .unwrap()
    }

    #[test]
    fn a_new_module_is_created_and_a_removed_one_dropped() {
        let base = Schema::default();
        let declared = with_modules(Schema::default(), &[("dbo.v", "SELECT 1")]);
        assert_eq!(kinds(&module_diff(&base, &declared)), ["CreateModule"]);

        let dropped = module_diff(&declared, &base);
        assert_eq!(kinds(&dropped), ["DropModule"]);
        // What a dropped module destroys is the validity of its dependents, so
        // it faces the gate — with no tombstone and no reason, because git
        // holds the definition it had.
        assert!(dropped.risks().contains(&RiskClass::Destructive));
    }

    /// Layout is not a change. Re-stating every view on every deploy would
    /// train reviewers to skim the plan, which is the one thing a plan must not
    /// invite.
    #[test]
    fn a_reindented_definition_is_not_a_change() {
        let base = with_modules(Schema::default(), &[("dbo.v", "SELECT a, b FROM t")]);
        let declared = with_modules(
            Schema::default(),
            &[("dbo.v", "SELECT a,\n       b\nFROM t")],
        );
        assert!(module_diff(&base, &declared).is_empty());
    }

    #[test]
    fn a_changed_definition_is_restated_in_full() {
        let base = with_modules(Schema::default(), &[("dbo.v", "SELECT a FROM t")]);
        let declared = with_modules(Schema::default(), &[("dbo.v", "SELECT a, b FROM t")]);
        let cs = module_diff(&base, &declared);
        assert_eq!(kinds(&cs), ["AlterModule"]);
        // CREATE OR ALTER preserves the permissions granted on the object, so
        // an alteration must never be planned as drop + create.
        assert!(cs.risks().is_empty());
    }

    /// `CREATE OR ALTER` cannot turn a view into a procedure, nor move a
    /// trigger to another table. Planning either as an alteration would fail at
    /// the statement, halfway through an apply.
    #[test]
    fn a_changed_kind_or_table_becomes_drop_plus_create() {
        let base = with_modules(Schema::default(), &[("dbo.thing", "SELECT 1")]);
        let mut declared = Schema::default();
        declared.modules.insert(
            "dbo.thing".parse().unwrap(),
            a_module(pbps_model::ModuleKind::Procedure, "AS SELECT 1"),
        );
        assert_eq!(
            kinds(&module_diff(&base, &declared)),
            ["DropModule", "CreateModule"]
        );
    }

    /// A view over a view has to be created second and dropped first, or the
    /// statement fails. The scan of the definition text is what produces the
    /// order; §8.2's "never parse" is about comparison, not about this.
    #[test]
    fn modules_are_ordered_by_what_they_reference() {
        let declared = with_modules(
            Schema::default(),
            &[
                ("dbo.top", "SELECT * FROM dbo.middle"),
                ("dbo.middle", "SELECT * FROM dbo.base"),
                ("dbo.base", "SELECT 1"),
            ],
        );
        let created: Vec<String> = module_diff(&Schema::default(), &declared)
            .changes
            .iter()
            .map(|p| p.change.table().to_string())
            .collect();
        assert_eq!(created, ["dbo.base", "dbo.middle", "dbo.top"]);

        let dropped: Vec<String> = module_diff(&declared, &Schema::default())
            .changes
            .iter()
            .map(|p| p.change.table().to_string())
            .collect();
        assert_eq!(dropped, ["dbo.top", "dbo.middle", "dbo.base"]);
    }

    /// A module that is going must go before the table changes: a SCHEMABINDING
    /// view blocks a rename of the column it binds. And one that is arriving
    /// must come after them, or it selects a column that does not exist yet.
    #[test]
    fn modules_bracket_the_table_changes() {
        let base = with_modules(
            schema_of("dbo.t", table(&[("a", Column::new(ty("int")))])),
            &[("dbo.going", "SELECT a FROM dbo.t")],
        );
        let declared = with_modules(
            schema_of(
                "dbo.t",
                table(&[("a", Column::new(ty("int"))), ("b", Column::new(ty("int")))]),
            ),
            &[("dbo.arriving", "SELECT a, b FROM dbo.t")],
        );
        let base_ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let declared_ids = crate::resolve(&declared, &base_ids, &[], &ctx())
            .unwrap()
            .ids;
        let cs = diff(
            Side {
                schema: &base,
                ids: &base_ids,
            },
            Side {
                schema: &declared,
                ids: &declared_ids,
            },
            &MinimalDialect,
            &Hints::default(),
        )
        .unwrap();
        assert_eq!(kinds(&cs), ["DropModule", "AddColumn", "CreateModule"]);
    }
}
