//! 環境狀態快照 —— 寫入各環境資料庫的 `__pbps_state`（SPEC §8）。
//!
//! 狀態存在資料庫自己身上，而不是 CI artifact，是這個工具能天然支援多環境的
//! 原因：dev / staging / prod 各自記得自己的狀態，落後幾版都不需要任何
//! artifact 傳遞或環境對照策略。

use crate::schema::Schema;

pub const CURRENT_VERSION: u32 = 1;

/// 這筆狀態是怎麼來的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateKind {
    /// 正常套用一份計畫
    Apply,
    /// 重設基準：不追究差異，以資料庫現況為新起點
    Baseline,
    /// 從宣告檔一次性建出完整 schema（DR / 新環境）
    Bootstrap,
}

/// 某一時刻某個環境的完整已驗證狀態。
///
/// 存整份 schema 而不是增量或 checksum：drift 檢查才能完整比對、可作為備援、
/// 也才能回答「三個月前這張表長什麼樣」。
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct StateSnapshot {
    pub version: u32,
    pub kind: StateKind,

    /// 實查資料庫得到的 schema
    pub schema: Schema,

    /// 產生此狀態的 commit。`None` 用於在版控之外執行的 baseline。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,

    /// 套用的計畫的 checksum，對應 saved plan 的釘死機制（SPEC §7.3）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_checksum: Option<String>,

    pub operator: String,

    /// baseline 必填 —— 「為什麼要跳過差異」正是稽核要問的問題。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl StateSnapshot {
    pub fn new(kind: StateKind, schema: Schema, operator: impl Into<String>) -> Self {
        Self {
            version: CURRENT_VERSION,
            kind,
            schema,
            git_sha: None,
            plan_checksum: None,
            operator: operator.into(),
            reason: None,
        }
    }

    /// 資料庫現況是否仍等於這份快照。不等於即為 drift。
    pub fn matches(&self, actual: &Schema) -> bool {
        &self.schema == actual
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::name::TableName;
    use crate::schema::{Column, Table};
    use crate::types::ColumnType;
    use indexmap::IndexMap;

    fn schema_with(ty: &str) -> Schema {
        let mut columns = IndexMap::new();
        columns.insert(
            "email".to_string(),
            Column::new(ty.parse::<ColumnType>().unwrap()),
        );
        let mut s = Schema::default();
        s.tables.insert(
            TableName::new("dbo", "customer"),
            Table { columns, ..Default::default() },
        );
        s
    }

    #[test]
    fn identical_schema_is_not_drift() {
        let snap = StateSnapshot::new(StateKind::Apply, schema_with("nvarchar(255)"), "leon");
        assert!(snap.matches(&schema_with("nvarchar(255)")));
    }

    #[test]
    fn any_difference_is_drift() {
        let snap = StateSnapshot::new(StateKind::Apply, schema_with("nvarchar(255)"), "leon");
        assert!(!snap.matches(&schema_with("nvarchar(100)")), "型別改變應為 drift");
        assert!(!snap.matches(&Schema::default()), "表消失應為 drift");
    }

    /// 大小寫差異不該被當成 drift，否則每次 introspect 都可能誤報。
    #[test]
    fn type_case_is_not_drift() {
        let snap = StateSnapshot::new(StateKind::Apply, schema_with("NVARCHAR(255)"), "leon");
        assert!(snap.matches(&schema_with("nvarchar(255)")));
    }

    #[test]
    fn version_is_recorded() {
        let snap = StateSnapshot::new(StateKind::Baseline, Schema::default(), "leon");
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains(r#""version":1"#));
        assert!(json.contains(r#""kind":"baseline""#));
    }

    #[test]
    fn round_trips_through_json() {
        let mut snap = StateSnapshot::new(StateKind::Apply, schema_with("bigint"), "leon");
        snap.git_sha = Some("bd4be74".into());
        snap.plan_checksum = Some("abc123".into());
        let back: StateSnapshot =
            serde_json::from_str(&serde_json::to_string(&snap).unwrap()).unwrap();
        assert_eq!(snap, back);
    }
}
