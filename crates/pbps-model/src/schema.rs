//! The structure of the desired state.
//!
//! # Two deliberate design constraints
//!
//! **Containers hold names; elements do not.** `Table` has no `name` field and
//! `Column` has no `name` field — the name is the key in the parent map. This
//! eliminates an entire class of bugs that cannot detect themselves: a map key
//! disagreeing with the name stored inside.
//!
//! **The model expresses state, never intent.** Transient annotations such as
//! `renamed_from` do not live here; `pbps-load` returns them separately. The
//! reason is that `Schema` must satisfy "two semantically identical schemas are
//! equal", which both diff and drift detection are built on. Mixing in one-shot
//! intent would make the same state compare unequal depending on whether an
//! annotation happened to be present.

use std::collections::BTreeMap;

use indexmap::IndexMap;

use crate::data::TableData;
use crate::module::{Module, ModuleId};
use crate::name::TableName;
use crate::types::ColumnType;

/// The complete desired state of a project.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schema {
    pub tables: BTreeMap<TableName, Table>,

    /// Views, procedures, functions and triggers (ADR-0002).
    ///
    /// Keyed by [`ModuleId`], not by name: what identifies a module depends on
    /// its kind — a routine by its signature, a trigger by its table
    /// (ADR-0009 §1).
    ///
    /// They live in the same [`Schema`] as the tables because they are part of
    /// the desired state and of the drift comparison — but they carry no data,
    /// so they get none of the identity machinery and never appear in the ids
    /// file.
    ///
    /// Defaulted on read: every state snapshot and plan written before modules
    /// existed is a project with no modules, not a broken file.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub modules: BTreeMap<ModuleId, Module>,

    /// Database roles and what they are granted (ADR-0005).
    ///
    /// By name, like modules — but unlike modules they carry identity in the
    /// ids file, because dropping and recreating a role destroys its
    /// membership, which pbps does not manage and cannot restore.
    ///
    /// Defaulted on read: a snapshot written before roles existed describes an
    /// environment with no managed roles, not a broken file.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub roles: BTreeMap<String, crate::role::Role>,
}

impl Schema {
    pub fn get(&self, name: &TableName) -> Option<&Table> {
        self.tables.get(name)
    }

    /// This schema with every column collation equal to `default` taken out
    /// (#1175).
    ///
    /// The catalog reads a column under its database's default collation as
    /// having none — an inherited default and the same name spelled out are
    /// identical there (DECISIONS 443) — so a declaration that names the
    /// target's default is compared as one that names nothing, or every
    /// connected plan would alter the column to what it already is. Only
    /// where the target's default is known: offline, the declaration stays as
    /// written and an emitted `COLLATE` says exactly what it means.
    pub fn without_collation(&self, default: &str) -> Schema {
        let default = Collation::new(default);
        let mut out = self.clone();
        for table in out.tables.values_mut() {
            for column in table.columns.values_mut() {
                if column.collation.as_ref() == Some(&default) {
                    column.collation = None;
                }
            }
        }
        out
    }

    /// Foreign keys whose two sides are declared under different collations
    /// (#1175). Measured on SQL Server 17.0, the engine refuses one (1757,
    /// "is not of same collation as referencing column"), so a plan with it
    /// fails at apply.
    ///
    /// `default_known` says whether this schema's absent collations have
    /// been resolved against a target ([`Schema::without_collation`]). Until
    /// they have, an absent collation facing a named one is not a problem: it
    /// is valid exactly when the named one is the target's default, which is
    /// not known offline — refusing it there refused a valid plan (#1247
    /// review). A connected plan asks again once the default is known.
    pub fn foreign_key_collation_problems(&self, default_known: bool) -> Vec<String> {
        let mut problems = Vec::new();
        for (name, table) in &self.tables {
            for (fk_name, fk) in &table.foreign_keys {
                let Some(parent) = self.tables.get(&fk.references_table) else {
                    continue;
                };
                for (local, referenced) in fk.columns.iter().zip(&fk.references_columns) {
                    let (Some(l), Some(r)) =
                        (table.columns.get(local), parent.columns.get(referenced))
                    else {
                        continue;
                    };
                    let undecided =
                        !default_known && (l.collation.is_none() != r.collation.is_none());
                    if l.collation != r.collation && !undecided {
                        let say = |c: &Option<Collation>| {
                            c.as_ref().map_or_else(
                                || "the database default".to_owned(),
                                |c| format!("`{c}`"),
                            )
                        };
                        problems.push(format!(
                            "{name}: foreign key `{fk_name}` joins `{local}` ({}) to {}.`{referenced}` \
                             ({}); both sides of a foreign key must declare the same collation",
                            say(&l.collation),
                            fk.references_table,
                            say(&r.collation)
                        ));
                    }
                }
            }
        }
        problems
    }
}

impl Table {
    /// Reused declared names across this table's constraint kinds.
    ///
    /// Separate maps cannot enforce this shared rule. Indexes are excluded:
    /// CHECK/FK names may match an ordinary index, while key-backed index
    /// collisions and any wider namespace belong to the dialect (decision 459).
    /// An unnamed primary key has no declared name to compare.
    pub fn constraint_name_conflicts(&self) -> Vec<String> {
        let primary = self.primary_key.as_ref().and_then(|pk| pk.name.as_deref());
        let names = primary
            .into_iter()
            .map(|name| (name, "primary key"))
            .chain(
                self.unique
                    .keys()
                    .map(|name| (name.as_str(), "unique constraint")),
            )
            .chain(
                self.foreign_keys
                    .keys()
                    .map(|name| (name.as_str(), "foreign key")),
            )
            .chain(
                self.checks
                    .keys()
                    .map(|name| (name.as_str(), "check constraint")),
            );
        let mut seen = BTreeMap::new();
        let mut problems = Vec::new();
        for (name, kind) in names {
            match seen.entry(name) {
                std::collections::btree_map::Entry::Vacant(entry) => { entry.insert(kind); }
                std::collections::btree_map::Entry::Occupied(entry) => problems.push(format!(
                    "{} and {kind} are both named `{name}`; constraint names must be distinct within a table",
                    entry.get()
                )),
            }
        }
        problems
    }

    /// Whether the primary key's index is the clustered one: the default
    /// layout, where the table has a key at all.
    pub fn primary_key_is_clustered(&self) -> bool {
        self.primary_key.is_some() && self.clustered.is_none()
    }

    /// Whether the UNIQUE constraint `name`'s index is the clustered one.
    pub fn unique_is_clustered(&self, name: &str) -> bool {
        matches!(&self.clustered, Some(Clustered::Unique(n)) if n == name)
    }

    /// Whether the index `name` is the clustered one.
    pub fn index_is_clustered(&self, name: &str) -> bool {
        matches!(&self.clustered, Some(Clustered::Index(n)) if n == name)
    }

    /// Why this table's [`Table::storage_parameters`] cannot be what they
    /// say: a name outside the closed list, or a value not in its canonical
    /// spelling, which a loaded declaration always is and a hand-edited
    /// state or plan may not be (#1441).
    pub fn storage_parameter_problems(&self) -> Vec<String> {
        self.storage_parameters
            .iter()
            .filter_map(
                |(name, value)| match crate::storage::canonical(name, value) {
                    Ok(canonical) if canonical == *value => None,
                    Ok(canonical) => Some(format!(
                        "storage parameter `{name}` is `{value}`, which is spelled `{canonical}`"
                    )),
                    Err(why) => Some(why),
                },
            )
            .collect()
    }

    /// Why this table's [`Table::replica_identity`] cannot be what it says:
    /// PostgreSQL takes as the identity only the index of a key, a unique
    /// constraint, or a unique index that is not partial, over columns that
    /// are all NOT NULL (measured on 16 and 18, #1444).
    pub fn replica_identity_problems(&self) -> Vec<String> {
        let nullable = |columns: &[String]| -> Vec<String> {
            columns
                .iter()
                .filter(|c| self.columns.get(*c).is_some_and(|col| col.nullable))
                .cloned()
                .collect()
        };
        let over = |what: String, columns: &[String]| -> Vec<String> {
            let open = nullable(columns);
            if open.is_empty() {
                Vec::new()
            } else {
                vec![format!(
                    "`replica_identity` names {what}, over nullable column(s) `{}`, which \
                     PostgreSQL does not take as a replica identity; declare them NOT NULL",
                    open.join("`, `")
                )]
            }
        };
        match &self.replica_identity {
            None | Some(ReplicaIdentity::Full | ReplicaIdentity::Nothing) => Vec::new(),
            Some(ReplicaIdentity::PrimaryKey) if self.primary_key.is_none() => vec![
                "`replica_identity: primary_key` on a table with no primary key names nothing"
                    .to_owned(),
            ],
            Some(ReplicaIdentity::PrimaryKey) => Vec::new(),
            Some(ReplicaIdentity::Unique(n)) => match self.unique.get(n) {
                None => vec![format!(
                    "`replica_identity` names unique constraint `{n}`, which this table does not \
                     declare"
                )],
                Some(u) => over(format!("unique constraint `{n}`"), &u.columns),
            },
            Some(ReplicaIdentity::Index(n)) => match self.indexes.get(n) {
                None => vec![format!(
                    "`replica_identity` names index `{n}`, which this table does not declare"
                )],
                Some(ix) if !ix.unique || ix.filter.is_some() => vec![format!(
                    "`replica_identity` names index `{n}`, which is not unique or is partial; \
                     PostgreSQL takes only a unique, non-partial index"
                )],
                Some(ix) => match ix.column_keys() {
                    None => vec![format!(
                        "`replica_identity` names index `{n}`, which holds an expression; \
                         PostgreSQL takes only an index over plain columns"
                    )],
                    Some(columns) => over(format!("index `{n}`"), &columns),
                },
            },
        }
    }

    /// Why this table's [`Table::clustered`] cannot be what it says.
    ///
    /// Structural, so every dialect asks it, as it asks
    /// [`Table::constraint_name_conflicts`]. A dialect with no clustered
    /// index at all refuses the field outright on top of this.
    pub fn clustered_problems(&self) -> Vec<String> {
        match &self.clustered {
            None => Vec::new(),
            Some(Clustered::Heap) if self.primary_key.is_none() => vec![
                "`clustered: heap` on a table with no primary key says nothing: without a \
                 key the table is a heap already; remove the line"
                    .to_owned(),
            ],
            Some(Clustered::Heap) => Vec::new(),
            Some(Clustered::Unique(n)) if !self.unique.contains_key(n) => vec![format!(
                "`clustered` names unique constraint `{n}`, which this table does not declare"
            )],
            Some(Clustered::Index(n)) if !self.indexes.contains_key(n) => vec![format!(
                "`clustered` names index `{n}`, which this table does not declare"
            )],
            Some(Clustered::Unique(_) | Clustered::Index(_)) => Vec::new(),
        }
    }

    /// The single column a declared row is keyed by, where this table has one.
    ///
    /// `None` is not "no key": it is a primary key that is absent or composite,
    /// which a `data:` block cannot be written against at all — the differ
    /// refuses it (`DataWithoutKey`) rather than emitting anything.
    pub fn data_key_column(&self) -> Option<&str> {
        match &self.primary_key {
            Some(pk) if pk.columns.len() == 1 => Some(pk.columns[0].as_str()),
            _ => None,
        }
    }

    /// The columns a declared row can hold a value in: every one but the
    /// column its key lives in and the engine's own `IDENTITY`s.
    ///
    /// The key is the map key of the row, not a cell in it, and a non-key
    /// `IDENTITY` is the engine's — never written by a row and never read back
    /// (DECISIONS 94). So neither is a cell that can differ, and a table with
    /// none of these can never produce an `UPDATE`: the differ builds one only
    /// from columns that pass this filter and emits it only if the result is
    /// non-empty.
    ///
    /// One spelling, because it is asked from two sides. The differ asks which
    /// cells to compare; `doctor` asks whether an `UPDATE` is possible at all,
    /// and demanding the permission for a table that can never emit one would
    /// report a gap against an account that can run every statement this
    /// declaration can produce.
    pub fn row_columns<'a>(
        &'a self,
        key_column: &'a str,
    ) -> impl Iterator<Item = (&'a String, &'a Column)> {
        self.columns
            .iter()
            .filter(move |(c, spec)| c.as_str() != key_column && !spec.engine_assigned())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Table {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// An `IndexMap` preserves declaration order, which decides the column
    /// layout of `CREATE TABLE`. Equality ignores order, so reordering is never
    /// mistaken for a schema change — and neither is it by
    /// [`state_checksum`](crate::state_checksum), which sorts these keys before
    /// hashing them for that reason (DECISIONS 238).
    pub columns: IndexMap<String, Column>,

    /// SQL Server computed columns (#1174): a name, an expression, and
    /// whether the value is stored. Not [`Column`]s. The engine infers their
    /// type and nullability, so there is none to declare or compare, and no
    /// reference row can write one, so they are not among
    /// [`row_columns`](Table::row_columns). A separate map rather than a
    /// kind of column, so that neither can hold what only the other has.
    ///
    /// Ordered by name: a computed column is created at the end of its table
    /// (an expression change re-adds it there, measured on 17.0), so no
    /// declared order could be kept.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub computed: BTreeMap<String, ComputedColumn>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_key: Option<PrimaryKey>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unique: BTreeMap<String, UniqueConstraint>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub foreign_keys: BTreeMap<String, ForeignKey>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub checks: BTreeMap<String, CheckConstraint>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub indexes: BTreeMap<String, Index>,

    /// Which index holds the table's rows, where that is not the default
    /// (SQL Server's clustered index, #1178).
    ///
    /// `None` is the default layout, and it is a reading rather than a gap:
    /// the primary key is clustered, and a table without one is a heap. That
    /// is what the engine does with a key that spells neither keyword on a
    /// table with no clustered index, so it is what every table pbps created
    /// before this field existed has — which is why a snapshot or plan
    /// without the field reads as `None` and is not refused.
    ///
    /// One selector for the whole table, not a flag on each key and index:
    /// a table has at most one clustered index, and a selector cannot name
    /// two, where two flags could both say yes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clustered: Option<Clustered>,

    /// What a PostgreSQL table's logical-replication change records carry
    /// to identify an old row, where that is not the default (#1444).
    ///
    /// `None` is `REPLICA IDENTITY DEFAULT`: the primary key, or nothing
    /// without one. A reading, not a gap: until this field existed a table
    /// with any other identity was left out of the pull as a limitation, so
    /// every table pbps recorded had the default, and a state or plan
    /// without the field reads as `None` truthfully.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replica_identity: Option<ReplicaIdentity>,

    /// A PostgreSQL table's heap storage parameters (#1441), by name, each
    /// in its canonical spelling ([`crate::storage::canonical`]): the
    /// engine keeps whatever spelling it was given, and several read as one
    /// value. Empty is every parameter at its default, which is what an
    /// older reader recorded for every table: it did not read them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub storage_parameters: BTreeMap<String, String>,

    /// Declared reference data (ADR-0004).
    ///
    /// `None` — the overwhelmingly common case — is the opt-in switch being
    /// off: pbps touches no row of a table that does not declare one. It lives
    /// inside [`Table`] rather than beside the model because rows are desired
    /// state that the database shows, so `==` must see them.
    ///
    /// Defaulted on read: every snapshot and plan written before `data:`
    /// existed describes a table that declares no rows, not a broken file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<TableData>,
}

/// A table layout other than the default (see [`Table::clustered`]).
///
/// Names the object by its declared name within the same table. A rename of
/// that object is a drop and an add under the new name (constraints and
/// indexes are never renamed in place), so the selector follows it by being
/// written with the new name; nothing else holds the name a second time.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Clustered {
    /// No clustered index, although the table has a primary key: the key is
    /// nonclustered and the rows are a heap. Without a key a heap is already
    /// the default, and this is refused as saying nothing.
    Heap,
    /// This UNIQUE constraint's index is the clustered one; the primary key,
    /// if any, is nonclustered.
    Unique(String),
    /// This index is the clustered one; the primary key, if any, is
    /// nonclustered.
    Index(String),
}

/// What a PostgreSQL table's logical-replication records carry to identify
/// an old row, where that is not the default (#1444). Names an index's
/// owner by kind and declared name, as [`Clustered`] does: constraints and
/// indexes are never renamed in place, so the selector follows a rename by
/// being written with the new name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicaIdentity {
    /// `REPLICA IDENTITY FULL`: every column of the old row.
    Full,
    /// `REPLICA IDENTITY NOTHING`: no old row, so a published table takes
    /// no replicated `UPDATE` or `DELETE`.
    Nothing,
    /// `USING INDEX` the primary key's index. It carries what the default
    /// does while the key stands, but the catalog holds it apart, so the
    /// model does too.
    PrimaryKey,
    /// `USING INDEX` this UNIQUE constraint's index.
    Unique(String),
    /// `USING INDEX` this unique index.
    Index(String),
}

/// As the declaration spells it.
impl std::fmt::Display for ReplicaIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("full"),
            Self::Nothing => f.write_str("nothing"),
            Self::PrimaryKey => f.write_str("primary_key"),
            Self::Unique(n) => write!(f, "{{unique: {n}}}"),
            Self::Index(n) => write!(f, "{{index: {n}}}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    #[serde(rename = "type")]
    pub ty: ColumnType,

    /// Defaults to `true`, matching SQL's own default.
    pub nullable: bool,

    /// The default-value expression, kept verbatim (`0`, `SYSUTCDATETIME()`).
    ///
    /// It is not parsed into a structure: expression syntax is dialect knowledge,
    /// and all we ever need from it is to hand it to the database untouched and
    /// to compare whether it changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<Identity>,

    /// A generation expression (DEC-1168.1): the column's value is computed
    /// from the row's other columns on every write, where a default is
    /// computed once, when a row that omits it is inserted. Never both: the
    /// engine refuses a default beside a generation expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated: Option<Generated>,

    /// An explicit collation (#1175). `None` is the database's default
    /// collation — whichever database the column is created in — which is
    /// what a column declared without one gets, and what the reader reports
    /// for a column whose collation is the connected database's own default:
    /// the catalog cannot tell an inherited default from the same name spelled
    /// out (DECISIONS 443).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collation: Option<Collation>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Present means deprecated; the value is the reason.
    ///
    /// No date is stored — git already records it, so asking the user to write
    /// one by hand is redundant and guaranteed to drift from the real time
    /// (SPEC §4.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<String>,
}

impl Column {
    /// The most common shape: nullable, no default.
    pub fn new(ty: ColumnType) -> Self {
        Self {
            ty,
            nullable: true,
            default: None,
            identity: None,
            generated: None,
            collation: None,
            description: None,
            deprecated: None,
        }
    }

    /// Whether the engine assigns this column's value and a row may not: an
    /// IDENTITY outside the key, or a generated column, which is computed on
    /// every write (DEC-1168.1).
    pub fn engine_assigned(&self) -> bool {
        self.identity.is_some() || self.generated.is_some()
    }

    pub fn not_null(mut self) -> Self {
        self.nullable = false;
        self
    }

    pub fn is_deprecated(&self) -> bool {
        self.deprecated.is_some()
    }

    /// Whether the engine has a declared source for existing rows when this
    /// is added as a required column.
    ///
    /// Expressions stay opaque everywhere else, but a default that explicitly
    /// invokes a NULL-producing construct is not a trustworthy value source.
    /// This is deliberately a conservative lexical check rather than an
    /// attempt to evaluate or normalize SQL.
    ///
    /// Boundaries an identifier by [`crate::module::is_regular_identifier_continue`]
    /// — SQL Server's own rule. That is right for SQL Server's own caller.
    /// The compatibility risk classifier (`Change::intrinsic_risks`) keeps
    /// this boundary too. Planning and saved-plan validation instead pass
    /// the selected dialect's boundary. A caller that has a dialect uses
    /// [`Self::has_required_add_value_source_with`] instead, the same split
    /// ADR-0011 Amendment 2 made for `normalize_definition` (DECISIONS 226,
    /// 433).
    pub fn has_required_add_value_source(&self) -> bool {
        self.has_required_add_value_source_with(crate::module::is_regular_identifier_continue)
    }

    /// [`Self::has_required_add_value_source`], with an identifier's end
    /// decided by `continues_ident` instead of assumed to be SQL Server's.
    ///
    /// PostgreSQL's rule is over bytes, so every non-ASCII byte continues a
    /// name there and the shared default's `char::is_alphanumeric` does not
    /// agree with it on all of them — **measured**, a combining mark
    /// (`\u{301}`) is not alphanumeric, so `null\u{301}x` — one PostgreSQL
    /// identifier, accepted unquoted — splits at the mark under the shared
    /// rule into the bare word `null`, and a function call is misread as an
    /// explicit `NULL`.
    pub fn has_required_add_value_source_with(&self, continues_ident: fn(char) -> bool) -> bool {
        self.identity.is_some()
            || self
                .default
                .as_deref()
                .is_some_and(|default| !has_explicit_null_semantics(default, continues_ident))
    }
}

fn has_explicit_null_semantics(expression: &str, continues_ident: fn(char) -> bool) -> bool {
    // String literals and comments are not SQL expressions. Blanking them
    // keeps a harmless default such as 'NULL' from being mistaken for the NULL
    // keyword while still finding it inside CAST(NULL AS int), arithmetic, and
    // other expression shapes.
    crate::module::code_without_quoted_identifiers(expression)
        // Where a word ends is the caller's rule, not always SQL Server's
        // (`is_regular_identifier_continue`) — treating `$`, `@`, or `#` as
        // punctuation there would turn `seq$null` into a false NULL token,
        // and treating a byte PostgreSQL reads as a name as a boundary
        // instead turns a name into one.
        .split(|ch: char| !continues_ident(ch))
        .any(|word| {
            word.eq_ignore_ascii_case("null")
                || word.eq_ignore_ascii_case("nullif")
                || word.eq_ignore_ascii_case("try_cast")
                || word.eq_ignore_ascii_case("try_convert")
                || word.eq_ignore_ascii_case("try_parse")
        })
}

/// A collation's name, compared the way the engine compares it (#1175).
///
/// SQL Server takes a collation name in any case — `latin1_general_ci_as`
/// names `Latin1_General_CI_AS`, measured on 17.0 — and reads it back in its
/// own spelling. Kept as written, and compared without ASCII case, so a
/// declaration spelled one way and a read-back spelled the other are the one
/// schema they are (constraint 1) instead of a change planned forever.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Collation(String);

impl Collation {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn key(&self) -> impl Iterator<Item = u8> + '_ {
        self.0.bytes().map(|b| b.to_ascii_lowercase())
    }
}

impl PartialEq for Collation {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq_ignore_ascii_case(&other.0)
    }
}

impl Eq for Collation {}

impl PartialOrd for Collation {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Collation {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key().cmp(other.key())
    }
}

impl std::hash::Hash for Collation {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        for b in self.key() {
            state.write_u8(b);
        }
    }
}

impl std::fmt::Display for Collation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub seed: i64,
    pub increment: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrimaryKey {
    /// The constraint name. `None` leaves the database to name it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UniqueConstraint {
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignKey {
    pub columns: Vec<String>,
    pub references_table: TableName,
    pub references_columns: Vec<String>,

    #[serde(default)]
    pub on_delete: ReferentialAction,
    #[serde(default)]
    pub on_update: ReferentialAction,
}

/// `JsonSchema` is derived here and nowhere else in the model.
///
/// This enum is not merely a domain value: it is a set of literal words a user
/// types into a declaration file, and the editor schema of SPEC §14.1 is
/// generated from the loader's own types precisely so the two cannot drift.
/// Mirroring the variants in `pbps-load` to avoid the derive would recreate the
/// drift the generation exists to prevent.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ReferentialAction {
    #[default]
    NoAction,
    Cascade,
    SetNull,
    SetDefault,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckConstraint {
    /// The check expression, kept verbatim.
    pub expression: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    pub columns: Vec<IndexColumn>,

    /// Payload columns that are not part of the key (SQL Server's INCLUDE).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,

    #[serde(default)]
    pub unique: bool,

    /// The filtered-index predicate, kept verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,

    /// The access method. Absent is the B-tree every engine builds by
    /// default, so an index declared before methods existed reads as the
    /// index it always was (DEC-1169.1).
    #[serde(default, skip_serializing_if = "IndexMethod::is_btree")]
    pub method: IndexMethod,
}

impl Index {
    /// The key's columns, where every key is a column; `None` where any key is
    /// an expression. What a question about "the columns an index is over"
    /// has to ask, so an expression is never read as a column (DEC-1169.2).
    /// Whether the index holds an expression the engine resolves names in:
    /// a filter, or any expression key. Such an index can call a module, so
    /// it is built after the modules it may call, and observed wherever a
    /// filtered index is (DEC-1169.2).
    pub fn holds_expression(&self) -> bool {
        self.filter.is_some() || self.columns.iter().any(|c| c.key.expression().is_some())
    }

    pub fn column_keys(&self) -> Option<Vec<String>> {
        self.columns
            .iter()
            .map(|c| c.key.column().map(str::to_owned))
            .collect()
    }
}

/// How an index is built. A closed list: a method this model does not name is
/// left out of a pull and reported, never stood in for by one it does.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum IndexMethod {
    /// The default B-tree.
    #[default]
    Btree,
    /// PostgreSQL's generalized inverted index, over `jsonb` in this model.
    Gin,
}

impl IndexMethod {
    pub fn is_btree(&self) -> bool {
        *self == IndexMethod::Btree
    }

    /// The spelling a declaration and PostgreSQL's `pg_am.amname` share.
    pub fn as_str(self) -> &'static str {
        match self {
            IndexMethod::Btree => "btree",
            IndexMethod::Gin => "gin",
        }
    }
}

/// How a generated column is computed (DEC-1168.1).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Generated {
    /// The generation expression, kept verbatim as a default's is.
    pub expression: String,
    /// Whether the value is computed on write and stored (`STORED`), rather
    /// than computed on read. A kind, not a flag to leave off: PostgreSQL 18
    /// reads a generation expression with no kind as `VIRTUAL`.
    pub stored: bool,
}

/// A SQL Server computed column (#1174, DEC-1174.1).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputedColumn {
    /// The expression, kept verbatim as a default's is.
    pub expression: String,
    /// Whether the value is computed on write and stored (`PERSISTED`),
    /// rather than on read. Off unless declared, as in the engine.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub persisted: bool,
    /// `PERSISTED NOT NULL`: a NULL the expression yields is refused. The
    /// engine infers the nullability of every other computed column, and
    /// reads back the same `is_nullable = 0` for an expression that is never
    /// NULL as for a declared `NOT NULL`. So a read-back holds this wherever
    /// a persisted column is not nullable, and only a declared `true` is held
    /// to the database (`ComputedColumn::declares`), never a declared absence.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub not_null: bool,
}

impl ComputedColumn {
    /// Whether a column read back as `read` is what this declaration asks
    /// for: the same expression and persistence, and NOT NULL wherever this
    /// declares it. A declaration without `not_null` matches either reading,
    /// since the engine may have inferred it (#1174).
    pub fn declares(&self, read: &ComputedColumn) -> bool {
        self.expression == read.expression
            && self.persisted == read.persisted
            && (!self.not_null || read.not_null)
    }
}

/// One key of an index, in the index's order.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "RawIndexColumn", into = "RawIndexColumn")]
pub struct IndexColumn {
    pub key: IndexKey,
    pub descending: bool,
    /// The operator class this key is indexed with, where it is not the
    /// method's default for the key's type; absent is that default. Which
    /// names are accepted is the dialect's question (DEC-1169.1).
    pub opclass: Option<String>,
}

/// What an index key orders by: a column of the table, or an expression over
/// its columns, kept verbatim as declared (DEC-1169.2).
///
/// An enum and not an optional expression beside the name, so a key cannot be
/// both or neither, and a reader that wants a column has to say what it does
/// with an expression rather than read one as a column named `lower(email)`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IndexKey {
    Column(String),
    Expression(String),
}

impl IndexKey {
    /// The column this key is, or `None` for an expression.
    pub fn column(&self) -> Option<&str> {
        match self {
            IndexKey::Column(name) => Some(name),
            IndexKey::Expression(_) => None,
        }
    }

    /// The expression this key is, or `None` for a column.
    pub fn expression(&self) -> Option<&str> {
        match self {
            IndexKey::Column(_) => None,
            IndexKey::Expression(text) => Some(text),
        }
    }

    /// The column's name or the expression's text, for display only: which
    /// of the two it is does not survive this.
    pub fn text(&self) -> &str {
        match self {
            IndexKey::Column(text) | IndexKey::Expression(text) => text,
        }
    }
}

impl IndexColumn {
    /// An ascending key on `name` under its default class: what most
    /// indexes are made of.
    pub fn column(name: impl Into<String>) -> Self {
        IndexColumn {
            key: IndexKey::Column(name.into()),
            descending: false,
            opclass: None,
        }
    }
}

/// How a key is written to a state or a plan: `name` for a column, as every
/// key was before expressions existed, so an older file reads unchanged and a
/// column key writes byte for byte what it always did; `expression` for an
/// expression. One and only one of the two.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIndexColumn {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expression: Option<String>,
    #[serde(default)]
    descending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    opclass: Option<String>,
}

impl TryFrom<RawIndexColumn> for IndexColumn {
    type Error = String;

    fn try_from(raw: RawIndexColumn) -> Result<Self, String> {
        let key = match (raw.name, raw.expression) {
            (Some(name), None) => IndexKey::Column(name),
            (None, Some(expression)) => IndexKey::Expression(expression),
            (Some(_), Some(_)) => {
                return Err("an index key names a column or an expression, not both".into());
            }
            (None, None) => {
                return Err("an index key names neither a column nor an expression".into());
            }
        };
        Ok(IndexColumn {
            key,
            descending: raw.descending,
            opclass: raw.opclass,
        })
    }
}

impl From<IndexColumn> for RawIndexColumn {
    fn from(c: IndexColumn) -> Self {
        let (name, expression) = match c.key {
            IndexKey::Column(name) => (Some(name), None),
            IndexKey::Expression(text) => (None, Some(text)),
        };
        RawIndexColumn {
            name,
            expression,
            descending: c.descending,
            opclass: c.opclass,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ty(s: &str) -> ColumnType {
        s.parse().unwrap()
    }

    fn sample() -> Table {
        let mut columns = IndexMap::new();
        columns.insert("customer_id".into(), Column::new(ty("bigint")).not_null());
        columns.insert("email".into(), Column::new(ty("nvarchar(255)")));
        Table {
            columns,
            primary_key: Some(PrimaryKey {
                name: Some("pk_customer".into()),
                columns: vec!["customer_id".into()],
            }),
            ..Default::default()
        }
    }

    /// A generated column is the engine's to fill, like a non-key identity:
    /// no row writes it (DEC-1168.1).
    #[test]
    fn a_generated_column_is_not_a_row_column() {
        let mut t = sample();
        let mut total = Column::new(ty("int"));
        total.generated = Some(Generated {
            expression: "customer_id * 2".into(),
            stored: true,
        });
        t.columns.insert("total".into(), total);
        let columns: Vec<&String> = t.row_columns("customer_id").map(|(c, _)| c).collect();
        assert!(
            !columns.iter().any(|c| c.as_str() == "total"),
            "{columns:?}"
        );
        // Negative: an ordinary column is one.
        assert!(columns.iter().any(|c| c.as_str() == "email"), "{columns:?}");
        assert!(t.columns["total"].engine_assigned());
        assert!(!t.columns["email"].engine_assigned());
    }

    #[test]
    fn nullable_defaults_to_true() {
        assert!(Column::new(ty("int")).nullable);
        assert!(!Column::new(ty("int")).not_null().nullable);
    }

    #[test]
    fn an_explicitly_nullable_default_is_not_a_required_add_value_source() {
        let mut column = Column::new(ty("int"));
        assert!(!column.has_required_add_value_source());

        for default in [
            "NULL",
            " null ",
            "(NULL)",
            "((( null )))",
            "CAST(NULL AS int)",
            "CONVERT(int, NULL)",
            "(NULL) + 1",
            "NULLIF(1, 1)",
            "TRY_CONVERT(int, 'not a number')",
            // A bare CR ends a line comment on the engine (a YAML `"\r"`
            // escape gets one there), so what follows it is the keyword.
            "-- carried over\rNULL",
            "-- carried over\r\nNULL",
            "-- carried over\nNULL",
        ] {
            column.default = Some(default.into());
            assert!(!column.has_required_add_value_source(), "{default:?}");
        }

        for default in [
            "0",
            "'NULL'",
            "SYSUTCDATETIME()",
            "NEWID()",
            "NEXT VALUE FOR dbo.[null]",
            "NEXT VALUE FOR dbo.\"try_cast\"",
            "NEXT VALUE FOR dbo.[seq]]null]",
            "NEXT VALUE FOR dbo.\"seq\"\"try_cast\"",
            "NEXT VALUE FOR dbo.seq$null",
            "NEXT VALUE FOR dbo.seq@null",
            "NEXT VALUE FOR dbo.seq#null",
            "NEXT VALUE FOR dbo.序列null",
            "1 /* outer /* nested */ NULL */",
            // These were measured not to end a comment; the engine sees a
            // default of `1` and a long comment, and so must the scan.
            "1 -- note\u{85}NULL",
            "1 -- note\u{2028}NULL",
            "1 -- note\u{0c}NULL",
            "1 -- note\u{0b}NULL",
        ] {
            column.default = Some(default.into());
            assert!(column.has_required_add_value_source(), "{default}");
        }
    }

    /// The keyword scan's word boundary is the caller's rule, not always SQL
    /// Server's. **Measured on PostgreSQL 18.6**, `null\u{301}x` — `null`
    /// followed by a combining acute accent, then `x` — is one identifier:
    /// `CREATE FUNCTION zz.null\u{301}x() ...` is accepted and names the
    /// routine `nulĺx`, callable unquoted. `\u{301}` is not alphanumeric, so
    /// the shared rule (`is_regular_identifier_continue`, `char::is_alphanumeric`)
    /// splits it from `null` and reads a bare `null` where the engine reads
    /// one name — misclassifying a plain function call as an explicit NULL
    /// and, under [`Change::intrinsic_risks`] or an mssql-shaped preflight
    /// probe, the column as having no trustworthy value source.
    #[test]
    fn a_combining_mark_is_read_as_a_name_byte_under_postgresqls_boundary_and_a_gap_under_the_shared_one()
     {
        // PostgreSQL's own rule (`pbps_dialect::continues_ident`, not
        // imported here: the model stays dialect-free by constraint) — every
        // non-ASCII byte continues a name, so the mark stays glued to `null`
        // and the whole thing reads as the one word `null\u{301}x`, which
        // does not match the keyword list.
        fn postgresql_boundary(ch: char) -> bool {
            ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || !ch.is_ascii()
        }

        let mut column = Column::new(ty("int"));
        column.default = Some("zz.null\u{301}x()".into());

        assert!(
            column.has_required_add_value_source_with(postgresql_boundary),
            "a plain call to a name PostgreSQL reads as one identifier"
        );

        // Negative: unparametrized, the answer is unchanged — SQL Server's
        // rule still splits at the mark and still reads the bare word
        // `null`, exactly as it did before `has_required_add_value_source_with`
        // existed.
        assert!(
            !column.has_required_add_value_source(),
            "the shared default keeps SQL Server's answer"
        );
    }

    /// The selector names an object of this table or says `heap` of a keyed
    /// one; anything else cannot be the table it claims (#1178).
    #[test]
    fn a_clustered_selector_must_name_something_this_table_declares() {
        let mut t = sample();
        t.unique.insert(
            "uq_email".into(),
            UniqueConstraint {
                columns: vec!["email".into()],
            },
        );
        for fine in [
            None,
            Some(Clustered::Heap),
            Some(Clustered::Unique("uq_email".into())),
        ] {
            t.clustered = fine.clone();
            assert!(t.clustered_problems().is_empty(), "{fine:?}");
        }
        // Negative: an index of that name does not exist, and the constraint
        // of that name is a constraint, not an index.
        for dangling in [
            Clustered::Index("uq_email".into()),
            Clustered::Unique("uq_missing".into()),
        ] {
            t.clustered = Some(dangling.clone());
            assert_eq!(t.clustered_problems().len(), 1, "{dangling:?}");
        }
        // Negative: without a key, `heap` says nothing.
        t.primary_key = None;
        t.clustered = Some(Clustered::Heap);
        assert!(t.clustered_problems()[0].contains("says nothing"));
        // The helpers answer from the one selector.
        t.primary_key = sample().primary_key;
        t.clustered = None;
        assert!(t.primary_key_is_clustered());
        t.clustered = Some(Clustered::Unique("uq_email".into()));
        assert!(!t.primary_key_is_clustered());
        assert!(t.unique_is_clustered("uq_email"));
        assert!(!t.index_is_clustered("uq_email"));
    }

    /// The identity names an object PostgreSQL can take: a key, a unique
    /// constraint, or a unique, non-partial index over plain columns, all
    /// NOT NULL. Anything else is refused by name (measured on 16 and 18,
    /// #1444).
    #[test]
    fn a_replica_identity_must_name_an_index_postgres_takes() {
        let mut t = sample();
        let index = |unique: bool, filter: Option<&str>, key: IndexKey| Index {
            columns: vec![IndexColumn {
                key,
                descending: false,
                opclass: None,
            }],
            include: Vec::new(),
            unique,
            filter: filter.map(Into::into),
            method: IndexMethod::default(),
        };
        let column = |c: &str| IndexKey::Column(c.into());
        t.unique.insert(
            "uq_id".into(),
            UniqueConstraint {
                columns: vec!["customer_id".into()],
            },
        );
        t.unique.insert(
            "uq_email".into(),
            UniqueConstraint {
                columns: vec!["email".into()],
            },
        );
        t.indexes
            .insert("ix_id".into(), index(true, None, column("customer_id")));
        t.indexes
            .insert("ix_plain".into(), index(false, None, column("customer_id")));
        t.indexes.insert(
            "ix_part".into(),
            index(true, Some("customer_id > 0"), column("customer_id")),
        );
        t.indexes.insert(
            "ix_expr".into(),
            index(true, None, IndexKey::Expression("customer_id + 1".into())),
        );
        t.indexes
            .insert("ix_email".into(), index(true, None, column("email")));
        for fine in [
            None,
            Some(ReplicaIdentity::Full),
            Some(ReplicaIdentity::Nothing),
            Some(ReplicaIdentity::PrimaryKey),
            Some(ReplicaIdentity::Unique("uq_id".into())),
            Some(ReplicaIdentity::Index("ix_id".into())),
        ] {
            t.replica_identity = fine.clone();
            assert!(t.replica_identity_problems().is_empty(), "{fine:?}");
        }
        for (wrong, says) in [
            (
                ReplicaIdentity::Unique("uq_missing".into()),
                "does not declare",
            ),
            (ReplicaIdentity::Index("uq_id".into()), "does not declare"),
            (
                ReplicaIdentity::Index("ix_plain".into()),
                "not unique or is partial",
            ),
            (
                ReplicaIdentity::Index("ix_part".into()),
                "not unique or is partial",
            ),
            (
                ReplicaIdentity::Index("ix_expr".into()),
                "holds an expression",
            ),
            (
                ReplicaIdentity::Index("ix_email".into()),
                "nullable column(s) `email`",
            ),
            (
                ReplicaIdentity::Unique("uq_email".into()),
                "nullable column(s) `email`",
            ),
        ] {
            t.replica_identity = Some(wrong.clone());
            let problems = t.replica_identity_problems();
            assert_eq!(problems.len(), 1, "{wrong:?}");
            assert!(problems[0].contains(says), "{wrong:?}: {problems:?}");
        }
        t.primary_key = None;
        t.replica_identity = Some(ReplicaIdentity::PrimaryKey);
        assert!(t.replica_identity_problems()[0].contains("names nothing"));
    }

    /// A storage parameter holds a listed name and its canonical spelling;
    /// a hand-edited state or plan with anything else is refused by name
    /// (#1441).
    #[test]
    fn a_storage_parameter_must_be_listed_and_canonical() {
        let mut t = sample();
        for (name, value) in [("fillfactor", "70"), ("autovacuum_enabled", "false")] {
            t.storage_parameters.insert(name.into(), value.into());
        }
        assert!(t.storage_parameter_problems().is_empty());
        t.storage_parameters
            .insert("fillfactor".into(), "070".into());
        assert!(t.storage_parameter_problems()[0].contains("spelled `56`"));
        t.storage_parameters
            .insert("fillfactor".into(), "70".into());
        t.storage_parameters.insert("bogus".into(), "1".into());
        assert!(t.storage_parameter_problems()[0].contains("`bogus`"));
    }

    /// A snapshot or plan from before the field reads as the default layout,
    /// and the default is written as nothing, so no existing file changes.
    #[test]
    fn the_default_layout_is_absent_from_json_and_read_back_from_its_absence() {
        let json = serde_json::to_string(&sample()).unwrap();
        assert!(!json.contains("clustered"), "{json}");
        let back: Table = serde_json::from_str(&json).unwrap();
        assert_eq!(back.clustered, None);
        let mut heap = sample();
        heap.clustered = Some(Clustered::Heap);
        let json = serde_json::to_string(&heap).unwrap();
        assert!(json.contains(r#""clustered":"heap""#), "{json}");
        assert_eq!(serde_json::from_str::<Table>(&json).unwrap(), heap);
    }

    /// A collation name compares the way the engine resolves one, without
    /// case, and a declaration naming the target's default compares as one
    /// naming none (#1175).
    #[test]
    fn collations_compare_without_case_and_the_default_drops_out() {
        assert_eq!(
            Collation::new("Latin1_General_CS_AS"),
            Collation::new("latin1_general_cs_as")
        );
        assert_ne!(
            Collation::new("Latin1_General_CS_AS"),
            Collation::new("Latin1_General_CI_AS")
        );
        let mut s = Schema::default();
        let mut t = sample();
        t.columns.get_mut("email").unwrap().collation =
            Some(Collation::new("latin1_general_ci_as"));
        t.columns.get_mut("customer_id").unwrap().collation =
            Some(Collation::new("Latin1_General_BIN2"));
        s.tables.insert(TableName::new("dbo", "customer"), t);
        let normal = s.without_collation("Latin1_General_CI_AS");
        let t = &normal.tables[&TableName::new("dbo", "customer")];
        assert_eq!(t.columns["email"].collation, None);
        // Negative: another collation stays.
        assert!(t.columns["customer_id"].collation.is_some());
    }

    /// A foreign key between two collations is refused (1757 on SQL Server),
    /// and so is a named collation facing an absent one (#1175).
    #[test]
    fn a_foreign_key_across_collations_is_a_problem() {
        let mut parent = sample();
        parent.columns.get_mut("email").unwrap().collation =
            Some(Collation::new("Latin1_General_CS_AS"));
        parent.unique.insert(
            "uq_email".into(),
            UniqueConstraint {
                columns: vec!["email".into()],
            },
        );
        let mut child = Table::default();
        child
            .columns
            .insert("email".into(), Column::new(ty("nvarchar(255)")));
        child.foreign_keys.insert(
            "fk_child_email".into(),
            ForeignKey {
                columns: vec!["email".into()],
                references_table: TableName::new("dbo", "customer"),
                references_columns: vec!["email".into()],
                on_delete: ReferentialAction::NoAction,
                on_update: ReferentialAction::NoAction,
            },
        );
        let mut s = Schema::default();
        s.tables.insert(TableName::new("dbo", "customer"), parent);
        s.tables.insert(TableName::new("dbo", "child"), child);
        let set_child = |s: &mut Schema, collation: &str| {
            s.tables
                .get_mut(&TableName::new("dbo", "child"))
                .unwrap()
                .columns
                .get_mut("email")
                .unwrap()
                .collation = Some(Collation::new(collation));
        };
        // Absent facing named: undecidable offline, so not refused there; a
        // problem once the target's default is known and is not the named
        // one (the schema then has the default taken out already).
        assert!(s.foreign_key_collation_problems(false).is_empty());
        let problems = s.foreign_key_collation_problems(true);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("fk_child_email"), "{problems:?}");
        // Named on the target's default: normalized away, and valid.
        assert!(
            s.without_collation("Latin1_General_CS_AS")
                .foreign_key_collation_problems(true)
                .is_empty()
        );
        // Two named collations that differ: refused even offline.
        set_child(&mut s, "Latin1_General_CI_AS");
        assert_eq!(s.foreign_key_collation_problems(false).len(), 1);
        set_child(&mut s, "latin1_general_cs_as");
        assert!(s.foreign_key_collation_problems(true).is_empty());
    }

    /// Column order affects CREATE TABLE output, so it has to be preserved.
    #[test]
    fn column_order_is_preserved() {
        let t = sample();
        assert_eq!(
            t.columns.keys().collect::<Vec<_>>(),
            ["customer_id", "email"]
        );
    }

    /// ...but a different order must not count as a schema change, or every
    /// reshuffle would produce a phantom diff.
    #[test]
    fn column_order_does_not_affect_equality() {
        let a = sample();
        let mut columns = IndexMap::new();
        columns.insert("email".into(), Column::new(ty("nvarchar(255)")));
        columns.insert("customer_id".into(), Column::new(ty("bigint")).not_null());
        let b = Table {
            columns,
            ..a.clone()
        };
        assert_eq!(a, b);
    }

    /// Serialization must be deterministic: state snapshots go into git and into
    /// the database, and shifting order would manufacture phantom diffs.
    #[test]
    fn serialisation_is_deterministic() {
        let mut schema = Schema::default();
        schema
            .tables
            .insert(TableName::new("dbo", "customer"), sample());
        schema
            .tables
            .insert(TableName::new("app", "region"), Table::default());

        let first = serde_json::to_string(&schema).unwrap();
        for _ in 0..20 {
            assert_eq!(serde_json::to_string(&schema).unwrap(), first);
        }
        // BTreeMap ordering: app.region comes before dbo.customer
        assert!(first.find("app.region").unwrap() < first.find("dbo.customer").unwrap());
    }

    #[test]
    fn schema_round_trips_through_json() {
        let mut schema = Schema::default();
        schema
            .tables
            .insert(TableName::new("dbo", "customer"), sample());
        let json = serde_json::to_string(&schema).unwrap();
        let back: Schema = serde_json::from_str(&json).unwrap();
        assert_eq!(schema, back);
    }

    /// Empty collections must not pollute the output, or identity files and
    /// snapshots would fill up with `{}`.
    #[test]
    fn empty_collections_are_omitted() {
        let json = serde_json::to_string(&Table::default()).unwrap();
        assert_eq!(json, r#"{"columns":{}}"#);
    }
}
