//! What `doctor` asks a database about the connected account's permissions,
//! on every engine, and about the runway of its RANGE-partitioned tables.
//!
//! The question is read off the declarations, which is all `doctor` can see —
//! it never looks at a plan — and the reading is engine-free: which tables
//! declare rows and what their rows could do, which objects the managed roles
//! are granted on. What the engine answers, and how it is asked, is each
//! engine's (`pbps_mssql::doctor`, `pbps_pg::doctor`): a SQL Server
//! permission is held on `OBJECT::`, `SCHEMA::` or the database, a PostgreSQL
//! one is ownership or a privilege on a relation, and the two reports render
//! their securables in their own `GRANT` spelling. So the *ask* lives here and
//! the *held* stays with the engine (DECISIONS 417).

use pbps_model::ColumnName;
use std::collections::{BTreeMap, BTreeSet};

use pbps_model::{GrantTarget, ModuleId, ObjectName, Permission, Table};

/// Everything the declarations say the deployment will need rights on.
///
/// One value rather than five parameters travelling together: they are
/// derived from the same load and handed on unchanged to every environment.
/// An engine reads the fields its permission model answers and documents the
/// ones it does not.
#[derive(Debug, Clone, Copy)]
pub struct Ask<'a> {
    /// The schemas this project manages.
    pub managed_schemas: &'a [String],
    /// Every table the declarations hold, under the managed schemas.
    pub managed_tables: &'a [ObjectName],
    /// Foreign-key targets the declarations do not hold.
    ///
    /// Membership of the managed *tables*, not of their schemas: an undeclared
    /// parent sharing a schema with the declarations is still somebody else's
    /// table, and neither engine's managed-table question reaches it
    /// (DECISIONS 509).
    pub referenced: &'a [ObjectName],
    /// The columns a declared key names on each of `referenced`'s targets —
    /// the union across every key that points there.
    ///
    /// Both engines grant `SELECT` and `REFERENCES` per column (issues #195
    /// and #215). A key and its preflight probe need only the columns it
    /// names; the target's unrelated columns must not demand wider grants.
    pub referenced_columns: &'a ReferencedColumns,
    /// What the managed roles are granted on (ADR-0005).
    pub granted: &'a GrantTargets,
    /// The tables that declare rows, and what each demands (ADR-0004).
    pub data: &'a DataTables,
    /// Every declared foreign key, by the table that holds it.
    ///
    /// The whole map, targets the declarations hold and targets they do not
    /// alike — [`Ask::referenced`] is the second half of it, kept apart
    /// because the two are asked about at different securables.
    ///
    /// This is how `doctor` reads a key the next apply takes away: the catalog
    /// has a key this map does not name, so a `DropForeignKey` runs before the
    /// deletes and the demands that key would have carried are not made
    /// (DECISIONS 513). Where the environment's recorded state lets the
    /// differ answer, a declared key the plan drops and *re-adds* is left out
    /// too, since it is just as absent while the deletes run (DEC-678.1). That
    /// includes an unchanged key rebuilt around a column the plan retypes
    /// (DEC-1511.1).
    pub declared_keys: &'a DeclaredKeys,
}

/// A foreign-key target the declarations do not hold, mapped to the columns a
/// declared key names on it — the union across every key that points there.
/// See [`Ask::referenced_columns`].
pub type ReferencedColumns = BTreeMap<ObjectName, BTreeSet<pbps_model::ColumnName>>;

/// The declared table holding foreign keys, mapped to the tables its keys
/// point at. See [`Ask::declared_keys`].
pub type DeclaredKeys = BTreeMap<ObjectName, BTreeSet<ObjectName>>;

/// What the managed roles are granted on, as `doctor` has to ask about it
/// (ADR-0005). Empty from the project files is not yet "no role": the
/// recorded state of the environment is consulted too, and a role it holds
/// that the declarations no longer have is one the next plan drops.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrantTargets {
    /// Exact targets and privileges: PostgreSQL grant authority is per
    /// privilege, and a routine's signature distinguishes its overloads.
    /// The flat lists below remain the SQL Server securable-level demand.
    pub permissions: BTreeMap<GrantTarget, BTreeSet<Permission>>,
    /// The objects the declarations grant on, as two parts. Joined by the
    /// engine in its own spelling: a name holding a `.` or a `]` joined here
    /// would resolve to nothing and read as a gap the account did not have.
    pub objects: Vec<ObjectName>,
    pub schemas: Vec<String>,
    /// The managed roles by name, as the project files know them: declared,
    /// or recorded in the ids file. The roles in the environment's recorded
    /// state join them. Whatever all of these hold — in the recorded state,
    /// and in the catalog where this login can see it — is asked about too,
    /// because a grant that is gone from the declarations is a `REVOKE` the
    /// plan will write, and the securable it names is where the right to
    /// revoke has to be held; the declarations alone cannot see it.
    pub roles: Vec<String>,
    /// Declared and ids-recorded tables whose adopted ACLs the plan manages.
    /// PostgreSQL adds the environment's recorded tables before inspecting ACLs.
    pub managed_tables: BTreeSet<ObjectName>,
    /// Declared module identities, including exact routine overloads. Recorded
    /// modules join these at the connected boundary, as in the differ's scope.
    pub managed_modules: BTreeSet<ModuleId>,
}

/// What one table's declaration could have done to its rows (ADR-0004).
///
/// Read off the declaration, which is all `doctor` can see — it never looks at
/// a plan. Each of the three is asked for on its own, because a declaration
/// can reach one and not another: an enumeration table whose only column is
/// its code inserts and deletes and can never update, and `mode: exact` with
/// no declared row deletes and can never insert.
///
/// Built only through [`DataDemand::of`], which answers `None` for a
/// declaration that could emit nothing at all — so "declares rows and demands
/// nothing" is an absence from [`DataTables`] rather than a value in it, and
/// the reading of the model happens in one place rather than at each caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDemand {
    inserts: bool,
    corrects: bool,
    removes: bool,
    /// The columns an emitted `UPDATE` could name, in declaration order.
    ///
    /// Carried rather than recomputed because it is two answers, not one:
    /// whether the declaration can correct a row at all (`corrects` is this
    /// list being non-empty), and *which* columns the permission has to cover
    /// — the question a column-level grant makes different from the
    /// object-level one.
    row_columns: Vec<ColumnName>,
    /// The key and writable cells used by data readback.
    data_columns: Vec<ColumnName>,
    /// The key and cells explicitly supplied by at least one declared row.
    insert_columns: Vec<ColumnName>,
}

impl DataDemand {
    /// What this table's `data:` block could do to it, or `None` if nothing.
    ///
    /// `None` covers three cases that are all "no statement": no block at all,
    /// `mode: ensure` with no declared row — which manages no row, so it can
    /// neither insert nor correct nor remove — and a block whose table has no
    /// single-column primary key, which the differ refuses outright
    /// (`DataWithoutKey`). The last is a broken declaration rather than an
    /// empty one, and it is not read as good news anywhere: `validate` reports
    /// it, and `doctor` runs `validate`'s own findings beside this.
    #[must_use]
    pub fn of(table: &Table) -> Option<Self> {
        let data = table.data.as_ref()?;
        let key_column = table.data_key_column()?;
        let declares_a_row = !data.rows.is_empty();
        let row_columns: Vec<ColumnName> = table
            .row_columns(key_column.raw())
            .map(|(name, _)| name.clone())
            .collect();
        let demand = Self {
            insert_columns: std::iter::once(key_column.to_owned())
                .chain(
                    row_columns
                        .iter()
                        .filter(|name| data.rows.values().any(|row| row.0.contains_key(*name)))
                        .cloned(),
                )
                .collect(),
            data_columns: std::iter::once(key_column.to_owned())
                .chain(row_columns.iter().cloned())
                .collect(),
            inserts: declares_a_row,
            // Both halves are needed: a row to compare, and a cell to compare
            // it in. The differ builds an `UPDATE` only from `row_columns` and
            // emits it only if that came out non-empty.
            corrects: declares_a_row && !row_columns.is_empty(),
            // `exact` alone, whether or not a row is declared: with none, the
            // declaration says the table must be empty, and every surviving
            // row is a `DELETE`.
            removes: data.mode == pbps_model::DataMode::Exact,
            row_columns,
        };
        (demand.inserts || demand.corrects || demand.removes).then_some(demand)
    }

    /// Whether a row could be inserted, which is what `INSERT` is asked for.
    #[must_use]
    pub const fn inserts(&self) -> bool {
        self.inserts
    }

    /// Whether a row could be corrected, which is what `UPDATE` is asked for.
    #[must_use]
    pub const fn corrects(&self) -> bool {
        self.corrects
    }

    /// Whether a row could be removed, which is what `DELETE` is asked for.
    #[must_use]
    pub const fn removes(&self) -> bool {
        self.removes
    }

    /// The columns an emitted `UPDATE` could name — the set `UPDATE` has to
    /// be held on, column by column, when it is not held on the table.
    #[must_use]
    pub fn row_columns(&self) -> &[ColumnName] {
        &self.row_columns
    }

    /// Columns named by data readback, including omitted defaulted cells.
    #[must_use]
    pub fn data_columns(&self) -> &[ColumnName] {
        &self.data_columns
    }

    /// Columns an INSERT can explicitly name; omitted cells use their defaults.
    #[must_use]
    pub fn insert_columns(&self) -> &[ColumnName] {
        &self.insert_columns
    }
}

/// The tables whose declarations carry rows, and what each of them demands.
///
/// Keyed by the table, not by its schema: both engines authorize DML on the
/// table, and a grant a careful DBA puts there is invisible to a schema-scoped
/// question.
pub type DataTables = BTreeMap<ObjectName, DataDemand>;

#[cfg(test)]
mod tests {
    use super::*;
    use pbps_model::{Column, DataMode, PrimaryKey, Row, TableData, Value};

    #[test]
    fn insert_demand_keeps_explicit_cells_separate_from_full_readback() {
        let mut table = Table::default();
        for name in ["id", "label", "note"] {
            table
                .columns
                .insert(name.into(), Column::new("integer".parse().unwrap()));
        }
        table.primary_key = Some(PrimaryKey {
            name: None,
            columns: vec!["id".into()],
            storage_parameters: Default::default(),
        });
        table.data = Some(TableData {
            mode: DataMode::Ensure,
            rows: [("1".into(), Row::default())].into(),
        });
        let omitted = DataDemand::of(&table).unwrap();
        assert_eq!(
            omitted.insert_columns(),
            [pbps_model::ColumnName::from("id")]
        );
        assert_eq!(
            omitted.data_columns(),
            [
                pbps_model::ColumnName::from("id"),
                pbps_model::ColumnName::from("label"),
                pbps_model::ColumnName::from("note")
            ]
        );
        assert_eq!(
            omitted.row_columns(),
            [
                pbps_model::ColumnName::from("label"),
                pbps_model::ColumnName::from("note")
            ]
        );
        let rows = &mut table.data.as_mut().unwrap().rows;
        rows.insert("2".into(), Row([("note".into(), Value::Null)].into()));
        rows.insert("3".into(), Row([("label".into(), Value::Int(3))].into()));
        assert_eq!(
            DataDemand::of(&table).unwrap().insert_columns(),
            [
                pbps_model::ColumnName::from("id"),
                pbps_model::ColumnName::from("label"),
                pbps_model::ColumnName::from("note")
            ]
        );
        table.data.as_mut().unwrap().rows.clear();
        assert!(
            DataDemand::of(&table).is_none(),
            "empty ensure has no statement demand"
        );
        table.data.as_mut().unwrap().mode = DataMode::Exact;
        let exact = DataDemand::of(&table).unwrap();
        assert!(!exact.inserts() && !exact.corrects() && exact.removes());
    }
}

/// Why a session that could not read the deployment lock cannot, as far as a
/// session with the same identity can see from the catalog (#822).
///
/// Asked only after the lock read has already failed, and only to choose
/// `doctor`'s remedy: a complete statement for the one gap the catalog shows,
/// or nothing to emit. It never changes the diagnosis. Each variant is a fact
/// that session observed, not one inferred from the failed read's error: a
/// denied lookup does not say whether the table exists, and on SQL Server a
/// table this user may not see reads exactly like one that is not there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockReadGap {
    /// PostgreSQL: the ledger's schema exists and this role may not use it.
    /// Nothing about the table is asked: without the schema it cannot be the
    /// next gap, and a never-initialized database has no table to grant on.
    SchemaUsage,
    /// SQL Server: this user cannot see anything under the lock table's name
    /// — no metadata visibility, so whether it exists is not something this
    /// session can learn. Absent and hidden are the same answer here, and
    /// neither is taken as absent. A visible object of another kind under
    /// that name is not this: it is [`LockReadGap::Unexplained`].
    Hidden,
    /// The lock table exists, this session can see it, and it may not
    /// `SELECT` from it.
    TableSelect,
    /// Nothing this session can see explains the failure: the privileges it
    /// would need are held, the table is not where the catalog would show it,
    /// or the question itself could not be asked. Never a reason to invent a
    /// grant.
    Unexplained,
}

// ---------------------------------------------------------------------------
// The runway of a RANGE-partitioned table (#1789, DEC-1789.2)
// ---------------------------------------------------------------------------

/// A parent the declarations hold that the runway question can be asked of:
/// a RANGE-partitioned table of one key column whose partitions pbps declares.
///
/// Read off the declarations like the rest of [`Ask`], and engine-free: which
/// parents there are and which ranges the declarations give them. What the
/// engine holds, where the newest rows are and how far the series reaches is
/// each engine's (`pbps_pg::doctor::runway`). A parent of several key columns
/// has no single frontier, and one declared `partitions: external` has its
/// partitions made by another tool, so neither is here (DEC-1789.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionedParent {
    /// The parent, as declared. A pending rename is followed by the engine,
    /// through the recorded identities, as the permission question is.
    pub name: ObjectName,
    /// The ranges its declared partitions take, in name order. A DEFAULT
    /// partition takes none.
    pub ranges: Vec<DeclaredRange>,
}

/// One declared range partition: the key's single column from one datum
/// (inclusive) to another (exclusive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredRange {
    pub partition: ObjectName,
    pub from: pbps_model::BoundDatum,
    pub to: pbps_model::BoundDatum,
}

impl PartitionedParent {
    /// Every declared parent the question applies to.
    #[must_use]
    pub fn all(schema: &pbps_model::Schema) -> Vec<Self> {
        use pbps_model::PartitionBound;
        let mut ranges: BTreeMap<&ObjectName, Vec<DeclaredRange>> = BTreeMap::new();
        for (name, table) in &schema.tables {
            let Some(of) = &table.partition_of else {
                continue;
            };
            // A bound of any other arity is the declarations' fault, which
            // `validate` reports; it is no range for this question.
            if let PartitionBound::Range { from, to } = &of.bound
                && let ([from], [to]) = (from.as_slice(), to.as_slice())
            {
                ranges.entry(&of.parent).or_default().push(DeclaredRange {
                    partition: name.clone(),
                    from: from.clone(),
                    to: to.clone(),
                });
            }
        }
        schema
            .tables
            .iter()
            .filter(|(_, table)| {
                table.partition_of.is_none()
                    && table
                        .partition_by
                        .as_ref()
                        .is_some_and(|by| !by.external && by.columns.len() == 1)
            })
            .map(|(name, _)| Self {
                name: name.clone(),
                ranges: ranges.remove(name).unwrap_or_default(),
            })
            .collect()
    }
}

/// What `doctor` asks about the partitioned parents (DEC-1789.2).
#[derive(Debug, Clone, Copy)]
pub struct RunwayAsk<'a> {
    pub parents: &'a [PartitionedParent],
    /// `partition_runway:` as written, or `None` for the default: twice the
    /// newest partition's width. The engine reads the text, since only it
    /// knows what the key's values are.
    pub horizon: Option<&'a str>,
}

/// What the engine found, for the parents it has something to say about.
///
/// A parent with nothing here is one with nothing to report: a series that
/// reaches the horizon, a table with no rows, a DEFAULT holding no row of a
/// range the declarations hold and the database lacks, a parent this
/// database does not have yet, or one the question does not apply to (an
/// engine without RANGE partitions answers with an empty report).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunwayReport {
    pub parents: Vec<ParentRunway>,
}

/// The two findings one parent can carry, each present or absent on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentRunway {
    /// The parent as declared.
    pub parent: ObjectName,
    pub runway: Option<Runway>,
    pub default_rows: Option<DefaultRows>,
}

/// Whether a parent's partitions reach far enough past its rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runway {
    Short(ShortRunway),
    /// The question could not be answered, for this reason. Never read as a
    /// series that is long enough.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortRunway {
    /// Where the database's partitions end.
    pub frontier: String,
    /// Where the partition holding the newest rows starts (or, for one with
    /// no lower end, where it ends).
    pub held_from: String,
    /// What the horizon asks the series to reach.
    pub reach: Reach,
    /// How the horizon is stated: the configured text, or the default's words.
    pub horizon: String,
    /// The newest partition's width as `--every` spells it, when it can be.
    pub every: Option<String>,
    /// Whether the declarations already reach as far as the horizon asks, so
    /// that what is missing is a plan applied and not a partition declared.
    pub declared_covers: bool,
    /// The parent's DEFAULT partition, where the rows past the frontier land.
    pub default: Option<ObjectName>,
}

/// What a horizon asks of a series, worked out against the partition that
/// holds the newest rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// The series should end at or after this key value, as the engine
    /// prints it.
    Until(String),
    /// This many partitions should end after where the newest rows start, the
    /// one holding them the first.
    Partitions(u32),
    /// The series should run past this edge of the key's axis (the last year
    /// the calendar supports, or the largest value of the key's type), which
    /// no partition with an upper end can reach: the horizon asks for a
    /// partition with no upper end (#2036). The edge is worded for the
    /// finding.
    Beyond(String),
}

/// Rows in a parent's DEFAULT partition that belong to partitions the
/// declarations hold and the database does not yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultRows {
    Held {
        /// The DEFAULT partition, under the name the database has.
        default: ObjectName,
        /// The declared partitions the database lacks, in name order.
        missing: Vec<ObjectName>,
        /// Those of them whose range the DEFAULT holds a row of. Empty when
        /// which could not be read, and then it is one of `missing`.
        holding: Vec<ObjectName>,
    },
    /// The DEFAULT could not be read, for this reason.
    Unknown(String),
}
