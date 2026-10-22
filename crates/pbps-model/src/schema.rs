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

use crate::ColumnName;
use std::collections::{BTreeMap, BTreeSet};

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
    pub roles: BTreeMap<crate::RoleName, crate::role::Role>,
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
                            fk.references_table.shown(),
                            say(&r.collation),
                            name = name.shown(),

                            local = local.shown(),

                            referenced = referenced.shown(),
                        ));
                    }
                }
            }
        }
        problems
    }
}

impl Table {
    /// Disabled policies and FORCE without ENABLE are state too (DEC-1172.1).
    pub fn has_row_security(&self) -> bool {
        !self.row_security.is_default() || !self.row_policies.is_empty()
    }

    /// The columns whose `pg_attrdef` row this declaration holds an
    /// expression for: a column's default or generation expression, and a
    /// partition's own default on its parent's column (#1578). One list, so
    /// that every inventory of binding surfaces sees the same rows; a
    /// partition declares no column, and an inventory reading `columns`
    /// alone read its own defaults as nothing there.
    pub fn expression_columns(&self) -> impl Iterator<Item = &ColumnName> {
        self.columns
            .iter()
            .filter(|(_, c)| match c.value_source {
                ValueSource::Default(_) | ValueSource::Generated(_) => true,
                // A `serial`'s `nextval` is the engine's text, not one this
                // declaration holds (#1631).
                ValueSource::None | ValueSource::Identity(_) | ValueSource::Sequence(_) => false,
            })
            .map(|(name, _)| name)
            .chain(
                self.partition_of
                    .iter()
                    .flat_map(|of| &of.columns)
                    .filter(|(_, own)| own.default.is_some())
                    .map(|(name, _)| name),
            )
    }

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
        let check = |what: &str,
                     parameters: &BTreeMap<String, String>,
                     canonical: &dyn Fn(&str, &str) -> Result<String, String>| {
            parameters
                .iter()
                .filter_map(|(name, value)| match canonical(name, value) {
                    Ok(spelled) if spelled == *value => None,
                    Ok(spelled) => Some(format!(
                        "{what}storage parameter `{name}` is `{value}`, which is spelled `{spelled}`"
                    )),
                    Err(why) => Some(format!("{what}{why}")),
                })
                .collect::<Vec<_>>()
        };
        // The index's own, by its method (#1442); a key's and a unique
        // constraint's index is a B-tree.
        let btree = |n: &str, v: &str| crate::storage::canonical_index(IndexMethod::Btree, n, v);
        let mut found = check("", &self.storage_parameters, &|n, v| {
            crate::storage::canonical(n, v)
        });
        if let Some(pk) = &self.primary_key {
            found.extend(check("primary key: ", &pk.storage_parameters, &btree));
        }
        for (name, unique) in &self.unique {
            found.extend(check(
                &format!("unique constraint `{name}`: "),
                &unique.storage_parameters,
                &btree,
            ));
        }
        for (name, index) in &self.indexes {
            let method = index.method;
            found.extend(check(
                &format!("index `{name}`: "),
                &index.storage_parameters,
                &|n, v| crate::storage::canonical_index(method, n, v),
            ));
        }
        found
    }

    /// Why this table's [`Table::replica_identity`] cannot be what it says:
    /// PostgreSQL takes as the identity only the index of a key, a unique
    /// constraint, or a unique index that is not partial, over columns that
    /// are all NOT NULL (measured on 16 and 18, #1444).
    pub fn replica_identity_problems(&self) -> Vec<String> {
        let nullable = |columns: &[ColumnName]| -> Vec<ColumnName> {
            columns
                .iter()
                .filter(|c| self.columns.get(*c).is_some_and(|col| col.nullable))
                .cloned()
                .collect()
        };
        let over = |what: String, columns: &[ColumnName]| -> Vec<String> {
            let open = nullable(columns);
            if open.is_empty() {
                Vec::new()
            } else {
                vec![format!(
                    "`replica_identity` names {what}, over nullable column(s) `{}`, which \
                     PostgreSQL does not take as a replica identity; declare them NOT NULL",
                    open.iter()
                        // raw: whole-message escaped by dialect validation until #2084 step 5.
                        .map(ColumnName::raw)
                        .collect::<Vec<_>>()
                        .join("`, `")
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
    pub fn data_key_column(&self) -> Option<&ColumnName> {
        match &self.primary_key {
            Some(pk) if pk.columns.len() == 1 => Some(&pk.columns[0]),
            _ => None,
        }
    }

    /// The columns a declared row can hold a value in: every one but the
    /// column its key lives in, the engine's own `IDENTITY`s and a period's
    /// columns.
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
    ) -> impl Iterator<Item = (&'a ColumnName, &'a Column)> {
        self.columns
            .iter()
            .filter(move |(c, spec)| {
                c.raw() != key_column
                    && !(match spec.value_source {
                        // A `serial` is the engine's to number, as an
                        // identity is (#1631).
                        crate::ValueSource::Identity(_)
                        | crate::ValueSource::Sequence(_)
                        | crate::ValueSource::Generated(_) => true,
                        crate::ValueSource::None | crate::ValueSource::Default(_) => false,
                    })
            })
            // The engine writes a period's columns on every change (#1176).
            .filter(move |(c, _)| {
                !self
                    .system_time
                    .as_ref()
                    .is_some_and(|p| p.is_period_column(c.raw()))
            })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Table {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<crate::Text>,

    /// An `IndexMap` preserves declaration order, which decides the column
    /// layout of `CREATE TABLE`. Equality ignores order, so reordering is never
    /// mistaken for a schema change — and neither is it by
    /// [`state_checksum`](crate::state_checksum), which sorts these keys before
    /// hashing them for that reason (DECISIONS 238).
    pub columns: IndexMap<ColumnName, Column>,

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
    pub computed: BTreeMap<ColumnName, ComputedColumn>,

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

    /// A PostgreSQL `UNLOGGED` table (#1443): its writes skip the
    /// write-ahead log, so a crash or an unclean shutdown empties it and a
    /// standby never has its rows. `false`, the default, is a permanent
    /// table, which every table an older reader recorded was: it left an
    /// unlogged one out as a limitation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unlogged: bool,

    /// PostgreSQL's independent ENABLE and FORCE flags (DEC-1172.1).
    /// Older readers excluded every table with either flag or a policy, so
    /// absent fields truthfully mean disabled security and no policies.
    #[serde(default, skip_serializing_if = "RowSecurity::is_default")]
    pub row_security: RowSecurity,

    /// Policies have their own table-local namespace and no data identity.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub row_policies: BTreeMap<String, RowPolicy>,

    /// SQL Server's `PERIOD FOR SYSTEM_TIME` and system versioning (#1176,
    /// DEC-1176.1). `None` is an ordinary table, which every table an older
    /// reader recorded was: it left a temporal one out as a limitation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_time: Option<SystemTime>,

    /// A PostgreSQL RANGE-partitioned parent's key, by column (#1170,
    /// DEC-1170.1). `None` is an ordinary table, which every table an older
    /// reader recorded was: it left a partitioned one out as a limitation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition_by: Option<PartitionBy>,

    /// This table is a partition of another, with its bound (#1170). Its
    /// columns and keys are its parent's, which the engine gives it, with
    /// clones of the parent's indexes, checks and foreign keys; what it holds
    /// of its own beside them is its checks, indexes (#1577), foreign keys
    /// (#1632), column defaults and NOT NULLs (#1578), persistence and storage
    /// parameters (#1580).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition_of: Option<PartitionOf>,

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

/// FORCE is meaningful state even while row security is disabled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RowSecurity {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub forced: bool,
}

impl RowSecurity {
    pub fn is_default(&self) -> bool {
        !self.enabled && !self.forced
    }
}

/// A policy's name belongs to its table's map, not its definition.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RowPolicy {
    #[serde(default)]
    pub command: PolicyCommand,
    #[serde(default)]
    pub restrictive: bool,
    #[serde(default)]
    pub to: PolicyRoles,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub using: Option<crate::Text>,
    /// None must never be manufactured from USING: the catalog distinguishes
    /// omission from an explicitly equal expression (DEC-1172.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub with_check: Option<crate::Text>,
}

impl RowPolicy {
    /// A freshly written predicate is held to presence: PostgreSQL deparses
    /// its text. Untouched policies still use exact equality (SPEC §7.6).
    pub fn agrees_when_written(&self, now: &Self) -> bool {
        self.command == now.command
            && self.restrictive == now.restrictive
            && self.to == now.to
            && self.using.is_some() == now.using.is_some()
            && self.with_check.is_some() == now.with_check.is_some()
    }

    /// Shared by YAML conversion and validation of programmatically built state.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.with_check.is_some()
            && matches!(self.command, PolicyCommand::Select | PolicyCommand::Delete)
        {
            problems.push("WITH CHECK cannot be applied to SELECT or DELETE".to_owned());
        }
        if self.using.is_some() && self.command == PolicyCommand::Insert {
            problems.push("only WITH CHECK expression allowed for INSERT".to_owned());
        }
        if let PolicyRoles::Roles(roles) = &self.to {
            if roles.is_empty() {
                problems
                    .push("`to` must name at least one role, or be the scalar `public`".to_owned());
            }
            for role in roles {
                match role.raw() {
                    // PostgreSQL otherwise discards the named roles beside
                    // PUBLIC. Uppercase "PUBLIC" is a distinct legal role.
                    "public" => problems.push(
                        "`public` cannot appear in a role list; use the scalar `to: public`"
                            .to_owned(),
                    ),
                    "current_user" | "session_user" | "current_role" => problems.push(format!(
                        "`{role}` is a session-dependent target; name a role explicitly",
                        role = role.shown()
                    )),
                    _ => {}
                }
            }
        }
        problems
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyCommand {
    #[default]
    All,
    Select,
    Insert,
    Update,
    Delete,
}

impl PolicyCommand {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Select => "select",
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

/// PUBLIC is a target, never a sentinel role name (ADR-0010 §5).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PolicyRoles {
    #[default]
    Public,
    Roles(BTreeSet<crate::RoleName>),
}

/// `PARTITION BY RANGE | LIST | HASH (…)` over plain columns (#1170,
/// #1632).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartitionBy {
    /// How rows are routed to partitions. `RANGE` is written as nothing, so
    /// every file, snapshot and plan from before #1632 reads as it was.
    #[serde(default, skip_serializing_if = "PartitionStrategy::is_range")]
    pub strategy: PartitionStrategy,
    /// The key's columns, in order.
    pub columns: Vec<ColumnName>,
    /// Its partitions are another tool's to create and drop: pg_partman, a
    /// scheduled job (#1533, DEC-1533.1). pbps manages the parent alone,
    /// declares no partition, and holds each it finds only as the parent
    /// makes it. Declared as `partitions: external`; the catalog cannot say
    /// it, so a live read leaves it `false` and the managed scope sets it
    /// from what is compared against.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub external: bool,
}

/// How a partitioned table routes rows (#1632, DEC-1632.1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartitionStrategy {
    /// By ranges of the key, with an optional DEFAULT partition.
    #[default]
    Range,
    /// By lists of the key's values, one key column, with an optional
    /// DEFAULT partition.
    List,
    /// By the key's hash, modulus and remainder; never a DEFAULT partition.
    Hash,
}

impl PartitionStrategy {
    pub fn is_range(&self) -> bool {
        *self == PartitionStrategy::Range
    }

    /// The keyword `PARTITION BY` and the catalog's key text use.
    pub fn keyword(self) -> &'static str {
        match self {
            PartitionStrategy::Range => "RANGE",
            PartitionStrategy::List => "LIST",
            PartitionStrategy::Hash => "HASH",
        }
    }
}

/// A partition's parent and the rows it takes (#1170).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartitionOf {
    pub parent: TableName,
    pub bound: PartitionBound,
    /// What the partition holds of its parent's columns otherwise than its
    /// parent does, by column: a default of its own, or a NOT NULL the
    /// parent's column does not have (#1578). Here and not in the table's
    /// `columns`, which a partition never has: its columns, types included,
    /// are its parent's, and only a partition can hold one of these.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub columns: BTreeMap<ColumnName, PartitionColumn>,
}

/// One column's own default and NOT NULL on a partition (#1578). Each is an
/// override of what the parent's column has, so neither repeats the parent's:
/// `None` and `false` are the parent's.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartitionColumn {
    /// The partition's own default, where it differs from the parent's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// NOT NULL on the partition where the parent's column is nullable.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub not_null: bool,
}

/// What a partition takes: every row no other partition does, a range from
/// one key value (inclusive) to another (exclusive), one datum per key
/// column, a list of the key's values, or the rows whose key hashes to a
/// remainder of a modulus (#1632).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartitionBound {
    Default,
    Range {
        from: Vec<BoundDatum>,
        to: Vec<BoundDatum>,
    },
    /// `FOR VALUES IN (…)`. A set: the engine keeps the order written and
    /// drops a repeated value (measured on 16 and 18), and neither changes
    /// which rows the partition takes, so two lists of the same values are
    /// one bound however they are written (#1632 review).
    List {
        values: std::collections::BTreeSet<ListDatum>,
    },
    /// `FOR VALUES WITH (MODULUS m, REMAINDER r)`.
    Hash {
        modulus: u32,
        remainder: u32,
    },
}

impl PartitionBound {
    /// The strategy a bound of this kind belongs to; `None` for DEFAULT,
    /// which a RANGE or LIST parent may have.
    pub fn strategy(&self) -> Option<PartitionStrategy> {
        match self {
            PartitionBound::Default => None,
            PartitionBound::Range { .. } => Some(PartitionStrategy::Range),
            PartitionBound::List { .. } => Some(PartitionStrategy::List),
            PartitionBound::Hash { .. } => Some(PartitionStrategy::Hash),
        }
    }
}

/// One value of a LIST partition: the engine's own text for it, unquoted,
/// or `NULL`, which a LIST partition may take and a range may not.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListDatum {
    Null,
    Value(String),
}

/// As a declaration writes one: `NULL`, or the value.
impl std::fmt::Display for ListDatum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ListDatum::Null => f.write_str("NULL"),
            ListDatum::Value(v) => f.write_str(v),
        }
    }
}

/// One key column's end of a range: the engine's own text for a value,
/// unquoted, or one of the two unbounded ends.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundDatum {
    MinValue,
    MaxValue,
    Value(String),
}

/// As a declaration writes one: `MINVALUE`, `MAXVALUE`, or the value.
impl std::fmt::Display for BoundDatum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BoundDatum::MinValue => f.write_str("MINVALUE"),
            BoundDatum::MaxValue => f.write_str("MAXVALUE"),
            BoundDatum::Value(v) => f.write_str(v),
        }
    }
}

impl BoundDatum {
    /// A declared datum: `MINVALUE` and `MAXVALUE` in any case are the
    /// unbounded ends, anything else a value as written.
    pub fn declared(text: &str) -> Self {
        if text.eq_ignore_ascii_case("minvalue") {
            BoundDatum::MinValue
        } else if text.eq_ignore_ascii_case("maxvalue") {
            BoundDatum::MaxValue
        } else {
            BoundDatum::Value(text.to_owned())
        }
    }
}

/// A SQL Server table's `PERIOD FOR SYSTEM_TIME`, and its system versioning
/// where that is on (#1176, DEC-1176.1).
///
/// The period's two columns stay in [`Table::columns`], which holds their
/// type, nullability and place; this names them. The engine writes both on
/// every change to a row, so neither is a column a declared row can write.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemTime {
    /// `GENERATED ALWAYS AS ROW START`.
    pub start: ColumnName,
    /// `GENERATED ALWAYS AS ROW END`.
    pub end: ColumnName,
    /// Both period columns `HIDDEN`: left out of `SELECT *`. One flag for the
    /// pair; a table with one hidden and one visible is read as a limitation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// `None` is the period alone: a table with versioning off, which is also
    /// what `SET (SYSTEM_VERSIONING = OFF)` leaves behind (measured on 17.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub versioning: Option<SystemVersioning>,
}

impl SystemTime {
    /// Whether `column` is one of the period's two columns.
    pub fn is_period_column(&self, column: &str) -> bool {
        self.start.raw() == column || self.end.raw() == column
    }
}

/// `SYSTEM_VERSIONING = ON`: the engine keeps every replaced row in a history
/// table. The history is part of its table, with no identity of its own: its
/// columns follow the table's, and the engine builds its layout (DEC-1176.1).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemVersioning {
    /// Always a name, even where the engine chose one: a table recreated
    /// without it gets a history of another name (DEC-1176.1).
    pub history: TableName,
    /// `HISTORY_RETENTION_PERIOD`; `None` is `INFINITE`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<Retention>,
}

/// A finite `HISTORY_RETENTION_PERIOD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retention {
    /// Positive: the engine refuses zero.
    pub count: u32,
    pub unit: RetentionUnit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionUnit {
    Day,
    Week,
    Month,
    Year,
}

impl RetentionUnit {
    /// The engine's keyword, singular.
    pub fn keyword(self) -> &'static str {
        match self {
            RetentionUnit::Day => "DAY",
            RetentionUnit::Week => "WEEK",
            RetentionUnit::Month => "MONTH",
            RetentionUnit::Year => "YEAR",
        }
    }
}

/// As a declaration writes it: `6 months`, `1 day`.
impl std::fmt::Display for Retention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let unit = self.unit.keyword().to_lowercase();
        if self.count == 1 {
            write!(f, "1 {unit}")
        } else {
            write!(f, "{} {unit}s", self.count)
        }
    }
}

impl std::str::FromStr for Retention {
    type Err = String;

    /// `<count> <unit>`, the unit singular or plural, in any case.
    fn from_str(text: &str) -> Result<Self, String> {
        let wrong = || {
            format!(
                "`{text}` is not a retention period: write a positive count and a unit, such as \
                 `6 months` or `1 year`, or leave `retention` out for INFINITE"
            )
        };
        let mut words = text.split_whitespace();
        let (Some(count), Some(unit), None) = (words.next(), words.next(), words.next()) else {
            return Err(wrong());
        };
        let count: u32 = count.parse().map_err(|_| wrong())?;
        if count == 0 {
            return Err(wrong());
        }
        // Singular or plural, and nothing else: one `s` at most.
        let lower = unit.to_ascii_lowercase();
        let unit = match lower.strip_suffix('s').unwrap_or(&lower) {
            "day" => RetentionUnit::Day,
            "week" => RetentionUnit::Week,
            "month" => RetentionUnit::Month,
            "year" => RetentionUnit::Year,
            _ => return Err(wrong()),
        };
        Ok(Retention { count, unit })
    }
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

/// Exactly one source supplies a column's value. Consumers match the complete
/// enum so a new source cannot silently inherit an ordinary column's policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueSource {
    None,
    /// Verbatim SQL, evaluated when an insert omits the column.
    Default(String),
    Identity(Identity),
    /// A sequence the column owns and defaults from: the shape PostgreSQL's
    /// `serial` creates (#1631, DEC-1631.1). Its own source, never an
    /// identity: a role that may insert into the table may not use the
    /// sequence, and a column with a default cannot take an identity in
    /// place. Declared as `type: serial` and its kin.
    Sequence(OwnedSequence),
    /// Verbatim SQL, evaluated from the row on every write.
    Generated(Generated),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub ty: ColumnType,

    /// Defaults to `true`, matching SQL's own default.
    pub nullable: bool,

    pub value_source: ValueSource,

    /// An explicit collation (#1175). `None` is the database's default
    /// collation — whichever database the column is created in — which is
    /// what a column declared without one gets, and what the reader reports
    /// for a column whose collation is the connected database's own default:
    /// the catalog cannot tell an inherited default from the same name spelled
    /// out (DECISIONS 443).
    pub collation: Option<Collation>,

    pub description: Option<crate::Text>,

    /// Present means deprecated; the value is the reason.
    ///
    /// No date is stored — git already records it, so asking the user to write
    /// one by hand is redundant and guaranteed to drift from the real time
    /// (SPEC §4.2).
    pub deprecated: Option<crate::Text>,
}

impl Column {
    /// The most common shape: nullable, no default.
    pub fn new(ty: ColumnType) -> Self {
        Self {
            ty,
            nullable: true,
            value_source: ValueSource::None,
            collation: None,
            description: None,
            deprecated: None,
        }
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
        match &self.value_source {
            ValueSource::Identity(_) | ValueSource::Sequence(_) => true,
            ValueSource::Default(default) => !has_explicit_null_semantics(default, continues_ident),
            // A generated expression can produce NULL; evaluating it here
            // would execute the operator's SQL before approval.
            ValueSource::None | ValueSource::Generated(_) => false,
        }
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
    /// Its sequence's bounds, cycle and cache (#1921, DEC-1921.1). Default
    /// is the engine's, and written as nothing.
    #[serde(default, skip_serializing_if = "SequenceOptions::is_default")]
    pub options: SequenceOptions,
}

impl Identity {
    /// An identity with the engine's bounds, no cycle and a cache of one.
    pub fn new(seed: i64, increment: i64) -> Self {
        Self {
            seed,
            increment,
            options: SequenceOptions::default(),
        }
    }
}

/// What a PostgreSQL sequence holds beyond its start and increment (#1921,
/// DEC-1921.1): its bounds, whether it cycles, and how many numbers a session
/// takes at once.
///
/// A bound is `None` where it is the engine's default for the column's type
/// and the direction counted: `1 ..= type max` counting up, `type min ..= -1`
/// counting down. Measured on 16 and 18, a retype moves a default bound with
/// the type and keeps an explicit one, so `None` is not the same as the
/// number it stands for today. A bound declared equal to its default is
/// refused by validation, so each sequence has one spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SequenceOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cycle: bool,
    /// `None` is the engine's cache of one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<i64>,
}

impl SequenceOptions {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Whether `self`, replacing `was`, can make a writer fail sooner
    /// (DEC-1921.1): the bound it counts toward set or tightened, or a
    /// cycle turned off. Needs no type: an explicit bound is always inside
    /// the default one it replaces, so `None` is the loosest.
    pub fn ends_sooner_than(&self, was: &SequenceOptions, increment: i64) -> bool {
        let tighter = |now: Option<i64>, then: Option<i64>, below: bool| match (now, then) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(now), Some(then)) => {
                if below {
                    now < then
                } else {
                    now > then
                }
            }
        };
        let bound = if increment < 0 {
            tighter(self.min, was.min, false)
        } else {
            tighter(self.max, was.max, true)
        };
        bound || (was.cycle && !self.cycle)
    }
}

/// A sequence a column owns and defaults from (#1631, DEC-1631.1): created
/// with the column, as `AS` the column's type, and dropped with it.
///
/// Its start and increment are held, as an identity's are. Its name is held
/// only where it is not the engine's generated `<table>_<column>_seq`, and is
/// then the sequence's bare name in the table's schema. `None` is whatever
/// name the engine gave it, and is not compared: a column or table rename
/// keeps the sequence's name, so the generated one stops matching the
/// declaration the first time either is renamed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedSequence {
    pub start: i64,
    pub increment: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Its bounds, cycle and cache (#1921, DEC-1921.1). Default is the
    /// engine's, and written as nothing.
    #[serde(default, skip_serializing_if = "SequenceOptions::is_default")]
    pub options: SequenceOptions,
}

impl OwnedSequence {
    /// What `serial` creates: a sequence that starts at one and counts up.
    pub fn serial() -> Self {
        Self {
            start: 1,
            increment: 1,
            name: None,
            options: SequenceOptions::default(),
        }
    }

    /// Whether a sequence read back as `read` is what this declares: the same
    /// start, increment and options, and the same name where this one names
    /// it.
    pub fn declares(&self, read: &OwnedSequence) -> bool {
        self.start == read.start
            && self.increment == read.increment
            && self.options == read.options
            && self
                .name
                .as_ref()
                .is_none_or(|n| read.name.as_ref() == Some(n))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrimaryKey {
    /// The constraint name. `None` leaves the database to name it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub columns: Vec<ColumnName>,

    /// PostgreSQL index storage parameters (#1442), by name, each in its
    /// canonical spelling ([`crate::storage::canonical_index`]). A key's and
    /// a unique constraint's index is a B-tree. Empty is every parameter at
    /// its default.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub storage_parameters: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UniqueConstraint {
    pub columns: Vec<ColumnName>,

    /// PostgreSQL index storage parameters (#1442), by name, each in its
    /// canonical spelling ([`crate::storage::canonical_index`]). A key's and
    /// a unique constraint's index is a B-tree. Empty is every parameter at
    /// its default.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub storage_parameters: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignKey {
    pub columns: Vec<ColumnName>,
    pub references_table: TableName,
    pub references_columns: Vec<ColumnName>,

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
    // PostgreSQL's: checked as each row is changed, never at the end of the
    // statement nor deferred, so it is not `NoAction` (DEC-1633.1). SQL
    // Server has none, and its validation refuses it. A plain comment, so
    // the published schema keeps one flat enum of words.
    Restrict,
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
    pub include: Vec<ColumnName>,

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

    /// PostgreSQL index storage parameters (#1442), by name, each in its
    /// canonical spelling ([`crate::storage::canonical_index`]). A key's and
    /// a unique constraint's index is a B-tree. Empty is every parameter at
    /// its default.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub storage_parameters: BTreeMap<String, String>,
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

    pub fn column_keys(&self) -> Option<Vec<ColumnName>> {
        self.columns
            .iter()
            .map(|c| c.key.column().cloned())
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
    Column(ColumnName),
    Expression(String),
}

impl IndexKey {
    /// The column this key is, or `None` for an expression.
    pub fn column(&self) -> Option<&ColumnName> {
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
            IndexKey::Column(name) => name.raw(),
            IndexKey::Expression(text) => text,
        }
    }
}

impl IndexColumn {
    /// An ascending key on `name` under its default class: what most
    /// indexes are made of.
    pub fn column(name: impl Into<ColumnName>) -> Self {
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
    name: Option<ColumnName>,
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
                storage_parameters: Default::default(),
            }),
            ..Default::default()
        }
    }

    /// A period's columns are the engine's to write on every change to a
    /// row, so no declared row writes them (#1176).
    #[test]
    fn a_period_column_is_not_a_row_column() {
        let mut t = sample();
        for c in ["valid_from", "valid_to"] {
            t.columns
                .insert(c.into(), Column::new(ty("datetime2(7)")).not_null());
        }
        let row_columns = |t: &Table| -> Vec<String> {
            t.row_columns("customer_id")
                .map(|(c, _)| c.raw().to_owned())
                .collect()
        };
        // Negative first: without a period they are ordinary columns.
        assert_eq!(row_columns(&t), ["email", "valid_from", "valid_to"]);
        t.system_time = Some(SystemTime {
            start: "valid_from".into(),
            end: "valid_to".into(),
            hidden: false,
            versioning: None,
        });
        assert_eq!(row_columns(&t), ["email"]);
    }

    /// A retention period reads in either number and any case, is written
    /// back singular for one and plural otherwise, and anything the engine
    /// would refuse, zero included, is refused here (#1176).
    #[test]
    fn a_retention_period_reads_both_numbers_and_refuses_what_the_engine_does() {
        for (text, count, unit, written) in [
            ("6 months", 6, RetentionUnit::Month, "6 months"),
            ("1 MONTH", 1, RetentionUnit::Month, "1 month"),
            ("1 days", 1, RetentionUnit::Day, "1 day"),
            ("  3   Weeks ", 3, RetentionUnit::Week, "3 weeks"),
            ("10 year", 10, RetentionUnit::Year, "10 years"),
        ] {
            let r: Retention = text.parse().unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(r, Retention { count, unit }, "{text}");
            assert_eq!(r.to_string(), written, "{text}");
            assert_eq!(written.parse::<Retention>(), Ok(r), "{text}");
        }
        for text in [
            "0 days",
            "-1 days",
            "months",
            "6",
            "six months",
            "6 fortnights",
            "6 months ago",
            "6 monthss",
            "1 dayss",
            "",
        ] {
            assert!(text.parse::<Retention>().is_err(), "{text:?}");
        }
    }

    /// A generated column is the engine's to fill, like a non-key identity:
    /// no row writes it (DEC-1168.1).
    #[test]
    fn a_generated_column_is_not_a_row_column() {
        let mut t = sample();
        let mut total = Column::new(ty("int"));
        total.value_source = crate::ValueSource::Generated(Generated {
            expression: "customer_id * 2".into(),
            stored: true,
        });
        t.columns.insert("total".into(), total);
        let columns: Vec<&str> = t.row_columns("customer_id").map(|(c, _)| c.raw()).collect();
        assert!(!columns.contains(&"total"), "{columns:?}");
        // Negative: an ordinary column is one.
        assert!(columns.contains(&"email"), "{columns:?}");
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
            column.value_source = crate::ValueSource::Default(default.into());
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
            column.value_source = crate::ValueSource::Default(default.into());
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
        column.value_source = crate::ValueSource::Default("zz.null\u{301}x()".into());

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
                storage_parameters: Default::default(),
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
            storage_parameters: Default::default(),
        };
        let column = |c: &str| IndexKey::Column(c.into());
        t.unique.insert(
            "uq_id".into(),
            UniqueConstraint {
                columns: vec!["customer_id".into()],
                storage_parameters: Default::default(),
            },
        );
        t.unique.insert(
            "uq_email".into(),
            UniqueConstraint {
                columns: vec!["email".into()],
                storage_parameters: Default::default(),
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

    /// An index's parameters are its method's, and a key's and a unique
    /// constraint's are a B-tree's; anything else is refused by name (#1442).
    #[test]
    fn an_index_parameter_must_be_its_methods() {
        let mut t = sample();
        let gin = Index {
            columns: vec![IndexColumn {
                key: IndexKey::Column("email".into()),
                descending: false,
                opclass: None,
            }],
            include: Vec::new(),
            unique: false,
            filter: None,
            method: IndexMethod::Gin,
            storage_parameters: [("fastupdate".to_owned(), "false".to_owned())].into(),
        };
        t.indexes.insert("ix_gin".into(), gin);
        t.primary_key.as_mut().unwrap().storage_parameters =
            [("fillfactor".to_owned(), "70".to_owned())].into();
        assert!(
            t.storage_parameter_problems().is_empty(),
            "{:?}",
            t.storage_parameter_problems()
        );
        t.indexes.get_mut("ix_gin").unwrap().storage_parameters =
            [("fillfactor".to_owned(), "70".to_owned())].into();
        assert!(t.storage_parameter_problems()[0].contains("index `ix_gin`"));
        t.indexes
            .get_mut("ix_gin")
            .unwrap()
            .storage_parameters
            .clear();
        t.primary_key.as_mut().unwrap().storage_parameters =
            [("fastupdate".to_owned(), "true".to_owned())].into();
        assert!(t.storage_parameter_problems()[0].contains("primary key"));
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
                storage_parameters: Default::default(),
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
            t.columns.keys().cloned().collect::<Vec<_>>(),
            [
                crate::ColumnName::from("customer_id"),
                crate::ColumnName::from("email")
            ]
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

    /// Every `pg_attrdef` row a declaration holds an expression for: a
    /// column's default and generation expression, and a partition's own
    /// default, which is no column of the partition's (#1578 review). A
    /// column with neither, and a partition's own NOT NULL alone, hold none.
    #[test]
    fn expression_columns_include_a_partitions_own_defaults() {
        let mut table = Table::default();
        let mut d = Column::new("int".parse().unwrap());
        d.value_source = crate::ValueSource::Default("0".into());
        table.columns.insert("d".into(), d);
        let mut g = Column::new("int".parse().unwrap());
        g.value_source = crate::ValueSource::Generated(Generated {
            expression: "d * 2".into(),
            stored: true,
        });
        table.columns.insert("g".into(), g);
        table
            .columns
            .insert("plain".into(), Column::new("int".parse().unwrap()));
        assert_eq!(
            table.expression_columns().cloned().collect::<Vec<_>>(),
            [crate::ColumnName::from("d"), crate::ColumnName::from("g")]
        );
        let partition = Table {
            partition_of: Some(PartitionOf {
                parent: "app.t".parse().unwrap(),
                bound: PartitionBound::Default,
                columns: [
                    (
                        "d".into(),
                        PartitionColumn {
                            default: Some("1".into()),
                            not_null: false,
                        },
                    ),
                    (
                        "plain".into(),
                        PartitionColumn {
                            default: None,
                            not_null: true,
                        },
                    ),
                ]
                .into(),
            }),
            ..Table::default()
        };
        assert_eq!(
            partition.expression_columns().cloned().collect::<Vec<_>>(),
            [crate::ColumnName::from("d")]
        );
    }
}
