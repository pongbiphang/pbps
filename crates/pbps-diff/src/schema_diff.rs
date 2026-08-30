//! 變更計畫：比對兩個「狀態 + 身份」組合，產出變更集。
//!
//! # 為什麼兩邊都要帶身份檔
//!
//! 配對「哪個欄位是哪個欄位」不能靠名稱 —— 改名會讓名稱在兩邊對不上。也不能
//! 靠本次的意圖註記，因為部署可能落後很多版：prod 停在 v1、宣告已經到 v5 時，
//! 當初那則改名意圖早就不在工作區裡了。
//!
//! 因此兩邊各自帶著自己的身份檔，用 **uid** 配對：基準的 ids 說
//! `c_x → customer_name`、宣告的 ids 說 `c_x → full_name`，一比就是改名，
//! 一步到位，不需要沿著名稱鏈逐版回推（SPEC §4.2 要解決的正是這件事）。
//!
//! # 基準是誰
//!
//! `base` 可能來自版控中的前一版（離線預覽），也可能來自資料庫實查
//! （Phase 3 的權威計畫）。這一層不在意來源，只做純粹的比對。

use std::collections::BTreeMap;

use pbps_dialect::Dialect;
use pbps_model::{
    Change, ChangeSet, ColumnRef, ColumnType, IdsFile, PlannedChange, Schema, Table, TableName, Uid,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiffError {
    /// IDENTITY 在多數資料庫中無法用 ALTER 修改，必須重建整張表。
    /// 這超出宣告式自動化的範圍，得由人決定遷移策略。
    #[error("{column} 的 IDENTITY 屬性改變了，但 IDENTITY 無法用 ALTER 修改")]
    IdentityChangeUnsupported { column: ColumnRef },
}

/// 一側的完整輸入：狀態加上它自己的身份對照。
#[derive(Debug, Clone, Copy)]
pub struct Side<'a> {
    pub schema: &'a Schema,
    pub ids: &'a IdsFile,
}

/// 比對基準與宣告，產出變更集。
///
/// 目前**不比對 `description`**：它只影響資料目錄的說明文字，不影響結構，
/// 寫入 extended property 屬於 Phase 5。
pub fn diff(
    base: Side<'_>,
    declared: Side<'_>,
    dialect: &dyn Dialect,
) -> Result<ChangeSet, Vec<DiffError>> {
    let mut changes = Vec::new();
    let mut errs = Vec::new();

    let base_tables = &base.ids.tables;
    let declared_tables = &declared.ids.tables;

    // 表只在基準 → 刪除
    for (uid, name) in base_tables {
        if !declared_tables.contains_key(uid) {
            changes.push(Change::DropTable {
                uid: uid.clone(),
                name: name.clone(),
            });
        }
    }

    // 表只在宣告 → 新建
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

    // 表兩邊都在 → 可能改名，並且要比內容
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

/// 屬於某張仍存在的表、且兩邊都有的欄位：比屬性。
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
            // 只在宣告側 → 新增欄位
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

    // 只在基準側 → 刪除欄位
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

/// 套用順序。
///
/// 改名排最前面，讓後續步驟都能用現行名稱；約束與索引的移除要早於欄位移除
/// （它們可能參照到那些欄位），新增則要晚於欄位新增。
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

    /// 重現真實流程：基準側的身份檔來自那一版，宣告側的身份檔是解析後的結果。
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

    /// 跳版部署：這是整個 uid 配對設計的理由。
    ///
    /// prod 停在 v1，宣告已經演進到 v3，中間發生過改名。當初那則意圖早就不在
    /// 工作區裡了 —— 但兩邊的身份檔都記著同一個 uid，所以改名依然判得出來，
    /// 而且是一步到位，不需要沿著 v1→v2→v3 的名稱鏈回推。
    #[test]
    fn a_rename_is_detected_across_many_versions() {
        let v1 = schema_of(
            "dbo.t",
            table(&[("customer_name", Column::new(ty("nvarchar(50)")))]),
        );
        let v1_ids = crate::resolve(&v1, &IdsFile::default(), &[], &ctx())
            .unwrap()
            .ids;

        // v2：改名。意圖只在這一版存在。
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

        // v3：只是加長欄位。沒有任何意圖。
        let v3 = schema_of(
            "dbo.t",
            table(&[("full_name", Column::new(ty("nvarchar(200)")))]),
        );
        let v3_ids = crate::resolve(&v3, &v2_ids, &[], &ctx()).unwrap().ids;

        // 對停在 v1 的環境套用 v3：不提供任何意圖。
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
            "跳版時改名仍須被判定為改名，而不是刪除加新增"
        );
        assert!(
            !cs.risks().contains(&RiskClass::Destructive),
            "絕不能變成掉資料的計畫"
        );
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
