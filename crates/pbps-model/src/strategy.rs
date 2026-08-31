//! Execution strategy: **how** to reach the desired state, never **where** to go
//! ([ADR-0003](../../../docs/ADR-0003-execution-strategy.md)).
//!
//! # Why this is not part of `Schema`
//!
//! A strategy changes the SQL that gets emitted, not the state that results. It
//! is invisible in the database, so it can never take part in the drift
//! comparison — and putting it in [`crate::Schema`] would make two schemas that
//! are identical in the database compare unequal, breaking inviolable
//! constraint 1.
//!
//! It therefore travels beside the model, the way [`crate::Intent`] does.
//! `pbps-load` returns it separately, the differ attaches the relevant one to
//! each `PlannedChange`, and the emitter consumes it.
//!
//! # How it differs from `renamed_from`
//!
//! Intent is **one-shot**: once `plan` absorbs a `renamed_from` into the ids
//! file, `fmt` strips it. A strategy is **persistent** — "this table is large,
//! always operate online" stays true across every future revision — so `fmt`
//! preserves it.

use std::collections::BTreeMap;

use crate::name::TableName;

/// Execution hints for one table.
///
/// The key set starts at exactly `online` and grows conservatively. Unknown keys
/// are rejected at load time rather than ignored: a typo that silently became a
/// no-op would leave the user believing a large table is being altered online
/// when it is not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Strategy {
    /// Emit `WITH (ONLINE = ON)` where the dialect and the statement support it.
    ///
    /// Whether the *server* supports it is a different question: ONLINE index
    /// operations are Enterprise-only, which an offline plan cannot know. The
    /// dialect classifies conservatively until `plan --db` can read the real
    /// edition (ADR-0003 decision 3).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub online: bool,
}

impl Strategy {
    /// Whether this is the default, in which case rendering it would be noise.
    pub fn is_default(&self) -> bool {
        self == &Strategy::default()
    }
}

/// Per-table strategies for a whole project.
///
/// A `BTreeMap` for the same reason everything else is: the plan JSON goes into
/// review, and shifting order would manufacture diff noise.
pub type Strategies = BTreeMap<TableName, Strategy>;

#[cfg(test)]
mod tests {
    use super::*;

    /// The default must render as nothing, so a table that says nothing about
    /// strategy keeps a file with no `strategy:` block.
    #[test]
    fn the_default_strategy_is_recognisable_as_such() {
        assert!(Strategy::default().is_default());
        assert!(!Strategy { online: true }.is_default());
    }

    #[test]
    fn a_default_strategy_serializes_to_nothing() {
        assert_eq!(serde_json::to_string(&Strategy::default()).unwrap(), "{}");
        assert_eq!(
            serde_json::to_string(&Strategy { online: true }).unwrap(),
            r#"{"online":true}"#
        );
    }
}
