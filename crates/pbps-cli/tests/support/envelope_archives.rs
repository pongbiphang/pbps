//! DEC-997.1's wire-version rule, checked on real output (DEC-1038.1): every
//! envelope a test emits must validate against each archived envelope
//! schema, from `FIRST_SET_UNDER_THE_WIRE_RULE` on, stamped with the wire
//! version the current document pins. Shared by every place the tests
//! validate an envelope, so none checks the current schema alone.

use std::path::PathBuf;

/// The first schema set published under DEC-997.1. Earlier sets under the
/// same wire version predate the rule (DECISIONS 435's `denied` variant).
const FIRST_SET_UNDER_THE_WIRE_RULE: u32 = 16;

fn wire(document: &serde_json::Value) -> serde_json::Value {
    document["$defs"]["envelope.status"]["properties"]["schema_version"]["const"].clone()
}

fn validators() -> &'static [(u32, jsonschema::Validator)] {
    static VALIDATORS: std::sync::OnceLock<Vec<(u32, jsonschema::Validator)>> =
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
        let mut validators = Vec::new();
        for entry in std::fs::read_dir(&archives).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            let Ok(set) = name.parse::<u32>() else {
                continue;
            };
            if set < FIRST_SET_UNDER_THE_WIRE_RULE {
                continue;
            }
            let path = archives.join(&name).join("envelope.schema.json");
            let document: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            if wire(&document) == current {
                validators.push((set, jsonschema::validator_for(&document).unwrap()));
            }
        }
        validators.sort_by_key(|(set, _)| *set);
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
