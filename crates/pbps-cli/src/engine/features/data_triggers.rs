//! Data-write trigger authentication and deferred constraint-trigger settling
//! (DEC-2798.19).
//!
//! PostgreSQL authenticates triggers that managed row writes and table-rewriting
//! DDL can fire, and flushes deferred constraint triggers before commit.
//! Engines without this mechanism declare [`Absent`]: neutral answers, refusing
//! only statements explicitly marked as a guarded row write or rewrite.
//! [`Absent`] deliberately does **not** refuse `Change::Row(_)`, because SQL
//! Server applies managed reference-data rows without PostgreSQL's trigger
//! authenticator.

use pbps_cli::resolver::server::PlanningDialect;
use pbps_db::Conn;
use pbps_dialect::routines::RoutineAdmission;
use pbps_model::{ChangeSet, IdsFile, StateSnapshot};

use super::Features;

/// The guard type carried across a transaction for engine `E`.
pub(crate) type Guard<E> = <<E as Features>::DataTriggers as Feature<E>>::Guard;

/// Whether engine `E` implements this feature rather than [`Absent`].
///
/// Projects through `E` so parity tests avoid `E0283` when `E::DataTriggers`
/// normalizes to the blanket `impl<E> Feature<E> for Absent`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const fn is_present<E: Features>() -> bool {
    <E::DataTriggers as Feature<E>>::PRESENT
}

/// Authenticating triggers that row writes and table rewrites can fire, and
/// settling deferred constraint triggers before commit.
#[allow(async_fn_in_trait)]
pub(crate) trait Feature<E: crate::emission::Engine + PlanningDialect> {
    /// Compile-time marker read by [`is_present`] for parity tests (`true` for
    /// real implementations, `false` for [`Absent`]).
    const PRESENT: bool;

    /// Trigger identities authenticated in the caller's open transaction;
    /// zero-sized when the feature is absent.
    type Guard: Default;

    /// Authenticates triggers in the caller's open transaction (plan time and
    /// apply time); what it holds is evidence for [`Self::check`].
    async fn prepare(
        dialect: &E,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        changes: &ChangeSet,
        baseline: &StateSnapshot,
        planned_ids: &IdsFile,
    ) -> anyhow::Result<Self::Guard>;

    /// Refuses a row write or a rewrite whose triggers `guard` did not
    /// authenticate.
    async fn check(
        dialect: &E,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        statement: &pbps_dialect::Statement,
        guard: &Self::Guard,
    ) -> anyhow::Result<()>;

    /// Flushes deferred constraint triggers; `true` means managed state must be
    /// re-read and compared.
    async fn settle(dialect: &E, conn: &mut Conn) -> anyhow::Result<bool>;
}

/// Dispatches [`Feature::prepare`] from a concrete `dialect` value.
///
/// Free functions rather than direct `D::DataTriggers::prepare` calls because
/// `with_plan_dialect!` binds a concrete `dialect` with no type parameter `D`
/// in scope, and keeping one uniform entry point avoids importing [`Feature`]
/// at every call site.
pub(crate) async fn prepare<E: Features>(
    dialect: &E,
    conn: &mut Conn,
    admission: &RoutineAdmission,
    changes: &ChangeSet,
    baseline: &StateSnapshot,
    planned_ids: &IdsFile,
) -> anyhow::Result<Guard<E>> {
    E::DataTriggers::prepare(dialect, conn, admission, changes, baseline, planned_ids).await
}

/// Dispatches [`Feature::check`] from a concrete `dialect` value (see
/// [`prepare`] for why callers go through free functions).
pub(crate) async fn check<E: Features>(
    dialect: &E,
    conn: &mut Conn,
    admission: &RoutineAdmission,
    statement: &pbps_dialect::Statement,
    guard: &Guard<E>,
) -> anyhow::Result<()> {
    E::DataTriggers::check(dialect, conn, admission, statement, guard).await
}

/// Dispatches [`Feature::settle`] from a concrete `dialect` value (see
/// [`prepare`] for why callers go through free functions).
pub(crate) async fn settle<E: Features>(dialect: &E, conn: &mut Conn) -> anyhow::Result<bool> {
    E::DataTriggers::settle(dialect, conn).await
}

/// Zero-size marker for engines without PostgreSQL's trigger authenticator.
///
/// Does **not** refuse `Change::Row(_)`: SQL Server writes reference rows
/// without PostgreSQL trigger authentication, while engines without managed
/// rows (SQLite) refuse them earlier in dialect validation, row reads, and
/// statement emission.
pub(crate) struct Absent;

impl<E: crate::emission::Engine + PlanningDialect> Feature<E> for Absent {
    const PRESENT: bool = false;
    type Guard = ();

    async fn prepare(
        _dialect: &E,
        _conn: &mut Conn,
        _admission: &RoutineAdmission,
        _changes: &ChangeSet,
        _baseline: &StateSnapshot,
        _planned_ids: &IdsFile,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn check(
        _dialect: &E,
        _conn: &mut Conn,
        _admission: &RoutineAdmission,
        statement: &pbps_dialect::Statement,
        _guard: &(),
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            statement.row_write.is_none() && statement.rewrites.is_none(),
            "{} has no data-write trigger authenticator for a guarded row write or table rewrite",
            E::NAME
        );
        Ok(())
    }

    async fn settle(_dialect: &E, _conn: &mut Conn) -> anyhow::Result<bool> {
        Ok(false)
    }
}

/// PostgreSQL's data-write trigger authentication and deferred-trigger
/// settling.
pub(crate) struct Postgres;

impl Feature<pbps_pg::Postgres> for Postgres {
    const PRESENT: bool = true;
    type Guard = pbps_pg::data_triggers::Guard;

    async fn prepare(
        _dialect: &pbps_pg::Postgres,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        changes: &ChangeSet,
        baseline: &StateSnapshot,
        planned_ids: &IdsFile,
    ) -> anyhow::Result<Self::Guard> {
        use pbps_dialect::{RowOperation, RowWrite};
        use pbps_model::Change;
        use pbps_model::ConstraintChange;
        use pbps_model::ModuleChange;
        use pbps_model::RowChange;
        use pbps_model::TableChange;
        let mut writes = Vec::new();
        let mut dropped = pbps_pg::data_triggers::Dropped::default();
        // Asked once per row change and updated column: indexed once,
        // not scanned per question (#1870).
        let planned_names = planned_ids.index();
        let baseline_names = baseline.ids.index();
        // The plan's own name for a table, in the spelling the catalog
        // still holds: the guard runs before the renames (DECISIONS 445).
        let stored = |table: &pbps_model::TableName| {
            planned_names
                .table_uid(table)
                .or_else(|| baseline_names.table_uid(table))
                .and_then(|uid| baseline.ids.tables.get(uid))
                .cloned()
        };
        let stored_column = |column: &pbps_model::ColumnRef| {
            planned_names
                .column_uid(column)
                .or_else(|| baseline_names.column_uid(column))
                .and_then(|uid| baseline.ids.columns.get(uid))
                .map_or_else(
                    || column.name.raw().to_owned(),
                    |old| old.name.raw().to_owned(),
                )
        };
        for planned in &changes.changes {
            if let Change::Module(ModuleChange::DropModule { id, .. }) = &planned.change {
                dropped.modules.insert(id.clone());
            }
            // Both are ordered ahead of every row change, so the
            // referential action they carry cannot write when the row
            // statement runs (DECISIONS 451).
            if let Change::Constraint(ConstraintChange::DropForeignKey { table, name }) =
                &planned.change
                && let Some(old) = stored(table)
            {
                dropped.foreign_keys.insert((old, name.clone()));
            }
            if let Change::Table(TableChange::DropTable { name, .. }) = &planned.change {
                dropped
                    .tables
                    .insert(stored(name).unwrap_or_else(|| name.clone()));
            }
            // The expressions the plan removes or replaces before a row is
            // written or a table rewritten: `prepare` does not judge what
            // will be gone by then, and `check` judges whatever is not
            // (DEC-2563.9).
            dropped.note_removed_expressions(&planned.change, &stored, &stored_column);
            // A table the plan rewrites, scans or copies into runs its index
            // expressions as the deploying role even though no row of the
            // plan is written to it. One that did not exist has no stored
            // name and is judged at its own statement instead.
            if let Some(write) = pbps_pg::existing_expressions::rewrite_write(&planned.change)
                && let Some(old) = stored(&write.table)
            {
                writes.push(RowWrite {
                    table: old,
                    operation: write.operation,
                });
            }
            let (table, mut operation) = if let Change::Row(RowChange::InsertRow {
                table,
                key_column,
                row,
                ..
            }) = &planned.change
            {
                // The columns the INSERT names: the guard judges the
                // DEFAULT of no other (#2748).
                (
                    table,
                    RowOperation::Insert {
                        columns: std::iter::once(key_column.clone())
                            .chain(row.columns().map(|(column, _)| column.clone()))
                            .collect(),
                    },
                )
            } else if let Change::Row(RowChange::UpdateRow { table, columns, .. }) = &planned.change
            {
                (
                    table,
                    RowOperation::Update {
                        columns: columns.keys().cloned().collect(),
                    },
                )
            } else if let Change::Row(RowChange::DeleteRow { table, .. }) = &planned.change {
                (table, RowOperation::Delete)
            } else {
                continue;
            };
            // The logical target has the plan's final spelling; locks are
            // acquired before any rename, through the stable table uid.
            // New tables receive no existing-trigger allowance.
            if let Some(old) = stored(table).as_ref() {
                // Like the table, an updated or inserted column may still
                // carry its pre-plan name while the guard authenticates
                // triggers.
                if let RowOperation::Update { columns } | RowOperation::Insert { columns } =
                    &mut operation
                {
                    *columns = columns
                        .iter()
                        .map(|name| {
                            let column = pbps_model::ColumnRef {
                                table: table.clone(),
                                name: name.clone(),
                            };
                            planned_names
                                .column_uid(&column)
                                .or_else(|| baseline_names.column_uid(&column))
                                .and_then(|uid| baseline.ids.columns.get(uid))
                                .map_or_else(|| name.clone(), |old| old.name.clone())
                        })
                        .collect();
                }
                writes.push(RowWrite {
                    table: old.clone(),
                    operation,
                });
            }
        }
        Ok(
            pbps_pg::data_triggers::prepare(conn, admission, &writes, &baseline.schema, &dropped)
                .await?,
        )
    }

    async fn check(
        _dialect: &pbps_pg::Postgres,
        conn: &mut Conn,
        admission: &RoutineAdmission,
        statement: &pbps_dialect::Statement,
        guard: &Self::Guard,
    ) -> anyhow::Result<()> {
        let Some(write) = statement.row_write.as_ref().or(statement.rewrites.as_ref()) else {
            return Ok(());
        };
        pbps_pg::data_triggers::check(conn, admission, write, guard).await?;
        Ok(())
    }

    async fn settle(_dialect: &pbps_pg::Postgres, conn: &mut Conn) -> anyhow::Result<bool> {
        pbps_pg::data_triggers::settle(conn).await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::plan_session::sqlite::test_support::{TempDb, now};
    use pbps_config::DialectName;
    use pbps_dialect::{RowOperation, RowWrite, Statement};
    use pbps_model::{Change, PlannedChange, RowChange, Schema, StateKind, TableName};
    use std::collections::BTreeSet;

    const fn present_of<E: Features>(_: &E) -> bool {
        is_present::<E>()
    }

    fn empty_baseline(dialect: &str) -> StateSnapshot {
        StateSnapshot::new(
            StateKind::Baseline,
            dialect,
            Schema::default(),
            IdsFile::default(),
            "test",
        )
    }

    #[test]
    fn absent_guard_is_zero_sized_while_postgres_carries_a_trigger_guard() {
        assert_eq!(std::mem::size_of::<Guard<pbps_mssql::Mssql>>(), 0);
        assert_eq!(
            std::mem::size_of::<Guard<pbps_sqlite::dialect::Sqlite>>(),
            0
        );
        // Contrast: PostgreSQL's real guard carries authenticated trigger state.
        assert!(std::mem::size_of::<Guard<pbps_pg::Postgres>>() > 0);
    }

    #[test]
    fn postgres_implements_data_triggers_and_other_engines_declare_absent() {
        assert!(is_present::<pbps_pg::Postgres>());
        assert!(!is_present::<pbps_mssql::Mssql>());
        assert!(!is_present::<pbps_sqlite::dialect::Sqlite>());

        // Exhaustive over DialectName::ALL so a fourth engine must declare its parity here.
        for &dialect in DialectName::ALL {
            let present =
                pbps_cli::with_plan_dialect!(crate::dialect_for(dialect), |d| { present_of(&d) });
            match dialect {
                DialectName::Postgres => {
                    assert!(present, "PostgreSQL must implement data_triggers")
                }
                DialectName::Mssql => assert!(!present, "SQL Server must declare Absent"),
                DialectName::Sqlite => assert!(!present, "SQLite must declare Absent"),
            }
        }
    }

    #[test]
    fn absent_is_neutral_on_all_three_operations_and_accepts_row_changes_in_prepare() {
        let (_tmp, mut conn) = TempDb::create();
        let mssql = pbps_mssql::Mssql;
        let sqlite = pbps_sqlite::dialect::Sqlite::default();
        let admission = RoutineAdmission::default();
        let empty = ChangeSet::default();
        let row_plan = ChangeSet {
            changes: vec![PlannedChange::new(Change::Row(RowChange::SetEnsureKeys {
                table: TableName::new("dbo", "t"),
                from: BTreeSet::new(),
                to: BTreeSet::new(),
            }))],
        };
        let plain = Statement::new("SELECT 1");

        // SQL Server: neutral on empty and row-change plans, plain DDL, and settle.
        let mssql_base = empty_baseline("mssql");
        now(prepare(
            &mssql,
            &mut conn,
            &admission,
            &empty,
            &mssql_base,
            &IdsFile::default(),
        ))
        .expect("Absent::prepare accepts empty plan on SQL Server");
        now(prepare(
            &mssql,
            &mut conn,
            &admission,
            &row_plan,
            &mssql_base,
            &IdsFile::default(),
        ))
        .expect("Absent::prepare must not refuse Change::Row on SQL Server");
        now(check(&mssql, &mut conn, &admission, &plain, &()))
            .expect("Absent::check accepts plain DDL on SQL Server");
        assert!(
            !now(settle(&mssql, &mut conn)).expect("Absent::settle returns false on SQL Server")
        );

        // SQLite: neutral on empty and row-change plans, plain DDL, and settle.
        let sqlite_base = empty_baseline("sqlite");
        now(prepare(
            &sqlite,
            &mut conn,
            &admission,
            &empty,
            &sqlite_base,
            &IdsFile::default(),
        ))
        .expect("Absent::prepare accepts empty plan on SQLite");
        now(prepare(
            &sqlite,
            &mut conn,
            &admission,
            &row_plan,
            &sqlite_base,
            &IdsFile::default(),
        ))
        .expect("Absent::prepare must not refuse Change::Row on SQLite");
        now(check(&sqlite, &mut conn, &admission, &plain, &()))
            .expect("Absent::check accepts plain DDL on SQLite");
        assert!(!now(settle(&sqlite, &mut conn)).expect("Absent::settle returns false on SQLite"));
    }

    #[test]
    fn absent_check_refuses_row_write_and_rewrite_statements_naming_the_engine() {
        let (_tmp, mut conn) = TempDb::create();
        let mssql = pbps_mssql::Mssql;
        let sqlite = pbps_sqlite::dialect::Sqlite::default();
        let admission = RoutineAdmission::default();

        let row_write = Statement::new("INSERT INTO t VALUES (1)")
            .writing_rows(TableName::new("dbo", "t"), RowOperation::Delete);
        let mut rewrite = Statement::new("ALTER TABLE t ALTER COLUMN a TYPE bigint");
        rewrite.rewrites = Some(RowWrite {
            table: TableName::new("main", "t"),
            operation: RowOperation::Delete,
        });

        for (label, stmt) in [("row_write", &row_write), ("rewrites", &rewrite)] {
            let err = now(check(&mssql, &mut conn, &admission, stmt, &()))
                .expect_err(label)
                .to_string();
            assert!(
                err.contains("SQL Server"),
                "expected 'SQL Server' in {label} refusal: {err}"
            );

            let err = now(check(&sqlite, &mut conn, &admission, stmt, &()))
                .expect_err(label)
                .to_string();
            assert!(
                err.contains("SQLite"),
                "expected 'SQLite' in {label} refusal: {err}"
            );
        }
    }
}
