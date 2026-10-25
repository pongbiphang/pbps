//! Stdin is an explicit reason source for single commands (DEC-2332.1).

use pbps_model::Reason;

pub fn resolve(reason: &mut Reason) -> Result<(), String> {
    if reason.as_ref() != "-" {
        return Ok(());
    }
    *reason = from_stdin()?;
    Ok(())
}

/// The reason piped on stdin, shared by the single commands and the one `-`
/// a `pbps intent` batch may carry.
pub fn from_stdin() -> Result<Reason, String> {
    let reason = read(std::io::stdin().lock())?;
    if encoding_warning(reason.as_ref(), cfg!(windows)) {
        eprintln!(
            "warning: Windows PowerShell 5.1 can replace piped non-ASCII text with '?'; pass the reason as an argument or use PowerShell 7."
        );
    }
    Ok(reason)
}

fn read(mut input: impl std::io::Read) -> Result<Reason, String> {
    let mut value = String::new();
    input
        .read_to_string(&mut value)
        .map_err(|error| format!("cannot read UTF-8 reason from stdin: {error}"))?;
    // A pipe usually adds one line ending; intentional preceding whitespace
    // and every other newline belong to the audit record.
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    Reason::new(value).map_err(|error| error.to_string())
}

fn encoding_warning(reason: &str, windows: bool) -> bool {
    windows && reason.contains('?')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_preserves_the_reason_except_one_final_line_ending() {
        for (input, expected) in [
            (" \t核准\n ticket \r\n", " \t核准\n ticket "),
            ("ticket\n\n", "ticket\n"),
            ("ticket\r\n\r\n", "ticket\r\n"),
            ("ticket\r", "ticket\r"),
            ("ticket", "ticket"),
        ] {
            assert_eq!(read(input.as_bytes()).unwrap().as_ref(), expected);
        }
    }

    #[test]
    fn stdin_refuses_blank_or_non_utf8_reasons() {
        for input in [b"".as_slice(), b" \t\r\n", "\u{3000}\n".as_bytes(), b"\xff"] {
            assert!(read(input).is_err());
        }
    }

    #[test]
    fn the_encoding_hint_only_flags_question_marks_on_windows() {
        assert!(encoding_warning("approved?", true));
        assert!(!encoding_warning("核准", true));
        assert!(!encoding_warning("approved?", false));
    }
}
