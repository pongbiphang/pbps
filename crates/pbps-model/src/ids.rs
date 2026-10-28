//! The identity file (`schema.ids.json`).
//!
//! This file is committed to git and holds exactly what **cannot be derived from
//! the declaration files**: the mapping from UID to name, tombstones for
//! deleted objects, and the moves a human allowed (SPEC §5, §6). Types, nullability and index definitions are never
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

/// What the identity file or a plan's final identities got wrong.
///
/// A name in a message is shown escaped by this type's own `Display`: the file
/// is committed text, and a plan carries one that nobody here wrote, so a name
/// that holds a line break or a terminal sequence must not start a line of the
/// refusal (DEC-2032.1, #2035). A uid is safe as it is: it is parsed against a
/// fixed alphabet.
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
        "{a} and {b} both point at the name `{}`; decide which uid survives and delete the other entry from the identity file",
        crate::escape::Escaped(.name)
    )]
    DuplicateName { a: Uid, b: Uid, name: String },

    #[error("the prefix of {uid} does not match the section it appears in")]
    KindMismatch { uid: Uid },

    #[error("{uid} points at `{}`, but no live table entry owns it", crate::escape::Escaped(.column))]
    OrphanColumn { uid: Uid, column: String },

    #[error("a move is recorded for {uid}, but no live table entry has that uid")]
    OrphanMove { uid: Uid },

    #[error(
        "the rebuild recorded for {uid} does not match the identities around it: its sources must be tombstones and its replacements live or retired"
    )]
    InvalidRebuild { uid: Uid },

    #[error("a conversion is recorded for {uid}, but no live column entry has that uid")]
    OrphanConversion { uid: Uid },

    #[error(
        "the conversion recorded for {uid} is blank, or its `from` and `to` are not two different types"
    )]
    InvalidConversion { uid: Uid },

    #[error(
        "the row requirement recorded for {uid} is blank or is not on a table or column tombstone"
    )]
    InvalidDropRequirement { uid: Uid },

    #[error("{uid} points at `{}`, but the snapshot schema does not hold it", crate::escape::Escaped(.name))]
    NotHeld { uid: Uid, name: String },
}

/// An internally consistent identity mapping. Admission checks stay valid:
/// callers can read it, but cannot mutate or deserialize around validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidIds(IdsFile);

impl std::ops::Deref for ValidIds {
    type Target = IdsFile;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
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
    pub roles: BTreeMap<Uid, crate::RoleName>,

    /// Compatible evolution under version 1, as `roles` was (DEC-1635.1).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub types: BTreeMap<Uid, crate::TypeRef>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tombstones: BTreeMap<Uid, Tombstone>,

    /// The moves a human allowed (#1788): by a new range partition's uid, the
    /// DEFAULT partition's rows of its range the plan may move into it. A
    /// compatible evolution under the same version, as `roles` is: a file
    /// without the section allows no move.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub moves: BTreeMap<Uid, Move>,

    /// The lineage of approved blocking rewrites, keyed by the source parent's
    /// uid (DEC-2298.12). Approval has already made the replacement identities
    /// live and the source ones tombstones; a record is what lets an
    /// environment that still holds the source plan the rewrite.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rebuilds: BTreeMap<Uid, crate::Rebuild>,

    /// The conversions a human recorded for a column type change the engine
    /// cannot make by itself (`pbps convert`, #2331), keyed by the column's
    /// uid. A compatible evolution under the same version: a file without the
    /// section records none, and every such change keeps its refusal.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub conversions: BTreeMap<Uid, Conversion>,
}

impl Default for IdsFile {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            tables: BTreeMap::new(),
            columns: BTreeMap::new(),
            roles: BTreeMap::new(),
            types: BTreeMap::new(),
            tombstones: BTreeMap::new(),
            moves: BTreeMap::new(),
            rebuilds: BTreeMap::new(),
            conversions: BTreeMap::new(),
        }
    }
}

/// A move of a DEFAULT partition's rows into a new range partition, allowed by
/// a human (`pbps move-rows`, #1788).
///
/// Moving rows is a delete and an insert, which whatever reads the database's
/// changes sees: a logical slot decoding without a publication, an
/// application holding a row's `xmin`. Whether an environment tolerates that,
/// and whether the table is small enough to move in one transaction, is not
/// in the declarations or the catalog, so it is recorded intent, as a drop's
/// reason is (SPEC §6).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Move {
    /// The most rows the move may take. A count seen when planning does not
    /// bind the database the plan is applied to, so the moving statement
    /// counts again, and refuses above this.
    pub max_rows: u64,
    /// `YYYY-MM-DD`
    pub allowed_at: String,
    pub reason: String,
    pub operator: String,
}

/// A conversion a human recorded for one column type change (`pbps convert`,
/// #2331, DEC-2331.1).
///
/// What a value becomes when the engine will not pick the cast is data
/// transformation that nobody may choose but a person (ADR-0012 section 5), and
/// it is one-time per environment, so it is recorded the way a rename is:
/// answered once, kept in git beside the identities, covered by the plan's
/// checksum and dropped by the tool when it stops applying. It is keyed by the
/// column's uid, so it follows the column through a rename.
///
/// `from` and `to` are canonical types as the dialect spells them, so a record
/// applies to exactly the change it was written for: an environment on another
/// type is refused by name and never converted under an expression written for
/// a different starting point.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Conversion {
    pub from: String,
    pub to: String,
    /// The expression, verbatim.
    pub using: String,
    /// `YYYY-MM-DD`
    pub allowed_at: String,
    pub operator: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require: Option<DropRequirement>,
}

/// Human intent, kept outside schema equality and sealed with the tombstone.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DropRequirement {
    pub predicate: String,
    pub required_at: String,
    pub operator: String,
}

/// Derived before certification; consumers must execute this check or refuse.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DropGuard {
    pub table: TableName,
    pub detach_from: Option<TableName>,
    pub requirements: BTreeMap<Uid, DropRequirement>,
}

impl IdsFile {
    pub fn validated(self) -> Result<ValidIds, IdsError> {
        self.validate()?;
        Ok(ValidIds(self))
    }

    /// Checks only live membership, not complete schema coverage. Tombstones
    /// name retired objects, while a schema may hold objects with no uid.
    /// File baselines require this; Git revisions may retain ids beside a
    /// declaration's deletion, and ledger recording has its own boundary
    /// (DEC-1797.1, DEC-1600.1).
    pub fn check_held_by(&self, schema: &crate::Schema) -> Result<(), IdsError> {
        for (uid, name) in &self.types {
            if !schema.types.contains_key(name) {
                return Err(IdsError::NotHeld {
                    uid: uid.clone(),
                    name: name.raw().to_string(),
                });
            }
        }
        for (uid, name) in &self.tables {
            if !schema.tables.contains_key(name) {
                return Err(IdsError::NotHeld {
                    uid: uid.clone(),
                    name: name.raw().to_string(),
                });
            }
        }
        for (uid, column) in &self.columns {
            if !schema
                .tables
                .get(&column.table)
                .is_some_and(|table| table.columns.contains_key(&column.name))
            {
                return Err(IdsError::NotHeld {
                    uid: uid.clone(),
                    name: column.raw().to_string(),
                });
            }
        }
        for (uid, name) in &self.roles {
            if !schema.roles.contains_key(name) {
                return Err(IdsError::NotHeld {
                    uid: uid.clone(),
                    name: name.raw().to_owned(),
                });
            }
        }
        Ok(())
    }

    /// This mapping without the tables named and their columns: what a state
    /// records for a database that does not hold them.
    ///
    /// A recorded uid is what the next plan reads as "already there": the
    /// differ creates a table only for a uid its base side lacks. A state
    /// that kept the uid of a table the database never had would make the
    /// next plan skip the table and its apply record it as built (#1600).
    /// Tombstones stay: they record identities retired, not objects held.
    #[must_use]
    pub fn without(&self, tables: &[TableName]) -> Self {
        let mut kept = self.clone();
        kept.tables.retain(|_, name| !tables.contains(name));
        kept.columns
            .retain(|_, column| !tables.contains(&column.table));
        let live = kept.tables.clone();
        kept.moves.retain(|uid, _| live.contains_key(uid));
        let (columns, tombstones) = (&kept.columns, &kept.tombstones);
        kept.conversions.retain(|uid, _| columns.contains_key(uid));
        // A lineage whose replacement parts were removed is no longer whole;
        // like a partial record anywhere, it goes with them (DEC-2298.12).
        kept.rebuilds.retain(|_, rebuild| {
            rebuild
                .replacement
                .tables
                .keys()
                .all(|uid| live.contains_key(uid) || tombstones.contains_key(uid))
                && rebuild
                    .replacement
                    .columns
                    .keys()
                    .all(|uid| columns.contains_key(uid) || tombstones.contains_key(uid))
        });
        kept
    }

    /// This mapping without the roles named: what a state records for a
    /// database that does not hold them, as [`IdsFile::without`] is for
    /// tables. A recorded role uid is one the next plan reads as already
    /// there, so a role dropped by hand and kept here is never created again
    /// (#1606).
    #[must_use]
    pub fn without_roles(&self, roles: &[crate::RoleName]) -> Self {
        let mut kept = self.clone();
        kept.roles.retain(|_, name| !roles.contains(name));
        kept
    }

    #[must_use]
    pub fn without_types(&self, types: &[crate::TypeRef]) -> Self {
        let mut kept = self.clone();
        kept.types.retain(|_, name| !types.contains(name));
        kept
    }

    /// Looks a UID up by name, scanning the file. A caller that looks up per
    /// table or per column builds an [`IdsIndex`] once instead (#1870).
    pub fn table_uid(&self, name: &TableName) -> Option<&Uid> {
        self.tables.iter().find(|(_, n)| *n == name).map(|(u, _)| u)
    }

    pub fn column_uid(&self, r: &ColumnRef) -> Option<&Uid> {
        self.columns.iter().find(|(_, n)| *n == r).map(|(u, _)| u)
    }

    pub fn role_uid(&self, name: &crate::RoleName) -> Option<&Uid> {
        self.roles.iter().find(|(_, n)| *n == name).map(|(u, _)| u)
    }

    pub fn type_uid(&self, name: &crate::TypeRef) -> Option<&Uid> {
        self.types.iter().find(|(_, n)| *n == name).map(|(u, _)| u)
    }

    /// Reverse lookups over this file, built once in one pass (#1870).
    pub fn index(&self) -> IdsIndex<'_> {
        IdsIndex::of(self)
    }

    /// Whether any live entry or tombstone already holds this uid.
    pub fn contains_uid(&self, uid: &Uid) -> bool {
        self.tables.contains_key(uid)
            || self.columns.contains_key(uid)
            || self.roles.contains_key(uid)
            || self.types.contains_key(uid)
            || self.tombstones.contains_key(uid)
    }

    /// The lineage record that produced the table with this uid, with the uid
    /// of the source parent it is keyed by.
    pub fn rebuild_into(&self, replacement_root: &Uid) -> Option<(&Uid, &crate::Rebuild)> {
        self.rebuilds
            .iter()
            .find(|(_, rebuild)| &rebuild.replacement_root == replacement_root)
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
    pub fn rename_role(&mut self, from: &crate::RoleName, to: &crate::RoleName) {
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
    /// historically) and `doctor`'s readiness questions (DECISIONS 439): both
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

        for uid in self.types.keys() {
            if uid.kind() != UidKind::Type {
                return Err(IdsError::KindMismatch { uid: uid.clone() });
            }
        }

        for (uid, tombstone) in &self.tombstones {
            if let Some(requirement) = &tombstone.require
                && (requirement.predicate.trim().is_empty()
                    || !matches!(uid.kind(), UidKind::Table | UidKind::Column))
            {
                return Err(IdsError::InvalidDropRequirement { uid: uid.clone() });
            }
            if self.tables.contains_key(uid)
                || self.columns.contains_key(uid)
                || self.roles.contains_key(uid)
                || self.types.contains_key(uid)
            {
                return Err(IdsError::LiveAndTombstoned { uid: uid.clone() });
            }
        }

        check_unique(self.tables.iter().map(|(u, n)| (u, n.raw().to_string())))?;
        check_unique(self.columns.iter().map(|(u, n)| (u, n.raw().to_string())))?;
        check_unique(self.roles.iter().map(|(u, n)| (u, n.raw().to_owned())))?;
        check_unique(self.types.iter().map(|(u, n)| (u, n.raw().to_string())))?;

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
                    column: c.raw().to_string(),
                });
            }
        }
        // A move belongs to a live table: a drop takes its move with it, so
        // one left without its table was written by hand or a bad merge.
        for uid in self.moves.keys() {
            if !self.tables.contains_key(uid) {
                return Err(IdsError::OrphanMove { uid: uid.clone() });
            }
        }
        // A conversion belongs to a live column, and says what it converts
        // between: the tool drops one with its column or its type, so a record
        // left without either was written by hand or a bad merge (#2331).
        for (uid, c) in &self.conversions {
            if !self.columns.contains_key(uid) {
                return Err(IdsError::OrphanConversion { uid: uid.clone() });
            }
            let blank = |t: &str| t.trim().is_empty();
            if blank(&c.using) || blank(&c.from) || blank(&c.to) || c.from == c.to {
                return Err(IdsError::InvalidConversion { uid: uid.clone() });
            }
        }
        // Approval installed the replacements and retired the sources, so a
        // record is whole only while every source identity is a tombstone and
        // every replacement identity is live or retired. Anything else is a
        // half-withdrawn or hand-edited file, which must not read as "nothing
        // to rebuild" (DEC-2298.12).
        let mut reserved = BTreeSet::new();
        for (uid, rebuild) in &self.rebuilds {
            if rebuild.conversions.iter().any(|(to, c)| {
                !rebuild.replacement.columns.contains_key(to)
                    || !rebuild.continues.contains_key(to)
                    || c.from.trim().is_empty()
                    || c.to.trim().is_empty()
                    || c.using.trim().is_empty()
                    || c.from == c.to
            }) {
                return Err(IdsError::InvalidRebuild { uid: uid.clone() });
            }
            let source = rebuild.source.as_ids();
            let target = rebuild.replacement.as_ids();
            let held = |u: &Uid| {
                self.tables.contains_key(u)
                    || self.columns.contains_key(u)
                    || self.tombstones.contains_key(u)
            };
            if rebuild.max_rows == 0
                || rebuild.target_checksum.len() != 64
                || !rebuild
                    .target_checksum
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit())
                || source.validate().is_err()
                || target.validate().is_err()
                || rebuild.source_root() != Some(uid)
                || !rebuild.continues_are_whole()
                || rebuild
                    .types
                    .keys()
                    .any(|u| !rebuild.replacement.columns.contains_key(u))
                || source
                    .tables
                    .keys()
                    .chain(source.columns.keys())
                    .any(|u| !self.tombstones.contains_key(u))
                || target.tables.keys().chain(target.columns.keys()).any(|u| {
                    !held(u)
                        || source.tables.contains_key(u)
                        || source.columns.contains_key(u)
                        || !reserved.insert(u.clone())
                })
            {
                return Err(IdsError::InvalidRebuild { uid: uid.clone() });
            }
        }
        Ok(())
    }
}

/// Name-to-uid lookups over one [`IdsFile`], built in one pass, for the
/// callers that look up once per table or per column (#1870).
///
/// [`IdsFile::table_uid`] and its siblings scan the whole file per call, so a
/// diff asking once per table was quadratic in the schema: 13 s of a 5,000-table
/// zero-change plan. The index borrows the file, so the file cannot change
/// while an index of it exists, and no answer can be stale. Each lookup
/// returns what the scan returns: the lowest uid holding the name, should two
/// hold one.
pub struct IdsIndex<'a> {
    types: BTreeMap<&'a crate::TypeRef, &'a Uid>,
    tables: BTreeMap<&'a TableName, &'a Uid>,
    columns: BTreeMap<&'a ColumnRef, &'a Uid>,
    roles: BTreeMap<&'a str, &'a Uid>,
    columns_by_table: BTreeMap<&'a TableName, BTreeMap<&'a Uid, &'a ColumnRef>>,
}

impl<'a> IdsIndex<'a> {
    fn of(ids: &'a IdsFile) -> Self {
        // In uid order, keeping the first: the scans' `find` answer.
        let mut tables = BTreeMap::new();
        for (uid, name) in &ids.tables {
            tables.entry(name).or_insert(uid);
        }
        let mut columns = BTreeMap::new();
        let mut columns_by_table: BTreeMap<&TableName, BTreeMap<&Uid, &ColumnRef>> =
            BTreeMap::new();
        for (uid, column) in &ids.columns {
            columns.entry(column).or_insert(uid);
            columns_by_table
                .entry(&column.table)
                .or_default()
                .insert(uid, column);
        }
        let mut roles = BTreeMap::new();
        for (uid, name) in &ids.roles {
            roles.entry(name.raw()).or_insert(uid);
        }
        Self {
            types: ids
                .types
                .iter()
                .rev()
                .map(|(uid, name)| (name, uid))
                .collect(),
            tables,
            columns,
            roles,
            columns_by_table,
        }
    }

    /// [`IdsFile::table_uid`], by lookup.
    pub fn table_uid(&self, name: &TableName) -> Option<&'a Uid> {
        self.tables.get(name).copied()
    }

    pub fn type_uid(&self, name: &crate::TypeRef) -> Option<&'a Uid> {
        self.types.get(name).copied()
    }

    /// [`IdsFile::column_uid`], by lookup.
    pub fn column_uid(&self, r: &ColumnRef) -> Option<&'a Uid> {
        self.columns.get(r).copied()
    }

    /// [`IdsFile::role_uid`], by lookup.
    pub fn role_uid(&self, name: &crate::RoleName) -> Option<&'a Uid> {
        self.roles.get(name.raw()).copied()
    }

    /// [`IdsFile::resolved_in`], by lookup.
    pub fn resolved_in(&self, wanted: &TableName, recorded: &IdsFile) -> TableName {
        self.table_uid(wanted)
            .and_then(|uid| recorded.tables.get(uid))
            .cloned()
            .unwrap_or_else(|| wanted.clone())
    }

    /// The file's columns under one table name, by uid.
    pub fn columns_of(&self, table: &TableName) -> impl Iterator<Item = (&'a Uid, &'a ColumnRef)> {
        self.columns_by_table
            .get(table)
            .into_iter()
            .flat_map(|columns| columns.iter().map(|(u, c)| (*u, *c)))
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
    #[test]
    fn duplicate_name_keys_keep_the_raw_table_and_column_spelling() {
        use super::*;
        let part = "a\nb\\c\u{200b}";
        let table = TableName::new("app", part);
        let mut ids = IdsFile::default();
        ids.tables
            .insert("t_aaaaaa".parse().unwrap(), table.clone());
        ids.tables
            .insert("t_bbbbbb".parse().unwrap(), table.clone());
        let IdsError::DuplicateName { name, .. } = ids.validate().unwrap_err() else {
            panic!("the duplicate table must be refused");
        };
        assert_eq!(name, format!("app.{part}"));
        ids.tables.remove(&"t_bbbbbb".parse::<Uid>().unwrap());
        let column = table.column(part);
        ids.columns
            .insert("c_aaaaaa".parse().unwrap(), column.clone());
        ids.columns.insert("c_bbbbbb".parse().unwrap(), column);
        let IdsError::DuplicateName { name, .. } = ids.validate().unwrap_err() else {
            panic!("the duplicate column must be refused");
        };
        assert_eq!(name, format!("app.{part}.{part}"));
        // A literal escape is a different key from the character it resembles.
        ids.columns
            .insert("c_bbbbbb".parse().unwrap(), table.column(r"a\nb\c\u{200b}"));
        assert!(ids.validate().is_ok());
    }

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
        assert_eq!(*sample().validated().unwrap(), sample());
    }

    #[test]
    fn a_validated_mapping_refuses_unsupported_identity_versions() {
        for version in [0, CURRENT_VERSION + 1] {
            let mut ids = sample();
            ids.version = version;
            assert_eq!(
                ids.validated().unwrap_err(),
                IdsError::UnsupportedVersion {
                    found: version,
                    supported: CURRENT_VERSION,
                }
            );
        }
    }

    #[test]
    fn snapshot_membership_requires_every_live_identity_but_not_retired_or_untracked_objects() {
        use crate::{Column, Role, Schema, Table};
        let mut ids = sample();
        ids.roles.insert(uid("r_aaaaaa"), "reader".into());
        ids.tombstones.insert(
            uid("c_zzzzzz"),
            Tombstone {
                require: None,
                was: "dbo.retired.old".into(),
                dropped_at: "2026-10-12".into(),
                reason: "retired".into(),
                operator: "test".into(),
            },
        );
        let ids = ids.validated().unwrap();
        let mut table = Table::default();
        for name in ["customer_id", "full_name", "untracked"] {
            table
                .columns
                .insert(name.into(), Column::new("int".parse().unwrap()));
        }
        let mut schema = Schema::default();
        let customer = "dbo.customer".parse().unwrap();
        schema.tables.insert(customer, table.clone());
        schema
            .tables
            .insert("dbo.untracked".parse().unwrap(), table);
        schema.roles.insert("reader".into(), Role::default());
        schema.roles.insert("untracked".into(), Role::default());
        ids.check_held_by(&schema).unwrap();
        IdsFile::default().check_held_by(&schema).unwrap();

        let mut missing = schema.clone();
        missing.tables.remove(&"dbo.customer".parse().unwrap());
        assert_eq!(
            ids.check_held_by(&missing).unwrap_err(),
            IdsError::NotHeld {
                uid: uid("t_a9k2mq"),
                name: "dbo.customer".into(),
            }
        );
        let mut missing = schema.clone();
        missing
            .tables
            .get_mut(&"dbo.customer".parse().unwrap())
            .unwrap()
            .columns
            .shift_remove("full_name");
        assert_eq!(
            ids.check_held_by(&missing).unwrap_err(),
            IdsError::NotHeld {
                uid: uid("c_p3n8vd"),
                name: "dbo.customer.full_name".into(),
            }
        );
        schema.roles.remove("reader");
        assert_eq!(
            ids.check_held_by(&schema).unwrap_err(),
            IdsError::NotHeld {
                uid: uid("r_aaaaaa"),
                name: "reader".into(),
            }
        );
    }

    /// #1600: a state records only the uids of what the database holds. A
    /// missing table goes with its columns; the tables and columns it holds,
    /// the roles, and every tombstone stay.
    #[test]
    fn a_state_keeps_only_the_uids_the_database_holds() {
        let mut f = sample();
        f.tables
            .insert(uid("t_b4r7wx"), "dbo.order".parse().unwrap());
        f.columns.insert(uid("c_q2m9zt"), col("dbo.order.order_id"));
        f.roles.insert(uid("r_h5t3pk"), "reader".into());
        f.roles.insert(uid("r_w8n2cd"), "writer".into());
        f.tombstones.insert(
            uid("c_z9z9z9"),
            Tombstone {
                require: None,
                was: "dbo.order.legacy".into(),
                dropped_at: "2026-10-07".into(),
                reason: "gone".into(),
                operator: "leon".into(),
            },
        );
        let held = f.without(&["dbo.order".parse().unwrap()]);
        assert!(held.table_uid(&"dbo.order".parse().unwrap()).is_none());
        assert!(held.column_uid(&col("dbo.order.order_id")).is_none());
        // Negative: what the database holds keeps its uid, and tombstones
        // are not objects held.
        assert_eq!(
            held.table_uid(&"dbo.customer".parse().unwrap()),
            Some(&uid("t_a9k2mq"))
        );
        assert_eq!(held.columns.len(), 2);
        assert_eq!(held.roles, f.roles);
        assert_eq!(held.tombstones.len(), 1);
        held.validate().unwrap();
        // Nothing missing, nothing removed.
        assert_eq!(f.without(&[]), f);
    }

    /// #1606: a state records only the role uids the database holds. A
    /// missing role goes; the other roles, every table and column, and the
    /// tombstones stay.
    #[test]
    fn a_state_keeps_only_the_role_uids_the_database_holds() {
        let mut f = sample();
        f.roles.insert(uid("r_h5t3pk"), "reader".into());
        f.roles.insert(uid("r_w8n2cd"), "writer".into());
        let held = f.without_roles(&[("reader".to_owned()).into()]);
        assert!(held.role_uid(&crate::RoleName::from("reader")).is_none());
        // Negative: what the database holds keeps its uid, and the tables
        // are not touched by a role going.
        assert_eq!(
            held.role_uid(&crate::RoleName::from("writer")),
            Some(&uid("r_w8n2cd"))
        );
        assert_eq!(held.tables, f.tables);
        assert_eq!(held.columns, f.columns);
        assert_eq!(held.tombstones, f.tombstones);
        held.validate().unwrap();
        // A name the file does not hold removes nothing, and nothing missing
        // removes nothing.
        assert_eq!(f.without_roles(&[("Reader".to_owned()).into()]), f);
        assert_eq!(f.without_roles(&[]), f);
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
        f.roles
            .insert(uid("r_q8m2kd"), ("reader".to_owned()).into());
        f.rename_role(
            &crate::RoleName::from("reader"),
            &crate::RoleName::from("app_reader"),
        );
        assert_eq!(f.roles[&uid("r_q8m2kd")], ("app_reader").into());
        assert_eq!(
            f.role_uid(&crate::RoleName::from("app_reader")),
            Some(&uid("r_q8m2kd"))
        );
        let before = f.clone();
        f.rename_role(
            &crate::RoleName::from("reader"),
            &crate::RoleName::from("app_reader"),
        );
        assert_eq!(f, before);
        // A name nobody has is a no-op, not a new role.
        f.rename_role(
            &crate::RoleName::from("ghost"),
            &crate::RoleName::from("phantom"),
        );
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
            f.validated().unwrap_err(),
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
            f.validated().unwrap_err(),
            IdsError::DuplicateName { .. }
        ));
    }

    #[test]
    fn a_uid_cannot_be_both_live_and_tombstoned() {
        let mut f = sample();
        f.tombstones.insert(
            uid("c_p3n8vd"),
            Tombstone {
                require: None,
                was: "dbo.customer.full_name".into(),
                dropped_at: "2026-08-30".into(),
                reason: "test".into(),
                operator: "leon".into(),
            },
        );
        assert!(matches!(
            f.validated().unwrap_err(),
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
            f.validated().unwrap_err(),
            IdsError::OrphanColumn { .. }
        ));
    }

    #[test]
    fn uid_prefix_must_match_its_section() {
        let mut f = sample();
        f.columns.insert(uid("t_bbbbbb"), col("dbo.customer.x"));
        assert!(matches!(
            f.validated().unwrap_err(),
            IdsError::KindMismatch { .. }
        ));
        let mut f = sample();
        f.roles.insert(uid("c_bbbbbb"), "app_reader".into());
        assert!(matches!(
            f.validated().unwrap_err(),
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
        assert_eq!(
            f.role_uid(&crate::RoleName::from("app_reader")),
            Some(&uid("r_aaaaaa"))
        );
        assert_eq!(f.role_uid(&crate::RoleName::from("nobody")), None);
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
            f.validated().unwrap_err(),
            IdsError::DuplicateName { .. }
        ));
    }

    /// #1788: a move joins the file as roles did, under the same version: an
    /// older file without the section allows no move and still reads, and
    /// an empty section is not written. A move without its live table, and
    /// one whose table a state leaves out, are refused and dropped.
    #[test]
    fn moves_join_the_file_and_belong_to_a_live_table() {
        let allowed = Move {
            max_rows: 1000,
            allowed_at: "2026-10-11".into(),
            reason: "no CDC on this table".into(),
            operator: "leon".into(),
        };
        let mut f = sample();
        f.moves.insert(uid("t_a9k2mq"), allowed.clone());
        f.validate().unwrap();
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains("\"moves\""), "{json}");
        let back: IdsFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, f);
        let old: IdsFile =
            serde_json::from_str(&serde_json::to_string(&sample()).unwrap()).unwrap();
        assert!(old.moves.is_empty());
        assert!(!serde_json::to_string(&sample()).unwrap().contains("moves"));
        // A state for a database without the table keeps no move for it.
        assert!(
            f.without(&["dbo.customer".parse().unwrap()])
                .moves
                .is_empty()
        );
        // Negative: a move whose table has no live entry.
        f.moves.insert(uid("t_bbbbbb"), allowed);
        assert!(matches!(
            f.validated().unwrap_err(),
            IdsError::OrphanMove { .. }
        ));
    }

    fn recorded() -> Conversion {
        Conversion {
            from: "text".into(),
            to: "integer".into(),
            using: "amount::integer".into(),
            allowed_at: "2026-10-24".into(),
            operator: "leon".into(),
        }
    }

    /// #2331: a conversion joins the file under the same version, as a move
    /// does: an older file records none and still reads, an empty section is
    /// not written, and a state for a database without the column drops it.
    #[test]
    fn conversions_join_the_file_and_belong_to_a_live_column() {
        let mut f = sample();
        let column = f.columns.keys().next().unwrap().clone();
        f.conversions.insert(column.clone(), recorded());
        f.validate().unwrap();
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains("\"conversions\""), "{json}");
        assert_eq!(serde_json::from_str::<IdsFile>(&json).unwrap(), f);
        let plain = serde_json::to_string(&sample()).unwrap();
        assert!(!plain.contains("conversions"));
        assert!(
            serde_json::from_str::<IdsFile>(&plain)
                .unwrap()
                .conversions
                .is_empty()
        );
        let table = f.columns[&column].table.clone();
        assert!(f.without(&[table]).conversions.is_empty());
    }

    /// Negative cases: a record for a column the file does not hold, and one
    /// that converts nothing or has nothing to run, is a hand edit or a bad
    /// merge and is refused rather than read as a conversion.
    #[test]
    fn a_conversion_without_its_column_or_its_expression_is_refused() {
        let mut f = sample();
        f.conversions.insert(uid("c_bbbbbb"), recorded());
        assert!(matches!(
            f.validate().unwrap_err(),
            IdsError::OrphanConversion { .. }
        ));
        let column = sample().columns.keys().next().unwrap().clone();
        for broken in [
            Conversion {
                using: " \n".into(),
                ..recorded()
            },
            Conversion {
                from: String::new(),
                ..recorded()
            },
            Conversion {
                to: "text".into(),
                ..recorded()
            },
        ] {
            let mut f = sample();
            f.conversions.insert(column.clone(), broken);
            assert!(matches!(
                f.validate().unwrap_err(),
                IdsError::InvalidConversion { .. }
            ));
        }
        assert!(
            serde_json::from_str::<IdsFile>(
                r#"{"version":1,"conversions":{"c_aaaaaa":{"from":"a","to":"b","using":"x","allowed_at":"d","operator":"o","extra":1}}}"#
            )
            .is_err(),
            "an unknown key is not a record"
        );
    }

    #[test]
    fn round_trips_through_json() {
        let f = sample();
        let back: IdsFile = serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
        assert_eq!(f, back);
    }

    /// Every [`IdsIndex`] lookup answers what the scan it replaces answers
    /// (#1870), over pseudo-random files where names repeat across uids (the
    /// lowest uid holding one wins in both) and lookups miss: absent names,
    /// a column under the wrong table, a table only a column names.
    #[test]
    fn the_index_answers_what_the_scans_answer() {
        let mut seed: u64 = 0x1870;
        let mut next = |bound: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % bound
        };
        let tables = ["a.t", "a.u", "b.t", "b.v"];
        let columns = ["id", "name", "note"];
        let roles = ["app", "reader", "writer"];
        for round in 0..200 {
            let mut f = IdsFile::default();
            for i in 0..next(8) {
                let name = tables[next(4) as usize].parse().unwrap();
                f.tables.insert(uid(&format!("t_{i:06}")), name);
            }
            for i in 0..next(16) {
                let table: TableName = tables[next(4) as usize].parse().unwrap();
                let column = table.column(columns[next(3) as usize]);
                f.columns.insert(uid(&format!("c_{i:06}")), column);
            }
            for i in 0..next(5) {
                f.roles
                    .insert(uid(&format!("r_{i:06}")), roles[next(3) as usize].into());
            }
            let index = f.index();
            let other = sample();
            for t in tables {
                let t: TableName = t.parse().unwrap();
                assert_eq!(
                    index.table_uid(&t),
                    f.table_uid(&t),
                    "round {round}: {t}",
                    t = t.raw()
                );
                assert_eq!(index.resolved_in(&t, &other), f.resolved_in(&t, &other));
                let scanned: Vec<(&Uid, &ColumnRef)> =
                    f.columns.iter().filter(|(_, c)| c.table == t).collect();
                assert_eq!(index.columns_of(&t).collect::<Vec<_>>(), scanned);
                for c in columns {
                    let c = t.column(c);
                    assert_eq!(
                        index.column_uid(&c),
                        f.column_uid(&c),
                        "round {round}: {c}",
                        c = c.raw()
                    );
                }
            }
            for r in roles.iter().chain(&["absent"]) {
                assert_eq!(
                    index.role_uid(&crate::RoleName::from(*(r))),
                    f.role_uid(&crate::RoleName::from(*(r))),
                    "round {round}: {r}"
                );
            }
            let absent: TableName = "z.absent".parse().unwrap();
            assert_eq!(index.table_uid(&absent), None);
            assert_eq!(index.columns_of(&absent).count(), 0);
            assert_eq!(index.column_uid(&absent.column("id")), None);
        }
    }

    /// The names in a refusal are the file's, and a file may hold a line break
    /// or a terminal sequence in a name (DEC-2032.1, #2035).
    #[test]
    fn a_refusal_shows_the_name_it_names_escaped() {
        let hostile = "x\n\u{1b}[31mred\r";
        let shown = "x\\n\\u{1b}[31mred\\r";
        let mut duplicate = sample();
        duplicate.tables.clear();
        duplicate.columns.clear();
        duplicate
            .tables
            .insert(uid("t_a9k2mq"), TableName::new("app", hostile));
        duplicate
            .tables
            .insert(uid("t_b9k2mq"), TableName::new("app", hostile));
        let mut orphan = IdsFile::default();
        orphan
            .columns
            .insert(uid("c_k7x2mq"), TableName::new("app", hostile).column("c"));
        let mut not_held = sample();
        not_held.tables.clear();
        not_held.columns.clear();
        not_held
            .tables
            .insert(uid("t_a9k2mq"), TableName::new("app", hostile));
        for (what, refusal) in [
            ("a duplicate name", duplicate.validate().unwrap_err()),
            ("an orphan column", orphan.validate().unwrap_err()),
            (
                "a name the schema does not hold",
                IdsError::NotHeld {
                    uid: uid("t_a9k2mq"),
                    name: format!("app.{hostile}"),
                },
            ),
        ] {
            let text = refusal.to_string();
            assert!(!text.chars().any(char::is_control), "{what}: {text:?}");
            assert!(text.contains(shown), "{what}: {text}");
        }
        // Negative: a Unicode name is shown as the file spells it.
        let mut unicode = IdsFile::default();
        unicode.columns.insert(uid("c_k7x2mq"), col("app.名前.ünï"));
        let text = unicode.validate().unwrap_err().to_string();
        assert!(text.contains("`app.名前.ünï`"), "{text}");
    }
}

#[cfg(test)]
mod managed_type_tests {
    use super::*;

    #[test]
    fn type_ids_are_optional_under_version_one_and_validate_their_own_namespace() {
        let mut ids: IdsFile = serde_json::from_str(r#"{"version":1}"#).unwrap();
        assert!(ids.types.is_empty());
        let name = crate::TypeRef::new("app", "mood").unwrap();
        let uid: Uid = "y_000001".parse().unwrap();
        ids.types.insert(uid.clone(), name.clone());
        ids.validate().unwrap();
        assert_eq!(
            serde_json::from_str::<IdsFile>(&serde_json::to_string(&ids).unwrap()).unwrap(),
            ids
        );
        let mut wrong = ids.clone();
        wrong
            .types
            .insert("r_000002".parse().unwrap(), name.clone());
        assert!(matches!(
            wrong.validate(),
            Err(IdsError::KindMismatch { .. })
        ));
        let mut duplicate = ids.clone();
        duplicate
            .types
            .insert("y_000002".parse().unwrap(), name.clone());
        assert!(matches!(
            duplicate.validate(),
            Err(IdsError::DuplicateName { .. })
        ));
        assert!(matches!(
            ids.check_held_by(&crate::Schema::default()),
            Err(IdsError::NotHeld { .. })
        ));
        ids.tombstones.insert(
            uid,
            Tombstone {
                require: None,
                was: name.raw().to_string(),
                dropped_at: "2026-10-23".into(),
                reason: "fixture".into(),
                operator: "test".into(),
            },
        );
        assert!(matches!(
            ids.validate(),
            Err(IdsError::LiveAndTombstoned { .. })
        ));
    }
}
