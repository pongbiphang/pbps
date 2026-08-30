//! 欄位型別的**語法**表示。
//!
//! 這一層只回答「這個字串的結構長什麼樣」：型別名加上零到多個參數。
//! 它刻意不知道 `nvarchar` 是否為合法型別、`int → bigint` 算不算安全 ——
//! 那些是 `pbps-dialect` 的職責（SPEC §11.2）。
//!
//! 這樣切的好處是：`Schema` 可以被解析、比較、序列化，而完全不需要方言在場。

use std::fmt;
use std::str::FromStr;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TypeParseError {
    #[error("型別不可為空")]
    Empty,

    #[error("型別 `{0}` 的括號未閉合")]
    UnclosedParen(String),

    #[error("型別 `{0}` 在右括號之後還有內容")]
    TrailingContent(String),

    #[error("型別 `{0}` 的參數為空")]
    EmptyArg(String),

    #[error("型別名 `{0}` 不是合法的識別名")]
    BadBaseName(String),
}

/// 型別參數。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeArg {
    /// `nvarchar(100)` 的 100、`decimal(18, 2)` 的 18 與 2
    Int(i64),
    /// `nvarchar(max)`。獨立成一個變體，因為它在長度比較上的語意是「無上限」，
    /// 讓方言判斷窄化時不必去解析字串。
    Max,
    /// 其他具名參數，保留原樣（小寫化）
    Ident(String),
}

impl fmt::Display for TypeArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeArg::Int(n) => write!(f, "{n}"),
            TypeArg::Max => f.write_str("max"),
            TypeArg::Ident(s) => f.write_str(s),
        }
    }
}

/// 已做語法正規化的型別，如 `nvarchar(100)`、`decimal(18,2)`、`bigint`。
///
/// 正規化只做兩件事：大小寫統一為小寫、去除空白。**別名不在此展開**
/// （`integer` 不會變成 `int`），因為哪些名字互為別名是方言知識。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ColumnType {
    pub base: String,
    pub args: Vec<TypeArg>,
}

impl ColumnType {
    pub fn new(base: impl Into<String>, args: Vec<TypeArg>) -> Self {
        Self { base: base.into().to_ascii_lowercase(), args }
    }

    /// 無參數型別，如 `bigint`。
    pub fn simple(base: impl Into<String>) -> Self {
        Self::new(base, Vec::new())
    }

    /// 第一個整數參數，方言判斷長度窄化時最常用到。
    /// `nvarchar(max)` 回傳 `None`（無上限，不是長度為 0）。
    pub fn first_int_arg(&self) -> Option<i64> {
        match self.args.first() {
            Some(TypeArg::Int(n)) => Some(*n),
            Some(TypeArg::Max) | Some(TypeArg::Ident(_)) | None => None,
        }
    }

    pub fn is_max(&self) -> bool {
        self.args.first() == Some(&TypeArg::Max)
    }
}

impl fmt::Display for ColumnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.base)?;
        if !self.args.is_empty() {
            f.write_str("(")?;
            for (i, a) in self.args.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{a}")?;
            }
            f.write_str(")")?;
        }
        Ok(())
    }
}

impl FromStr for ColumnType {
    type Err = TypeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(TypeParseError::Empty);
        }

        let Some(open) = s.find('(') else {
            return finish_base(s, s, Vec::new());
        };

        let Some(close) = s.rfind(')') else {
            return Err(TypeParseError::UnclosedParen(s.to_owned()));
        };
        if close < open {
            return Err(TypeParseError::UnclosedParen(s.to_owned()));
        }
        if !s[close + 1..].trim().is_empty() {
            return Err(TypeParseError::TrailingContent(s.to_owned()));
        }

        let base = s[..open].trim();
        let mut args = Vec::new();
        for raw in s[open + 1..close].split(',') {
            let a = raw.trim();
            if a.is_empty() {
                return Err(TypeParseError::EmptyArg(s.to_owned()));
            }
            args.push(match a.parse::<i64>() {
                Ok(n) => TypeArg::Int(n),
                Err(_) if a.eq_ignore_ascii_case("max") => TypeArg::Max,
                Err(_) => TypeArg::Ident(a.to_ascii_lowercase()),
            });
        }
        finish_base(s, base, args)
    }
}

fn finish_base(whole: &str, base: &str, args: Vec<TypeArg>) -> Result<ColumnType, TypeParseError> {
    let base = base.trim();
    // 型別名允許空白（`double precision`、`timestamp with time zone`），
    // 但不允許括號、逗號等結構字元混進來。
    let ok = !base.is_empty()
        && base
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ' ');
    if !ok {
        return Err(TypeParseError::BadBaseName(whole.to_owned()));
    }
    Ok(ColumnType {
        base: base.to_ascii_lowercase(),
        args,
    })
}

impl TryFrom<String> for ColumnType {
    type Error = TypeParseError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<ColumnType> for String {
    fn from(v: ColumnType) -> String {
        v.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> ColumnType {
        s.parse().unwrap()
    }

    #[test]
    fn parses_simple_and_parameterised() {
        assert_eq!(p("bigint"), ColumnType::simple("bigint"));
        assert_eq!(
            p("nvarchar(100)"),
            ColumnType::new("nvarchar", vec![TypeArg::Int(100)])
        );
        assert_eq!(
            p("decimal(18, 2)"),
            ColumnType::new("decimal", vec![TypeArg::Int(18), TypeArg::Int(2)])
        );
    }

    #[test]
    fn normalises_case_and_whitespace() {
        assert_eq!(p("  NVARCHAR ( 100 ) "), p("nvarchar(100)"));
        assert_eq!(p("BigInt"), p("bigint"));
    }

    /// 大小寫不同的同一型別必須相等，否則 diff 會產生假的變更。
    #[test]
    fn case_differences_do_not_create_changes() {
        assert_eq!(p("NVARCHAR(255)"), p("nvarchar(255)"));
        assert_eq!(p("DATETIME2(3)"), p("datetime2(3)"));
    }

    #[test]
    fn max_is_its_own_variant() {
        let t = p("nvarchar(MAX)");
        assert!(t.is_max());
        assert_eq!(t.first_int_arg(), None, "max 不是長度 0");
        assert_eq!(t.to_string(), "nvarchar(max)");
    }

    #[test]
    fn multi_word_base_names_are_allowed() {
        assert_eq!(p("double precision").base, "double precision");
        assert_eq!(p("timestamp with time zone").base, "timestamp with time zone");
    }

    #[test]
    fn display_round_trips() {
        for s in ["bigint", "nvarchar(100)", "decimal(18, 2)", "nvarchar(max)"] {
            assert_eq!(p(s).to_string(), s, "{s} 的往返不一致");
        }
    }

    #[test]
    fn malformed_types_are_rejected() {
        assert!("".parse::<ColumnType>().is_err());
        assert!("nvarchar(100".parse::<ColumnType>().is_err());
        assert!("nvarchar(100) junk".parse::<ColumnType>().is_err());
        assert!("nvarchar(,)".parse::<ColumnType>().is_err());
        assert!("nvarchar()".parse::<ColumnType>().is_err());
    }

    /// 別名展開是方言的事，這一層不能自作主張。
    #[test]
    fn aliases_are_left_alone() {
        assert_ne!(p("integer"), p("int"));
    }
}
