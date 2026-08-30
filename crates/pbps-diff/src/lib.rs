//! 宣告狀態的比對。
//!
//! 目前包含**身份解析**（[`identity`]）：把宣告檔中的名稱對應回 UID，判定
//! 哪些是新增、哪些是改名、哪些是刪除，以及哪些情況必須由人裁決。
//!
//! 這一層不產生 SQL，也不接觸資料庫（CLAUDE.md 約束 3）。

pub mod identity;
pub mod schema_diff;

pub use identity::{Blocker, Context, Resolution, resolve};
pub use schema_diff::{DiffError, Side, diff};

#[cfg(test)]
// 測試中用 catch-all 搭配 panic 來表達「不該走到這裡」是恰當的；
// 這條 lint 的價值在產品程式碼的窮舉性（新增 Change 變體時強制處理）。
#[allow(clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use pbps_model::{Column, ColumnType, IdsFile, Intent, Schema, Table, TableName};

    fn ctx() -> Context {
        Context {
            operator: "leon".into(),
            today: "2026-08-30".into(),
        }
    }

    fn t(s: &str) -> TableName {
        s.parse().unwrap()
    }

    /// 用 `表名 -> [欄位名]` 快速造一個 schema。型別一律 int，因為身份解析
    /// 不看屬性。
    fn schema(spec: &[(&str, &[&str])]) -> Schema {
        let mut s = Schema::default();
        for (name, cols) in spec {
            let mut columns = IndexMap::new();
            for c in *cols {
                columns.insert(
                    (*c).to_string(),
                    Column::new("int".parse::<ColumnType>().unwrap()),
                );
            }
            s.tables.insert(
                t(name),
                Table {
                    columns,
                    ..Default::default()
                },
            );
        }
        s
    }

    /// 先把一份 schema 登記進身份檔，作為「上次已知狀態」。
    fn baseline(spec: &[(&str, &[&str])]) -> (Schema, IdsFile) {
        let s = schema(spec);
        let r = resolve(&s, &IdsFile::default(), &[], &ctx()).unwrap();
        (s, r.ids)
    }

    // ---- 第一次執行 ----

    #[test]
    fn first_run_creates_everything() {
        let s = schema(&[("dbo.customer", &["id", "email"])]);
        let r = resolve(&s, &IdsFile::default(), &[], &ctx()).unwrap();

        assert_eq!(r.created_tables.len(), 1);
        assert_eq!(r.added_columns.len(), 2);
        assert!(r.dropped_columns.is_empty());
        assert_eq!(r.ids.tables.len(), 1);
        assert_eq!(r.ids.columns.len(), 2);
        r.ids.validate().unwrap();
    }

    /// 沒有變化時不該產生任何身份層級的動作 —— 否則每次執行都會有假的變更。
    #[test]
    fn unchanged_schema_produces_nothing() {
        let (s, ids) = baseline(&[("dbo.customer", &["id", "email"])]);
        let r = resolve(&s, &ids, &[], &ctx()).unwrap();

        assert_eq!(
            r,
            Resolution {
                ids: ids.clone(),
                ..Default::default()
            }
        );
        assert_eq!(r.ids, ids, "身份檔不應被無謂地改寫");
    }

    // ---- 新增與刪除 ----

    #[test]
    fn pure_addition_needs_no_intent() {
        let (_, ids) = baseline(&[("dbo.customer", &["id"])]);
        let s = schema(&[("dbo.customer", &["id", "mobile"])]);
        let r = resolve(&s, &ids, &[], &ctx()).unwrap();

        assert_eq!(r.added_columns.len(), 1);
        assert_eq!(r.added_columns[0].1.name, "mobile");
    }

    /// 刪除在操作上沒有歧義，但墓碑要回答「為什麼」，那是演算法生不出來的。
    #[test]
    fn deletion_without_a_reason_is_blocked() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "legacy"])]);
        let s = schema(&[("dbo.customer", &["id"])]);
        let errs = resolve(&s, &ids, &[], &ctx()).unwrap_err();

        assert_eq!(errs.len(), 1);
        assert!(matches!(
            &errs[0],
            Blocker::DropColumnNeedsReason { column } if column.name == "legacy"
        ));
    }

    #[test]
    fn deletion_with_a_reason_produces_a_tombstone() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "national_id"])]);
        let s = schema(&[("dbo.customer", &["id"])]);
        let intent = Intent::DropColumn {
            column: "dbo.customer.national_id".parse().unwrap(),
            reason: "REG-2026-042 PII 刪除要求".into(),
        };
        let r = resolve(&s, &ids, &[intent], &ctx()).unwrap();

        assert_eq!(r.dropped_columns.len(), 1);
        assert_eq!(r.ids.tombstones.len(), 1);
        let tomb = r.ids.tombstones.values().next().unwrap();
        assert_eq!(tomb.was, "dbo.customer.national_id");
        assert_eq!(tomb.reason, "REG-2026-042 PII 刪除要求");
        assert_eq!(tomb.operator, "leon");
        assert_eq!(tomb.dropped_at, "2026-08-30");
        r.ids.validate().unwrap();
    }

    /// 墓碑留在身份檔，宣告檔因此永遠只有「你要的東西」，不會累積殭屍欄位。
    #[test]
    fn tombstoned_column_does_not_reappear_as_a_change() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "national_id"])]);
        let s = schema(&[("dbo.customer", &["id"])]);
        let intent = Intent::DropColumn {
            column: "dbo.customer.national_id".parse().unwrap(),
            reason: "REG-1".into(),
        };
        let after = resolve(&s, &ids, &[intent], &ctx()).unwrap().ids;

        let again = resolve(&s, &after, &[], &ctx()).unwrap();
        assert!(again.dropped_columns.is_empty());
        assert!(again.added_columns.is_empty());
    }

    // ---- 改名 ----

    /// 這是整個工具存在的理由：沒有意圖時，絕不猜。
    #[test]
    fn rename_without_intent_is_ambiguous() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "customer_name"])]);
        let s = schema(&[("dbo.customer", &["id", "full_name"])]);
        let errs = resolve(&s, &ids, &[], &ctx()).unwrap_err();

        assert_eq!(errs.len(), 1);
        match &errs[0] {
            Blocker::AmbiguousColumns {
                table,
                disappeared,
                appeared,
            } => {
                assert_eq!(table, &t("dbo.customer"));
                assert_eq!(disappeared, &["customer_name"]);
                assert_eq!(appeared, &["full_name"]);
            }
            other => panic!("預期欄位歧義，得到 {other:?}"),
        }
    }

    #[test]
    fn rename_with_intent_preserves_the_uid() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "customer_name"])]);
        let before_uid = ids
            .column_uid(&"dbo.customer.customer_name".parse().unwrap())
            .unwrap()
            .clone();

        let s = schema(&[("dbo.customer", &["id", "full_name"])]);
        let intent = Intent::RenameColumn {
            table: t("dbo.customer"),
            from: "customer_name".into(),
            to: "full_name".into(),
        };
        let r = resolve(&s, &ids, &[intent], &ctx()).unwrap();

        assert_eq!(r.renamed_columns.len(), 1);
        let (uid, from, to) = &r.renamed_columns[0];
        assert_eq!(uid, &before_uid, "改名必須保留原本的身份");
        assert_eq!(from.name, "customer_name");
        assert_eq!(to.name, "full_name");
        assert!(r.added_columns.is_empty(), "改名不該同時算成新增");
        assert!(r.dropped_columns.is_empty(), "改名不該同時算成刪除");
        assert!(r.ids.tombstones.is_empty(), "改名不該留下墓碑");
    }

    /// 一次同時改名與新增，兩者都要正確分開。
    #[test]
    fn rename_and_addition_together() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "customer_name"])]);
        let s = schema(&[("dbo.customer", &["id", "full_name", "mobile"])]);
        let intent = Intent::RenameColumn {
            table: t("dbo.customer"),
            from: "customer_name".into(),
            to: "full_name".into(),
        };
        let r = resolve(&s, &ids, &[intent], &ctx()).unwrap();

        assert_eq!(r.renamed_columns.len(), 1);
        assert_eq!(r.added_columns.len(), 1);
        assert_eq!(r.added_columns[0].1.name, "mobile");
    }

    // ---- 表層級 ----

    #[test]
    fn table_rename_moves_its_columns() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "email"])]);
        let col_uid = ids
            .column_uid(&"dbo.customer.email".parse().unwrap())
            .unwrap()
            .clone();

        let s = schema(&[("dbo.client", &["id", "email"])]);
        let intent = Intent::RenameTable {
            from: t("dbo.customer"),
            to: t("dbo.client"),
        };
        let r = resolve(&s, &ids, &[intent], &ctx()).unwrap();

        assert_eq!(r.renamed_tables.len(), 1);
        assert!(
            r.added_columns.is_empty() && r.dropped_columns.is_empty(),
            "表改名不該讓底下的欄位被看成全新的"
        );
        assert_eq!(
            r.ids.columns.get(&col_uid).unwrap().to_string(),
            "dbo.client.email",
            "欄位的限定名稱要跟著表名更新"
        );
    }

    #[test]
    fn table_drop_tombstones_its_columns_too() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "email"])]);
        let s = Schema::default();
        let intent = Intent::DropTable {
            table: t("dbo.customer"),
            reason: "不再使用".into(),
        };
        let r = resolve(&s, &ids, &[intent], &ctx()).unwrap();

        assert_eq!(r.dropped_tables.len(), 1);
        assert_eq!(r.ids.tombstones.len(), 3, "表本身加上兩個欄位");
        assert!(r.ids.tables.is_empty());
        assert!(r.ids.columns.is_empty());
        r.ids.validate().unwrap();
    }

    #[test]
    fn table_rename_without_intent_is_ambiguous() {
        let (_, ids) = baseline(&[("dbo.customer", &["id"])]);
        let s = schema(&[("dbo.client", &["id"])]);
        let errs = resolve(&s, &ids, &[], &ctx()).unwrap_err();
        assert!(matches!(errs[0], Blocker::AmbiguousTables { .. }));
    }

    // ---- 意圖本身出錯 ----

    /// 打錯字的意圖若被靜默忽略，使用者接著會看到一個他不理解的歧義錯誤。
    #[test]
    fn a_typo_in_an_intent_is_reported() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "customer_name"])]);
        let s = schema(&[("dbo.customer", &["id", "full_name"])]);
        let intent = Intent::RenameColumn {
            table: t("dbo.customer"),
            from: "custmer_name".into(), // 打錯
            to: "full_name".into(),
        };
        let errs = resolve(&s, &ids, std::slice::from_ref(&intent), &ctx()).unwrap_err();

        assert!(
            errs.iter()
                .any(|b| matches!(b, Blocker::UnusedIntent { intent: i } if i == &intent)),
            "應指出這則意圖沒有對應到任何東西：{errs:?}"
        );
    }

    // ---- 不變式 ----

    /// 解析後的身份檔必須自洽，否則下一次比對會建立在壞掉的基準上。
    #[test]
    fn resulting_ids_file_is_always_valid() {
        let (_, ids) = baseline(&[("dbo.a", &["x", "y"]), ("dbo.b", &["z"])]);
        // dbo.b 保留、dbo.c 是純新增；dbo.a 的欄位改名。
        let s = schema(&[
            ("dbo.a", &["x", "y2"]),
            ("dbo.b", &["z"]),
            ("dbo.c", &["w"]),
        ]);
        let intents = vec![Intent::RenameColumn {
            table: t("dbo.a"),
            from: "y".into(),
            to: "y2".into(),
        }];
        let r = resolve(&s, &ids, &intents, &ctx()).unwrap();
        r.ids.validate().unwrap();
    }

    /// 套用一次之後再解析同一份宣告檔，應該完全沒有動作 —— 收斂性。
    #[test]
    fn resolution_converges() {
        let (_, ids) = baseline(&[("dbo.customer", &["id", "customer_name"])]);
        let s = schema(&[("dbo.customer", &["id", "full_name", "mobile"])]);
        let intents = vec![Intent::RenameColumn {
            table: t("dbo.customer"),
            from: "customer_name".into(),
            to: "full_name".into(),
        }];
        let after = resolve(&s, &ids, &intents, &ctx()).unwrap().ids;

        let again = resolve(&s, &after, &[], &ctx()).unwrap();
        assert_eq!(
            again,
            Resolution {
                ids: after.clone(),
                ..Default::default()
            },
            "第二次解析不應有任何動作"
        );
    }

    /// 多個問題要一次報完，不要修一個跑一次。
    #[test]
    fn multiple_blockers_are_all_reported() {
        let (_, ids) = baseline(&[("dbo.a", &["x", "gone"]), ("dbo.b", &["y", "old"])]);
        let s = schema(&[("dbo.a", &["x"]), ("dbo.b", &["y", "new"])]);
        let errs = resolve(&s, &ids, &[], &ctx()).unwrap_err();

        assert_eq!(errs.len(), 2, "應同時回報刪除缺理由與欄位歧義：{errs:?}");
    }
}
