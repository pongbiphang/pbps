//! 身份錨點。
//!
//! UID 是工具內部用來回答「這是不是同一個欄位」的唯一依據。**使用者永遠不會
//! 輸入或看見它** —— 它只出現在身份檔與 `__pbps_state` 裡（見 SPEC §5.2）。
//!
//! 用隨機而非流水號，是為了讓兩個分支各自新增欄位時不會配到同一個號碼。

use std::fmt;
use std::str::FromStr;

/// 去掉容易誤讀的 `i` `l` `o` `u`，剩 32 個字元。
/// 使用者不會手打 UID，但 UID 會出現在錯誤訊息與 git diff 裡，好讀仍有價值。
const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// 隨機段長度。32^6 ≈ 1.07e9；以單一專案的欄位數量級（10^3～10^4）而言，
/// 依生日問題估算碰撞機率約在 10^-3 以下，且配發時會檢查既有 UID，
/// 碰撞只會導致重抽，不會產生錯誤的身份。
const LEN: usize = 6;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UidError {
    #[error("UID `{0}` 缺少 `t_` 或 `c_` 前綴")]
    BadPrefix(String),

    #[error("UID `{0}` 的長度應為 {LEN} 個字元（不含前綴）")]
    BadLength(String),

    #[error("UID `{0}` 含有字母表以外的字元")]
    BadChar(String),
}

/// UID 指向的物件種類。前綴讓身份檔一眼看得出這一列在講表還是欄位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UidKind {
    Table,
    Column,
}

impl UidKind {
    pub const fn prefix(self) -> &'static str {
        match self {
            UidKind::Table => "t_",
            UidKind::Column => "c_",
        }
    }
}

/// 形如 `c_k7x2mq` / `t_a9k2mq`。
///
/// `Ord` 直接取字串序，因此同類的 UID 會排在一起，身份檔的 diff 比較好讀。
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct Uid(String);

impl Uid {
    /// 配發一個新的 UID。
    ///
    /// 呼叫端有責任確認結果不與既有 UID 重複；碰撞極罕見但並非不可能，
    /// 而「靜默重用同一個身份」會直接造成錯誤的 rename 判定。
    pub fn generate(kind: UidKind) -> Self {
        let mut s = String::with_capacity(2 + LEN);
        s.push_str(kind.prefix());
        for _ in 0..LEN {
            let i = (next_random() % ALPHABET.len() as u64) as usize;
            s.push(ALPHABET[i] as char);
        }
        Self(s)
    }

    pub fn kind(&self) -> UidKind {
        if self.0.starts_with("t_") {
            UidKind::Table
        } else {
            UidKind::Column
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Uid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Uid {
    type Err = UidError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let rest = s
            .strip_prefix("t_")
            .or_else(|| s.strip_prefix("c_"))
            .ok_or_else(|| UidError::BadPrefix(s.to_owned()))?;

        if rest.len() != LEN {
            return Err(UidError::BadLength(s.to_owned()));
        }
        if !rest.bytes().all(|b| ALPHABET.contains(&b)) {
            return Err(UidError::BadChar(s.to_owned()));
        }
        Ok(Self(s.to_owned()))
    }
}

impl TryFrom<String> for Uid {
    type Error = UidError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Uid> for String {
    fn from(v: Uid) -> String {
        v.0
    }
}

/// UID 的隨機來源。
///
/// **刻意不使用 `rand`。** UID 是身份標記而非秘密 —— 猜到別人的 UID 沒有任何
/// 好處，因此不需要密碼學品質的隨機性，只需要「兩個分支不會配到同一個號碼」。
/// 為此拉進一個依賴（以及它底下的 `getrandom` 與平台 FFI）不划算：這個工具
/// 會在受管制的環境裡被稽核，依賴樹愈短愈好。
///
/// 種子取自 `RandomState`（作業系統提供、每個 process 不同）與單調時間，
/// 之後以 SplitMix64 推進。
fn next_random() -> u64 {
    use std::cell::Cell;
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }

    STATE.with(|st| {
        let mut x = st.get();
        if x == 0 {
            let mut h = RandomState::new().build_hasher();
            h.write_u64(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0),
            );
            // RandomState 每個實例的 key 不同，雜湊結果因此帶有 OS 熵。
            x = h.finish() | 1;
        }
        // SplitMix64
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        st.set(x);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn generated_uids_round_trip() {
        for kind in [UidKind::Table, UidKind::Column] {
            let u = Uid::generate(kind);
            assert_eq!(u.kind(), kind);
            assert_eq!(u.as_str().parse::<Uid>().unwrap(), u);
        }
    }

    #[test]
    fn generated_uids_avoid_ambiguous_letters() {
        for _ in 0..500 {
            let u = Uid::generate(UidKind::Column);
            let body = &u.as_str()[2..];
            assert!(
                !body.contains(['i', 'l', 'o', 'u']),
                "字母表不該產出易誤讀字元: {u}"
            );
        }
    }

    /// 不是密碼學品質的要求，只是確認沒有寫成常數。
    #[test]
    fn generation_is_not_constant() {
        let set: BTreeSet<_> = (0..200).map(|_| Uid::generate(UidKind::Column)).collect();
        assert!(
            set.len() > 190,
            "隨機性明顯不足：200 次只產生 {} 個相異值",
            set.len()
        );
    }

    #[test]
    fn malformed_uids_are_rejected() {
        assert!("k7x2mq".parse::<Uid>().is_err(), "缺前綴");
        assert!("c_k7x2m".parse::<Uid>().is_err(), "太短");
        assert!("c_k7x2mqq".parse::<Uid>().is_err(), "太長");
        assert!("c_k7x2mi".parse::<Uid>().is_err(), "含字母表外的 i");
        assert!("c_K7X2MQ".parse::<Uid>().is_err(), "大寫不在字母表內");
    }
}
