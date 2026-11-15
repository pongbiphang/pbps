//! A standalone sequence: one no column owns (#2721, DEC-2721.1).
//!
//! A sequence a column owns is part of that column (`Identity`, `OwnedSequence`) and is
//! created and dropped with it. This is the other one: an object in the relation
//! namespace that defaults and application code call by name. It is keyed by that
//! name, so a table and a sequence of one name cannot both be declared, as the engine
//! would refuse the pair.
//!
//! **The current value is not here, and must never be added.** `last_value` and
//! `is_called` are state the database advances on every `nextval`; anything that read
//! them into the model would make each insert look like a change. A declaration says
//! where a restart would begin (`start`), never where the counter stands.

use schemars::JsonSchema;

use crate::{ColumnType, SequenceOptions};

/// What a standalone sequence is, without its name, its owner or its current value.
///
/// `options` is the same record a column's sequence has, so a bound is `None` where it
/// is the engine's default for the type and the direction counted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Sequence {
    /// `smallint`, `integer` or `bigint`; the dialect decides which spellings it accepts.
    #[serde(rename = "type")]
    pub ty: ColumnType,
    /// Where a restart would begin. Not the counter's position.
    pub start: i64,
    pub increment: i64,
    #[serde(default, skip_serializing_if = "SequenceOptions::is_default")]
    pub options: SequenceOptions,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bigint() -> ColumnType {
        ColumnType::simple("bigint")
    }

    #[test]
    fn a_sequence_survives_a_serde_round_trip_with_its_options() {
        let sequence = Sequence {
            ty: bigint(),
            start: 10,
            increment: 5,
            options: SequenceOptions {
                min: Some(10),
                max: Some(1000),
                cycle: true,
                cache: Some(20),
            },
        };
        let text = serde_json::to_string(&sequence).unwrap();
        assert_eq!(serde_json::from_str::<Sequence>(&text).unwrap(), sequence);
    }

    #[test]
    fn default_options_are_not_written_and_unknown_fields_are_refused() {
        let plain = Sequence {
            ty: bigint(),
            start: 1,
            increment: 1,
            options: SequenceOptions::default(),
        };
        let text = serde_json::to_string(&plain).unwrap();
        assert!(!text.contains("options"), "{text}");
        // Negative: a field the model does not hold is an error, not ignored. In particular
        // there is nowhere to put the counter's position.
        let with_value = r#"{"type":"bigint","start":1,"increment":1,"last_value":7}"#;
        assert!(serde_json::from_str::<Sequence>(with_value).is_err());
    }
}
