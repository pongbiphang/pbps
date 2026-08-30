//! 變更集：diff 的產物，planner 的輸入。
//!
//! # 為什麼是結構化資料而不是 SQL 字串
//!
//! 風險分類、閘門判斷、影響分析全部在這一層進行，SQL 只在方言的 emitter 中
//! 出現一次（SPEC §11.1）。若 diff 直接吐字串，上述判斷就得靠 regex 去猜自己
//! 剛剛寫了什麼 —— 那正是這個工具存在的理由的反面。
//!
//! # 風險為什麼是欄位而不是方法
//!
//! 「`int → bigint` 是放寬還是窄化」需要方言知識，模型層答不出來。因此風險由
//! 帶著 `Dialect` 的 differ 算好，以資料的形式附在 [`PlannedChange`] 上。

use std::collections::BTreeSet;
use std::fmt;

use crate::name::{ColumnRef, TableName};
use crate::schema::{
    CheckConstraint, Column, ForeignKey, Index, PrimaryKey, Table, UniqueConstraint,
};
use crate::types::ColumnType;
use crate::uid::Uid;

/// 需要在指令層顯性放行的風險類別（SPEC §7.2）。
///
/// 判斷依據是**變更類別本身是否可能失敗**，不是「這次資料剛好安不安全」——
/// 讀資料判斷屬於執行期，不在宣告層的職責內。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RiskClass {
    /// 改名。依賴此物件的 view / SP / 應用程式會失效。
    Rename,
    /// 資料遺失：DROP COLUMN / TABLE / INDEX。
    Destructive,
    /// 型別窄化或不相容轉換：可能截斷或轉換失敗。
    Narrowing,
    /// nullable → NOT NULL 且無 DEFAULT：既有 NULL 會違反。
    NotNull,
    /// 新增 UNIQUE / FK / CHECK：既有資料可能不滿足。
    Constraint,
}

impl RiskClass {
    /// `--allow` 接受的名稱。
    pub const fn as_str(self) -> &'static str {
        match self {
            RiskClass::Rename => "rename",
            RiskClass::Destructive => "destructive",
            RiskClass::Narrowing => "narrowing",
            RiskClass::NotNull => "not-null",
            RiskClass::Constraint => "constraint",
        }
    }

    pub const ALL: [RiskClass; 5] = [
        RiskClass::Rename,
        RiskClass::Destructive,
        RiskClass::Narrowing,
        RiskClass::NotNull,
        RiskClass::Constraint,
    ];
}

impl fmt::Display for RiskClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RiskClass {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        RiskClass::ALL
            .into_iter()
            .find(|r| r.as_str() == s)
            .ok_or_else(|| {
                let all: Vec<_> = RiskClass::ALL.iter().map(|r| r.as_str()).collect();
                format!("未知的風險類別 `{s}`，可用的有：{}", all.join(", "))
            })
    }
}

/// 一個原子變更。
///
/// 每個變體都帶著受影響物件的 UID，讓計畫在套用時不必再依賴名稱去對照 ——
/// 名稱正是可能同時在改變的東西。
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Change {
    CreateTable {
        uid: Uid,
        name: TableName,
        table: Box<Table>,
    },
    DropTable {
        uid: Uid,
        name: TableName,
    },
    RenameTable {
        uid: Uid,
        from: TableName,
        to: TableName,
    },

    AddColumn {
        uid: Uid,
        table: TableName,
        name: String,
        column: Box<Column>,
    },
    DropColumn {
        uid: Uid,
        column: ColumnRef,
    },
    RenameColumn {
        uid: Uid,
        table: TableName,
        from: String,
        to: String,
    },
    AlterColumnType {
        uid: Uid,
        column: ColumnRef,
        from: ColumnType,
        to: ColumnType,
    },
    AlterColumnNullability {
        uid: Uid,
        column: ColumnRef,
        /// 變更後是否可為 NULL
        to_nullable: bool,
    },
    AlterColumnDefault {
        uid: Uid,
        column: ColumnRef,
        from: Option<String>,
        to: Option<String>,
    },
    /// 棄用狀態改變。不產生結構變更，可選擇性寫入 extended property。
    SetColumnDeprecated {
        uid: Uid,
        column: ColumnRef,
        reason: Option<String>,
    },

    // 約束與索引一律 drop + add，不做原地修改 —— 資料庫本身也是這樣實作的，
    // 假裝可以原地改只會讓 emitter 多一條會出錯的路徑。
    SetPrimaryKey {
        table: TableName,
        from: Option<PrimaryKey>,
        to: Option<PrimaryKey>,
    },
    AddUnique {
        table: TableName,
        name: String,
        constraint: UniqueConstraint,
    },
    DropUnique {
        table: TableName,
        name: String,
    },
    AddForeignKey {
        table: TableName,
        name: String,
        constraint: Box<ForeignKey>,
    },
    DropForeignKey {
        table: TableName,
        name: String,
    },
    AddCheck {
        table: TableName,
        name: String,
        constraint: CheckConstraint,
    },
    DropCheck {
        table: TableName,
        name: String,
    },
    AddIndex {
        table: TableName,
        name: String,
        index: Box<Index>,
    },
    DropIndex {
        table: TableName,
        name: String,
    },
}

impl Change {
    /// 此變更作用的表。用於分組顯示與排序。
    pub fn table(&self) -> &TableName {
        match self {
            Change::CreateTable { name, .. } | Change::DropTable { name, .. } => name,
            Change::RenameTable { from, .. } => from,
            Change::AddColumn { table, .. }
            | Change::RenameColumn { table, .. }
            | Change::SetPrimaryKey { table, .. }
            | Change::AddUnique { table, .. }
            | Change::DropUnique { table, .. }
            | Change::AddForeignKey { table, .. }
            | Change::DropForeignKey { table, .. }
            | Change::AddCheck { table, .. }
            | Change::DropCheck { table, .. }
            | Change::AddIndex { table, .. }
            | Change::DropIndex { table, .. } => table,
            Change::DropColumn { column, .. }
            | Change::AlterColumnType { column, .. }
            | Change::AlterColumnNullability { column, .. }
            | Change::AlterColumnDefault { column, .. }
            | Change::SetColumnDeprecated { column, .. } => &column.table,
        }
    }

    /// 與變更種類本身綁定、不需要方言知識就能斷定的風險。
    ///
    /// 需要比較型別才能判定的（窄化）不在這裡，由 differ 補上。
    pub fn intrinsic_risks(&self) -> BTreeSet<RiskClass> {
        let mut r = BTreeSet::new();
        match self {
            Change::DropTable { .. } | Change::DropColumn { .. } | Change::DropIndex { .. } => {
                r.insert(RiskClass::Destructive);
            }
            Change::RenameTable { .. } | Change::RenameColumn { .. } => {
                r.insert(RiskClass::Rename);
            }
            Change::AlterColumnNullability { to_nullable: false, .. } => {
                r.insert(RiskClass::NotNull);
            }
            Change::AddUnique { .. }
            | Change::AddForeignKey { .. }
            | Change::AddCheck { .. } => {
                r.insert(RiskClass::Constraint);
            }
            Change::SetPrimaryKey { to: Some(_), .. } => {
                r.insert(RiskClass::Constraint);
            }
            Change::SetPrimaryKey { to: None, .. } => {
                r.insert(RiskClass::Destructive);
            }
            Change::CreateTable { .. }
            | Change::AddColumn { .. }
            | Change::AlterColumnType { .. }
            | Change::AlterColumnNullability { to_nullable: true, .. }
            | Change::AlterColumnDefault { .. }
            | Change::SetColumnDeprecated { .. }
            | Change::DropUnique { .. }
            | Change::DropForeignKey { .. }
            | Change::DropCheck { .. }
            | Change::AddIndex { .. } => {}
        }
        r
    }
}

/// 一個變更加上它已判定的風險。
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PlannedChange {
    #[serde(flatten)]
    pub change: Change,

    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub risks: BTreeSet<RiskClass>,
}

impl PlannedChange {
    /// 由變更本身可斷定的風險建立。需要方言判斷的風險由 differ 另外加入。
    pub fn new(change: Change) -> Self {
        let risks = change.intrinsic_risks();
        Self { change, risks }
    }

    pub fn with_risk(mut self, r: RiskClass) -> Self {
        self.risks.insert(r);
        self
    }
}

/// 一次 diff 的完整結果。
///
/// `changes` 的順序即為套用順序。排序（先 drop index 再 drop column…）
/// 是 planner 的職責，模型只保證順序被保留。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ChangeSet {
    pub changes: Vec<PlannedChange>,
}

impl ChangeSet {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// 這份計畫涉及的所有風險類別 —— 即 `--allow` 必須涵蓋的集合。
    pub fn risks(&self) -> BTreeSet<RiskClass> {
        self.changes.iter().flat_map(|c| c.risks.iter().copied()).collect()
    }

    /// 未被 `allowed` 涵蓋的風險。非空即代表 apply 應中止。
    pub fn unapproved_risks(&self, allowed: &BTreeSet<RiskClass>) -> BTreeSet<RiskClass> {
        self.risks().difference(allowed).copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Column;

    fn col(s: &str) -> ColumnRef {
        s.parse().unwrap()
    }
    fn uid(s: &str) -> Uid {
        s.parse().unwrap()
    }
    fn ty(s: &str) -> ColumnType {
        s.parse().unwrap()
    }

    fn drop_column() -> Change {
        Change::DropColumn {
            uid: uid("c_k7x2mq"),
            column: col("dbo.customer.legacy_code"),
        }
    }

    fn add_column() -> Change {
        Change::AddColumn {
            uid: uid("c_p3n8vd"),
            table: "dbo.customer".parse().unwrap(),
            name: "mobile".into(),
            column: Box::new(Column::new(ty("nvarchar(20)"))),
        }
    }

    #[test]
    fn destructive_changes_are_flagged() {
        assert!(drop_column().intrinsic_risks().contains(&RiskClass::Destructive));
    }

    #[test]
    fn additive_changes_carry_no_risk() {
        assert!(add_column().intrinsic_risks().is_empty());
    }

    /// 加 NOT NULL 有風險，放寬成 nullable 沒有 —— 方向必須分得清楚。
    #[test]
    fn nullability_risk_is_directional() {
        let tighten = Change::AlterColumnNullability {
            uid: uid("c_k7x2mq"),
            column: col("dbo.customer.email"),
            to_nullable: false,
        };
        let loosen = Change::AlterColumnNullability {
            uid: uid("c_k7x2mq"),
            column: col("dbo.customer.email"),
            to_nullable: true,
        };
        assert!(tighten.intrinsic_risks().contains(&RiskClass::NotNull));
        assert!(loosen.intrinsic_risks().is_empty());
    }

    /// 型別窄化需要方言判斷，模型層不該自作主張。
    #[test]
    fn type_change_risk_is_left_to_the_dialect() {
        let c = Change::AlterColumnType {
            uid: uid("c_k7x2mq"),
            column: col("dbo.customer.balance"),
            from: ty("bigint"),
            to: ty("int"),
        };
        assert!(c.intrinsic_risks().is_empty(), "模型層不應猜測型別風險");

        let planned = PlannedChange::new(c).with_risk(RiskClass::Narrowing);
        assert!(planned.risks.contains(&RiskClass::Narrowing));
    }

    #[test]
    fn unapproved_risks_are_reported() {
        let cs = ChangeSet {
            changes: vec![PlannedChange::new(drop_column()), PlannedChange::new(add_column())],
        };
        assert_eq!(cs.risks(), BTreeSet::from([RiskClass::Destructive]));

        let none = BTreeSet::new();
        assert_eq!(
            cs.unapproved_risks(&none),
            BTreeSet::from([RiskClass::Destructive])
        );

        let allowed = BTreeSet::from([RiskClass::Destructive]);
        assert!(cs.unapproved_risks(&allowed).is_empty());
    }

    /// 無風險的計畫不需要任何旗標。
    #[test]
    fn safe_plans_need_no_flags() {
        let cs = ChangeSet {
            changes: vec![PlannedChange::new(add_column())],
        };
        assert!(cs.unapproved_risks(&BTreeSet::new()).is_empty());
    }

    #[test]
    fn risk_names_round_trip() {
        for r in RiskClass::ALL {
            assert_eq!(r.as_str().parse::<RiskClass>().unwrap(), r);
        }
        assert!("nonsense".parse::<RiskClass>().is_err());
    }

    #[test]
    fn changes_round_trip_through_json() {
        let cs = ChangeSet {
            changes: vec![PlannedChange::new(drop_column()), PlannedChange::new(add_column())],
        };
        let back: ChangeSet = serde_json::from_str(&serde_json::to_string(&cs).unwrap()).unwrap();
        assert_eq!(cs, back);
    }

    #[test]
    fn every_change_reports_its_table() {
        assert_eq!(drop_column().table().to_string(), "dbo.customer");
        assert_eq!(add_column().table().to_string(), "dbo.customer");
    }
}
