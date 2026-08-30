//! 宣告檔的 YAML 形狀。
//!
//! 這裡的結構刻意與 [`pbps_model`] 的領域模型**不同**：YAML 用 map key 表示
//! 名稱、有預設值、還帶著 `renamed_from` 這類一次性的意圖註記。轉換由
//! [`crate::convert`] 負責，模型因此得以保持乾淨（見 CLAUDE.md 約束 1、2）。
//!
//! 所有結構都加 `deny_unknown_fields`：拼錯欄位名是最常見的使用者錯誤，
//! 默默套用預設值會讓宣告與實際產生無聲的偏差。

use indexmap::IndexMap;
use serde_saphyr::Spanned;
use std::collections::BTreeMap;

use pbps_model::ReferentialAction;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableDto {
    pub table: Spanned<String>,

    #[serde(default)]
    pub description: Option<String>,

    /// 一次性意圖：這張表是從哪個名稱改過來的。
    /// 被 `pbps plan` 吸收進身份檔後會從檔案中移除。
    #[serde(default)]
    pub renamed_from: Option<Spanned<String>>,

    /// `IndexMap` 保留文件中的宣告順序 —— 那會決定 `CREATE TABLE` 的欄位排列。
    pub columns: IndexMap<String, ColumnDto>,

    #[serde(default)]
    pub primary_key: Option<PrimaryKeyDto>,

    #[serde(default)]
    pub unique: BTreeMap<String, Vec<String>>,

    #[serde(default)]
    pub foreign_keys: BTreeMap<String, ForeignKeyDto>,

    /// 約束名 → 檢查運算式
    #[serde(default)]
    pub checks: BTreeMap<String, String>,

    #[serde(default)]
    pub indexes: BTreeMap<String, IndexDto>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnDto {
    #[serde(rename = "type")]
    pub ty: Spanned<String>,

    #[serde(default = "yes")]
    pub nullable: bool,

    /// 預設值運算式，原樣保留。
    #[serde(default)]
    pub default: Option<String>,

    /// `[seed, increment]`
    #[serde(default)]
    pub identity: Option<[i64; 2]>,

    #[serde(default)]
    pub description: Option<String>,

    /// 有值即代表已棄用，內容是原因。日期由 git 提供，不在這裡寫。
    #[serde(default)]
    pub deprecated: Option<String>,

    /// 一次性意圖，同 [`TableDto::renamed_from`]。
    #[serde(default)]
    pub renamed_from: Option<Spanned<String>>,
}

const fn yes() -> bool {
    true
}

/// 主鍵可以只寫欄位清單（名稱交給資料庫），也可以指定約束名。
///
/// 支援具名形式不是為了好看：`pbps pull` 從既有資料庫反向生成時，若丟失原本的
/// 約束名，下一次 diff 就會想把它改名。
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
pub enum PrimaryKeyDto {
    Columns(Vec<String>),
    Named { name: String, columns: Vec<String> },
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignKeyDto {
    pub columns: Vec<String>,

    /// `dbo.region(region_id)` 或 `dbo.region(a, b)`
    pub references: Spanned<String>,

    #[serde(default)]
    pub on_delete: ReferentialAction,

    #[serde(default)]
    pub on_update: ReferentialAction,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexDto {
    /// 每項是 `欄位名` 或 `欄位名 desc`
    pub columns: Vec<Spanned<String>>,

    #[serde(default)]
    pub include: Vec<String>,

    #[serde(default)]
    pub unique: bool,

    /// 篩選索引條件。YAML 用 `where`，那是 Rust 的關鍵字，故改名。
    #[serde(rename = "where", default)]
    pub filter: Option<String>,
}
