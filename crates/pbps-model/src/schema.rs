//! 期望狀態的結構。
//!
//! # 兩條刻意的設計約束
//!
//! **容器持有名稱，元素不持有。** `Table` 沒有 `name` 欄位、`Column` 沒有
//! `name` 欄位 —— 名稱是父層 map 的 key。這消滅了「map key 與內部 name 不一致」
//! 這一整類不可能自我察覺的 bug。
//!
//! **模型只表達狀態，不表達意圖。** `renamed_from` 這種暫時性註記不在這裡，
//! 由 `pbps-load` 另外回傳。理由是 `Schema` 必須滿足「兩份語意相同的 schema
//! 一定相等」，diff 與 drift 檢查都建立在這個前提上；把一次性的意圖混進來，
//! 同一個狀態就會因為註記有無而不相等。

use std::collections::BTreeMap;

use indexmap::IndexMap;

use crate::name::TableName;
use crate::types::ColumnType;

/// 一個專案的完整期望狀態。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Schema {
    pub tables: BTreeMap<TableName, Table>,
}

impl Schema {
    pub fn get(&self, name: &TableName) -> Option<&Table> {
        self.tables.get(name)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Table {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// 用 `IndexMap` 保留宣告順序 —— 那會決定 `CREATE TABLE` 的欄位排列。
    /// 相等性比較與順序無關，所以順序變動不會被誤判為 schema 變更。
    pub columns: IndexMap<String, Column>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_key: Option<PrimaryKey>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unique: BTreeMap<String, UniqueConstraint>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub foreign_keys: BTreeMap<String, ForeignKey>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub checks: BTreeMap<String, CheckConstraint>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub indexes: BTreeMap<String, Index>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Column {
    #[serde(rename = "type")]
    pub ty: ColumnType,

    /// 預設 `true`，與 SQL 的預設一致。
    pub nullable: bool,

    /// 預設值運算式，原樣保留（如 `0`、`SYSUTCDATETIME()`）。
    ///
    /// 不解析成結構 —— 運算式的語法是方言知識，而我們對它唯一的需求是
    /// 「原封不動送給資料庫」與「比較是否改變」。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<Identity>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// 有值即代表已棄用，內容是棄用原因。
    ///
    /// 日期不存 —— git 已經記錄了它，要使用者手寫一次是多餘的
    /// 且必然會與真實時間不符（SPEC §4.2）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<String>,
}

impl Column {
    /// 最常見的形狀：可為 NULL、無預設值。
    pub fn new(ty: ColumnType) -> Self {
        Self {
            ty,
            nullable: true,
            default: None,
            identity: None,
            description: None,
            deprecated: None,
        }
    }

    pub fn not_null(mut self) -> Self {
        self.nullable = false;
        self
    }

    pub fn is_deprecated(&self) -> bool {
        self.deprecated.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Identity {
    pub seed: i64,
    pub increment: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PrimaryKey {
    /// 約束名。`None` 表示交由資料庫自動命名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct UniqueConstraint {
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ForeignKey {
    pub columns: Vec<String>,
    pub references_table: TableName,
    pub references_columns: Vec<String>,

    #[serde(default)]
    pub on_delete: ReferentialAction,
    #[serde(default)]
    pub on_update: ReferentialAction,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferentialAction {
    #[default]
    NoAction,
    Cascade,
    SetNull,
    SetDefault,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct CheckConstraint {
    /// 檢查運算式，原樣保留。
    pub expression: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Index {
    pub columns: Vec<IndexColumn>,

    /// 只讀不入 key 的附加欄位（SQL Server 的 INCLUDE）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,

    #[serde(default)]
    pub unique: bool,

    /// 篩選索引的條件，原樣保留。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct IndexColumn {
    pub name: String,
    #[serde(default)]
    pub descending: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ty(s: &str) -> ColumnType {
        s.parse().unwrap()
    }

    fn sample() -> Table {
        let mut columns = IndexMap::new();
        columns.insert("customer_id".into(), Column::new(ty("bigint")).not_null());
        columns.insert("email".into(), Column::new(ty("nvarchar(255)")));
        Table {
            columns,
            primary_key: Some(PrimaryKey {
                name: Some("pk_customer".into()),
                columns: vec!["customer_id".into()],
            }),
            ..Default::default()
        }
    }

    #[test]
    fn nullable_defaults_to_true() {
        assert!(Column::new(ty("int")).nullable);
        assert!(!Column::new(ty("int")).not_null().nullable);
    }

    /// 欄位順序影響 CREATE TABLE 的輸出，所以必須保留。
    #[test]
    fn column_order_is_preserved() {
        let t = sample();
        assert_eq!(
            t.columns.keys().collect::<Vec<_>>(),
            ["customer_id", "email"]
        );
    }

    /// ...但順序不同不該被判定為 schema 變更，否則每次重排都會產生假的 diff。
    #[test]
    fn column_order_does_not_affect_equality() {
        let a = sample();
        let mut columns = IndexMap::new();
        columns.insert("email".into(), Column::new(ty("nvarchar(255)")));
        columns.insert("customer_id".into(), Column::new(ty("bigint")).not_null());
        let b = Table { columns, ..a.clone() };
        assert_eq!(a, b);
    }

    /// 序列化必須是決定性的 —— 狀態快照要進 git 與 DB，順序跳動會製造假 diff。
    #[test]
    fn serialisation_is_deterministic() {
        let mut schema = Schema::default();
        schema.tables.insert(TableName::new("dbo", "customer"), sample());
        schema.tables.insert(TableName::new("app", "region"), Table::default());

        let first = serde_json::to_string(&schema).unwrap();
        for _ in 0..20 {
            assert_eq!(serde_json::to_string(&schema).unwrap(), first);
        }
        // BTreeMap 排序：app.region 在 dbo.customer 之前
        assert!(first.find("app.region").unwrap() < first.find("dbo.customer").unwrap());
    }

    #[test]
    fn schema_round_trips_through_json() {
        let mut schema = Schema::default();
        schema.tables.insert(TableName::new("dbo", "customer"), sample());
        let json = serde_json::to_string(&schema).unwrap();
        let back: Schema = serde_json::from_str(&json).unwrap();
        assert_eq!(schema, back);
    }

    /// 空集合不應污染輸出，否則身份檔與快照會充滿 `{}`。
    #[test]
    fn empty_collections_are_omitted() {
        let json = serde_json::to_string(&Table::default()).unwrap();
        assert_eq!(json, r#"{"columns":{}}"#);
    }
}
