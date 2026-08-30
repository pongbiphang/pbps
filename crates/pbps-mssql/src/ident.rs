//! Identifiers: comparison and quoting.

use pbps_dialect::DialectError;

/// The limit on a regular identifier, in characters, not bytes.
pub const MAX_IDENT_CHARS: usize = 128;

/// Wraps an identifier in brackets, doubling any closing bracket inside it.
///
/// Bracket quoting is used rather than double quotes because `QUOTED_IDENTIFIER`
/// can be turned off on a connection, which would silently turn `"a"` into a
/// string literal. Brackets mean the same thing under every setting.
pub fn quote(ident: &str) -> Result<String, DialectError> {
    if ident.is_empty() {
        return Err(DialectError::UnquotableIdent(ident.to_owned()));
    }
    // A NUL cannot be sent to the server inside an identifier at all, and unlike
    // `]` there is no escape for it.
    if ident.contains('\0') {
        return Err(DialectError::UnquotableIdent(ident.to_owned()));
    }
    if ident.chars().count() > MAX_IDENT_CHARS {
        return Err(DialectError::UnquotableIdent(ident.to_owned()));
    }
    Ok(format!("[{}]", ident.replace(']', "]]")))
}

/// Renders an identifier as an `N'...'` literal, for the procedures that take
/// names as strings (`sp_rename`).
pub fn literal(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brackets_are_doubled_not_stripped() {
        assert_eq!(quote("order").unwrap(), "[order]");
        assert_eq!(quote("we[i]rd").unwrap(), "[we[i]]rd]");
    }

    /// The whole point of quoting is that a name can never end the quoting
    /// early; if this ever regresses, a table name becomes an injection point.
    #[test]
    fn a_closing_bracket_cannot_escape_the_quotes() {
        let q = quote("a] DROP TABLE x --").unwrap();
        assert_eq!(q, "[a]] DROP TABLE x --]");
        assert!(q.starts_with('[') && q.ends_with(']'));
        // Every internal `]` is doubled, so no single `]` closes the identifier.
        assert!(!q[1..q.len() - 1].replace("]]", "").contains(']'));
    }

    #[test]
    fn unquotable_identifiers_are_refused() {
        assert!(quote("").is_err());
        assert!(quote("a\0b").is_err());
        assert!(quote(&"x".repeat(MAX_IDENT_CHARS + 1)).is_err());
        assert!(quote(&"x".repeat(MAX_IDENT_CHARS)).is_ok());
    }

    /// The limit is 128 characters, not bytes: a name of 128 CJK characters is
    /// legal even though it is 384 bytes.
    #[test]
    fn the_length_limit_counts_characters() {
        assert!(quote(&"\u{4f7f}".repeat(MAX_IDENT_CHARS)).is_ok());
        assert!(quote(&"\u{4f7f}".repeat(MAX_IDENT_CHARS + 1)).is_err());
    }

    #[test]
    fn literals_double_the_quote() {
        assert_eq!(literal("o'brien"), "N'o''brien'");
        assert_eq!(literal("plain"), "N'plain'");
    }
}
