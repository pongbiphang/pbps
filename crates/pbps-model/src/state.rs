//! Environment state snapshots — written to each environment's `__pbps_state`
//! (SPEC §8).
//!
//! Keeping state in the database itself rather than in a CI artifact is what
//! makes multi-environment support fall out naturally: dev, staging and prod each
//! remember their own state, and an environment several versions behind needs no
//! artifact passing and no environment-mapping strategy at all.

use crate::ids::IdsFile;
use crate::schema::Schema;

pub const CURRENT_VERSION: u32 = 1;

/// How this state came about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateKind {
    /// A plan was applied normally.
    Apply,
    /// Rebaseline: differences are not pursued; the database as it stands
    /// becomes the new starting point.
    Baseline,
    /// The whole schema was built from the declarations in one shot (DR, or a
    /// new environment).
    Bootstrap,
}

impl StateKind {
    /// The value stored in the ledger's `kind` column, matching the serde
    /// representation so the column and `state_json` cannot disagree.
    pub const fn as_str(self) -> &'static str {
        match self {
            StateKind::Apply => "apply",
            StateKind::Baseline => "baseline",
            StateKind::Bootstrap => "bootstrap",
        }
    }
}

impl std::fmt::Display for StateKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One environment's complete, verified state at a point in time.
///
/// The whole schema is stored, not a delta or a checksum: that is what lets
/// drift detection compare in full, what makes the snapshot usable as a backup,
/// and what lets it answer "what did this table look like three months ago?".
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StateSnapshot {
    pub version: u32,
    pub kind: StateKind,

    /// The schema that environment actually has.
    pub schema: Schema,

    /// The identity mapping as of this state.
    ///
    /// Without it, an environment several versions behind cannot match the
    /// current declarations by uid, and a rename degrades into "drop plus add" —
    /// which is data loss. State and identity have to be stored together.
    pub ids: IdsFile,

    /// The commit that produced this state. `None` for a baseline taken outside
    /// version control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,

    /// Checksum of the plan that was applied, matching the saved-plan pinning
    /// mechanism (SPEC §7.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_checksum: Option<String>,

    pub operator: String,

    /// Required for a baseline: "why were the differences skipped?" is precisely
    /// what an audit asks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl StateSnapshot {
    pub fn new(kind: StateKind, schema: Schema, ids: IdsFile, operator: impl Into<String>) -> Self {
        Self {
            version: CURRENT_VERSION,
            kind,
            schema,
            ids,
            git_sha: None,
            plan_checksum: None,
            operator: operator.into(),
            reason: None,
        }
    }

    /// Whether the database still equals this snapshot. Anything else is drift.
    pub fn matches(&self, actual: &Schema) -> bool {
        &self.schema == actual
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::IdsFile;
    use crate::name::TableName;
    use crate::schema::{Column, Table};
    use crate::types::ColumnType;
    use indexmap::IndexMap;

    fn schema_with(ty: &str) -> Schema {
        let mut columns = IndexMap::new();
        columns.insert(
            "email".to_string(),
            Column::new(ty.parse::<ColumnType>().unwrap()),
        );
        let mut s = Schema::default();
        s.tables.insert(
            TableName::new("dbo", "customer"),
            Table {
                columns,
                ..Default::default()
            },
        );
        s
    }

    #[test]
    fn identical_schema_is_not_drift() {
        let snap = StateSnapshot::new(
            StateKind::Apply,
            schema_with("nvarchar(255)"),
            IdsFile::default(),
            "leon",
        );
        assert!(snap.matches(&schema_with("nvarchar(255)")));
    }

    #[test]
    fn any_difference_is_drift() {
        let snap = StateSnapshot::new(
            StateKind::Apply,
            schema_with("nvarchar(255)"),
            IdsFile::default(),
            "leon",
        );
        assert!(
            !snap.matches(&schema_with("nvarchar(100)")),
            "a type change is drift"
        );
        assert!(
            !snap.matches(&Schema::default()),
            "a vanished table is drift"
        );
    }

    /// Case differences must not count as drift, or every introspection could
    /// raise a false alarm.
    #[test]
    fn type_case_is_not_drift() {
        let snap = StateSnapshot::new(
            StateKind::Apply,
            schema_with("NVARCHAR(255)"),
            IdsFile::default(),
            "leon",
        );
        assert!(snap.matches(&schema_with("nvarchar(255)")));
    }

    #[test]
    fn version_is_recorded() {
        let snap = StateSnapshot::new(
            StateKind::Baseline,
            Schema::default(),
            IdsFile::default(),
            "leon",
        );
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains(r#""version":1"#));
        assert!(json.contains(r#""kind":"baseline""#));
    }

    /// The ledger writes `kind` into a column of its own so `status` can filter
    /// without parsing JSON; the two spellings must be the same one.
    #[test]
    fn the_kind_column_matches_the_json_spelling() {
        for kind in [StateKind::Apply, StateKind::Baseline, StateKind::Bootstrap] {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
        }
    }

    #[test]
    fn round_trips_through_json() {
        let mut snap = StateSnapshot::new(
            StateKind::Apply,
            schema_with("bigint"),
            IdsFile::default(),
            "leon",
        );
        snap.git_sha = Some("bd4be74".into());
        snap.plan_checksum = Some("abc123".into());
        let back: StateSnapshot =
            serde_json::from_str(&serde_json::to_string(&snap).unwrap()).unwrap();
        assert_eq!(snap, back);
    }
}
