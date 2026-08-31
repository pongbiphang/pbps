//! The saved plan (SPEC §7.3) — what the deployment gate approves, and the only
//! thing `apply` will run.
//!
//! # Two checksums, two different jobs
//!
//! - [`state_checksum`] fingerprints the **environment** the plan was computed
//!   against. `apply` recomputes it from the live database and refuses when it
//!   differs: that is the drift check, and it is what makes a flag as coarse as
//!   `--allow rename,destructive` safe — what gets approved is exactly the plan
//!   approved at the gate, no more and no less.
//! - [`plan_checksum`] fingerprints the **plan**. It goes into the ledger so the
//!   record can answer "which plan ran here" without storing the plan twice.
//!
//! Neither checksum is stored inside the file it describes: a self-referential
//! digest is a digest of nothing. `plan_checksum` is computed from the file as
//! read, and the state checksum in [`PlanBaseline`] describes the database, not
//! the plan.
//!
//! # Why an offline plan can never be applied
//!
//! [`PlanOrigin::Preview`] is not a warning label that `apply` may be talked out
//! of. A preview's baseline is a git revision or a state file, so its checksum
//! describes something that is not the target environment at all — comparing it
//! would be theatre. `apply` rejects a preview outright and says to run
//! `plan --db`.

use sha2::{Digest, Sha256};

use crate::change::ChangeSet;
use crate::ids::IdsFile;
use crate::schema::Schema;

/// The current plan-file format version.
pub const CURRENT_VERSION: u32 = 1;

/// Where a plan came from, and therefore whether it may be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanOrigin {
    /// Computed offline, against a git revision or a state file. A preview for
    /// the MR layer — never applyable.
    Preview,
    /// Computed against the target environment as queried (`plan --db`).
    Database,
}

impl PlanOrigin {
    pub const fn is_applyable(self) -> bool {
        matches!(self, PlanOrigin::Database)
    }
}

/// What the plan was computed against.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlanBaseline {
    /// Human-readable, for the approver reading the plan at the gate: "prod as
    /// queried", "git HEAD~3".
    pub description: String,

    /// [`state_checksum`] of the baseline. `apply` recomputes it from the live
    /// database; a mismatch is drift and stops the apply.
    pub checksum: String,
}

/// A plan as written to disk by `pbps plan --out`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SavedPlan {
    pub version: u32,

    pub origin: PlanOrigin,

    /// The dialect the SQL was emitted for. A plan computed for one engine and
    /// applied to another is nonsense that the file should be able to catch.
    pub dialect: String,

    /// `YYYY-MM-DDTHH:MM:SSZ`. For the human reading the plan, not for any
    /// decision the tool makes — staleness is caught by the checksum, which is
    /// the only honest test.
    pub created_at: String,

    /// The commit the declarations came from, when the tool is run inside a
    /// checkout. Carried into the ledger, which is where an audit looks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,

    pub baseline: PlanBaseline,

    pub changes: ChangeSet,

    /// The identity mapping the environment has **after** this plan.
    ///
    /// Carried so that `apply` needs nothing but the plan file: the state it
    /// records afterwards has to pair the new schema with the new mapping, and
    /// reusing the baseline's mapping would record a rename as never having
    /// happened. State and identity travel together everywhere else in this
    /// tool (see [`crate::StateSnapshot`]), and an applyable artifact that left
    /// half of it behind would be an artifact that has to be applied from a
    /// checkout — which is exactly what an air-gapped host does not have.
    pub ids: IdsFile,
}

impl SavedPlan {
    pub fn new(
        origin: PlanOrigin,
        dialect: impl Into<String>,
        created_at: impl Into<String>,
        baseline: PlanBaseline,
        changes: ChangeSet,
        ids: IdsFile,
    ) -> Self {
        Self {
            version: CURRENT_VERSION,
            origin,
            dialect: dialect.into(),
            created_at: created_at.into(),
            git_sha: None,
            baseline,
            changes,
            ids,
        }
    }

    /// This plan's own fingerprint. See [`plan_checksum`].
    pub fn checksum(&self) -> String {
        plan_checksum(self)
    }
}

/// The fingerprint of an environment's state.
///
/// Both halves go in. The schema alone is not enough: a plan is computed with a
/// particular identity mapping, and applying it against an environment whose
/// mapping has moved on would resolve a rename against the wrong column. State
/// and identity travel together everywhere else in this tool (see
/// [`crate::StateSnapshot`]), and they travel together here.
///
/// Determinism comes from the model's collections being `BTreeMap` / `BTreeSet`
/// (the one `IndexMap` is column order, which is itself meaningful). Without
/// that, this function would return a different answer for the same database on
/// every run and the drift check would be a coin flip.
pub fn state_checksum(schema: &Schema, ids: &IdsFile) -> String {
    #[derive(serde::Serialize)]
    struct Fingerprint<'a> {
        schema: &'a Schema,
        ids: &'a IdsFile,
    }
    digest_of(&Fingerprint { schema, ids })
}

/// The fingerprint of a plan, for the ledger.
///
/// The whole file is hashed, `created_at` included: two plans that differ only
/// in when they were computed are still two different artifacts, and the ledger
/// entry should name the one that actually ran.
pub fn plan_checksum(plan: &SavedPlan) -> String {
    digest_of(plan)
}

/// SHA-256 of a value's canonical JSON, as lowercase hex.
fn digest_of<T: serde::Serialize>(value: &T) -> String {
    // These are the model's own types, whose map keys all serialize as strings;
    // a failure here would mean the model can no longer be written to the ids
    // file or to `state_json` either, which the rest of the tool would have
    // discovered long before this line.
    let json = serde_json::to_vec(value).expect("the model always serializes to JSON");
    let mut hasher = Sha256::new();
    hasher.update(&json);
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::{Change, PlannedChange};
    use crate::name::TableName;
    use crate::schema::{Column, Table};
    use crate::types::ColumnType;
    use crate::uid::Uid;
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

    fn ids_with(uid: &str) -> IdsFile {
        let mut ids = IdsFile::default();
        ids.tables.insert(
            uid.parse::<Uid>().unwrap(),
            TableName::new("dbo", "customer"),
        );
        ids
    }

    fn plan_over(cs: ChangeSet) -> SavedPlan {
        SavedPlan::new(
            PlanOrigin::Database,
            "mssql",
            "2026-08-31T09:00:00Z",
            PlanBaseline {
                description: "prod as queried".into(),
                checksum: state_checksum(&schema_with("nvarchar(255)"), &IdsFile::default()),
            },
            cs,
            ids_with("t_a1b2c3"),
        )
    }

    #[test]
    fn a_checksum_is_sixty_four_hex_characters() {
        let sum = state_checksum(&Schema::default(), &IdsFile::default());
        assert_eq!(sum.len(), 64, "{sum}");
        assert!(sum.chars().all(|c| c.is_ascii_hexdigit()), "{sum}");
    }

    /// The whole drift check rests on this: the same state must hash the same
    /// way every time, or `apply` would refuse plans at random.
    #[test]
    fn the_same_state_always_hashes_the_same_way() {
        let a = state_checksum(&schema_with("nvarchar(255)"), &ids_with("t_a1b2c3"));
        let b = state_checksum(&schema_with("nvarchar(255)"), &ids_with("t_a1b2c3"));
        assert_eq!(a, b);
    }

    #[test]
    fn a_changed_schema_changes_the_checksum() {
        let a = state_checksum(&schema_with("nvarchar(255)"), &IdsFile::default());
        let b = state_checksum(&schema_with("nvarchar(100)"), &IdsFile::default());
        assert_ne!(a, b);
    }

    /// A rename recorded in the ids file with no schema change still moves the
    /// environment on. Hashing only the schema would let a plan computed before
    /// it be applied after — resolving the rename against the wrong column.
    #[test]
    fn a_changed_identity_changes_the_checksum() {
        let schema = schema_with("nvarchar(255)");
        assert_ne!(
            state_checksum(&schema, &ids_with("t_a1b2c3")),
            state_checksum(&schema, &ids_with("t_z9y8x7")),
        );
    }

    #[test]
    fn a_changed_plan_changes_its_checksum() {
        let empty = plan_over(ChangeSet::default());
        let one = plan_over(ChangeSet {
            changes: vec![PlannedChange::new(Change::DropTable {
                uid: "t_a1b2c3".parse().unwrap(),
                name: TableName::new("dbo", "customer"),
            })],
        });
        assert_ne!(empty.checksum(), one.checksum());
        assert_eq!(empty.checksum(), plan_over(ChangeSet::default()).checksum());
    }

    /// The recorded state after an apply pairs the new schema with the new
    /// mapping. A plan that carried only the baseline's would record a rename
    /// as never having happened.
    #[test]
    fn the_plan_carries_the_identity_it_leaves_behind() {
        let plan = plan_over(ChangeSet::default());
        assert_eq!(plan.ids, ids_with("t_a1b2c3"));
        let back: SavedPlan = serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
        assert_eq!(back.ids, plan.ids);
    }

    /// Only `plan --db` produces something applyable (SPEC §7.3); a preview must
    /// never be talked into being one.
    #[test]
    fn only_a_database_plan_is_applyable() {
        assert!(PlanOrigin::Database.is_applyable());
        assert!(!PlanOrigin::Preview.is_applyable());
    }

    #[test]
    fn round_trips_through_json() {
        let plan = plan_over(ChangeSet::default());
        let back: SavedPlan = serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
        assert_eq!(plan, back);
        assert_eq!(plan.checksum(), back.checksum());
    }

    /// The version field exists so a later format change can be detected rather
    /// than misread.
    #[test]
    fn the_format_version_is_written() {
        let json = serde_json::to_string(&plan_over(ChangeSet::default())).unwrap();
        assert!(json.contains(r#""version":1"#), "{json}");
        assert!(json.contains(r#""origin":"database""#), "{json}");
    }
}
