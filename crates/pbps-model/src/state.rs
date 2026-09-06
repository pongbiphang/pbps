//! Environment state snapshots — written to each environment's `__pbps_state`
//! (SPEC §8).
//!
//! Keeping state in the database itself rather than in a CI artifact is what
//! makes multi-environment support fall out naturally: dev, staging and prod each
//! remember their own state, and an environment several versions behind needs no
//! artifact passing and no environment-mapping strategy at all.

use crate::declared::Declared;
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
/// Bumped to 6 when a module's key became a [`crate::ModuleId`] (ADR-0009 §1).
/// This one is not an added field but a changed meaning: a version 5 snapshot
/// spells a trigger as `app.audit` with its table in a field beside it, and
/// this build reads `app.audit` as a *view or routine* named `audit` and has
/// nowhere to put the table. Read partially it would say the environment has
/// a module it does not have, and no trigger where there is one. Refused, and
/// re-recorded — the project is pre-release and a baseline is one command
/// (DECISIONS 145).
///
/// Bumped to 7 when the state gained [`Declared`]: what each managed object
/// was declared as when last written, beside what the engine read back
/// (ADR-0009 §2.2, ADR-0013 §3–§4). A version 6 state stays readable, with
/// the field empty — see `OLDEST_READABLE_VERSION`.
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
pub const CURRENT_VERSION: u32 = 7;

/// The oldest snapshot version this build reads as its own.
///
/// 6, not 4: the module key's meaning changed under the same spelling, so
/// there is no reading of an older snapshot that is merely incomplete. And 6,
/// not 7: version 7 adds what was *declared* beside the read-back, and a
/// version 6 state recorded none of it — so "nothing declared here" is what
/// that state truly says, and the differ falls back to the read-back it always
/// compared (DECISIONS 207, the same rule that keeps 4 readable).
pub const OLDEST_READABLE_VERSION: u32 = 6;

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

    /// What was declared when each managed object was last written through
    /// this tool — module text, the three verbatim expressions, and the
    /// bindings — beside the schema read back (ADR-0009 §2.2, ADR-0013 §3–§4).
    /// Empty for a baseline or a snapshot, which record the database as it
    /// stands and applied nothing; and empty in a version 6 state, which
    /// recorded none, so reading one as empty is a *true* reading (DECISIONS
    /// 207).
    #[serde(default, skip_serializing_if = "Declared::is_empty")]
    pub declared: Declared,
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
            declared: Declared::default(),
        }
    }

    /// Whether the database still equals this snapshot. Anything else is drift.
    pub fn matches(&self, actual: &Schema) -> bool {
        &self.schema == actual
    }

    /// Refuses a snapshot this build cannot read faithfully.
    ///
    /// For a snapshot already in hand — one this build built, or one
    /// [`Self::from_json`] has already admitted. Reading one from bytes goes
    /// through `from_json`, which asks this before the shape is parsed at all;
    /// `check_version_number` carries the reason.
    pub fn check_version(&self) -> Result<(), String> {
        check_version_number(self.version)
    }

    /// Reads a snapshot from its JSON form, checking the version **before** the
    /// shape.
    ///
    /// This is the only way to read one, and the order is the whole point.
    /// `StateSnapshot` carries `deny_unknown_fields`, and the fields a version
    /// adds or drops are exactly what an older snapshot spells differently — a
    /// version 5 module still carries a trigger's `on`. Parsed first, such a
    /// snapshot is refused as "unknown field `on`": a field the user never
    /// wrote, naming an internal shape instead of the one fact that explains
    /// the refusal and the one command that fixes it. The version is what the
    /// envelope is for, so it is read on its own first and
    /// `check_version`'s remedy is what the reader sees.
    ///
    /// The strict parse still runs afterwards, and still refuses an unknown
    /// field: within a version this build *does* read, a field it does not know
    /// is a hand-edited or corrupt row, and "absent, empty and unreadable are
    /// three different things".
    pub fn from_json(json: &str) -> Result<Self, String> {
        /// The envelope: the version, and deliberately nothing else.
        ///
        /// Unknown fields are allowed here — they are the rest of the snapshot.
        /// Refusing them is the strict parse's job, and only once the version
        /// says this build is entitled to an opinion about the shape.
        #[derive(serde::Deserialize)]
        struct Envelope {
            version: u32,
        }

        let envelope: Envelope =
            serde_json::from_str(json).map_err(|e| format!("malformed: {e}"))?;
        check_version_number(envelope.version)?;
        serde_json::from_str(json).map_err(|e| format!("malformed: {e}"))
    }
}

/// Refuses a version this build cannot read faithfully.
///
/// Free-standing because the question has to be asked before there is a
/// snapshot to ask it of ([`StateSnapshot::from_json`]);
/// [`StateSnapshot::check_version`] is the same question about one already in
/// hand, and both give the same sentence.
///
/// Asked at every read, because serde would otherwise accept a newer file by
/// ignoring the fields it does not know — and the fields a state gains are the
/// objects a drift check compares. Silently reporting "no drift" about half a
/// schema is the one answer this tool must never give.
fn check_version_number(version: u32) -> Result<(), String> {
    if (OLDEST_READABLE_VERSION..=CURRENT_VERSION).contains(&version) {
        return Ok(());
    }
    Err(format!(
        "this is a version {version} state and this build of pbps reads version \
         {CURRENT_VERSION}. {}",
        if version < CURRENT_VERSION {
            "It was recorded by an older pbps; re-record it with `pbps baseline --reason ...`."
        } else {
            "It was recorded by a newer pbps; upgrade this one rather than reading it \
             partially."
        }
    ))
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

    /// Version 5 is what the trunk wrote before this phase, so it is what a
    /// deployed environment is most likely to be sitting on — and it is
    /// refused, not read partially. A version 5 snapshot spells a trigger as
    /// `app.audit` with its table in a field beside it; this build reads that
    /// key as a view and has nowhere to put the table (ADR-0009 §1). "No
    /// modules of that shape" is not a true reading of such a snapshot, it is
    /// a missing one, so the remedy is to re-record (DECISIONS 145, 203).
    /// Version 6 is the exception the sibling test pins: it recorded nothing
    /// as declared, and reading it so is true (DECISIONS 207).
    #[test]
    fn a_snapshot_from_before_module_identity_is_refused_with_its_remedy() {
        for version in 1..OLDEST_READABLE_VERSION {
            let mut snap = StateSnapshot::new(
                StateKind::Apply,
                Schema::default(),
                IdsFile::default(),
                "leon",
            );
            snap.version = version;
            let e = snap
                .check_version()
                .expect_err("every earlier version is refused");
            assert!(e.contains("older pbps"), "version {version}: {e}");
            assert!(e.contains("pbps baseline"), "version {version}: {e}");
        }
        // Nothing older than 6 is readable, which is what makes the loop above
        // the whole story rather than a sample of it; 6 is readable because
        // the field 7 added is one it truly lacks.
        assert_eq!(OLDEST_READABLE_VERSION, 6);
        assert_eq!(CURRENT_VERSION, 7);
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
    /// The declared record rides in the snapshot's JSON and comes back equal:
    /// module text, all three expressions and a binding. A snapshot that
    /// declares nothing writes no `declared` key at all, so a baseline's JSON
    /// is what it was.
    #[test]
    fn the_declared_record_round_trips_through_json_and_is_absent_when_empty() {
        let mut snap = StateSnapshot::new(
            StateKind::Apply,
            Schema::default(),
            IdsFile::default(),
            "leon",
        );
        assert!(!serde_json::to_string(&snap).unwrap().contains("declared"));
        let t: TableName = "dbo.t".parse().unwrap();
        snap.declared
            .modules
            .insert("dbo.v".parse().unwrap(), "SELECT 1".to_owned());
        snap.declared
            .expressions
            .defaults
            .entry(t.clone())
            .or_default()
            .insert("n".to_owned(), "GETDATE()".to_owned());
        snap.declared
            .expressions
            .checks
            .entry(t.clone())
            .or_default()
            .insert("ck".to_owned(), "n > 0".to_owned());
        snap.declared
            .expressions
            .filters
            .entry(t.clone())
            .or_default()
            .insert("ix".to_owned(), "n > 0 AND label <> 'none'".to_owned());
        snap.declared.bindings.modules.insert(
            "dbo.v".parse().unwrap(),
            crate::declared::Binding {
                candidates: [(
                    "helper".to_owned(),
                    [
                        "app.helper(integer)".to_owned(),
                        "app.helper(text)".to_owned(),
                    ]
                    .into_iter()
                    .collect(),
                )]
                .into_iter()
                .collect(),
            },
        );
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"declared\""), "{json}");
        let back: StateSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snap);
        assert_eq!(back.version, 7);
    }

    /// The one a deployed environment is most likely to hit, and the reason
    /// `from_json` reads the version first.
    ///
    /// A version 5 snapshot spells a trigger as the plain name `app.audit`
    /// with its table in an `on` field beside it (ADR-0009 §1). Both `Module`
    /// and `StateSnapshot` carry `deny_unknown_fields`, so parsing the whole
    /// value first refuses it as "unknown field `on`" — a field the operator
    /// never wrote, naming this build's shape instead of the one fact that
    /// explains the refusal. The version is the envelope's job, so it is read
    /// alone first and the operator gets `check_version`'s remedy.
    ///
    /// Both halves are asserted: that the message names the version and the
    /// command, *and* that it never names the field. Asserting only the first
    /// would pass on a message that carried both.
    #[test]
    fn a_snapshot_from_before_module_ids_is_refused_by_version_not_by_shape() {
        let v5 = r#"{"version":5,"kind":"apply","schema":{"tables":{},"modules":{"app.audit":{"kind":"trigger","definition":"CREATE TRIGGER app.audit ...","on":"dbo.t"}}},"ids":{"version":1,"tables":{},"columns":{}},"operator":"leon"}"#;

        // The shape really is the thing that would have been reported: the
        // strict parse this test exists to get out of the way does fail, and
        // fails naming the field. Without this the test could pass over a
        // fixture that simply parsed.
        let by_shape = serde_json::from_str::<StateSnapshot>(v5)
            .expect_err("a version 5 trigger does not fit this build's shape");
        assert!(by_shape.to_string().contains("on"), "{by_shape}");

        let e = StateSnapshot::from_json(v5).expect_err("version 5 is not readable");
        assert!(e.contains("version 5"), "{e}");
        assert!(e.contains("older pbps"), "{e}");
        assert!(e.contains("pbps baseline"), "{e}");
        assert!(
            !e.contains("unknown field"),
            "the refusal names this build's shape: {e}"
        );
    }

    /// The other half: within a version this build reads, the strict parse
    /// still refuses a field it does not know. Reading the version first buys
    /// a better message for an old snapshot, not a laxer reader for a current
    /// one — a hand-edited or corrupt row is still unreadable, and unreadable
    /// is not the same as empty.
    #[test]
    fn an_unknown_field_in_a_readable_version_is_still_refused() {
        let current = format!(
            r#"{{"version":{CURRENT_VERSION},"kind":"apply","schema":{{"tables":{{}}}},"ids":{{"version":1,"tables":{{}},"columns":{{}}}},"operator":"leon","invented":true}}"#
        );
        let e = StateSnapshot::from_json(&current).expect_err("an unknown field is refused");
        assert!(e.contains("malformed"), "{e}");
        assert!(e.contains("invented"), "{e}");
    }

    /// A snapshot whose version is readable comes back whole, and one that is
    /// not JSON at all is refused as malformed rather than by its version —
    /// the envelope read has its own failure and must not borrow the other's
    /// message.
    #[test]
    fn from_json_reads_a_current_snapshot_and_refuses_bytes_that_are_not_one() {
        let snap = StateSnapshot::new(
            StateKind::Apply,
            Schema::default(),
            IdsFile::default(),
            "leon",
        );
        let json = serde_json::to_string(&snap).unwrap();
        assert_eq!(StateSnapshot::from_json(&json).unwrap(), snap);

        for not_a_snapshot in ["", "null", "{}", "{\"version\":\"seven\"}", "not json"] {
            let e = StateSnapshot::from_json(not_a_snapshot)
                .expect_err("`{not_a_snapshot}` is not a snapshot");
            assert!(e.contains("malformed"), "`{not_a_snapshot}`: {e}");
        }
    }

    /// A version 6 state recorded nothing as declared, so reading it with the
    /// record empty says exactly what that state says — the same rule that
    /// keeps version 4 readable (DECISIONS 160, 207). A newer or older-than-6
    /// state is still refused.
    #[test]
    fn a_version_6_state_reads_with_nothing_declared() {
        let json = r#"{"version":6,"kind":"apply","schema":{"tables":{"dbo.t":{"columns":{"n":{"type":"int","nullable":true,"default":"((0))"}}}}},"ids":{"version":1,"tables":{},"columns":{}},"operator":"leon"}"#;
        let snap: StateSnapshot = serde_json::from_str(json).expect("a version 6 state parses");
        assert!(snap.check_version().is_ok());
        assert!(snap.declared.is_empty());
        assert_eq!(
            snap.schema.tables[&"dbo.t".parse::<TableName>().unwrap()].columns["n"]
                .default
                .as_deref(),
            Some("((0))")
        );
        let newer: StateSnapshot =
            serde_json::from_str(&json.replace("\"version\":6", "\"version\":8")).unwrap();
        assert!(newer.check_version().unwrap_err().contains("newer pbps"));
    }

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
