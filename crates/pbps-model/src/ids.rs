//! 身份檔（`schema.ids.json`）。
//!
//! 這個檔案進 git，存的是**無法從宣告檔推導的資訊**：UID 與名稱的對照，
//! 以及已刪除欄位的墓碑（SPEC §5）。型別、nullable、index 定義一律不存 ——
//! 那些 YAML 裡就有，重複存放只會製造兩份會不一致的真相。

use std::collections::BTreeMap;

use crate::name::{ColumnRef, TableName};
use crate::uid::{Uid, UidKind};

/// 目前的檔案格式版本。
///
/// 從第一版就存在，日後才有機會做相容性遷移；等到需要時才加就太遲了。
pub const CURRENT_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdsError {
    #[error("身份檔版本為 {found}，本工具只支援 {supported}")]
    UnsupportedVersion { found: u32, supported: u32 },

    #[error("UID {uid} 同時是活躍的與墓碑 —— 身份檔已損毀")]
    LiveAndTombstoned { uid: Uid },

    #[error("{a} 與 {b} 都指向同一個名稱 `{name}`")]
    DuplicateName { a: Uid, b: Uid, name: String },

    #[error("{uid} 的前綴與它所在的區塊不符")]
    KindMismatch { uid: Uid },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct IdsFile {
    pub version: u32,

    #[serde(default)]
    pub tables: BTreeMap<Uid, TableName>,

    #[serde(default)]
    pub columns: BTreeMap<Uid, ColumnRef>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tombstones: BTreeMap<Uid, Tombstone>,
}

impl Default for IdsFile {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            tables: BTreeMap::new(),
            columns: BTreeMap::new(),
            tombstones: BTreeMap::new(),
        }
    }
}

/// 已物理刪除的物件留下的紀錄。
///
/// 墓碑存在身份檔而不是宣告檔裡，宣告檔因此永遠只包含「你要的東西」，
/// 不會隨年資累積殭屍欄位（SPEC §4.4）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Tombstone {
    /// 刪除當下的完整名稱。稽核要回答「這個欄位曾經叫什麼」。
    pub was: String,
    /// `YYYY-MM-DD`
    pub dropped_at: String,
    pub reason: String,
    pub operator: String,
}

impl IdsFile {
    /// 以名稱反查 UID。差異比對每次都要做這件事。
    pub fn table_uid(&self, name: &TableName) -> Option<&Uid> {
        self.tables.iter().find(|(_, n)| *n == name).map(|(u, _)| u)
    }

    pub fn column_uid(&self, r: &ColumnRef) -> Option<&Uid> {
        self.columns.iter().find(|(_, n)| *n == r).map(|(u, _)| u)
    }

    /// 檢查內部一致性。
    ///
    /// 身份檔是工具自己寫的，正常情況不會壞；但它進 git，就可能被手動編輯或
    /// 被錯誤的 merge 解決搞亂。這裡的檢查是在 diff 拿它當真相之前的最後一道防線。
    pub fn validate(&self) -> Result<(), IdsError> {
        if self.version != CURRENT_VERSION {
            return Err(IdsError::UnsupportedVersion {
                found: self.version,
                supported: CURRENT_VERSION,
            });
        }

        for uid in self.tables.keys() {
            if uid.kind() != UidKind::Table {
                return Err(IdsError::KindMismatch { uid: uid.clone() });
            }
        }
        for uid in self.columns.keys() {
            if uid.kind() != UidKind::Column {
                return Err(IdsError::KindMismatch { uid: uid.clone() });
            }
        }

        for uid in self.tombstones.keys() {
            if self.tables.contains_key(uid) || self.columns.contains_key(uid) {
                return Err(IdsError::LiveAndTombstoned { uid: uid.clone() });
            }
        }

        check_unique(self.tables.iter().map(|(u, n)| (u, n.to_string())))?;
        check_unique(self.columns.iter().map(|(u, n)| (u, n.to_string())))?;
        Ok(())
    }
}

/// 兩個 UID 指向同一個名稱，代表身份錯亂 —— 之後的 rename 判定會是錯的。
fn check_unique<'a>(it: impl Iterator<Item = (&'a Uid, String)>) -> Result<(), IdsError> {
    let mut seen: BTreeMap<String, &Uid> = BTreeMap::new();
    for (uid, name) in it {
        if let Some(prev) = seen.insert(name.clone(), uid) {
            return Err(IdsError::DuplicateName {
                a: prev.clone(),
                b: uid.clone(),
                name,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid(s: &str) -> Uid {
        s.parse().unwrap()
    }
    fn col(s: &str) -> ColumnRef {
        s.parse().unwrap()
    }

    fn sample() -> IdsFile {
        let mut f = IdsFile::default();
        f.tables.insert(uid("t_a9k2mq"), "dbo.customer".parse().unwrap());
        f.columns.insert(uid("c_k7x2mq"), col("dbo.customer.customer_id"));
        f.columns.insert(uid("c_p3n8vd"), col("dbo.customer.full_name"));
        f
    }

    #[test]
    fn sample_is_valid() {
        sample().validate().unwrap();
    }

    #[test]
    fn reverse_lookup_works() {
        let f = sample();
        assert_eq!(
            f.column_uid(&col("dbo.customer.full_name")),
            Some(&uid("c_p3n8vd"))
        );
        assert_eq!(f.column_uid(&col("dbo.customer.nope")), None);
    }

    /// rename 的 diff 應該只有一行 —— 這是身份檔在 review 中可讀的關鍵。
    #[test]
    fn rename_changes_exactly_one_line() {
        let before = serde_json::to_string_pretty(&sample()).unwrap();
        let mut after = sample();
        after
            .columns
            .insert(uid("c_p3n8vd"), col("dbo.customer.display_name"));
        let after = serde_json::to_string_pretty(&after).unwrap();

        let diff = before
            .lines()
            .zip(after.lines())
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(diff, 1, "rename 應只改動一行");
        assert_eq!(before.lines().count(), after.lines().count());
    }

    #[test]
    fn version_is_written_and_checked() {
        let json = serde_json::to_string(&IdsFile::default()).unwrap();
        assert!(json.contains(r#""version":1"#));

        let mut f = sample();
        f.version = 99;
        assert_eq!(
            f.validate().unwrap_err(),
            IdsError::UnsupportedVersion { found: 99, supported: 1 }
        );
    }

    /// 手動編輯或錯誤的 merge 可能造出兩個 UID 指向同一欄位。
    #[test]
    fn duplicate_names_are_rejected() {
        let mut f = sample();
        f.columns.insert(uid("c_zzzzzz"), col("dbo.customer.full_name"));
        assert!(matches!(
            f.validate().unwrap_err(),
            IdsError::DuplicateName { .. }
        ));
    }

    #[test]
    fn a_uid_cannot_be_both_live_and_tombstoned() {
        let mut f = sample();
        f.tombstones.insert(
            uid("c_p3n8vd"),
            Tombstone {
                was: "dbo.customer.full_name".into(),
                dropped_at: "2026-08-30".into(),
                reason: "測試".into(),
                operator: "leon".into(),
            },
        );
        assert!(matches!(
            f.validate().unwrap_err(),
            IdsError::LiveAndTombstoned { .. }
        ));
    }

    #[test]
    fn uid_prefix_must_match_its_section() {
        let mut f = sample();
        f.columns.insert(uid("t_bbbbbb"), col("dbo.customer.x"));
        assert!(matches!(
            f.validate().unwrap_err(),
            IdsError::KindMismatch { .. }
        ));
    }

    #[test]
    fn round_trips_through_json() {
        let f = sample();
        let back: IdsFile = serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
        assert_eq!(f, back);
    }
}
