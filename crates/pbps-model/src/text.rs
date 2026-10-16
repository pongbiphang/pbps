//! Free text an artifact or the database can carry, which has no `Display`.
//!
//! A description, a reason, a commit hash, a timestamp and the other strings a
//! saved plan, a state snapshot or a resolver's evidence carries are written by
//! whoever made that file. A line break in one starts a line the tool never
//! wrote, and a terminal sequence rewrites the lines before it. DEC-2032.1
//! closed that one print site at a time, and the next `{}` reopened it, because
//! a `String` prints the same whether the writer meant it for a human or for
//! SQL (#2084).
//!
//! [`Text`] makes the choice a compile error to skip. It implements neither
//! `Display` nor `ToString`, so `format!("{}", text)` does not build, and it
//! offers exactly two ways to use it as text:
//!
//! - [`Text::shown`] for anything a person reads. It escapes through
//!   [`Escaped`], the character set DEC-2032.1 fixed.
//! - [`Text::raw`] for what must stay the bytes the file held: a statement that
//!   quotes it through its dialect, a serialized form, a comparison, a hash.
//!
//! It serializes as the bare string, so a saved plan, a state snapshot, a ledger
//! row and every checksum over them are the bytes they were before the type
//! existed.
//!
//! ```compile_fail,E0277
//! let text = pbps_model::Text::from("a\nb");
//! let _ = format!("{}", text);
//! ```
//!
//! ```compile_fail,E0599
//! let text = pbps_model::Text::from("a\nb");
//! let _ = text.to_string();
//! ```

use crate::escape::Escaped;

/// A string an artifact or the database can carry, kept as it was written.
///
/// The line breaks stay in it: [`Text::raw`] returns them and [`Text::shown`]
/// is where they are escaped. Escaping at construction instead would change the
/// bytes a plan holds and so its checksum, and would make a description that
/// the declaration file spelled with a line break come back from the database
/// as another description.
#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct Text(String);

impl Text {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The text for a person to read: control characters, line and paragraph
    /// separators and the invisible reordering characters are shown as escapes,
    /// and the backslash is doubled (DEC-2032.1).
    pub fn shown(&self) -> Escaped<'_> {
        Escaped(&self.0)
    }

    /// The text as it was written. Never for a message a person reads (#2084).
    pub fn raw(&self) -> &str {
        &self.0
    }

    pub fn into_raw(self) -> String {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// So a map keyed by [`Text`] is looked up by `&str`; hashing and ordering are
/// those of the string, as `Borrow` requires.
impl std::borrow::Borrow<str> for Text {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<String> for Text {
    fn from(text: String) -> Self {
        Self(text)
    }
}

impl From<&str> for Text {
    fn from(text: &str) -> Self {
        Self(text.to_owned())
    }
}

/// The schema of a bare string, which is what [`Text`] serializes as, so the
/// published schemas do not move.
impl schemars::JsonSchema for Text {
    fn inline_schema() -> bool {
        <String as schemars::JsonSchema>::inline_schema()
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        <String as schemars::JsonSchema>::schema_name()
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        <String as schemars::JsonSchema>::schema_id()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        <String as schemars::JsonSchema>::json_schema(generator)
    }
}

#[cfg(test)]
mod tests {
    use super::Text;

    const HOSTILE: &str = "a\nb\r\u{1b}[31mred\u{9b}2J";

    #[test]
    fn shown_escapes_what_raw_keeps() {
        let text = Text::new(HOSTILE);
        assert_eq!(text.raw(), HOSTILE);
        let shown = text.shown().to_string();
        assert_eq!(shown, "a\\nb\\r\\u{1b}[31mred\\u{9b}2J");
        assert!(!shown.chars().any(char::is_control), "{shown:?}");
    }

    #[test]
    fn plain_unicode_is_shown_unchanged() {
        // Negative: only control and invisible characters are rewritten.
        for text in [
            "",
            "refresh the nightly load",
            "刪除舊欄位 🙂 ünï",
            "a, b: 'c'",
        ] {
            assert_eq!(Text::from(text).shown().to_string(), text);
        }
    }

    #[test]
    fn it_serializes_as_the_bare_string_it_holds() {
        let text = Text::new(HOSTILE);
        let json = serde_json::to_string(&text).unwrap();
        assert_eq!(json, serde_json::to_string(HOSTILE).unwrap());
        let back: Text = serde_json::from_str(&json).unwrap();
        assert_eq!(back, text);
        // Negative: a non-string is not text, as it was not a string before.
        assert!(serde_json::from_str::<Text>("1").is_err());
        assert!(serde_json::from_str::<Text>("null").is_err());
    }

    #[test]
    fn its_schema_is_the_one_of_a_string() {
        use schemars::{JsonSchema, SchemaGenerator};
        let text = Text::json_schema(&mut SchemaGenerator::default());
        let string = String::json_schema(&mut SchemaGenerator::default());
        assert_eq!(
            serde_json::to_value(text).unwrap(),
            serde_json::to_value(string).unwrap()
        );
    }
}
