//! `pbps` 的領域模型。
//!
//! # 這一層的界線
//!
//! - **方言無關**。這裡沒有任何 T-SQL / PostgreSQL 的知識。型別字串只做
//!   *語法* 解析（名稱 + 參數），「`nvarchar` 是不是有效型別」「`int → bigint`
//!   算不算窄化」屬於 `pbps-dialect`。
//! - **不帶 span**。載入期的位置資訊留在 `pbps-load`，不進模型。理由是
//!   `Schema` 要能直接用 `==` 比較（diff 與 drift 檢查都靠它），
//!   夾帶 span 會讓兩份語意相同的 schema 不相等。
//! - **序列化目標是 JSON**（身份檔與 `__pbps_state` 快照），不是 YAML。
//!   YAML 的形狀與模型不同（map key 即名稱、有預設值），
//!   由 `pbps-load` 負責兩者之間的轉換。
//!
//! # 決定性
//!
//! 所有集合一律用 `BTreeMap` / `BTreeSet`，讓序列化輸出穩定 —— 身份檔要進
//! git，順序跳動會製造假的 diff。唯一的例外是 `Table::columns` 用
//! `IndexMap` 保留宣告順序，因為那會影響 `CREATE TABLE` 的欄位排列。

pub mod change;
pub mod ids;
pub mod name;
pub mod schema;
pub mod state;
pub mod types;
pub mod uid;

pub use change::{Change, ChangeSet, PlannedChange, RiskClass};
pub use ids::{IdsFile, Tombstone};
pub use name::{ColumnRef, NameError, TableName};
pub use schema::{
    CheckConstraint, Column, ForeignKey, Identity, Index, IndexColumn, PrimaryKey,
    ReferentialAction, Schema, Table, UniqueConstraint,
};
pub use state::{StateKind, StateSnapshot};
pub use types::{ColumnType, TypeArg, TypeParseError};
pub use uid::{Uid, UidError, UidKind};
