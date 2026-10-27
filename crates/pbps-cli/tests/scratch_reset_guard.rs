//! A new reset site must use the engine's registered restoration recipe.

use regex_lite::Regex;
use std::path::{Path, PathBuf};

const HELPER: &str = "crates/pbps-pg/src/resolver/session.rs";

// A bounded Rust literal scanner, rather than a line search: comments can
// explain resets and test modules can demonstrate them without opening a
// second production reset path. Unknown/unreadable source never means empty.
fn production_strings(source: &str) -> Vec<String> {
    let cfg_test = Regex::new(r"^#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]").unwrap();
    let test_item = Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:mod|fn)\b").unwrap();
    let raw = Regex::new("^(?:br|cr|r)(#*)\"").unwrap();
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let (mut i, mut skip_item, mut test_depth) = (0, false, 0usize);
    while i < bytes.len() {
        if source[i..].starts_with("//") {
            i += source[i..].find('\n').unwrap_or(bytes.len() - i);
        } else if source[i..].starts_with("/*") {
            i += 2;
            let mut depth = 1;
            while i < bytes.len() && depth > 0 {
                if source[i..].starts_with("/*") {
                    depth += 1;
                    i += 2;
                } else if source[i..].starts_with("*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += source[i..].chars().next().unwrap().len_utf8();
                }
            }
            assert_eq!(depth, 0, "unterminated Rust comment");
        } else if let Some(attribute) = cfg_test.find(&source[i..]) {
            i += attribute.end();
            // A cfg(test) field is not a test module: skipping to its next
            // brace would accidentally hide a subsequent production function.
            if test_item.is_match(&source[i..]) {
                skip_item = true;
            }
        } else if let Some(open) = raw.captures(&source[i..]) {
            let start = i + open.get(0).unwrap().end();
            let end_marker = format!("\"{}", &open[1]);
            let end = start
                + source[start..]
                    .find(&end_marker)
                    .expect("closed raw string");
            if !skip_item && test_depth == 0 {
                out.push(source[start..end].to_owned());
            }
            i = end + end_marker.len();
        } else if bytes[i] == b'"' {
            i += 1;
            let mut value = String::new();
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' {
                    i += 1;
                    assert!(i < bytes.len(), "closed Rust escape");
                    let escaped = match bytes[i] {
                        b'n' => Some('\n'),
                        b'r' => Some('\r'),
                        b't' => Some('\t'),
                        _ => None,
                    };
                    if let Some(ch) = escaped {
                        value.push(ch);
                        i += 1;
                        continue;
                    }
                    if bytes[i] == b'\n' {
                        i += 1;
                        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                            i += 1;
                        }
                        continue;
                    }
                }
                let ch = source[i..].chars().next().unwrap();
                value.push(ch);
                i += ch.len_utf8();
            }
            assert!(i < bytes.len(), "closed Rust string");
            if !skip_item && test_depth == 0 {
                out.push(value);
            }
            i += 1;
        } else if bytes[i] == b'\'' && i + 2 < bytes.len() && bytes[i + 2] == b'\'' {
            i += 3;
        } else if source[i..].starts_with("'\\") {
            i += 3;
            while i < bytes.len() && bytes[i] != b'\'' {
                i += 1;
            }
            assert!(i < bytes.len(), "closed Rust character");
            i += 1;
        } else {
            match bytes[i] {
                b'{' if skip_item => {
                    skip_item = false;
                    test_depth += 1;
                }
                b'{' if test_depth > 0 => test_depth += 1,
                b'}' if test_depth > 0 => test_depth -= 1,
                b';' if skip_item => skip_item = false,
                _ => {}
            }
            i += source[i..].chars().next().unwrap().len_utf8();
        }
    }
    out
}

fn resets(source: &str) -> bool {
    let reset = Regex::new(r"(?i)(?:^|;)\s*(?:RESET|DISCARD)\s+ALL\b").unwrap();
    production_strings(source).iter().any(|s| reset.is_match(s))
}

fn scan(root: &Path, relative: &Path, violations: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(root.join(relative)).expect("readable resolver sources") {
        let entry = entry.expect("readable source entry");
        let path = relative.join(entry.file_name());
        let kind = entry.file_type().expect("readable source type");
        if kind.is_dir() {
            if !path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("tests")
            {
                scan(root, &path, violations);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            let stem = path.file_stem().unwrap().to_string_lossy();
            if stem == "tests"
                || stem.ends_with("_tests")
                || stem == "fixtures"
                || path == Path::new(HELPER)
            {
                continue;
            }
            let source = std::fs::read_to_string(root.join(&path)).expect("readable Rust source");
            if resets(&source) {
                violations.push(path);
            }
        }
    }
}

#[test]
fn executable_resets_live_only_in_the_restoration_helper() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    assert!(resets(&std::fs::read_to_string(root.join(HELPER)).unwrap()));
    let mut violations = Vec::new();
    for directory in [
        "crates/pbps-cli/src/resolver",
        "crates/pbps-pg/src/resolver",
    ] {
        scan(root, Path::new(directory), &mut violations);
    }
    assert!(
        violations.is_empty(),
        "reset bypasses restoration: {violations:?}"
    );
}

#[test]
fn a_reset_planted_in_a_second_production_file_is_refused() {
    let root =
        std::env::temp_dir().join(format!("pbps-reset-guard-{:016x}", rand::random::<u64>()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("second.rs");
    std::fs::write(
        &path,
        "async fn surprise() { conn.execute(r#\"DISCARD ALL\"#).await; }",
    )
    .unwrap();
    let mut violations = Vec::new();
    scan(&root, Path::new(""), &mut violations);
    std::fs::remove_dir_all(&root).unwrap();
    assert_eq!(violations, [PathBuf::from("second.rs")]);
}

#[test]
fn comments_tests_and_stored_default_resets_are_not_connection_resets() {
    assert!(!resets(
        r##"
        // conn.execute("RESET ALL");
        /* nested /* "DISCARD ALL" */ comment */
        let sql = "ALTER ROLE SESSION_USER IN DATABASE db RESET ALL";
        #[cfg(test)] mod tests { fn example() { let s = r#"RESET ALL"#; } }
        fn ordinary() { let c = '{'; let s = "SELECT 1"; }
    "##
    ));
    assert!(resets(r#"let sql = "SELECT 1; RESET ALL";"#));
    assert!(resets(r#"let sql = "\nRESET ALL";"#));
    assert!(resets("let sql = \"RESET \\\n        ALL\";"));
    assert!(resets(
        r#"#[cfg(test)] mod tests {} fn live() { let s = "RESET ALL"; }"#
    ));
    assert!(resets(
        r#"struct S { #[cfg(test)] hidden: bool } fn live() { let sql = "DISCARD ALL"; }"#
    ));
}
