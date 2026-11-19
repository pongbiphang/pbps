//! SQLite's lexis, read exactly where the shared scanner cannot (#2950).
//!
//! The shared [`pbps_dialect::Lexicon`] scanner nests block comments and counts
//! vertical tab as white space. SQLite does neither: a comment ends at the
//! first `*/` (`SELECT 1 /* a /* b */ c */` is a syntax error), and white space
//! is space, tab, line feed, form feed and carriage return only. Text after
//! the first `*/` that the shared scanner would still call a comment is code to
//! SQLite, so a `;` hidden there starts another statement. Anything that must
//! not let a statement through (a view body, a check expression, a default)
//! is read here.

/// One run of a definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece<'a> {
    /// Code: anything that is not one of the others.
    Code(&'a str),
    /// A `'…'` string, or a `"…"`, `` `…` `` or `[…]` name, closers included.
    Quoted(&'a str),
    /// A `--` comment (to the end of the line) or a `/*…*/` comment (to the
    /// first `*/`, or to the end of the text if it never closes).
    Comment(&'a str),
}

/// The length of the quoted run at the start of `rest`, and whether it closes.
/// An unterminated one runs to the end of the text, as SQLite reads it (and
/// refuses it).
fn quoted_len(rest: &str, quote: char) -> (usize, bool) {
    let mut at = quote.len_utf8();
    while let Some(found) = rest[at..].find(quote) {
        at += found + quote.len_utf8();
        if rest[at..].starts_with(quote) {
            at += quote.len_utf8();
        } else {
            return (at, true);
        }
    }
    (rest.len(), false)
}

/// Whether `text` is exactly one complete `'…'` string, quotes doubled inside.
#[must_use]
pub fn is_one_string(text: &str) -> bool {
    text.starts_with('\'') && quoted_len(text, '\'') == (text.len(), true)
}

/// The runs of `text`, in order, which together are `text`.
#[must_use]
pub fn pieces(text: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < text.len() {
        let rest = &text[i..];
        let len = if rest.starts_with("--") {
            Some((rest.find('\n').unwrap_or(rest.len()), true))
        } else if let Some(after) = rest.strip_prefix("/*") {
            Some((after.find("*/").map_or(rest.len(), |at| at + 4), true))
        } else if rest.starts_with('[') {
            Some((rest.find(']').map_or(rest.len(), |at| at + 1), false))
        } else {
            match rest.chars().next() {
                Some(q @ ('\'' | '"' | '`')) => Some((quoted_len(rest, q).0, false)),
                _ => None,
            }
        };
        match len {
            Some((len, comment)) => {
                if start < i {
                    out.push(Piece::Code(&text[start..i]));
                }
                let run = &text[i..i + len];
                out.push(if comment {
                    Piece::Comment(run)
                } else {
                    Piece::Quoted(run)
                });
                i += len;
                start = i;
            }
            None => i += rest.chars().next().map_or(1, char::len_utf8),
        }
    }
    if start < text.len() {
        out.push(Piece::Code(&text[start..]));
    }
    out
}

/// SQLite's white space: nothing else separates tokens.
#[must_use]
pub fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0c' | '\r')
}

/// The definition with strings and comments blanked to spaces of the same
/// length, and names kept: the text a dependency scan reads. A blob prefix `x`
/// goes with its string, as a name that happened to be `x` would otherwise
/// look referenced.
#[must_use]
pub fn code_only(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for piece in pieces(text) {
        match piece {
            Piece::Code(code) => out.push_str(code),
            Piece::Comment(run) => out.push_str(&" ".repeat(run.len())),
            Piece::Quoted(run) if run.starts_with('\'') => {
                let prefix = out
                    .chars()
                    .last()
                    .filter(|c| matches!(c, 'x' | 'X'))
                    .is_some()
                    && !out[..out.len() - 1]
                        .chars()
                        .last()
                        .is_some_and(crate::ident::identifier_continues);
                if prefix {
                    out.pop();
                    out.push(' ');
                }
                out.push_str(&" ".repeat(run.len()));
            }
            Piece::Quoted(run) => out.push_str(run),
        }
    }
    out
}

/// Whether the text holds a token: anything but white space and comments. A
/// string or a quoted name is a token, and can be a whole expression.
#[must_use]
pub fn has_token(text: &str) -> bool {
    pieces(text).iter().any(|piece| match piece {
        Piece::Code(code) => code.chars().any(|c| !is_space(c)),
        Piece::Quoted(_) => true,
        Piece::Comment(_) => false,
    })
}

/// Whether a quoted run or a comment ends where SQLite ends it. One that does
/// not runs to the end of the text, and takes with it whatever is written
/// after the text: a `--` comment with no line end, a `/*` with no `*/`, a
/// string or a quoted name with no closer.
pub(crate) fn closes(piece: Piece<'_>) -> bool {
    match piece {
        Piece::Code(_) => true,
        Piece::Comment(run) if run.starts_with("--") => false,
        Piece::Comment(run) => run.len() >= 4 && run.ends_with("*/"),
        Piece::Quoted(run) if run.starts_with('[') => run.ends_with(']'),
        Piece::Quoted(run) => run
            .chars()
            .next()
            .is_some_and(|quote| quoted_len(run, quote) == (run.len(), true)),
    }
}

/// Whether `text` can stand inside a pair of parentheses as one expression:
/// its code closes every parenthesis it opens and never closes one it did
/// not, so it cannot end a wrapper placed around it; it holds no `;`; no
/// group is empty unless a name or `IN` comes before it (`random()`, `c IN
/// ()`), and no other keyword does (`AND ()`);
/// and no comment or quoted run is left open at the end, where it would
/// swallow the wrapper's own closing parenthesis. A `--` comment is closed
/// only by a line end that follows it.
#[must_use]
pub fn is_one_expression(text: &str) -> bool {
    // One entry per open group: whether a name came before it, and whether
    // anything has been written inside it.
    let mut groups: Vec<(bool, bool)> = Vec::new();
    // The bare word just read, if the last thing read was one.
    let mut word = String::new();
    // Whether layout or a comment has ended that word: the next name
    // character starts another.
    let mut separated = false;
    let mut after_quoted = false;
    let all = pieces(text);
    for (at, piece) in all.iter().enumerate() {
        match *piece {
            Piece::Comment(run) => {
                let line = run.starts_with("--");
                let ended = if line {
                    all.get(at + 1).is_some()
                } else {
                    closes(*piece)
                };
                if !ended {
                    return false;
                }
                // A comment separates two words as layout does.
                separated = true;
            }
            Piece::Quoted(_) => {
                if !closes(*piece) {
                    return false;
                }
                if let Some(group) = groups.last_mut() {
                    group.1 = true;
                }
                // A quoted name may be called; a string may not, and the
                // engine says so.
                word.clear();
                after_quoted = true;
            }
            Piece::Code(code) => {
                for c in code.chars() {
                    match c {
                        ';' => return false,
                        '(' => {
                            let named = after_quoted
                                || (!word.is_empty()
                                    && (!crate::keywords::is_reserved(&word)
                                        || word.eq_ignore_ascii_case("in")));
                            groups.push((named, false));
                            word.clear();
                            after_quoted = false;
                        }
                        ')' => {
                            let Some((named, filled)) = groups.pop() else {
                                return false;
                            };
                            if !named && !filled {
                                return false;
                            }
                            if let Some(group) = groups.last_mut() {
                                group.1 = true;
                            }
                            word.clear();
                            after_quoted = false;
                        }
                        c if is_space(c) => separated = true,
                        c => {
                            if let Some(group) = groups.last_mut() {
                                group.1 = true;
                            }
                            if !crate::ident::identifier_continues(c) || after_quoted || separated {
                                word.clear();
                            }
                            separated = false;
                            if crate::ident::identifier_continues(c) {
                                word.push(c);
                            }
                            after_quoted = false;
                        }
                    }
                }
            }
        }
    }
    groups.is_empty()
}

/// Whether anything but `;` and layout follows the first `;` in code, which
/// is a second statement SQLite would run.
#[must_use]
pub fn has_second_statement(text: &str) -> bool {
    let mut after_semicolon = false;
    for piece in pieces(text) {
        match piece {
            Piece::Code(code) => {
                for c in code.chars() {
                    if c == ';' {
                        after_semicolon = true;
                    } else if after_semicolon && !is_space(c) {
                        return true;
                    }
                }
            }
            Piece::Quoted(_) if after_semicolon => return true,
            Piece::Quoted(_) | Piece::Comment(_) => {}
        }
    }
    false
}

/// Whether the code of `text` holds `word` as a bare word of its own, ASCII
/// case ignored: not inside a string, a quoted name or a comment, and not as
/// part of a longer name.
#[must_use]
pub fn has_bare_word(text: &str, word: &str) -> bool {
    pieces(text).iter().any(|piece| match piece {
        Piece::Code(code) => code
            .split(|c| !crate::ident::identifier_continues(c))
            .any(|found| found.eq_ignore_ascii_case(word)),
        Piece::Quoted(_) | Piece::Comment(_) => false,
    })
}

/// The first word of the code, lower-cased, skipping layout and comments: what
/// a statement starts with.
#[must_use]
pub fn first_word(text: &str) -> Option<String> {
    for piece in pieces(text) {
        match piece {
            Piece::Code(code) => {
                let trimmed = code.trim_start_matches(is_space);
                if trimmed.is_empty() {
                    continue;
                }
                // The whole name, by SQLite's rule for one: `SELECT$x` and
                // `SELECTé` are one word each, and neither is `SELECT`.
                let word: String = trimmed
                    .chars()
                    .take_while(|c| crate::ident::identifier_continues(*c))
                    .collect();
                return Some(word.to_ascii_lowercase());
            }
            Piece::Quoted(_) => return None,
            Piece::Comment(_) => {}
        }
    }
    None
}

/// The name a quoted run spells, its quotes undone: `"a""b"` and `` `a``b` ``
/// double their closer, `[a b]` does not, and `'a''b'` is a string that
/// SQLite also takes as a name in a few places. `None` for a run that is not
/// one of the four, or that does not close.
#[must_use]
pub fn unquote(run: &str) -> Option<String> {
    let quote = run.chars().next()?;
    if quote == '[' {
        return run.strip_suffix(']').map(|rest| rest[1..].to_owned());
    }
    if !matches!(quote, '"' | '`' | '\'') || quoted_len(run, quote) != (run.len(), true) {
        return None;
    }
    let inner = &run[1..run.len() - 1];
    let doubled: String = [quote, quote].iter().collect();
    Some(inner.replace(&doubled, &quote.to_string()))
}

/// Whether the code of `text` may name `name`: as a bare word, or as a quoted
/// name (`"…"`, `` `…` `` or `[…]`, quotes undoubled), never in a comment or a
/// `'…'` string. `same` says when two spellings are one name. An
/// over-approximation on purpose, as every dependency scan is: a double-quoted
/// word SQLite falls back to reading as a string counts as a name, and a column
/// of another table with the same name counts too. A false yes costs a view
/// torn down and built again; a false no costs an apply the engine refuses.
#[must_use]
pub fn names(text: &str, name: &str, same: impl Fn(&str, &str) -> bool) -> bool {
    pieces(text).iter().any(|piece| match piece {
        Piece::Code(code) => code
            .split(|c| !crate::ident::identifier_continues(c))
            .any(|word| !word.is_empty() && same(word, name)),
        Piece::Quoted(run) => match run.chars().next() {
            Some(q @ ('"' | '`')) => {
                let inner = run[1..].strip_suffix(q).unwrap_or(&run[1..]);
                same(
                    &inner.replace(&[q, q].iter().collect::<String>(), &q.to_string()),
                    name,
                )
            }
            Some('[') => same(run[1..].strip_suffix(']').unwrap_or(&run[1..]), name),
            _ => false,
        },
        Piece::Comment(_) => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{Fresh, accepts};

    /// View bodies, with whether SQLite runs a second statement in them (a
    /// table named `smuggled` appears). Each way a `;` can be hidden, and each
    /// way it cannot.
    const BODIES: [(&str, bool); 17] = [
        ("SELECT 1; CREATE TABLE smuggled(a)", true),
        ("SELECT 1;\nCREATE TABLE smuggled(a)", true),
        ("SELECT 1 /* /* */ ; CREATE TABLE smuggled(a) /* */", true),
        ("SELECT 1 /* a /* b */ ; CREATE TABLE smuggled(a)", true),
        ("SELECT 1 /* x */ ; CREATE TABLE smuggled(a)", true),
        ("SELECT 'x;CREATE TABLE smuggled(a)'", false),
        ("SELECT 1 -- ; CREATE TABLE smuggled(a)\n", false),
        ("SELECT 1 --\r; CREATE TABLE smuggled(a)", false),
        ("SELECT 1 /* ; CREATE TABLE smuggled(a) */", false),
        ("SELECT 1 /* unterminated ; CREATE TABLE smuggled(a)", false),
        ("SELECT \"a;b\"", false),
        ("SELECT [a;b] FROM (SELECT 1 AS [a;b])", false),
        ("SELECT `a;b` FROM (SELECT 1 AS `a;b`)", false),
        ("SELECT 1;", false),
        ("SELECT 1;;", false),
        ("SELECT 1; -- done\n", false),
        ("SELECT 1 ; /* done */ ", false),
    ];

    /// Whether the engine ran a statement after the view: it made the table.
    fn smuggled(body: &str) -> bool {
        let mut fresh = Fresh::memory();
        let c = fresh.connection();
        let _ = c.execute_batch(&["CREATE VIEW v AS ", body].concat());
        c.query(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'smuggled'",
            &[],
        )
        .map(|rows| rows[0].integer(0).unwrap_or(0) == 1)
        .unwrap_or(false)
    }

    #[test]
    fn a_second_statement_is_found_exactly_when_the_engine_runs_one() {
        for (body, runs) in BODIES {
            assert_eq!(smuggled(body), runs, "the engine on {body:?}");
            assert_eq!(
                has_second_statement(body),
                runs,
                "has_second_statement on {body:?}"
            );
        }
    }

    /// The reason this module exists. The shared scanner nests block comments,
    /// so after `/* /* */` it is still inside a comment and sees no `;`, where
    /// SQLite has left the comment and runs what follows. Known gap in the
    /// shared `Lexicon` for route steps 4 to 6; `Sqlite::code_only` and every
    /// check here read with `lexis` instead.
    #[test]
    fn the_shared_scanner_hides_a_statement_behind_a_nested_comment_and_lexis_does_not() {
        let body = "SELECT 1 /* /* */ ; CREATE TABLE smuggled(a) /* */";
        assert!(smuggled(body), "the engine runs the second statement");
        assert!(
            !crate::ident::LEXICON.code_only(body).contains(';'),
            "the shared scanner sees none"
        );
        assert!(code_only(body).contains(';') && has_second_statement(body));
    }

    /// White space is five characters, against the engine: a character
    /// separates two tokens exactly when `is_space` says so. Vertical tab, a
    /// no-break space, NEL and an em space do not.
    #[test]
    fn white_space_is_what_the_engine_separates_tokens_by() {
        let mut chars: Vec<char> = (0x01u8..0x80).map(char::from).collect();
        chars.extend(['\u{a0}', '\u{85}', '\u{2003}', '\u{200b}']);
        for c in chars {
            let sql = ["SELECT 1", &c.to_string(), "FROM(SELECT 1)"].concat();
            assert_eq!(accepts(&sql), is_space(c), "{c:?}");
        }
    }

    /// Comments end where the engine ends them: `--` at a line feed only, `/*`
    /// at the first `*/` or at the end of the text.
    #[test]
    fn comments_end_where_the_engine_ends_them() {
        assert!(accepts("SELECT 1 /* a /* b */") && !accepts("SELECT 1 /* a /* b */ c */"));
        assert_eq!(
            pieces("a /* b /* c */ d */"),
            [
                Piece::Code("a "),
                Piece::Comment("/* b /* c */"),
                Piece::Code(" d */")
            ]
        );
        assert_eq!(
            pieces("a --b\r\nc"),
            [
                Piece::Code("a "),
                Piece::Comment("--b\r"),
                Piece::Code("\nc")
            ]
        );
        assert_eq!(
            pieces("a /* open"),
            [Piece::Code("a "), Piece::Comment("/* open")]
        );
    }

    #[test]
    fn the_runs_of_a_text_are_the_text() {
        for text in [
            "",
            "SELECT 1",
            "a 'b''c' \"d\"\"e\" `f``g` [h] -- i\nj /* k */ l",
            "'unterminated",
            "\"open",
            "[open",
            "é 'ü' /* ñ */ x'41'",
            "a--b",
            "/*/",
        ] {
            assert_eq!(
                pieces(text)
                    .iter()
                    .map(|p| match p {
                        Piece::Code(s) | Piece::Quoted(s) | Piece::Comment(s) => *s,
                    })
                    .collect::<String>(),
                text
            );
        }
    }

    #[test]
    fn code_only_blanks_strings_and_comments_keeps_names_and_keeps_the_length() {
        let text = "SELECT x'41', 'a;b' /* c */, \"d;e\" -- f\nFROM t WHERE ex'1' = 1";
        let blanked = code_only(text);
        assert_eq!(blanked.len(), text.len());
        assert!(
            !blanked.contains("a;b") && !blanked.contains("/* c */") && !blanked.contains("-- f")
        );
        assert!(blanked.contains("\"d;e\"") && blanked.contains("FROM t"));
        // The blob prefix goes with its string, but a name ending in `x` stays.
        assert!(!blanked.contains("x'") && blanked.contains("ex"));
        assert_eq!(code_only("é 'ü'").len(), "é 'ü'".len());
    }

    #[test]
    fn a_bare_word_is_found_only_as_code_and_only_whole() {
        for text in ["a IN (SELECT 1)", "select", "(sElEcT\t1)", "x,select,y"] {
            assert!(has_bare_word(text, "select"), "{text:?}");
        }
        for text in [
            "'select'",
            "\"select\"",
            "`select`",
            "[select]",
            "-- select\n1",
            "/* select */ 1",
            "selected",
            "preselect",
            "select$",
            "sélect",
            "",
        ] {
            assert!(!has_bare_word(text, "select"), "{text:?}");
        }
    }

    #[test]
    fn tokens_expressions_and_first_words_are_read_with_sqlite_s_lexis() {
        for text in ["1", "'x'", "\"a\"", "[a]", "a /* c */"] {
            assert!(has_token(text), "{text:?}");
        }
        for text in ["", "  \t\n", "/* c */", "-- c", "/* a */ -- b"] {
            assert!(!has_token(text), "{text:?}");
        }
        assert!(
            is_one_expression("(a) + (b)")
                && is_one_expression("f(')')")
                && is_one_expression("a -- )\n")
        );
        for text in [
            "a) or (b",
            "(a",
            "a)",
            ";",
            "a; b",
            "a /* /* */ ) ; ( /* */ b",
        ] {
            assert!(!is_one_expression(text), "{text:?}");
        }
        assert_eq!(
            first_word(" /* c */ -- d\n With x").as_deref(),
            Some("with")
        );
        assert_eq!(first_word("'x'"), None);
        assert_eq!(first_word(""), None);
        assert!(!has_second_statement("SELECT 1") && has_second_statement("SELECT 1; x"));
    }
}
