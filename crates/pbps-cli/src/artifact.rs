//! The one way the text of an artifact is decoded.
//!
//! A saved plan, the identity file and a `--base` snapshot are artifacts a human
//! reviews, and a hand-made one can put any text into a name or a field. When a
//! decoder refuses one it repeats the text it stopped at (an unknown variant, an
//! unknown field, a name a type refused), and that text is the file's. Every
//! decode of an artifact goes through [`decode`], so the sentence is shown
//! escaped and a new artifact gets that by construction (DEC-2032.1, #2084).

/// Decodes `raw` with `read`, and carries a refusal out as an error whose
/// `Display` shows the decoder's sentence escaped.
///
/// `read` is the artifact's own decoder, such as `serde_json::from_str::<T>` or
/// `StateSnapshot::from_json`. Its error may be any `Display`: a decoder that
/// has already flattened its refusal into a `String` repeats the file's text in
/// that string just as serde does, so the sentence is what is escaped, not the
/// type that carried it.
pub(crate) fn decode<'a, T, E: std::fmt::Display>(
    raw: &'a str,
    read: impl FnOnce(&'a str) -> Result<T, E>,
) -> anyhow::Result<T> {
    read(raw).map_err(|error| anyhow::Error::new(Echoed(error.to_string())))
}

/// The decoder's own sentence about a file it could not read.
///
/// serde repeats the text it stopped at — an unknown variant, an unknown field,
/// a name a type refused — and that text is the file's. The sentence is one line
/// by construction, so every control character in it came from the artifact and
/// is shown escaped, like the other values a refusal takes from a plan
/// (DEC-2032.1), and so is its backslash: serde repeats some of that text raw
/// and some behind `Debug` escapes, and only a doubled backslash tells a line
/// break from the two characters `\n` in both (#2038). Held as text rather than
/// as the decoder's error because nothing downcasts the cause, and the error's
/// `Display` is the only thing printed.
#[derive(Debug, thiserror::Error)]
#[error("{}", pbps_model::escape::Escaped(.0))]
struct Echoed(String);

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Artifact {
        #[allow(dead_code)]
        version: u32,
    }

    #[test]
    fn a_valid_artifact_decodes_unchanged() {
        let decoded = decode(r#"{"version": 3}"#, serde_json::from_str::<Artifact>).unwrap();
        assert_eq!(decoded.version, 3);
    }

    #[test]
    fn a_serde_refusal_shows_the_files_text_escaped() {
        let raw = serde_json::json!({"version": 1, "x\ny\u{1b}[31m": 1}).to_string();
        let shown = decode(&raw, serde_json::from_str::<Artifact>)
            .unwrap_err()
            .to_string();
        assert!(shown.contains(r"unknown field `x\ny\u{1b}[31m`"), "{shown}");
        assert!(!shown.chars().any(char::is_control), "{shown:?}");
    }

    #[test]
    fn a_decoder_that_returns_text_is_escaped_the_same_way() {
        let shown = decode("", |_| -> Result<(), String> {
            Err("malformed: unknown field `a\nb`".to_owned())
        })
        .unwrap_err()
        .to_string();
        assert_eq!(shown, r"malformed: unknown field `a\nb`");
    }

    /// The two characters `\n` in the file and a line break must read apart, so
    /// the backslash is doubled (#2038).
    #[test]
    fn a_backslash_in_the_sentence_is_doubled() {
        let shown = decode("", |_| -> Result<(), String> { Err(r"a\nb".to_owned()) })
            .unwrap_err()
            .to_string();
        assert_eq!(shown, r"a\\nb");
    }

    #[test]
    fn an_ordinary_refusal_reads_as_the_decoder_wrote_it() {
        let plain = serde_json::from_str::<Artifact>("{}")
            .unwrap_err()
            .to_string();
        let shown = decode("{}", serde_json::from_str::<Artifact>)
            .unwrap_err()
            .to_string();
        assert_eq!(shown, plain);
    }
}
