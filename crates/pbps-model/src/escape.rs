//! Text that came from a file, shown in a sentence of our own.
//!
//! A saved plan is read by someone who may not have written it, and a refusal
//! that names a value out of that file prints whatever the file holds. A line
//! break in it starts a line the tool never wrote, and an escape sequence
//! rewrites or erases the lines before it, so a hand-made plan could dress its
//! refusal as other output. [`Escaped`] is the one way such a value is put into
//! a refusal (DEC-2032.1).
//!
//! # Why the printing side, and not the decode
//!
//! Refusing a control character when the plan is decoded would be a smaller
//! surface to reason about, but it cannot be applied to the strings that matter
//! most. A declared name may hold any character, controls included — the
//! formatter keeps them as double-quoted escapes precisely so that the text
//! round-trips (SPEC §4.3, DEC-1801.1) — so a plan that carries one is a valid
//! plan, and refusing it at decode would refuse it for a name nothing is wrong
//! with. Escaping only changes how the value is *shown*: the file is not read
//! differently, the checksum does not move, and the reviewer still sees what
//! the file held, which a refusal that only said "invalid" would hide.
//!
//! What is escaped is a control character, a line or paragraph separator, and
//! the invisible characters that reorder or hide text (zero-width and
//! bidirectional marks), together with the other default-ignorable characters
//! no terminal draws. Every other character, including every non-ASCII letter
//! and every variation selector, is shown as it is: an identifier is allowed to
//! be Unicode.

use std::fmt;

/// A [`Display`](fmt::Display) adapter for artifact-controlled text.
///
/// The backslash is escaped as well. Otherwise the two characters `\n` written
/// in a file and the line break escaped from it would read the same, and an
/// escape that cannot be told apart from the text is not showing what was there.
///
/// That holds for a decoder's sentence too, which quotes some of the text it
/// stopped at with Rust's `Debug` escapes (`string "a\nb"`) and some raw (an
/// unknown variant's name). Doubling the first form shows a line break as `\\n`
/// and a backslash as `\\\\`, which reads oddly but loses nothing; leaving the
/// backslash alone would show the raw-quoted `\n` the same whether the file held
/// a line break or the two characters (DEC-2032.1, #2038).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Escaped<'a>(pub &'a str);

impl fmt::Display for Escaped<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_inert(f, self.0)
    }
}

fn write_inert(f: &mut fmt::Formatter<'_>, text: &str) -> fmt::Result {
    use fmt::Write as _;
    for c in text.chars() {
        match c {
            '\n' => f.write_str("\\n")?,
            '\r' => f.write_str("\\r")?,
            '\t' => f.write_str("\\t")?,
            '\\' => f.write_str("\\\\")?,
            c if hides_or_moves(c) => write!(f, "\\u{{{:x}}}", u32::from(c))?,
            c => f.write_char(c)?,
        }
    }
    Ok(())
}

/// Whether showing `c` as itself could change what the lines around it say.
///
/// `char::is_control` is the C0 and C1 controls and DEL, which covers the
/// escape character that starts a terminal sequence and the single-character
/// C1 introducer (U+009B) that starts one on some terminals. The explicit
/// ranges are the characters Unicode treats as format or separator and that no
/// terminal draws: the line and paragraph separators, the zero-width and
/// directional marks, the embeddings, overrides and isolates, the byte order
/// mark, and the rest of the default-ignorable set that draws nothing by
/// itself (the soft hyphen, the grapheme joiner, the Hangul and Khmer fillers,
/// the Mongolian vowel separator, the reserved U+FFF0..=U+FFF8, the shorthand
/// and musical format controls, the tag characters). The interlinear annotation
/// marks (U+FFF9..=U+FFFB) are not default-ignorable and are left alone. A
/// hand-made name carrying one would otherwise read the same as one without it
/// (#2040, #2063).
///
/// The variation selectors (U+FE00..=U+FE0F, the Mongolian U+180B..=U+180D and
/// U+180F, and U+E0100..=U+E01EF) are left alone on purpose. Each follows a
/// visible character in valid text, as the emoji presentation selector U+FE0F
/// does in `❤️` and an ideographic variation selector does in a CJK name, so
/// escaping them would rewrite names that are valid (DEC-2032.1).
fn hides_or_moves(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00ad}'
                | '\u{034f}'
                | '\u{061c}'
                | '\u{115f}'..='\u{1160}'
                | '\u{17b4}'..='\u{17b5}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{3164}'
                | '\u{feff}'
                | '\u{ffa0}'
                | '\u{fff0}'..='\u{fff8}'
                | '\u{1bca0}'..='\u{1bca3}'
                | '\u{1d173}'..='\u{1d17a}'
                | '\u{e0000}'..='\u{e00ff}'
                | '\u{e01f0}'..='\u{e0fff}'
        )
}

#[cfg(test)]
mod tests {
    use super::Escaped;

    fn shown(text: &str) -> String {
        Escaped(text).to_string()
    }

    #[test]
    fn a_line_break_and_a_terminal_sequence_are_shown_not_acted_on() {
        let text = shown("rule\nerror: forged\r\u{1b}[31mred\u{1b}[0m\u{9b}2J\u{7f}");
        assert_eq!(
            text,
            "rule\\nerror: forged\\r\\u{1b}[31mred\\u{1b}[0m\\u{9b}2J\\u{7f}"
        );
        assert!(!text.chars().any(char::is_control), "{text:?}");
    }

    #[test]
    fn characters_that_reorder_or_hide_text_are_shown() {
        for c in [
            '\u{200b}', '\u{200e}', '\u{2028}', '\u{2029}', '\u{202e}', '\u{2066}', '\u{feff}',
            '\u{61c}',
        ] {
            let text = shown(&format!("a{c}b"));
            assert_eq!(text, format!("a\\u{{{:x}}}b", u32::from(c)));
        }
    }

    #[test]
    fn zero_width_default_ignorable_characters_are_shown() {
        // The first and the last character of each range added for #2040 and
        // #2063 are shown, so a range cut short at either end fails here.
        for (first, last) in [
            ('\u{00ad}', '\u{00ad}'),
            ('\u{034f}', '\u{034f}'),
            ('\u{115f}', '\u{1160}'),
            ('\u{17b4}', '\u{17b5}'),
            ('\u{180e}', '\u{180e}'),
            ('\u{2065}', '\u{2065}'),
            ('\u{3164}', '\u{3164}'),
            ('\u{ffa0}', '\u{ffa0}'),
            ('\u{fff0}', '\u{fff8}'),
            ('\u{1bca0}', '\u{1bca3}'),
            ('\u{1d173}', '\u{1d17a}'),
            ('\u{e0000}', '\u{e00ff}'),
            ('\u{e01f0}', '\u{e0fff}'),
        ] {
            for c in [first, last] {
                let text = shown(&format!("a{c}b"));
                assert_eq!(text, format!("a\\u{{{:x}}}b", u32::from(c)));
            }
        }
    }

    #[test]
    fn a_variation_selector_after_a_visible_character_is_left_alone() {
        // Negative: the selectors that valid text puts after a base character
        // (emoji presentation, a CJK variant, a Mongolian letter) are not
        // rewritten, and nor is the visible character beside each added range.
        for name in [
            "\u{2764}\u{fe0f}",
            "\u{2764}\u{fe0e}",
            "\u{845b}\u{e0100}",
            "\u{845b}\u{e01ef}",
            "\u{1820}\u{180b}",
            "\u{1820}\u{180d}",
            "\u{1820}\u{180f}",
            "e\u{301}\u{350}",
            "\u{180a}\u{1810}",
            "\u{1161}\u{17b6}\u{3165}\u{ffa1}",
            "\u{1bc9f}\u{1d172}\u{1d17b}",
            "\u{fff9}\u{fffa}\u{fffb}",
        ] {
            assert_eq!(shown(name), name);
        }
    }

    #[test]
    fn unicode_names_and_ordinary_punctuation_are_left_alone() {
        // Negative: only the characters above change. A valid identifier is not
        // rewritten, so a refusal still names it the way the file spells it.
        for name in [
            "postgres",
            "ünïcode_名前",
            "pg-reported-scope-v1",
            "e\u{301}",
            "`quoted` 'single' \"double\" {} %s",
            "emoji 🙂",
            "\u{a0}non-breaking",
            // The neighbours of the soft hyphen, the not sign and the
            // registered sign, are drawn and stay as written (#2063).
            "\u{ac}\u{ae}",
            "",
        ] {
            assert_eq!(shown(name), name);
        }
    }

    #[test]
    fn the_rule_refusal_cannot_print_its_identifier_raw() {
        use crate::resolver::EvidenceError;
        let message = EvidenceError::Rule("pg\n\u{1b}[2Jv9".into()).to_string();
        assert!(!message.chars().any(char::is_control), "{message:?}");
        assert!(message.contains("`pg\\n\\u{1b}[2Jv9`"), "{message}");
        // Negative: an identifier that is only unknown is named as it is.
        let message = EvidenceError::Rule("pg-reported-scope-v1".into()).to_string();
        assert!(message.contains("`pg-reported-scope-v1`"), "{message}");
    }

    #[test]
    fn a_decoder_sentence_is_shown_the_same_way_whichever_way_it_quoted() {
        // serde quotes a string value with `Debug` escapes and a variant or a
        // field name raw. The file held a line break in the first and the two
        // characters `\n` in the second, and each pair is told apart.
        let quoted = |text: &str| format!("invalid type: string {text:?}, expected u32");
        let raw = |text: &str| format!("unknown variant `{text}`, expected `x`");
        let sentences: [&dyn Fn(&str) -> String; 2] = [&quoted, &raw];
        for sentence in sentences {
            let line_break = Escaped(&sentence("a\nb")).to_string();
            let two_characters = Escaped(&sentence("a\\nb")).to_string();
            assert_ne!(line_break, two_characters);
            assert!(!line_break.chars().any(char::is_control), "{line_break:?}");
        }
        assert_eq!(
            Escaped(&quoted("a\nb")).to_string(),
            "invalid type: string \"a\\\\nb\", expected u32"
        );
        assert_eq!(
            Escaped(&raw("a\nb\u{1b}")).to_string(),
            "unknown variant `a\\nb\\u{1b}`, expected `x`"
        );
        assert_eq!(
            Escaped(&raw("a\\nb")).to_string(),
            "unknown variant `a\\\\nb`, expected `x`"
        );
    }

    #[test]
    fn a_backslash_is_doubled_so_an_escape_is_never_mistaken_for_text() {
        assert_eq!(shown("a\\nb"), "a\\\\nb");
        assert_ne!(shown("a\\nb"), shown("a\nb"));
    }
}
