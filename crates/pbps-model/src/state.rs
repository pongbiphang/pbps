//! Environment state snapshots — written to each environment's `__pbps_state`
//! (SPEC §8).
//!
//! Keeping state in the database itself rather than in a CI artifact is what
//! makes multi-environment support fall out naturally: dev, staging and prod each
//! remember their own state, and an environment several versions behind needs no
//! artifact passing and no environment-mapping strategy at all.

use crate::ids::IdsFile;
use crate::module::ModuleDeps;
use crate::schema::Schema;

/// The current state-snapshot format version.
///
/// Bumped to 2 when `Schema` grew `modules`: what a state records is what a
/// drift check compares, so an older client would deserialize a new snapshot,
/// silently ignore the modules in it, introspect only tables, and report a
/// clean verification for an environment whose managed procedure has drifted.
///
/// Bumped to 3 when a staged checkpoint's `ids` became the mapping *at that
/// checkpoint* rather than the plan's. An older client resuming a version 3
/// checkpoint would scope the live side by the plan's names and the recorded
/// side by the checkpoint's — two different sets of objects — and refuse the
/// resume with a checksum mismatch it cannot explain.
///
/// Bumped to 4 when module dependency annotations and failed attempts joined
/// the snapshot. The annotations are needed to order a later connected drop;
/// failed attempts make the audit promise explicit without changing the schema
/// recorded as the current baseline.
///
/// Bumped to 5 when `Schema` grew `roles` (ADR-0005), for the reason 2 was:
/// an older client would drop the field, compare every table and no role, and
/// report no drift about grants it never looked at.
///
/// Readers refuse a version they do not understand rather than reading it
/// partially.
///
/// Version 4 is still read, and 3 is not, and the line between them is the
/// project's own rule about absence (DECISIONS 160). Every field a later
/// version adds defaults to empty, but "empty" is only a safe reading where it
/// is a *true* one. An environment recorded before roles were managed **is**
/// one with no managed roles, so a version 4 snapshot read by this build says
/// exactly what that environment was — and refusing it would leave a deployed
/// environment with no way to be read at all, since re-recording one reads the
/// latest entry first (DECISIONS 138). A version 3 snapshot predates
/// `module_deps`, and "no dependencies" is not a true reading of it but a
/// missing one: a later revision that removes several dependent modules has
/// no declaration left carrying their `depends_on:` edges, so the drop order
/// comes from the snapshot — and defaulted to empty it falls back to name
/// order, which can drop a schema-bound dependency before its dependent.
/// Refused, with the remedy `check_version` already names: re-record it with
/// `pbps baseline --reason ...`.
pub const CURRENT_VERSION: u32 = 5;

/// The oldest snapshot version this build reads as its own.
pub const OLDEST_READABLE_VERSION: u32 = 4;

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
    /// A checkpoint inside a staged apply: one statement of a staged plan
    /// completed, and the rest have not run yet (ADR-0003 decision 2).
    ///
    /// It is a state like any other — the schema recorded is the database as it
    /// stands — but it is deliberately a *different* kind, because an
    /// environment sitting on one is mid-deployment: planning or applying
    /// anything else against it would build on a half-finished change.
    Staged,
    /// An apply or bootstrap attempt failed. The schema and ids in this entry
    /// remain the last known current state; `reason` records the failure.
    Failed,
}

impl StateKind {
    /// The value stored in the ledger's `kind` column, matching the serde
    /// representation so the column and `state_json` cannot disagree.
    pub const fn as_str(self) -> &'static str {
        match self {
            StateKind::Apply => "apply",
            StateKind::Baseline => "baseline",
            StateKind::Bootstrap => "bootstrap",
            StateKind::Staged => "staged",
            StateKind::Failed => "failed",
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
#[serde(deny_unknown_fields)]
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

    /// Explicit module ordering edges as of this state. They sit beside the
    /// schema because creation order is not database state and must not
    /// participate in drift comparison.
    #[serde(default, skip_serializing_if = "ModuleDeps::is_empty")]
    pub module_deps: ModuleDeps,

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

    /// Present only on a [`StateKind::Staged`] checkpoint: how far through the
    /// staged plan this environment is.
    ///
    /// Its absence is what "this environment is not mid-deployment" means, so
    /// it is never written on any other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged: Option<StagedProgress>,
}

/// How far a staged apply has got (ADR-0003 decision 2).
///
/// A staged plan runs outside a transaction, because the whole reason it is
/// staged is that it contains something a transaction cannot hold. Nothing
/// rolls back, so the only honest way to make a mid-way failure visible rather
/// than mysterious is to record each completed statement as it completes —
/// which is also what lets `--resume` know where to start.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedProgress {
    /// How many of the plan's statements have run. The resume point.
    pub completed: usize,

    /// How many there are in total, so a reader of the ledger can see "3 of 5"
    /// without holding the plan file.
    pub total: usize,

    /// The statement that has just completed, verbatim.
    ///
    /// Stored because the operator meeting a failed staged apply needs to know
    /// what did run, and the plan file may be on a machine they do not have.
    pub last_statement: String,
}

impl StagedProgress {
    pub fn is_finished(&self) -> bool {
        self.completed >= self.total
    }
}

impl StateSnapshot {
    pub fn new(kind: StateKind, schema: Schema, ids: IdsFile, operator: impl Into<String>) -> Self {
        Self {
            version: CURRENT_VERSION,
            kind,
            schema,
            ids,
            module_deps: ModuleDeps::default(),
            git_sha: None,
            plan_checksum: None,
            operator: operator.into(),
            reason: None,
            staged: None,
        }
    }

    /// Whether the database still equals this snapshot. Anything else is drift.
    pub fn matches(&self, actual: &Schema) -> bool {
        &self.schema == actual
    }

    /// Refuses a snapshot this build cannot read faithfully.
    ///
    /// Called at every read, because serde would otherwise accept a newer file
    /// by ignoring the fields it does not know — and the fields a state gains
    /// are the objects a drift check compares. Silently reporting "no drift"
    /// about half a schema is the one answer this tool must never give.
    pub fn check_version(&self) -> Result<(), String> {
        if (OLDEST_READABLE_VERSION..=CURRENT_VERSION).contains(&self.version) {
            return Ok(());
        }
        Err(format!(
            "this is a version {} state and this build of pbps reads version {CURRENT_VERSION}. \
             {}",
            self.version,
            if self.version < CURRENT_VERSION {
                "It was recorded by an older pbps; re-record it with `pbps baseline --reason ...`."
            } else {
                "It was recorded by a newer pbps; upgrade this one rather than reading it \
                 partially."
            }
        ))
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

    /// Serde accepts a newer file by ignoring what it does not know, and the
    /// fields a state gains are the objects a drift check compares. "No drift"
    /// about half a schema is the one answer this tool must never give.
    #[test]
    fn a_state_from_another_format_version_is_refused() {
        let mut snap = StateSnapshot::new(
            StateKind::Baseline,
            Schema::default(),
            IdsFile::default(),
            "leon",
        );
        assert!(snap.check_version().is_ok());

        // The version before roles is read as an environment with none: the
        // fields it lacks default to empty, that is a *true* reading of it,
        // and refusing it would leave a deployed environment unreadable, its
        // re-record included (DECISIONS 138). The version before that is
        // refused, because "no module dependencies" is a missing reading
        // rather than a true one (DECISIONS 160).
        snap.version = OLDEST_READABLE_VERSION;
        assert!(snap.check_version().is_ok());
        snap.version = OLDEST_READABLE_VERSION - 1;
        assert!(snap.check_version().unwrap_err().contains("older pbps"));
        snap.version = CURRENT_VERSION + 1;
        assert!(snap.check_version().unwrap_err().contains("newer pbps"));
    }

    /// What an entry from before roles actually holds, read by this build: the
    /// same state with no roles, not an error and not a partial read.
    ///
    /// Version 3 by its number, not by the constant. It is a fixed historical
    /// format, and what makes it unreadable is a specific thing it lacks:
    /// `module_deps`. A test written against `OLDEST_READABLE_VERSION` follows
    /// the constant wherever it goes and pins nothing — it passed unchanged
    /// with the boundary moved back to 3 (DECISIONS 160).
    #[test]
    fn a_snapshot_from_before_module_dependencies_is_refused() {
        let mut snap = StateSnapshot::new(
            StateKind::Apply,
            Schema::default(),
            IdsFile::default(),
            "leon",
        );
        snap.version = 3;
        let e = snap.check_version().expect_err("version 3 is not readable");
        assert!(e.contains("older pbps"), "{e}");
        // And the remedy, because refusing without one strands the operator.
        assert!(e.contains("pbps baseline"), "{e}");
    }

    /// 4 is the one a deployed environment is most likely to be sitting on —
    /// it is what the trunk wrote before this phase — and "the oldest
    /// readable version still works" says nothing about it on its own.
    #[test]
    fn a_snapshot_from_before_roles_reads_as_one_with_no_managed_roles() {
        for version in OLDEST_READABLE_VERSION..CURRENT_VERSION {
            let mut snap = StateSnapshot::new(
                StateKind::Apply,
                Schema::default(),
                IdsFile::default(),
                "leon",
            );
            snap.version = version;
            let mut json: serde_json::Value = serde_json::to_value(&snap).unwrap();
            // A writer before roles never wrote these sections at all.
            json["schema"].as_object_mut().unwrap().remove("roles");
            json["ids"].as_object_mut().unwrap().remove("roles");
            let read: StateSnapshot = serde_json::from_value(json).unwrap();
            assert!(read.check_version().is_ok(), "version {version}");
            assert!(read.schema.roles.is_empty(), "version {version}");
            assert!(read.ids.roles.is_empty(), "version {version}");
            assert_eq!(read.schema, snap.schema, "version {version}");
        }
    }

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

    /// Asserted on the parsed document, not on a substring of it. A snapshot
    /// embeds an [`IdsFile`], which carries a `version` of its own, so
    /// `contains(r#""version":1"#)` went on passing after this format was
    /// bumped to 2, 3 and 4 — matching the nested field every time and
    /// checking nothing about the snapshot's own.
    #[test]
    fn version_is_recorded() {
        let snap = StateSnapshot::new(
            StateKind::Baseline,
            Schema::default(),
            IdsFile::default(),
            "leon",
        );
        let json: serde_json::Value = serde_json::to_value(&snap).unwrap();
        assert_eq!(
            json["version"],
            serde_json::json!(CURRENT_VERSION),
            "{json}"
        );
        assert_eq!(json["kind"], serde_json::json!("baseline"), "{json}");
    }

    /// The ledger writes `kind` into a column of its own so `status` can filter
    /// without parsing JSON; the two spellings must be the same one.
    #[test]
    fn the_kind_column_matches_the_json_spelling() {
        for kind in [
            StateKind::Apply,
            StateKind::Baseline,
            StateKind::Bootstrap,
            StateKind::Staged,
            StateKind::Failed,
        ] {
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

    /// A staged checkpoint is what tells every other command that this
    /// environment is mid-deployment, so the marker has to survive the round
    /// trip through `state_json` intact.
    #[test]
    fn a_staged_checkpoint_carries_its_progress_through_json() {
        let mut snap = StateSnapshot::new(
            StateKind::Staged,
            schema_with("bigint"),
            IdsFile::default(),
            "leon",
        );
        snap.plan_checksum = Some("abc123".into());
        snap.staged = Some(StagedProgress {
            completed: 1,
            total: 3,
            last_statement: "CREATE INDEX [ix] ON [dbo].[t] ([a] ASC);".into(),
        });
        let back: StateSnapshot =
            serde_json::from_str(&serde_json::to_string(&snap).unwrap()).unwrap();
        assert_eq!(back, snap);
        assert!(!back.staged.unwrap().is_finished());
    }

    /// Every other kind must serialize without the marker: its presence is the
    /// signal, and an empty one written on an ordinary apply would leave every
    /// later command believing a deployment is still in flight.
    #[test]
    fn an_ordinary_state_carries_no_staged_marker() {
        let snap = StateSnapshot::new(
            StateKind::Apply,
            Schema::default(),
            IdsFile::default(),
            "leon",
        );
        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.contains("staged"), "{json}");
    }
}
