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
    Change, ChangeSet, ColumnRef, ColumnType, IdsFile, PlannedChange, Schema, Table, TableName, Uid,
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
pub fn diff(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &dyn Dialect,
) -> Result<ChangeSet, Vec<DiffError>> {
    let mut changes = Vec::new();
    let mut errs = Vec::new();

    let base_tables = &base.ids.tables;
    let declared_tables = &declared.ids.tables;

    // Table present only in the base: dropped.
    for (uid, name) in base_tables {
        if !declared_tables.contains_key(uid) {
            changes.push(Change::DropTable {
                uid: uid.clone(),
                name: name.clone(),
            });
        }
    }

    // Table present only in the declarations: created.
    for (uid, name) in declared_tables {
        if !base_tables.contains_key(uid)
            && let Some(t) = declared.schema.tables.get(name)
        {
            changes.push(Change::CreateTable {
                uid: uid.clone(),
                name: name.clone(),
                table: Box::new(t.clone()),
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
    }
    planned.sort_by_key(|p| (order_key(&p.change), format!("{:?}", p.change)));
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
        if from_ty != to_ty {
            changes.push(Change::AlterColumnType {
                uid: uid.clone(),
                column: declared_ref.clone(),
                from: from_ty,
                to: to_ty,
            });
        }
        if base_col.nullable != col.nullable {
            changes.push(Change::AlterColumnNullability {
                uid: uid.clone(),
                column: declared_ref.clone(),
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

/// The order of application.
///
/// Renames come first, so every later step can use current names. Dropping
/// constraints and indexes must precede dropping columns, since they may
/// reference those columns; adding them must follow adding columns.
fn order_key(c: &Change) -> u8 {
    match c {
        Change::RenameTable { .. } | Change::RenameColumn { .. } => 0,
        Change::DropIndex { .. }
        | Change::DropUnique { .. }
        | Change::DropForeignKey { .. }
        | Change::DropCheck { .. } => 1,
        Change::DropColumn { .. } => 2,
        Change::DropTable { .. } => 3,
        Change::CreateTable { .. } => 4,
        Change::AddColumn { .. } => 5,
        Change::AlterColumnType { .. }
        | Change::AlterColumnNullability { .. }
        | Change::AlterColumnDefault { .. } => 6,
        Change::SetColumnDeprecated { .. } => 7,
        Change::SetPrimaryKey { .. }
        | Change::AddUnique { .. }
        | Change::AddForeignKey { .. }
        | Change::AddCheck { .. }
        | Change::AddIndex { .. } => 8,
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
}
