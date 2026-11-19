//! No writer sets a plan's or a snapshot's number by hand (#2412, DEC-2412.2).
//!
//! The number and the fingerprint are written together by one `stamp()` per
//! type. A line that assigns `CURRENT_VERSION` to a `version` field anywhere else
//! writes a number with no fingerprint, which the next reader refuses as another
//! build's. The scan reads every Rust source of the workspace, so a new writer in
//! any crate is found.

use std::path::{Path, PathBuf};

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

/// Lines (path, text) that write the bare constant to a field called `version`.
fn writers_of_the_number(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("//"))
        .filter(|line| {
            let compact: String = line.split_whitespace().collect();
            ["version=", "version:"].iter().any(|key| {
                compact.find(key).is_some_and(|at| {
                    let value = compact[at + key.len()..]
                        .trim_start_matches("pbps_model::")
                        .trim_start_matches("plan::")
                        .trim_start_matches("state::");
                    value == "CURRENT_VERSION;" || value == "CURRENT_VERSION,"
                })
            })
        })
        .map(str::to_owned)
        .collect()
}

#[test]
fn no_writer_sets_the_number_outside_stamp() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut files = Vec::new();
    sources(crates, &mut files);
    let mut found = Vec::new();
    for file in &files {
        // The two formats that have a number of their own and no fingerprint.
        let name = file.file_name().unwrap().to_string_lossy();
        if name == "ids.rs" || name == "drift.rs" || name == "stamp_sites.rs" {
            continue;
        }
        let text = std::fs::read_to_string(file).unwrap();
        for line in writers_of_the_number(&text) {
            found.push(format!(
                "{}: {line}",
                file.strip_prefix(crates).unwrap().display()
            ));
        }
    }
    found.sort();
    assert_eq!(
        found,
        [
            "pbps-model/src/plan.rs: self.version = CURRENT_VERSION;",
            "pbps-model/src/state.rs: self.version = CURRENT_VERSION;",
        ],
        "write the number through `stamp()`, which records the fingerprint with it"
    );
}

#[test]
fn the_scan_finds_each_spelling_of_a_write_and_not_a_comparison() {
    for line in [
        "plan.version = CURRENT_VERSION;",
        "version: CURRENT_VERSION,",
        "next.version = pbps_model::state::CURRENT_VERSION;",
        "x.version=plan::CURRENT_VERSION;",
    ] {
        assert_eq!(writers_of_the_number(line).len(), 1, "{line}");
    }
    for line in [
        "// plan.version = CURRENT_VERSION;",
        "future.version = pbps_model::state::CURRENT_VERSION + 1;",
        "assert_eq!(plan.version, CURRENT_VERSION);",
        "if plan.version != CURRENT_VERSION {",
    ] {
        assert!(writers_of_the_number(line).is_empty(), "{line}");
    }
}
