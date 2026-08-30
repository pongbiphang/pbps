//! 屬性比對：在身份已經對應好之後，找出同一個東西的哪些屬性變了。
//!
//! # 基準是誰
//!
//! `base` 是「目前實際上長什麼樣」。它可能來自資料庫實查（Phase 3），也可能
//! 來自版控中的前一版宣告（離線預覽）。這一層不在意來源，只做純粹的比對 ——
//! 這正是它能同時服務兩種情境的原因。
//!
//! # 為什麼身份要先解析好
//!
//! 「同一個東西」不能靠名稱判斷，改名會讓名稱在兩邊對不上。因此本函式吃的是
//! [`Resolution`]，由它提供 uid 層級的對應，這裡只負責比屬性。

use std::collections::{BTreeMap, BTreeSet};

use pbps_dialect::Dialect;
use pbps_model::{Change, ChangeSet, ColumnRef, PlannedChange, Schema, Table, TableName};

use crate::identity::Resolution;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiffError {
    /// IDENTITY 屬性在多數資料庫中無法用 ALTER 修改，必須重建整張表。
    /// 這超出宣告式自動化的範圍，得由人決定遷移策略。
    #[error("{column} 的 IDENTITY 屬性改變了，但 IDENTITY 無法用 ALTER 修改")]
    IdentityChangeUnsupported { column: ColumnRef },
}

/// 比對兩份 schema，產出變更集。
///
/// 目前**不比對 `description`**：它只影響資料目錄的說明文字，不影響結構，
/// 而寫入 extended property 是 Phase 5 的範圍。
pub fn diff(
    base: &Schema,
    declared: &Schema,
    res: &Resolution,
    dialect: &dyn Dialect,
) -> Result<ChangeSet, Vec<DiffError>> {
    let mut changes = Vec::new();
    let mut errs = Vec::new();

    let created: BTreeSet<&TableName> = res.created_tables.iter().map(|(_, n)| n).collect();
    let dropped: BTreeSet<&TableName> = res.dropped_tables.iter().map(|(_, n)| n).collect();

    for (uid, from, to) in &res.renamed_tables {
        changes.push(Change::RenameTable {
            uid: uid.clone(),
            from: from.clone(),
            to: to.clone(),
        });
    }

    for (uid, name) in &res.created_tables {
        if let Some(table) = declared.tables.get(name) {
            changes.push(Change::CreateTable {
                uid: uid.clone(),
                name: name.clone(),
                table: Box::new(table.clone()),
            });
        }
    }

    for (uid, name) in &res.dropped_tables {
        changes.push(Change::DropTable {
            uid: uid.clone(),
            name: name.clone(),
        });
    }

    for (uid, from, to) in &res.renamed_columns {
        changes.push(Change::RenameColumn {
            uid: uid.clone(),
            table: to.table.clone(),
            from: from.name.clone(),
            to: to.name.clone(),
        });
    }

    // 新建表的欄位已包含在 CreateTable 裡，不再重複；
    // 已刪除表的欄位同理由 DropTable 帶走。
    for (uid, col) in &res.added_columns {
        if created.contains(&col.table) {
            continue;
        }
        if let Some(c) = declared
            .tables
            .get(&col.table)
            .and_then(|t| t.columns.get(&col.name))
        {
            changes.push(Change::AddColumn {
                uid: uid.clone(),
                table: col.table.clone(),
                name: col.name.clone(),
                column: Box::new(c.clone()),
            });
        }
    }

    for (uid, col) in &res.dropped_columns {
        if dropped.contains(&col.table) {
            continue;
        }
        changes.push(Change::DropColumn {
            uid: uid.clone(),
            column: col.clone(),
        });
    }

    // 改名對照：宣告檔中的欄位 → 它在基準中的名稱
    let renamed_back: BTreeMap<&ColumnRef, &ColumnRef> = res
        .renamed_columns
        .iter()
        .map(|(_, from, to)| (to, from))
        .collect();
    let table_renamed_back: BTreeMap<&TableName, &TableName> = res
        .renamed_tables
        .iter()
        .map(|(_, from, to)| (to, from))
        .collect();
    let added: BTreeSet<&ColumnRef> = res.added_columns.iter().map(|(_, c)| c).collect();

    for (name, table) in &declared.tables {
        if created.contains(name) {
            continue;
        }
        let base_name = table_renamed_back.get(name).copied().unwrap_or(name);
        let Some(base_table) = base.tables.get(base_name) else {
            continue;
        };

        diff_columns(
            name,
            base_name,
            base_table,
            table,
            &renamed_back,
            &added,
            res,
            dialect,
            &mut changes,
            &mut errs,
        );
        diff_constraints(name, base_table, table, &mut changes);
    }

    if !errs.is_empty() {
        return Err(errs);
    }

    let mut planned: Vec<PlannedChange> = changes.into_iter().map(PlannedChange::new).collect();
    // 加上需要方言知識才能判定的風險
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

#[allow(clippy::too_many_arguments)]
fn diff_columns(
    name: &TableName,
    base_name: &TableName,
    base_table: &Table,
    table: &Table,
    renamed_back: &BTreeMap<&ColumnRef, &ColumnRef>,
    added: &BTreeSet<&ColumnRef>,
    res: &Resolution,
    dialect: &dyn Dialect,
    changes: &mut Vec<Change>,
    errs: &mut Vec<DiffError>,
) {
    for (col_name, col) in &table.columns {
        let ref_new = name.column(col_name);
        if added.contains(&ref_new) {
            continue;
        }
        let base_col_name = renamed_back
            .get(&ref_new)
            .map(|r| r.name.clone())
            .unwrap_or_else(|| col_name.clone());
        let Some(base_col) = base_table.columns.get(&base_col_name) else {
            continue;
        };

        let uid = match res.ids.column_uid(&ref_new) {
            Some(u) => u.clone(),
            None => continue,
        };

        if base_col.identity != col.identity {
            errs.push(DiffError::IdentityChangeUnsupported {
                column: ref_new.clone(),
            });
        }

        let normalised =
            |t: &pbps_model::ColumnType| dialect.normalize_type(t).unwrap_or_else(|_| t.clone());
        let (from_ty, to_ty) = (normalised(&base_col.ty), normalised(&col.ty));
        if from_ty != to_ty {
            changes.push(Change::AlterColumnType {
                uid: uid.clone(),
                column: ref_new.clone(),
                from: from_ty,
                to: to_ty,
            });
        }

        if base_col.nullable != col.nullable {
            changes.push(Change::AlterColumnNullability {
                uid: uid.clone(),
                column: ref_new.clone(),
                to_nullable: col.nullable,
            });
        }

        if base_col.default != col.default {
            changes.push(Change::AlterColumnDefault {
                uid: uid.clone(),
                column: ref_new.clone(),
                from: base_col.default.clone(),
                to: col.default.clone(),
            });
        }

        if base_col.deprecated != col.deprecated {
            changes.push(Change::SetColumnDeprecated {
                uid,
                column: ref_new.clone(),
                reason: col.deprecated.clone(),
            });
        }
    }
    let _ = base_name;
}

/// 約束與索引一律以名稱比對，且不做原地修改 —— 資料庫本身也是 drop + add，
/// 假裝可以原地改只會讓 emitter 多一條會出錯的路徑。
fn diff_constraints(name: &TableName, base: &Table, declared: &Table, changes: &mut Vec<Change>) {
    if base.primary_key != declared.primary_key {
        changes.push(Change::SetPrimaryKey {
            table: name.clone(),
            from: base.primary_key.clone(),
            to: declared.primary_key.clone(),
        });
    }

    for (n, c) in &declared.unique {
        if base.unique.get(n) != Some(c) {
            if base.unique.contains_key(n) {
                changes.push(Change::DropUnique {
                    table: name.clone(),
                    name: n.clone(),
                });
            }
            changes.push(Change::AddUnique {
                table: name.clone(),
                name: n.clone(),
                constraint: c.clone(),
            });
        }
    }
    for n in base
        .unique
        .keys()
        .filter(|n| !declared.unique.contains_key(*n))
    {
        changes.push(Change::DropUnique {
            table: name.clone(),
            name: n.clone(),
        });
    }

    for (n, c) in &declared.foreign_keys {
        if base.foreign_keys.get(n) != Some(c) {
            if base.foreign_keys.contains_key(n) {
                changes.push(Change::DropForeignKey {
                    table: name.clone(),
                    name: n.clone(),
                });
            }
            changes.push(Change::AddForeignKey {
                table: name.clone(),
                name: n.clone(),
                constraint: Box::new(c.clone()),
            });
        }
    }
    for n in base
        .foreign_keys
        .keys()
        .filter(|n| !declared.foreign_keys.contains_key(*n))
    {
        changes.push(Change::DropForeignKey {
            table: name.clone(),
            name: n.clone(),
        });
    }

    for (n, c) in &declared.checks {
        if base.checks.get(n) != Some(c) {
            if base.checks.contains_key(n) {
                changes.push(Change::DropCheck {
                    table: name.clone(),
                    name: n.clone(),
                });
            }
            changes.push(Change::AddCheck {
                table: name.clone(),
                name: n.clone(),
                constraint: c.clone(),
            });
        }
    }
    for n in base
        .checks
        .keys()
        .filter(|n| !declared.checks.contains_key(*n))
    {
        changes.push(Change::DropCheck {
            table: name.clone(),
            name: n.clone(),
        });
    }

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

/// 套用順序。
///
/// 改名排最前面，讓後續所有步驟都能用現行名稱；約束與索引的移除要早於欄位
/// 移除（它們可能參照到那些欄位），而新增則要晚於欄位新增。
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

    /// 建立基準：先讓身份檔認識 `base`，再對 `declared` 解析身份、比對屬性。
    fn run(base: &Schema, declared: &Schema, intents: &[Intent]) -> ChangeSet {
        let ids = crate::resolve(base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let res = crate::resolve(declared, &ids, intents, &ctx()).unwrap();
        diff(base, declared, &res, &MinimalDialect).unwrap()
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
        assert!(cs.risks().is_empty(), "放寬長度不需要放行");
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

    /// 大小寫不同不是變更，否則每次重新輸入型別都會產生假的 diff。
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
            reason: "不再使用".into(),
        }];
        let cs = run(&base, &want, &intents);

        assert_eq!(kinds(&cs), ["DropColumn"]);
        assert!(cs.risks().contains(&RiskClass::Destructive));
    }

    /// 改名只能產生一個 RenameColumn，不能同時冒出新增與刪除。
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

    /// 改名同時改型別：身份對應正確的話，型別比對要拿舊欄位來比。
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

    /// 新表的欄位由 CreateTable 帶著，不該再逐欄產生 AddColumn。
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

    /// 順序要能安全執行：改名最先，移除約束早於移除欄位，新增約束最後。
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
                reason: "不再使用".into(),
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
            "索引必須在它參照的欄位之前被移除"
        );
    }

    /// description 只影響文件，不影響結構，Phase 1 刻意不產生變更。
    #[test]
    fn description_change_is_not_a_structural_change() {
        let base = schema_of("dbo.t", table(&[("a", Column::new(ty("int")))]));
        let mut c = Column::new(ty("int"));
        c.description = Some("客戶編號".into());
        let want = schema_of("dbo.t", table(&[("a", c)]));
        assert!(run(&base, &want, &[]).is_empty());
    }

    #[test]
    fn deprecating_a_column_is_a_change_but_not_a_risk() {
        let base = schema_of("dbo.t", table(&[("a", Column::new(ty("int")))]));
        let mut c = Column::new(ty("int"));
        c.deprecated = Some("改用 email".into());
        let want = schema_of("dbo.t", table(&[("a", c)]));
        let cs = run(&base, &want, &[]);

        assert_eq!(kinds(&cs), ["SetColumnDeprecated"]);
        assert!(cs.risks().is_empty());
    }

    /// IDENTITY 無法用 ALTER 修改，必須明確擋下而不是產生無效的 SQL。
    #[test]
    fn identity_change_is_rejected() {
        let base = schema_of("dbo.t", table(&[("a", Column::new(ty("int")))]));
        let mut c = Column::new(ty("int"));
        c.identity = Some(pbps_model::Identity {
            seed: 1,
            increment: 1,
        });
        let want = schema_of("dbo.t", table(&[("a", c)]));

        let ids = crate::resolve(&base, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;
        let res = crate::resolve(&want, &ids, &[], &ctx()).unwrap();
        let err = diff(&base, &want, &res, &MinimalDialect).unwrap_err();
        assert!(matches!(
            err[0],
            DiffError::IdentityChangeUnsupported { .. }
        ));
    }

    /// 同樣的輸入必須產生同樣順序的計畫，否則 plan 的 checksum 會不穩定。
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
            assert_eq!(kinds(&first), kinds(&again), "同樣輸入應產生同樣順序的計畫");
        }
        let _ = Uid::generate(pbps_model::UidKind::Column);
    }
}
