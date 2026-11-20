//! No test asserts a literal format number (#2412, DEC-2412.3).
//!
//! A format's number is a label that moves once per release, and what tells
//! two builds apart is its fingerprint, so a test that spells the number out
//! fails on every release for no reason and gets "fixed" by editing the
//! literal, which is the five-to-ten-place increment this rule removes. A test
//! that needs a number takes the constant, or an older number it builds from
//! it (`CURRENT_VERSION - 1`), or a historical fixture's own number, which
//! stays what it was.
//!
//! The scan reads every Rust source of the workspace and looks for one shape
//! of comparison (`assert_eq!`, `assert_ne!`, `==`, `!=`): a name of a format's
//! number (`FORMAT_CONSTANTS`) against a digit literal, whatever the number.
//! The names are the constants, the two historical boundaries, and the plan's
//! and the state's descriptors `PLAN.current` and `STATE.current`, which are
//! the same two numbers spelled through `format::PLAN` and `format::STATE`.
//! Its few legitimate hits are listed below with their reasons, and an entry
//! that no longer matches a line fails the test, so the list cannot outlive
//! what it excuses.
//!
//! The limit, in plain words: a `version` field against a literal, as in
//! `assert_eq!(plan.version, 83)`, is not found. The text does not name which
//! format the field belongs to, and a guess flags other formats' numbers (the
//! UI's, the resolver manifest's, a hook's), which would fail a release for
//! nothing once the three numbers are reset to 1 (#1159). Such a test fails by
//! itself when the number moves, and its author takes the constant then.
//! The same reason refuses a bare `.current` and a local variable: only the two
//! exact descriptor spellings name a format.

use std::path::Path;

#[path = "support/source_scan.rs"]
mod source_scan;
use source_scan::{relative, sources};

/// The constants that name a format's number or a boundary in its history.
const FORMAT_CONSTANTS: [&str; 6] = [
    "CURRENT_VERSION",
    "SCHEMA_VERSION",
    "OLDEST_READABLE_VERSION",
    "OBJECT_DEPS_VERSION",
    // The descriptors of `pbps_model::format`: `current` is `CURRENT_VERSION`.
    "PLAN.current",
    "STATE.current",
];

/// Comparisons of a format constant with a literal that stay on purpose: path
/// under `crates/`, the statement with its whitespace removed, and why.
const ALLOWED: [(&str, &str, &str); 2] = [
    (
        "pbps-model/src/state.rs",
        "assert_eq!(OLDEST_READABLE_VERSION,6)",
        "the oldest number the legacy reading path accepts is a historical boundary, not the \
         current number: it does not move with a release. #1159 removes the path and the pin",
    ),
    (
        "pbps-model/src/state.rs",
        "assert_eq!(OBJECT_DEPS_VERSION,47)",
        "the number at which `object_deps` replaced `module_deps` is a fact about old rows: a \
         later bump of the current number must not move it, which is what the test pins",
    ),
];

/// Code with whitespace removed, one statement-ish piece at a time. Comment
/// lines are dropped; strings are not parsed, so a brace inside one splits a
/// piece early, which loses nothing: only the first two operands are read.
fn pieces(text: &str) -> Vec<String> {
    let code: String = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("//"))
        .collect::<Vec<_>>()
        .join(" ");
    code.split([';', '{', '}'])
        .map(|piece| piece.split_whitespace().collect::<String>())
        .filter(|piece| !piece.is_empty())
        .collect()
}

fn is_literal(operand: &str) -> bool {
    let digits = operand.strip_suffix("u32").unwrap_or(operand);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit() || b == b'_')
}

fn names_a_format_constant(operand: &str) -> bool {
    let name = operand.rsplit("::").next().unwrap_or(operand);
    // The scan reads code with its whitespace removed, so a keyword before the
    // operand is glued to it: `if PLAN.current != 1` reaches here as
    // `ifPLAN.current`, and `} else if` as `elseif`. Every listed name starts
    // with a capital and a keyword has none, so the lower-case run at the front
    // is never part of the name. No list of keywords is kept: it would have to
    // grow with each one a comparison can follow.
    let name = name.trim_start_matches(|c: char| c.is_ascii_lowercase());
    FORMAT_CONSTANTS.contains(&name)
}

/// The two operands an `assert_eq!`/`assert_ne!` compares, from the text after
/// its opening parenthesis.
fn macro_operands(after_paren: &str) -> Option<(String, String)> {
    let (mut depth, mut start) = (0_i32, 0);
    let mut operands = Vec::new();
    for (at, c) in after_paren.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' if depth == 0 => {
                operands.push(&after_paren[start..at]);
                break;
            }
            ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                operands.push(&after_paren[start..at]);
                start = at + 1;
            }
            _ => {}
        }
        if operands.len() == 2 {
            break;
        }
    }
    match operands[..] {
        [a, b, ..] => Some((a.to_owned(), b.to_owned())),
        _ => None,
    }
}

/// The operands on either side of each `==` and `!=`.
fn operator_operands(piece: &str) -> Vec<(String, String)> {
    let word = |c: char| c.is_alphanumeric() || matches!(c, '_' | '.' | ':' | '"' | '[' | ']');
    let mut found = Vec::new();
    for (at, _) in piece.match_indices("==").chain(piece.match_indices("!=")) {
        let left: String = piece[..at]
            .chars()
            .rev()
            .take_while(|&c| word(c))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let right: String = piece[at + 2..].chars().take_while(|&c| word(c)).collect();
        found.push((left, right));
    }
    found
}

/// Every comparison in `text` that breaks the rule, as its statement.
fn offences(text: &str) -> Vec<String> {
    let breaks = |a: &str, b: &str| {
        (names_a_format_constant(a) && is_literal(b))
            || (names_a_format_constant(b) && is_literal(a))
    };
    let mut found = Vec::new();
    for piece in pieces(text) {
        let mut operands = operator_operands(&piece);
        for name in ["assert_eq!(", "assert_ne!("] {
            for (at, _) in piece.match_indices(name) {
                operands.extend(macro_operands(&piece[at + name.len()..]));
            }
        }
        if operands.iter().any(|(a, b)| breaks(a, b)) {
            found.push(piece);
        }
    }
    found
}

#[test]
fn no_test_compares_a_format_number_with_a_literal() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut files = Vec::new();
    sources(crates, &mut files);
    let mut found = Vec::new();
    for file in &files {
        // This file spells the shapes it looks for.
        if file
            .file_name()
            .is_some_and(|n| n == "format_number_literals.rs")
        {
            continue;
        }
        let text = std::fs::read_to_string(file).unwrap();
        for statement in offences(&text) {
            found.push((relative(crates, file), statement));
        }
    }
    found.sort();
    let mut allowed: Vec<(String, String)> = ALLOWED
        .iter()
        .map(|(path, statement, _)| ((*path).to_owned(), (*statement).to_owned()))
        .collect();
    allowed.sort();
    assert_eq!(
        found, allowed,
        "a test names a format number: take the constant, or build the older number from it \
         (`CURRENT_VERSION - 1`). A historical boundary may be listed in ALLOWED with its reason"
    );
}

#[test]
fn every_allowed_comparison_says_why() {
    for (path, statement, reason) in ALLOWED {
        assert!(!reason.trim().is_empty(), "{path}: {statement}");
        assert!(reason.len() > 40, "{path}: {statement}: say why, not that");
    }
}

#[test]
fn the_scan_finds_a_format_constant_against_a_literal() {
    for line in [
        "assert_eq!(CURRENT_VERSION, 51);",
        "assert_eq!(pbps_model::state::CURRENT_VERSION, 52u32);",
        "assert_eq!(82, plan::CURRENT_VERSION);",
        "assert_ne!(SCHEMA_VERSION, 9);",
        "assert_eq!(OLDEST_READABLE_VERSION,\n    6\n);",
        "assert!(CURRENT_VERSION == 83);",
        "if state::CURRENT_VERSION != 7 {",
        "assert_eq!(value[\"x\"], 1_000); assert_eq!(SCHEMA_VERSION, 1_17);",
        "assert_eq!(pbps_model::format::PLAN.current, 83);",
        "assert!(format::STATE.current == 52);",
        "assert_eq!(52u32, STATE.current);",
        // A keyword glued to the name by the whitespace removal, whichever it is.
        "if PLAN.current != 1 {",
        "if CURRENT_VERSION != 7 {",
        "} else if STATE.current == 52 {",
        "return SCHEMA_VERSION == 117;",
        "while CURRENT_VERSION != 83 {",
    ] {
        assert!(!offences(line).is_empty(), "{line}");
    }
    // What taking the lower-case front off a name relies on.
    for name in FORMAT_CONSTANTS {
        assert!(name.starts_with(|c: char| c.is_ascii_uppercase()), "{name}");
    }
}

#[test]
fn the_scan_leaves_symbols_other_fields_and_comments_alone() {
    for line in [
        // The constant against another symbol or arithmetic on itself.
        "assert_eq!(plan.version, CURRENT_VERSION);",
        "assert_eq!(read.version, OBJECT_DEPS_VERSION - 1);",
        "assert!(OBJECT_DEPS_VERSION <= CURRENT_VERSION);",
        "snapshot.version = CURRENT_VERSION + 1;",
        // A descriptor against a symbol, and another value's `current`.
        "assert_eq!(plan.version, format::PLAN.current);",
        "assert_eq!(format::STATE.current, state::CURRENT_VERSION);",
        "assert_eq!(PLAN.current + 1, next.version);",
        "assert_eq!(cursor.current, 3);",
        "assert_eq!(current, 83);",
        "assert_eq!(self.current, 52);",
        "assert_eq!(OTHER.current, 83);",
        "assert_eq!(plan_current, 83);",
        // The limit: a `version` field against a literal names no format, so it
        // is not found, whether the number is current or an old fixture's.
        "assert_eq!(plan.version, 83);",
        "assert_eq!(value[\"version\"], 52, \"{value}\");",
        "assert_eq!(117, schema[\"x-pbps-schema-version\"]);",
        "assert_eq!(before.version, 33);",
        "assert_eq!(value[\"schema_version\"], 1);",
        "assert!(self.version != 1);",
        // A literal with no format beside it.
        "assert_eq!(rows.len(), 83);",
        "assert_eq!(count, 52);",
        // A comment is not a test.
        "// assert_eq!(CURRENT_VERSION, 51);",
        "/// assert_eq!(plan.version, 83);",
    ] {
        assert!(offences(line).is_empty(), "{line}");
    }
}
