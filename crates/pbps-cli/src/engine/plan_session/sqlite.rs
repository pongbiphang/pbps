//! SQLite's positions of a connected plan (#2950).
//!
//! Only the 7 required methods of [`PlanSession`] are implemented here; every
//! provided method keeps its refusing default until the SQLite catalog name,
//! schedule, ordering, drop-blocker and site-vetting passes are built.

use super::*;
use pbps_sqlite::dialect::Sqlite;

/// What SQLite carries from one position of a plan to a later one: only the
/// site admission every engine's state holds, which stays empty because SQLite
/// has no stored routines.
#[derive(Default)]
pub(crate) struct PlanState {
    admission: RoutineAdmission,
}

impl AsRef<RoutineAdmission> for PlanState {
    fn as_ref(&self) -> &RoutineAdmission {
        &self.admission
    }
}

impl AsMut<RoutineAdmission> for PlanState {
    fn as_mut(&mut self) -> &mut RoutineAdmission {
        &mut self.admission
    }
}

/// The SQLite connection inside `conn`, or a refusal: a session handed another
/// engine's connection is a routing bug and never an empty database.
pub(in crate::engine) fn sqlite_conn(
    conn: &mut Conn,
) -> Result<&mut pbps_db::sqlite::Connection, DbError> {
    conn.sqlite().ok_or_else(|| {
        DbError::Refused(
            "the SQLite session was given a connection that is not to a SQLite file".into(),
        )
    })
}

/// Why role-management questions refuse on SQLite rather than answer empty.
///
/// SQLite has no roles or users at all (`Dialect::manages_roles()` is `false`),
/// so its dialect never plans `CREATE ROLE`, `RENAME ROLE` or `DROP ROLE`. Each
/// caller asks these questions only to clear one of those statements; answering
/// empty would read as "owns nothing / no members / no conflict" for a plan
/// that was not built for SQLite.
pub(in crate::engine) fn sqlite_has_no_roles(question: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{question} is a question about a role this tool creates, renames or drops, and SQLite \
         has no roles: this dialect plans none of those statements, so a plan that asks this was \
         not built for SQLite"
    )
}

impl PlanSession for Sqlite {
    type State = PlanState;

    /// SQLite has no partitions and no session-dependent date/time types, and
    /// declared reference-data rows are not built on SQLite yet. An empty
    /// [`Spellings`] is honest only when no table declares rows or partitions;
    /// any table that does is refused by name rather than answered empty.
    async fn misspelt(
        &self,
        _state: &PlanState,
        _conn: &mut Conn,
        schema: &Schema,
        _at: &CatalogNames,
    ) -> Result<Spellings, RowsError> {
        for (name, table) in &schema.tables {
            if table.data.is_some() {
                return Err(RowsError::Unreadable {
                    table: name.clone(),
                    why: "declared row spellings are not checked on SQLite yet".into(),
                });
            }
            if table.partition_of.is_some() || table.partition_by.is_some() {
                return Err(RowsError::Unreadable {
                    table: name.clone(),
                    why: "SQLite has no partitions, so partition bound spellings do not apply"
                        .into(),
                });
            }
        }
        Ok(Spellings::default())
    }

    /// SQLite's pure steps depend on the project's version floor, which `self`
    /// already carries, and not on a server setting; still verify the
    /// connection is a SQLite file and that its library meets the catalog
    /// reader's floor.
    async fn planning_dialect(&self, conn: &mut Conn) -> anyhow::Result<Self> {
        let c = sqlite_conn(conn)?;
        let problems = pbps_sqlite::preflight::library_problems(c, &ChangeSet::default())?;
        if !problems.is_empty() {
            anyhow::bail!("{}", pbps_sqlite::preflight::refusal(&problems));
        }
        Ok(*self)
    }

    /// SQLite has no roles; a plan that asks what a dropped role owns was not
    /// built for SQLite (refused rather than answered empty).
    async fn role_owned_securables(
        &self,
        _conn: &mut Conn,
    ) -> anyhow::Result<BTreeMap<pbps_model::RoleName, Vec<String>>> {
        Err(sqlite_has_no_roles("reading what the roles own"))
    }

    /// SQLite's only managed schema is `main` (`pbps_sqlite::validate::SCHEMA`),
    /// compared ASCII case-insensitively as SQLite does: `"main"` spells
    /// itself, `"MAIN"` spells `"main"` (so `refuse_wrongly_spelt` catches the
    /// case mismatch), and any other schema name is absent (`None`).
    async fn schema_spellings(
        &self,
        conn: &mut Conn,
        names: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, Option<String>>, DbError> {
        let _ = sqlite_conn(conn)?;
        let schema = pbps_sqlite::validate::SCHEMA;
        Ok(names
            .iter()
            .map(|name| {
                let spelled = pbps_sqlite::ident::same(name, schema).then(|| schema.to_owned());
                (name.clone(), spelled)
            })
            .collect())
    }

    /// SQLite has no SQL Server computed-column expression edges to read, so
    /// the differ screens standing computed columns by text.
    fn computed_column_screen(&self) -> pbps_diff::Screen {
        pbps_diff::Screen::Text
    }

    /// SQLite has no stored routines, so it pins none.
    async fn pinned_routines(
        &self,
        conn: &mut Conn,
    ) -> Result<Option<pbps_dialect::routines::Read>, DbError> {
        let _ = sqlite_conn(conn)?;
        Ok(None)
    }

    /// SQLite has no partitioned tables and no partition-maintenance tool.
    async fn maintained_partitions(
        &self,
        conn: &mut Conn,
    ) -> anyhow::Result<Vec<MaintainedParent>> {
        let _ = sqlite_conn(conn)?;
        Ok(Vec::new())
    }
}

#[cfg(test)]
pub(in crate::engine) mod test_support {
    use pbps_db::{Conn, Driver};
    use std::future::Future;
    use std::path::PathBuf;
    use std::task::{Context, Poll, Waker};

    /// Polls a future once: a SQLite session call runs in place on the calling
    /// thread and must be ready at the first poll (DEC-2950.17).
    pub(in crate::engine) fn now<T>(future: impl Future<Output = T>) -> T {
        let mut cx = Context::from_waker(Waker::noop());
        match std::pin::pin!(future).as_mut().poll(&mut cx) {
            Poll::Ready(value) => value,
            Poll::Pending => {
                panic!("a SQLite call is made in place and is ready at the first poll")
            }
        }
    }

    /// A temporary SQLite database file removed on drop.
    pub(in crate::engine) struct TempDb {
        dir: PathBuf,
    }

    impl TempDb {
        pub(in crate::engine) fn create() -> (Self, Conn) {
            let dir = std::env::temp_dir().join(format!(
                "pbps-cli-sqlite-test-{}-{:032x}",
                std::process::id(),
                rand::random::<u128>()
            ));
            std::fs::create_dir_all(&dir).expect("a temporary directory");
            let path = dir.join("test.db");
            let conn = Conn::create(Driver::Sqlite, path.to_str().expect("UTF-8 path"))
                .expect("the SQLite file is created");
            (Self { dir }, conn)
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{TempDb, now};
    use super::*;

    fn plain_schema() -> Schema {
        let mut table = pbps_model::Table::default();
        table.columns.insert(
            "id".into(),
            pbps_model::Column::new("integer".parse().expect("type")),
        );
        let mut schema = Schema::default();
        schema.tables.insert(TableName::new("main", "items"), table);
        schema
    }

    #[test]
    fn planning_dialect_preserves_the_configured_version_floor_on_a_real_file() {
        let (_tmp, mut conn) = TempDb::create();
        let floor = pbps_sqlite::Version::new(3, 42, 0);
        let dialect = Sqlite::with_floor(floor);
        let planned = now(dialect.planning_dialect(&mut conn)).expect("library meets floor");
        assert_eq!(planned.floor(), floor);
        // Negative: the default floor stays distinct from the configured one.
        assert_ne!(planned, Sqlite::default());
    }

    #[test]
    fn schema_spellings_maps_main_case_insensitively_and_reports_other_schemas_absent() {
        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        let names: BTreeSet<String> = ["main", "MAIN", "Main", "other", "public"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let spelled = now(dialect.schema_spellings(&mut conn, &names)).expect("spelled");
        assert_eq!(spelled["main"], Some("main".to_owned()));
        assert_eq!(spelled["MAIN"], Some("main".to_owned()));
        assert_eq!(spelled["Main"], Some("main".to_owned()));
        // Negative: schemas other than `main` are absent, not spelt back.
        assert_eq!(spelled["other"], None);
        assert_eq!(spelled["public"], None);
    }

    #[test]
    fn misspelt_is_empty_for_plain_tables_and_refuses_declared_rows_and_partitions_by_table() {
        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        let state = PlanState::default();
        let at = CatalogNames::new();

        let ok = now(dialect.misspelt(&state, &mut conn, &plain_schema(), &at)).expect("plain");
        assert_eq!(ok, Spellings::default());

        // Negative: a table declaring reference data is refused naming the table and SQLite.
        let mut with_rows = plain_schema();
        let row_table = TableName::new("main", "items");
        with_rows.tables.get_mut(&row_table).expect("table").data = Some(pbps_model::TableData {
            mode: pbps_model::DataMode::Ensure,
            rows: BTreeMap::new(),
        });
        match now(dialect.misspelt(&state, &mut conn, &with_rows, &at)) {
            Err(RowsError::Unreadable { table, why }) => {
                assert_eq!(table, row_table);
                assert!(why.contains("SQLite"), "{why}");
            }
            other => panic!("expected Unreadable for declared rows, got {other:?}"),
        }

        // Negative: a partitioned table is refused naming the table and SQLite.
        let mut with_partition = plain_schema();
        with_partition
            .tables
            .get_mut(&row_table)
            .expect("table")
            .partition_of = Some(pbps_model::PartitionOf {
            parent: TableName::new("main", "parent"),
            bound: pbps_model::PartitionBound::Default,
            columns: Default::default(),
        });
        match now(dialect.misspelt(&state, &mut conn, &with_partition, &at)) {
            Err(RowsError::Unreadable { table, why }) => {
                assert_eq!(table, row_table);
                assert!(why.contains("SQLite") && why.contains("partition"), "{why}");
            }
            other => panic!("expected Unreadable for partitioned table, got {other:?}"),
        }
    }

    #[test]
    fn role_owned_securables_refuses_naming_sqlite_rather_than_answering_empty() {
        let (_tmp, mut conn) = TempDb::create();
        let err = now(Sqlite::default().role_owned_securables(&mut conn)).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("SQLite") && message.contains("role"),
            "{message}"
        );
    }

    #[test]
    fn sqlite_plan_session_constants_reflect_no_routines_no_partitions_and_text_screen() {
        let (_tmp, mut conn) = TempDb::create();
        let dialect = Sqlite::default();
        assert_eq!(dialect.computed_column_screen(), pbps_diff::Screen::Text);
        assert!(
            now(dialect.pinned_routines(&mut conn))
                .expect("ok")
                .is_none()
        );
        assert!(
            now(dialect.maintained_partitions(&mut conn))
                .expect("ok")
                .is_empty()
        );
        // Provided defaults stay refusing for plans that create a table.
        let create = ChangeSet {
            changes: vec![pbps_model::PlannedChange::new(pbps_model::Change::Table(
                pbps_model::TableChange::CreateTable {
                    uid: pbps_model::Uid::derived(pbps_model::UidKind::Table, "main.t", 0),
                    name: TableName::new("main", "t"),
                    table: Default::default(),
                    beside_default: None,
                    moves_at_most: None,
                },
            ))],
        };
        let empty_scoped = pbps_diff::scope(
            &Schema::default(),
            &IdsFile::default(),
            &BTreeSet::new(),
            &BTreeSet::new(),
        );
        let refused = dialect
            .refuse_occupied_relation_names(&create, &empty_scoped, &[], &[], "test.db")
            .unwrap_err();
        assert!(refused.to_string().contains("sqlite"), "{refused}");
    }
}
