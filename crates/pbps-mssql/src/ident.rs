//! Identifiers: comparison and quoting.

use pbps_dialect::DialectError;

/// SQL Server's limit on a regular identifier, in UTF-16 code units.
///
/// The `CHARS` suffix is retained for the public constant's compatibility, but
/// SQL Server exposes this limit through `sysname` (`nvarchar(128)`), whose
/// length is measured in UTF-16 units rather than Unicode scalar values.
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
    if ident.encode_utf16().count() > MAX_IDENT_CHARS {
        return Err(DialectError::UnquotableIdent(ident.to_owned()));
    }
    Ok(format!("[{}]", ident.replace(']', "]]")))
}

/// Whether a lower-cased word can never stand unbracketed as a name — for
/// the name scans, which read a bare word as a possible reference (DECISIONS
/// 316).
///
/// The documented reserved keywords of Transact-SQL, every one **measured**
/// on SQL Server 2022 with a table of that name: `FROM word` is refused for
/// all of them, and `FROM dbo.word` for all but `disk`, `dump`, `load`,
/// `precision` and `securityaudit`. The scans ask only about the bare form,
/// so the five are reserved here too.
pub fn is_reserved(word: &str) -> bool {
    RESERVED.binary_search(&word).is_ok()
}

/// The words `is_reserved` names, sorted for the search.
const RESERVED: &[&str] = &[
    "add",
    "all",
    "alter",
    "and",
    "any",
    "as",
    "asc",
    "authorization",
    "backup",
    "begin",
    "between",
    "break",
    "browse",
    "bulk",
    "by",
    "cascade",
    "case",
    "check",
    "checkpoint",
    "close",
    "clustered",
    "coalesce",
    "collate",
    "column",
    "commit",
    "compute",
    "constraint",
    "contains",
    "containstable",
    "continue",
    "convert",
    "create",
    "cross",
    "current",
    "current_date",
    "current_time",
    "current_timestamp",
    "current_user",
    "cursor",
    "database",
    "dbcc",
    "deallocate",
    "declare",
    "default",
    "delete",
    "deny",
    "desc",
    "disk",
    "distinct",
    "distributed",
    "double",
    "drop",
    "dump",
    "else",
    "end",
    "errlvl",
    "escape",
    "except",
    "exec",
    "execute",
    "exists",
    "exit",
    "external",
    "fetch",
    "file",
    "fillfactor",
    "for",
    "foreign",
    "freetext",
    "freetexttable",
    "from",
    "full",
    "function",
    "goto",
    "grant",
    "group",
    "having",
    "holdlock",
    "identity",
    "identity_insert",
    "identitycol",
    "if",
    "in",
    "index",
    "inner",
    "insert",
    "intersect",
    "into",
    "is",
    "join",
    "key",
    "kill",
    "left",
    "like",
    "lineno",
    "load",
    "merge",
    "national",
    "nocheck",
    "nonclustered",
    "not",
    "null",
    "nullif",
    "of",
    "off",
    "offsets",
    "on",
    "open",
    "opendatasource",
    "openquery",
    "openrowset",
    "openxml",
    "option",
    "or",
    "order",
    "outer",
    "over",
    "percent",
    "pivot",
    "plan",
    "precision",
    "primary",
    "print",
    "proc",
    "procedure",
    "public",
    "raiserror",
    "read",
    "readtext",
    "reconfigure",
    "references",
    "replication",
    "restore",
    "restrict",
    "return",
    "revert",
    "revoke",
    "right",
    "rollback",
    "rowcount",
    "rowguidcol",
    "rule",
    "save",
    "schema",
    "securityaudit",
    "select",
    "semantickeyphrasetable",
    "semanticsimilaritydetailstable",
    "semanticsimilaritytable",
    "session_user",
    "set",
    "setuser",
    "shutdown",
    "some",
    "statistics",
    "system_user",
    "table",
    "tablesample",
    "textsize",
    "then",
    "to",
    "top",
    "tran",
    "transaction",
    "trigger",
    "truncate",
    "try_convert",
    "tsequal",
    "union",
    "unique",
    "unpivot",
    "update",
    "updatetext",
    "use",
    "user",
    "values",
    "varying",
    "view",
    "waitfor",
    "when",
    "where",
    "while",
    "with",
    "writetext",
];

/// Renders an identifier as an `N'...'` literal, for the procedures that take
/// names as strings (`sp_rename`).
pub fn literal(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Measured: `FROM select` and `FROM order` are refused, `FROM customer`
    /// is not.
    #[test]
    fn a_reserved_word_is_one_the_engine_refuses_unbracketed() {
        for word in ["select", "order", "user", "key", "precision"] {
            assert!(is_reserved(word), "{word}");
        }
        for word in ["customer", "name", "value", "zone", "int"] {
            assert!(!is_reserved(word), "{word}");
        }
        assert!(
            RESERVED.windows(2).all(|w| w[0] < w[1]),
            "sorted, for the search"
        );
    }

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

    #[test]
    fn a_128_character_bmp_identifier_is_accepted() {
        let ident = "\u{4f7f}".repeat(MAX_IDENT_CHARS);
        assert_eq!(ident.chars().count(), MAX_IDENT_CHARS);
        assert_eq!(ident.encode_utf16().count(), MAX_IDENT_CHARS);
        assert!(quote(&ident).is_ok());
    }

    #[test]
    fn a_128_character_supplementary_identifier_is_refused_by_utf16_width() {
        let ident = "\u{1f600}".repeat(MAX_IDENT_CHARS);
        assert_eq!(ident.chars().count(), MAX_IDENT_CHARS);
        assert_eq!(ident.encode_utf16().count(), MAX_IDENT_CHARS * 2);
        assert!(quote(&ident).is_err());
    }

    #[test]
    fn literals_double_the_quote() {
        assert_eq!(literal("o'brien"), "N'o''brien'");
        assert_eq!(literal("plain"), "N'plain'");
    }
}
