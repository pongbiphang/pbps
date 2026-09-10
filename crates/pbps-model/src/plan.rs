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

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::change::ChangeSet;
use crate::ids::IdsFile;
use crate::module::ModuleDeps;
use crate::schema::Schema;

/// The current plan-file format version.
///
/// Bumped to 2 when `mode` and `strategy` arrived. Both change what *executing*
/// the plan does, and serde would let an older `apply` read the file, ignore the
/// unknown fields, and run a staged plan inside a transaction with the reviewed
/// online strategy silently dropped. `apply` compares this exactly, so an older
/// deployment host refuses the artifact instead.
///
/// Bumped to 3 when the post-plan module dependency annotations joined the
/// artifact. A connected plan needs the baseline annotations to order drops,
/// and the state written after apply needs the new ones for the next plan.
///
/// Bumped to 4 for everything Phase 4 puts in the file: `data`, which says
/// which tables' rows the state recorded after the apply has to cover, and
/// the guard each row change carries — `InsertRow`'s `defaults` and `types`,
/// `UpdateRow`'s `types` and `after_types`, `DeleteRow`'s `row`, `types` and
/// `after_types` (DECISIONS 133, 136, 137, 140, 143, 149). Every one of them
/// is `#[serde(default)]`, so an older `apply` reads the file, drops them
/// silently, and executes the same DML with none of its preconditions or
/// postconditions — recording, on top of that, a state with no rows in it and
/// leaving every later `verify` blind to what it had just written. That is
/// the exact failure this number exists to prevent.
///
/// One bump covers them all: they arrived in one release cycle, and a version
/// per guard would invalidate saved plans seven times over for one story. It
/// covers the type *pair* on `UpdateRow` and `DeleteRow` too, which changes
/// what `types` means rather than adding a field — a reader without it would
/// take `types` for the type the column has now, and hold a retyped cell to
/// the old type's spelling of a value the engine has already converted
/// (DECISIONS 149).
/// Bumped to 5 with the state's own bump to 6: a saved plan carries a
/// `Schema` and a `ChangeSet`, and both spell a module's identity the new way
/// (ADR-0009 §1, DECISIONS 203). An older `apply` reading this file would take
/// a trigger's three-part key for a name it cannot parse; a newer one reading
/// an older file would take `app.audit` for a view. Neither is a partial reading, so
/// the number moves.
///
/// Bumped to 6 when [`state_checksum`] began sorting a table's columns
/// (DECISIONS 238). This is the one artifact carrying a fingerprint *written*
/// by one build and *recomputed* by another: `apply` compares
/// `baseline.checksum` against what it computes from the live database, so a
/// version 5 plan — whose string came from the order-sensitive algorithm —
/// read by this build would be refused as drift against a database nobody had
/// touched, and a version 6 plan read by an older build would be refused the
/// same way in the other direction. Neither refusal names the real reason,
/// which is what this number is for: the plan is turned away as a format this
/// build does not understand, and the remedy is the one that was always
/// right for a stale artifact — run `plan --db` again and take the new plan
/// through the gate.
///
/// Bumped to 7 when intrinsic risk derivation began classifying `DropUnique`
/// as destructive. A version 6 plan can carry no risk for that change, while
/// this build derives `destructive`; without this bump, `validate_saved_plan`
/// would reject it as an edited or broken artifact instead of identifying it as
/// stale and asking for a new plan.
pub const CURRENT_VERSION: u32 = 7;

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

/// How a plan is to be executed (ADR-0003 decision 2).
///
/// The default is the rule of SPEC §7.5 and is not weakened by this enum
/// existing: a [`PlanMode::Transactional`] plan is still all or nothing. What
/// [`PlanMode::Staged`] adds is a way through for the operation a transaction
/// cannot hold at all — isolated in a deployment of its own, applied statement
/// by statement with each completion recorded, and resumable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanMode {
    /// One plan, one transaction, all or nothing.
    #[default]
    Transactional,
    /// One logical change, applied outside a transaction with a per-statement
    /// checkpoint in the ledger.
    Staged,
}

impl PlanMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            PlanMode::Transactional => "transactional",
            PlanMode::Staged => "staged",
        }
    }

    pub const fn is_staged(self) -> bool {
        matches!(self, PlanMode::Staged)
    }
}

impl std::fmt::Display for PlanMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the plan was computed against.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct SavedPlan {
    pub version: u32,

    pub origin: PlanOrigin,

    /// How this plan is to be executed.
    ///
    /// It is a value in the file, not a flag at apply time, for the same reason
    /// [`PlanOrigin`] is: what the deployment gate approved has to be what
    /// runs. `apply` refuses a mismatch between the file and the flag rather
    /// than silently doing whichever the operator typed.
    ///
    /// Defaulted on read so that the field can be omitted from a transactional
    /// plan; a file old enough to predate it is refused by the version check
    /// instead, since an old *reader* is the dangerous direction.
    #[serde(default)]
    pub mode: PlanMode,

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

    /// The explicit module ordering edges after this plan.
    ///
    /// These remain annotations rather than schema state: they do not
    /// participate in drift comparison. They travel with the artifact and the
    /// ledger so a later connected plan can order drops for modules whose
    /// declaration file has disappeared.
    #[serde(default, skip_serializing_if = "ModuleDeps::is_empty")]
    pub module_deps: ModuleDeps,

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

    /// The tables whose rows are under management **after** this plan, with
    /// their mode and declared keys (ADR-0004).
    ///
    /// Carried for the same reason `ids` is: the state `apply` records is the
    /// database read back, and reading rows back needs a scope — which of a
    /// table's rows are declared is not a fact the database holds. It is the
    /// declarations' scope at plan time, so `apply` needs no checkout.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub data: crate::data::DataScopes,
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
            mode: PlanMode::Transactional,
            dialect: dialect.into(),
            created_at: created_at.into(),
            git_sha: None,
            baseline,
            changes,
            module_deps: ModuleDeps::default(),
            ids,
            data: BTreeMap::new(),
        }
    }

    /// This plan's own fingerprint. See [`plan_checksum`].
    pub fn checksum(&self) -> String {
        plan_checksum(self)
    }

    pub fn staged(mut self) -> Self {
        self.mode = PlanMode::Staged;
        self
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
/// Determinism comes from the model's collections being `BTreeMap` / `BTreeSet`.
/// Without that, this function would return a different answer for the same
/// database on every run and the drift check would be a coin flip.
///
/// The one exception is `Table::columns`, an `IndexMap` whose order is the
/// column layout of `CREATE TABLE`; it is sorted by name here, and only here,
/// so that this answers the same question `Schema`'s own `==` answers
/// (DECISIONS 238). Serialized in declaration order it did not: a column
/// dropped and re-added by hand comes back last, and the differ — which
/// compares with `==`, and so ignores order — reported no changes while this
/// checksum said the environment had moved. `verify` then named the drift and
/// listed nothing that drifted, and `plan --db` refused with no forward path
/// but a re-baseline. The order itself is still recorded in `state_json` and
/// still decides what `CREATE TABLE` emits from the declarations; it is only
/// not part of this fingerprint.
pub fn state_checksum(schema: &Schema, ids: &IdsFile) -> String {
    // By value, not by reference: the sort is the canonical form this hash is
    // taken over, and the caller's schema — the recorded snapshot, or the one
    // just read from the catalog — keeps the order it came in with.
    let mut canonical = schema.clone();
    for table in canonical.tables.values_mut() {
        table.columns.sort_unstable_keys();
    }
    #[derive(serde::Serialize)]
    struct Fingerprint<'a> {
        schema: &'a Schema,
        ids: &'a IdsFile,
    }
    digest_of(&Fingerprint {
        schema: &canonical,
        ids,
    })
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
    use crate::module::{Module, ModuleKind};
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

    /// One table whose columns are declared in the order given.
    fn schema_of(columns: &[&str]) -> Schema {
        let mut map = IndexMap::new();
        for c in columns {
            map.insert(
                (*c).to_string(),
                Column::new("nvarchar(200)".parse::<ColumnType>().unwrap()),
            );
        }
        let mut s = Schema::default();
        s.tables.insert(
            TableName::new("dbo", "customer"),
            Table {
                columns: map,
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

    /// The two rules that answer "has this environment moved" must answer it
    /// the same way. `Schema`'s `==` ignores column order; a fingerprint that
    /// did not made a hand-made drop-and-re-add of one column — which comes
    /// back last — into drift the differ could not name and no plan could
    /// resolve (DECISIONS 238).
    #[test]
    fn column_order_does_not_change_the_state_checksum() {
        let ids = ids_with("t_a1b2c3");
        let one = schema_of(&["id", "note", "email"]);
        let other = schema_of(&["id", "email", "note"]);
        assert_eq!(one, other, "equality already ignores column order");
        assert_eq!(state_checksum(&one, &ids), state_checksum(&other, &ids));
    }

    /// A saved plan is the one artifact carrying a fingerprint *written* by one
    /// build and *recomputed* by another: `apply` compares `baseline.checksum`
    /// against what it computes from the live database. So the value below is
    /// pinned. If it changes, the algorithm changed, and
    /// [`CURRENT_VERSION`] has to change with it (DECISIONS 238) — otherwise
    /// a plan from the previous build is refused as drift against a database
    /// nobody touched, which names the wrong problem and sends the operator
    /// to reconcile nothing.
    #[test]
    fn the_fingerprint_is_pinned_to_the_plan_format_version() {
        assert_eq!(
            state_checksum(&schema_of(&["id", "note", "email"]), &ids_with("t_a1b2c3")),
            "ea1c85e7867a7a63332cf5f7ca6e8356b64a6d3cbd4c7a503222bc7d3d40f1d9"
        );
        assert_eq!(CURRENT_VERSION, 7);
    }

    /// Sorting the keys must not sort away a difference. The same three
    /// columns under a fourth name, and one column fewer, are both changes the
    /// checksum has to keep seeing.
    #[test]
    fn a_renamed_or_dropped_column_still_changes_the_state_checksum() {
        let ids = ids_with("t_a1b2c3");
        let base = state_checksum(&schema_of(&["id", "note", "email"]), &ids);
        assert_ne!(
            base,
            state_checksum(&schema_of(&["id", "notes", "email"]), &ids),
            "a renamed column"
        );
        assert_ne!(
            base,
            state_checksum(&schema_of(&["id", "note"]), &ids),
            "a dropped column"
        );
    }

    /// The plan is the other artifact, and its checksum pins the SQL that will
    /// run: `CREATE TABLE` emits the columns in the order the table carries
    /// them, so two plans that would emit two different statements must stay
    /// two different plans.
    #[test]
    fn column_order_still_changes_a_plans_own_checksum() {
        let created = |order: &[&str]| {
            plan_over(ChangeSet {
                changes: vec![PlannedChange::new(Change::CreateTable {
                    uid: "t_a1b2c3".parse().unwrap(),
                    name: TableName::new("dbo", "customer"),
                    table: Box::new(
                        schema_of(order)
                            .tables
                            .remove(&TableName::new("dbo", "customer"))
                            .unwrap(),
                    ),
                })],
            })
            .checksum()
        };
        assert_ne!(created(&["id", "note"]), created(&["note", "id"]));
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
        // The parsed field, not a substring: a plan embeds an `IdsFile`,
        // whose own `version` would answer a `contains` the day the two
        // constants meet.
        let json: serde_json::Value =
            serde_json::to_value(plan_over(ChangeSet::default())).unwrap();
        assert_eq!(
            json["version"],
            serde_json::json!(CURRENT_VERSION),
            "{json}"
        );
        assert_eq!(json["origin"], serde_json::json!("database"), "{json}");
    }

    #[test]
    fn unknown_scope_and_finding_fields_in_saved_plans_are_refused() {
        let mut plan = plan_over(ChangeSet {
            changes: vec![PlannedChange::new(Change::CreateRole {
                name: "app_reader".into(),
                uid: "r_a1b2c3".parse().unwrap(),
            })],
        });
        plan.data.insert(
            TableName::new("dbo", "customer"),
            crate::DataScope {
                mode: crate::DataMode::Ensure,
                keys: [crate::RowKey("1".into())].into_iter().collect(),
            },
        );
        plan.changes.changes[0].findings.push(crate::Finding::new(
            "naming.role",
            crate::Severity::Warning,
            "example",
            Some("role app_reader".into()),
        ));
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(
            serde_json::from_value::<SavedPlan>(json.clone()).unwrap(),
            plan
        );
        for (pointer, field) in [
            ("/data/dbo.customer", "kyes"),
            ("/changes/changes/0/findings/0", "subjet"),
        ] {
            let mut malformed = json.clone();
            malformed.pointer_mut(pointer).unwrap()[field] = serde_json::json!({});
            let error = serde_json::from_value::<SavedPlan>(malformed)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(&format!("unknown field `{field}`")),
                "{error}"
            );
        }
    }

    #[test]
    fn unknown_plan_fields_are_refused() {
        let mut json = serde_json::to_value(plan_over(ChangeSet::default())).unwrap();
        json["approved"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<SavedPlan>(json).is_err());
    }

    #[test]
    fn unknown_change_fields_are_refused() {
        let mut json = serde_json::to_value(plan_over(ChangeSet {
            changes: vec![PlannedChange::new(Change::DropTable {
                uid: "t_a1b2c3".parse().unwrap(),
                name: TableName::new("dbo", "customer"),
            })],
        }))
        .unwrap();
        json["changes"]["changes"][0]["approved"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<SavedPlan>(json).is_err());
    }

    /// Strictness has to follow the whole artifact tree. Checking only the
    /// SavedPlan and Change envelopes would still let a typo silently disappear
    /// from the reviewed execution strategy or model payload before hashing.
    #[test]
    fn unknown_nested_plan_fields_are_refused() {
        let add_column = || {
            plan_over(ChangeSet {
                changes: vec![PlannedChange::new(Change::AddColumn {
                    uid: "c_a1b2c3".parse().unwrap(),
                    table: TableName::new("dbo", "customer"),
                    name: "score".into(),
                    column: Box::new(Column::new("int".parse().unwrap())),
                })],
            })
        };

        let mut column = serde_json::to_value(add_column()).unwrap();
        column["changes"]["changes"][0]["column"]["nulable"] = serde_json::Value::Bool(false);
        assert!(serde_json::from_value::<SavedPlan>(column).is_err());

        let mut strategy = serde_json::to_value(add_column()).unwrap();
        strategy["changes"]["changes"][0]["strategy"] = serde_json::json!({ "onlien": true });
        assert!(serde_json::from_value::<SavedPlan>(strategy).is_err());

        let mut table = serde_json::to_value(plan_over(ChangeSet {
            changes: vec![PlannedChange::new(Change::CreateTable {
                uid: "t_a1b2c3".parse().unwrap(),
                name: TableName::new("dbo", "customer"),
                table: Box::new(Table::default()),
            })],
        }))
        .unwrap();
        table["changes"]["changes"][0]["table"]["colums"] = serde_json::json!({});
        assert!(serde_json::from_value::<SavedPlan>(table).is_err());

        let mut module = serde_json::to_value(plan_over(ChangeSet {
            changes: vec![PlannedChange::new(Change::CreateModule {
                id: crate::ModuleId::Named(TableName::new("dbo", "active_customer")),
                module: Box::new(Module {
                    kind: ModuleKind::View,
                    description: None,
                    definition: "SELECT 1 AS id".into(),
                }),
            })],
        }))
        .unwrap();
        module["changes"]["changes"][0]["module"]["defintion"] =
            serde_json::Value::String("SELECT 2 AS id".into());
        assert!(serde_json::from_value::<SavedPlan>(module).is_err());
    }

    /// The mode is part of the pinned artifact: a plan approved as staged must
    /// not be applyable as a transactional one, and the checksum is what makes
    /// "approved" mean anything.
    #[test]
    fn the_execution_mode_is_pinned_by_the_checksum() {
        let plain = plan_over(ChangeSet::default());
        let staged = plan_over(ChangeSet::default()).staged();
        assert_eq!(plain.mode, PlanMode::Transactional);
        assert!(staged.mode.is_staged());
        assert_ne!(plain.checksum(), staged.checksum());
    }

    /// Plans written before staged apply existed carry no `mode` at all, and
    /// they are transactional plans — the field must default, not fail.
    #[test]
    fn a_plan_without_a_mode_reads_as_transactional() {
        let mut json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&plan_over(ChangeSet::default())).unwrap())
                .unwrap();
        json.as_object_mut().unwrap().remove("mode");
        let back: SavedPlan = serde_json::from_value(json).unwrap();
        assert_eq!(back.mode, PlanMode::Transactional);
    }
}
