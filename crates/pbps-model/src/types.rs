//! The **syntactic** representation of a column type.
//!
//! This layer answers only "what shape does this string have?": a type name plus
//! zero or more arguments. It deliberately does not know whether `nvarchar` is a
//! valid type, or whether `int → bigint` is safe — those belong to
//! `pbps-dialect` (SPEC §11.2).
//!
//! The payoff of drawing the line here is that a `Schema` can be parsed,
//! compared and serialized without any dialect being present at all.

use std::fmt;
use std::str::FromStr;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TypeParseError {
    #[error("a type must not be empty")]
    Empty,

    #[error("unclosed parenthesis in type `{0}`")]
    UnclosedParen(String),

    #[error("type `{0}` has trailing content after the closing parenthesis")]
    TrailingContent(String),

    #[error("type `{0}` has an empty argument")]
    EmptyArg(String),

    #[error("the type name in `{0}` is not a valid identifier")]
    BadBaseName(String),

    #[error("the argument position in type `{0}` must be between name words")]
    BadArgumentPosition(String),
}

/// A type argument.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TypeArg {
    /// The 100 of `nvarchar(100)`; the 18 and 2 of `decimal(18, 2)`.
    Int(i64),
    /// The `max` of `nvarchar(max)`. It gets its own variant because in a length
    /// comparison it means "no upper bound", which spares the dialect from
    /// parsing strings when judging narrowing.
    Max,
    /// Any other named argument, kept as written (lowercased).
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

/// A syntactically normalized type, such as `nvarchar(100)`, `decimal(18,2)` or
/// `bigint`.
///
/// Normalization does exactly two things: lowercase everything and strip
/// whitespace. **Aliases are not expanded here** (`integer` does not become
/// `int`), because which names alias which is dialect knowledge.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct ColumnType {
    pub base: String,
    pub args: Vec<TypeArg>,
    // A word boundary, not a byte offset or an engine-specific suffix. Array
    // dimensions can later be represented independently of this modifier.
    args_after_word: Option<usize>,
}

impl ColumnType {
    pub fn new(base: impl Into<String>, args: Vec<TypeArg>) -> Self {
        Self {
            base: base.into().to_ascii_lowercase(),
            args,
            args_after_word: None,
        }
    }

    /// A type with no arguments, such as `bigint`.
    pub fn simple(base: impl Into<String>) -> Self {
        Self::new(base, Vec::new())
    }

    /// Move the arguments between words of the base name. The default position
    /// remains after the complete name, preserving existing serialized types.
    pub fn with_args_after_word(mut self, words: usize) -> Result<Self, TypeParseError> {
        if self.args.is_empty() || words == 0 || words >= self.base.split_whitespace().count() {
            return Err(TypeParseError::BadArgumentPosition(self.to_string()));
        }
        self.base = self.base.split_whitespace().collect::<Vec<_>>().join(" ");
        self.args_after_word = Some(words);
        Ok(self)
    }

    /// An in-name modifier position, counted in complete base-name words.
    pub fn args_after_word(&self) -> Option<usize> {
        self.args_after_word
    }

    /// The first integer argument, which is what a dialect most often needs when
    /// judging length narrowing. `nvarchar(max)` returns `None` — no upper
    /// bound, not a length of zero.
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
        let boundary = self
            .args_after_word
            .and_then(|words| self.base.match_indices(' ').nth(words - 1).map(|(i, _)| i));
        let (before, after) = boundary.map_or((self.base.as_str(), ""), |i| self.base.split_at(i));
        f.write_str(before)?;
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
        f.write_str(after)?;
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
        let suffix = s[close + 1..].trim();
        if !suffix.is_empty()
            && (!s[close + 1..].starts_with(char::is_whitespace)
                || !suffix
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ' '))
        {
            return Err(TypeParseError::TrailingContent(s.to_owned()));
        }

        let base = s[..open].trim();
        let mut args = Vec::new();
        for raw in s[open + 1..close].split(',') {
            let a = raw.trim();
            if a.is_empty() {
                return Err(TypeParseError::EmptyArg(s.to_owned()));
            }
            if a.contains(['(', ')']) {
                return Err(TypeParseError::TrailingContent(s.to_owned()));
            }
            args.push(match a.parse::<i64>() {
                Ok(n) => TypeArg::Int(n),
                Err(_) if a.eq_ignore_ascii_case("max") => TypeArg::Max,
                Err(_) => TypeArg::Ident(a.to_ascii_lowercase()),
            });
        }
        if suffix.is_empty() {
            finish_base(s, base, args)
        } else {
            let prefix = finish_base(s, base, Vec::new())?;
            let base = format!("{} {suffix}", prefix.base);
            finish_base(s, &base, args)?
                .with_args_after_word(prefix.base.split_whitespace().count())
        }
    }
}

fn finish_base(whole: &str, base: &str, args: Vec<TypeArg>) -> Result<ColumnType, TypeParseError> {
    let base = base.trim();
    // Type names may contain spaces (`double precision`, `timestamp with time
    // zone`), but structural characters such as parentheses and commas must not
    // sneak in.
    let ok = !base.is_empty()
        && base
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ' ');
    if !ok {
        return Err(TypeParseError::BadBaseName(whole.to_owned()));
    }
    Ok(ColumnType::new(base, args))
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

    /// The same type in different cases must compare equal, or diff will invent
    /// changes that are not there.
    #[test]
    fn case_differences_do_not_create_changes() {
        assert_eq!(p("NVARCHAR(255)"), p("nvarchar(255)"));
        assert_eq!(p("DATETIME2(3)"), p("datetime2(3)"));
    }

    #[test]
    fn max_is_its_own_variant() {
        let t = p("nvarchar(MAX)");
        assert!(t.is_max());
        assert_eq!(t.first_int_arg(), None, "max is not a length of zero");
        assert_eq!(t.to_string(), "nvarchar(max)");
    }

    #[test]
    fn multi_word_base_names_are_allowed() {
        assert_eq!(p("double precision").base, "double precision");
        assert_eq!(
            p("timestamp with time zone").base,
            "timestamp with time zone"
        );
    }

    #[test]
    fn display_round_trips() {
        for s in ["bigint", "nvarchar(100)", "decimal(18, 2)", "nvarchar(max)"] {
            assert_eq!(p(s).to_string(), s, "{s} did not round-trip");
        }
    }

    #[test]
    fn in_name_modifiers_preserve_position_and_string_serialization() {
        for name in ["time", "timestamp"] {
            for zone in ["with", "without"] {
                let spelling = format!("{name}(3) {zone} time zone");
                let ty = p(&spelling);
                assert_eq!(ty.base, format!("{name} {zone} time zone"));
                assert_eq!(ty.args_after_word(), Some(1));
                assert_eq!(ty.to_string(), spelling);
                assert_eq!(p(&ty.to_string()), ty);
                let json = serde_json::to_string(&ty).unwrap();
                assert_eq!(json, format!("\"{spelling}\""));
                assert_eq!(serde_json::from_str::<ColumnType>(&json).unwrap(), ty);
                assert_ne!(ty, ColumnType::new(ty.base.clone(), ty.args.clone()));
            }
        }
        assert_eq!(p("custom name(4) qualifier").args_after_word(), Some(2));
        assert_eq!(
            p("timestamp (3)  with  time zone"),
            p("timestamp(3) with time zone")
        );
        for spelling in ["nvarchar(100)", "decimal(18, 2)", "datetime2(7)"] {
            assert_eq!(
                serde_json::to_string(&p(spelling)).unwrap(),
                format!("\"{spelling}\"")
            );
        }
    }

    #[test]
    fn modifiers_need_name_boundaries_and_arrays_remain_unrepresented() {
        for spelling in [
            "timestamp(3)with time zone",
            "timestamp(3) with time zone;",
            "timestamp(3)(4)",
            "timestamp(3) with (4)",
            "timestamp(3) with time zone[]",
            "text[]",
        ] {
            assert!(spelling.parse::<ColumnType>().is_err(), "{spelling}");
        }
        for position in [0, 4, usize::MAX] {
            assert!(
                ColumnType::new("timestamp with time zone", vec![TypeArg::Int(3)])
                    .with_args_after_word(position)
                    .is_err()
            );
        }
        assert!(
            ColumnType::simple("timestamp with time zone")
                .with_args_after_word(1)
                .is_err()
        );
    }

    #[test]
    fn malformed_types_are_rejected() {
        assert!("".parse::<ColumnType>().is_err());
        assert!("nvarchar(100".parse::<ColumnType>().is_err());
        assert!("nvarchar(100) junk;".parse::<ColumnType>().is_err());
        assert!("nvarchar(,)".parse::<ColumnType>().is_err());
        assert!("nvarchar()".parse::<ColumnType>().is_err());
    }

    /// Expanding aliases is the dialect's job; this layer must not decide on its
    /// own.
    #[test]
    fn aliases_are_left_alone() {
        assert_ne!(p("integer"), p("int"));
    }
}
