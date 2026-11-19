//! Which envelope schemas the wire-rule corpus reads (DEC-997.1, DEC-1038.1,
//! DEC-2412.3).
//!
//! These tests live in this one binary and not in
//! `support/envelope_archives.rs`: that file is included by every test binary
//! that validates an envelope, and a test written in it would run once per
//! binary. They aim the directory read at a scratch directory, because no
//! released directory may exist in the checked-in fixtures before the first
//! release.

use serde_json::json;
use std::path::{Path, PathBuf};

// This binary uses the read and not the validators or the floor's other users.
#[allow(dead_code)]
#[path = "support/envelope_archives.rs"]
mod envelope_archives;
use envelope_archives::corpus;

/// A directory of this test's own, removed when it goes out of scope.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("pbps-corpus-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn put(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// An envelope schema that pins `wire` the way the real one does.
    fn envelope(&self, directory: &str, wire: u64) {
        let document = json!({
            "$defs": {"envelope.status": {"properties": {"schema_version": {"const": wire}}}}
        });
        self.put(
            &format!("{directory}/envelope.schema.json"),
            &document.to_string(),
        );
    }

    fn corpus(&self, floor: u32) -> Result<Vec<String>, String> {
        let archives: &Path = &self.0;
        corpus(archives, &json!(7), floor)
            .map(|found| found.into_iter().map(|(label, _)| label).collect())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Paths in a message are compared with `/` whatever the platform prints.
fn slashed(message: String) -> String {
    message.replace('\\', "/")
}

#[test]
fn a_released_sets_envelope_joins_the_corpus_whatever_its_number() {
    let scratch = Scratch::new();
    scratch.envelope("106", 7);
    scratch.envelope("released/1", 7);
    // A numbered directory and a released one with the same number are two
    // documents, and the label says which is which.
    scratch.envelope("released/106", 7);
    assert_eq!(
        scratch.corpus(105).unwrap(),
        ["106", "released/1", "released/106"]
    );
    // No floor applies to a released set, even above the floor's own number.
    assert_eq!(
        scratch.corpus(1000).unwrap(),
        ["released/1", "released/106"]
    );
}

#[test]
fn only_sets_under_the_current_wire_version_are_in_the_corpus() {
    let scratch = Scratch::new();
    scratch.envelope("106", 6);
    scratch.envelope("107", 7);
    scratch.envelope("released/1", 6);
    scratch.envelope("released/2", 7);
    assert_eq!(scratch.corpus(105).unwrap(), ["107", "released/2"]);
}

#[test]
fn a_numbered_directory_below_the_floor_and_a_foreign_name_are_never_opened() {
    let scratch = Scratch::new();
    scratch.envelope("106", 7);
    // Neither is a valid document, so a read of either would fail the call.
    scratch.put("104/envelope.schema.json", "not json");
    scratch.put("notes/envelope.schema.json", "not json");
    scratch.put("README.md", "a file");
    scratch.put("released/notes/envelope.schema.json", "not json");
    assert_eq!(scratch.corpus(105).unwrap(), ["106"]);
}

#[test]
fn no_released_directory_is_no_released_set() {
    let scratch = Scratch::new();
    scratch.envelope("106", 7);
    assert_eq!(scratch.corpus(105).unwrap(), ["106"]);
    // An empty `released/` is the same: nothing was released.
    std::fs::create_dir(scratch.0.join("released")).unwrap();
    assert_eq!(scratch.corpus(105).unwrap(), ["106"]);
}

#[test]
fn a_released_directory_with_no_readable_envelope_is_a_failure_naming_its_path() {
    // Absent, unparseable and not a wire pin are three failures; none skips.
    let missing = Scratch::new();
    missing.envelope("106", 7);
    std::fs::create_dir_all(missing.0.join("released/3")).unwrap();
    let why = slashed(missing.corpus(105).unwrap_err());
    assert!(why.contains("released/3/envelope.schema.json"), "{why}");

    let broken = Scratch::new();
    broken.put("released/4/envelope.schema.json", "{ not json");
    let why = slashed(broken.corpus(105).unwrap_err());
    assert!(why.contains("released/4/envelope.schema.json"), "{why}");
    assert!(why.contains("not JSON"), "{why}");

    let unpinned = Scratch::new();
    unpinned.put("released/5/envelope.schema.json", "{}");
    let why = slashed(unpinned.corpus(105).unwrap_err());
    assert!(why.contains("released/5/envelope.schema.json"), "{why}");
    assert!(why.contains("no wire version"), "{why}");
}

#[test]
fn a_numbered_directory_at_or_above_the_floor_without_an_envelope_still_fails() {
    let scratch = Scratch::new();
    std::fs::create_dir_all(scratch.0.join("106")).unwrap();
    let why = slashed(scratch.corpus(105).unwrap_err());
    assert!(why.contains("106/envelope.schema.json"), "{why}");
}

#[test]
fn a_missing_archive_root_is_a_failure_and_not_an_empty_corpus() {
    let scratch = Scratch::new();
    let absent = scratch.0.join("nowhere");
    let why = corpus(&absent, &json!(7), 105).unwrap_err();
    assert!(why.contains("cannot list"), "{why}");
}
