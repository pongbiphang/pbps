//! The words SQLite cannot take as a bare name (#2950).
//!
//! Measured on the bundled SQLite 3.54.1 against the 154 words of its keyword
//! list: these 58 are refused as a bare table name, as a bare column name and
//! after `FROM`; the other keywords (`abort`, `key`, `left`, `rowid`, `temp`,
//! `main`) stand bare. `if` alone is refused only as a table name and is left
//! out. Quoting always works, and this dialect quotes every name it writes, so
//! the list serves the scanners that read a bare word in a definition and the
//! check on a declared type, where a keyword would end the type name and be
//! read as the start of a constraint.

/// The reserved words, lower case.
const RESERVED: [&str; 58] = [
    "add",
    "all",
    "alter",
    "and",
    "as",
    "autoincrement",
    "between",
    "case",
    "check",
    "collate",
    "commit",
    "constraint",
    "create",
    "default",
    "deferrable",
    "delete",
    "distinct",
    "drop",
    "else",
    "escape",
    "except",
    "exists",
    "foreign",
    "from",
    "group",
    "having",
    "in",
    "index",
    "insert",
    "intersect",
    "into",
    "is",
    "isnull",
    "join",
    "limit",
    "not",
    "nothing",
    "notnull",
    "null",
    "on",
    "or",
    "order",
    "primary",
    "references",
    "returning",
    "select",
    "set",
    "table",
    "then",
    "to",
    "transaction",
    "union",
    "unique",
    "update",
    "using",
    "values",
    "when",
    "where",
];

/// Whether `word` can never stand bare as a name. Compared without regard to
/// ASCII case, as SQLite reads keywords.
#[must_use]
pub fn is_reserved(word: &str) -> bool {
    RESERVED.iter().any(|w| w.eq_ignore_ascii_case(word))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Fresh;

    fn accepts(sql: &str) -> bool {
        Fresh::memory().connection().execute_batch(sql).is_ok()
    }

    /// Every listed word fails bare in all three positions and works quoted,
    /// so none is listed by mistake and the list is not a guess.
    #[test]
    fn every_reserved_word_is_refused_bare_and_accepted_quoted() {
        for word in RESERVED {
            assert!(
                !accepts(&format!("CREATE TABLE {word}(a)")),
                "{word} as a table"
            );
            assert!(
                !accepts(&format!("CREATE TABLE t({word} int)")),
                "{word} as a column"
            );
            assert!(
                !accepts(&format!("CREATE TABLE \"{word}\"(a); SELECT * FROM {word}")),
                "{word} after FROM"
            );
            assert!(
                accepts(&format!("CREATE TABLE \"{word}\"(a)")),
                "{word} quoted"
            );
            assert!(is_reserved(word) && is_reserved(&word.to_ascii_uppercase()));
        }
    }

    /// The negative side: keywords SQLite does take bare are not listed, so
    /// the scans do not skip a real reference to a table called `key`.
    #[test]
    fn keywords_that_stand_bare_are_not_reserved() {
        for word in [
            "abort", "action", "key", "left", "rowid", "temp", "main", "replace", "if",
        ] {
            assert!(!is_reserved(word), "{word}");
        }
        for word in ["abort", "key", "left", "rowid"] {
            assert!(
                accepts(&format!("CREATE TABLE {word}(a); SELECT * FROM {word}")),
                "{word}"
            );
        }
    }
}
