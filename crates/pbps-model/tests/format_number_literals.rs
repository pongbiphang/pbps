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
//! The scan reads every Rust source of the workspace and looks at two shapes of
//! comparison (`assert_eq!`, `assert_ne!`, `==`, `!=`):
//!
//! - a format constant against a digit literal, whatever the number;
//! - a `version` field against a digit literal that is *now* the current number
//!   of the plan, the state or the schema set, which is how a hand-written
//!   `assert_eq!(plan.version, 83)` reads once the constant is not named.
//!
//! A comparison with an older number is a historical fixture and is not
//! flagged by the second shape. The first shape's few legitimate hits are
//! listed below with their reasons, and an entry that no longer matches a line
//! fails the test, so the list cannot outlive what it excuses.

use std::path::{Path, PathBuf};

/// The constants that name a format's number or a boundary in its history.
const FORMAT_CONSTANTS: [&str; 4] = [
    "CURRENT_VERSION",
    "SCHEMA_VERSION",
    "OLDEST_READABLE_VERSION",
    "OBJECT_DEPS_VERSION",
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

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|n| n != "target" && n != "fixtures")
            {
                sources(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

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

fn literal(operand: &str) -> Option<u32> {
    is_literal(operand)
        .then(|| {
            operand
                .strip_suffix("u32")
                .unwrap_or(operand)
                .replace('_', "")
        })
        .and_then(|digits| digits.parse().ok())
}

fn names_a_format_constant(operand: &str) -> bool {
    FORMAT_CONSTANTS.contains(&operand.rsplit("::").next().unwrap_or(operand))
}

fn names_a_version(operand: &str) -> bool {
    // `plan.version`, `value["version"]` and `schema["x-pbps-schema-version"]`.
    operand
        .to_ascii_lowercase()
        .trim_end_matches(['"', ']'])
        .ends_with("version")
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
fn offences(text: &str, current: &[u32]) -> Vec<String> {
    let breaks = |a: &str, b: &str| {
        let against_a_constant = (names_a_format_constant(a) && is_literal(b))
            || (names_a_format_constant(b) && is_literal(a));
        let current_number = |version: &str, number: &str| {
            names_a_version(version) && literal(number).is_some_and(|n| current.contains(&n))
        };
        against_a_constant || current_number(a, b) || current_number(b, a)
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

fn current_numbers(crates: &Path) -> Vec<u32> {
    let cli = std::fs::read_to_string(crates.join("pbps-cli/src/integration.rs")).unwrap();
    let schema_set = cli
        .lines()
        .find_map(|line| line.strip_prefix("pub const SCHEMA_VERSION: u32 = "))
        .and_then(|rest| rest.trim_end_matches(';').parse().ok())
        .expect("integration.rs declares SCHEMA_VERSION on one line");
    vec![
        pbps_model::plan::CURRENT_VERSION,
        pbps_model::state::CURRENT_VERSION,
        schema_set,
    ]
}

#[test]
fn no_test_compares_a_format_number_with_a_literal() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let current = current_numbers(crates);
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
        for statement in offences(&text, &current) {
            found.push((
                file.strip_prefix(crates)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                statement,
            ));
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
fn the_scan_finds_a_constant_or_a_current_number_against_a_literal() {
    let current = [83, 52, 117];
    for line in [
        "assert_eq!(CURRENT_VERSION, 51);",
        "assert_eq!(pbps_model::state::CURRENT_VERSION, 52u32);",
        "assert_eq!(82, plan::CURRENT_VERSION);",
        "assert_ne!(SCHEMA_VERSION, 9);",
        "assert_eq!(OLDEST_READABLE_VERSION,\n    6\n);",
        "assert!(CURRENT_VERSION == 83);",
        "if state::CURRENT_VERSION != 7 {",
        "assert_eq!(plan.version, 83);",
        "assert_eq!(value[\"version\"], 52, \"{value}\");",
        "assert_eq!(117, schema[\"x-pbps-schema-version\"]);",
        "assert_eq!(1_17, a.schema_version);",
    ] {
        assert!(!offences(line, &current).is_empty(), "{line}");
    }
}

#[test]
fn the_scan_leaves_symbols_older_numbers_and_comments_alone() {
    let current = [83, 52, 117];
    for line in [
        // The constant against another symbol or arithmetic on itself.
        "assert_eq!(plan.version, CURRENT_VERSION);",
        "assert_eq!(read.version, OBJECT_DEPS_VERSION - 1);",
        "assert!(OBJECT_DEPS_VERSION <= CURRENT_VERSION);",
        "snapshot.version = CURRENT_VERSION + 1;",
        // A historical fixture's own number, and fields of other formats.
        "assert_eq!(before.version, 33);",
        "assert_eq!(fixture[\"version\"], 11);",
        "assert_eq!(value[\"schema_version\"], 1);",
        // A literal with no version beside it.
        "assert_eq!(rows.len(), 83);",
        "assert_eq!(count, 52);",
        // A comment is not a test.
        "// assert_eq!(CURRENT_VERSION, 51);",
        "/// assert_eq!(plan.version, 83);",
    ] {
        assert!(offences(line, &current).is_empty(), "{line}");
    }
}
