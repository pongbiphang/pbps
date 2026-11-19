//! Names: quoting, comparison, the reserved prefix and the lexicon (#2950).
//!
//! Every rule here was measured on SQLite 3.54.1 and is run against the engine
//! in both directions by the tests: a name the engine takes is quoted, one it
//! compares alike folds alike, and what is refused is refused for a reason the
//! tests show.

use std::borrow::Cow;

use pbps_dialect::{DialectError, Lexicon};

use crate::keywords;

/// Writes an identifier so SQLite reads it back as the same name: in double
/// quotes with each `"` doubled. Refuses the empty name, which SQLite accepts
/// and which no plan a person reads can show (a known, pinned gap), and a NUL,
/// which ends the statement (SQLite refuses it as an unrecognized token).
///
/// Quotes every name, so a keyword or a name with a space needs no special
/// case, and `"` is the form SQLite reads as an identifier first and a string
/// only where no column matches.
pub fn quote(ident: &str) -> Result<String, DialectError> {
    if ident.is_empty() || ident.contains('\0') {
        return Err(DialectError::UnquotableIdent(ident.to_owned()));
    }
    Ok(["\"", &ident.replace('"', "\"\""), "\""].concat())
}

/// The form two names share when SQLite holds them as one: ASCII letters
/// folded to lower case and nothing else. Measured: `é` and `É`, `ß` and `SS`,
/// `Σ` and `σ`, full-width letters, the Kelvin sign and `k` are all different
/// names; only `A` to `Z` fold.
#[must_use]
pub fn fold(ident: &str) -> Cow<'_, str> {
    if ident.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(ident.to_ascii_lowercase())
    } else {
        Cow::Borrowed(ident)
    }
}

/// Whether SQLite holds the two names as one.
#[must_use]
pub fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Whether a table, index, view or trigger name is reserved for SQLite's own
/// objects: it starts with `sqlite_`, ASCII case ignored, quoted or not. A
/// column may carry the prefix.
#[must_use]
pub fn has_reserved_prefix(name: &str) -> bool {
    name.get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("sqlite_"))
}

/// Whether `c` can continue a bare name once one has started: ASCII letters
/// and digits, `_`, `$` and every non-ASCII character (a no-break space is one).
#[must_use]
pub fn identifier_continues(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii()
}

/// What the shared definition scanner is told about SQLite's lexis. It is an
/// approximation in two places the type cannot express, and `lexis` holds the
/// exact reading where it matters (see there): block comments do not nest, and
/// vertical tab is not white space.
///
/// `"…"`, `` `…` `` and `[…]` quote a name (the first two double their closer,
/// brackets do not). `E'…'`, `N'…'`, `B'…'` and `U&'…'` are a name followed by
/// a string, `$$…$$` is a parameter, and a backslash escapes nothing; only the
/// blob prefix `x` joins a string.
pub const LEXICON: Lexicon = Lexicon {
    whitespace_is_ascii: true,
    quoted_identifiers: &[('"', '"'), ('`', '`'), ('[', ']')],
    escape_strings: false,
    dollar_quoted_strings: false,
    string_prefixes: &["x"],
    identifier_continues,
    reserved: keywords::is_reserved,
    unicode_identifiers: false,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{Fresh, accepts, refusal};

    /// A name goes in as `quote` writes it and comes out of the catalog as the
    /// same bytes: every ASCII punctuation character, every quote form's own
    /// closer, a keyword, layout, non-ASCII, a long name.
    #[test]
    fn a_quoted_name_is_read_back_as_the_same_name() {
        let mut names: Vec<String> = (0x01u8..0x7f)
            .filter(|b| !b.is_ascii_alphanumeric())
            .map(|b| ["a", &char::from(b).to_string(), "b"].concat())
            .collect();
        names.extend(
            [
                "a\"b", "\"", "\"\"", "[a]", "a`b", "'", "select", "main", "sqlite", " ", "a b.c",
                "a\nb", "a\tb", "é", "日本", "a\u{a0}b", "--", "/*", "x'41'",
            ]
            .map(str::to_owned),
        );
        names.push("n".repeat(1000));
        for name in &names {
            let mut fresh = Fresh::memory();
            let c = fresh.connection();
            let ddl = ["CREATE TABLE ", &quote(name).expect("quotable"), "(c)"].concat();
            c.execute_batch(&ddl)
                .unwrap_or_else(|e| panic!("{name:?}: {e}"));
            let rows = c
                .query("SELECT name FROM sqlite_schema", &[])
                .expect("catalog");
            assert_eq!(rows[0].text(0).expect("name"), name, "{name:?}");
        }
    }

    /// The two names `quote` refuses. A NUL really does end the statement for
    /// SQLite, so it is refused for the engine's own reason; the empty name is
    /// **valid SQLite refused on purpose** (a name a plan cannot show), pinned.
    #[test]
    fn the_names_quote_refuses_are_a_nul_and_the_empty_name() {
        assert!(matches!(quote(""), Err(DialectError::UnquotableIdent(_))));
        assert!(matches!(
            quote("a\0b"),
            Err(DialectError::UnquotableIdent(_))
        ));
        assert!(
            !accepts("CREATE TABLE \"a\0b\"(c)"),
            "the engine refuses a NUL"
        );
        assert!(
            accepts("CREATE TABLE \"\"(c)"),
            "the engine takes the empty name: a known gap"
        );
    }

    /// Two names fold alike exactly when the engine holds them as one, for
    /// every kind of object that shares the tables' namespace.
    #[test]
    fn names_fold_alike_exactly_when_the_engine_holds_them_as_one() {
        let pairs = [
            ("a", "A"),
            ("Parent", "PARENT"),
            ("a b", "A B"),
            ("é", "É"),
            ("ß", "SS"),
            ("ǅ", "ǆ"),
            ("İ", "i"),
            ("ı", "I"),
            ("Σ", "σ"),
            ("ς", "σ"),
            ("Ａ", "ａ"),
            ("\u{212a}", "k"),
            ("ſ", "s"),
            ("a\u{300}", "à"),
            ("x", "x "),
            ("é", "é"),
            ("a", "b"),
        ];
        for (a, b) in pairs {
            let (qa, qb) = (quote(a).expect("quote"), quote(b).expect("quote"));
            let table_table =
                !accepts(&["CREATE TABLE ", &qa, "(c); CREATE TABLE ", &qb, "(c)"].concat());
            let table_view = !accepts(
                &[
                    "CREATE TABLE ",
                    &qa,
                    "(c); CREATE VIEW ",
                    &qb,
                    " AS SELECT 1",
                ]
                .concat(),
            );
            let table_index = !accepts(
                &[
                    "CREATE TABLE t(c); CREATE TABLE ",
                    &qa,
                    "(c); CREATE INDEX ",
                    &qb,
                    " ON t(c)",
                ]
                .concat(),
            );
            let index_index = !accepts(
                &[
                    "CREATE TABLE t(c); CREATE TABLE u(c); CREATE INDEX ",
                    &qa,
                    " ON t(c); CREATE INDEX ",
                    &qb,
                    " ON u(c)",
                ]
                .concat(),
            );
            for (what, got) in [
                ("tables", table_table),
                ("table and view", table_view),
                ("table and index", table_index),
                ("indexes", index_index),
            ] {
                assert_eq!(got, same(a, b), "{what}: {a:?} and {b:?}");
            }
            assert_eq!(fold(a) == fold(b), same(a, b), "{a:?} {b:?}");
        }
        // A trigger has a namespace of its own: the same name beside a table is fine.
        assert!(accepts(
            "CREATE TABLE t(c); CREATE TRIGGER t AFTER INSERT ON t BEGIN SELECT 1; END"
        ));
        assert!(!accepts(
            "CREATE TABLE t(c); CREATE TRIGGER x AFTER INSERT ON t BEGIN SELECT 1; END; CREATE TRIGGER X AFTER DELETE ON t BEGIN SELECT 1; END"
        ));
        // Columns of one table fold the same way.
        assert!(!accepts("CREATE TABLE t(Id, id)") && accepts("CREATE TABLE t(é, É)"));
        // No Unicode folding, and no other change: only A to Z.
        assert_eq!(fold("ÀB"), "Àb");
        assert!(matches!(fold("ab"), Cow::Borrowed(_)));
    }

    /// The reserved prefix, against the engine for each kind of object, quoted
    /// or not, and on a column, which may carry it.
    #[test]
    fn the_reserved_prefix_is_what_the_engine_refuses() {
        let names = [
            "sqlite_x",
            "SQLITE_x",
            "SQLite_",
            "sqlite_",
            "sqlitex",
            "sqlite",
            "ſqlite_x",
            " sqlite_x",
            "xsqlite_x",
            "sqlite-x",
            "sqlit_e",
        ];
        for name in names {
            let q = quote(name).expect("quote");
            for (kind, sql) in [
                ("table", ["CREATE TABLE ", &q, "(c)"].concat()),
                ("view", ["CREATE VIEW ", &q, " AS SELECT 1"].concat()),
                (
                    "index",
                    ["CREATE TABLE t(c); CREATE INDEX ", &q, " ON t(c)"].concat(),
                ),
                (
                    "trigger",
                    [
                        "CREATE TABLE t(c); CREATE TRIGGER ",
                        &q,
                        " AFTER INSERT ON t BEGIN SELECT 1; END",
                    ]
                    .concat(),
                ),
            ] {
                let refused = refusal(&sql).contains("reserved for internal use");
                assert_eq!(refused, has_reserved_prefix(name), "{kind} {name:?}");
            }
        }
        assert!(
            accepts("CREATE TABLE t(sqlite_a)"),
            "a column may carry the prefix"
        );
    }

    /// A character continues a bare name exactly when the engine reads it as
    /// part of one: the column comes back named `a<c>b`.
    #[test]
    fn identifier_characters_are_what_the_engine_reads_as_one_name() {
        let mut chars: Vec<char> = (0x01u8..0x80).map(char::from).collect();
        chars.extend(['é', '日', '\u{a0}', '\u{2003}', '\u{200b}', '\u{85}']);
        for c in chars {
            let name = ["a", &c.to_string(), "b"].concat();
            let mut fresh = Fresh::memory();
            let c_conn = fresh.connection();
            let one_name = c_conn
                .execute_batch(&["CREATE TABLE t(", &name, ")"].concat())
                .is_ok()
                && c_conn
                    .query("SELECT name FROM pragma_table_xinfo('t')", &[])
                    .map(|rows| rows.len() == 1 && rows[0].text(0).ok() == Some(name.as_str()))
                    .unwrap_or(false);
            assert_eq!(one_name, identifier_continues(c), "{c:?}");
        }
    }

    /// What the lexicon says about string prefixes and quotes, run on the
    /// engine: only `x` makes a string, the others are a name then a string; a
    /// quote form is a name; a backslash escapes nothing.
    #[test]
    fn the_lexicon_describes_what_the_engine_reads() {
        let mut fresh = Fresh::memory();
        let c = fresh.connection();
        let error = |sql: &str| c.query(sql, &[]).err().map(|e| e.to_string());
        assert_eq!(LEXICON.string_prefixes, ["x"]);
        assert!(error("SELECT x'41'").is_none() && error("SELECT X'41'").is_none());
        for prefix in ["E", "N", "B", "U&"] {
            let sql = ["SELECT ", prefix, "'a'"].concat();
            assert!(
                error(&sql).is_some_and(|e| e.contains("no such column")),
                "{sql}"
            );
        }
        assert!(
            !LEXICON.escape_strings && error("SELECT 'a\\'b'").is_some(),
            "a backslash escapes nothing"
        );
        assert!(
            !LEXICON.dollar_quoted_strings
                && error("SELECT $$a$$").is_some_and(|e| e.contains("parameter"))
        );
        assert!(
            LEXICON.quoted_identifiers.contains(&('"', '"'))
                && error("SELECT \"a\"\"b\" FROM (SELECT 1 AS \"a\"\"b\")").is_none()
        );
        assert!(
            LEXICON.quoted_identifiers.contains(&('`', '`'))
                && error("SELECT `a``b` FROM (SELECT 1 AS `a``b`)").is_none()
        );
        assert!(
            LEXICON.quoted_identifiers.contains(&('[', ']'))
                && error("SELECT [a] FROM (SELECT 1 AS [a])").is_none()
        );
        // Brackets do not double their closer: `[a]]b]` is `[a]` and then trouble.
        assert!(error("SELECT [a]]b] FROM (SELECT 1 AS [a]]b])").is_some());
        assert!(!std::hint::black_box(LEXICON).unicode_identifiers);
    }
}
