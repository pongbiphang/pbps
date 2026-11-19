//! DEC-997.1's wire-version rule, checked on real output (DEC-1038.1): every
//! envelope a test emits must validate against each archived envelope
//! schema, from `FIRST_SET_UNDER_THE_WIRE_RULE` on, stamped with the wire
//! version the current document pins. Shared by every place the tests
//! validate an envelope, so none checks the current schema alone.

use std::path::{Path, PathBuf};

/// The first schema set the current envelopes must still validate against.
/// Sets before 16 predate DEC-997.1 (DECISIONS 435's `denied` variant).
/// Before the first release a breaking change raises it to its own set
/// instead of moving the wire version (DEC-1734.1): 89 adds the `repin`
/// operation's closed `repin` record and phase `side`, which every older set
/// refuses. 103 renames `doctor`'s required `identity_file` to `identity_dir`
/// (#2600 PR 5a), which every older set refuses in turn.
const FIRST_SET_UNDER_THE_WIRE_RULE: u32 = 105;

fn wire(document: &serde_json::Value) -> serde_json::Value {
    document["$defs"]["envelope.status"]["properties"]["schema_version"]["const"].clone()
}

/// A number-named directory entry; anything else (`README.md`, `released`) is
/// not a set.
fn numbered(name: &std::ffi::OsStr) -> Option<u32> {
    name.to_str()?.parse().ok()
}

/// One envelope schema of the corpus, read and parsed. A missing file and an
/// unparseable one are different failures from a document that is not in the
/// corpus, so neither is ever skipped.
fn read_envelope(path: &Path) -> Result<serde_json::Value, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{} is not JSON: {e}", path.display()))
}

/// The envelope schemas the current envelopes must validate against, each with
/// its label, in order: the numbered sets from `floor` on, then every released
/// set. Only documents that pin the current wire version are returned
/// (DEC-997.1). The labels are `117` and `released/1`, since a numbered
/// directory and a released one can share a number once the numbers are reset.
///
/// A numbered directory below `floor` and a name that is not a number are
/// skipped and never opened. A missing `released/` is no released set. A
/// released set is always under the rule, so one whose envelope cannot be read
/// or parsed, or that pins no wire version, is an error naming its path, never
/// a skip.
pub fn corpus(
    archives: &Path,
    current: &serde_json::Value,
    floor: u32,
) -> Result<Vec<(String, serde_json::Value)>, String> {
    let list = |dir: &Path| -> Result<Vec<(u32, PathBuf)>, String> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && dir != archives => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(format!("cannot list {}: {e}", dir.display())),
        };
        let mut sets = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
            if let Some(n) = numbered(&entry.file_name()) {
                sets.push((n, entry.path().join("envelope.schema.json")));
            }
        }
        sets.sort();
        Ok(sets)
    };
    let mut found = Vec::new();
    for (set, path) in list(archives)? {
        if set < floor {
            continue;
        }
        let document = read_envelope(&path)?;
        if wire(&document) == *current {
            found.push((set.to_string(), document));
        }
    }
    for (set, path) in list(&archives.join("released"))? {
        let document = read_envelope(&path)?;
        let pinned = wire(&document);
        if !pinned.is_u64() {
            return Err(format!(
                "{} pins no wire version; a released set's envelope must",
                path.display()
            ));
        }
        if pinned == *current {
            found.push((format!("released/{set}"), document));
        }
    }
    Ok(found)
}

fn validators() -> &'static [(String, jsonschema::Validator)] {
    static VALIDATORS: std::sync::OnceLock<Vec<(String, jsonschema::Validator)>> =
        std::sync::OnceLock::new();
    VALIDATORS.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let current: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("../../schemas/envelope.schema.json")).unwrap(),
        )
        .unwrap();
        let current = wire(&current);
        assert!(
            current.is_u64(),
            "the current envelope pins its wire version"
        );
        let archives = root.join("tests/fixtures/published-schemas");
        let validators: Vec<_> = corpus(&archives, &current, FIRST_SET_UNDER_THE_WIRE_RULE)
            .unwrap_or_else(|why| panic!("the envelope corpus: {why}"))
            .into_iter()
            .map(|(label, document)| {
                let validator = jsonschema::validator_for(&document)
                    .unwrap_or_else(|e| panic!("schema set {label}: not a schema: {e}"));
                (label, validator)
            })
            .collect();
        assert!(
            !validators.is_empty(),
            "no archived schema set shares the current wire version"
        );
        validators
    })
}

/// Panics when an archived schema under the same wire version refuses
/// `envelope`: the change that produced it was breaking, and
/// `output::SCHEMA_VERSION` should have moved.
#[track_caller]
pub fn assert_accepted_by_archives(envelope: &serde_json::Value, label: &str) {
    for (set, archived) in validators() {
        if let Err(e) = archived.validate(envelope) {
            panic!(
                "{label}: schema set {set}, published under the same envelope wire version, \
                 refuses this envelope ({e}); a breaking change moves output::SCHEMA_VERSION \
                 (DEC-997.1)\n{envelope}"
            );
        }
    }
}
