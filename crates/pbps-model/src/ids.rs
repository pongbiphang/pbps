//! The identity file (`schema.ids.json`).
//!
//! This file is committed to git and holds exactly what **cannot be derived from
//! the declaration files**: the mapping from UID to name, plus tombstones for
//! deleted objects (SPEC §5). Types, nullability and index definitions are never
//! stored here — the YAML already has them, and duplicating them would only
//! create two sources of truth that can disagree.

use std::collections::{BTreeMap, BTreeSet};

use crate::name::{ColumnRef, TableName};
use crate::uid::{Uid, UidKind};

/// The current file-format version.
///
/// It is present from version one so that a compatibility migration is possible
/// later; adding it once it is needed is already too late.
pub const CURRENT_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdsError {
    #[error("the identity file is version {found}, but this tool only supports {supported}")]
    UnsupportedVersion { found: u32, supported: u32 },

    #[error("UID {uid} is both live and tombstoned; the identity file is corrupt")]
    LiveAndTombstoned { uid: Uid },

    // The remedy is spelled out here because no algorithm can pick the survivor:
    // this state usually comes from two branches adding a same-named object and
    // git auto-merging the two lines cleanly (SPEC §5.3).
    #[error(
        "{a} and {b} both point at the name `{name}`; decide which uid survives and delete the other entry from the identity file"
    )]
    DuplicateName { a: Uid, b: Uid, name: String },

    #[error("the prefix of {uid} does not match the section it appears in")]
    KindMismatch { uid: Uid },

    #[error("{uid} points at `{column}`, but no live table entry owns it")]
    OrphanColumn { uid: Uid, column: String },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdsFile {
    pub version: u32,

    #[serde(default)]
    pub tables: BTreeMap<Uid, TableName>,

    #[serde(default)]
    pub columns: BTreeMap<Uid, ColumnRef>,

    /// Database roles (ADR-0005), `r_`-prefixed. A compatible evolution under
    /// the same version: a file without the section has no managed roles.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub roles: BTreeMap<Uid, String>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tombstones: BTreeMap<Uid, Tombstone>,
}

impl Default for IdsFile {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            tables: BTreeMap::new(),
            columns: BTreeMap::new(),
            roles: BTreeMap::new(),
            tombstones: BTreeMap::new(),
        }
    }
}

/// The record left behind by an object that was physically deleted.
///
/// Tombstones live in the identity file rather than in the declaration files, so
/// the declarations only ever contain what you want and never accumulate zombie
/// columns over the years (SPEC §4.4).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tombstone {
    /// The full name at the moment of deletion. An audit has to be able to answer
    /// "what was this column called?".
    pub was: String,
    /// `YYYY-MM-DD`
    pub dropped_at: String,
    pub reason: String,
    pub operator: String,
}

impl IdsFile {
    /// Looks a UID up by name. Diffing does this constantly.
    pub fn table_uid(&self, name: &TableName) -> Option<&Uid> {
        self.tables.iter().find(|(_, n)| *n == name).map(|(u, _)| u)
    }

    pub fn column_uid(&self, r: &ColumnRef) -> Option<&Uid> {
        self.columns.iter().find(|(_, n)| *n == r).map(|(u, _)| u)
    }

    pub fn role_uid(&self, name: &str) -> Option<&Uid> {
        self.roles.iter().find(|(_, n)| *n == name).map(|(u, _)| u)
    }

    /// Whether any live entry or tombstone already holds this uid.
    pub fn contains_uid(&self, uid: &Uid) -> bool {
        self.tables.contains_key(uid)
            || self.columns.contains_key(uid)
            || self.roles.contains_key(uid)
            || self.tombstones.contains_key(uid)
    }

    /// Moves a table, and the columns under it, to a new name.
    ///
    /// The uids do not change — that is the point of an identity file, and it is
    /// what makes this the right operation to record "the object is now called
    /// something else" without deciding anything about identity.
    ///
    /// A no-op when nothing is called `from`, so replaying a sequence of renames
    /// over a mapping that has already absorbed some of them is safe.
    pub fn rename_table(&mut self, from: &TableName, to: &TableName) {
        let Some(uid) = self.table_uid(from).cloned() else {
            return;
        };
        self.tables.insert(uid, to.clone());
        // The columns travel with it, or the mapping would name them under a
        // table that no longer exists.
        for r in self.columns.values_mut() {
            if &r.table == from {
                r.table = to.clone();
            }
        }
    }

    /// Moves a role to a new name, keeping its uid — the same operation as
    /// [`Self::rename_table`], for the same reason, and the same no-op when
    /// nothing is called `from`.
    pub fn rename_role(&mut self, from: &str, to: &str) {
        let Some(uid) = self.role_uid(from).cloned() else {
            return;
        };
        self.roles.insert(uid, to.to_owned());
    }

    /// The name `recorded` has for the same object `self` calls `wanted` —
    /// the name a live environment or a plan's own catalog read currently
    /// holds it under, found by way of the uid rather than the name.
    ///
    /// Falls back to `wanted` itself in the two cases where there is nothing
    /// to resolve through, and both are today's ordinary behaviour rather
    /// than an error: `self` has no uid for `wanted` (nothing has minted one
    /// yet — an object added since the last `plan`, so nothing has been
    /// applied for it anywhere), or `recorded` does not have that uid (this
    /// particular environment has never had the object, so there is nothing
    /// recorded to resolve against). A caller reading an unreadable
    /// `recorded` — a ledger that could not be read at all — must not build
    /// one from `self` to paper over that: passing `IdsFile::default()` falls
    /// through the same "no uid known here" path deliberately, rather than
    /// this function inventing a distinction between "never recorded" and
    /// "could not be read" that it has no way to tell apart.
    ///
    /// Shared by `apply`'s staged-resume bookkeeping (`deploy::live_name`,
    /// historically) and `doctor`'s readiness questions (DECISIONS 440): both
    /// need "what does the object I mean by this name answer to right now",
    /// and both get it from the same two maps.
    pub fn resolved_in(&self, wanted: &TableName, recorded: &IdsFile) -> TableName {
        self.table_uid(wanted)
            .and_then(|uid| recorded.tables.get(uid))
            .cloned()
            .unwrap_or_else(|| wanted.clone())
    }

    /// Checks internal consistency.
    ///
    /// The tool writes this file itself, so under normal conditions it cannot
    /// break. But it is committed to git, which means it can be hand-edited or
    /// mangled by a bad merge resolution. These checks are the last line of
    /// defence before diff treats it as truth.
    pub fn validate(&self) -> Result<(), IdsError> {
        if self.version != CURRENT_VERSION {
            return Err(IdsError::UnsupportedVersion {
                found: self.version,
                supported: CURRENT_VERSION,
            });
        }

        for uid in self.tables.keys() {
            if uid.kind() != UidKind::Table {
                return Err(IdsError::KindMismatch { uid: uid.clone() });
            }
        }
        for uid in self.columns.keys() {
            if uid.kind() != UidKind::Column {
                return Err(IdsError::KindMismatch { uid: uid.clone() });
            }
        }
        for uid in self.roles.keys() {
            if uid.kind() != UidKind::Role {
                return Err(IdsError::KindMismatch { uid: uid.clone() });
            }
        }

        for uid in self.tombstones.keys() {
            if self.tables.contains_key(uid)
                || self.columns.contains_key(uid)
                || self.roles.contains_key(uid)
            {
                return Err(IdsError::LiveAndTombstoned { uid: uid.clone() });
            }
        }

        check_unique(self.tables.iter().map(|(u, n)| (u, n.to_string())))?;
        check_unique(self.columns.iter().map(|(u, n)| (u, n.to_string())))?;
        check_unique(self.roles.iter().map(|(u, n)| (u, n.clone())))?;

        // A live column whose table has no entry cannot be produced by the tool:
        // a table rename moves its columns and a table drop tombstones them. So it
        // means the file was hand-edited or badly merged — and a comparison would
        // then be matching a column that belongs to nothing. Without this, the
        // "identity file is consistent" that `validate` prints claims more than it
        // has checked.
        let live: BTreeSet<&TableName> = self.tables.values().collect();
        for (uid, c) in &self.columns {
            if !live.contains(&c.table) {
                return Err(IdsError::OrphanColumn {
                    uid: uid.clone(),
                    column: c.to_string(),
                });
            }
        }
        Ok(())
    }
}

/// Two UIDs pointing at one name means identity has been scrambled, and every
/// later rename decision would be wrong.
fn check_unique<'a>(it: impl Iterator<Item = (&'a Uid, String)>) -> Result<(), IdsError> {
    let mut seen: BTreeMap<String, &Uid> = BTreeMap::new();
    for (uid, name) in it {
        if let Some(prev) = seen.insert(name.clone(), uid) {
            return Err(IdsError::DuplicateName {
                a: prev.clone(),
                b: uid.clone(),
                name,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid(s: &str) -> Uid {
        s.parse().unwrap()
    }
    fn col(s: &str) -> ColumnRef {
        s.parse().unwrap()
    }

    fn sample() -> IdsFile {
        let mut f = IdsFile::default();
        f.tables
            .insert(uid("t_a9k2mq"), "dbo.customer".parse().unwrap());
        f.columns
            .insert(uid("c_k7x2mq"), col("dbo.customer.customer_id"));
        f.columns
            .insert(uid("c_p3n8vd"), col("dbo.customer.full_name"));
        f
    }

    #[test]
    fn sample_is_valid() {
        sample().validate().unwrap();
    }

    /// A staged apply replays the renames the emitter declared, one statement
    /// at a time, so a table halfway through a cross-schema move can still be
    /// found. The uids must not move: this records where an object *is*, not a
    /// decision about what it is.
    #[test]
    fn renaming_a_table_moves_its_columns_and_keeps_every_uid() {
        let mut f = sample();
        // The two halves of `dbo.customer` -> `sales.client`, in order.
        f.rename_table(
            &"dbo.customer".parse().unwrap(),
            &"sales.customer".parse().unwrap(),
        );
        assert_eq!(
            f.tables[&uid("t_a9k2mq")],
            "sales.customer".parse().unwrap()
        );
        assert_eq!(
            f.columns[&uid("c_k7x2mq")],
            col("sales.customer.customer_id")
        );

        f.rename_table(
            &"sales.customer".parse().unwrap(),
            &"sales.client".parse().unwrap(),
        );
        assert_eq!(f.tables[&uid("t_a9k2mq")], "sales.client".parse().unwrap());
        assert_eq!(f.columns[&uid("c_p3n8vd")], col("sales.client.full_name"));
        f.validate().unwrap();

        // Replaying a rename the mapping has already absorbed changes nothing,
        // which is what makes a resume safe to start from a checkpoint.
        let before = f.clone();
        f.rename_table(
            &"dbo.customer".parse().unwrap(),
            &"sales.customer".parse().unwrap(),
        );
        assert_eq!(f, before);
    }

    /// The role half of the same replay: the uid stays, the name moves, and
    /// a rename already absorbed changes nothing.
    #[test]
    fn renaming_a_role_keeps_its_uid_and_is_idempotent() {
        let mut f = sample();
        f.roles.insert(uid("r_q8m2kd"), "reader".to_owned());
        f.rename_role("reader", "app_reader");
        assert_eq!(f.roles[&uid("r_q8m2kd")], "app_reader");
        assert_eq!(f.role_uid("app_reader"), Some(&uid("r_q8m2kd")));
        let before = f.clone();
        f.rename_role("reader", "app_reader");
        assert_eq!(f, before);
        // A name nobody has is a no-op, not a new role.
        f.rename_role("ghost", "phantom");
        assert_eq!(f, before);
    }

    /// The finding this method exists for (issue #133): a rename that is
    /// declared but not yet applied to a given environment must resolve to
    /// the name *that environment* still has, not the name the declarations
    /// (or another environment's) ids file already moved to.
    #[test]
    fn resolution_prefers_the_recorded_name_over_the_wanted_one() {
        let mut wanted = IdsFile::default();
        wanted
            .tables
            .insert(uid("t_a9k2mq"), "app.new_name".parse().unwrap());
        let mut recorded = IdsFile::default();
        // Same uid, this environment's own name for it: the rename has not
        // reached here yet.
        recorded
            .tables
            .insert(uid("t_a9k2mq"), "app.old_name".parse().unwrap());

        assert_eq!(
            wanted.resolved_in(&"app.new_name".parse().unwrap(), &recorded),
            "app.old_name".parse().unwrap()
        );
    }

    /// Two cases fall through to the name asked with, and both are ordinary
    /// rather than an error: no uid for it in `wanted` at all (nothing has
    /// been applied for this object anywhere yet), and a uid `wanted` knows
    /// that `recorded` does not (this environment has never had it).
    #[test]
    fn resolution_falls_back_to_the_wanted_name_when_it_cannot_resolve() {
        let mut wanted = IdsFile::default();
        wanted
            .tables
            .insert(uid("t_a9k2mq"), "app.t".parse().unwrap());
        let empty = IdsFile::default();

        // No uid for the asked name at all: an object added since the last
        // `plan`.
        assert_eq!(
            wanted.resolved_in(&"app.never_minted".parse().unwrap(), &empty),
            "app.never_minted".parse().unwrap()
        );
        // A uid `wanted` has, but `recorded` — an environment that has never
        // had this table, or whose ledger could not be read at all — does
        // not.
        assert_eq!(
            wanted.resolved_in(&"app.t".parse().unwrap(), &empty),
            "app.t".parse().unwrap()
        );
    }

    #[test]
    fn reverse_lookup_works() {
        let f = sample();
        assert_eq!(
            f.column_uid(&col("dbo.customer.full_name")),
            Some(&uid("c_p3n8vd"))
        );
        assert_eq!(f.column_uid(&col("dbo.customer.nope")), None);
    }

    /// A rename's diff should be a single line — that is what makes the identity
    /// file reviewable.
    #[test]
    fn rename_changes_exactly_one_line() {
        let before = serde_json::to_string_pretty(&sample()).unwrap();
        let mut after = sample();
        after
            .columns
            .insert(uid("c_p3n8vd"), col("dbo.customer.display_name"));
        let after = serde_json::to_string_pretty(&after).unwrap();

        let diff = before
            .lines()
            .zip(after.lines())
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(diff, 1, "a rename should change exactly one line");
        assert_eq!(before.lines().count(), after.lines().count());
    }

    #[test]
    fn version_is_written_and_checked() {
        // The parsed field, not a substring: a literal version in an
        // assertion is one nothing updates when the constant moves, and a
        // nested `version` elsewhere in the document would answer for it.
        let json: serde_json::Value = serde_json::to_value(IdsFile::default()).unwrap();
        assert_eq!(
            json["version"],
            serde_json::json!(CURRENT_VERSION),
            "{json}"
        );

        let mut f = sample();
        f.version = 99;
        assert_eq!(
            f.validate().unwrap_err(),
            IdsError::UnsupportedVersion {
                found: 99,
                supported: 1
            }
        );
    }

    /// A hand edit or a bad merge can produce two UIDs pointing at one column.
    #[test]
    fn duplicate_names_are_rejected() {
        let mut f = sample();
        f.columns
            .insert(uid("c_zzzzzz"), col("dbo.customer.full_name"));
        assert!(matches!(
            f.validate().unwrap_err(),
            IdsError::DuplicateName { .. }
        ));
    }

    #[test]
    fn a_uid_cannot_be_both_live_and_tombstoned() {
        let mut f = sample();
        f.tombstones.insert(
            uid("c_p3n8vd"),
            Tombstone {
                was: "dbo.customer.full_name".into(),
                dropped_at: "2026-08-30".into(),
                reason: "test".into(),
                operator: "leon".into(),
            },
        );
        assert!(matches!(
            f.validate().unwrap_err(),
            IdsError::LiveAndTombstoned { .. }
        ));
    }

    /// A merge that keeps one branch's deletion of a table line and the other's
    /// edit to a column line leaves the column pointing at nothing. Diff would
    /// then compare a column that belongs to no table.
    #[test]
    fn a_column_whose_table_has_no_entry_is_rejected() {
        let mut f = sample();
        f.columns.insert(uid("c_qqqqqq"), col("dbo.ghost.x"));
        assert!(matches!(
            f.validate().unwrap_err(),
            IdsError::OrphanColumn { .. }
        ));
    }

    #[test]
    fn uid_prefix_must_match_its_section() {
        let mut f = sample();
        f.columns.insert(uid("t_bbbbbb"), col("dbo.customer.x"));
        assert!(matches!(
            f.validate().unwrap_err(),
            IdsError::KindMismatch { .. }
        ));
        let mut f = sample();
        f.roles.insert(uid("c_bbbbbb"), "app_reader".into());
        assert!(matches!(
            f.validate().unwrap_err(),
            IdsError::KindMismatch { .. }
        ));
    }

    /// Roles are a compatible evolution of the file: absent means none, two
    /// entries may not share a name, and a tombstoned role is not live.
    #[test]
    fn roles_join_the_file_under_the_same_rules_as_tables() {
        let mut f = sample();
        f.roles.insert(uid("r_aaaaaa"), "app_reader".into());
        f.validate().unwrap();
        assert_eq!(f.role_uid("app_reader"), Some(&uid("r_aaaaaa")));
        assert_eq!(f.role_uid("nobody"), None);
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains("\"roles\""), "{json}");
        let back: IdsFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, f);
        // Absent in an older file: no roles, not a broken file.
        let old: IdsFile =
            serde_json::from_str(&serde_json::to_string(&sample()).unwrap()).unwrap();
        assert!(old.roles.is_empty());
        assert!(!serde_json::to_string(&sample()).unwrap().contains("roles"));

        f.roles.insert(uid("r_bbbbbb"), "app_reader".into());
        assert!(matches!(
            f.validate().unwrap_err(),
            IdsError::DuplicateName { .. }
        ));
    }

    #[test]
    fn round_trips_through_json() {
        let f = sample();
        let back: IdsFile = serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
        assert_eq!(f, back);
    }
}
