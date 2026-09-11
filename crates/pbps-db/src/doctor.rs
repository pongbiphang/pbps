//! What `doctor` asks a database about the connected account's permissions,
//! on every engine.
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

use std::collections::BTreeMap;

use pbps_model::{ObjectName, Table};

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
    /// Foreign-key targets outside the managed schemas.
    pub referenced: &'a [ObjectName],
    /// What the managed roles are granted on (ADR-0005).
    pub granted: &'a GrantTargets,
    /// The tables that declare rows, and what each demands (ADR-0004).
    pub data: &'a DataTables,
}

/// What the managed roles are granted on, as `doctor` has to ask about it
/// (ADR-0005). Empty from the project files is not yet "no role": the
/// recorded state of the environment is consulted too, and a role it holds
/// that the declarations no longer have is one the next plan drops.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrantTargets {
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
    row_columns: Vec<String>,
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
        let row_columns: Vec<String> = table
            .row_columns(key_column)
            .map(|(name, _)| name.clone())
            .collect();
        let demand = Self {
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
    pub fn row_columns(&self) -> &[String] {
        &self.row_columns
    }
}

/// The tables whose declarations carry rows, and what each of them demands.
///
/// Keyed by the table, not by its schema: both engines authorize DML on the
/// table, and a grant a careful DBA puts there is invisible to a schema-scoped
/// question.
pub type DataTables = BTreeMap<ObjectName, DataDemand>;
