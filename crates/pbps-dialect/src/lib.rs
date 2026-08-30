//! 方言抽象。
//!
//! # 邊界
//!
//! `pbps-model`、`pbps-load`、`pbps-diff` 完全不知道任何資料庫的存在。所有
//! 「這個型別合不合法」「這個變更怎麼寫成 SQL」「兩個識別名算不算同一個」
//! 的知識都集中在這裡的實作中（SPEC §11.2）。
//!
//! # 為什麼拆成兩個 trait
//!
//! [`Dialect`] 是純函式，不碰網路，Phase 1 的 diff 與 Phase 2 的 planner 都
//! 只需要它。連線相關的能力（introspection、rename 影響分析）留給 Phase 3 的
//! `DialectDb`，那時才會引入 async 與 DB driver。把它們綁在一起會讓 Phase 1
//! 的測試被迫拖著一個 runtime 跑。
//!
//! # Phase 0 的 PostgreSQL 檢驗
//!
//! 這個介面刻意拿 PG 當第二個假想實作驗證過，四個最容易漏掉的差異都容得下：
//!
//! | 差異 | PostgreSQL | SQL Server | 介面如何容納 |
//! |---|---|---|---|
//! | 未加引號的識別名 | 摺疊成小寫 | 保留原樣 | [`Dialect::fold_ident`] |
//! | 改型別 + 改 nullable | 必須兩道語句 | 可合併成一道 | [`Dialect::emit`] 回傳 `Vec` |
//! | rename 對 view 的影響 | 自動更新 | 定義文字失效 | 留在 Phase 3 的 `DialectDb` |
//! | 批次分隔 | 不需要 | 部分 DDL 需自成批次 | [`Statement::own_batch`] |
//!
//! 若日後新增方言需要改動 `pbps-model`，代表這裡的抽象抓錯了。

use std::borrow::Cow;

use pbps_model::{Change, ColumnType, RiskClass, Table, TableName};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DialectError {
    #[error("{dialect} 沒有型別 `{ty}`")]
    UnknownType { dialect: &'static str, ty: String },

    #[error("{dialect} 的 `{ty}` 參數數量不對：{detail}")]
    BadTypeArity {
        dialect: &'static str,
        ty: String,
        detail: String,
    },

    #[error("{dialect} 不支援{feature}")]
    Unsupported {
        dialect: &'static str,
        feature: String,
    },

    #[error("識別名 `{0}` 無法安全地寫進 SQL")]
    UnquotableIdent(String),
}

/// 型別變更的安全性判定。
///
/// 依據是**變更類別本身是否可能失敗**，不是「這批資料剛好安不安全」——
/// 讀資料判斷屬於執行期，不在宣告層的職責內（SPEC §7.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeChangeRisk {
    /// 無變化，或是放寬（`int` → `bigint`、`varchar(50)` → `varchar(100)`）
    Safe,
    /// 窄化：可能截斷
    Narrowing,
    /// 不相容：轉換本身可能失敗（`nvarchar` → `int`）
    Incompatible,
}

impl TypeChangeRisk {
    /// 對應到閘門用的風險類別。`Safe` 不需要放行。
    pub const fn risk_class(self) -> Option<RiskClass> {
        match self {
            TypeChangeRisk::Safe => None,
            TypeChangeRisk::Narrowing | TypeChangeRisk::Incompatible => Some(RiskClass::Narrowing),
        }
    }
}

/// 一道可執行的語句。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub sql: String,

    /// 必須自成一個批次。
    ///
    /// SQL Server 有些 DDL 不能與後續參照它的語句同批（新增欄位後立刻在同批
    /// 中引用會編譯失敗）。PostgreSQL 沒有這個限制，實作一律填 `false` 即可 ——
    /// 但這個欄位必須存在於介面裡，否則 executor 沒有辦法知道該不該切批次。
    pub own_batch: bool,
}

impl Statement {
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            own_batch: false,
        }
    }

    pub fn own_batch(mut self) -> Self {
        self.own_batch = true;
        self
    }
}

/// 不需要資料庫連線的方言知識。
pub trait Dialect {
    fn name(&self) -> &'static str;

    /// 展開別名並補上省略的預設參數，讓語意相同的兩種寫法變成同一個值。
    ///
    /// 這一步是 diff 正確性的前提：`INTEGER` 與 `int` 若沒有先收斂成同一個值，
    /// 每次都會被判定成型別變更。
    fn normalize_type(&self, ty: &ColumnType) -> Result<ColumnType, DialectError>;

    /// 判定型別變更的安全性。呼叫端有責任先 [`normalize_type`](Dialect::normalize_type)。
    fn type_change_risk(&self, from: &ColumnType, to: &ColumnType) -> TypeChangeRisk;

    /// 未加引號的識別名在此方言中的正規形式。
    ///
    /// PostgreSQL 摺疊成小寫、SQL Server 保留原樣。名稱比對必須經過這一步，
    /// 否則 introspection 讀回來的名稱會與宣告檔對不上，drift 檢查天天誤報。
    fn fold_ident<'a>(&self, ident: &'a str) -> Cow<'a, str>;

    /// 加上引號，供寫進 SQL 使用。
    fn quote_ident(&self, ident: &str) -> Result<String, DialectError>;

    /// 檢查這張表用到的功能此方言是否支援。
    ///
    /// 回傳全部問題而非第一個 —— 使用者應該一次看完所有要修的地方。
    fn validate_table(&self, name: &TableName, table: &Table) -> Vec<DialectError>;

    /// 把一個變更寫成語句。
    ///
    /// 回傳 `Vec` 是必要的：PostgreSQL 改型別與改 nullable 必須拆成兩道
    /// `ALTER COLUMN`，SQL Server 則可以合併成一道。
    fn emit(&self, change: &Change) -> Result<Vec<Statement>, DialectError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_type_changes_need_no_approval() {
        assert_eq!(TypeChangeRisk::Safe.risk_class(), None);
    }

    #[test]
    fn unsafe_type_changes_map_to_narrowing() {
        assert_eq!(
            TypeChangeRisk::Narrowing.risk_class(),
            Some(RiskClass::Narrowing)
        );
        assert_eq!(
            TypeChangeRisk::Incompatible.risk_class(),
            Some(RiskClass::Narrowing)
        );
    }

    fn ty(s: &str) -> ColumnType {
        s.parse().unwrap()
    }

    #[test]
    fn minimal_dialect_treats_shrinking_as_narrowing() {
        let d = MinimalDialect;
        assert_eq!(
            d.type_change_risk(&ty("nvarchar(100)"), &ty("nvarchar(50)")),
            TypeChangeRisk::Narrowing
        );
        assert_eq!(
            d.type_change_risk(&ty("nvarchar(50)"), &ty("nvarchar(100)")),
            TypeChangeRisk::Safe
        );
        assert_eq!(
            d.type_change_risk(&ty("bigint"), &ty("bigint")),
            TypeChangeRisk::Safe
        );
        assert_eq!(
            d.type_change_risk(&ty("bigint"), &ty("nvarchar(10)")),
            TypeChangeRisk::Incompatible
        );
    }

    /// max 是「無上限」而不是長度 0，方向判斷不能反過來。
    #[test]
    fn minimal_dialect_handles_max_length() {
        let d = MinimalDialect;
        assert_eq!(
            d.type_change_risk(&ty("nvarchar(max)"), &ty("nvarchar(100)")),
            TypeChangeRisk::Narrowing
        );
        assert_eq!(
            d.type_change_risk(&ty("nvarchar(100)"), &ty("nvarchar(max)")),
            TypeChangeRisk::Safe
        );
    }

    #[test]
    fn statements_default_to_shared_batch() {
        let s = Statement::new("ALTER TABLE t ADD c INT");
        assert!(!s.own_batch);
        assert!(s.own_batch().own_batch);
    }
}

/// 測試與 Phase 1 使用的最小方言。
///
/// **不是任何真實資料庫。** 它只實作型別比較所需的保守規則，讓不依賴特定
/// 資料庫的邏輯（diff、風險分類）可以被測試。真正的 MSSQL 實作在 Phase 2。
///
/// 保守的意思是：拿不準就當成危險。寧可要求使用者多按一次放行，
/// 也不要漏放一個會截斷資料的變更。
#[derive(Debug, Clone, Copy, Default)]
pub struct MinimalDialect;

impl Dialect for MinimalDialect {
    fn name(&self) -> &'static str {
        "minimal"
    }

    /// 不做別名展開 —— 哪些名字互為別名是真實方言的知識。
    fn normalize_type(&self, ty: &ColumnType) -> Result<ColumnType, DialectError> {
        Ok(ty.clone())
    }

    fn type_change_risk(&self, from: &ColumnType, to: &ColumnType) -> TypeChangeRisk {
        if from == to {
            return TypeChangeRisk::Safe;
        }
        if from.base != to.base {
            return TypeChangeRisk::Incompatible;
        }
        match (from.is_max(), to.is_max()) {
            // 從無上限縮到有上限一定可能截斷
            (true, false) => TypeChangeRisk::Narrowing,
            // 放寬成無上限是安全的
            (false, true) => TypeChangeRisk::Safe,
            _ => match (from.first_int_arg(), to.first_int_arg()) {
                (Some(a), Some(b)) if b < a => TypeChangeRisk::Narrowing,
                (Some(_), Some(_)) => TypeChangeRisk::Safe,
                // 參數有無不一致（例如 decimal → decimal(18,2)）語意不明，
                // 保守地當成窄化。
                _ => TypeChangeRisk::Narrowing,
            },
        }
    }

    fn fold_ident<'a>(&self, ident: &'a str) -> Cow<'a, str> {
        Cow::Borrowed(ident)
    }

    fn quote_ident(&self, ident: &str) -> Result<String, DialectError> {
        if ident.contains('"') {
            return Err(DialectError::UnquotableIdent(ident.to_owned()));
        }
        Ok(format!("\"{ident}\""))
    }

    fn validate_table(&self, _name: &TableName, _table: &Table) -> Vec<DialectError> {
        Vec::new()
    }

    fn emit(&self, _change: &Change) -> Result<Vec<Statement>, DialectError> {
        Err(DialectError::Unsupported {
            dialect: "minimal",
            feature: "SQL 生成（真正的 emitter 在 Phase 2）".to_owned(),
        })
    }
}
