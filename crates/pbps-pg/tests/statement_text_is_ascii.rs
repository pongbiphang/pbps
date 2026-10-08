//! The text pbps writes into statements is ASCII (#1629).
//!
//! The driver connects with `client_encoding=UTF8`, and the server converts
//! every statement into the database's encoding, SQL comments and `RAISE`
//! messages included. A character that encoding cannot hold fails the whole
//! statement: measured on 18, `—` and `…` have no equivalent in `LATIN1`,
//! `LATIN2`, `LATIN9`, `KOI8R`, `ISO_8859_5` or `EUC_JP`, among others, so
//! `bootstrap` failed outright on a `LATIN1` database. Only `UTF8` and
//! `WIN1252` held every character pbps used.
//!
//! Which literal reaches the server is not something a scan can tell, so the
//! rule covers every string and character literal in the non-test code of the
//! two crates that send statement text, `pbps-pg` and `pbps-db`. Comments,
//! doc comments included, are free; test-only code is not checked. What a
//! user declares is sent as declared and is the database's to hold.

use std::path::{Path, PathBuf};

/// The non-ASCII characters in `source`'s code, by line, skipping comments
/// and every item under a `cfg` attribute that names `test`.
fn non_ascii_in_code(source: &str) -> Vec<(usize, char)> {
    let chars: Vec<char> = source.chars().collect();
    let at = |i: usize| chars.get(i).copied().unwrap_or('\0');
    let starts = |i: usize, s: &str| s.chars().enumerate().all(|(k, c)| at(i + k) == c);
    let mut found = Vec::new();
    let mut line = 1;
    let mut i = 0;
    let mut depth = 0usize;
    // The brace depth a skipped test-only item opened at, and whether one is
    // pending: the next `{` opens it, or a `;` ends it before any brace.
    let mut skipping: Option<usize> = None;
    let mut pending = false;
    let record = |line: usize, c: char, skipping: bool, found: &mut Vec<(usize, char)>| {
        if !skipping && !c.is_ascii() {
            found.push((line, c));
        }
    };
    while i < chars.len() {
        let c = chars[i];
        let skip = skipping.is_some() || pending;
        if c == '\n' {
            line += 1;
            i += 1;
        } else if starts(i, "//") {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if starts(i, "/*") {
            let mut nested = 0;
            while i < chars.len() {
                if starts(i, "/*") {
                    nested += 1;
                    i += 2;
                } else if starts(i, "*/") {
                    nested -= 1;
                    i += 2;
                    if nested == 0 {
                        break;
                    }
                } else {
                    if chars[i] == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
        } else if (c == 'r' || (c == 'b' && at(i + 1) == 'r'))
            && !at(i.wrapping_sub(1)).is_alphanumeric()
            && at(i.wrapping_sub(1)) != '_'
            && {
                let mut k = i + if c == 'b' { 2 } else { 1 };
                while at(k) == '#' {
                    k += 1;
                }
                at(k) == '"'
            }
        {
            // A raw string: `r"…"`, `r#"…"#`, `br"…"`.
            i += if c == 'b' { 2 } else { 1 };
            let mut hashes = 0;
            while at(i) == '#' {
                hashes += 1;
                i += 1;
            }
            i += 1;
            let close: String = std::iter::once('"')
                .chain(std::iter::repeat_n('#', hashes))
                .collect();
            while i < chars.len() && !starts(i, &close) {
                if chars[i] == '\n' {
                    line += 1;
                }
                record(line, chars[i], skip, &mut found);
                i += 1;
            }
            i += close.chars().count();
        } else if c == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    i += 1;
                }
                if at(i) == '\n' {
                    line += 1;
                }
                record(line, at(i), skip, &mut found);
                i += 1;
            }
            i += 1;
        } else if c == '\'' {
            // A character literal, or a lifetime, which has no closing quote.
            if at(i + 1) == '\\' {
                let mut k = i + 2;
                while k < chars.len() && chars[k] != '\'' {
                    k += 1;
                }
                i = k + 1;
            } else if at(i + 2) == '\'' {
                record(line, at(i + 1), skip, &mut found);
                i += 3;
            } else {
                i += 1;
            }
        } else if starts(i, "#[cfg(") && {
            let end = (i..chars.len()).find(|&k| chars[k] == ']').unwrap_or(i);
            let attr: String = chars[i..end].iter().collect();
            test_only(&attr)
        } {
            if skipping.is_none() {
                pending = true;
            }
            i += 1;
        } else if c == '{' {
            if pending {
                pending = false;
                skipping = Some(depth);
            }
            depth += 1;
            i += 1;
        } else if c == '}' {
            depth = depth.saturating_sub(1);
            if skipping == Some(depth) {
                skipping = None;
            }
            i += 1;
        } else if c == ';' {
            if pending && skipping.is_none() {
                pending = false;
            }
            i += 1;
        } else {
            record(line, c, skip, &mut found);
            i += 1;
        }
    }
    found
}

/// Whether a `#[cfg(...)` attribute compiles its item into test builds only:
/// `cfg(test)`, or `cfg(all(...))` with `test` among its own arguments.
/// `not(test)` and `any(test, ...)` reach other builds too, so they are
/// checked (#1649 review).
fn test_only(attr: &str) -> bool {
    let predicate: String = attr
        .trim_start_matches("#[cfg(")
        .trim_end_matches(')')
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if predicate == "test" {
        return true;
    }
    let Some(args) = predicate.strip_prefix("all(") else {
        return false;
    };
    let mut depth = 0usize;
    let mut arg = String::new();
    for c in args.chars() {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => break,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                if arg == "test" {
                    return true;
                }
                arg.clear();
                continue;
            }
            _ => {}
        }
        arg.push(c);
    }
    arg == "test"
}

/// Every `.rs` file under `dir` that is not a test-only module file.
fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("readable source directory") {
        let path = entry.expect("readable entry").path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            // Declared under `#[cfg(test)] mod …;` in their parent.
            if stem != "tests" && !stem.ends_with("_tests") {
                out.push(path);
            }
        }
    }
}

#[test]
fn the_statement_text_pbps_writes_is_ascii() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for krate in ["pbps-pg", "pbps-db"] {
        sources(&crates.join(krate).join("src"), &mut files);
    }
    assert!(files.len() > 20, "the scan found the sources: {files:?}");
    let mut offending = Vec::new();
    for file in &files {
        let source = std::fs::read_to_string(file).expect("readable source");
        for (line, c) in non_ascii_in_code(&source) {
            offending.push(format!("{}:{line}: {c:?}", file.display()));
        }
    }
    assert!(
        offending.is_empty(),
        "statement text must be ASCII, so every server encoding holds it (#1629); \
         write `--` for `—`, `...` for `…`, `section` for `§`:\n  {}",
        offending.join("\n  ")
    );
}

/// The scan's own edges: what it must catch, and what it must leave alone.
#[test]
fn the_ascii_scan_reads_literals_and_skips_comments_and_test_code() {
    let caught = |s: &str| {
        non_ascii_in_code(s)
            .into_iter()
            .map(|(_, c)| c)
            .collect::<String>()
    };
    assert_eq!(caught("const A: &str = \"x — y\";"), "—");
    assert_eq!(caught("const A: &str = r#\"a … \"b\"\"#;"), "…");
    assert_eq!(caught("const C: char = '§';"), "§");
    assert_eq!(caught("fn f<'a>(x: &'a str) { let _ = \"é\"; }"), "é");
    // Outside any test item after one closes.
    assert_eq!(
        caught("#[cfg(test)]\nmod tests { fn t() { \"—\"; } }\nconst B: &str = \"²\";"),
        "²"
    );
    // Negative: comments, doc comments, nested block comments, test items,
    // a test-only declaration ending in `;`, and a `}` inside a string.
    assert_eq!(caught("// — \n/// … \n//! §\n/* a /* — */ b */"), "");
    assert_eq!(caught("#[cfg(test)]\nmod tests { fn t() { \"}—\"; } }"), "");
    assert_eq!(caught("#[cfg(all(test, unix))]\nfn t() { \"—\"; }"), "");
    assert_eq!(
        caught("#[cfg(test)]\nmod tests;\nconst A: &str = \"ok\";"),
        ""
    );
    assert_eq!(caught("const A: &str = \"plain ascii -- ...\";"), "");
    // A `cfg` that also reaches non-test builds is checked.
    assert_eq!(caught("#[cfg(not(test))]\nfn p() { \"—\"; }"), "—");
    assert_eq!(caught("#[cfg(any(test, unix))]\nfn p() { \"…\"; }"), "…");
    assert_eq!(
        caught("#[cfg(all(unix, not(test)))]\nfn p() { \"§\"; }"),
        "§"
    );
    assert_eq!(caught("#[cfg(all(unix, test))]\nfn t() { \"—\"; }"), "");
}
