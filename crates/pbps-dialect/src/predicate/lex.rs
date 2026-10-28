use super::*;
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Token {
    Word(String, bool),
    Number(String),
    String(String, bool),
    Mark(&'static str),
}
pub(super) fn tokens(text: &str, lexicon: crate::Lexicon) -> Result<Vec<(Token, usize)>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        let ch = rest
            .chars()
            .next()
            .ok_or_else(|| Error::new("unreadable token"))?;
        let start = at;
        if lexicon.is_definition_whitespace(ch) {
            at += ch.len_utf8();
            continue;
        }
        if rest.starts_with("--") {
            at += rest.find(['\r', '\n']).unwrap_or(rest.len());
            continue;
        }
        if rest.starts_with("/*") {
            at = crate::closing_block_comment(text, at)
                .ok_or_else(|| Error::new("unterminated block comment"))?;
            continue;
        }
        let national = lexicon
            .string_prefixes
            .iter()
            .any(|p| p.eq_ignore_ascii_case("n"))
            && (rest.starts_with("N'") || rest.starts_with("n'"));
        let token = if ch == '\'' || national {
            let (value, end) =
                crate::quoted_piece(text, at + if national { 2 } else { 1 }, '\'', false)
                    .ok_or_else(|| Error::new("unterminated string literal"))?;
            at = end;
            Token::String(value, national)
        } else if let Some((_, close)) = lexicon
            .quoted_identifiers
            .iter()
            .find(|(open, _)| *open == ch)
        {
            let (value, end) = crate::quoted_piece(text, at + ch.len_utf8(), *close, false)
                .ok_or_else(|| Error::new("unterminated quoted identifier"))?;
            at = end;
            Token::Word(value, true)
        } else if rest.starts_with("0x") || rest.starts_with("0X") {
            return Err(Error::new("binary literals are not supported"));
        } else if ch.is_ascii_digit()
            || ch == '.' && rest.as_bytes().get(1).is_some_and(u8::is_ascii_digit)
        {
            while text.as_bytes().get(at).is_some_and(u8::is_ascii_digit) {
                at += 1;
            }
            if text.as_bytes().get(at) == Some(&b'.') {
                at += 1;
                while text.as_bytes().get(at).is_some_and(u8::is_ascii_digit) {
                    at += 1;
                }
            }
            if matches!(text.as_bytes().get(at), Some(b'e' | b'E')) {
                at += 1;
                if matches!(text.as_bytes().get(at), Some(b'+' | b'-')) {
                    at += 1;
                }
                let digits = at;
                while text.as_bytes().get(at).is_some_and(u8::is_ascii_digit) {
                    at += 1;
                }
                if at == digits {
                    return Err(Error::new("numeric exponent requires digits"));
                }
            }
            Token::Number(text[start..at].to_ascii_uppercase())
        } else if ch == '@' {
            return Err(Error::new(
                "variables and system variables are not supported",
            ));
        } else if (lexicon.identifier_continues)(ch) && !ch.is_ascii_digit() {
            at += ch.len_utf8();
            while let Some(c) = text[at..].chars().next() {
                if !(lexicon.identifier_continues)(c) {
                    break;
                }
                at += c.len_utf8();
            }
            Token::Word(text[start..at].to_owned(), false)
        } else {
            let mark = [
                "!=", "!<", "!>", "<=", ">=", "<>", "(", ")", ",", "+", "-", "*", "/", "%", "=",
                "<", ">", ".",
            ]
            .into_iter()
            .find(|s| rest.starts_with(s))
            .ok_or_else(|| Error::new(format!("unsupported token at byte {at}")))?;
            at += mark.len();
            Token::Mark(mark)
        };
        if text[start..at].contains('\0') {
            return Err(Error::new("NUL is not supported"));
        }
        out.push((token, start));
    }
    Ok(out)
}
