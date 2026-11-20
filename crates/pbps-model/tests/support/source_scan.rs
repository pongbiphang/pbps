//! The walk over the workspace's Rust sources that the source-scanning tests
//! share (`stamp_sites.rs`, `format_number_literals.rs`), so the two cannot
//! disagree about which files they read or how they name one.

use std::path::{Path, PathBuf};

/// Every `.rs` file under `dir`, skipping build output and fixtures: a fixture
/// is a frozen copy of old text, and what it spells is not this tree's code.
pub fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
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

/// A relative path as the expected names spell it. Windows separates with `\`,
/// and the expected names are written once, with `/`.
pub fn slashed(shown: &str) -> String {
    shown.replace('\\', "/")
}

/// `file` relative to `root`, spelled with `/` on every platform.
pub fn relative(root: &Path, file: &Path) -> String {
    slashed(&file.strip_prefix(root).unwrap().to_string_lossy())
}
