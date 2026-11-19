//! The words SQLite cannot take as a bare name (#2950).
//!
//! Measured on the bundled SQLite 3.54.1 against the 154 words of its keyword
//! list: these 58 are refused as a bare table name, as a bare column name and
//! after `FROM`; the other keywords (`abort`, `key`, `left`, `rowid`, `temp`,
//! `main`) stand bare. `if` alone is refused only as a table name and is left
//! out. Quoting always works, and this dialect quotes every name it writes, so
//! the list serves the scanners that read a bare word in a definition.
//!
//! A word of a declared type is another position with another rule
//! (`ends_type`): a reserved word ends the type name there and is read as the
//! start of a constraint, and eight more keywords that stand bare as a name
//! are a syntax error inside a type.

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

/// Keywords that stand bare as a name and still cannot be a word of a type:
/// the grammar takes the join words and `indexed` as a name, not as a type
/// word. Measured: each is a syntax error alone, first and last in a type.
const NOT_IN_A_TYPE: [&str; 8] = [
    "cross", "full", "indexed", "inner", "left", "natural", "outer", "right",
];

/// Whether `word` cannot be a word of a declared type: a reserved word, which
/// ends the type and starts a constraint, or one of the eight the grammar
/// refuses there.
#[must_use]
pub fn ends_type(word: &str) -> bool {
    is_reserved(word) || NOT_IN_A_TYPE.iter().any(|w| w.eq_ignore_ascii_case(word))
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

    /// SQLite's whole keyword list (3.54.1), for the tests that ask the engine
    /// about every one.
    const ALL: &str = "\
        abort action add after all alter always analyze and as asc attach autoincrement \
        before begin between by cascade case cast check collate column commit conflict \
        constraint create cross current current_date current_time current_timestamp database \
        default deferrable deferred delete desc detach distinct do drop each else end escape \
        except exclude exclusive exists explain fail filter first following for foreign from \
        full generated glob group groups having if ignore immediate in index indexed \
        initially inner insert instead intersect into is isnull join key last left like \
        limit match materialized natural no not nothing notnull null nulls of offset on or \
        order others outer over partition plan pragma preceding primary query raise range \
        recursive references regexp reindex release rename replace restrict returning right \
        rollback row rows savepoint select set table temp temporary then ties to transaction \
        trigger unbounded union unique update using vacuum values view virtual when where \
        window with without true false rowid oid _rowid_ main temp";

    /// The type the engine stores for a column declared with `ty`, or `None`
    /// where it refuses the statement. The column name is long on purpose:
    /// SQLite drops a trailing `always` only from a long enough declaration.
    fn stored_type(ty: &str) -> Option<String> {
        let mut fresh = Fresh::memory();
        let c = fresh.connection();
        c.execute_batch(&["CREATE TABLE t(a_long_column_name ", ty, ")"].concat())
            .ok()?;
        let rows = c
            .query("SELECT type FROM pragma_table_xinfo('t')", &[])
            .expect("the column");
        Some(rows[0].text(0).expect("a type").to_owned())
    }

    /// Every keyword, alone, first and last in a type: `normalize` takes the
    /// type exactly when the engine stores it as written. A keyword list made
    /// for bare names is not this rule (`left` stands bare and is refused
    /// here), so the engine is asked about each.
    #[test]
    fn a_keyword_is_a_type_word_exactly_when_the_engine_keeps_it() {
        assert_eq!(ALL.split_ascii_whitespace().count(), 154);
        let mut refused = 0;
        for word in ALL.split_ascii_whitespace() {
            for ty in [
                word.to_owned(),
                ["int ", word].concat(),
                [word, " int"].concat(),
            ] {
                let kept = stored_type(&ty).is_some_and(|back| back.eq_ignore_ascii_case(&ty));
                let parsed: pbps_model::ColumnType = ty.parse().expect("a parseable type");
                let taken = crate::types::normalize(&parsed).is_ok();
                assert_eq!(kept, taken, "`{ty}`");
                refused += usize::from(!taken);
            }
        }
        // Negative control: the loop saw both answers.
        assert!(refused > 150 && refused < 154 * 3 - 150, "{refused}");
    }

    /// SQLite drops a trailing `always`, and a `generated` before it, from the
    /// type it stores, once the type's text is 16 bytes or longer and has no
    /// arguments. It looks at the last six bytes and not at a word. Such a
    /// type would not read back as declared, so it is refused; a shorter one
    /// is kept by the engine and taken here.
    #[test]
    fn a_long_type_ending_in_always_is_cut_by_the_engine_and_refused_here() {
        let taken = |ty: &str| {
            let parsed: pbps_model::ColumnType = ty.parse().expect("a parseable type");
            crate::types::normalize(&parsed).is_ok()
        };
        for (ty, stored) in [
            ("int generated always", "INT"),
            ("integer generated always", "integer"),
            ("unsigned int always", "unsigned int"),
            ("some_type_always", "some_type_"),
            ("UNSIGNED INT ALWAYS", "UNSIGNED INT"),
        ] {
            // Compared without case: SQLite stores its own upper-case
            // spelling when what is left is one of its standard names.
            assert!(
                stored_type(ty).is_some_and(|back| back.eq_ignore_ascii_case(stored)),
                "{ty}"
            );
            assert!(!taken(ty), "{ty}");
        }
        // Negative: under 16 bytes, elsewhere in the type, or before arguments
        // the engine keeps the text, and so does this.
        for ty in [
            "int always",
            "always",
            "some_typealways",
            "always unsigned big int",
            "generated unsigned int",
            "unsigned int always(5)",
        ] {
            assert!(
                stored_type(ty).is_some_and(|back| back.eq_ignore_ascii_case(ty)),
                "{ty}"
            );
            assert!(taken(ty), "{ty}");
        }
    }
}
