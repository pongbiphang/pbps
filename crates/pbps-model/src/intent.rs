//! 需要人給定的意圖。
//!
//! # 為什麼意圖是獨立的概念
//!
//! 絕大多數變更都能從「宣告檔現在長什麼樣」自動推導。只有兩件事推導不出來，
//! 因為所需的資訊根本不在檔案裡，只在當事人腦中（SPEC §6）：
//!
//! - **改名還是刪除加新增**：同一張表同時有欄位消失與新增時，結構上完全無法區分
//! - **為什麼刪除**：稽核要回答的問題，任何演算法都生不出答案
//!
//! 意圖不進 [`crate::Schema`]。`Schema` 必須滿足「兩份語意相同的 schema 一定
//! 相等」，而意圖是一次性的：同一個期望狀態，可能來自改名也可能來自重建，
//! 把它混進去會讓相等性失效（見 CLAUDE.md 約束 1）。

use crate::name::{ColumnRef, TableName};

/// 一則由人提供的意圖。
///
/// 來源有三種且完全等價（SPEC §6）：`pbps rename` 之類的 CLI 指令、
/// 宣告檔中的暫時性註記、互動式 prompt。它們最終都匯流到身份檔。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(tag = "intent", rename_all = "snake_case")]
pub enum Intent {
    RenameTable {
        from: TableName,
        to: TableName,
    },
    RenameColumn {
        table: TableName,
        from: String,
        to: String,
    },
    /// 刪除一張表。`reason` 是稽核要求，不可省略。
    DropTable {
        table: TableName,
        reason: String,
    },
    /// 刪除一個欄位。
    DropColumn {
        column: ColumnRef,
        reason: String,
    },
}

impl Intent {
    /// 這則意圖是否關於指定的表。
    pub fn concerns_table(&self, t: &TableName) -> bool {
        match self {
            Intent::RenameTable { from, to } => from == t || to == t,
            Intent::RenameColumn { table, .. } | Intent::DropTable { table, .. } => table == t,
            Intent::DropColumn { column, .. } => &column.table == t,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> TableName {
        s.parse().unwrap()
    }

    #[test]
    fn rename_concerns_both_old_and_new_table() {
        let i = Intent::RenameTable {
            from: t("dbo.customer"),
            to: t("dbo.client"),
        };
        assert!(i.concerns_table(&t("dbo.customer")));
        assert!(i.concerns_table(&t("dbo.client")));
        assert!(!i.concerns_table(&t("dbo.other")));
    }

    #[test]
    fn round_trips_through_json() {
        let all = vec![
            Intent::RenameTable {
                from: t("dbo.a"),
                to: t("dbo.b"),
            },
            Intent::RenameColumn {
                table: t("dbo.a"),
                from: "x".into(),
                to: "y".into(),
            },
            Intent::DropTable {
                table: t("dbo.a"),
                reason: "REG-1".into(),
            },
            Intent::DropColumn {
                column: "dbo.a.x".parse().unwrap(),
                reason: "REG-2".into(),
            },
        ];
        let back: Vec<Intent> =
            serde_json::from_str(&serde_json::to_string(&all).unwrap()).unwrap();
        assert_eq!(all, back);
    }
}
